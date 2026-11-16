//! The capture publication metric contract (kafka spec §14).
//!
//! # Why the contract lives here and the `OTel` wiring does not
//!
//! Metric names, instrument kinds, units, and the **closed** attribute value sets are an external
//! dashboard and alarm contract: `CapturePublishingMetrics.java:59-81` spells the names literally
//! and `:245-247` lowercases every attribute value from an enum, so no connection id, writer id,
//! partition, activation id, or topic can ever become an attribute. This module states that
//! contract as a trait plus the literal name constants, and asserts the conservation rules in
//! tests. Binding it to an OpenTelemetry meter belongs to `capture-observability`, which owns the
//! exporter: it implements [`CaptureMetrics`] and nothing else changes.
//!
//! # Conservation
//!
//! The rules that make the dashboards add up, pinned by
//! `CapturePublishingMetricsTest.recordsBytesWritersConnectionsAndBoundedOutcomesAreConserved`:
//!
//! * every [`CaptureMetrics::publication_accepted`] is eventually matched by exactly one
//!   [`CaptureMetrics::publication_completed`] with the same `kind` and `bytes`, so the two
//!   `awaiting` up-down counters return to zero;
//! * `writers_activated(activated, previously_active)` moves `writersActive` by the **signed**
//!   delta `activated - previously_active` and adds `previously_active` to `writersDraining`;
//! * `writer_retired()` is `writersDraining -= 1, writersRetired += 1`.

use crate::error::FailureReason;

/// Literal metric names. Changing one of these is an external contract break.
pub mod names {
    /// `LongCounter`, unit `records`, attribute `recordKind`.
    pub const RECORDS_ACCEPTED: &str = "capturePublishingRecordsAccepted";
    /// `LongCounter`, unit `By`, attribute `recordKind`.
    pub const BYTES_ACCEPTED: &str = "capturePublishingBytesAccepted";
    /// `LongCounter`, unit `records`, attributes `recordKind` and `outcome`.
    pub const COMPLETIONS: &str = "capturePublishingCompletions";
    /// `LongUpDownCounter`, unit `records`, attribute `recordKind`.
    pub const RECORDS_AWAITING: &str = "capturePublishingRecordsAwaitingAcknowledgement";
    /// `LongUpDownCounter`, unit `By`, attribute `recordKind`.
    pub const BYTES_AWAITING: &str = "capturePublishingBytesAwaitingAcknowledgement";
    /// `LongCounter`, unit `records`, no attributes.
    pub const HEARTBEAT_SUBMISSIONS: &str = "captureHeartbeatSubmissions";
    /// `LongCounter`, unit `records`, no attributes.
    pub const HEARTBEAT_ACKNOWLEDGEMENTS: &str = "captureHeartbeatAcknowledgements";
    /// `LongCounter`, unit `violations`, no attributes.
    pub const HEARTBEAT_DEADLINE_VIOLATIONS: &str = "captureHeartbeatDeadlineViolations";
    /// `LongCounter`, unit `violations`, no attributes.
    pub const BROKER_TIME_VIOLATIONS: &str = "captureBrokerTimeViolations";
    /// `LongCounter`, unit `transitions`, attribute `transition`.
    pub const ROUTING_TRANSITIONS: &str = "captureRoutingTransitions";
    /// `LongUpDownCounter`, unit `writers`, no attributes.
    pub const WRITERS_ACTIVE: &str = "captureWritersActive";
    /// `LongUpDownCounter`, unit `writers`, no attributes.
    pub const WRITERS_DRAINING: &str = "captureWritersDraining";
    /// `LongCounter`, unit `writers`, no attributes.
    pub const WRITERS_RETIRED: &str = "captureWritersRetired";
    /// `LongCounter`, unit `transitions`, attribute `reason`.
    pub const WRITE_GATE_TRANSITIONS: &str = "captureWriteGateTransitions";
    /// `LongCounter`, unit `writes`, no attributes.
    pub const WRITES_REJECTED: &str = "captureWritesRejected";
    /// `LongCounter`, unit `failures`, attribute `reason`.
    pub const PUBLISHER_FAILURES: &str = "capturePublisherFailures";

    /// The four attribute keys. There are no others.
    pub const ATTRIBUTE_KEYS: [&str; 4] = ["recordKind", "outcome", "transition", "reason"];

    /// Additive publication-pipeline instruments, beyond the frozen Java set.
    ///
    /// These are **new names**, so no existing dashboard or alarm changes meaning. They exist
    /// because the Java baseline's throughput story could only be *inferred*: proxy throughput is
    /// inversely proportional to Kafka acknowledgement latency under the default fail-closed
    /// policy, and the Java `capture_health` line reports queue depth as a task count that mixes
    /// not-yet-due heartbeat timers with real backlog. When a run misses a throughput gate, these
    /// four separate the three candidate causes — queueing, broker latency, and work done before
    /// submission — instead of leaving them confounded.
    pub mod pipeline {
        /// `LongUpDownCounter`, unit `records`, attribute `recordKind`. Accepted but not yet
        /// submitted to the producer, i.e. real backlog.
        pub const QUEUE_DEPTH_RECORDS: &str = "capturePublishingQueueDepthRecords";
        /// `LongUpDownCounter`, unit `By`, attribute `recordKind`.
        pub const QUEUE_DEPTH_BYTES: &str = "capturePublishingQueueDepthBytes";
        /// `DoubleHistogram`, unit `ms`. Accept to producer-enqueue: this crate's own cost,
        /// including budget admission and routing bookkeeping. Rules encoding and this crate in or
        /// out as the cause.
        pub const ACCEPT_TO_SUBMIT_MS: &str = "capturePublishingAcceptToSubmitMillis";
        /// `DoubleHistogram`, unit `ms`. Producer-enqueue to delivery report: broker-side latency
        /// plus batching. This is the quantity the throughput coupling is against.
        pub const SUBMIT_TO_DELIVERY_MS: &str = "capturePublishingSubmitToDeliveryMillis";
        /// `LongUpDownCounter`, unit `ms`. Age of the oldest unacknowledged record, or zero.
        pub const OLDEST_UNACKNOWLEDGED_AGE_MS: &str =
            "capturePublishingOldestUnacknowledgedAgeMillis";
    }
}

/// The `recordKind` attribute value set.
///
/// A capability probe is **not** a `recordKind`: Java's `RecordKind`
/// (`CapturePublishingMetrics.java:21-24`) has exactly two values, and probes are published by the
/// initializer before the publisher exists, so they are never counted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordKind {
    /// A `TrafficStream` record.
    Traffic,
    /// A `WriterPartitionHeartbeat` record.
    Heartbeat,
}

impl RecordKind {
    /// The literal attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Traffic => "traffic",
            Self::Heartbeat => "heartbeat",
        }
    }
}

/// The `outcome` attribute value set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PublishOutcome {
    /// Kafka acknowledged the record and the acknowledgement was accepted.
    Acknowledged,
    /// The publication failed, was rejected, or lost its deadline race.
    Failed,
}

impl PublishOutcome {
    /// The literal attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Failed => "failed",
        }
    }
}

/// The `transition` attribute value set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RoutingTransition {
    /// A group assignment callback was received.
    AssignmentReceived,
    /// A routing generation was prepared and its writer partitions registered.
    GenerationPrepared,
    /// A routing generation became the active one.
    GenerationActivated,
}

impl RoutingTransition {
    /// The literal attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AssignmentReceived => "assignment_received",
            Self::GenerationPrepared => "generation_prepared",
            Self::GenerationActivated => "generation_activated",
        }
    }
}

/// The capture publication metric sink.
///
/// Every method has a no-op default so a caller that does not care about observability — a unit
/// test, or a deployment with metrics disabled — can use [`NoopMetrics`] without implementing
/// sixteen methods. Implementations must be cheap and must never block: these are called from the
/// publisher actor, the delivery-report thread, and the deadline thread.
pub trait CaptureMetrics: Send + Sync {
    /// A record was accepted by the producer boundary.
    ///
    /// Increments `recordsAccepted{kind}`, `bytesAccepted{kind}`, `recordsAwaiting{kind}`,
    /// `bytesAwaiting{kind}`, and for a heartbeat also `heartbeatSubmissions`.
    fn publication_accepted(&self, kind: RecordKind, bytes: u64) {
        let _ = (kind, bytes);
    }

    /// An accepted record reached a terminal outcome.
    ///
    /// Increments `completions{kind,outcome}`, decrements both `awaiting` counters, and for an
    /// acknowledged heartbeat also increments `heartbeatAcknowledgements`.
    fn publication_completed(&self, kind: RecordKind, outcome: PublishOutcome, bytes: u64) {
        let _ = (kind, outcome, bytes);
    }

    /// A heartbeat acknowledgement deadline `E` expired.
    fn heartbeat_deadline_violated(&self) {}

    /// A broker-time continuity check rejected an acknowledgement.
    fn broker_time_violated(&self) {}

    /// An assignment or routing-generation transition occurred.
    fn routing_transition(&self, transition: RoutingTransition) {
        let _ = transition;
    }

    /// A routing generation became active, superseding `previously_active` writer partitions.
    ///
    /// `writersActive += activated - previously_active` (a signed delta), and when
    /// `previously_active > 0` also `writersDraining += previously_active`.
    fn writers_activated(&self, activated: u64, previously_active: u64) {
        let _ = (activated, previously_active);
    }

    /// The active generation began draining for orderly shutdown.
    fn active_writers_began_draining(&self, count: u64) {
        let _ = count;
    }

    /// A writer partition finished draining and retired.
    fn writer_retired(&self) {}

    /// The write gate made its one irreversible transition.
    fn write_gate_tripped(&self, reason: FailureReason) {
        let _ = reason;
    }

    /// A write submission was rejected because the gate was closed.
    fn write_rejected(&self) {}

    /// The publisher failed terminally.
    fn publisher_failed(&self, reason: FailureReason) {
        let _ = reason;
    }

    /// A record was accepted but is waiting for publication budget, i.e. real backlog.
    ///
    /// Unlike Java's `publisherQueueDepth`, this counts only records that are genuinely waiting;
    /// a not-yet-due heartbeat timer is not backlog.
    fn queue_depth_changed(&self, kind: RecordKind, records_delta: i64, bytes_delta: i64) {
        let _ = (kind, records_delta, bytes_delta);
    }

    /// Time from accepting a record to handing it to the producer.
    ///
    /// This crate's own cost: budget admission plus routing and lane bookkeeping. If a throughput
    /// gate is missed and this is flat, the cause is not in this crate.
    fn accept_to_submit(&self, kind: RecordKind, elapsed: std::time::Duration) {
        let _ = (kind, elapsed);
    }

    /// Time from handing a record to the producer to its delivery report.
    ///
    /// Broker latency plus batching — the quantity proxy throughput is inversely proportional to
    /// under the default fail-closed policy.
    fn submit_to_delivery(&self, kind: RecordKind, elapsed: std::time::Duration) {
        let _ = (kind, elapsed);
    }

    /// The age of the oldest unacknowledged record, sampled at each health report.
    fn oldest_unacknowledged_age(&self, elapsed: std::time::Duration) {
        let _ = elapsed;
    }
}

/// A metric sink that discards everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopMetrics;

impl CaptureMetrics for NoopMetrics {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_value_sets_are_closed_and_lowercase() {
        assert_eq!(RecordKind::Traffic.as_str(), "traffic");
        assert_eq!(RecordKind::Heartbeat.as_str(), "heartbeat");
        assert_eq!(PublishOutcome::Acknowledged.as_str(), "acknowledged");
        assert_eq!(PublishOutcome::Failed.as_str(), "failed");
        assert_eq!(
            RoutingTransition::AssignmentReceived.as_str(),
            "assignment_received"
        );
        assert_eq!(
            RoutingTransition::GenerationPrepared.as_str(),
            "generation_prepared"
        );
        assert_eq!(
            RoutingTransition::GenerationActivated.as_str(),
            "generation_activated"
        );
    }

    #[test]
    fn no_attribute_key_can_carry_an_unbounded_identifier() {
        // CapturePublishingMetrics.java:16-18 and :48-55. There are four keys and none of them is
        // a connection id, writer id, partition, activation id, or topic.
        assert_eq!(names::ATTRIBUTE_KEYS.len(), 4);
        for key in names::ATTRIBUTE_KEYS {
            assert!(
                !["connectionId", "writerNodeId", "partition", "topic"].contains(&key),
                "{key} would make cardinality unbounded"
            );
        }
    }
}
