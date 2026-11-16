//! Capture failure taxonomy (traceability row KAFKA-07).
//!
//! # The one distinction that matters
//!
//! There are exactly **two** terminal classes, and they are not interchangeable:
//!
//! | Class | Trigger set | Effect |
//! |---|---|---|
//! | [`FailureClass::Capture`] | ordinary Kafka failures: send error, missing or non-positive `LogAppendTime`, heartbeat `E` deadline expiry, broker-time continuity violation, payload-limit violation, metadata or probe failure, membership failure before the first assignment | follows `--capture-failure-policy` |
//! | [`FailureClass::UnstableProcess`] | [`CorruptedCaptureState`] and any internal invariant violation | **terminates the process in BOTH fail modes** |
//!
//! `CaptureKafkaPublisher.failForThrowable` (`CaptureKafkaPublisher.java:1204-1210`) is the Java
//! router: `Error || CorruptedCaptureStateException` go to `failUnstableProcess`, everything else to
//! `failPublisher`. [`CaptureFailure::class`] is the same decision, made once at construction so it
//! cannot drift between call sites.
//!
//! An unstable failure never invokes the configurable capture-failure policy
//! (`KafkaCaptureFactory.java:910` passes `notifyCaptureFailurePolicy = false`), because it has
//! already reported through the unstable path.
//!
//! # Canonical identity
//!
//! [`CaptureFailure`] is a cheap clone over one `Arc`, so [`CaptureFailure::is_same`] answers the
//! question the Java tests ask with `assertSame`: every rejected submission, every failed in-flight
//! future, and every gate listener observes the *same* first cause, not a copy or a wrapper
//! (`CaptureKafkaWriteGateTest.terminalListenerReceivesOnlyTheFirstCause`).

use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// The bounded metric `reason` attribute value set.
///
/// These are the literal lowercase strings emitted as the `reason` attribute of
/// `capturePublisherFailures` and `captureWriteGateTransitions`
/// (`CapturePublishingMetrics.java:37-46`, lowercased at `:245-247`). The set is **closed**: it is
/// an external dashboard contract, so a new failure mode maps onto an existing value rather than
/// adding one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailureReason {
    /// A heartbeat acknowledgement deadline `E` expired.
    HeartbeatDeadline,
    /// A broker-time continuity check rejected an acknowledgement.
    BrokerTime,
    /// A serialized record exceeded the configured payload limit.
    PayloadLimit,
    /// `producer.send` or a delivery report reported a failure.
    KafkaSend,
    /// Internal capture state was found corrupted. Always [`FailureClass::UnstableProcess`].
    CorruptedState,
    /// A work queue rejected a task while the component was not shutting down.
    ExecutorRejected,
    /// Group membership failed before the first usable assignment.
    ///
    /// # Deviation
    ///
    /// Java declares this value and never emits it (spec discrepancy D-13): a membership-caused
    /// failure lands in [`FailureReason::Other`], so the documented alarm cannot distinguish a
    /// membership startup failure from any other startup failure. The spec's recommendation is to
    /// emit it, which this crate does.
    Membership,
    /// Any other terminal capture failure, including a capture-configuration refusal.
    Other,
}

impl FailureReason {
    /// The literal metric attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
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

    /// Every value, for exhaustive metric-contract tests.
    #[must_use]
    pub fn all() -> [Self; 8] {
        [
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

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which terminal path a failure takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Follows `--capture-failure-policy`: terminate under `fail-closed`, permanent pass-through
    /// under `fail-open`.
    Capture,
    /// Terminates the process regardless of `--capture-failure-policy`.
    UnstableProcess,
}

impl FailureClass {
    /// Whether this class terminates the process even under `fail-open`.
    #[must_use]
    pub fn always_terminates(self) -> bool {
        matches!(self, Self::UnstableProcess)
    }
}

/// An internal capture-state invariant was violated.
///
/// This is the Rust equivalent of `CorruptedCaptureStateException`
/// (`CorruptedCaptureStateException.java:11-15`). It is **never** routed to the configurable
/// capture-failure policy; it always terminates the process, because the process can no longer
/// reason about what it has and has not captured.
///
/// `check` carries the spec's corruption-catalogue identifier so an operator report can be traced
/// back to the enumerated site (kafka spec §6.5, 28 sites).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptedCaptureState {
    check: CorruptionCheck,
    detail: String,
}

impl CorruptedCaptureState {
    /// Records a corruption at `check` with a human-readable `detail`.
    #[must_use]
    pub fn new(check: CorruptionCheck, detail: impl Into<String>) -> Self {
        Self {
            check,
            detail: detail.into(),
        }
    }

    /// Which enumerated invariant was violated.
    #[must_use]
    pub fn check(&self) -> CorruptionCheck {
        self.check
    }

    /// The violation detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for CorruptedCaptureState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.check.message(), self.detail)
    }
}

impl Error for CorruptedCaptureState {}

/// The enumerated corruption checks, one per site in kafka spec §6.5.
///
/// Numbering follows the spec table so a failure can be matched to its Java site. Sites the Rust
/// design makes unreachable are still enumerated, and the spec note is recorded on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CorruptionCheck {
    /// 1 — `prepareRoutingGeneration` re-registered an existing `(writerNodeId, partition)`.
    WriterPartitionAlreadyPrepared,
    /// 2 — activation with a non-`INITIALIZING` status or no heartbeat baseline.
    WriterPartitionNotReadyForActivation,
    /// 3 — the outgoing generation held a non-`CURRENT` writer partition.
    ActiveGenerationHasNonCurrentWriterPartition,
    /// 4 — the routing hash selected a partition whose writer is not `CURRENT`.
    NewConnectionRoutedToNonCurrentWriterPartition,
    /// 5 — duplicate `(writerNodeId, connectionId)`.
    ConnectionAlreadyRegistered,
    /// 6 — a traffic submission arrived for an unregistered or replaced route.
    ConnectionRouteNotActive,
    /// 7 — a traffic submission reached a `RETIRING`/`RETIRED` writer partition.
    TrafficSubmissionReachedRetiredWriterPartition,
    /// 8 — a second traffic submission followed the accepted terminal record.
    TrafficSubmissionFollowedTerminalRecord,
    /// 9 — terminal removal without an accepted terminal submission.
    ConnectionRouteHasNoAcceptedTerminalRecord,
    /// 10 — abandoning a connection after accepting traffic publication.
    CannotAbandonAfterAcceptedPublication,
    /// 11 — double removal of a connection route.
    ConnectionRouteNotRegistered,
    /// 12 — the per-writer connection set lost an id it should hold.
    ConnectionRegistryInconsistent,
    /// 13 — a heartbeat acknowledgement was accepted after writer-partition retirement.
    HeartbeatAcknowledgementAfterRetirement,
    /// 14 — the pre-forward check ran with no heartbeat baseline.
    CriticalMutationBeforeHeartbeatBaseline,
    /// 15 — `completeWriterRetirement` when not `RETIRING`, or connections remain.
    WriterPartitionCannotCompleteRetirement,
    /// 16 — orderly retirement began with a non-empty connection registry.
    CannotRetireWhileConnectionsRemain,
    /// 17 — orderly retirement found a non-`CURRENT` active partition.
    CurrentWriterPartitionIsNotCurrent,
    /// 18 — lookup of an unregistered `(writerNodeId, partition)`.
    UnknownWriterPartition,
    /// 19 — duplicate publisher-lane creation.
    PublisherLaneAlreadyExists,
    /// 20 — heartbeat submission while the lane is not `OPEN`, or one is already in flight.
    HeartbeatSubmissionNotPermitted,
    /// 21 — an active heartbeat lane had no acknowledgement deadline.
    ActiveHeartbeatLaneHasNoDeadline,
    /// 22 — a heartbeat acknowledgement arrived with no heartbeat in flight.
    HeartbeatAcknowledgementWithoutInFlight,
    /// 23 — a lane returned by the drain scan was not `OPEN`.
    PublisherLaneNotOpenAtRetirement,
    /// 24 — Kafka invoked a delivery callback more than once for one record.
    DuplicateDeliveryCallback,
    /// 25 — publisher-lane lookup miss.
    NoPublisherLaneExists,
    /// 26 — the accepted-send count underflowed.
    AcceptedSendCountNegative,
    /// 27 — a record's identity did not match its immutable connection route.
    TrafficIdentityMismatch,
    /// 28 — orderly retirement finished with a writer partition not `RETIRED`.
    OrderlyRetirementIncomplete,
}

impl CorruptionCheck {
    /// The Java message prefix for this check, preserved so operator greps keep working.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::WriterPartitionAlreadyPrepared => "Writer partition was already prepared",
            Self::WriterPartitionNotReadyForActivation => {
                "Writer partition is not ready for routing activation"
            }
            Self::ActiveGenerationHasNonCurrentWriterPartition => {
                "Active routing generation contains a non-current writer partition"
            }
            Self::NewConnectionRoutedToNonCurrentWriterPartition => {
                "New connection routed to a writer partition that is not current"
            }
            Self::ConnectionAlreadyRegistered => "Connection is already registered for writer",
            Self::ConnectionRouteNotActive => "Connection route is not active",
            Self::TrafficSubmissionReachedRetiredWriterPartition => {
                "Traffic submission reached a retired writer partition"
            }
            Self::TrafficSubmissionFollowedTerminalRecord => {
                "Traffic submission followed the terminal record"
            }
            Self::ConnectionRouteHasNoAcceptedTerminalRecord => {
                "Connection route has no accepted terminal record"
            }
            Self::CannotAbandonAfterAcceptedPublication => {
                "Cannot abandon a connection after accepting traffic publication"
            }
            Self::ConnectionRouteNotRegistered => "Connection route was not registered",
            Self::ConnectionRegistryInconsistent => "Connection registry is inconsistent",
            Self::HeartbeatAcknowledgementAfterRetirement => {
                "Heartbeat acknowledgement followed writer-partition retirement"
            }
            Self::CriticalMutationBeforeHeartbeatBaseline => {
                "Critical Mutation Traffic was acknowledged before the writer partition established \
                 its initial heartbeat broker-time baseline"
            }
            Self::WriterPartitionCannotCompleteRetirement => {
                "Writer partition cannot complete retirement"
            }
            Self::CannotRetireWhileConnectionsRemain => {
                "Cannot retire proxy writers while captured connections remain"
            }
            Self::CurrentWriterPartitionIsNotCurrent => "Current writer partition is not current",
            Self::UnknownWriterPartition => "Unknown writer partition",
            Self::PublisherLaneAlreadyExists => "Publisher lane already exists for",
            Self::HeartbeatSubmissionNotPermitted => "Heartbeat submission is not permitted for",
            Self::ActiveHeartbeatLaneHasNoDeadline => {
                "An active heartbeat lane has no acknowledgement deadline for"
            }
            Self::HeartbeatAcknowledgementWithoutInFlight => {
                "Heartbeat acknowledgement arrived without an in-flight heartbeat for"
            }
            Self::PublisherLaneNotOpenAtRetirement => "Publisher lane is not open at retirement",
            Self::DuplicateDeliveryCallback => {
                "Kafka invoked the acknowledgement callback more than once for"
            }
            Self::NoPublisherLaneExists => "No publisher lane exists for",
            Self::AcceptedSendCountNegative => "Publisher accepted-send count became negative for",
            Self::TrafficIdentityMismatch => {
                "TrafficStream identity does not match its immutable connection route"
            }
            Self::OrderlyRetirementIncomplete => {
                "Orderly proxy retirement completed without retiring every writer partition"
            }
        }
    }
}

/// Why startup refused to proceed.
///
/// All of these happen **before the listener opens**, so no traffic is ever accepted by a process
/// that reports one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StartupRefusal {
    /// The traffic topic does not use `message.timestamp.type=LogAppendTime`.
    ///
    /// # Owner decision OD-5
    ///
    /// This is a distinct capture-configuration failure, not a warning. A `CreateTime` topic stores
    /// the producer's own clock verbatim, which makes capture-before-forward validation and
    /// replayer expiration unsound: the proxy would be comparing its own clock against itself and
    /// calling the result a broker-time proof. The proxy does **not** attempt to change the topic
    /// configuration (OD-7).
    #[error(
        "the traffic topic {topic:?} does not use message.timestamp.type=LogAppendTime: the \
         capability probe for partition {partition} was acknowledged with {observed}, so broker \
         time cannot be proven and capture would be unsound. Set \
         message.timestamp.type=LogAppendTime on the topic (an out-of-band, intentional change) \
         and restart."
    )]
    TopicIsNotLogAppendTime {
        /// The configured traffic topic.
        topic: String,
        /// The probed partition.
        partition: i32,
        /// What the delivery report actually reported.
        observed: String,
    },
    /// The topic layout is not usable for capture routing.
    #[error("the traffic topic layout is invalid: {0}")]
    InvalidTopicLayout(#[from] TopicLayoutError),
    /// A client configuration was rejected.
    #[error("capture Kafka client configuration is invalid: {0}")]
    Configuration(#[from] ClientConfigError),
    /// The capability probe did not complete.
    #[error("the capture capability probe failed: {0}")]
    CapabilityProbe(String),
    /// Group membership failed before the first usable assignment.
    #[error("capture group membership failed before the first usable routing generation: {0}")]
    Membership(String),
    /// The first routing generation never became usable.
    #[error("the first capture routing generation never became usable: {0}")]
    FirstGeneration(String),
}

impl StartupRefusal {
    /// The metric `reason` attribute for this refusal.
    ///
    /// Capture-configuration refusals map to [`FailureReason::Other`] rather than adding a value to
    /// the closed set; the distinction is carried by the error type, the log event, and the exit
    /// path, which is where an operator needs it.
    #[must_use]
    pub fn reason(&self) -> FailureReason {
        match self {
            Self::Membership(_) => FailureReason::Membership,
            _ => FailureReason::Other,
        }
    }
}

/// The traffic topic's partition layout is unusable (traceability row KAFKA-10, owner decision
/// OD-7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TopicLayoutError {
    /// Kafka reported no partitions for the topic. Retriable: the topic may not exist yet.
    #[error("Kafka returned no partitions for topic {topic:?}")]
    NoPartitions {
        /// The configured traffic topic.
        topic: String,
    },
    /// The partition indices are not `0..N-1`.
    #[error(
        "expected contiguous Kafka partitions 0..{expected_count}-1 for {topic:?}, but index \
         {index} is partition {partition}"
    )]
    NonContiguous {
        /// The configured traffic topic.
        topic: String,
        /// How many partitions were reported.
        expected_count: usize,
        /// The position in the ascending-sorted list.
        index: i32,
        /// The partition id found at that position.
        partition: i32,
    },
    /// A partition has no current leader. Retriable: leadership may be in election.
    #[error("Kafka returned no current leader for {topic:?} partition {partition}")]
    NoLeader {
        /// The configured traffic topic.
        topic: String,
        /// The leaderless partition.
        partition: i32,
    },
    /// The partition count disagrees with the configured minimum.
    ///
    /// # Owner decision OD-7
    ///
    /// The proxy never increases the partition count. A topic below the configured minimum is an
    /// out-of-band provisioning task, so startup refuses rather than mutating the topic.
    #[error(
        "the traffic topic {topic:?} has {actual} partitions but at least {minimum} are \
         configured. The proxy never changes the partition count (owner decision OD-7): increase \
         the topic's partitions out of band and restart."
    )]
    BelowConfiguredMinimum {
        /// The configured traffic topic.
        topic: String,
        /// Partitions the topic actually has.
        actual: usize,
        /// Partitions the configuration requires.
        minimum: usize,
    },
    /// Metadata could not be fetched at all. Retriable.
    #[error("could not fetch Kafka metadata for topic {topic:?}: {detail}")]
    MetadataUnavailable {
        /// The configured traffic topic.
        topic: String,
        /// The underlying error text.
        detail: String,
    },
}

impl TopicLayoutError {
    /// Whether startup should retry rather than fail.
    ///
    /// Matches the Java split exactly (`KafkaCaptureFactory.java:371-372` retries forever on
    /// `IllegalStateException` from metadata validation): absent partitions, a missing leader, and
    /// unavailable metadata are transient facts about the cluster, while a non-contiguous layout or
    /// a count below the configured minimum require an operator.
    #[must_use]
    pub fn is_retriable(&self) -> bool {
        match self {
            Self::NoPartitions { .. }
            | Self::NoLeader { .. }
            | Self::MetadataUnavailable { .. } => true,
            Self::NonContiguous { .. } | Self::BelowConfiguredMinimum { .. } => false,
        }
    }
}

/// A client configuration was rejected before any client was created (traceability row KAFKA-09).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientConfigError {
    /// A property file key is not on the allowlist for this client role.
    ///
    /// librdkafka **hard-fails** on an unknown configuration key, where the Java client only logs
    /// `was supplied but isn't a known config` and continues (spec discrepancy D-8). Java's own
    /// membership consumer properties contain `max.in.flight.requests.per.connection`
    /// (`KafkaConfig.java:142-147` removes six producer keys but not that one), so a
    /// copy-and-delete port would fail to start. Each client's configuration is therefore built
    /// from an allowlist.
    #[error(
        "property {key:?} is not permitted for the capture {role} client. librdkafka rejects an \
         unknown or unsupported property at client creation, so the capture proxy builds each \
         client's configuration from an allowlist rather than passing a property file through."
    )]
    PropertyNotAllowed {
        /// The rejected key.
        key: String,
        /// Which client role rejected it.
        role: &'static str,
    },
    /// A property file tried to weaken a forced ordering guarantee.
    #[error(
        "property {key:?} cannot be set to {attempted:?}: the capture protocol forces {forced:?} \
         to preserve per-partition append order, and a user property file must not weaken it"
    )]
    CannotWeakenOrdering {
        /// The forced key.
        key: &'static str,
        /// What the property file asked for.
        attempted: String,
        /// What the protocol requires.
        forced: &'static str,
    },
    /// A required credential was missing.
    ///
    /// Java omits the SCRAM JAAS line entirely when either credential is null
    /// (`KafkaConfig.java:217`), which silently authenticates as nobody and fails later with an
    /// opaque broker error. This is an explicit startup error instead.
    #[error("capture Kafka auth mode {mode} requires {missing}")]
    MissingCredential {
        /// The configured auth mode.
        mode: &'static str,
        /// What was missing.
        missing: &'static str,
    },
    /// `bootstrap.servers` was empty.
    #[error("capture Kafka requires a non-empty bootstrap server list")]
    MissingBootstrapServers,
    /// A Java TLS store property has no librdkafka equivalent.
    ///
    /// librdkafka reads PEM trust material and PKCS#12 client keystores, but not Java key stores.
    /// A JKS (or PKCS#12) *trust* store therefore cannot be honoured; refusing to start is safer
    /// than silently connecting without the operator's trust anchor.
    #[error("capture Kafka TLS property {key:?}={value:?} cannot be honoured: {remedy}")]
    UnsupportedJavaTlsStore {
        /// The Java property.
        key: String,
        /// Its configured value.
        value: String,
        /// What to configure instead.
        remedy: &'static str,
    },
    /// The property file could not be read.
    #[error("could not read the capture Kafka property file {path:?}: {detail}")]
    PropertyFileUnreadable {
        /// The configured path.
        path: String,
        /// The I/O error text.
        detail: String,
    },
    /// A property file line was not `key=value`.
    #[error("capture Kafka property file {path:?} line {line} is not key=value: {content:?}")]
    PropertyFileMalformed {
        /// The configured path.
        path: String,
        /// 1-based line number.
        line: usize,
        /// The offending line.
        content: String,
    },
    /// The effective client configuration did not match what was requested.
    ///
    /// Closes spec discrepancy D-1: Java forces the three ordering properties when *building*
    /// properties, but `KafkaCaptureFactory` receives an already-constructed producer
    /// (`KafkaCaptureFactory.java:221`) and never inspects it, so an embedder that builds its own
    /// producer gets no ordering guarantees at all. The Rust port constructs the producer itself
    /// and reads the effective values back.
    #[error(
        "the effective capture producer configuration does not preserve ordering: {key} is \
         {actual:?} but must be {expected:?}"
    )]
    EffectiveConfigMismatch {
        /// Which property disagreed.
        key: &'static str,
        /// The effective value read back from the client.
        actual: String,
        /// The required value.
        expected: &'static str,
    },
    /// An auth mode is configured that this build cannot perform.
    #[error("capture Kafka auth mode {mode} is unavailable: {detail}")]
    AuthUnavailable {
        /// The configured auth mode.
        mode: &'static str,
        /// Why it is unavailable and what to do.
        detail: String,
    },
    /// librdkafka rejected the assembled configuration.
    #[error("librdkafka rejected the capture {role} client configuration: {detail}")]
    ClientCreation {
        /// Which client role failed.
        role: &'static str,
        /// The librdkafka error text.
        detail: String,
    },
}

/// A terminal capture failure, cloneable and identity-comparable.
///
/// Construct through the classifying constructors so the [`FailureClass`] is decided exactly once.
#[derive(Debug, Clone)]
pub struct CaptureFailure(Arc<FailureInner>);

#[derive(Debug)]
struct FailureInner {
    reason: FailureReason,
    class: FailureClass,
    message: String,
    corruption: Option<CorruptedCaptureState>,
}

impl CaptureFailure {
    /// An ordinary capture failure, subject to `--capture-failure-policy`.
    #[must_use]
    pub fn capture(reason: FailureReason, message: impl Into<String>) -> Self {
        debug_assert_ne!(
            reason,
            FailureReason::CorruptedState,
            "corrupted state is always an unstable-process failure; use CaptureFailure::corrupted"
        );
        Self(Arc::new(FailureInner {
            reason,
            class: FailureClass::Capture,
            message: message.into(),
            corruption: None,
        }))
    }

    /// A corruption failure. Terminates the process in both fail modes.
    #[must_use]
    pub fn corrupted(corruption: CorruptedCaptureState) -> Self {
        Self(Arc::new(FailureInner {
            reason: FailureReason::CorruptedState,
            class: FailureClass::UnstableProcess,
            message: corruption.to_string(),
            corruption: Some(corruption),
        }))
    }

    /// An unstable-process failure that is not a catalogued corruption, such as a work queue
    /// rejecting a task while the component was still running.
    #[must_use]
    pub fn unstable(reason: FailureReason, message: impl Into<String>) -> Self {
        Self(Arc::new(FailureInner {
            reason,
            class: FailureClass::UnstableProcess,
            message: message.into(),
            corruption: None,
        }))
    }

    /// The bounded metric reason.
    #[must_use]
    pub fn reason(&self) -> FailureReason {
        self.0.reason
    }

    /// Which terminal path this failure takes.
    #[must_use]
    pub fn class(&self) -> FailureClass {
        self.0.class
    }

    /// The failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// The corruption detail, when this failure is a catalogued corruption.
    #[must_use]
    pub fn corruption(&self) -> Option<&CorruptedCaptureState> {
        self.0.corruption.as_ref()
    }

    /// Whether both handles refer to the identical failure instance.
    ///
    /// The Rust answer to the Java suite's `assertSame` assertions.
    #[must_use]
    pub fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Whether this failure must terminate the process regardless of policy.
    #[must_use]
    pub fn always_terminates(&self) -> bool {
        self.0.class.always_terminates()
    }
}

impl fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl Error for CaptureFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0
            .corruption
            .as_ref()
            .map(|c| c as &(dyn Error + 'static))
    }
}

impl From<CorruptedCaptureState> for CaptureFailure {
    fn from(value: CorruptedCaptureState) -> Self {
        Self::corrupted(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_value_set_is_the_closed_metric_contract() {
        // CapturePublishingMetrics.java:37-46, lowercased at :245-247. Dashboards select on these
        // literals, so the set and the spellings are frozen.
        let names: Vec<&str> = FailureReason::all().iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "heartbeat_deadline",
                "broker_time",
                "payload_limit",
                "kafka_send",
                "corrupted_state",
                "executor_rejected",
                "membership",
                "other",
            ]
        );
    }

    #[test]
    fn corruption_is_always_an_unstable_process_failure() {
        let failure = CaptureFailure::corrupted(CorruptedCaptureState::new(
            CorruptionCheck::ConnectionRouteNotActive,
            "route conn-1",
        ));
        assert_eq!(failure.class(), FailureClass::UnstableProcess);
        assert_eq!(failure.reason(), FailureReason::CorruptedState);
        assert!(
            failure.always_terminates(),
            "a corrupted-state failure must terminate under fail-open too"
        );
    }

    #[test]
    fn an_ordinary_kafka_failure_follows_the_configurable_policy() {
        let failure = CaptureFailure::capture(FailureReason::KafkaSend, "send failed");
        assert_eq!(failure.class(), FailureClass::Capture);
        assert!(!failure.always_terminates());
    }

    #[test]
    fn clones_are_the_same_instance_but_distinct_failures_are_not() {
        let first = CaptureFailure::capture(FailureReason::BrokerTime, "stale");
        let clone = first.clone();
        let other = CaptureFailure::capture(FailureReason::BrokerTime, "stale");
        assert!(first.is_same(&clone));
        assert!(
            !first.is_same(&other),
            "equal text is not the same instance"
        );
    }

    #[test]
    fn layout_errors_split_retriable_from_operator_action() {
        assert!(
            TopicLayoutError::NoLeader {
                topic: "t".into(),
                partition: 3
            }
            .is_retriable(),
            "leader election is transient"
        );
        assert!(
            TopicLayoutError::NoPartitions { topic: "t".into() }.is_retriable(),
            "the topic may not exist yet"
        );
        assert!(
            !TopicLayoutError::BelowConfiguredMinimum {
                topic: "t".into(),
                actual: 1,
                minimum: 2
            }
            .is_retriable(),
            "OD-7: the proxy never grows the topic, so this needs an operator"
        );
        assert!(
            !TopicLayoutError::NonContiguous {
                topic: "t".into(),
                expected_count: 3,
                index: 1,
                partition: 2
            }
            .is_retriable()
        );
    }

    #[test]
    fn a_membership_startup_refusal_is_attributed_to_membership() {
        // Deviation from Java, per spec D-13: Java lands this in `other`, so the documented alarm
        // cannot tell a membership startup failure from any other one.
        assert_eq!(
            StartupRefusal::Membership("poll failed".into()).reason(),
            FailureReason::Membership
        );
    }

    #[test]
    fn a_create_time_topic_refusal_names_the_remedy() {
        let refusal = StartupRefusal::TopicIsNotLogAppendTime {
            topic: "logging-traffic-topic".into(),
            partition: 0,
            observed: "CreateTime(1794790929566)".into(),
        };
        let text = refusal.to_string();
        assert!(text.contains("message.timestamp.type=LogAppendTime"));
        assert!(text.contains("out-of-band"), "OD-7: not the proxy's job");
        assert_eq!(refusal.reason(), FailureReason::Other);
    }
}
