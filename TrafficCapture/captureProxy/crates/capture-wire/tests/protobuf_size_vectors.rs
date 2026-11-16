//! Ported Java size-arithmetic vectors (traceability row WIRE-02, treatment *Exact*).
//!
//! Sources:
//!
//! * `TrafficCapture/captureOffloader/src/test/java/.../CodedOutputStreamSizeUtilTest.java`
//!   — all eleven vectors, labelled T1-T11 as in the wire spec §11.
//! * `TrafficCapture/captureOffloader/src/test/java/.../StreamChannelConnectionCaptureSerializerTest.java`
//!   `test_computeMaxLengthDelimitedFieldSizeForSpace` — the whole `@CsvSource` and all of its
//!   invariants, including the reference "optimal" oracle.
//!
//! No assertion here is weakened relative to the Java original. Where the Java assertion itself is
//! odd (invariant 4 below), the oddity is reproduced verbatim and called out.

use capture_wire::protobuf_size::{
    CaptureTimestamp, bytes_needed_for_observation_and_closing_index, bytes_remaining_size,
    compute_max_length_delimited_field_size_for_space, int32_size_no_tag,
    max_bytes_needed_for_a_segmented_observation,
    optimal_max_length_delimited_field_size_for_space, size_of_timestamp, uint32_size_no_tag,
    varint_len,
};

/// `Instant.parse("2024-01-01T00:00:00Z")`.
const TS_2024_01_01: CaptureTimestamp = CaptureTimestamp::new(1_704_067_200, 0);
/// `Instant.parse("2024-12-31T23:59:59.123456789Z")`.
const TS_2024_12_31: CaptureTimestamp = CaptureTimestamp::new(1_735_689_599, 123_456_789);

// ---------------------------------------------------------------------- CodedOutputStreamSizeUtil

/// T1 — `testGetSizeOfTimestamp`.
#[test]
fn t1_size_of_timestamp_seconds_only() {
    assert_eq!(size_of_timestamp(TS_2024_01_01), 6);
}

/// T2 — `testGetSizeOfTimestamp`, seconds and nanos.
#[test]
fn t2_size_of_timestamp_with_nanos() {
    assert_eq!(size_of_timestamp(TS_2024_12_31), 11);
}

/// T3 — `testMaxBytesNeededForASegmentedObservation`.
#[test]
fn t3_max_bytes_needed_for_a_segmented_observation() {
    // observationFieldNumber = 1, dataFieldNumber = 2, buf = "Test" (4 readable bytes), seq = 1.
    assert_eq!(
        max_bytes_needed_for_a_segmented_observation(TS_2024_01_01, 1, 2, 4, 1),
        27
    );
}

/// T4 — `test_computeByteBufRemainingSize_emptyBufWithCapacity`.
///
/// A 100-byte direct buffer with nothing written. Pins that *readable* bytes drive the size, not
/// capacity: the answer is `tag(2) + varint(0)`, not anything derived from 100.
#[test]
fn t4_byte_buf_remaining_size_empty_with_capacity() {
    let buf: Vec<u8> = Vec::with_capacity(100);
    assert_eq!(buf.capacity(), 100);
    assert_eq!(
        bytes_remaining_size(2, u32::try_from(buf.len()).unwrap()),
        2
    );
}

/// T5 — `test_computeByteBufRemainingSize`, 10 readable bytes.
#[test]
fn t5_byte_buf_remaining_size() {
    let buf = b"hello_test";
    assert_eq!(
        bytes_remaining_size(2, u32::try_from(buf.len()).unwrap()),
        12
    );
}

/// T6 — `test_computeByteBufRemainingSize_largeBuf`: pins the 3-byte length varint transition.
#[test]
fn t6_byte_buf_remaining_size_large_buf() {
    let buf = vec![0u8; 1_000_000];
    assert_eq!(
        bytes_remaining_size(2, u32::try_from(buf.len()).unwrap()),
        1_000_004
    );
}

/// T7 — `test_computeByteBufRemainingSize_ByteBufAtCapacity`: a 4-byte buffer holding "Test".
#[test]
fn t7_byte_buf_remaining_size_at_capacity() {
    let mut buf: Vec<u8> = Vec::with_capacity(4);
    buf.extend_from_slice(b"Test");
    assert_eq!(buf.len(), buf.capacity());
    assert_eq!(
        bytes_remaining_size(2, u32::try_from(buf.len()).unwrap()),
        6
    );
}

/// T8 — `test_computeByteBufRemainingSize_EmptyByteBuf`: zero capacity, zero readable.
#[test]
fn t8_byte_buf_remaining_size_empty() {
    let buf: Vec<u8> = Vec::new();
    assert_eq!(
        bytes_remaining_size(2, u32::try_from(buf.len()).unwrap()),
        2
    );
}

/// T9 — `testBytesNeededForObservationAndClosingIndex`.
#[test]
fn t9_bytes_needed_for_observation_and_closing_index() {
    assert_eq!(bytes_needed_for_observation_and_closing_index(50, 10), 54);
}

/// T10 — `testBytesNeededForObservationAndClosingIndex_WithZeroContent`.
#[test]
fn t10_bytes_needed_with_zero_content() {
    assert_eq!(bytes_needed_for_observation_and_closing_index(0, 0), 4);
}

/// T11 — `testBytesNeededForObservationAndClosingIndex_VariousIndices`.
#[test]
fn t11_bytes_needed_various_indices() {
    let indices = [1, 1000, 100_000];
    let expected = [24, 25, 26];
    for (index, want) in indices.into_iter().zip(expected) {
        assert_eq!(
            bytes_needed_for_observation_and_closing_index(20, index),
            want,
            "index {index} scaled the closing-index varint wrongly"
        );
    }
}

// ------------------------------------------------- computeMaxLengthDelimitedFieldSizeForSpace

/// Every `(totalAvailableSpace, requestedFieldSize)` pair of the Java `@CsvSource`.
const CSV_SOURCE: &[(i32, i32)] = &[
    // totalAvailableSpace >= requestedWriteableSpace
    (2, 2),
    (3, 2),
    (4, 2),
    // around a length-varint increase, with totalAvailableSpace < requestedWriteableSpace
    (1, 10),
    (2, 10),
    (3, 10),
    (127, 10_000_000),
    (128, 10_000_000), // 2^7
    (129, 10_000_000),
    (130, 10_000_000),
    (131, 10_000_000),
    (132, 10_000_000),
    (16_381, 10_000_000),
    (16_382, 10_000_000),
    (16_383, 10_000_000),
    (16_384, 10_000_000), // 2^14
    (16_385, 10_000_000),
    (16_386, 10_000_000),
    (16_387, 10_000_000),
    (16_388, 10_000_000),
    (16_389, 10_000_000),
    (2_097_150, 10_000_000),
    (2_097_151, 10_000_000),
    (2_097_152, 10_000_000), // 2^21
    (2_097_153, 10_000_000),
    (2_097_154, 10_000_000),
    (2_097_155, 10_000_000),
    (2_097_156, 10_000_000),
    (2_097_157, 10_000_000),
    (2_097_158, 10_000_000),
    (268_435_455, 100_000_000),
    (268_435_456, 100_000_000), // 2^28
    (268_435_457, 100_000_000),
    (268_435_458, 100_000_000),
    (268_435_459, 100_000_000),
    (268_435_460, 100_000_000),
    (268_435_461, 100_000_000),
    (268_435_462, 100_000_000),
];

#[test]
fn csv_source_covers_every_java_row() {
    assert_eq!(CSV_SOURCE.len(), 38, "the Java @CsvSource has 38 rows");
}

/// The four invariants `StreamChannelConnectionCaptureSerializerTest`:911-957 asserts for every
/// row, plus the "implementation is optimal or optimal - 1" check.
#[test]
fn compute_max_length_delimited_field_size_for_space_matches_every_java_row() {
    for &(total, requested) in CSV_SOURCE {
        let optimal = optimal_max_length_delimited_field_size_for_space(total, requested);

        // (1) cannot write more bytes than requested
        assert!(
            optimal <= requested,
            "({total},{requested}): cannot write more than requested"
        );

        // (2) expected non-negative space left
        let space_left_after_write = total - int32_size_no_tag(optimal).cast_signed() - optimal;
        assert!(
            space_left_after_write >= 0,
            "({total},{requested}): expected non-negative space left, got {space_left_after_write}"
        );

        if optimal < requested {
            // (3) at most one byte may be left over when the request could not be satisfied
            assert!(
                space_left_after_write <= 1,
                "({total},{requested}): expected at most 1 byte left, got {space_left_after_write}"
            );

            // (4) no space to write one more byte.
            //
            // Reproduced verbatim from the Java assertion, which subtracts the length-varint width
            // of BOTH `optimal` and `optimal + 1`. That double subtraction is almost certainly a
            // slip in the Java test rather than the intended arithmetic, but it is the assertion
            // that exists, it passes, and weakening it here would hide a real divergence.
            let if_one_more_byte = total
                - int32_size_no_tag(optimal).cast_signed()
                - int32_size_no_tag(optimal + 1).cast_signed()
                - (optimal + 1);
            assert!(
                if_one_more_byte < 0,
                "({total},{requested}): expected no space to write one more byte, got {if_one_more_byte}"
            );
        }

        // (5) the implementation is optimal, or exactly one byte pessimistic.
        let actual = compute_max_length_delimited_field_size_for_space(total, requested);
        if actual != optimal {
            assert!(
                actual > 0,
                "({total},{requested}): pessimistic result must stay positive"
            );
            assert_eq!(
                actual,
                optimal - 1,
                "({total},{requested}): pessimism must be exactly one byte"
            );
            if actual < requested {
                let space_left = total - int32_size_no_tag(actual).cast_signed() - actual;
                assert!(
                    space_left <= 1,
                    "({total},{requested}): pessimistic leftover {space_left} exceeded 1"
                );
            }
        }
    }
}

/// Pins exactly which rows are one byte pessimistic.
///
/// A varint-width transition at `2^(7k)` makes the implementation pessimistic for the `k` rows
/// starting at the boundary, because the pessimistic estimate sizes the length prefix from `total`
/// while the optimal one sizes it from `total - width(total) + 1`, and that lower value keeps the
/// narrower width for `k` more steps.
///
/// **Spec discrepancy.** Wire spec §5.6's table lists `optimal` equal to `pessimistic` for
/// `total = 16385` (it says 16382) and for `total = 2097153` (it says 2097149). Both rows are in
/// fact one byte pessimistic: the Java reference oracle
/// `optimalComputeMaxLengthDelimitedFieldSizeForSpace` returns 16383 and 2097150 respectively, which
/// is what this crate's port returns. Verified against
/// `StreamChannelConnectionCaptureSerializerTest`:963-980 directly.
#[test]
fn pessimism_appears_only_at_the_varint_transitions() {
    let mut pessimistic_rows = Vec::new();
    for &(total, requested) in CSV_SOURCE {
        let actual = compute_max_length_delimited_field_size_for_space(total, requested);
        let optimal = optimal_max_length_delimited_field_size_for_space(total, requested);
        if actual != optimal {
            pessimistic_rows.push(total);
        }
    }
    assert_eq!(
        pessimistic_rows,
        vec![128, 16_384, 16_385, 2_097_152, 2_097_153, 2_097_154],
        "one pessimistic row at 2^7, two at 2^14, three at 2^21"
    );
}

/// The `(pessimistic, optimal)` pairs tabulated in wire spec §5.6.
///
/// Two rows below differ from that table because the table is wrong; see
/// [`pessimism_appears_only_at_the_varint_transitions`] for the derivation. The values here are
/// what the Java implementation and its own reference oracle produce.
#[test]
fn tabulated_boundary_values() {
    let table: &[(i32, i32, i32, i32)] = &[
        // total, requested, pessimistic, optimal
        (1, 10, 0, 0),
        (2, 2, 1, 1),
        (2, 10, 1, 1),
        (3, 10, 2, 2),
        (127, 10_000_000, 126, 126),
        (128, 10_000_000, 126, 127),
        (129, 10_000_000, 127, 127),
        (130, 10_000_000, 128, 128),
        (16_383, 10_000_000, 16_381, 16_381),
        (16_384, 10_000_000, 16_381, 16_382),
        // Wire spec §5.6 claims optimal = 16382 here; Java returns 16383.
        (16_385, 10_000_000, 16_382, 16_383),
        (16_386, 10_000_000, 16_383, 16_383),
        (2_097_151, 10_000_000, 2_097_148, 2_097_148),
        (2_097_152, 10_000_000, 2_097_148, 2_097_149),
        // Wire spec §5.6 claims optimal = 2097149 here; Java returns 2097150.
        (2_097_153, 10_000_000, 2_097_149, 2_097_150),
        (2_097_154, 10_000_000, 2_097_150, 2_097_151),
        (2_097_155, 10_000_000, 2_097_151, 2_097_151),
        (268_435_455, 100_000_000, 100_000_000, 100_000_000),
        (268_435_456, 100_000_000, 100_000_000, 100_000_000),
    ];
    for &(total, requested, pessimistic, optimal) in table {
        assert_eq!(
            compute_max_length_delimited_field_size_for_space(total, requested),
            pessimistic,
            "pessimistic value for ({total},{requested})"
        );
        assert_eq!(
            optimal_max_length_delimited_field_size_for_space(total, requested),
            optimal,
            "optimal value for ({total},{requested})"
        );
    }
}

/// `total = 1` is the row that makes Java throw its no-forward-progress `IllegalStateException`.
#[test]
fn a_single_available_byte_admits_no_data_byte() {
    assert_eq!(compute_max_length_delimited_field_size_for_space(1, 10), 0);
}

// ---------------------------------------------------------------------------- property tests

mod properties {
    use capture_wire::proto;
    use capture_wire::protobuf_size::{
        canonical_size_of_timestamp, int32_size_no_tag, int64_size_no_tag, string_size,
        uint32_size_no_tag, uint64_size_no_tag, varint_len,
    };
    use proptest::prelude::*;
    use prost::Message;

    proptest! {
        /// Cross-check every varint width against `prost::encoding::encoded_len_varint`.
        #[test]
        fn varint_len_agrees_with_prost(value: u64) {
            prop_assert_eq!(
                varint_len(value) as usize,
                prost::encoding::encoded_len_varint(value)
            );
            prop_assert_eq!(uint64_size_no_tag(value), varint_len(value));
        }

        #[test]
        fn uint32_size_no_tag_agrees_with_prost(value: u32) {
            prop_assert_eq!(
                uint32_size_no_tag(value) as usize,
                prost::encoding::encoded_len_varint(u64::from(value))
            );
        }

        /// A negative `int32`/`int64` is sign-extended to 64 bits, i.e. ten bytes.
        #[test]
        fn signed_sizes_agree_with_prost(value: i32) {
            prop_assert_eq!(
                int32_size_no_tag(value) as usize,
                prost::encoding::encoded_len_varint(i64::from(value).cast_unsigned())
            );
            prop_assert_eq!(
                int64_size_no_tag(i64::from(value)) as usize,
                prost::encoding::encoded_len_varint(i64::from(value).cast_unsigned())
            );
        }

        /// The canonical `Timestamp` body size must equal what prost would encode.
        #[test]
        fn canonical_timestamp_size_agrees_with_prost(
            seconds in -4_000_000_000i64..4_000_000_000,
            nanos in 0i32..1_000_000_000,
        ) {
            let timestamp = prost_types::Timestamp { seconds, nanos };
            prop_assert_eq!(
                canonical_size_of_timestamp(capture_wire::CaptureTimestamp::new(seconds, nanos))
                    as usize,
                timestamp.encoded_len()
            );
        }

        /// `string_size` must equal the encoded length of a one-string message, including the
        /// omission of the empty default.
        #[test]
        fn string_size_agrees_with_prost(text in ".{0,200}") {
            let message = proto::ConnectionExceptionObservation { message: text.clone() };
            prop_assert_eq!(string_size(1, &text) as usize, message.encoded_len());
        }

        /// A length-delimited `bytes` field must agree with prost for every length, including the
        /// varint-width transitions.
        #[test]
        fn bytes_field_size_agrees_with_prost(len in 0usize..300) {
            let data = vec![0xABu8; len];
            // prost omits a zero-length `bytes` field entirely, which is exactly what the
            // encoder does too: an empty read observation is `tag 0x00`, not a zero-length `data`.
            let expected = |n: usize, field: u32| -> usize {
                if n == 0 {
                    0
                } else {
                    capture_wire::protobuf_size::length_delimited_size(
                        field,
                        u32::try_from(n).unwrap(),
                    ) as usize
                }
            };
            let whole = proto::ReadObservation { data: data.clone().into() };
            prop_assert_eq!(whole.encoded_len(), expected(len, 1));
            // Field 2 is the segment data field; its tag is also one byte.
            let segment = proto::ReadSegmentObservation { data: data.into() };
            prop_assert_eq!(segment.encoded_len(), expected(len, 2));
        }
    }
}

/// The varint-width table the whole module depends on.
#[test]
fn varint_width_transitions() {
    assert_eq!(varint_len(127), 1);
    assert_eq!(varint_len(128), 2);
    assert_eq!(varint_len(16_383), 2);
    assert_eq!(varint_len(16_384), 3);
    assert_eq!(varint_len(2_097_151), 3);
    assert_eq!(varint_len(2_097_152), 4);
    assert_eq!(varint_len(268_435_455), 4);
    assert_eq!(varint_len(268_435_456), 5);
    assert_eq!(uint32_size_no_tag(u32::MAX), 5);
    assert_eq!(varint_len(u64::MAX), 10);
}
