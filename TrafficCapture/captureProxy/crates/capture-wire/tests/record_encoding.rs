//! Behavioural obligations ported from `StreamChannelConnectionCaptureSerializerTest` and
//! `KafkaCaptureFactoryTest` (traceability rows WIRE-03, WIRE-08).
//!
//! Case labels B1-B21 and K1-K3 match wire spec §11.

use std::sync::Arc;
use std::time::Duration;

use capture_wire::connection_recorder::decode_traffic_streams;
use capture_wire::proto::traffic_observation::Capture;
use capture_wire::proto::{self, TrafficStream};
use capture_wire::protobuf_size::{self as size, field};
use capture_wire::record_encoder::{ObservationBody, ObservationPlan, RecordHeader};
use capture_wire::record_sink::CollectingSink;
use capture_wire::{
    CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig, EncodedRecord, RecordEncoder,
    WireError,
};

const CONNECTION_ID: &str = "9a25a4fffe620014-00034cfa-00000001-d208faac76346d02-864e38e2";
const NODE_ID: &str = "b671d2f2-577b-414e-9eb4-8bc3e89ee182:1";
const TS: CaptureTimestamp = CaptureTimestamp::new(1_704_067_200, 0);

type Recorder = ConnectionRecorder<CollectingSink>;

fn recorder(payload_limit: Option<u32>) -> Recorder {
    let mut config = ConnectionRecorderConfig::new(CONNECTION_ID, NODE_ID)
        // A long interval keeps the size and terminal triggers isolated from the F deadline.
        .with_flush_interval(Duration::from_hours(1));
    config.payload_limit = payload_limit;
    ConnectionRecorder::new(config, CollectingSink::new()).expect("valid config")
}

/// Acknowledges every publication so the whole chain reaches the sink.
///
/// The chain deliberately holds record N+1 until record N is acknowledged
/// (`proxyCaptureProtocol.md` §4.1), so a test that wants to see every record must play the part of
/// Kafka. Nothing here weakens that invariant: `publish_chain_ordering.rs` asserts it directly.
fn drain(recorder: &mut Recorder) {
    while recorder.chain().in_flight().is_some() {
        recorder.acknowledge_publication();
    }
    while recorder.poll_publication_outcome().is_some() {}
    assert!(
        recorder.publication_failure().is_none(),
        "no publication should have failed"
    );
}

fn streams(recorder: &mut Recorder) -> Vec<TrafficStream> {
    drain(recorder);
    decode_traffic_streams(recorder.sink().records())
}

/// Smallest payload limit whose derived `TrafficStream` budget is exactly `target`.
fn payload_limit_for_budget(target: u32) -> u32 {
    for limit in target..target + 16 {
        if size::traffic_stream_budget_for_payload_limit(limit) == Some(target) {
            return limit;
        }
    }
    panic!("no payload limit yields a {target}-byte budget");
}

fn header(prior: u32, unterminated: bool) -> RecordHeader {
    RecordHeader {
        connection_id: Arc::from(CONNECTION_ID),
        node_id: Arc::from(NODE_ID),
        prior_request_boundaries: prior,
        reads_awaiting_end_of_message: unterminated,
    }
}

fn observation_cost(sequence: u64, capture_field: u32, body: &ObservationBody<'_>) -> u32 {
    ObservationPlan::new(sequence, TS, capture_field, body).record_cost()
}

/// B11 — the golden record.
///
/// read(16 B), read(empty), exception("abcdefghijklmnop"), exception(""), firstLine = 17,
/// headers = 72, end-of-message, close, terminal flush. One record, six observations,
/// `numberOfThisLastChunk == 1`, no `number`, sequences 1..6, and the ids round-trip.
#[test]
fn b11_golden_record() {
    let mut recorder = recorder(None);
    recorder.record_read(TS, b"ABCDEFGHIJKLMNOP").expect("read");
    recorder.record_read(TS, b"").expect("empty read");
    recorder
        .record_connection_exception(TS, "abcdefghijklmnop")
        .expect("exception");
    recorder
        .record_connection_exception(TS, "")
        .expect("empty exception");
    recorder.set_end_of_first_line(17);
    recorder.set_end_of_headers(72);
    recorder.commit_end_of_message(TS).expect("end of message");
    recorder.close(TS).expect("terminal close");

    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1, "exactly one output record");
    let stream = &records[0];
    assert_eq!(stream.connection_id, CONNECTION_ID);
    assert_eq!(stream.node_id, NODE_ID);
    assert_eq!(stream.sub_stream.len(), 6);
    assert_eq!(
        stream.index,
        Some(proto::traffic_stream::Index::NumberOfThisLastChunk(1)),
        "the terminal record carries numberOfThisLastChunk, never number"
    );
    for (position, observation) in stream.sub_stream.iter().enumerate() {
        assert_eq!(
            observation.connection_observation_sequence,
            Some(u64::try_from(position).unwrap() + 1),
            "connectionObservationSequence must be present and contiguous from 1"
        );
    }

    let expected = proto::TrafficStream {
        connection_id: CONNECTION_ID.to_string(),
        node_id: NODE_ID.to_string(),
        // §3.4 snapshot semantics: the header is written when the record is CREATED, before the
        // observation that triggered creation mutates the counters. The record whose own fifth
        // observation is the end-of-message therefore carries the PRE-end-of-message count, which
        // here is zero and is therefore absent. Matches the Java ground truth
        // `makeSampleTrafficStream`, which does not set the field either.
        prior_requests_received: None,
        last_observation_was_unterminated_read: None,
        index: Some(proto::traffic_stream::Index::NumberOfThisLastChunk(1)),
        sub_stream: vec![
            observation(
                1,
                Capture::Read(proto::ReadObservation {
                    data: b"ABCDEFGHIJKLMNOP".as_slice().into(),
                }),
            ),
            observation(
                2,
                Capture::Read(proto::ReadObservation {
                    data: bytes::Bytes::new(),
                }),
            ),
            observation(
                3,
                Capture::ConnectionException(proto::ConnectionExceptionObservation {
                    message: "abcdefghijklmnop".to_string(),
                }),
            ),
            observation(
                4,
                Capture::ConnectionException(proto::ConnectionExceptionObservation {
                    message: String::new(),
                }),
            ),
            observation(
                5,
                Capture::EndOfMessageIndicator(proto::EndOfMessageIndication {
                    first_line_byte_length: Some(17),
                    headers_byte_length: Some(72),
                }),
            ),
            observation(6, Capture::Close(proto::CloseObservation {})),
        ],
    };
    assert_eq!(stream, &expected);
    // The empty read's data really is zero length, not an absent-but-nonzero field.
    match stream.sub_stream[1].capture.as_ref().expect("capture set") {
        Capture::Read(read) => assert_eq!(read.data.len(), 0),
        other => panic!("unexpected capture: {other:?}"),
    }
}

fn observation(sequence: u64, capture: Capture) -> proto::TrafficObservation {
    proto::TrafficObservation {
        ts: Some(prost_types::Timestamp {
            seconds: TS.seconds,
            nanos: TS.nanos,
        }),
        connection_observation_sequence: Some(sequence),
        capture: Some(capture),
    }
}

/// B12 — read then close yields two observations with sequences 1 and 2.
#[test]
fn b12_read_then_close_sequences() {
    let mut recorder = recorder(None);
    recorder.record_read(TS, b"hello").expect("read");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    let sequences: Vec<_> = records[0]
        .sub_stream
        .iter()
        .map(|o| o.connection_observation_sequence)
        .collect();
    assert_eq!(sequences, vec![Some(1), Some(2)]);
}

/// B1 / B2 / B5 — a read much larger than the record budget is segmented, in order, losslessly.
#[test]
fn b1_large_read_is_segmented_in_order_and_losslessly() {
    let budget = 512u32;
    let mut recorder = recorder(Some(payload_limit_for_budget(budget)));
    let payload: Vec<u8> = (0..2500u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    recorder.record_read(TS, &payload).expect("segmented read");
    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    assert!(
        records.len() >= 3,
        "expected at least 3 records, got {}",
        records.len()
    );

    // Record indices are 1..N in order, with exactly one numberOfThisLastChunk, last.
    for (position, stream) in records.iter().enumerate() {
        let index = u32::try_from(position).unwrap() + 1;
        let expected = if position + 1 == records.len() {
            proto::traffic_stream::Index::NumberOfThisLastChunk(i32::try_from(index).unwrap())
        } else {
            proto::traffic_stream::Index::Number(i32::try_from(index).unwrap())
        };
        assert_eq!(
            stream.index,
            Some(expected),
            "record {index} has the wrong index arm"
        );
    }

    // The first observation of the first record is a readSegment, and every segment honours the
    // budget.
    let mut reassembled = Vec::new();
    let mut segment_ends = 0;
    let mut sequence = 0u64;
    for stream in &records {
        for observation in &stream.sub_stream {
            sequence += 1;
            assert_eq!(
                observation.connection_observation_sequence,
                Some(sequence),
                "sequences must be contiguous across record boundaries"
            );
            match observation.capture.as_ref().expect("capture set") {
                Capture::ReadSegment(segment) => {
                    assert!(
                        segment.data.len() <= budget as usize,
                        "a segment exceeded the record budget"
                    );
                    reassembled.extend_from_slice(&segment.data);
                }
                Capture::SegmentEnd(_) => segment_ends += 1,
                Capture::Close(_) => {}
                other => panic!("unexpected capture: {other:?}"),
            }
        }
    }
    assert_eq!(
        reassembled, payload,
        "segments must reassemble to the exact original bytes"
    );
    assert_eq!(
        segment_ends, 1,
        "exactly one EndOfSegmentsIndication for one logical read"
    );

    // The last observation of the whole stream is the close.
    let last = records.last().unwrap().sub_stream.last().unwrap();
    assert!(matches!(last.capture, Some(Capture::Close(_))));
}

/// B4 — a segmented interim response never falls back to `write`, and `segmentEnd` is last.
#[test]
fn b4_segmented_interim_response_stays_interim() {
    let mut recorder = recorder(Some(payload_limit_for_budget(256)));
    let interim: Vec<u8> = (0..960u32).map(|i| u8::try_from(i % 97).unwrap()).collect();
    recorder
        .record_interim_response(TS, &interim)
        .expect("segmented interim");
    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    let mut reassembled = Vec::new();
    let mut captures = Vec::new();
    for stream in &records {
        for observation in &stream.sub_stream {
            match observation.capture.as_ref().expect("capture set") {
                Capture::InterimResponseSegment(segment) => {
                    reassembled.extend_from_slice(&segment.data);
                    captures.push("interimResponseSegment");
                }
                Capture::SegmentEnd(_) => captures.push("segmentEnd"),
                Capture::Close(_) => captures.push("close"),
                other => panic!("interim bytes must not fall back to another kind: {other:?}"),
            }
        }
    }
    assert_eq!(reassembled, interim);
    assert_eq!(captures.iter().filter(|c| **c == "segmentEnd").count(), 1);
    // segmentEnd is the last observation of the segmented value, immediately before the close.
    let close_at = captures
        .iter()
        .position(|c| *c == "close")
        .expect("close present");
    assert_eq!(captures[close_at - 1], "segmentEnd");
    assert!(!captures.contains(&"write"));
}

/// B18 — an unsegmented write emits zero `segmentEnd` observations.
#[test]
fn b18_unsegmented_write_emits_no_segment_end() {
    let mut recorder = recorder(Some(payload_limit_for_budget(500)));
    recorder
        .record_write(TS, &[0x5Au8; 160])
        .expect("whole write");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    let segment_ends: usize = records
        .iter()
        .flat_map(|s| &s.sub_stream)
        .filter(|o| matches!(o.capture, Some(Capture::SegmentEnd(_))))
        .count();
    assert_eq!(segment_ends, 0);
    let writes: usize = records
        .iter()
        .flat_map(|s| &s.sub_stream)
        .filter(|o| matches!(o.capture, Some(Capture::Write(_))))
        .count();
    assert_eq!(writes, 1);
}

/// B8 — two empty writes both parse with a zero-length `data`.
#[test]
fn b8_empty_writes_round_trip() {
    let mut recorder = recorder(None);
    recorder.record_write(TS, b"").expect("empty write");
    recorder.record_write(TS, b"").expect("empty write");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    let datas: Vec<usize> = records[0]
        .sub_stream
        .iter()
        .filter_map(|o| match o.capture.as_ref() {
            Some(Capture::Write(write)) => Some(write.data.len()),
            _ => None,
        })
        .collect();
    assert_eq!(datas, vec![0, 0]);
}

/// B7 — a record sized to hold one write but not the following close publishes two records, the
/// first holding the write and the second the close.
#[test]
fn b7_the_close_triggers_a_size_flush() {
    let data = [0x11u8; 40];
    let write_body = ObservationBody::Data {
        field_number: field::WHOLE_DATA,
        bytes: &data,
    };
    let write_cost = observation_cost(1, field::WRITE, &write_body);
    let close_cost = observation_cost(2, field::CLOSE, &ObservationBody::Empty);
    let fixed = header(0, false).leading_len() + header(0, false).trailer_len(1);
    // One byte short of room for the close.
    let budget = fixed + write_cost + close_cost - 1;

    let mut recorder = recorder(Some(payload_limit_for_budget(budget)));
    recorder.record_write(TS, &data).expect("write");
    assert_eq!(recorder.sink().records().len(), 0, "nothing published yet");

    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].sub_stream.len(), 1);
    assert!(matches!(
        records[0].sub_stream[0].capture,
        Some(Capture::Write(_))
    ));
    assert_eq!(
        records[0].index,
        Some(proto::traffic_stream::Index::Number(1))
    );
    assert_eq!(records[1].sub_stream.len(), 1);
    assert!(matches!(
        records[1].sub_stream[0].capture,
        Some(Capture::Close(_))
    ));
    assert_eq!(
        records[1].index,
        Some(proto::traffic_stream::Index::NumberOfThisLastChunk(2))
    );
}

/// B6 — a budget with no room for even one data byte fails the data observation, and a terminal
/// close still completes.
#[test]
fn b6_no_forward_progress_still_permits_a_terminal_close() {
    let close_cost = observation_cost(1, field::CLOSE, &ObservationBody::Empty);
    let fixed = header(0, false).leading_len() + header(0, false).trailer_len(1);
    // Room for an empty observation, but nowhere near room for a one-byte write.
    let budget = fixed + close_cost;
    let mut recorder = recorder(Some(payload_limit_for_budget(budget)));

    let outcome = recorder.record_write(TS, b"X");
    assert!(
        matches!(outcome, Err(WireError::NoForwardProgress)),
        "expected no-forward-progress, got {outcome:?}"
    );

    recorder
        .close(TS)
        .expect("a terminal close still completes");
    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sub_stream.len(), 1);
    assert!(matches!(
        records[0].sub_stream[0].capture,
        Some(Capture::Close(_))
    ));
}

/// B10 — an unbounded record is never segmented.
#[test]
fn b10_unbounded_records_are_never_segmented() {
    let mut recorder = recorder(None);
    recorder
        .record_write(TS, &vec![1u8; 200_000])
        .expect("write");
    recorder
        .record_write(TS, &vec![2u8; 200_000])
        .expect("write");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sub_stream.len(), 3);
    assert!(records[0].sub_stream.iter().all(|o| !matches!(
        o.capture,
        Some(Capture::WriteSegment(_) | Capture::SegmentEnd(_))
    )));
}

/// B19 — long writer and connection ids round-trip; there is no artificial id length limit.
#[test]
fn b19_long_identities_round_trip() {
    let node = "b".repeat(51);
    let connection = "c".repeat(59);
    let config = ConnectionRecorderConfig::new(connection.as_str(), node.as_str())
        .with_flush_interval(Duration::from_hours(1))
        .with_payload_limit(payload_limit_for_budget(1024));
    let mut recorder =
        ConnectionRecorder::new(config, CollectingSink::new()).expect("valid config");
    recorder.close(TS).expect("close");
    while recorder.chain().in_flight().is_some() {
        recorder.acknowledge_publication();
    }
    let records = decode_traffic_streams(recorder.sink().records());
    assert_eq!(records[0].connection_id, connection);
    assert_eq!(records[0].node_id, node);
}

/// B20 — a dropped request advances the successor record's `priorRequestsReceived`.
#[test]
fn b20_intentionally_dropped_request_advances_the_successor_baseline() {
    let mut recorder = recorder(None);
    recorder
        .record_read(TS, b"GET / HTTP/1.1\r\n")
        .expect("read");
    assert!(recorder.reads_awaiting_end_of_message());
    recorder
        .cancel_capture_for_current_request(TS)
        .expect("drop");
    assert!(!recorder.reads_awaiting_end_of_message());
    recorder.flush().expect("non-final flush");
    recorder
        .record_read(TS, b"GET /next HTTP/1.1\r\n")
        .expect("read");
    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    assert_eq!(records.len(), 2);
    assert!(matches!(
        records[0].sub_stream[1].capture,
        Some(Capture::RequestDropped(_))
    ));
    // Absent on the first record (count was 0), present with 1 on the successor.
    assert_eq!(records[0].prior_requests_received, None);
    assert_eq!(records[1].prior_requests_received, Some(1));
    // The successor also inherits lastObservationWasUnterminatedRead = absent, because the drop
    // cleared it before the record was created.
    assert_eq!(records[1].last_observation_was_unterminated_read, None);
}

/// `priorRequestsReceived` is absent at zero, never present-zero, and
/// `lastObservationWasUnterminatedRead` is absent at false, never present-false.
#[test]
fn optional_continuity_fields_are_absent_rather_than_defaulted() {
    let mut recorder = recorder(None);
    recorder.record_read(TS, b"partial").expect("read");
    recorder.flush().expect("flush");
    recorder.record_read(TS, b"more").expect("read");
    recorder.commit_end_of_message(TS).expect("end of message");
    recorder.flush().expect("flush");
    recorder
        .record_write(TS, b"HTTP/1.1 200 OK\r\n\r\n")
        .expect("write");
    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    assert_eq!(records.len(), 3);
    // Record 1: nothing accumulated yet.
    assert_eq!(records[0].prior_requests_received, None);
    assert_eq!(records[0].last_observation_was_unterminated_read, None);
    // Record 2: a read is outstanding, no request has completed.
    assert_eq!(records[1].prior_requests_received, None);
    assert_eq!(
        records[1].last_observation_was_unterminated_read,
        Some(true)
    );
    // Record 3: one request completed, no read outstanding.
    assert_eq!(records[2].prior_requests_received, Some(1));
    assert_eq!(records[2].last_observation_was_unterminated_read, None);
}

/// Only `read`/`readSegment` set `lastObservationWasUnterminatedRead`.
#[test]
fn writes_and_interim_responses_never_set_the_unterminated_read_flag() {
    for kind in ["write", "interim"] {
        let mut recorder = recorder(None);
        match kind {
            "write" => recorder.record_write(TS, b"body").expect("write"),
            _ => recorder
                .record_interim_response(TS, b"HTTP/1.1 100 Continue\r\n\r\n")
                .expect("interim"),
        };
        assert!(
            !recorder.reads_awaiting_end_of_message(),
            "{kind} must not mark reads as awaiting an end-of-message"
        );
        recorder.flush().expect("flush");
        recorder.record_write(TS, b"x").expect("write");
        recorder.close(TS).expect("close");
        let records = streams(&mut recorder);
        assert!(
            records
                .iter()
                .all(|s| s.last_observation_was_unterminated_read.is_none()),
            "{kind} leaked lastObservationWasUnterminatedRead"
        );
    }
}

/// `EndOfMessageIndication` writes both fields always, including the `-1` sentinel.
#[test]
fn end_of_message_always_writes_both_length_fields() {
    let mut recorder = recorder(None);
    recorder
        .commit_end_of_message(TS)
        .expect("end of message with unknown lengths");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    match records[0].sub_stream[0]
        .capture
        .as_ref()
        .expect("capture set")
    {
        Capture::EndOfMessageIndicator(eom) => {
            assert_eq!(
                eom.first_line_byte_length,
                Some(capture_wire::UNKNOWN_MESSAGE_LENGTH)
            );
            assert_eq!(
                eom.headers_byte_length,
                Some(capture_wire::UNKNOWN_MESSAGE_LENGTH)
            );
        }
        other => panic!("unexpected capture: {other:?}"),
    }
    // Both lengths reset after a commit, so the next indicator is a sentinel pair again.
    assert_eq!(records[0].sub_stream.len(), 2);
}

/// K2 — the J-1 fix. Every enveloped record stays within the publisher's payload ceiling, for every
/// configured maximum Kafka message size, and the largest record still uses most of the budget.
#[test]
fn k2_every_enveloped_record_honours_the_payload_limit() {
    for max_message_size in [1024u32 * 1024, 8 * 1024 * 1024] {
        // KafkaCaptureFactory reserves 500 bytes of Kafka overhead before the record budget.
        let payload_limit = max_message_size - 500;
        let mut recorder = recorder(Some(payload_limit));
        let payload: Vec<u8> = (0..(max_message_size as usize + 16 * 1024))
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        recorder.record_read(TS, &payload).expect("segmented read");
        recorder.close(TS).expect("close");
        drain(&mut recorder);

        let records = recorder.sink().records();
        assert!(
            records.len() > 1,
            "a payload above the limit must span records"
        );
        let mut largest = 0usize;
        for record in records {
            let enveloped = record.payload_len();
            assert!(
                enveloped <= payload_limit as usize,
                "an enveloped record of {enveloped} bytes exceeded the {payload_limit}-byte limit"
            );
            assert_eq!(
                record.connection_id(),
                CONNECTION_ID,
                "every record keys on connectionId"
            );
            largest = largest.max(enveloped);
        }
        // Not merely safe but tight: the Java test requires the largest record to be within two
        // Kafka overhead reservations of the ceiling.
        assert!(
            largest >= payload_limit as usize - 2 * 500,
            "largest record was only {largest} bytes of a {payload_limit}-byte budget"
        );

        // Reassembly is exact.
        let streams = decode_traffic_streams(records);
        let mut reassembled = Vec::new();
        for stream in &streams {
            for observation in &stream.sub_stream {
                if let Some(Capture::ReadSegment(segment)) = observation.capture.as_ref() {
                    reassembled.extend_from_slice(&segment.data);
                }
            }
        }
        assert_eq!(reassembled, payload);
    }
}

/// The record index is 1-based and contiguous, and continues across every flush trigger.
#[test]
fn record_indices_are_one_based_and_contiguous_across_all_flush_triggers() {
    let mut recorder = recorder(Some(payload_limit_for_budget(400)));
    recorder.record_read(TS, b"small").expect("read");
    recorder.flush().expect("explicit flush");
    recorder
        .record_write(TS, &[9u8; 2000])
        .expect("size-triggered segmentation");
    recorder.close(TS).expect("close");

    let records = streams(&mut recorder);
    let indices: Vec<i32> = records
        .iter()
        .map(|stream| match stream.index.as_ref().expect("index set") {
            proto::traffic_stream::Index::Number(n)
            | proto::traffic_stream::Index::NumberOfThisLastChunk(n) => *n,
        })
        .collect();
    let expected: Vec<i32> = (1..=i32::try_from(records.len()).unwrap()).collect();
    assert_eq!(indices, expected);
    // Exactly one numberOfThisLastChunk, and it is last.
    let finals: Vec<bool> = records
        .iter()
        .map(|s| {
            matches!(
                s.index,
                Some(proto::traffic_stream::Index::NumberOfThisLastChunk(_))
            )
        })
        .collect();
    assert_eq!(finals.iter().filter(|f| **f).count(), 1);
    assert!(*finals.last().unwrap());
}

/// Nothing may follow the terminal close, and a second close is refused.
#[test]
fn no_observation_may_follow_the_terminal_close() {
    let mut recorder = recorder(None);
    recorder.record_read(TS, b"req").expect("read");
    recorder.close(TS).expect("close");

    assert!(matches!(
        recorder.record_read(TS, b"late"),
        Err(WireError::AfterTerminalClose)
    ));
    assert!(matches!(
        recorder.record_write(TS, b"late"),
        Err(WireError::AfterTerminalClose)
    ));
    assert!(matches!(
        recorder.record_request_forwarded(TS),
        Err(WireError::AfterTerminalClose)
    ));
    assert!(matches!(
        recorder.commit_end_of_message(TS),
        Err(WireError::AfterTerminalClose)
    ));
    assert!(matches!(
        recorder.close(TS),
        Err(WireError::AfterTerminalClose)
    ));
    // A non-final flush after the close is a no-op, never a new record.
    assert!(recorder.flush().expect("flush is a no-op").is_none());
    drain(&mut recorder);
    assert_eq!(recorder.sink().records().len(), 1);
}

/// `connectionException` is non-terminal: capture continues after it.
#[test]
fn connection_exception_is_not_terminal() {
    let mut recorder = recorder(None);
    recorder
        .record_connection_exception(TS, "connection reset by peer")
        .expect("exception");
    recorder
        .record_write(TS, b"after")
        .expect("write after the exception");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    assert_eq!(records[0].sub_stream.len(), 3);
    assert!(matches!(
        records[0].sub_stream[0].capture,
        Some(Capture::ConnectionException(_))
    ));
}

/// `DEVIATION: J-5` — an oversized diagnostic message is truncated, not a capture failure.
#[test]
fn an_oversized_exception_message_is_truncated() {
    let mut recorder = recorder(None);
    let huge = "e".repeat(64 * 1024);
    recorder
        .record_connection_exception(TS, &huge)
        .expect("exception is never a hard failure");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    match records[0].sub_stream[0]
        .capture
        .as_ref()
        .expect("capture set")
    {
        Capture::ConnectionException(exception) => {
            assert_eq!(
                exception.message.len(),
                capture_wire::record_encoder::MAX_EXCEPTION_MESSAGE_BYTES
            );
        }
        other => panic!("unexpected capture: {other:?}"),
    }
}

/// Truncation never splits a UTF-8 character.
#[test]
fn exception_truncation_respects_character_boundaries() {
    let mut recorder = recorder(None);
    // Each 'é' is two bytes, so the 1024-byte bound lands mid-character without a boundary check.
    let message = "é".repeat(2000);
    recorder
        .record_connection_exception(TS, &message)
        .expect("exception");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    match records[0].sub_stream[0]
        .capture
        .as_ref()
        .expect("capture set")
    {
        Capture::ConnectionException(exception) => {
            assert!(exception.message.len() <= 1024);
            assert!(exception.message.chars().all(|c| c == 'é'));
        }
        other => panic!("unexpected capture: {other:?}"),
    }
}

/// The dead observation kinds are emittable for catalogue completeness and consume a sequence like
/// any other observation, but nothing in this crate emits them.
#[test]
fn the_dead_observation_kinds_still_respect_sequencing() {
    let mut recorder = recorder(None);
    assert_eq!(recorder.record_bind(TS).expect("bind"), 1);
    assert_eq!(recorder.record_connect(TS).expect("connect"), 2);
    assert_eq!(recorder.record_disconnect(TS).expect("disconnect"), 3);
    // disconnect is explicitly NOT terminal.
    assert!(!recorder.is_terminated());
    recorder.close(TS).expect("close");
    assert_eq!(recorder.next_sequence(), 5);
    let records = streams(&mut recorder);
    assert_eq!(records[0].sub_stream.len(), 4);
    assert!(matches!(
        records[0].sub_stream[0].capture,
        Some(Capture::Bind(_))
    ));
    assert!(matches!(
        records[0].sub_stream[1].capture,
        Some(Capture::Connect(_))
    ));
    assert!(matches!(
        records[0].sub_stream[2].capture,
        Some(Capture::Disconnect(_))
    ));
    assert!(matches!(
        records[0].sub_stream[3].capture,
        Some(Capture::Close(_))
    ));
}

/// `requestForwarded` carries an empty body and only its own timestamp.
#[test]
fn request_forwarded_is_an_empty_marker() {
    let forward_ts = CaptureTimestamp::new(1_704_067_201, 500);
    let mut recorder = recorder(None);
    recorder
        .record_read(TS, b"POST / HTTP/1.1\r\n\r\n")
        .expect("read");
    recorder.commit_end_of_message(TS).expect("end of message");
    recorder
        .record_request_forwarded(forward_ts)
        .expect("forwarded");
    recorder.close(TS).expect("close");
    let records = streams(&mut recorder);
    let forwarded = &records[0].sub_stream[2];
    assert!(matches!(
        forwarded.capture,
        Some(Capture::RequestForwarded(_))
    ));
    assert_eq!(
        forwarded.ts,
        Some(prost_types::Timestamp {
            seconds: forward_ts.seconds,
            nanos: forward_ts.nanos
        })
    );
}

/// Configuration guards.
#[test]
fn configuration_is_validated() {
    assert!(matches!(
        ConnectionRecorder::new(
            ConnectionRecorderConfig::new("", NODE_ID),
            CollectingSink::new()
        ),
        Err(WireError::EmptyConnectionId)
    ));
    assert!(matches!(
        ConnectionRecorder::new(
            ConnectionRecorderConfig::new(CONNECTION_ID, ""),
            CollectingSink::new()
        ),
        Err(WireError::EmptyNodeId)
    ));
    // DEVIATION: the documented protocol rejects F = 0; Java silently disables deadlines.
    assert!(matches!(
        ConnectionRecorder::new(
            ConnectionRecorderConfig::new(CONNECTION_ID, NODE_ID)
                .with_flush_interval(Duration::ZERO),
            CollectingSink::new()
        ),
        Err(WireError::NonPositiveFlushInterval(_))
    ));
    assert!(matches!(
        ConnectionRecorder::new(
            ConnectionRecorderConfig::new(CONNECTION_ID, NODE_ID).with_payload_limit(10),
            CollectingSink::new()
        ),
        Err(WireError::PayloadLimitTooSmall { .. })
    ));
}

/// The encoder is reachable directly, and a detached record exposes the identity a publisher must
/// cross-check before sending (wire spec §2).
#[test]
fn a_detached_record_exposes_its_routing_identity() {
    let mut recorder = recorder(None);
    recorder.close(TS).expect("close");
    drain(&mut recorder);
    let record: &EncodedRecord = &recorder.sink().records()[0];
    assert_eq!(record.connection_id(), CONNECTION_ID);
    assert_eq!(record.node_id(), NODE_ID);
    assert_eq!(record.index(), 1);
    assert!(record.is_final());
    assert_eq!(record.observation_count(), 1);
    assert_eq!(record.first_sequence(), 1);
    assert_eq!(record.last_sequence(), 1);
    // The payload is one CaptureRecord; the TrafficStream is the envelope's only content.
    let overhead = size::capture_record_envelope_overhead(
        u32::try_from(record.traffic_stream_bytes().len()).unwrap(),
    );
    assert_eq!(
        record.payload_len(),
        record.traffic_stream_bytes().len() + overhead as usize
    );
}

/// An unused import guard: `RecordEncoder` is part of the public surface this suite relies on.
#[test]
fn record_encoder_is_publicly_constructible() {
    let encoder = RecordEncoder::start(
        capture_wire::RecordBuf::new(64),
        header(0, false),
        1,
        Some(4096),
    );
    assert!(encoder.is_empty());
    assert_eq!(encoder.index(), 1);
    assert!(encoder.space_left().is_some());
}

/// Byte-identity with canonical protobuf.
///
/// The single-pass encoder must produce exactly the bytes `prost` would produce from the equivalent
/// message tree, because that is exactly what Java ships: its hand-rolled buffer is re-parsed and
/// re-serialized canonically before publication (wire spec §2, §10.1).
///
/// `interimResponseSegment`(19) is the one exception and is excluded here: Java's generated writer
/// emits strictly ascending field numbers (18 before 19) while `prost` places the whole `Capture`
/// oneof at its minimum tag and emits 19 before 18. This encoder follows Java; see
/// `record_encoder::tests::interim_response_segment_emits_field_18_before_field_19_like_java`.
#[test]
fn every_record_is_byte_identical_to_canonical_prost_output() {
    use prost::Message;

    let mut recorder = recorder(Some(payload_limit_for_budget(700)));
    recorder
        .record_read(TS, b"GET /_cluster/health HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("read");
    recorder.set_end_of_first_line(31);
    recorder.set_end_of_headers(9);
    recorder.commit_end_of_message(TS).expect("end of message");
    recorder
        .record_request_forwarded(CaptureTimestamp::new(1_704_067_201, 7))
        .expect("forwarded");
    recorder
        .record_interim_response(TS, b"HTTP/1.1 100 Continue\r\n\r\n")
        .expect("interim");
    recorder
        .record_write(TS, &vec![0x7Eu8; 3000])
        .expect("segmented write");
    recorder
        .record_connection_exception(TS, "broken pipe")
        .expect("exception");
    recorder.close(TS).expect("close");
    drain(&mut recorder);

    let records = recorder.sink().records();
    assert!(
        records.len() > 1,
        "the write must have been segmented across records"
    );
    for record in records {
        let envelope =
            proto::CaptureRecord::decode(record.payload()).expect("payload is a CaptureRecord");
        assert_eq!(
            record.payload(),
            envelope.encode_to_vec(),
            "record {} is not byte-identical to canonical protobuf",
            record.index()
        );
    }
}

/// A poisoned publication chain refuses further observations and surfaces the original failure.
///
/// `DEVIATION`: Java's serializer keeps accepting observations after a publication failure and
/// relies on an outer layer to stop capture; this crate fails closed. See the `DEVIATION` comment
/// in `connection_recorder::append_observation`.
#[test]
fn a_poisoned_chain_refuses_further_observations() {
    use capture_wire::record_sink::SinkError;

    let mut recorder = recorder(None);
    recorder.record_read(TS, b"first").expect("read");
    recorder
        .sink_mut()
        .fail_next_submit(SinkError::new("broker down"));
    recorder.flush().expect("flush is still accepted");

    assert!(recorder.publication_failure().is_some());
    match recorder.record_read(TS, b"second") {
        Err(WireError::PoisonedPublishChain(failure)) => {
            assert_eq!(failure.error().message(), "broker down");
        }
        other => panic!("expected a poisoned-chain error, got {other:?}"),
    }
}
