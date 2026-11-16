//! Error types for the capture wire layer.

use std::time::Duration;

use crate::publish_chain::PublishFailure;

/// Everything that can go wrong while recording or encoding a connection's capture stream.
///
/// Every variant is an internal-invariant or configuration failure. None of them is a recoverable
/// data condition: `docs/captureAndReplay/proxyCaptureProtocol.md` §7 requires ownership or
/// invariant corruption to enter the capture-failure workflow rather than be papered over.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WireError {
    /// `TrafficStream.connectionId` was empty.
    ///
    /// The replayer keys connections on `(nodeId, connectionId)` and an empty `connectionId` raises
    /// an `IllegalArgumentException` that escapes its violation path and kills the replayer process
    /// (wire spec §1.8).
    #[error("connectionId must be non-empty: the replayer keys connections on it")]
    EmptyConnectionId,

    /// `TrafficStream.nodeId` was empty.
    ///
    /// It is both half of the connection key and the writer-time identity that must match
    /// `WriterPartitionHeartbeat.writerNodeId` byte for byte (wire spec §1.8).
    #[error(
        "nodeId must be non-empty: it is the writer identity the replayer matches heartbeats on"
    )]
    EmptyNodeId,

    /// The configured flush interval `F` was not positive.
    ///
    /// `DEVIATION`: `proxyCaptureProtocol.md` §4.1 requires `F` to be positive and states that zero
    /// and negative values are rejected; Java rejects only negatives and treats zero as "no
    /// deadline". This crate implements the documented behaviour.
    #[error(
        "trafficStreamFlushInterval must be positive (proxyCaptureProtocol.md §4.1), got {0:?}"
    )]
    NonPositiveFlushInterval(Duration),

    /// The Kafka payload ceiling cannot hold a usable record.
    #[error(
        "payload limit of {limit} bytes is too small: at least {minimum} bytes are needed for the \
         CaptureRecord envelope, the record header and trailer, and one minimal observation"
    )]
    PayloadLimitTooSmall {
        /// The rejected limit.
        limit: u32,
        /// The smallest limit that would have been accepted.
        minimum: u32,
    },

    /// An observation was offered after the connection's terminal `CloseObservation`.
    ///
    /// `proxyCaptureProtocol.md` §4.1: once `CloseObservation` is consumed, any later
    /// `TrafficObservation` for that connection is a protocol violation.
    #[error("cannot append an observation after this connection's terminal CloseObservation")]
    AfterTerminalClose,

    /// `connectionObservationSequence` would have exceeded its range.
    #[error("connectionObservationSequence overflowed")]
    SequenceOverflow,

    /// The 1-based record index would have exceeded `int32`.
    #[error("TrafficStream record index overflowed int32")]
    RecordIndexOverflow,

    /// A single observation cannot fit in an empty record.
    #[error(
        "an observation needing {needed} bytes cannot fit in an empty record with a {budget}-byte \
         TrafficStream budget"
    )]
    ObservationTooLarge {
        /// Bytes the observation needs inside the record.
        needed: u32,
        /// The record's whole `TrafficStream` budget.
        budget: u32,
    },

    /// Segmentation could not place even one data byte in an empty record.
    ///
    /// The counterpart of Java's `IllegalStateException("Stream space is not allowing forward
    /// progress on byteBuf reading")`.
    #[error("record space is not allowing forward progress on segment emission")]
    NoForwardProgress,

    /// The terminal-record biconditional was violated.
    ///
    /// `numberOfThisLastChunk` is set **iff** the record holds exactly one `close` and that `close`
    /// is the final observation (`KafkaCaptureFactory.isTerminalRecord`, wire spec §8). All four
    /// error shapes are rejected here.
    #[error(
        "a connection's final record must contain exactly one terminal CloseObservation as its \
         last observation (final={is_final}, closes={close_count}, close_is_last={close_is_last})"
    )]
    TerminalRecordInvariant {
        /// Whether the record was being closed with `numberOfThisLastChunk`.
        is_final: bool,
        /// How many `close` observations the record holds.
        close_count: u32,
        /// Whether the final observation of the record is a `close`.
        close_is_last: bool,
    },

    /// A record with no observations was about to be published.
    ///
    /// `proxyCaptureProtocol.md` §4.2: an idle connection emits no empty `TrafficStream`. This also
    /// closes Java defect J-2, where a terminal flush with no open record fabricates an
    /// observation-less final record that then fails the terminal-record invariant and declares the
    /// process unstable.
    #[error("refusing to publish a record with no observations")]
    EmptyRecord,

    /// The encoded record exceeded its own budget. An internal accounting bug if it ever fires.
    #[error("record body of {written} bytes exceeded its {budget}-byte TrafficStream budget")]
    RecordOverflow {
        /// Bytes actually written.
        written: u32,
        /// The budget that was exceeded.
        budget: u32,
    },

    /// The connection's publication chain is poisoned by an earlier failure.
    #[error("this connection's publication chain already failed: {0}")]
    PoisonedPublishChain(#[source] PublishFailure),
}
