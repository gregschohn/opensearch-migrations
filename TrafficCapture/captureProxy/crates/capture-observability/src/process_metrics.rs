//! Process identity and the capture-gap signal (PROXY-02).
//!
//! The proxy's capture-process state machine is irreversible: a process that loses required capture
//! either terminates (fail-closed) or lives on forwarding uncaptured traffic (fail-open). This
//! module projects that machine into the three low-cardinality instruments an operator watches, and
//! owns the process identity that ties those metric points to the startup and shutdown log lines.
//!
//! # The capture-gap latch
//!
//! `captureGapActive` is driven by a local boolean, not by the number of notifications received
//! (`CaptureProcessMetrics.java:36, 76-89`). Repeated `required capture failed` notifications must
//! leave the gauge at exactly 1, because an operator reads it as "how many proxy processes are
//! currently serving traffic without capturing it" and a double-count would misstate the blast
//! radius. [`CaptureProcessMetrics::record_transition`] reproduces that latch.
//!
//! # Why `mode` and `sourceForwardingContinued` are constants
//!
//! Both dimensions only ever carry one value (`fail-open`, `true`). They exist so the metric point
//! itself states what the gap means, without the reader having to join to configuration. The
//! `CloudWatch` dashboard's `SEARCH` expression enumerates them by name
//! (`generate-capture-replay-dashboard.py:311-313`), so dropping either would empty the panel.

use std::sync::Mutex;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter, UpDownCounter};

use crate::contract::{OPERATING_MODES, SOURCE_FORWARDING_CONTINUED};
use crate::instruments;

/// The capture-process state a transition enters.
///
/// `CaptureProcessState.State`. [`CaptureProcessState::Capture`] is the initial state, never a
/// transition target, which is why [`CaptureProcessMetrics::record_transition`] rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureProcessState {
    /// Authoritative capture: the initial state of every process.
    Capture,
    /// Irreversible pass-through after a capture failure under the fail-open policy.
    PassThrough,
    /// Immediate process termination.
    Terminating,
}

/// Recording a transition into [`CaptureProcessState::Capture`] was attempted.
///
/// Mirrors `CaptureProcessMetrics.java:90-92`, which throws rather than silently ignoring it: a
/// caller asking to re-enter capture has misunderstood the irreversibility of the state machine, and
/// quietly dropping the request would hide that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Capture is the initial capture-process state, not a process transition")]
pub struct NotATransition;

/// The proxy's process identity.
///
/// Both fields appear as metric dimensions on the capture-gap instruments **and** as fields on the
/// `capture_startup_summary` and `capture_shutdown_summary` events. That duplication is the whole
/// point: it is the only bridge between a `captureGapActive` point and the logs of the process that
/// raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    process_id: String,
    capture_activation_id: String,
}

impl ProcessIdentity {
    /// Builds an identity from explicit values.
    ///
    /// Blank values are preserved here and omitted at the point of use, matching
    /// `CaptureProcessMetrics.java:47-53`.
    #[must_use]
    pub fn new(process_id: impl Into<String>, capture_activation_id: impl Into<String>) -> Self {
        Self {
            process_id: process_id.into(),
            capture_activation_id: capture_activation_id.into(),
        }
    }

    /// Derives the identity for this process: the host-derived process id plus a fresh activation id.
    ///
    /// The process id is stable for the lifetime of the process (see [`node_instance_name`]); the
    /// capture activation id is fresh for every call, matching `CaptureProxy.java:376-378`, where one
    /// activation is one run of the capture pipeline.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            process_id: node_instance_name().to_owned(),
            capture_activation_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// The process id: `${HOSTNAME:-generated_}_<per-process UUID>`.
    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.process_id
    }

    /// The capture activation id: one UUID per run of the capture pipeline.
    #[must_use]
    pub fn capture_activation_id(&self) -> &str {
        &self.capture_activation_id
    }

    /// The identity as metric attributes, omitting blank values.
    fn attributes(&self) -> Vec<KeyValue> {
        let mut attributes = Vec::with_capacity(2);
        if !self.process_id.trim().is_empty() {
            attributes.push(KeyValue::new("processId", self.process_id.clone()));
        }
        if !self.capture_activation_id.trim().is_empty() {
            attributes.push(KeyValue::new(
                "captureActivationId",
                self.capture_activation_id.clone(),
            ));
        }
        attributes
    }
}

/// This process's node instance name, computed once.
///
/// `ProcessHelpers.java:15-21`: `${HOSTNAME}` when the environment variable is set, otherwise the
/// literal `generated_`, then `_`, then one UUID generated once per process. When `HOSTNAME` is
/// unset the result therefore contains a double underscore (`generated__<uuid>`) — that is Java's
/// behaviour, and it is preserved rather than tidied because the string appears in
/// `service.instance.id`, which Prometheus exposes as `exported_instance` and the Grafana per-pod
/// skew panel groups by.
#[must_use]
pub fn node_instance_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let base = std::env::var("HOSTNAME").unwrap_or_else(|_| "generated_".to_owned());
        format!("{base}_{}", uuid::Uuid::new_v4())
    })
}

/// The three capture-process instruments plus the gap latch.
#[derive(Debug)]
pub struct CaptureProcessMetrics {
    pass_through_transitions: Counter<u64>,
    termination_transitions: Counter<u64>,
    capture_gap_active: UpDownCounter<i64>,
    process_attributes: Vec<KeyValue>,
    capture_gap_attributes: Vec<KeyValue>,
    gap_active: Mutex<bool>,
}

impl CaptureProcessMetrics {
    /// Registers the instruments and pins the identity attributes.
    #[must_use]
    pub fn new(meter: &Meter, identity: &ProcessIdentity) -> Self {
        let process_attributes = identity.attributes();
        let mut capture_gap_attributes = process_attributes.clone();
        capture_gap_attributes.push(KeyValue::new("mode", OPERATING_MODES[0]));
        capture_gap_attributes.push(KeyValue::new(
            "sourceForwardingContinued",
            SOURCE_FORWARDING_CONTINUED[0] == "true",
        ));
        Self {
            pass_through_transitions: instruments::counter(
                meter,
                "captureProcessPassThroughTransitions",
            ),
            termination_transitions: instruments::counter(
                meter,
                "captureProcessTerminationTransitions",
            ),
            capture_gap_active: instruments::up_down_counter(meter, "captureGapActive"),
            process_attributes,
            capture_gap_attributes,
            gap_active: Mutex::new(false),
        }
    }

    /// Records one irreversible capture-process transition.
    ///
    /// `PassThrough` increments the pass-through counter every time and raises the gap gauge only on
    /// the first such transition. `Terminating` increments the termination counter and lowers the
    /// gauge only if this process had raised it, so a fail-closed process that never entered
    /// pass-through does not drive the gauge negative.
    ///
    /// Call this **before** running termination listeners, as
    /// `CaptureProcessState.java:124-127` does: with a one-second export interval and a bounded
    /// log-flush window before the process exits, the terminal point only escapes if it is recorded
    /// first.
    ///
    /// # Errors
    ///
    /// Returns [`NotATransition`] for [`CaptureProcessState::Capture`].
    ///
    /// # Panics
    ///
    /// Panics if the gap latch mutex was poisoned by a previous panic inside this method. Internal
    /// ownership corruption must terminate the process rather than be papered over.
    pub fn record_transition(&self, state: CaptureProcessState) -> Result<(), NotATransition> {
        match state {
            CaptureProcessState::Capture => Err(NotATransition),
            CaptureProcessState::PassThrough => {
                let mut gap_active = self.gap_active.lock().expect("capture gap latch poisoned");
                self.pass_through_transitions
                    .add(1, &self.capture_gap_attributes);
                if !*gap_active {
                    self.capture_gap_active.add(1, &self.capture_gap_attributes);
                    *gap_active = true;
                }
                Ok(())
            }
            CaptureProcessState::Terminating => {
                let mut gap_active = self.gap_active.lock().expect("capture gap latch poisoned");
                self.termination_transitions
                    .add(1, &self.process_attributes);
                if *gap_active {
                    self.capture_gap_active
                        .add(-1, &self.capture_gap_attributes);
                    *gap_active = false;
                }
                Ok(())
            }
        }
    }

    /// Whether this process is currently counted as an active capture gap.
    ///
    /// # Panics
    ///
    /// Panics if the gap latch mutex was poisoned.
    #[must_use]
    pub fn gap_active(&self) -> bool {
        *self.gap_active.lock().expect("capture gap latch poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SCOPE_NAME;
    use crate::snapshot::recording_meter_provider;
    use opentelemetry::metrics::MeterProvider;

    fn fixture() -> (
        opentelemetry_sdk::metrics::SdkMeterProvider,
        crate::snapshot::MetricRecorder,
        CaptureProcessMetrics,
    ) {
        let (provider, recorder) = recording_meter_provider("process-a");
        let meter = provider.meter(SCOPE_NAME);
        let metrics =
            CaptureProcessMetrics::new(&meter, &ProcessIdentity::new("process-a", "activation-a"));
        (provider, recorder, metrics)
    }

    #[test]
    fn capture_is_rejected_as_a_transition() {
        let (_provider, _recorder, metrics) = fixture();
        assert_eq!(
            metrics.record_transition(CaptureProcessState::Capture),
            Err(NotATransition)
        );
    }

    /// Port of `CaptureProcessMetricsTest.failOpenCaptureGapRemainsActiveAcrossRepeatedMetricCollection`.
    #[test]
    fn fail_open_capture_gap_remains_active_across_repeated_metric_collection() {
        let (provider, recorder, metrics) = fixture();
        metrics
            .record_transition(CaptureProcessState::PassThrough)
            .expect("pass-through is a transition");
        metrics
            .record_transition(CaptureProcessState::PassThrough)
            .expect("pass-through is a transition");

        provider.force_flush().expect("flush");
        let first = recorder.take();
        assert_eq!(first.points_for("captureGapActive").len(), 1);
        assert_eq!(
            first
                .points_for("captureProcessPassThroughTransitions")
                .len(),
            1
        );

        provider.force_flush().expect("second flush");
        let second = recorder.take();
        assert_eq!(
            second.points_for("captureGapActive").len(),
            1,
            "the gap must still be reported on a later collection"
        );
        assert!(metrics.gap_active());
    }

    /// Port of the fail-open-then-unstable case: the gap closes and termination is counted once.
    #[test]
    fn terminating_after_pass_through_closes_the_gap() {
        let (provider, recorder, metrics) = fixture();
        metrics
            .record_transition(CaptureProcessState::PassThrough)
            .expect("transition");
        metrics
            .record_transition(CaptureProcessState::Terminating)
            .expect("transition");
        assert!(!metrics.gap_active());

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        let termination = snapshot
            .point_for("captureProcessTerminationTransitions")
            .expect("termination counter exported");
        assert_eq!(termination.attributes.len(), 2, "identity only, no mode");
        assert!(termination.attributes.contains_key("processId"));
        assert!(termination.attributes.contains_key("captureActivationId"));
    }

    #[test]
    fn terminating_without_a_gap_does_not_drive_the_gauge_negative() {
        let (_provider, _recorder, metrics) = fixture();
        metrics
            .record_transition(CaptureProcessState::Terminating)
            .expect("transition");
        assert!(!metrics.gap_active());
    }

    /// Port of the attribute assertions on the single `captureGapActive` point.
    #[test]
    fn capture_gap_point_carries_the_four_declared_attributes() {
        let (provider, recorder, metrics) = fixture();
        metrics
            .record_transition(CaptureProcessState::PassThrough)
            .expect("transition");
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        let point = snapshot
            .point_for("captureGapActive")
            .expect("captureGapActive exported");
        assert_eq!(
            point.attributes.get("processId").map(String::as_str),
            Some("process-a")
        );
        assert_eq!(
            point
                .attributes
                .get("captureActivationId")
                .map(String::as_str),
            Some("activation-a")
        );
        assert_eq!(
            point.attributes.get("mode").map(String::as_str),
            Some("fail-open")
        );
        assert_eq!(
            point
                .attributes
                .get("sourceForwardingContinued")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(point.unit, "processes");
    }

    #[test]
    fn blank_identity_values_are_omitted() {
        let (provider, recorder) = recording_meter_provider("blank");
        let meter = provider.meter(SCOPE_NAME);
        let metrics = CaptureProcessMetrics::new(&meter, &ProcessIdentity::new("", "   "));
        metrics
            .record_transition(CaptureProcessState::PassThrough)
            .expect("transition");
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        let point = snapshot.point_for("captureGapActive").expect("exported");
        assert!(!point.attributes.contains_key("processId"));
        assert!(!point.attributes.contains_key("captureActivationId"));
        assert_eq!(
            point.attributes.len(),
            2,
            "mode and sourceForwardingContinued only"
        );
    }

    #[test]
    fn node_instance_name_is_stable_and_shaped_like_java() {
        let first = node_instance_name();
        assert_eq!(first, node_instance_name(), "one name per process");
        let (base, uuid) = first.rsplit_once('_').expect("the name is <base>_<uuid>");
        assert!(!base.is_empty());
        assert_eq!(uuid.len(), 36, "a hyphenated UUID");
    }
}
