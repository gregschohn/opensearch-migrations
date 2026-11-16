//! Buffer release-exactly-once on every path (traceability row WIRE-01, treatment *Retired*).
//!
//! Java's equivalent obligation is `CodedOutputStreamHolder.close()`, a manual release discharged at
//! six sites and *not* discharged at a seventh: wire spec defect J-3, the pass-through teardown
//! path, leaks the open record's pooled `ByteBuf` for every affected connection. These tests pin
//! that every path through this crate releases exactly once, including the one Java leaks.

use std::sync::Arc;
use std::time::Duration;

use capture_wire::protobuf_size::field;
use capture_wire::publish_chain::{PublishChain, PublishFailure};
use capture_wire::record_buffer::{CountingRecycler, RecordBuf};
use capture_wire::record_encoder::{ObservationBody, ObservationPlan, RecordEncoder, RecordHeader};
use capture_wire::record_sink::{CollectingSink, RecordSink, SinkError};
use capture_wire::{CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig, WireError};

const TS: CaptureTimestamp = CaptureTimestamp::EPOCH;

fn header() -> RecordHeader {
    RecordHeader {
        connection_id: Arc::from("conn-own"),
        node_id: Arc::from("node-own"),
        prior_request_boundaries: 0,
        reads_awaiting_end_of_message: false,
    }
}

fn recorder_with(
    recycler: &Arc<CountingRecycler>,
    payload_limit: Option<u32>,
) -> ConnectionRecorder<CollectingSink> {
    let mut config = ConnectionRecorderConfig::new("conn-own", "node-own")
        .with_flush_interval(Duration::from_hours(1));
    config.payload_limit = payload_limit;
    let sink = CollectingSink::with_recycler(Arc::clone(recycler) as Arc<_>);
    ConnectionRecorder::new(config, sink).expect("valid config")
}

/// Success: the sink owns the record, and releasing it is a single drop.
#[test]
fn a_published_record_is_released_exactly_once_when_the_sink_drops_it() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, None);
    recorder.record_read(TS, b"request").expect("read");
    recorder.close(TS).expect("close");
    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 0, "the sink still owns the record");

    recorder.sink_mut().clear();
    assert_eq!(recycler.released(), 1);
    assert_eq!(recycler.outstanding(), 0);
}

/// Failure: a sink that rejects the submission has already consumed the record, so the buffer is
/// released exactly once and never twice.
#[test]
fn a_rejected_submission_releases_the_buffer_exactly_once() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, None);
    recorder.record_read(TS, b"request").expect("read");
    recorder
        .sink_mut()
        .fail_next_submit(SinkError::new("broker unreachable"));
    recorder.close(TS).expect("the close itself still succeeds");

    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 1);
    assert!(recorder.publication_failure().is_some());
}

/// Rejection: a poisoned chain releases every record it refuses to publish.
#[test]
fn a_poisoned_chain_releases_every_rejected_record() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut sink = CollectingSink::with_recycler(Arc::clone(&recycler) as Arc<_>);
    let mut chain = PublishChain::new();
    chain.fail(PublishFailure::new(SinkError::new("already failed")));

    for index in 1..=5u32 {
        let buffer = sink.create_buffer(64);
        let mut encoder = RecordEncoder::start(buffer, header(), index, None);
        let body = ObservationBody::Empty;
        let plan = ObservationPlan::new(u64::from(index), TS, field::REQUEST_FORWARDED, &body);
        encoder.write_observation(&plan, &body);
        let record = encoder.finish(false).expect("non-final record");
        chain.submit(record, &mut sink);
    }

    assert_eq!(
        sink.submit_calls(),
        0,
        "a poisoned chain never reaches the sink"
    );
    assert_eq!(recycler.taken(), 5);
    assert_eq!(
        recycler.released(),
        5,
        "every rejected record was released exactly once"
    );
}

/// Failure: an encode error consumes the record, so the buffer is released even though nothing was
/// published.
#[test]
fn a_failed_encode_releases_the_buffer() {
    let recycler = Arc::new(CountingRecycler::new());
    let buffer = RecordBuf::with_recycler(64, Arc::clone(&recycler) as Arc<_>);
    let encoder = RecordEncoder::start(buffer, header(), 1, None);
    // An empty record is refused, and `finish` takes `self`, so the buffer cannot survive.
    assert!(matches!(encoder.finish(false), Err(WireError::EmptyRecord)));
    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 1);
}

/// Failure: the terminal-record biconditional rejects the record and still releases the buffer.
#[test]
fn a_terminal_invariant_violation_releases_the_buffer() {
    let recycler = Arc::new(CountingRecycler::new());
    let buffer = RecordBuf::with_recycler(64, Arc::clone(&recycler) as Arc<_>);
    let mut encoder = RecordEncoder::start(buffer, header(), 1, None);
    let body = ObservationBody::Empty;
    let plan = ObservationPlan::new(1, TS, field::REQUEST_FORWARDED, &body);
    encoder.write_observation(&plan, &body);
    assert!(matches!(
        encoder.finish(true),
        Err(WireError::TerminalRecordInvariant { .. })
    ));
    assert_eq!(recycler.released(), 1);
}

/// **The J-3 fix.** Teardown without publication releases the open record instead of leaking it.
#[test]
fn abort_releases_an_unpublished_record_instead_of_leaking_it() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, None);
    recorder
        .record_read(TS, b"a partially captured request")
        .expect("read");
    recorder
        .record_write(TS, b"and a partial response")
        .expect("write");
    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 0);

    let discarded = recorder.abort();

    assert_eq!(discarded, 2, "both buffered observations were discarded");
    assert_eq!(recycler.released(), 1, "the pooled buffer came back");
    assert_eq!(recycler.outstanding(), 0);
    assert_eq!(recorder.sink().records().len(), 0, "nothing was published");
    // After an abort the connection is closed to further capture.
    assert!(recorder.is_terminated());
    assert!(matches!(
        recorder.record_read(TS, b"late"),
        Err(WireError::AfterTerminalClose)
    ));
    assert!(
        recorder.flush_deadline().is_none(),
        "the deadline was invalidated too"
    );
}

/// Dropping the recorder outright also releases the open record.
#[test]
fn dropping_a_recorder_releases_its_open_record() {
    let recycler = Arc::new(CountingRecycler::new());
    {
        let mut recorder = recorder_with(&recycler, None);
        recorder.record_read(TS, b"never published").expect("read");
        assert_eq!(recycler.released(), 0);
    }
    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 1);
}

/// `into_sink` behaves like `abort` and leaves nothing outstanding.
#[test]
fn into_sink_releases_the_open_record() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, None);
    recorder.record_read(TS, b"buffered").expect("read");
    let sink = recorder.into_sink();
    assert_eq!(recycler.released(), 1);
    assert_eq!(sink.records().len(), 0);
}

/// A long segmented life cycle, published and acknowledged record by record, leaves no buffer
/// outstanding once the sink's records are dropped.
#[test]
fn a_whole_segmented_connection_leaves_no_buffer_outstanding() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, Some(1024));
    let payload: Vec<u8> = (0..20_000u32)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    recorder.record_read(TS, &payload).expect("segmented read");
    recorder.commit_end_of_message(TS).expect("end of message");
    recorder
        .record_write(TS, &payload)
        .expect("segmented write");
    recorder.close(TS).expect("close");
    while recorder.chain().in_flight().is_some() {
        recorder.acknowledge_publication();
    }

    let produced = recorder.sink().records().len();
    assert!(
        produced > 10,
        "a 40 KiB connection in 1 KiB records should span many records"
    );
    assert_eq!(recycler.taken(), produced);
    assert_eq!(recycler.released(), 0, "the sink holds them all");

    recorder.sink_mut().clear();
    assert_eq!(recycler.released(), produced);
    assert_eq!(recycler.outstanding(), 0);
}

/// The envelope prefix is back-filled into reserved head space, so the record is one contiguous
/// allocation and the `TrafficStream` bytes are never copied.
#[test]
fn the_kafka_value_is_one_contiguous_allocation() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut recorder = recorder_with(&recycler, None);
    recorder.record_read(TS, &[0x42u8; 5000]).expect("read");
    recorder.close(TS).expect("close");
    let record = &recorder.sink().records()[0];

    let payload = record.payload();
    let stream = record.traffic_stream_bytes();
    // The stream is a suffix of the payload, at the same address: no copy was made.
    assert_eq!(payload.len(), stream.len() + 2 + 1);
    assert!(std::ptr::eq(
        payload[payload.len() - stream.len()..].as_ptr(),
        stream.as_ptr()
    ));
    assert_eq!(payload[0], 0x0A, "CaptureRecord field 1, wire type 2");
}
