//! Exact port of the Java capture layer's protobuf size arithmetic (traceability row WIRE-02).
//!
//! Every record boundary and every segment boundary in the capture protocol is derived from these
//! functions, so they are ported bit-for-bit from
//! `com.google.protobuf.CodedOutputStream` plus
//! `TrafficCapture/captureOffloader/.../CodedOutputStreamSizeUtil.java` (`SIZE` in the wire spec).
//!
//! Two families live here and must not be confused:
//!
//! * the primitive accounting (`varint_len`, `tag_size`, `int32_size`, …) which the single-pass
//!   encoder uses to decide, *before* committing an observation, whether it fits;
//! * the Java-exact helpers ([`max_bytes_needed_for_a_segmented_observation`],
//!   [`compute_max_length_delimited_field_size_for_space`]) which reproduce Java's deliberately
//!   pessimistic estimates. The encoder does **not** use these for packing decisions (see the
//!   `DEVIATION` notes in [`crate::segmentation`]); they are retained because they are part of the
//!   frozen WIRE-02 arithmetic and serve as the parity oracle for the ported Java vectors.
//!
//! Sizes are `u32` because a `CaptureRecord` is bounded far below `u32::MAX`. The two Java-exact
//! space helpers use `i32` because their Java originals return negative values for degenerate
//! inputs and callers rely on that sign.

use std::time::SystemTime;

/// Protobuf field numbers of the capture schema.
///
/// Transcribed from `TrafficCapture/captureProtobufs/src/main/proto/TrafficCaptureStream.proto`.
/// Fields 16-19 of `TrafficObservation` have **two-byte** tags; assuming one byte mis-sizes every
/// `requestDropped`, `interimResponse`, `interimResponseSegment` and every
/// `connectionObservationSequence` (wire spec §10.3).
pub mod field {
    /// `CaptureRecord.trafficStream`.
    pub const CAPTURE_RECORD_TRAFFIC_STREAM: u32 = 1;

    /// `TrafficStream.connectionId`.
    pub const CONNECTION_ID: u32 = 1;
    /// `TrafficStream.subStream`.
    pub const SUB_STREAM: u32 = 2;
    /// `TrafficStream.number` (non-final arm of the `index` oneof).
    pub const NUMBER: u32 = 3;
    /// `TrafficStream.numberOfThisLastChunk` (final arm of the `index` oneof).
    pub const NUMBER_OF_THIS_LAST_CHUNK: u32 = 4;
    /// `TrafficStream.nodeId`.
    pub const NODE_ID: u32 = 5;
    /// `TrafficStream.priorRequestsReceived`.
    pub const PRIOR_REQUESTS_RECEIVED: u32 = 6;
    /// `TrafficStream.lastObservationWasUnterminatedRead`.
    pub const LAST_OBSERVATION_WAS_UNTERMINATED_READ: u32 = 7;

    /// `TrafficObservation.ts`.
    pub const TS: u32 = 1;
    /// `TrafficObservation.bind` — declared but never emitted by any production path.
    pub const BIND: u32 = 2;
    /// `TrafficObservation.connect` — declared but never emitted by any production path.
    pub const CONNECT: u32 = 3;
    /// `TrafficObservation.read`.
    pub const READ: u32 = 4;
    /// `TrafficObservation.readSegment`.
    pub const READ_SEGMENT: u32 = 5;
    /// `TrafficObservation.requestForwarded`.
    pub const REQUEST_FORWARDED: u32 = 6;
    /// `TrafficObservation.write`.
    pub const WRITE: u32 = 8;
    /// `TrafficObservation.writeSegment`.
    pub const WRITE_SEGMENT: u32 = 9;
    /// `TrafficObservation.disconnect` — declared but never emitted by any production path.
    pub const DISCONNECT: u32 = 10;
    /// `TrafficObservation.close`.
    pub const CLOSE: u32 = 11;
    /// `TrafficObservation.connectionException`.
    pub const CONNECTION_EXCEPTION: u32 = 12;
    /// `TrafficObservation.segmentEnd`.
    pub const SEGMENT_END: u32 = 14;
    /// `TrafficObservation.endOfMessageIndicator`.
    pub const END_OF_MESSAGE_INDICATOR: u32 = 15;
    /// `TrafficObservation.requestDropped` — two-byte tag.
    pub const REQUEST_DROPPED: u32 = 16;
    /// `TrafficObservation.interimResponse` — two-byte tag.
    pub const INTERIM_RESPONSE: u32 = 17;
    /// `TrafficObservation.connectionObservationSequence` — two-byte tag.
    pub const CONNECTION_OBSERVATION_SEQUENCE: u32 = 18;
    /// `TrafficObservation.interimResponseSegment` — two-byte tag.
    pub const INTERIM_RESPONSE_SEGMENT: u32 = 19;

    /// `data` of every whole byte-carrying observation body.
    pub const WHOLE_DATA: u32 = 1;
    /// `data` of every `*SegmentObservation` body. **Field 2, not 1** (wire spec §10.3).
    pub const SEGMENT_DATA: u32 = 2;
    /// `ConnectionExceptionObservation.message`.
    pub const EXCEPTION_MESSAGE: u32 = 1;
    /// `EndOfMessageIndication.firstLineByteLength`.
    pub const FIRST_LINE_BYTE_LENGTH: u32 = 1;
    /// `EndOfMessageIndication.headersByteLength`: u32.
    pub const HEADERS_BYTE_LENGTH: u32 = 2;

    /// `google.protobuf.Timestamp.seconds`.
    pub const TIMESTAMP_SECONDS: u32 = 1;
    /// `google.protobuf.Timestamp.nanos`.
    pub const TIMESTAMP_NANOS: u32 = 2;
}

/// Protobuf wire type 0 (varint).
pub const WIRE_TYPE_VARINT: u32 = 0;
/// Protobuf wire type 2 (length-delimited).
pub const WIRE_TYPE_LENGTH_DELIMITED: u32 = 2;

/// A capture observation timestamp, split exactly as `google.protobuf.Timestamp` carries it.
///
/// The capture protocol has no clock abstraction on the Java side: every observation timestamp is a
/// bare `Instant.now()` (wire spec §4.1). This type keeps the wire representation explicit so the
/// size arithmetic and the encoder cannot disagree about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CaptureTimestamp {
    /// Seconds since the Unix epoch. May be negative for pre-epoch instants.
    pub seconds: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    pub nanos: i32,
}

impl CaptureTimestamp {
    /// The Unix epoch. Encodes to an **empty** `Timestamp` submessage in canonical protobuf.
    pub const EPOCH: Self = Self {
        seconds: 0,
        nanos: 0,
    };

    /// Builds a timestamp from an explicit second/nanosecond pair.
    #[must_use]
    pub const fn new(seconds: i64, nanos: i32) -> Self {
        Self { seconds, nanos }
    }

    /// Reads the current wall-clock time.
    ///
    /// # Panics
    ///
    /// Panics if the system clock is set before the Unix epoch by more than `i64::MAX` seconds,
    /// which is not representable in `google.protobuf.Timestamp`.
    #[must_use]
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Converts a [`SystemTime`] into its protobuf timestamp components.
    ///
    /// # Panics
    ///
    /// Panics if the duration between `time` and the Unix epoch exceeds `i64::MAX` seconds.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(after) => Self {
                seconds: i64::try_from(after.as_secs()).expect("timestamp seconds fit in i64"),
                nanos: after.subsec_nanos().cast_signed(),
            },
            Err(before) => {
                let before = before.duration();
                let secs = i64::try_from(before.as_secs()).expect("timestamp seconds fit in i64");
                let nanos = before.subsec_nanos().cast_signed();
                if nanos == 0 {
                    Self {
                        seconds: -secs,
                        nanos: 0,
                    }
                } else {
                    // google.protobuf.Timestamp requires 0 <= nanos < 1e9, so borrow a second.
                    Self {
                        seconds: -secs - 1,
                        nanos: 1_000_000_000 - nanos,
                    }
                }
            }
        }
    }
}

/// Number of bytes a protobuf base-128 varint occupies.
///
/// Mirrors `CodedOutputStream.computeUInt64SizeNoTag`: 1 byte for `value <= 127`, 2 for
/// `<= 16383`, 3 for `<= 2097151`, 4 for `<= 268435455`, and so on up to 10.
#[must_use]
pub const fn varint_len(value: u64) -> u32 {
    let significant_bits = 64 - value.leading_zeros();
    let bits = if significant_bits == 0 {
        1
    } else {
        significant_bits
    };
    bits.div_ceil(7)
}

/// Number of bytes the tag of `field_number` occupies: 1 for fields 1-15, 2 for 16-2047.
///
/// # Panics
///
/// Panics if `field_number` is zero or above the protobuf maximum of `536_870_911`, which cannot
/// originate from the frozen capture schema.
#[must_use]
pub const fn tag_size(field_number: u32) -> u32 {
    assert!(
        field_number > 0 && field_number <= 536_870_911,
        "field number outside the legal protobuf range"
    );
    varint_len((field_number as u64) << 3)
}

/// `CodedOutputStream.computeUInt32SizeNoTag`.
#[must_use]
pub const fn uint32_size_no_tag(value: u32) -> u32 {
    varint_len(value as u64)
}

/// `CodedOutputStream.computeUInt64SizeNoTag`.
#[must_use]
pub const fn uint64_size_no_tag(value: u64) -> u32 {
    varint_len(value)
}

/// `CodedOutputStream.computeInt32SizeNoTag`.
///
/// A negative `int32` is sign-extended to 64 bits and therefore always occupies **exactly 10
/// bytes**. This is load-bearing for `EndOfMessageIndication`, which writes `-1` sentinels
/// (wire spec §5.7, defect J-6).
#[must_use]
pub const fn int32_size_no_tag(value: i32) -> u32 {
    if value >= 0 {
        varint_len(value.cast_unsigned() as u64)
    } else {
        10
    }
}

/// `CodedOutputStream.computeInt64SizeNoTag`: 10 bytes for any negative value.
#[must_use]
pub const fn int64_size_no_tag(value: i64) -> u32 {
    if value >= 0 {
        varint_len(value.cast_unsigned())
    } else {
        10
    }
}

/// `CodedOutputStream.computeUInt32Size`.
#[must_use]
pub const fn uint32_size(field_number: u32, value: u32) -> u32 {
    tag_size(field_number) + uint32_size_no_tag(value)
}

/// `CodedOutputStream.computeUInt64Size`.
#[must_use]
pub const fn uint64_size(field_number: u32, value: u64) -> u32 {
    tag_size(field_number) + uint64_size_no_tag(value)
}

/// `CodedOutputStream.computeInt32Size`.
#[must_use]
pub const fn int32_size(field_number: u32, value: i32) -> u32 {
    tag_size(field_number) + int32_size_no_tag(value)
}

/// `CodedOutputStream.computeInt64Size`.
#[must_use]
pub const fn int64_size(field_number: u32, value: i64) -> u32 {
    tag_size(field_number) + int64_size_no_tag(value)
}

/// `CodedOutputStream.computeBoolSize`: tag plus one payload byte.
#[must_use]
pub const fn bool_size(field_number: u32) -> u32 {
    tag_size(field_number) + 1
}

/// Bytes a length-delimited field of `len` payload bytes occupies, tag included.
#[must_use]
pub const fn length_delimited_size(field_number: u32, len: u32) -> u32 {
    tag_size(field_number) + uint32_size_no_tag(len) + len
}

/// Bytes a UTF-8 `string` field occupies, tag included.
///
/// Returns 0 for the empty string: proto3 implicit-presence string fields are omitted at their
/// default value, and the canonical bytes the replayer consumes omit them (wire spec §10.1.3).
///
/// # Panics
///
/// Panics if `value` is longer than `u32::MAX` bytes.
#[must_use]
pub fn string_size(field_number: u32, value: &str) -> u32 {
    let len = u32::try_from(value.len()).expect("capture strings are far below u32::MAX");
    if len == 0 {
        0
    } else {
        length_delimited_size(field_number, len)
    }
}

/// `CodedOutputStreamSizeUtil.computeByteBufRemainingSizeNoTag`.
///
/// `readable` is the number of *readable* bytes, never the buffer capacity; the Java tests
/// `test_computeByteBufRemainingSize_emptyBufWithCapacity` / `_ByteBufAtCapacity` /
/// `_EmptyByteBuf` exist specifically to pin that.
#[must_use]
pub const fn bytes_remaining_size_no_tag(readable: u32) -> u32 {
    uint32_size_no_tag(readable) + readable
}

/// `CodedOutputStreamSizeUtil.computeByteBufRemainingSize`.
#[must_use]
pub const fn bytes_remaining_size(field_number: u32, readable: u32) -> u32 {
    tag_size(field_number) + bytes_remaining_size_no_tag(readable)
}

/// `CodedOutputStreamSizeUtil.getSizeOfTimestamp` — the **Java-exact** form.
///
/// `seconds` is always counted, even when zero; `nanos` is omitted when zero
/// (`SIZE`:22-28, `SER`:435-438). This is the budgeting form Java uses. It is *not* the size of the
/// canonical bytes that reach Kafka, because Java's parse/re-serialize round trip drops an explicit
/// `seconds = 0` — for that use [`canonical_size_of_timestamp`].
#[must_use]
pub const fn size_of_timestamp(timestamp: CaptureTimestamp) -> u32 {
    let sec_size = int64_size(field::TIMESTAMP_SECONDS, timestamp.seconds);
    let nano_size = if timestamp.nanos == 0 {
        0
    } else {
        int32_size(field::TIMESTAMP_NANOS, timestamp.nanos)
    };
    sec_size + nano_size
}

/// Size of the canonical `google.protobuf.Timestamp` submessage body.
///
/// Both proto3 implicit-presence fields are omitted at their default, so `EPOCH` encodes to an
/// empty submessage. This is what the Java proxy actually ships, because `KafkaCaptureFactory`
/// re-parses the hand-rolled buffer and `CaptureKafkaPublisher` re-serializes it canonically
/// (wire spec §2, §10.1.1).
#[must_use]
pub const fn canonical_size_of_timestamp(timestamp: CaptureTimestamp) -> u32 {
    let sec_size = if timestamp.seconds == 0 {
        0
    } else {
        int64_size(field::TIMESTAMP_SECONDS, timestamp.seconds)
    };
    let nano_size = if timestamp.nanos == 0 {
        0
    } else {
        int32_size(field::TIMESTAMP_NANOS, timestamp.nanos)
    };
    sec_size + nano_size
}

/// `CodedOutputStreamSizeUtil.bytesNeededForObservationAndClosingIndex`.
///
/// Bytes needed to append an observation of `observation_content_size` content bytes to a
/// `TrafficStream` **plus** the closing `index` field that the flush will write.
#[must_use]
pub const fn bytes_needed_for_observation_and_closing_index(
    observation_content_size: u32,
    record_index: i32,
) -> u32 {
    let observation_tag_size = uint32_size(field::SUB_STREAM, observation_content_size);
    let index_size = int32_size(field::NUMBER_OF_THIS_LAST_CHUNK, record_index);
    observation_tag_size + observation_content_size + index_size
}

/// Bytes an observation of `observation_content_size` content bytes adds to a `TrafficStream`.
///
/// This is [`bytes_needed_for_observation_and_closing_index`] without the closing-index term. The
/// single-pass encoder reserves the closing index (and the record trailer) exactly once, at record
/// creation, so it must not re-reserve it per observation.
#[must_use]
pub const fn bytes_needed_for_observation(observation_content_size: u32) -> u32 {
    uint32_size(field::SUB_STREAM, observation_content_size) + observation_content_size
}

/// `CodedOutputStreamSizeUtil.maxBytesNeededForASegmentedObservation` — the **Java-exact** form.
///
/// Note the two deliberate pessimisms Java bakes in, both reproduced here:
///
/// * the timestamp is sized with [`size_of_timestamp`], which counts a zero `seconds`;
/// * the closing index is sized at `Integer.MAX_VALUE`, a fixed 6 bytes, which over-reserves by
///   4 bytes for any record index below 128 (`SIZE`:58-61).
///
/// The encoder does not use this function to place segments (see [`crate::segmentation`]); it is
/// kept because WIRE-02 is an exact-port row and because it is the oracle for the ported Java
/// vector `testMaxBytesNeededForASegmentedObservation`.
#[must_use]
pub const fn max_bytes_needed_for_a_segmented_observation(
    timestamp: CaptureTimestamp,
    observation_field_number: u32,
    data_field_number: u32,
    readable: u32,
    connection_observation_sequence: u64,
) -> u32 {
    let ts_content_size = size_of_timestamp(timestamp);
    let ts_tag_and_content_size =
        int32_size(field::TS, ts_content_size.cast_signed()) + ts_content_size;
    let ordering_metadata_size = uint64_size(
        field::CONNECTION_OBSERVATION_SEQUENCE,
        connection_observation_sequence,
    );
    let data_size = bytes_remaining_size(data_field_number, readable);
    let capture_tag_and_content_size =
        int32_size(observation_field_number, data_size.cast_signed()) + data_size;
    bytes_needed_for_observation_and_closing_index(
        ts_tag_and_content_size + ordering_metadata_size + capture_tag_and_content_size,
        i32::MAX,
    )
}

/// `StreamChannelConnectionCaptureSerializer.computeMaxLengthDelimitedFieldSizeForSpace`.
///
/// Sizes the length prefix from the *whole* available space rather than from the answer, so it can
/// return exactly one byte less than optimal at a varint boundary (128, 16384, 2097152, 2^28).
/// Signed because the Java original returns negative values for degenerate inputs and its callers
/// test the sign.
#[must_use]
pub const fn compute_max_length_delimited_field_size_for_space(
    total_available_space: i32,
    requested_field_size: i32,
) -> i32 {
    let pessimistic_length_field_space =
        uint32_size_no_tag(total_available_space.cast_unsigned()).cast_signed();
    let max_write_bytes_space = total_available_space - pessimistic_length_field_space;
    if max_write_bytes_space < requested_field_size {
        max_write_bytes_space
    } else {
        requested_field_size
    }
}

/// The optimal counterpart of [`compute_max_length_delimited_field_size_for_space`].
///
/// Ported from `StreamChannelConnectionCaptureSerializerTest`'s
/// `optimalComputeMaxLengthDelimitedFieldSizeForSpace`, which is the reference oracle the Java
/// `@CsvSource` asserts against. The implementation under test must equal this value or this value
/// minus one.
#[must_use]
pub const fn optimal_max_length_delimited_field_size_for_space(
    total_available_space: i32,
    requested_field_size: i32,
) -> i32 {
    let max_length_field_space =
        uint32_size_no_tag(total_available_space.cast_unsigned()).cast_signed();
    let initial_estimated_max_write_space = total_available_space - max_length_field_space;
    let length_field_space_for_initial_and_one_more =
        uint32_size_no_tag((initial_estimated_max_write_space + 1).cast_unsigned()).cast_signed();
    let max_write_bytes_space = total_available_space - length_field_space_for_initial_and_one_more;
    if max_write_bytes_space < requested_field_size {
        max_write_bytes_space
    } else {
        requested_field_size
    }
}

/// Bytes the `CaptureRecord` envelope adds around a `TrafficStream` of `traffic_stream_len` bytes.
///
/// `CaptureRecord` field 1 is length-delimited with a one-byte tag, so the envelope costs
/// `1 + varint_len(len)`: 2 bytes below 128, 3 below 16 KiB, 4 below 2 MiB, 5 below 256 MiB
/// (wire spec §2).
#[must_use]
pub const fn capture_record_envelope_overhead(traffic_stream_len: u32) -> u32 {
    tag_size(field::CAPTURE_RECORD_TRAFFIC_STREAM) + uint32_size_no_tag(traffic_stream_len)
}

/// The `TrafficStream` byte budget that keeps the enveloped `CaptureRecord` within `payload_limit`.
///
/// `DEVIATION: J-1`. Java never reserves the `CaptureRecord` envelope: the per-record protobuf
/// budget (`KafkaCaptureFactory`:265,1326) and the publisher's payload ceiling
/// (`CaptureKafkaPublisher`:329) are the same number, yet the publisher compares the **enveloped**
/// length against it and one byte over is a terminal `PAYLOAD_LIMIT` capture failure. Java survives
/// only because it accidentally double-reserves the closing `index` field. This function makes the
/// reservation explicit, as the wire spec's J-1 recommendation requires.
///
/// The reservation is computed from `payload_limit` rather than from the actual stream length, which
/// is the only safe direction: `varint_len` is monotonic, so `1 + varint_len(payload_limit)` is an
/// upper bound on the envelope of any stream that fits the budget.
///
/// Returns `None` if `payload_limit` is too small to hold the envelope and at least one byte.
#[must_use]
pub const fn traffic_stream_budget_for_payload_limit(payload_limit: u32) -> Option<u32> {
    let overhead = capture_record_envelope_overhead(payload_limit);
    if payload_limit <= overhead {
        None
    } else {
        Some(payload_limit - overhead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_len_matches_prost() {
        for value in [
            0u64,
            1,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            268_435_455,
            268_435_456,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            assert_eq!(
                varint_len(value) as usize,
                prost::encoding::encoded_len_varint(value),
                "varint_len disagreed with prost for {value}"
            );
        }
    }

    #[test]
    fn tag_sizes_of_the_capture_schema() {
        for one_byte in [1u32, 2, 4, 5, 6, 8, 9, 11, 12, 14, 15] {
            assert_eq!(
                tag_size(one_byte),
                1,
                "field {one_byte} should have a one-byte tag"
            );
        }
        // Wire spec §1.2: fields 16-19 have two-byte tags.
        for two_byte in [16u32, 17, 18, 19] {
            assert_eq!(
                tag_size(two_byte),
                2,
                "field {two_byte} must have a two-byte tag"
            );
        }
    }

    #[test]
    fn negative_int32_is_always_ten_bytes() {
        assert_eq!(int32_size_no_tag(-1), 10);
        assert_eq!(int32_size_no_tag(i32::MIN), 10);
        assert_eq!(int64_size_no_tag(-1), 10);
        // The -1 sentinel pair of EndOfMessageIndication: 23 bytes of body plus its own length.
        let pair = int32_size(field::FIRST_LINE_BYTE_LENGTH, -1)
            + int32_size(field::HEADERS_BYTE_LENGTH, -1);
        assert_eq!(pair, 22);
        assert_eq!(pair + uint32_size_no_tag(pair), 23);
    }

    #[test]
    fn end_of_message_with_known_lengths_is_five_bytes() {
        // Wire spec §5.7: firstLine = 17, headers = 72 gives a 4-byte pair and a 5-byte body.
        let pair = int32_size(field::FIRST_LINE_BYTE_LENGTH, 17)
            + int32_size(field::HEADERS_BYTE_LENGTH, 72);
        assert_eq!(pair, 4);
        assert_eq!(pair + uint32_size_no_tag(pair), 5);
    }

    #[test]
    fn envelope_overhead_transitions() {
        assert_eq!(capture_record_envelope_overhead(127), 2);
        assert_eq!(capture_record_envelope_overhead(128), 3);
        assert_eq!(capture_record_envelope_overhead(16_383), 3);
        assert_eq!(capture_record_envelope_overhead(16_384), 4);
        assert_eq!(capture_record_envelope_overhead(2_097_151), 4);
        assert_eq!(capture_record_envelope_overhead(2_097_152), 5);
    }

    #[test]
    fn traffic_stream_budget_reserves_the_envelope() {
        // Default production ceiling: 1 MiB minus the 500-byte Kafka reservation.
        let limit = 1_048_576 - 500;
        let budget = traffic_stream_budget_for_payload_limit(limit).expect("budget exists");
        assert_eq!(budget, limit - 4);
        // Reserving from the limit is an upper bound on the envelope of anything that fits.
        assert!(budget + capture_record_envelope_overhead(budget) <= limit);
        assert_eq!(traffic_stream_budget_for_payload_limit(2), None);
        assert_eq!(traffic_stream_budget_for_payload_limit(0), None);
    }

    #[test]
    fn canonical_timestamp_drops_a_zero_seconds_field() {
        assert_eq!(size_of_timestamp(CaptureTimestamp::EPOCH), 2);
        assert_eq!(canonical_size_of_timestamp(CaptureTimestamp::EPOCH), 0);
        let t = CaptureTimestamp::new(1_704_067_200, 0);
        assert_eq!(size_of_timestamp(t), 6);
        assert_eq!(canonical_size_of_timestamp(t), 6);
    }

    #[test]
    fn system_time_round_trips_through_capture_timestamp() {
        let t = CaptureTimestamp::from_system_time(SystemTime::UNIX_EPOCH);
        assert_eq!(t, CaptureTimestamp::EPOCH);
        let later = SystemTime::UNIX_EPOCH + std::time::Duration::new(1_704_067_200, 123_456_789);
        assert_eq!(
            CaptureTimestamp::from_system_time(later),
            CaptureTimestamp::new(1_704_067_200, 123_456_789)
        );
        let before = SystemTime::UNIX_EPOCH - std::time::Duration::new(1, 500_000_000);
        // Timestamp requires 0 <= nanos < 1e9, so a pre-epoch instant borrows a second.
        assert_eq!(
            CaptureTimestamp::from_system_time(before),
            CaptureTimestamp::new(-2, 500_000_000)
        );
    }
}
