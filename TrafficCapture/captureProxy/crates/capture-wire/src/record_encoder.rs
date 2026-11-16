//! Single-pass canonical-protobuf record encoder (traceability row WIRE-08).
//!
//! # Why one pass is byte-identical to Java
//!
//! Java does **not** ship the bytes its serializer wrote. It hand-rolls a `TrafficStream` into a
//! bounded `CodedOutputStream`, re-parses those bytes with `TrafficStream.parseFrom`
//! (`KafkaCaptureFactory`:1189), and re-serializes the result inside the envelope with
//! `CaptureRecord.newBuilder().setTrafficStream(ts).build().toByteArray()`
//! (`CaptureKafkaPublisher`:490-494). The only observable effect of that round trip is
//! normalization: the shipped bytes are canonical protobuf, in ascending field-number order, with
//! proto3 implicit-presence defaults dropped.
//!
//! This encoder therefore emits the canonical form directly, in one pass, with no intermediate
//! message tree and no re-serialization. The normalizations the collapse must also perform (wire
//! spec §10.1) are all handled here:
//!
//! 1. `Timestamp.seconds == 0` is omitted — see
//!    [`canonical_size_of_timestamp`](crate::protobuf_size::canonical_size_of_timestamp);
//! 2. `Timestamp.nanos == 0` is omitted;
//! 3. an empty `connectionId`/`nodeId` would be omitted (both are rejected at construction);
//! 4. `priorRequestsReceived` is written only when `> 0` and
//!    `lastObservationWasUnterminatedRead` only when `true` — never present-zero or present-false,
//!    which would flip `hasPriorRequestsReceived()` on the replayer;
//! 5. `connectionObservationSequence` is written **always**, including value 1;
//! 6. `EndOfMessageIndication` writes both fields always, including the `-1` sentinels;
//! 7. an empty `bytes data` omits the `data` field entirely rather than writing a zero-length one.
//!
//! # Field order
//!
//! Canonical order is ascending field number, which is why the record can be streamed:
//!
//! * `TrafficStream`: `connectionId`(1), `subStream`(2)\*, `number`(3) | `numberOfThisLastChunk`(4),
//!   `nodeId`(5), `priorRequestsReceived`(6), `lastObservationWasUnterminatedRead`(7). The header
//!   field 1 is written at creation, observations stream into field 2, and fields 3-7 form a
//!   trailer written at detach — still strictly ascending.
//! * `TrafficObservation`: `ts`(1), then the oneof case and `connectionObservationSequence`(18) in
//!   ascending numeric order. For every capture field below 18 the oneof comes first; for
//!   `interimResponseSegment`(19) the sequence comes first.
//!
//! Because the trailer is written last but reserved at creation, and because every trailer field's
//! value is a creation-time snapshot (wire spec §3.4) with an exactly known encoded size, the
//! reservation is exact — not pessimistic.

use std::sync::Arc;

use crate::error::WireError;
use crate::protobuf_size::{
    self as size, CaptureTimestamp, WIRE_TYPE_LENGTH_DELIMITED, WIRE_TYPE_VARINT, field,
};
use crate::record_buffer::{EncodedRecord, RecordBuf};

/// Largest `ConnectionExceptionObservation.message` this encoder will emit, in bytes.
///
/// `DEVIATION: J-5`. Java cannot segment `connectionException` (`addStringMessage` has no chunking),
/// so a long `Throwable.getMessage()` overflows the bounded output stream and becomes a capture
/// failure. The replayer never reads the field (wire spec §1.8), so the wire spec's J-5
/// recommendation is to truncate to a fixed small bound instead. Truncation is applied on a UTF-8
/// character boundary and is purely diagnostic.
pub const MAX_EXCEPTION_MESSAGE_BYTES: usize = 1024;

/// The per-connection values stamped into every record of a connection.
///
/// `connection_id` and `node_id` are immutable for the connection's life; the other two are
/// snapshots taken **when the record is created**, i.e. before the observation that triggered
/// creation mutates them (wire spec §3.4).
#[derive(Debug, Clone)]
pub struct RecordHeader {
    /// `TrafficStream.connectionId`. Non-empty.
    pub connection_id: Arc<str>,
    /// `TrafficStream.nodeId`, the writer identity. Non-empty.
    pub node_id: Arc<str>,
    /// `TrafficStream.priorRequestsReceived`. Written only when `> 0`.
    ///
    /// Counts request *boundaries*, completed and intentionally dropped alike (wire spec §3.2,
    /// discrepancy D-7). Getting this wrong desynchronizes every replayed request id on the
    /// connection after the first dropped request.
    pub prior_request_boundaries: u32,
    /// `TrafficStream.lastObservationWasUnterminatedRead`. Written only when `true`.
    ///
    /// Set by `read`/`readSegment` only, never by `write`, `writeSegment`, `interimResponse`, or
    /// `interimResponseSegment`.
    pub reads_awaiting_end_of_message: bool,
}

impl RecordHeader {
    /// Encoded size of the fields written at record creation: `connectionId` only.
    #[must_use]
    pub fn leading_len(&self) -> u32 {
        size::string_size(field::CONNECTION_ID, &self.connection_id)
    }

    /// Encoded size of the trailer fields `index`, `nodeId`, `priorRequestsReceived`,
    /// `lastObservationWasUnterminatedRead` for a record whose index is `record_index`.
    ///
    /// Both arms of the `index` oneof have a one-byte tag and carry the same value, so the
    /// reservation does not depend on whether the record turns out to be final.
    ///
    /// # Panics
    ///
    /// Panics if `record_index` exceeds `i32::MAX`; the caller rejects that earlier with
    /// [`WireError::RecordIndexOverflow`].
    #[must_use]
    pub fn trailer_len(&self, record_index: u32) -> u32 {
        let index = i32::try_from(record_index).expect("record index is checked against i32::MAX");
        let index_size = size::int32_size(field::NUMBER_OF_THIS_LAST_CHUNK, index);
        let node_size = size::string_size(field::NODE_ID, &self.node_id);
        let prior_size = if self.prior_request_boundaries > 0 {
            size::int32_size(
                field::PRIOR_REQUESTS_RECEIVED,
                i32::try_from(self.prior_request_boundaries).unwrap_or(i32::MAX),
            )
        } else {
            0
        };
        let unterminated_size = if self.reads_awaiting_end_of_message {
            size::bool_size(field::LAST_OBSERVATION_WAS_UNTERMINATED_READ)
        } else {
            0
        };
        index_size + node_size + prior_size + unterminated_size
    }
}

/// The body of one `TrafficObservation` capture oneof case.
///
/// Pairing the body with its own length computation is deliberate: a plan built from the body can
/// never disagree with the bytes later written from it.
#[derive(Debug, Clone, Copy)]
pub enum ObservationBody<'a> {
    /// An empty message: `close`, `requestForwarded`, `segmentEnd`, `requestDropped`, `bind`,
    /// `connect`, `disconnect`.
    Empty,
    /// A `bytes data` field. An empty slice omits the field, leaving an empty submessage.
    Data {
        /// `data` field number: 1 for whole observations, **2** for `*SegmentObservation`.
        field_number: u32,
        /// The captured bytes, verbatim.
        bytes: &'a [u8],
    },
    /// A `string` field. The empty string omits the field.
    Text {
        /// Field number of the string.
        field_number: u32,
        /// The text.
        text: &'a str,
    },
    /// `EndOfMessageIndication`. Both fields are explicit-presence and both are always written,
    /// including the `-1` sentinel Java uses when the decoder result is unavailable (defect J-6).
    EndOfMessage {
        /// `firstLineByteLength`, or `-1` when unknown.
        first_line_byte_length: i32,
        /// `headersByteLength`, or `-1` when unknown.
        headers_byte_length: i32,
    },
}

impl ObservationBody<'_> {
    /// Encoded length of this body, i.e. the length prefix of the capture submessage.
    ///
    /// # Panics
    ///
    /// Panics if a `Data` or `Text` body is larger than `u32::MAX`, which no capture buffer is.
    #[must_use]
    pub fn encoded_len(&self) -> u32 {
        match *self {
            Self::Empty => 0,
            Self::Data {
                field_number,
                bytes,
            } => {
                let len = u32::try_from(bytes.len()).expect("capture payloads fit in u32");
                if len == 0 {
                    0
                } else {
                    size::length_delimited_size(field_number, len)
                }
            }
            Self::Text { field_number, text } => size::string_size(field_number, text),
            Self::EndOfMessage {
                first_line_byte_length,
                headers_byte_length,
            } => {
                size::int32_size(field::FIRST_LINE_BYTE_LENGTH, first_line_byte_length)
                    + size::int32_size(field::HEADERS_BYTE_LENGTH, headers_byte_length)
            }
        }
    }
}

/// Everything about one observation that must be known **before** any byte of it is committed.
///
/// The budget decision, and therefore the record and segment boundaries, are functions of this
/// value. Note that the sequence number is allocated before the plan is built, because its varint
/// width is part of the size (wire spec §10.2.7).
#[derive(Debug, Clone, Copy)]
pub struct ObservationPlan {
    /// `connectionObservationSequence` for this observation.
    pub sequence: u64,
    /// The observation's `ts`.
    pub timestamp: CaptureTimestamp,
    /// The `TrafficObservation` capture oneof field number.
    pub capture_field: u32,
    /// Encoded length of the capture submessage body.
    pub capture_body_len: u32,
    /// Encoded length of the canonical `Timestamp` submessage body.
    pub timestamp_content_len: u32,
    /// Encoded length of the whole `TrafficObservation` body.
    pub observation_content_len: u32,
}

impl ObservationPlan {
    /// Builds the plan for one observation.
    #[must_use]
    pub fn new(
        sequence: u64,
        timestamp: CaptureTimestamp,
        capture_field: u32,
        body: &ObservationBody<'_>,
    ) -> Self {
        let capture_body_len = body.encoded_len();
        let timestamp_content_len = size::canonical_size_of_timestamp(timestamp);
        // `ts` is always present, so an empty Timestamp still costs its tag and a zero length.
        let ts_part = size::length_delimited_size(field::TS, timestamp_content_len);
        let sequence_part = size::uint64_size(field::CONNECTION_OBSERVATION_SEQUENCE, sequence);
        let capture_part = size::length_delimited_size(capture_field, capture_body_len);
        Self {
            sequence,
            timestamp,
            capture_field,
            capture_body_len,
            timestamp_content_len,
            observation_content_len: ts_part + sequence_part + capture_part,
        }
    }

    /// Bytes this observation adds to the record: the `subStream` tag, its length, and the body.
    #[must_use]
    pub fn record_cost(&self) -> u32 {
        size::bytes_needed_for_observation(self.observation_content_len)
    }
}

/// A record under construction for one connection.
///
/// Created by [`ConnectionRecorder`](crate::connection_recorder::ConnectionRecorder) when the first
/// observation of a new record is admitted, and consumed by [`RecordEncoder::finish`] at detach.
/// Dropping it without finishing releases the buffer and publishes nothing — that is the
/// `abort` path which closes Java defect J-3.
#[derive(Debug)]
pub struct RecordEncoder {
    buffer: RecordBuf,
    header: RecordHeader,
    /// `TrafficStream` content budget, or `None` when unbounded.
    budget: Option<u32>,
    trailer_reserve: u32,
    index: u32,
    observation_count: u32,
    close_count: u32,
    last_was_close: bool,
    first_sequence: u64,
    last_sequence: u64,
}

impl RecordEncoder {
    /// Bytes available in a brand-new record of this shape for `subStream` content.
    ///
    /// `None` means unbounded.
    #[must_use]
    pub fn fresh_space(header: &RecordHeader, index: u32, budget: Option<u32>) -> Option<u32> {
        budget.map(|budget| {
            budget
                .saturating_sub(header.leading_len())
                .saturating_sub(header.trailer_len(index))
        })
    }

    /// Starts a record: allocates the buffer and writes `connectionId`.
    #[must_use]
    pub fn start(buffer: RecordBuf, header: RecordHeader, index: u32, budget: Option<u32>) -> Self {
        let trailer_reserve = header.trailer_len(index);
        let mut encoder = Self {
            buffer,
            header,
            budget,
            trailer_reserve,
            index,
            observation_count: 0,
            close_count: 0,
            last_was_close: false,
            first_sequence: 0,
            last_sequence: 0,
        };
        encoder.write_leading_header();
        encoder
    }

    fn write_leading_header(&mut self) {
        // Canonical order starts at field 1. An empty connectionId would be omitted, but the
        // recorder rejects an empty one at construction.
        if !self.header.connection_id.is_empty() {
            let bytes = self.header.connection_id.as_bytes();
            self.buffer
                .push_tag(field::CONNECTION_ID, WIRE_TYPE_LENGTH_DELIMITED);
            self.buffer.push_varint(bytes.len() as u64);
            self.buffer.extend_from_slice(bytes);
        }
    }

    /// The record's 1-based per-connection index.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// How many observations have been written.
    #[must_use]
    pub fn observation_count(&self) -> u32 {
        self.observation_count
    }

    /// Whether the record holds no observations yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.observation_count == 0
    }

    /// Bytes of `TrafficStream` content written so far.
    ///
    /// # Panics
    ///
    /// Panics if the record somehow exceeded `u32::MAX` bytes, which the budget forbids.
    #[must_use]
    pub fn content_len(&self) -> u32 {
        u32::try_from(self.buffer.content_len()).expect("a record is far below u32::MAX bytes")
    }

    /// Bytes still available for `subStream` content, or `None` when unbounded.
    ///
    /// The trailer reservation is already subtracted, so a value returned here can be spent
    /// entirely on observations.
    #[must_use]
    pub fn space_left(&self) -> Option<u32> {
        self.budget.map(|budget| {
            budget
                .saturating_sub(self.content_len())
                .saturating_sub(self.trailer_reserve)
        })
    }

    /// Whether an observation of `observation_content_len` content bytes still fits.
    #[must_use]
    pub fn can_admit(&self, observation_content_len: u32) -> bool {
        match self.space_left() {
            None => true,
            Some(space) => size::bytes_needed_for_observation(observation_content_len) <= space,
        }
    }

    /// Writes one observation. The caller must have confirmed [`RecordEncoder::can_admit`].
    ///
    /// # Panics
    ///
    /// Panics if the observation does not fit, or if `plan` disagrees with `body`; both indicate an
    /// accounting bug in this crate rather than bad input.
    pub fn write_observation(&mut self, plan: &ObservationPlan, body: &ObservationBody<'_>) {
        assert_eq!(
            plan.capture_body_len,
            body.encoded_len(),
            "observation plan and body disagree about the capture body length"
        );
        assert!(
            self.can_admit(plan.observation_content_len),
            "write_observation called without space: needs {} bytes, {:?} left",
            plan.record_cost(),
            self.space_left()
        );

        let before = self.buffer.content_len();

        // TrafficStream.subStream, length-delimited.
        self.buffer
            .push_tag(field::SUB_STREAM, WIRE_TYPE_LENGTH_DELIMITED);
        self.buffer
            .push_varint(u64::from(plan.observation_content_len));

        // TrafficObservation.ts (field 1) is always present.
        self.buffer.push_tag(field::TS, WIRE_TYPE_LENGTH_DELIMITED);
        self.buffer
            .push_varint(u64::from(plan.timestamp_content_len));
        if plan.timestamp.seconds != 0 {
            self.buffer
                .push_tag(field::TIMESTAMP_SECONDS, WIRE_TYPE_VARINT);
            self.buffer
                .push_varint(plan.timestamp.seconds.cast_unsigned());
        }
        if plan.timestamp.nanos != 0 {
            self.buffer
                .push_tag(field::TIMESTAMP_NANOS, WIRE_TYPE_VARINT);
            self.buffer
                .push_varint(i64::from(plan.timestamp.nanos).cast_unsigned());
        }

        // Canonical order is ascending field number, and connectionObservationSequence is field 18,
        // so it precedes only interimResponseSegment(19).
        if plan.capture_field < field::CONNECTION_OBSERVATION_SEQUENCE {
            self.write_capture(plan, body);
            self.write_sequence(plan.sequence);
        } else {
            self.write_sequence(plan.sequence);
            self.write_capture(plan, body);
        }

        let written = u32::try_from(self.buffer.content_len() - before)
            .expect("one observation is far below u32::MAX bytes");
        debug_assert_eq!(
            written,
            plan.record_cost(),
            "observation wrote {written} bytes but was planned at {}",
            plan.record_cost()
        );

        self.observation_count += 1;
        if self.observation_count == 1 {
            self.first_sequence = plan.sequence;
        }
        self.last_sequence = plan.sequence;
        if plan.capture_field == field::CLOSE {
            self.close_count += 1;
            self.last_was_close = true;
        } else {
            self.last_was_close = false;
        }
    }

    fn write_sequence(&mut self, sequence: u64) {
        self.buffer
            .push_tag(field::CONNECTION_OBSERVATION_SEQUENCE, WIRE_TYPE_VARINT);
        self.buffer.push_varint(sequence);
    }

    fn write_capture(&mut self, plan: &ObservationPlan, body: &ObservationBody<'_>) {
        self.buffer
            .push_tag(plan.capture_field, WIRE_TYPE_LENGTH_DELIMITED);
        self.buffer.push_varint(u64::from(plan.capture_body_len));
        match *body {
            ObservationBody::Empty => {}
            ObservationBody::Data {
                field_number,
                bytes,
            } => {
                if !bytes.is_empty() {
                    self.buffer
                        .push_tag(field_number, WIRE_TYPE_LENGTH_DELIMITED);
                    self.buffer.push_varint(bytes.len() as u64);
                    self.buffer.extend_from_slice(bytes);
                }
            }
            ObservationBody::Text { field_number, text } => {
                if !text.is_empty() {
                    self.buffer
                        .push_tag(field_number, WIRE_TYPE_LENGTH_DELIMITED);
                    self.buffer.push_varint(text.len() as u64);
                    self.buffer.extend_from_slice(text.as_bytes());
                }
            }
            ObservationBody::EndOfMessage {
                first_line_byte_length,
                headers_byte_length,
            } => {
                self.buffer
                    .push_tag(field::FIRST_LINE_BYTE_LENGTH, WIRE_TYPE_VARINT);
                self.buffer
                    .push_varint(i64::from(first_line_byte_length).cast_unsigned());
                self.buffer
                    .push_tag(field::HEADERS_BYTE_LENGTH, WIRE_TYPE_VARINT);
                self.buffer
                    .push_varint(i64::from(headers_byte_length).cast_unsigned());
            }
        }
    }

    /// Writes the trailer and seals the record into an [`EncodedRecord`].
    ///
    /// # Errors
    ///
    /// * [`WireError::EmptyRecord`] if the record holds no observations. An idle connection emits no
    ///   empty `TrafficStream` (`proxyCaptureProtocol.md` §4.2), and refusing a terminal flush with
    ///   nothing to publish is also the fix for Java defect J-2.
    /// * [`WireError::TerminalRecordInvariant`] unless `is_final` is exactly equivalent to "this
    ///   record holds exactly one `close` and it is the final observation" — the biconditional
    ///   `KafkaCaptureFactory.isTerminalRecord` enforces by declaring the whole process unstable
    ///   (wire spec §8).
    /// * [`WireError::RecordOverflow`] if the finished body exceeded its budget. That would be an
    ///   accounting bug here, not bad input.
    ///
    /// On any error the record — and therefore its buffer — is consumed and released.
    pub fn finish(mut self, is_final: bool) -> Result<EncodedRecord, WireError> {
        if self.observation_count == 0 {
            return Err(WireError::EmptyRecord);
        }
        let close_is_last = self.close_count == 1 && self.last_was_close;
        if is_final != close_is_last {
            return Err(WireError::TerminalRecordInvariant {
                is_final,
                close_count: self.close_count,
                close_is_last,
            });
        }

        let index = i32::try_from(self.index).map_err(|_| WireError::RecordIndexOverflow)?;
        let index_field = if is_final {
            field::NUMBER_OF_THIS_LAST_CHUNK
        } else {
            field::NUMBER
        };
        self.buffer.push_tag(index_field, WIRE_TYPE_VARINT);
        self.buffer.push_varint(i64::from(index).cast_unsigned());

        if !self.header.node_id.is_empty() {
            let bytes = self.header.node_id.as_bytes();
            self.buffer
                .push_tag(field::NODE_ID, WIRE_TYPE_LENGTH_DELIMITED);
            self.buffer.push_varint(bytes.len() as u64);
            self.buffer.extend_from_slice(bytes);
        }
        // Absent, never present-zero: the replayer's hasPriorRequestsReceived() is load-bearing.
        if self.header.prior_request_boundaries > 0 {
            let prior = i32::try_from(self.header.prior_request_boundaries)
                .map_err(|_| WireError::RecordIndexOverflow)?;
            self.buffer
                .push_tag(field::PRIOR_REQUESTS_RECEIVED, WIRE_TYPE_VARINT);
            self.buffer.push_varint(i64::from(prior).cast_unsigned());
        }
        // Absent, never present-false.
        if self.header.reads_awaiting_end_of_message {
            self.buffer.push_tag(
                field::LAST_OBSERVATION_WAS_UNTERMINATED_READ,
                WIRE_TYPE_VARINT,
            );
            self.buffer.push(1);
        }

        if let Some(budget) = self.budget {
            let written = self.content_len();
            if written > budget {
                return Err(WireError::RecordOverflow { written, budget });
            }
        }

        Ok(EncodedRecord::seal(
            self.buffer,
            self.header.connection_id,
            self.header.node_id,
            self.index,
            is_final,
            self.observation_count,
            self.first_sequence,
            self.last_sequence,
        ))
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    use crate::proto;

    fn header() -> RecordHeader {
        RecordHeader {
            connection_id: Arc::from("conn-1"),
            node_id: Arc::from("node-1"),
            prior_request_boundaries: 0,
            reads_awaiting_end_of_message: false,
        }
    }

    fn decode(record: &EncodedRecord) -> proto::TrafficStream {
        let envelope =
            proto::CaptureRecord::decode(record.payload()).expect("payload is a CaptureRecord");
        match envelope.payload.expect("payload oneof is set") {
            proto::capture_record::Payload::TrafficStream(ts) => ts,
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn plan_lengths_match_prost_encoded_len() {
        let ts = CaptureTimestamp::new(1_704_067_200, 123_456_789);
        let data = b"hello world".to_vec();
        let body = ObservationBody::Data {
            field_number: field::WHOLE_DATA,
            bytes: &data,
        };
        let plan = ObservationPlan::new(7, ts, field::READ, &body);

        let expected = proto::TrafficObservation {
            ts: Some(prost_types::Timestamp {
                seconds: ts.seconds,
                nanos: ts.nanos,
            }),
            connection_observation_sequence: Some(7),
            capture: Some(proto::traffic_observation::Capture::Read(
                proto::ReadObservation {
                    data: data.clone().into(),
                },
            )),
        };
        assert_eq!(
            plan.observation_content_len as usize,
            expected.encoded_len()
        );
    }

    #[test]
    fn a_minimal_record_round_trips_canonically() {
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        let body = ObservationBody::Empty;
        let plan = ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::CLOSE, &body);
        encoder.write_observation(&plan, &body);
        let record = encoder
            .finish(true)
            .expect("terminal record with a trailing close");

        let stream = decode(&record);
        assert_eq!(stream.connection_id, "conn-1");
        assert_eq!(stream.node_id, "node-1");
        assert_eq!(
            stream.index,
            Some(proto::traffic_stream::Index::NumberOfThisLastChunk(1))
        );
        assert_eq!(stream.prior_requests_received, None);
        assert_eq!(stream.last_observation_was_unterminated_read, None);
        assert_eq!(stream.sub_stream.len(), 1);
        assert_eq!(
            stream.sub_stream[0].connection_observation_sequence,
            Some(1)
        );
        // Wire spec §10.1.1: an EPOCH timestamp is an empty, present Timestamp submessage.
        assert_eq!(
            stream.sub_stream[0].ts,
            Some(prost_types::Timestamp {
                seconds: 0,
                nanos: 0
            })
        );

        // The single-pass bytes must equal what prost would produce from the message tree.
        assert_eq!(
            record.payload(),
            proto::CaptureRecord {
                payload: Some(proto::capture_record::Payload::TrafficStream(stream)),
            }
            .encode_to_vec()
        );
    }

    /// `interimResponseSegment` is the one observation whose canonical byte order differs between
    /// the Java generated writer and `prost`'s derive:
    ///
    /// * Java's `TrafficObservation.writeTo` emits strictly ascending field numbers, so
    ///   `connectionObservationSequence`(18) precedes `interimResponseSegment`(19)
    ///   (verified in `captureProtobufs/build/generated/.../TrafficObservation.java`:708-714);
    /// * `prost` places the whole `Capture` oneof at its *minimum* tag (2) and therefore emits
    ///   field 19 before field 18.
    ///
    /// Both parse to the same message, but only Java's order is byte-identical to what the existing
    /// producer ships. This encoder follows Java.
    #[test]
    fn interim_response_segment_emits_field_18_before_field_19_like_java() {
        let data = b"xyz".to_vec();
        let body = ObservationBody::Data {
            field_number: field::SEGMENT_DATA,
            bytes: &data,
        };
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 3, None);
        let plan = ObservationPlan::new(
            5,
            CaptureTimestamp::new(10, 0),
            field::INTERIM_RESPONSE_SEGMENT,
            &body,
        );
        encoder.write_observation(&plan, &body);
        let record = encoder.finish(false).expect("non-final record");

        let observation = record.traffic_stream_bytes();
        let tag_18: &[u8] = &[0x90, 0x01];
        let tag_19: &[u8] = &[0x9A, 0x01];
        let at_18 = observation
            .windows(2)
            .position(|w| w == tag_18)
            .expect("field 18 tag present");
        let at_19 = observation
            .windows(2)
            .position(|w| w == tag_19)
            .expect("field 19 tag present");
        assert!(
            at_18 < at_19,
            "Java emits connectionObservationSequence(18) before field 19"
        );

        // Still semantically identical, and prost's own output differs only in field order.
        let stream = decode(&record);
        assert_eq!(
            stream.sub_stream[0].connection_observation_sequence,
            Some(5)
        );
        match stream.sub_stream[0].capture.as_ref().expect("capture set") {
            proto::traffic_observation::Capture::InterimResponseSegment(segment) => {
                assert_eq!(&segment.data[..], b"xyz");
            }
            other => panic!("unexpected capture case: {other:?}"),
        }
        let prost_bytes = proto::CaptureRecord {
            payload: Some(proto::capture_record::Payload::TrafficStream(
                stream.clone(),
            )),
        }
        .encode_to_vec();
        assert_eq!(
            prost_bytes.len(),
            record.payload().len(),
            "the two orders must at least agree on length"
        );
        assert_eq!(
            proto::CaptureRecord::decode(prost_bytes.as_slice()).expect("decodes"),
            proto::CaptureRecord {
                payload: Some(proto::capture_record::Payload::TrafficStream(stream)),
            },
            "both orders parse to the same message"
        );
    }

    #[test]
    fn the_terminal_biconditional_rejects_all_four_error_shapes() {
        let empty = ObservationBody::Empty;

        // (1) numberOfThisLastChunk with no close.
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        let plan =
            ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::REQUEST_FORWARDED, &empty);
        encoder.write_observation(&plan, &empty);
        assert!(matches!(
            encoder.finish(true),
            Err(WireError::TerminalRecordInvariant { close_count: 0, .. })
        ));

        // (2) numberOfThisLastChunk whose close is not last.
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        let close = ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::CLOSE, &empty);
        encoder.write_observation(&close, &empty);
        let after =
            ObservationPlan::new(2, CaptureTimestamp::EPOCH, field::REQUEST_FORWARDED, &empty);
        encoder.write_observation(&after, &empty);
        assert!(matches!(
            encoder.finish(true),
            Err(WireError::TerminalRecordInvariant {
                close_count: 1,
                close_is_last: false,
                ..
            })
        ));

        // (3) numberOfThisLastChunk with two closes.
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        for seq in 1..=2 {
            let plan = ObservationPlan::new(seq, CaptureTimestamp::EPOCH, field::CLOSE, &empty);
            encoder.write_observation(&plan, &empty);
        }
        assert!(matches!(
            encoder.finish(true),
            Err(WireError::TerminalRecordInvariant { close_count: 2, .. })
        ));

        // (4) a non-final record ending in a close.
        let mut encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        let plan = ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::CLOSE, &empty);
        encoder.write_observation(&plan, &empty);
        assert!(matches!(
            encoder.finish(false),
            Err(WireError::TerminalRecordInvariant {
                is_final: false,
                close_is_last: true,
                ..
            })
        ));
    }

    #[test]
    fn an_empty_record_is_never_published() {
        let encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        assert!(matches!(encoder.finish(false), Err(WireError::EmptyRecord)));
        let encoder = RecordEncoder::start(RecordBuf::new(64), header(), 1, None);
        assert!(matches!(encoder.finish(true), Err(WireError::EmptyRecord)));
    }

    #[test]
    fn the_trailer_reservation_is_exact() {
        for (prior, unterminated, index) in [
            (0u32, false, 1u32),
            (1, false, 1),
            (0, true, 1),
            (200, true, 300),
            (100_000, true, 2_000_000),
        ] {
            let header = RecordHeader {
                connection_id: Arc::from("9a25a4fffe620014-00034cfa-00000001-d208faac76346d02"),
                node_id: Arc::from("b671d2f2-577b-414e-9eb4-8bc3e89ee182:17"),
                prior_request_boundaries: prior,
                reads_awaiting_end_of_message: unterminated,
            };
            let reserve = header.trailer_len(index);
            let leading = header.leading_len();
            let mut encoder = RecordEncoder::start(RecordBuf::new(256), header, index, Some(4096));
            assert_eq!(encoder.content_len(), leading);
            let empty = ObservationBody::Empty;
            let plan = ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::CLOSE, &empty);
            encoder.write_observation(&plan, &empty);
            let before_trailer = encoder.content_len();
            let record = encoder.finish(true).expect("valid terminal record");
            let after = u32::try_from(record.traffic_stream_bytes().len()).expect("fits");
            assert_eq!(
                after - before_trailer,
                reserve,
                "trailer reservation must be exact, not pessimistic"
            );
        }
    }
}
