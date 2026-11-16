//! Segmentation of oversized byte observations (traceability row WIRE-08).
//!
//! Only the three byte-carrying observation kinds are segmentable. The whole/segment pairing is a
//! **total** match over [`ByteObservationKind`] with no catch-all, and each branch is sized with its
//! own field numbers. Both of those close Java defect J-4: `addDataMessage` sizes the whole-value
//! branch with the *segment* field numbers, and its unguarded `else` maps any future byte-carrying
//! kind to `writeSegment`/field 2.
//!
//! # Deviation from Java's packing boundaries
//!
//! `DEVIATION: J-1`. Java derives segment sizes from
//! `CodedOutputStreamSizeUtil.maxBytesNeededForASegmentedObservation`, which reserves the closing
//! `index` field a second time at a fixed six bytes, and from
//! `computeMaxLengthDelimitedFieldSizeForSpace`, which is deliberately one byte pessimistic at every
//! varint boundary. Those two pessimisms are the only reason Java's unreserved `CaptureRecord`
//! envelope does not already overflow the publisher's payload ceiling (wire spec §9B J-1).
//!
//! This crate reserves the envelope explicitly
//! ([`traffic_stream_budget_for_payload_limit`](crate::protobuf_size::traffic_stream_budget_for_payload_limit))
//! and therefore does not need — and must not keep — the accidental slack. Segments are placed with
//! [`max_segment_data_len`], an **exact** solver, so the resulting records differ from Java's
//! byte-for-byte packing boundaries. The rewrite plan does not require exact Java packing boundaries
//! absent a consumer dependency, and the wire spec confirms the replayer ignores record indices
//! entirely (§1.8, discrepancy D-3) and reassembles segments positionally. What the replayer does
//! require is preserved exactly: segments in order, terminated by `segmentEnd`, with a contiguous
//! `connectionObservationSequence`.

use crate::protobuf_size::{CaptureTimestamp, field};
#[cfg(doc)]
use crate::record_encoder::ObservationPlan;

/// The three byte-carrying observation kinds, each with its whole and segmented form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteObservationKind {
    /// Inbound client bytes: `read`(4) / `readSegment`(5).
    Read,
    /// Outbound bytes toward the client: `write`(8) / `writeSegment`(9).
    Write,
    /// A complete non-switching 1xx informational response: `interimResponse`(17) /
    /// `interimResponseSegment`(19).
    InterimResponse,
}

impl ByteObservationKind {
    /// Field number of the whole-value observation.
    #[must_use]
    pub const fn whole_field(self) -> u32 {
        match self {
            Self::Read => field::READ,
            Self::Write => field::WRITE,
            Self::InterimResponse => field::INTERIM_RESPONSE,
        }
    }

    /// Field number of the segmented observation.
    #[must_use]
    pub const fn segment_field(self) -> u32 {
        match self {
            Self::Read => field::READ_SEGMENT,
            Self::Write => field::WRITE_SEGMENT,
            Self::InterimResponse => field::INTERIM_RESPONSE_SEGMENT,
        }
    }

    /// `data` field number of the whole-value body. Always 1.
    #[must_use]
    pub const fn whole_data_field(self) -> u32 {
        field::WHOLE_DATA
    }

    /// `data` field number of the segmented body. Always **2**, never 1: using field 1 makes the
    /// replayer see empty segments and silently loses the bytes (wire spec §10.3).
    #[must_use]
    pub const fn segment_data_field(self) -> u32 {
        field::SEGMENT_DATA
    }

    /// Whether this kind leaves the connection with an unterminated request read, i.e. whether it
    /// sets `TrafficStream.lastObservationWasUnterminatedRead` on the successor record.
    ///
    /// Only `read` and `readSegment` do. Setting it for `write` or `interimResponse` makes the
    /// replayer discard a complete request prefix it should keep (wire spec §10.3).
    #[must_use]
    pub const fn leaves_read_awaiting_end_of_message(self) -> bool {
        matches!(self, Self::Read)
    }
}

/// Bytes one segment observation of `data_len` payload bytes costs inside a record.
///
/// This is exactly what [`ObservationPlan::record_cost`] would report for the same observation; it
/// is expressed in terms of the data *length* so the solver can evaluate it without a slice.
#[must_use]
pub fn segment_record_cost(
    data_len: u32,
    sequence: u64,
    timestamp: CaptureTimestamp,
    segment_field: u32,
    segment_data_field: u32,
) -> u32 {
    use crate::protobuf_size::{
        bytes_needed_for_observation, canonical_size_of_timestamp, length_delimited_size,
        uint64_size,
    };
    // A zero-length slice omits the `data` field entirely, leaving an empty submessage.
    let inner = if data_len == 0 {
        0
    } else {
        length_delimited_size(segment_data_field, data_len)
    };
    let ts_part = length_delimited_size(field::TS, canonical_size_of_timestamp(timestamp));
    let sequence_part = uint64_size(field::CONNECTION_OBSERVATION_SEQUENCE, sequence);
    let capture_part = length_delimited_size(segment_field, inner);
    bytes_needed_for_observation(ts_part + sequence_part + capture_part)
}

/// Largest number of data bytes that fit one segment observation into `available` record bytes.
///
/// Returns 0 when not even a single data byte fits, which the caller turns into either a record
/// boundary or [`WireError::NoForwardProgress`](crate::error::WireError::NoForwardProgress).
///
/// Unlike Java's `computeMaxLengthDelimitedFieldSizeForSpace`, this is exact: the cost is
/// monotonically non-decreasing in the data length, so the largest admissible length is found by
/// bisection and never leaves a usable byte behind at a varint boundary.
#[must_use]
pub fn max_segment_data_len(
    available: u32,
    remaining: u32,
    sequence: u64,
    timestamp: CaptureTimestamp,
    segment_field: u32,
    segment_data_field: u32,
) -> u32 {
    let cost =
        |n: u32| segment_record_cost(n, sequence, timestamp, segment_field, segment_data_field);
    if remaining == 0 || cost(1) > available {
        return 0;
    }
    if cost(remaining) <= available {
        return remaining;
    }
    // Invariant: cost(lo) <= available < cost(hi).
    let mut lo = 1u32;
    let mut hi = remaining;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if cost(mid) <= available {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_and_segment_pairings_are_total_and_distinct() {
        for kind in [
            ByteObservationKind::Read,
            ByteObservationKind::Write,
            ByteObservationKind::InterimResponse,
        ] {
            assert_ne!(kind.whole_field(), kind.segment_field());
            assert_eq!(kind.whole_data_field(), 1);
            assert_eq!(kind.segment_data_field(), 2);
        }
        assert_eq!(ByteObservationKind::Read.whole_field(), 4);
        assert_eq!(ByteObservationKind::Read.segment_field(), 5);
        assert_eq!(ByteObservationKind::Write.whole_field(), 8);
        assert_eq!(ByteObservationKind::Write.segment_field(), 9);
        assert_eq!(ByteObservationKind::InterimResponse.whole_field(), 17);
        assert_eq!(ByteObservationKind::InterimResponse.segment_field(), 19);
        assert!(ByteObservationKind::Read.leaves_read_awaiting_end_of_message());
        assert!(!ByteObservationKind::Write.leaves_read_awaiting_end_of_message());
        assert!(!ByteObservationKind::InterimResponse.leaves_read_awaiting_end_of_message());
    }

    #[test]
    fn the_segment_solver_is_exact_at_every_varint_boundary() {
        let ts = CaptureTimestamp::new(1_704_067_200, 0);
        for available in 1u32..600 {
            let n = max_segment_data_len(
                available,
                100_000,
                1,
                ts,
                field::WRITE_SEGMENT,
                field::SEGMENT_DATA,
            );
            if n > 0 {
                assert!(
                    segment_record_cost(n, 1, ts, field::WRITE_SEGMENT, field::SEGMENT_DATA)
                        <= available,
                    "chose {n} bytes which does not fit {available}"
                );
            }
            assert!(
                segment_record_cost(n + 1, 1, ts, field::WRITE_SEGMENT, field::SEGMENT_DATA)
                    > available,
                "one more byte than {n} should not have fit {available}"
            );
        }
    }

    #[test]
    fn the_segment_solver_is_clamped_by_the_remaining_bytes() {
        let ts = CaptureTimestamp::EPOCH;
        assert_eq!(
            max_segment_data_len(100_000, 7, 1, ts, field::READ_SEGMENT, field::SEGMENT_DATA),
            7
        );
        assert_eq!(
            max_segment_data_len(100_000, 0, 1, ts, field::READ_SEGMENT, field::SEGMENT_DATA),
            0
        );
    }
}
