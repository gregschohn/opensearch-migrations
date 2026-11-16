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

/// Types generated from the canonical, unchanged capture schema.
///
/// The `.proto` declares no `package`, so prost emits into the crate root of the generated file.
pub mod proto {
    #![allow(clippy::pedantic, clippy::all, missing_docs)]
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}
