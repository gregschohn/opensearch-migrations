//! The `F` flush-deadline obligations (traceability row WIRE-08).
//!
//! Ported from `StreamChannelConnectionCaptureSerializerTest` cases B13-B15 and from
//! `docs/captureAndReplay/proxyCaptureProtocol.md` §4.2 steps 1-7.

use std::sync::Arc;
use std::time::Duration;

use capture_wire::connection_recorder::decode_traffic_streams;
use capture_wire::proto::traffic_observation::Capture;
use capture_wire::proto::{self, TrafficStream};
use capture_wire::record_sink::CollectingSink;
use capture_wire::{
    CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig, DEFAULT_FLUSH_INTERVAL,
    ManualClock, ScheduledFlush,
};

const CONNECTION_ID: &str = "conn-deadline";
const NODE_ID: &str = "node-deadline";
const TS: CaptureTimestamp = CaptureTimestamp::new(1_704_067_200, 0);
/// The Java deadline tests use a 5 ms interval with a hand-driven nanosecond clock.
const INTERVAL: Duration = Duration::from_millis(5);

type Recorder = ConnectionRecorder<CollectingSink>;

fn recorder(interval: Duration) -> (Recorder, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let config =
        ConnectionRecorderConfig::new(CONNECTION_ID, NODE_ID).with_flush_interval(interval);
    let recorder =
        ConnectionRecorder::with_clock(config, CollectingSink::new(), Arc::clone(&clock) as Arc<_>)
            .expect("valid config");
    (recorder, clock)
}

fn streams(recorder: &mut Recorder) -> Vec<TrafficStream> {
    while recorder.chain().in_flight().is_some() {
        recorder.acknowledge_publication();
    }
    decode_traffic_streams(recorder.sink().records())
}

fn index_of(stream: &TrafficStream) -> i32 {
    match stream.index.as_ref().expect("index set") {
        proto::traffic_stream::Index::Number(n)
        | proto::traffic_stream::Index::NumberOfThisLastChunk(n) => *n,
    }
}

/// The production default is five seconds.
#[test]
fn the_default_interval_is_five_seconds() {
    assert_eq!(DEFAULT_FLUSH_INTERVAL, Duration::from_secs(5));
    let config = ConnectionRecorderConfig::new(CONNECTION_ID, NODE_ID);
    assert_eq!(config.flush_interval, DEFAULT_FLUSH_INTERVAL);
}

/// Step 1: the deadline is armed by the first observation of a new nonempty record, not by the
/// record's creation and not by the connection's last activity.
#[test]
fn the_deadline_is_armed_by_the_first_observation() {
    let (mut recorder, clock) = recorder(INTERVAL);
    assert!(
        recorder.flush_deadline().is_none(),
        "an empty record has no deadline"
    );
    clock.advance(Duration::from_millis(1));
    recorder.record_read(TS, b"a").expect("read");
    let deadline = recorder.flush_deadline().expect("deadline armed");
    assert_eq!(
        deadline.at_nanos,
        u64::try_from(Duration::from_millis(6).as_nanos()).unwrap(),
        "the deadline is now + F, measured from the first observation"
    );
}

/// B13 / step 3 — the deadline is FIXED: later observations inside the window never move it, and
/// the scheduled callback at `t = F` publishes one record holding both observations.
#[test]
fn b13_the_deadline_is_not_extended_by_progress() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"first").expect("read at t=0");
    let armed = recorder.flush_deadline().expect("deadline armed");

    clock.advance(Duration::from_millis(4));
    recorder.record_read(TS, b"second").expect("read at t=4ms");
    assert_eq!(
        recorder.flush_deadline().expect("still armed"),
        armed,
        "appending before the deadline must not move or replace it"
    );
    assert_eq!(
        recorder.sink().records().len(),
        0,
        "nothing published at t=4ms"
    );

    clock.advance(Duration::from_millis(1));
    let outcome = recorder
        .run_scheduled_flush(armed.generation)
        .expect("scheduled flush");
    assert!(
        matches!(outcome, ScheduledFlush::Detached { lateness, .. } if lateness.is_zero()),
        "expected an on-time detachment, got {outcome:?}"
    );

    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sub_stream.len(), 2);
    assert_eq!(index_of(&records[0]), 1);
    assert_eq!(
        records[0].index,
        Some(proto::traffic_stream::Index::Number(1)),
        "a periodic flush is never final"
    );
    let sequences: Vec<_> = records[0]
        .sub_stream
        .iter()
        .map(|o| o.connection_observation_sequence)
        .collect();
    assert_eq!(sequences, vec![Some(1), Some(2)]);
}

/// B14 / step 2 — at-the-deadline counts as expired (`>=`, not `>`): the old record is detached
/// **before** the new observation is admitted, so the observation lands in the successor.
#[test]
fn b14_an_observation_at_the_deadline_detaches_the_expired_record_first() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"first").expect("read at t=0");

    clock.advance(INTERVAL);
    recorder
        .record_read(TS, b"second")
        .expect("read at exactly t=5ms");

    // The first record was published before the second read was appended.
    let published = streams(&mut recorder);
    assert_eq!(
        published.len(),
        1,
        "exactly the expired record is published so far"
    );
    assert_eq!(
        published[0].sub_stream.len(),
        1,
        "the expired record holds one observation"
    );
    assert_eq!(
        published[0].sub_stream[0].connection_observation_sequence,
        Some(1)
    );

    // The successor continues at the next contiguous sequence.
    assert_eq!(recorder.current_record_observations(), 1);
    recorder.close(TS).expect("close");
    let all = streams(&mut recorder);
    assert_eq!(all.len(), 2);
    let successor_sequences: Vec<_> = all[1]
        .sub_stream
        .iter()
        .map(|o| o.connection_observation_sequence)
        .collect();
    assert_eq!(successor_sequences, vec![Some(2), Some(3)]);
}

/// B15 / "no timer is scheduled for an empty record" — an idle connection emits no empty
/// `TrafficStream`, however long the callback waits.
#[test]
fn b15_an_idle_connection_publishes_no_empty_record() {
    let (mut recorder, clock) = recorder(INTERVAL);
    assert!(recorder.flush_deadline().is_none());
    clock.advance(Duration::from_hours(1));
    // Any generation: there is no record, so there is nothing to publish.
    for generation in 0..4 {
        assert_eq!(
            recorder
                .run_scheduled_flush(generation)
                .expect("scheduled flush"),
            if generation == 0 {
                ScheduledFlush::NoRecord
            } else {
                ScheduledFlush::Stale
            }
        );
    }
    assert_eq!(recorder.sink().records().len(), 0);
    assert!(recorder.flush().expect("flush").is_none());
    assert_eq!(recorder.sink().records().len(), 0);
}

/// Step 5 — the scheduled callback is what publishes an otherwise-idle record. No further traffic
/// is required.
#[test]
fn the_scheduled_callback_publishes_an_otherwise_idle_record() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"only activity").expect("read");
    let deadline = recorder.flush_deadline().expect("armed");

    clock.advance(INTERVAL);
    let outcome = recorder
        .run_scheduled_flush(deadline.generation)
        .expect("flush");
    assert!(matches!(outcome, ScheduledFlush::Detached { .. }));
    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sub_stream.len(), 1);
    assert!(
        recorder.flush_deadline().is_none(),
        "the deadline is cleared by the detachment"
    );
}

/// A callback that runs early reschedules for the remainder and publishes nothing.
#[test]
fn an_early_callback_reschedules_instead_of_publishing() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"x").expect("read");
    let deadline = recorder.flush_deadline().expect("armed");

    clock.advance(Duration::from_millis(2));
    match recorder
        .run_scheduled_flush(deadline.generation)
        .expect("flush")
    {
        ScheduledFlush::Early { remaining } => assert_eq!(remaining, Duration::from_millis(3)),
        other => panic!("expected an early callback, got {other:?}"),
    }
    assert_eq!(recorder.sink().records().len(), 0);
    assert_eq!(
        recorder.time_until_flush_deadline(),
        Some(Duration::from_millis(3)),
        "the owner can re-arm from the exposed deadline"
    );
}

/// A callback that runs late reports its lateness and publishes immediately. Lateness alone does
/// not compromise capture.
#[test]
fn a_late_callback_reports_lateness_and_publishes() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"x").expect("read");
    let deadline = recorder.flush_deadline().expect("armed");

    clock.advance(Duration::from_millis(37));
    match recorder
        .run_scheduled_flush(deadline.generation)
        .expect("flush")
    {
        ScheduledFlush::Detached { lateness, .. } => {
            assert_eq!(lateness, Duration::from_millis(32));
        }
        other => panic!("expected a late detachment, got {other:?}"),
    }
    assert_eq!(streams(&mut recorder).len(), 1);
}

/// Step 7 — an earlier detachment invalidates the deadline, and a stale callback must not flush the
/// successor record.
#[test]
fn a_stale_callback_cannot_flush_the_successor_record() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"first").expect("read");
    let stale = recorder.flush_deadline().expect("armed");

    // An explicit flush detaches early and invalidates that deadline.
    recorder.flush().expect("explicit flush");
    recorder
        .record_read(TS, b"second")
        .expect("read into the successor");
    let fresh = recorder.flush_deadline().expect("re-armed");
    assert_ne!(stale.generation, fresh.generation);

    clock.advance(Duration::from_secs(1));
    assert_eq!(
        recorder
            .run_scheduled_flush(stale.generation)
            .expect("stale callback"),
        ScheduledFlush::Stale,
        "the stale callback must not touch the successor record"
    );
    assert_eq!(
        recorder.sink().records().len(),
        1,
        "only the explicitly flushed record"
    );

    // The fresh generation still works.
    assert!(matches!(
        recorder
            .run_scheduled_flush(fresh.generation)
            .expect("fresh callback"),
        ScheduledFlush::Detached { .. }
    ));
    assert_eq!(streams(&mut recorder).len(), 2);
}

/// A stale callback after the terminal close must not create a record for a closed connection.
#[test]
fn a_stale_callback_after_the_close_creates_nothing() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"request").expect("read");
    let deadline = recorder.flush_deadline().expect("armed");
    recorder.close(TS).expect("close");

    clock.advance(Duration::from_mins(1));
    for generation in [deadline.generation, deadline.generation + 1, 999] {
        assert_eq!(
            recorder
                .run_scheduled_flush(generation)
                .expect("post-close callback"),
            ScheduledFlush::NoRecord,
            "nothing may be created after CloseObservation"
        );
    }
    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    assert!(matches!(
        records[0].sub_stream.last().unwrap().capture,
        Some(Capture::Close(_))
    ));
}

/// Step 6 and §4.1 — terminal publication detaches the nonempty current record immediately,
/// regardless of its `F` deadline.
#[test]
fn terminal_publication_ignores_the_deadline() {
    let (mut recorder, clock) = recorder(Duration::from_hours(1));
    recorder.record_read(TS, b"request").expect("read");
    clock.advance(Duration::from_millis(1));
    assert!(
        recorder.time_until_flush_deadline().expect("armed") > Duration::from_mins(50),
        "the deadline is nowhere near"
    );

    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    assert_eq!(
        records.len(),
        1,
        "the open record is published as part of the terminal publication"
    );
    assert_eq!(records[0].sub_stream.len(), 2);
    assert_eq!(
        records[0].index,
        Some(proto::traffic_stream::Index::NumberOfThisLastChunk(1))
    );
}

/// Step 6 — detachment clears the current-record reference, so a later observation enters only a
/// successor, and that successor re-arms a fresh deadline.
#[test]
fn a_detachment_re_arms_the_deadline_for_the_successor() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder.record_read(TS, b"a").expect("read");
    clock.advance(INTERVAL);
    recorder
        .record_read(TS, b"b")
        .expect("read, detaching the expired record");

    let successor_deadline = recorder.flush_deadline().expect("re-armed");
    assert_eq!(
        successor_deadline.at_nanos,
        u64::try_from((INTERVAL + INTERVAL).as_nanos()).unwrap(),
        "the successor's deadline starts at its own first observation"
    );
}

/// The deadline is driven by the injected monotonic clock only; observation timestamps are
/// independent wall-clock values and cannot move it.
#[test]
fn observation_timestamps_do_not_influence_the_deadline() {
    let (mut recorder, clock) = recorder(INTERVAL);
    recorder
        .record_read(CaptureTimestamp::new(10_000_000_000, 0), b"a")
        .expect("read");
    let deadline = recorder.flush_deadline().expect("armed");
    assert_eq!(
        deadline.at_nanos,
        u64::try_from(INTERVAL.as_nanos()).unwrap()
    );

    // A wall-clock jump backwards in the observation timestamps changes nothing.
    recorder
        .record_read(CaptureTimestamp::new(1, 0), b"b")
        .expect("read");
    assert_eq!(recorder.flush_deadline().expect("armed"), deadline);
    clock.advance(INTERVAL);
    assert!(matches!(
        recorder
            .run_scheduled_flush(deadline.generation)
            .expect("flush"),
        ScheduledFlush::Detached { .. }
    ));
}
