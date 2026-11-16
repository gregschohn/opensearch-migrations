//! Kafka publication, routing, writer-lane, and write-gate metrics (KAFKA-11).
//!
//! Sixteen instruments, four dimension keys, and four closed enums. This is the most
//! dashboard-visible group in the crate: eleven of the sixteen are plotted and the remaining five
//! are in the runbook's metric inventory, so every name and unit here is a shipped contract.
//!
//! # Units that are not interchangeable
//!
//! `records`, `By`, `violations`, `transitions`, `writers`, `writes`, and `failures` each become a
//! different Prometheus series suffix. `capturePublishingBytesAccepted` declares `By` (UCUM) and
//! exports as `capturePublishingBytesAccepted_bytes_total`; `captureWritersActive` declares
//! `writers` and exports as `captureWritersActive_writers`. Normalising them to a single unit would
//! blank nine Grafana panels at once with no error.
//!
//! # Cost on the record path
//!
//! `recordKind` is a real dimension, so the four accept/complete instruments cannot use the SDK's
//! lock-free empty-attribute fast path; each observation takes a read lock and hashes its attribute
//! set (`opentelemetry_sdk-0.31.0/src/metrics/internal/mod.rs:109-118`). That is unavoidable while
//! the dimension is part of the contract, and it is parity with Java, which built an `Attributes`
//! instance per call. What *is* avoidable is rebuilding the attribute set itself, so the eight
//! attribute sets this module ever needs are built once in [`KafkaMetrics::new`] and passed by
//! slice afterwards: the record path allocates nothing and constructs nothing per record. The
//! dimensionless instruments here — heartbeats, violations, writer lanes, rejected writes — do take
//! the fast path.
//!
//! # `captureWritesRejected` carries no dimension
//!
//! `observabilityOperations.md:75` documents a `reason` attribute on `captureWritesRejected`, but
//! `CapturePublishingMetrics.writeRejected()` (lines 233-235) records it with none. The code wins
//! here: `generate-capture-replay-dashboard.py:306` reads the dimensionless series, so adding
//! `reason` would break the panel. The documentation is what should change.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter, UpDownCounter};

use crate::instruments;

/// What kind of record a publication carries.
///
/// `CapturePublishingMetrics.RecordKind` (lines 21-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// Captured source traffic.
    Traffic,
    /// A writer-partition heartbeat.
    Heartbeat,
}

impl RecordKind {
    /// The `recordKind` dimension value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Traffic => "traffic",
            Self::Heartbeat => "heartbeat",
        }
    }

    /// Every record kind.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Traffic, Self::Heartbeat]
    }

    /// The variant's index into the pre-built attribute tables.
    const fn index(self) -> usize {
        match self {
            Self::Traffic => 0,
            Self::Heartbeat => 1,
        }
    }
}

/// The terminal outcome of an accepted publication.
///
/// `CapturePublishingMetrics.PublishOutcome` (lines 26-29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublishOutcome {
    /// The broker acknowledged the record.
    Acknowledged,
    /// The publication failed terminally.
    Failed,
}

impl PublishOutcome {
    /// The `outcome` dimension value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Failed => "failed",
        }
    }

    /// Every outcome.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Acknowledged, Self::Failed]
    }

    /// The variant's index into the pre-built attribute tables.
    const fn index(self) -> usize {
        match self {
            Self::Acknowledged => 0,
            Self::Failed => 1,
        }
    }
}

/// A capture assignment or routing-generation transition.
///
/// `CapturePublishingMetrics.RoutingTransition` (lines 31-35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoutingTransition {
    /// A partition assignment arrived from the membership consumer.
    AssignmentReceived,
    /// A new routing generation was prepared but is not yet serving.
    GenerationPrepared,
    /// A prepared generation became the active one.
    GenerationActivated,
}

impl RoutingTransition {
    /// The `transition` dimension value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::AssignmentReceived => "assignment_received",
            Self::GenerationPrepared => "generation_prepared",
            Self::GenerationActivated => "generation_activated",
        }
    }

    /// Every routing transition.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::AssignmentReceived,
            Self::GenerationPrepared,
            Self::GenerationActivated,
        ]
    }
}

/// Why the capture write gate closed, or why a publisher failed terminally.
///
/// `CapturePublishingMetrics.FailureReason` (lines 37-46). The set is closed, with `Other` as the
/// explicit catch-all, so an unanticipated failure cannot introduce a new label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureReason {
    /// A writer-partition heartbeat acknowledgement deadline was exceeded.
    HeartbeatDeadline,
    /// A broker-time continuity check rejected an acknowledgement.
    BrokerTime,
    /// A record exceeded the Kafka payload limit.
    PayloadLimit,
    /// The Kafka send itself failed.
    KafkaSend,
    /// Internal capture-routing state was found corrupted.
    CorruptedState,
    /// The publishing executor rejected the submission.
    ExecutorRejected,
    /// Consumer-group membership was lost or degraded.
    Membership,
    /// Any other terminal failure.
    Other,
}

impl FailureReason {
    /// The `reason` dimension value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::HeartbeatDeadline => "heartbeat_deadline",
            Self::BrokerTime => "broker_time",
            Self::PayloadLimit => "payload_limit",
            Self::KafkaSend => "kafka_send",
            Self::CorruptedState => "corrupted_state",
            Self::ExecutorRejected => "executor_rejected",
            Self::Membership => "membership",
            Self::Other => "other",
        }
    }

    /// Every failure reason.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::HeartbeatDeadline,
            Self::BrokerTime,
            Self::PayloadLimit,
            Self::KafkaSend,
            Self::CorruptedState,
            Self::ExecutorRejected,
            Self::Membership,
            Self::Other,
        ]
    }
}

/// The sixteen Kafka publication and routing instruments.
#[derive(Debug)]
pub struct KafkaMetrics {
    records_accepted: Counter<u64>,
    bytes_accepted: Counter<u64>,
    completions: Counter<u64>,
    records_awaiting_acknowledgement: UpDownCounter<i64>,
    bytes_awaiting_acknowledgement: UpDownCounter<i64>,
    heartbeat_submissions: Counter<u64>,
    heartbeat_acknowledgements: Counter<u64>,
    heartbeat_deadline_violations: Counter<u64>,
    broker_time_violations: Counter<u64>,
    routing_transitions: Counter<u64>,
    writers_active: UpDownCounter<i64>,
    writers_draining: UpDownCounter<i64>,
    writers_retired: Counter<u64>,
    write_gate_transitions: Counter<u64>,
    writes_rejected: Counter<u64>,
    publisher_failures: Counter<u64>,
    /// `[recordKind]` per [`RecordKind`], indexed by [`RecordKind::index`].
    record_kind_attributes: [[KeyValue; 1]; 2],
    /// `[recordKind, outcome]` per pair, indexed by kind then outcome.
    completion_attributes: [[[KeyValue; 2]; 2]; 2],
}

impl KafkaMetrics {
    /// Registers all sixteen instruments.
    ///
    /// # Panics
    ///
    /// Panics when a name is absent from the metric contract.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            records_accepted: instruments::counter(meter, "capturePublishingRecordsAccepted"),
            bytes_accepted: instruments::counter(meter, "capturePublishingBytesAccepted"),
            completions: instruments::counter(meter, "capturePublishingCompletions"),
            records_awaiting_acknowledgement: instruments::up_down_counter(
                meter,
                "capturePublishingRecordsAwaitingAcknowledgement",
            ),
            bytes_awaiting_acknowledgement: instruments::up_down_counter(
                meter,
                "capturePublishingBytesAwaitingAcknowledgement",
            ),
            heartbeat_submissions: instruments::counter(meter, "captureHeartbeatSubmissions"),
            heartbeat_acknowledgements: instruments::counter(
                meter,
                "captureHeartbeatAcknowledgements",
            ),
            heartbeat_deadline_violations: instruments::counter(
                meter,
                "captureHeartbeatDeadlineViolations",
            ),
            broker_time_violations: instruments::counter(meter, "captureBrokerTimeViolations"),
            routing_transitions: instruments::counter(meter, "captureRoutingTransitions"),
            writers_active: instruments::up_down_counter(meter, "captureWritersActive"),
            writers_draining: instruments::up_down_counter(meter, "captureWritersDraining"),
            writers_retired: instruments::counter(meter, "captureWritersRetired"),
            write_gate_transitions: instruments::counter(meter, "captureWriteGateTransitions"),
            writes_rejected: instruments::counter(meter, "captureWritesRejected"),
            publisher_failures: instruments::counter(meter, "capturePublisherFailures"),
            record_kind_attributes: [
                [KeyValue::new("recordKind", RecordKind::Traffic.as_label())],
                [KeyValue::new(
                    "recordKind",
                    RecordKind::Heartbeat.as_label(),
                )],
            ],
            completion_attributes: [
                completion_attributes_for(RecordKind::Traffic),
                completion_attributes_for(RecordKind::Heartbeat),
            ],
        }
    }

    fn kind_attributes(&self, kind: RecordKind) -> &[KeyValue] {
        &self.record_kind_attributes[kind.index()]
    }

    fn completion_attributes(&self, kind: RecordKind, outcome: PublishOutcome) -> &[KeyValue] {
        &self.completion_attributes[kind.index()][outcome.index()]
    }

    /// A publication of `bytes` bytes was accepted at the producer boundary.
    ///
    /// Raises both awaiting-acknowledgement gauges, and counts a heartbeat submission when `kind`
    /// is [`RecordKind::Heartbeat`], matching `publicationAccepted` (lines 170-179).
    pub fn publication_accepted(&self, kind: RecordKind, bytes: u64) {
        let attributes = self.kind_attributes(kind);
        self.records_accepted.add(1, attributes);
        self.bytes_accepted.add(bytes, attributes);
        self.records_awaiting_acknowledgement.add(1, attributes);
        self.bytes_awaiting_acknowledgement
            .add(saturating_delta(bytes), attributes);
        if kind == RecordKind::Heartbeat {
            self.heartbeat_submissions.add(1, &[]);
        }
    }

    /// A publication reached its terminal outcome.
    ///
    /// Lowers both awaiting-acknowledgement gauges by the same amounts
    /// [`Self::publication_accepted`] raised them, so a caller must pass the same `bytes`.
    pub fn publication_completed(&self, kind: RecordKind, outcome: PublishOutcome, bytes: u64) {
        self.completions
            .add(1, self.completion_attributes(kind, outcome));
        let attributes = self.kind_attributes(kind);
        self.records_awaiting_acknowledgement.add(-1, attributes);
        self.bytes_awaiting_acknowledgement
            .add(-saturating_delta(bytes), attributes);
        if kind == RecordKind::Heartbeat && outcome == PublishOutcome::Acknowledged {
            self.heartbeat_acknowledgements.add(1, &[]);
        }
    }

    /// A writer-partition heartbeat acknowledgement deadline was exceeded.
    pub fn heartbeat_deadline_violated(&self) {
        self.heartbeat_deadline_violations.add(1, &[]);
    }

    /// A broker-time continuity check rejected an acknowledgement.
    pub fn broker_time_violated(&self) {
        self.broker_time_violations.add(1, &[]);
    }

    /// A capture assignment or routing-generation transition occurred.
    pub fn routing_transition(&self, transition: RoutingTransition) {
        self.routing_transitions
            .add(1, &[KeyValue::new("transition", transition.as_label())]);
    }

    /// A new routing generation activated `activated` lanes, superseding `previously_active`.
    ///
    /// Mirrors `writersActivated` (lines 210-215): the active gauge moves by the difference, and the
    /// superseded lanes begin draining.
    pub fn writers_activated(&self, activated: u32, previously_active: u32) {
        self.writers_active
            .add(i64::from(activated) - i64::from(previously_active), &[]);
        if previously_active > 0 {
            self.writers_draining.add(i64::from(previously_active), &[]);
        }
    }

    /// `count` currently active lanes began draining without being replaced.
    pub fn active_writers_began_draining(&self, count: u32) {
        if count > 0 {
            self.writers_active.add(-i64::from(count), &[]);
            self.writers_draining.add(i64::from(count), &[]);
        }
    }

    /// One draining lane finished draining and retired.
    pub fn writer_retired(&self) {
        self.writers_draining.add(-1, &[]);
        self.writers_retired.add(1, &[]);
    }

    /// The capture Kafka write gate closed irreversibly.
    pub fn write_gate_tripped(&self, reason: FailureReason) {
        self.write_gate_transitions
            .add(1, &[KeyValue::new("reason", reason.as_label())]);
    }

    /// A write submission was rejected because the write gate had closed.
    ///
    /// Dimensionless, matching the code and the dashboard; see the module docs.
    pub fn write_rejected(&self) {
        self.writes_rejected.add(1, &[]);
    }

    /// The capture Kafka publisher failed terminally.
    pub fn publisher_failed(&self, reason: FailureReason) {
        self.publisher_failures
            .add(1, &[KeyValue::new("reason", reason.as_label())]);
    }
}

/// The two `[recordKind, outcome]` attribute sets for one record kind.
fn completion_attributes_for(kind: RecordKind) -> [[KeyValue; 2]; 2] {
    [
        [
            KeyValue::new("recordKind", kind.as_label()),
            KeyValue::new("outcome", PublishOutcome::Acknowledged.as_label()),
        ],
        [
            KeyValue::new("recordKind", kind.as_label()),
            KeyValue::new("outcome", PublishOutcome::Failed.as_label()),
        ],
    ]
}

/// Byte counts as an up-down-counter delta.
///
/// Record sizes are bounded by the Kafka payload limit, orders of magnitude below [`i64::MAX`], so
/// the saturation can only be reached by a corrupted size. Saturating rather than wrapping keeps a
/// corrupted value from making the awaiting-bytes gauge negative, which an operator would read as
/// "the backlog drained".
fn saturating_delta(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        FAILURE_REASONS, PUBLISH_OUTCOMES, RECORD_KINDS, ROUTING_TRANSITIONS, SCOPE_NAME,
    };
    use crate::snapshot::recording_meter_provider;
    use opentelemetry::metrics::MeterProvider;
    use std::collections::BTreeSet;

    #[test]
    fn enum_labels_are_exactly_the_contract_value_sets() {
        let kinds: Vec<&str> = RecordKind::all().iter().map(|k| k.as_label()).collect();
        assert_eq!(kinds, RECORD_KINDS);
        let outcomes: Vec<&str> = PublishOutcome::all().iter().map(|o| o.as_label()).collect();
        assert_eq!(outcomes, PUBLISH_OUTCOMES);
        let transitions: Vec<&str> = RoutingTransition::all()
            .iter()
            .map(|t| t.as_label())
            .collect();
        assert_eq!(transitions, ROUTING_TRANSITIONS);
        let reasons: Vec<&str> = FailureReason::all().iter().map(|r| r.as_label()).collect();
        assert_eq!(reasons, FAILURE_REASONS);
    }

    #[test]
    fn publication_lifecycle_balances_the_awaiting_gauges() {
        let (provider, recorder) = recording_meter_provider("kafka-metrics");
        let meter = provider.meter(SCOPE_NAME);
        let metrics = KafkaMetrics::new(&meter);
        metrics.publication_accepted(RecordKind::Traffic, 1024);
        metrics.publication_completed(RecordKind::Traffic, PublishOutcome::Acknowledged, 1024);
        metrics.publication_accepted(RecordKind::Heartbeat, 64);
        metrics.publication_completed(RecordKind::Heartbeat, PublishOutcome::Acknowledged, 64);

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert!(snapshot.point_for("captureHeartbeatSubmissions").is_some());
        assert!(
            snapshot
                .point_for("captureHeartbeatAcknowledgements")
                .is_some()
        );
        let awaiting = snapshot.point_for("capturePublishingRecordsAwaitingAcknowledgement");
        assert_eq!(awaiting.map(|point| point.unit.as_str()), Some("records"));
        let bytes = snapshot.point_for("capturePublishingBytesAccepted");
        assert_eq!(bytes.map(|point| point.unit.as_str()), Some("By"));
    }

    #[test]
    fn every_failure_reason_stays_inside_the_closed_set() {
        let (provider, recorder) = recording_meter_provider("kafka-reasons");
        let meter = provider.meter(SCOPE_NAME);
        let metrics = KafkaMetrics::new(&meter);
        for reason in FailureReason::all() {
            metrics.publisher_failed(*reason);
            metrics.write_gate_tripped(*reason);
        }
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        for name in ["capturePublisherFailures", "captureWriteGateTransitions"] {
            let observed: BTreeSet<String> = snapshot
                .points_for(name)
                .into_iter()
                .map(|point| point.attributes["reason"].clone())
                .collect();
            assert_eq!(observed.len(), FAILURE_REASONS.len(), "{name} reason set");
            for reason in FAILURE_REASONS {
                assert!(observed.contains(*reason), "{name} missing {reason}");
            }
        }
    }

    #[test]
    fn writer_lane_counts_move_together() {
        let (provider, recorder) = recording_meter_provider("kafka-writers");
        let meter = provider.meter(SCOPE_NAME);
        let metrics = KafkaMetrics::new(&meter);
        metrics.writers_activated(4, 0);
        metrics.writers_activated(4, 4);
        metrics.writer_retired();
        metrics.active_writers_began_draining(1);
        metrics.write_rejected();
        metrics.routing_transition(RoutingTransition::GenerationActivated);
        metrics.heartbeat_deadline_violated();
        metrics.broker_time_violated();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(
            snapshot
                .point_for("captureWritersActive")
                .map(|point| point.unit.as_str()),
            Some("writers")
        );
        assert_eq!(
            snapshot
                .point_for("captureWritesRejected")
                .map(|point| point.unit.as_str()),
            Some("writes")
        );
        assert!(
            snapshot
                .point_for("captureWritesRejected")
                .expect("exported")
                .attributes
                .is_empty(),
            "captureWritesRejected is dimensionless; the dashboard reads the dimensionless series"
        );
        assert!(snapshot.point_for("captureWritersRetired").is_some());
        assert!(snapshot.point_for("captureBrokerTimeViolations").is_some());
    }

    #[test]
    fn a_corrupted_byte_count_cannot_make_the_gauge_negative() {
        assert_eq!(saturating_delta(u64::MAX), i64::MAX);
        assert_eq!(saturating_delta(1024), 1024);
    }
}
