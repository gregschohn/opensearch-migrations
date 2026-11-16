//! Capture-proxy wire layer: protobuf envelope encoding, observation sequencing, record
//! segmentation, and per-connection publication chaining.
//!
//! # Contract
//!
//! This crate owns the WIRE side of the externally frozen capture contract. The unchanged Java
//! replayer consumes everything this crate produces, so the following are not negotiable:
//!
//! * the generated types come from the shared `TrafficCaptureStream.proto` (see `build.rs`);
//! * `CaptureRecord.payload` is the only record-type discriminator (no Kafka headers);
//! * no payload carries a Kafka partition; the partition is Kafka record metadata only;
//! * captured request, response, and informational-response bytes are byte-exact;
//! * `connectionObservationSequence` is contiguous and connection-local;
//! * `CloseObservation` is the final observation of the final record for a connection.
//!
//! See `docs/captureAndReplay/proxyCaptureProtocol.md` §4 and §5.
//!
//! # Layout
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`protobuf_size`] | WIRE-02 | tag/varint/length-delimited/nested size accounting at every boundary |
//! | [`record_buffer`] | WIRE-01 | RAII-owned record buffers; release exactly once on every path |
//! | [`record_encoder`] | WIRE-08 | single-pass canonical `CaptureRecord` encoding |
//! | [`segmentation`] | WIRE-08 | whole-versus-segmented decision and exact segment placement |
//! | [`connection_recorder`] | WIRE-03, WIRE-08 | the observation catalogue, its ordering, counters, and the `F` deadline |
//! | [`publish_chain`] | WIRE-07 | per-connection acknowledgement ordering and failure poisoning |
//! | [`record_sink`] | WIRE-09 | the Kafka-free sink boundary |
//!
//! # Deliberate deviations from the Java implementation
//!
//! Every one is marked with a `DEVIATION:` comment at its site. In summary:
//!
//! * **J-1** — the `CaptureRecord` envelope is budgeted explicitly
//!   ([`protobuf_size::traffic_stream_budget_for_payload_limit`]) and Java's accidental
//!   double-reservation of the closing `index` field is dropped, so record-packing boundaries differ
//!   from Java's. Segments are placed with an exact solver rather than Java's one-byte-pessimistic
//!   helper (see [`segmentation`]).
//! * **`F = 0`** — rejected, per `proxyCaptureProtocol.md` §4.1, where Java silently accepts it and
//!   disables deadlines (see [`connection_recorder::ConnectionRecorder::with_clock`]).
//! * **J-5** — an oversized `connectionException` message is truncated rather than failing capture
//!   (see [`record_encoder::MAX_EXCEPTION_MESSAGE_BYTES`]).
//! * **J-2 / J-3** — a close and its terminal publication are one operation, and an explicit
//!   `abort` releases an unpublished record. Both remove Java failure modes rather than adding
//!   behaviour.

/// Types generated from the canonical, unchanged capture schema.
///
/// The `.proto` declares no `package`, so prost emits into the crate root of the generated file.
pub mod proto {
    #![allow(clippy::pedantic, clippy::all, missing_docs)]
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

pub mod connection_recorder;
pub mod error;
pub mod protobuf_size;
pub mod publish_chain;
pub mod record_buffer;
pub mod record_encoder;
pub mod record_sink;
pub mod segmentation;

pub use connection_recorder::{
    ConnectionRecorder, ConnectionRecorderConfig, DEFAULT_FLUSH_INTERVAL, FlushDeadline,
    ManualClock, MonotonicClock, ScheduledFlush, SystemMonotonicClock, UNKNOWN_MESSAGE_LENGTH,
};
pub use error::WireError;
pub use protobuf_size::CaptureTimestamp;
pub use publish_chain::{PublishChain, PublishFailure, PublishOutcome, PublishTicket};
pub use record_buffer::{
    BufferRecycler, CountingRecycler, ENVELOPE_PREFIX_RESERVE, EncodedRecord, HeapRecycler,
    RecordBuf,
};
pub use record_encoder::{ObservationBody, ObservationPlan, RecordEncoder, RecordHeader};
pub use record_sink::{CollectingSink, RecordSink, SinkError};
pub use segmentation::ByteObservationKind;
