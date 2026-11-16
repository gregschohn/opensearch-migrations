//! The operational event contract.
//!
//! Operational events follow `event=<bounded_name> key=value`
//! (`observabilityOperations.md:13, 19-20`), and the `event=capture_` prefix is a **hard
//! contract**, not a convention: `deployment/k8s/scripts/capture-replay-gameday.sh:63-65` collects
//! the proxy's logs with `grep -E 'event=capture_'`, so an event without that literal substring is
//! invisible to the operator verification script no matter how well structured it is.
//!
//! This module therefore owns three things: the bounded event-name set, the
//! [`capture_event!`](crate::capture_event) macro that renders the convention, and field-for-field
//! emitters for the two summary events whose exact text is asserted by Java tests.
//!
//! # Formatting is free; events and fields are not
//!
//! The rendered layout may change — [`crate::logging`] can emit the same event as a human-readable
//! line or as JSON. What may not change is the event name, the `event=capture_` substring, and the
//! correlation fields. In particular `processId` and `captureActivationId` appear on both summary
//! lines because they are the **only** bridge from a log line to the `captureGapActive` metric
//! point, which carries the same two attribute keys.

use crate::connection_metrics::CloseReason;
use crate::process_metrics::ProcessIdentity;

/// Renders one operational event in the `event=<name> key=value` convention.
///
/// The first argument is a `tracing` level macro name (`info`, `warn`, `error`, `debug`, `trace`),
/// the second the event name, and the rest `key = value` pairs whose values are rendered with
/// [`std::fmt::Display`].
///
/// ```
/// use capture_observability::capture_event;
/// capture_event!(info, "capture_health", writer = "w-0", lagMillis = 12);
/// // emits: event=capture_health writer=w-0 lagMillis=12
/// ```
///
/// Other crates in the workspace emit their own `capture_*` events through this macro so the
/// rendering stays in one place and the `event=capture_` prefix cannot be forgotten.
#[macro_export]
macro_rules! capture_event {
    ($level:ident, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {
        ::tracing::$level!(
            concat!("event={}" $(, " ", stringify!($key), "={}")*),
            $event
            $(, $value)*
        )
    };
}

/// One INFO line summarising the process's configuration at start-up.
pub const CAPTURE_STARTUP_SUMMARY: &str = "capture_startup_summary";
/// One INFO line summarising how the process shut down.
pub const CAPTURE_SHUTDOWN_SUMMARY: &str = "capture_shutdown_summary";
/// Orderly shutdown was interrupted before it finished.
pub const CAPTURE_SHUTDOWN_INTERRUPTED: &str = "capture_shutdown_interrupted";
/// Orderly shutdown failed.
pub const CAPTURE_SHUTDOWN_FAILED: &str = "capture_shutdown_failed";
/// A source connection was dropped, with the bounded close reason.
pub const CAPTURE_CONNECTION_DROPPED: &str = "capture_connection_dropped";
/// A Kafka partition assignment arrived.
pub const CAPTURE_ASSIGNMENT_RECEIVED: &str = "capture_assignment_received";
/// Consumer-group membership degraded.
pub const CAPTURE_MEMBERSHIP_DEGRADED: &str = "capture_membership_degraded";
/// Consumer-group membership failed.
pub const CAPTURE_MEMBERSHIP_FAILED: &str = "capture_membership_failed";
/// A routing generation was prepared.
pub const CAPTURE_GENERATION_PREPARED: &str = "capture_generation_prepared";
/// A superseded writer lane began draining.
pub const CAPTURE_WRITER_DRAINING: &str = "capture_writer_draining";
/// A prepared routing generation became active.
pub const CAPTURE_GENERATION_ACTIVATED: &str = "capture_generation_activated";
/// A writer lane was selected to carry a connection's records.
pub const CAPTURE_ROUTE_SELECTED: &str = "capture_route_selected";
/// A writer-partition heartbeat was later than its deadline allows.
pub const CAPTURE_HEARTBEAT_LATE: &str = "capture_heartbeat_late";
/// A drained writer lane retired.
pub const CAPTURE_WRITER_RETIRED: &str = "capture_writer_retired";
/// Publishing a capture health report failed.
pub const CAPTURE_HEALTH_REPORT_FAILED: &str = "capture_health_report_failed";
/// A periodic capture health line.
pub const CAPTURE_HEALTH: &str = "capture_health";
/// The capture Kafka write gate changed state.
pub const CAPTURE_WRITE_GATE_CHANGED: &str = "capture_write_gate_changed";
/// The capture Kafka publisher failed terminally.
pub const CAPTURE_PUBLISHER_FAILED: &str = "capture_publisher_failed";
/// An acknowledgement was rejected by a broker-time continuity check.
pub const CAPTURE_BROKER_TIME_VIOLATION: &str = "capture_broker_time_violation";
/// A frontside TLS handshake failed, rate-limited by [`crate::log_deduper`].
///
/// New in the Rust implementation. Java logs this as free text
/// (`"TLS handshake failed for frontside channel: <channelId>"`,
/// `ProxyChannelInitializer.java:31`), which the gameday log grep cannot see. Giving it an
/// `event=capture_` name is additive: it makes an existing diagnostic visible to tooling that
/// already looks for the prefix, and the deduper keeps its volume bounded.
pub const CAPTURE_TLS_HANDSHAKE_FAILED: &str = "capture_tls_handshake_failed";

/// Every `capture_*` event the Java proxy emits.
///
/// Transcribed from the operational-event contract. `tests/operational_events.rs` asserts this set
/// is complete and that the events this crate owns actually render with their correlation fields.
/// `capture_protocol_violation` is deliberately absent: it is emitted by the replayer, not by a
/// proxy module.
pub const REQUIRED_EVENTS: &[&str] = &[
    CAPTURE_STARTUP_SUMMARY,
    CAPTURE_SHUTDOWN_SUMMARY,
    CAPTURE_SHUTDOWN_INTERRUPTED,
    CAPTURE_SHUTDOWN_FAILED,
    CAPTURE_CONNECTION_DROPPED,
    CAPTURE_ASSIGNMENT_RECEIVED,
    CAPTURE_MEMBERSHIP_DEGRADED,
    CAPTURE_MEMBERSHIP_FAILED,
    CAPTURE_GENERATION_PREPARED,
    CAPTURE_WRITER_DRAINING,
    CAPTURE_GENERATION_ACTIVATED,
    CAPTURE_ROUTE_SELECTED,
    CAPTURE_HEARTBEAT_LATE,
    CAPTURE_WRITER_RETIRED,
    CAPTURE_HEALTH_REPORT_FAILED,
    CAPTURE_HEALTH,
    CAPTURE_WRITE_GATE_CHANGED,
    CAPTURE_PUBLISHER_FAILED,
    CAPTURE_BROKER_TIME_VIOLATION,
];

/// Whether the process captures traffic at all.
///
/// `CaptureProxy.java:726`: exactly two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// `--noCapture`: forwarding without a capture sink.
    Disabled,
    /// Capturing to Kafka.
    Kafka,
}

impl CaptureMode {
    /// The `captureMode` field value.
    #[must_use]
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Kafka => "kafka",
        }
    }
}

/// The capture failure policy, spelled as the startup log line spells it.
///
/// Note the **underscore**: `CaptureProxy.java:728` lowercases the enum name, yielding `fail_open` /
/// `fail_closed`, while the configuration spelling is hyphenated (`fail-open` / `fail-closed`).
/// The underscore form is preserved here because
/// `CaptureProxySetupTest.startupSummaryStatesCaptureModeAndTelemetryExportConfiguration` asserts
/// the line byte-for-byte and an operator's saved grep would stop matching. The inconsistency is
/// recorded rather than silently resolved; changing it is an owner decision, not an implementation
/// detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicyLabel {
    /// A capture compromise terminates the process.
    FailClosed,
    /// A capture compromise irreversibly enters pass-through.
    FailOpen,
}

impl FailurePolicyLabel {
    /// The `failurePolicy` field value, underscored.
    #[must_use]
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::FailClosed => "fail_closed",
            Self::FailOpen => "fail_open",
        }
    }
}

/// How the process shut down.
///
/// The three outcomes of the shutdown lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Orderly retirement and cleanup both finished inside their deadlines.
    Completed,
    /// A deadline expired, so cleanup was bounded rather than complete.
    Incomplete,
    /// The hard-stop deadline expired and the process was halted.
    HardStop,
}

impl ShutdownOutcome {
    /// The `outcome` field value.
    #[must_use]
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::HardStop => "hard_stop",
        }
    }

    /// Every outcome.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Completed, Self::Incomplete, Self::HardStop]
    }
}

/// The eight fields of the start-up summary.
#[derive(Debug, Clone, Copy)]
pub struct StartupSummary<'a> {
    /// The process identity, which also keys the capture-gap metric points.
    pub identity: &'a ProcessIdentity,
    /// The frontside listener port.
    pub listen_port: u16,
    /// Whether capture is enabled.
    pub capture_mode: CaptureMode,
    /// The traffic topic, or `None` when capture is disabled, which renders as `none`.
    pub kafka_topic: Option<&'a str>,
    /// The configured capture failure policy.
    pub failure_policy: FailurePolicyLabel,
    /// Whether a trace collector endpoint is configured.
    pub trace_export_enabled: bool,
    /// Whether a metric collector endpoint is configured.
    pub metric_export_enabled: bool,
}

/// Emits `event=capture_startup_summary` as one INFO line.
///
/// Field order is fixed and matches `CaptureProxy.java:713-732` exactly, so the assertion in
/// `CaptureProxySetupTest.java:36-58` still describes the Rust line:
///
/// ```text
/// event=capture_startup_summary processId=process-1 captureActivationId=activation-1 \
///   listenPort=8080 captureMode=kafka kafkaTopic=logging-traffic-topic failurePolicy=fail_open \
///   traceExportEnabled=false metricExportEnabled=true
/// ```
pub fn log_startup_summary(summary: &StartupSummary<'_>) {
    capture_event!(
        info,
        CAPTURE_STARTUP_SUMMARY,
        processId = summary.identity.process_id(),
        captureActivationId = summary.identity.capture_activation_id(),
        listenPort = summary.listen_port,
        captureMode = summary.capture_mode.as_log_value(),
        kafkaTopic = summary.kafka_topic.unwrap_or("none"),
        failurePolicy = summary.failure_policy.as_log_value(),
        traceExportEnabled = summary.trace_export_enabled,
        metricExportEnabled = summary.metric_export_enabled,
    );
}

/// Emits `event=capture_shutdown_summary` as one INFO line.
///
/// Three fields, in `CaptureProxy.java:734-747` order:
///
/// ```text
/// event=capture_shutdown_summary outcome=completed processId=process-1 captureActivationId=activation-1
/// ```
pub fn log_shutdown_summary(outcome: ShutdownOutcome, identity: &ProcessIdentity) {
    capture_event!(
        info,
        CAPTURE_SHUTDOWN_SUMMARY,
        outcome = outcome.as_log_value(),
        processId = identity.process_id(),
        captureActivationId = identity.capture_activation_id(),
    );
}

/// Emits `event=capture_shutdown_interrupted` at ERROR.
///
/// `detail` is the interrupting condition, rendered by the caller.
pub fn log_shutdown_interrupted(identity: &ProcessIdentity, detail: &dyn std::fmt::Display) {
    capture_event!(
        error,
        CAPTURE_SHUTDOWN_INTERRUPTED,
        processId = identity.process_id(),
        captureActivationId = identity.capture_activation_id(),
        detail = detail,
    );
}

/// Emits `event=capture_shutdown_failed` at ERROR.
pub fn log_shutdown_failed(identity: &ProcessIdentity, detail: &dyn std::fmt::Display) {
    capture_event!(
        error,
        CAPTURE_SHUTDOWN_FAILED,
        processId = identity.process_id(),
        captureActivationId = identity.capture_activation_id(),
        detail = detail,
    );
}

/// Emits `event=capture_connection_dropped` at WARN.
///
/// This is the log half of the division of labour described in [`crate::connection_metrics`]: the
/// metric carries the bounded `reason` and no identity, this line carries the identity and the same
/// reason, so an operator can go from a spike in one close reason to the specific connections.
pub fn log_connection_dropped(connection_id: &str, reason: CloseReason) {
    capture_event!(
        warn,
        CAPTURE_CONNECTION_DROPPED,
        connection = connection_id,
        reason = reason.as_label(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_required_event_uses_the_grepped_prefix() {
        for event in REQUIRED_EVENTS {
            assert!(
                event.starts_with("capture_"),
                "{event} would be invisible to capture-replay-gameday.sh"
            );
        }
        assert!(CAPTURE_TLS_HANDSHAKE_FAILED.starts_with("capture_"));
    }

    #[test]
    fn required_event_names_are_unique_and_complete() {
        let unique: BTreeSet<&&str> = REQUIRED_EVENTS.iter().collect();
        assert_eq!(unique.len(), REQUIRED_EVENTS.len(), "duplicate event name");
        assert_eq!(
            REQUIRED_EVENTS.len(),
            19,
            "the Java proxy emits 19 distinct capture_* events"
        );
    }

    #[test]
    fn field_values_are_the_bounded_java_spellings() {
        assert_eq!(CaptureMode::Disabled.as_log_value(), "disabled");
        assert_eq!(CaptureMode::Kafka.as_log_value(), "kafka");
        assert_eq!(FailurePolicyLabel::FailOpen.as_log_value(), "fail_open");
        assert_eq!(FailurePolicyLabel::FailClosed.as_log_value(), "fail_closed");
        let outcomes: Vec<&str> = ShutdownOutcome::all()
            .iter()
            .map(|outcome| outcome.as_log_value())
            .collect();
        assert_eq!(outcomes, ["completed", "incomplete", "hard_stop"]);
    }
}
