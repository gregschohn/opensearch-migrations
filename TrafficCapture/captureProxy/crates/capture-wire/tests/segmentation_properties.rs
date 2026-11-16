//! Fragmentation and boundary properties (traceability rows WIRE-02, WIRE-08).
//!
//! The Java suite pins segmentation at a handful of hand-picked buffer sizes. These properties
//! cover the whole space the hand-picked cases sample from, and in particular the varint-width
//! boundaries at 128, 16384 and 2097152 where the Java size arithmetic is one byte pessimistic.

use std::time::Duration;

use capture_wire::connection_recorder::decode_traffic_streams;
use capture_wire::proto::traffic_observation::Capture;
use capture_wire::protobuf_size::{self as size, field};
use capture_wire::record_encoder::{ObservationBody, ObservationPlan};
use capture_wire::record_sink::CollectingSink;
use capture_wire::segmentation::{ByteObservationKind, max_segment_data_len, segment_record_cost};
use capture_wire::{CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig};
use proptest::prelude::*;

type Recorder = ConnectionRecorder<CollectingSink>;

/// Smallest payload limit whose derived `TrafficStream` budget is exactly `target`, or `None` if
/// no limit produces it (the envelope reservation skips a value at each varint boundary).
fn payload_limit_for_budget(target: u32) -> Option<u32> {
    (target..target + 16)
        .find(|limit| size::traffic_stream_budget_for_payload_limit(*limit) == Some(target))
}

fn recorder(payload_limit: u32) -> Recorder {
    let config = ConnectionRecorderConfig::new("conn-prop", "node-prop")
        .with_flush_interval(Duration::from_hours(1))
        .with_payload_limit(payload_limit);
    ConnectionRecorder::new(config, CollectingSink::new()).expect("valid config")
}

fn drain(recorder: &mut Recorder) {
    while recorder.chain().in_flight().is_some() {
        recorder.acknowledge_publication();
    }
}

/// Reassembles a connection's captured bytes for one observation kind, checking structure as it
/// goes. Returns `(bytes, segment_end_count, largest_enveloped_record)`.
fn replay(recorder: &mut Recorder, kind: ByteObservationKind) -> (Vec<u8>, usize, usize) {
    drain(recorder);
    let records = recorder.sink().records();
    let largest = records
        .iter()
        .map(capture_wire::EncodedRecord::payload_len)
        .max()
        .unwrap_or(0);
    let streams = decode_traffic_streams(records);

    let mut bytes = Vec::new();
    let mut segment_ends = 0usize;
    let mut expected_sequence = 1u64;
    let mut saw_final = false;
    for (expected_index, stream) in (1i32..).zip(streams.iter()) {
        assert!(!saw_final, "no record may follow the final one");
        match stream
            .index
            .as_ref()
            .expect("the index oneof is always set")
        {
            capture_wire::proto::traffic_stream::Index::Number(n) => assert_eq!(*n, expected_index),
            capture_wire::proto::traffic_stream::Index::NumberOfThisLastChunk(n) => {
                assert_eq!(*n, expected_index);
                saw_final = true;
            }
        }
        assert!(!stream.sub_stream.is_empty(), "no record may be empty");
        for observation in &stream.sub_stream {
            assert_eq!(
                observation.connection_observation_sequence,
                Some(expected_sequence),
                "connectionObservationSequence must be present and exactly +1"
            );
            expected_sequence += 1;
            match (kind, observation.capture.as_ref().expect("capture set")) {
                (ByteObservationKind::Read, Capture::Read(whole)) => {
                    bytes.extend_from_slice(&whole.data);
                }
                (ByteObservationKind::Read, Capture::ReadSegment(segment)) => {
                    bytes.extend_from_slice(&segment.data);
                }
                (ByteObservationKind::Write, Capture::Write(whole)) => {
                    bytes.extend_from_slice(&whole.data);
                }
                (ByteObservationKind::Write, Capture::WriteSegment(segment)) => {
                    bytes.extend_from_slice(&segment.data);
                }
                (ByteObservationKind::InterimResponse, Capture::InterimResponse(whole)) => {
                    bytes.extend_from_slice(&whole.data);
                }
                (
                    ByteObservationKind::InterimResponse,
                    Capture::InterimResponseSegment(segment),
                ) => bytes.extend_from_slice(&segment.data),
                (_, Capture::SegmentEnd(_)) => segment_ends += 1,
                (_, Capture::Close(_)) => {}
                (_, other) => panic!("unexpected capture for {kind:?}: {other:?}"),
            }
        }
    }
    assert!(
        saw_final,
        "the connection must end with a numberOfThisLastChunk record"
    );
    (bytes, segment_ends, largest)
}

fn kind_strategy() -> impl Strategy<Value = ByteObservationKind> {
    prop_oneof![
        Just(ByteObservationKind::Read),
        Just(ByteObservationKind::Write),
        Just(ByteObservationKind::InterimResponse),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Fragmentation is lossless and order-preserving for every budget and every kind, and the
    /// enveloped record never exceeds the payload limit.
    #[test]
    fn fragmentation_is_lossless_and_bounded(
        budget in 120u32..3000,
        payload_len in 0usize..9000,
        kind in kind_strategy(),
    ) {
        let Some(payload_limit) = payload_limit_for_budget(budget) else {
            return Ok(());
        };
        let payload: Vec<u8> = (0..payload_len)
            .map(|i| u8::try_from((i * 31 + 7) % 256).unwrap())
            .collect();
        let timestamp = CaptureTimestamp::new(1_704_067_200, 123_456_789);

        let mut recorder = recorder(payload_limit);
        let emitted = match kind {
            ByteObservationKind::Read => recorder.record_read(timestamp, &payload),
            ByteObservationKind::Write => recorder.record_write(timestamp, &payload),
            ByteObservationKind::InterimResponse => {
                recorder.record_interim_response(timestamp, &payload)
            }
        };
        let emitted = emitted.expect("a 120-byte budget always makes forward progress");
        recorder.close(CaptureTimestamp::EPOCH).expect("close");

        let (reassembled, segment_ends, largest) = replay(&mut recorder, kind);
        prop_assert_eq!(&reassembled, &payload, "bytes must survive fragmentation exactly");
        // segmentEnd is emitted exactly once on the segmented path and never otherwise.
        let segmented = emitted.len() > 1;
        prop_assert_eq!(segment_ends, usize::from(segmented));
        prop_assert!(
            largest <= payload_limit as usize,
            "an enveloped record of {} bytes exceeded the {}-byte limit",
            largest,
            payload_limit
        );
        // Every emitted observation got its own contiguous sequence, including segmentEnd.
        prop_assert_eq!(
            recorder.next_sequence(),
            u64::try_from(emitted.len()).expect("fits") + 2
        );
    }

    /// Several byte events of mixed kinds interleave without losing a byte or a sequence.
    #[test]
    fn interleaved_byte_events_stay_ordered(
        budget in 150u32..1200,
        lengths in prop::collection::vec(0usize..900, 1..8),
    ) {
        let Some(payload_limit) = payload_limit_for_budget(budget) else {
            return Ok(());
        };
        let mut recorder = recorder(payload_limit);
        let mut expected_reads = Vec::new();
        let mut expected_writes = Vec::new();
        for (round, len) in lengths.iter().enumerate() {
            let seconds = 100 + i64::try_from(round).expect("round fits in i64");
            let read: Vec<u8> = (0..*len).map(|i| u8::try_from((i + round) % 256).unwrap()).collect();
            let write: Vec<u8> =
                (0..*len / 2).map(|i| u8::try_from((i * 3 + round) % 256).unwrap()).collect();
            recorder.record_read(CaptureTimestamp::new(seconds, 0), &read).expect("read");
            recorder.commit_end_of_message(CaptureTimestamp::new(seconds, 1))
                .expect("end of message");
            recorder.record_write(CaptureTimestamp::new(seconds, 2), &write).expect("write");
            expected_reads.extend_from_slice(&read);
            expected_writes.extend_from_slice(&write);
        }
        recorder.close(CaptureTimestamp::EPOCH).expect("close");
        drain(&mut recorder);

        let streams = decode_traffic_streams(recorder.sink().records());
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        let mut sequence = 1u64;
        for stream in &streams {
            for observation in &stream.sub_stream {
                prop_assert_eq!(observation.connection_observation_sequence, Some(sequence));
                sequence += 1;
                match observation.capture.as_ref().expect("capture set") {
                    Capture::Read(w) => reads.extend_from_slice(&w.data),
                    Capture::ReadSegment(s) => reads.extend_from_slice(&s.data),
                    Capture::Write(w) => writes.extend_from_slice(&w.data),
                    Capture::WriteSegment(s) => writes.extend_from_slice(&s.data),
                    Capture::SegmentEnd(_)
                    | Capture::EndOfMessageIndicator(_)
                    | Capture::Close(_) => {}
                    other => panic!("unexpected capture: {other:?}"),
                }
            }
        }
        prop_assert_eq!(reads, expected_reads);
        prop_assert_eq!(writes, expected_writes);
        prop_assert_eq!(
            recorder.prior_request_boundaries(),
            u32::try_from(lengths.len()).expect("fits")
        );
    }

    /// The segment solver is exact: the chosen length fits, and one more byte never would.
    #[test]
    fn the_segment_solver_is_exact(
        available in 1u32..400_000,
        remaining in 1u32..400_000,
        sequence in 1u64..u64::MAX,
        seconds in 0i64..4_000_000_000,
        nanos in 0i32..1_000_000_000,
    ) {
        let timestamp = CaptureTimestamp::new(seconds, nanos);
        for (segment_field, data_field) in
            [(field::READ_SEGMENT, field::SEGMENT_DATA), (field::INTERIM_RESPONSE_SEGMENT, field::SEGMENT_DATA)]
        {
            let chosen = max_segment_data_len(
                available, remaining, sequence, timestamp, segment_field, data_field,
            );
            prop_assert!(chosen <= remaining);
            if chosen > 0 {
                prop_assert!(
                    segment_record_cost(chosen, sequence, timestamp, segment_field, data_field)
                        <= available
                );
            }
            if chosen < remaining {
                prop_assert!(
                    segment_record_cost(chosen + 1, sequence, timestamp, segment_field, data_field)
                        > available,
                    "one more byte should not have fit"
                );
            }
        }
    }

    /// The solver's cost model agrees with what the encoder actually writes, for every length.
    #[test]
    fn the_cost_model_matches_the_encoder(
        data_len in 0u32..5000,
        sequence in 1u64..1_000_000_000_000,
        seconds in 0i64..4_000_000_000,
    ) {
        let timestamp = CaptureTimestamp::new(seconds, 0);
        let data = vec![0x33u8; data_len as usize];
        for (segment_field, data_field) in [
            (field::READ_SEGMENT, field::SEGMENT_DATA),
            (field::WRITE_SEGMENT, field::SEGMENT_DATA),
            (field::INTERIM_RESPONSE_SEGMENT, field::SEGMENT_DATA),
        ] {
            let body = ObservationBody::Data { field_number: data_field, bytes: &data };
            let planned = ObservationPlan::new(sequence, timestamp, segment_field, &body);
            prop_assert_eq!(
                planned.record_cost(),
                segment_record_cost(data_len, sequence, timestamp, segment_field, data_field)
            );
        }
    }

    /// Reserving the envelope from the limit is always an upper bound on the envelope of anything
    /// that fits the derived budget. This is the J-1 fix stated as a property.
    #[test]
    fn the_envelope_reservation_is_always_sufficient(payload_limit in 3u32..=u32::MAX) {
        let Some(budget) = size::traffic_stream_budget_for_payload_limit(payload_limit) else {
            return Ok(());
        };
        for stream_len in [0, 1, budget / 2, budget - 1, budget] {
            prop_assert!(
                stream_len + size::capture_record_envelope_overhead(stream_len) <= payload_limit,
                "a {stream_len}-byte stream overflowed a {payload_limit}-byte limit"
            );
        }
    }
}

/// A zero-length byte event is never segmented, whatever the budget.
#[test]
fn an_empty_byte_event_is_never_segmented() {
    for budget in [120u32, 200, 1000] {
        let Some(payload_limit) = payload_limit_for_budget(budget) else {
            continue;
        };
        let mut recorder = recorder(payload_limit);
        recorder
            .record_read(CaptureTimestamp::EPOCH, b"")
            .expect("empty read");
        recorder.close(CaptureTimestamp::EPOCH).expect("close");
        let (bytes, segment_ends, _) = replay(&mut recorder, ByteObservationKind::Read);
        assert!(bytes.is_empty());
        assert_eq!(segment_ends, 0);
    }
}
