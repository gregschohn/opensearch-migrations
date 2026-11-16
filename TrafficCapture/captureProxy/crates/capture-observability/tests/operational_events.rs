//! The operational event contract, asserted on rendered output.
//!
//! These tests capture what a `tracing-subscriber` actually writes and check it against the event
//! contract. They assert three things:
//!
//! 1. The two summary lines are **field-for-field** what Java emits, including the exact string
//!    `CaptureProxySetupTest` asserts byte-for-byte.
//! 2. Every required `capture_*` event renders with the literal `event=capture_` substring that
//!    `deployment/k8s/scripts/capture-replay-gameday.sh:63-65` greps for, and with its correlation
//!    fields.
//! 3. The unauthenticated-client deduper keeps its diagnostic visible while bounding its volume.
//!
//! The rendered *layout* is not the contract — the layout may change, and
//! `capture_observability::logging` can emit JSON instead. The event names, the `event=capture_`
//! substring, and the correlation fields are.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture_observability::capture_event;
use capture_observability::connection_metrics::CloseReason;
use capture_observability::events::{
    self, CaptureMode, FailurePolicyLabel, ShutdownOutcome, StartupSummary,
};
use capture_observability::log_deduper::{KnownEvent, LogDeduper};
use capture_observability::process_metrics::ProcessIdentity;
use tracing_subscriber::fmt::MakeWriter;

/// Collects rendered log output in memory.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("buffer poisoned");
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `emit` under a local subscriber and returns what it rendered.
fn captured(emit: impl FnOnce()) -> String {
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    buffer.text()
}

fn identity() -> ProcessIdentity {
    ProcessIdentity::new("process-1", "activation-1")
}

/// The string `CaptureProxySetupTest.startupSummaryStatesCaptureModeAndTelemetryExportConfiguration`
/// asserts (`CaptureProxySetupTest.java:36-58`), reproduced verbatim.
const JAVA_STARTUP_LINE: &str = "event=capture_startup_summary processId=process-1 \
     captureActivationId=activation-1 listenPort=8080 captureMode=kafka \
     kafkaTopic=logging-traffic-topic failurePolicy=fail_open traceExportEnabled=false \
     metricExportEnabled=true";

#[test]
fn the_startup_summary_is_field_for_field_the_java_line() {
    let identity = identity();
    let output = captured(|| {
        events::log_startup_summary(&StartupSummary {
            identity: &identity,
            listen_port: 8080,
            capture_mode: CaptureMode::Kafka,
            kafka_topic: Some("logging-traffic-topic"),
            failure_policy: FailurePolicyLabel::FailOpen,
            trace_export_enabled: false,
            metric_export_enabled: true,
        });
    });
    assert!(
        output.contains(JAVA_STARTUP_LINE),
        "startup summary drifted from the Java contract.\n  expected substring: {JAVA_STARTUP_LINE}\n  rendered: {output}"
    );
    assert!(
        output.contains("INFO"),
        "the summary is an INFO line: {output}"
    );
}

#[test]
fn a_disabled_capture_reports_no_topic() {
    let identity = identity();
    let output = captured(|| {
        events::log_startup_summary(&StartupSummary {
            identity: &identity,
            listen_port: 9200,
            capture_mode: CaptureMode::Disabled,
            kafka_topic: None,
            failure_policy: FailurePolicyLabel::FailClosed,
            trace_export_enabled: true,
            metric_export_enabled: true,
        });
    });
    assert!(
        output.contains("captureMode=disabled kafkaTopic=none failurePolicy=fail_closed"),
        "{output}"
    );
}

#[test]
fn every_shutdown_outcome_renders_with_both_correlation_keys() {
    for outcome in ShutdownOutcome::all() {
        let identity = identity();
        let output = captured(|| events::log_shutdown_summary(*outcome, &identity));
        let expected = format!(
            "event=capture_shutdown_summary outcome={} processId=process-1 \
             captureActivationId=activation-1",
            outcome.as_log_value()
        );
        assert!(output.contains(&expected), "{output}");
    }
}

#[test]
fn the_summary_lines_carry_the_metric_bridge_fields() {
    // processId and captureActivationId are the only bridge from a log line to a captureGapActive
    // metric point, which carries the same two attribute keys.
    let identity = identity();
    let output = captured(|| {
        events::log_startup_summary(&StartupSummary {
            identity: &identity,
            listen_port: 8080,
            capture_mode: CaptureMode::Kafka,
            kafka_topic: Some("t"),
            failure_policy: FailurePolicyLabel::FailOpen,
            trace_export_enabled: false,
            metric_export_enabled: true,
        });
        events::log_shutdown_summary(ShutdownOutcome::Completed, &identity);
    });
    for line in output
        .lines()
        .filter(|line| line.contains("event=capture_"))
    {
        assert!(
            line.contains("processId=process-1")
                && line.contains("captureActivationId=activation-1"),
            "a summary line without both correlation keys: {line}"
        );
    }
}

#[test]
fn the_shutdown_failure_events_render_with_identity_and_detail() {
    let identity = identity();
    let output = captured(|| {
        events::log_shutdown_interrupted(&identity, &"interrupted while draining");
        events::log_shutdown_failed(&identity, &"cleanup failed");
    });
    assert!(
        output.contains(
            "event=capture_shutdown_interrupted processId=process-1 \
             captureActivationId=activation-1 detail=interrupted while draining"
        ),
        "{output}"
    );
    assert!(
        output.contains(
            "event=capture_shutdown_failed processId=process-1 \
             captureActivationId=activation-1 detail=cleanup failed"
        ),
        "{output}"
    );
    assert_eq!(
        output.matches("ERROR").count(),
        2,
        "both shutdown failures are ERROR lines: {output}"
    );
}

#[test]
fn a_dropped_connection_logs_the_identity_the_metric_omits() {
    let output = captured(|| {
        for reason in CloseReason::all() {
            events::log_connection_dropped("ch-42", *reason);
        }
    });
    for reason in CloseReason::all() {
        let expected = format!(
            "event=capture_connection_dropped connection=ch-42 reason={}",
            reason.as_label()
        );
        assert!(output.contains(&expected), "{output}");
    }
}

/// Every event in the required set must be emittable through the facade and visible to the gameday
/// log grep. The fields below are the runbook's correlation keys for each event's domain.
#[test]
fn every_required_event_renders_with_the_grepped_prefix() {
    let output = captured(|| {
        capture_event!(info, events::CAPTURE_STARTUP_SUMMARY, processId = "p");
        capture_event!(info, events::CAPTURE_SHUTDOWN_SUMMARY, processId = "p");
        capture_event!(error, events::CAPTURE_SHUTDOWN_INTERRUPTED, processId = "p");
        capture_event!(error, events::CAPTURE_SHUTDOWN_FAILED, processId = "p");
        capture_event!(
            warn,
            events::CAPTURE_CONNECTION_DROPPED,
            connection = "c-1",
            reason = "normal"
        );
        capture_event!(
            info,
            events::CAPTURE_ASSIGNMENT_RECEIVED,
            topic = "t",
            partition = 0,
            generation = 1
        );
        capture_event!(
            warn,
            events::CAPTURE_MEMBERSHIP_DEGRADED,
            detail = "rebalance"
        );
        capture_event!(error, events::CAPTURE_MEMBERSHIP_FAILED, detail = "fenced");
        capture_event!(info, events::CAPTURE_GENERATION_PREPARED, generation = 2);
        capture_event!(
            info,
            events::CAPTURE_WRITER_DRAINING,
            writer = "w-0",
            generation = 1
        );
        capture_event!(info, events::CAPTURE_GENERATION_ACTIVATED, generation = 2);
        capture_event!(
            debug,
            events::CAPTURE_ROUTE_SELECTED,
            connection = "c-1",
            writer = "w-0",
            partition = 3
        );
        capture_event!(
            warn,
            events::CAPTURE_HEARTBEAT_LATE,
            writer = "w-0",
            lateMillis = 1200
        );
        capture_event!(info, events::CAPTURE_WRITER_RETIRED, writer = "w-0");
        capture_event!(
            error,
            events::CAPTURE_HEALTH_REPORT_FAILED,
            detail = "timeout"
        );
        capture_event!(info, events::CAPTURE_HEALTH, writer = "w-0", lagMillis = 5);
        capture_event!(
            warn,
            events::CAPTURE_WRITE_GATE_CHANGED,
            reason = "kafka_send"
        );
        capture_event!(
            error,
            events::CAPTURE_PUBLISHER_FAILED,
            reason = "kafka_send"
        );
        capture_event!(
            error,
            events::CAPTURE_BROKER_TIME_VIOLATION,
            topic = "t",
            partition = 0
        );
    });

    for event in events::REQUIRED_EVENTS {
        let token = format!("event={event}");
        assert!(
            output.contains(&token),
            "{event} was not emitted; the gameday script would never see it"
        );
    }

    // Simulate `grep -E 'event=capture_'`: every rendered event line must match.
    let event_lines: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("event="))
        .collect();
    assert_eq!(
        event_lines.len(),
        events::REQUIRED_EVENTS.len(),
        "one line per event: {output}"
    );
    for line in event_lines {
        assert!(
            line.contains("event=capture_"),
            "a line the gameday grep would miss: {line}"
        );
    }
}

#[test]
fn the_unauthenticated_client_diagnostic_stays_visible_but_bounded() {
    let deduper = LogDeduper::new(Duration::from_mins(1));
    let output = captured(|| {
        for _ in 0..5_000 {
            deduper.record_and_log(
                KnownEvent::FrontsideTlsHandshakeFailure,
                "ch-7",
                &"handshake failure",
            );
        }
    });
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("event=capture_tls_handshake_failed"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "5000 handshake failures inside one window must produce one line, not {}",
        lines.len()
    );
    assert!(lines[0].contains("channel=ch-7"), "{}", lines[0]);
    assert!(
        lines[0].contains("suppressedSincePreviousLog=0"),
        "the first line reports nothing suppressed yet: {}",
        lines[0]
    );
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
}

#[test]
fn a_suppressed_burst_is_reported_on_the_next_emitted_line() {
    let deduper = LogDeduper::new(Duration::from_nanos(1));
    let output = captured(|| {
        for _ in 0..3 {
            deduper.record_and_log(
                KnownEvent::FrontsideTlsHandshakeFailure,
                "ch-8",
                &"handshake failure",
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("event=capture_tls_handshake_failed"))
        .collect();
    assert_eq!(
        lines.len(),
        3,
        "a 1 ns window logs every occurrence: {output}"
    );
}

#[test]
fn the_deduper_event_is_a_capture_event() {
    assert_eq!(
        KnownEvent::FrontsideTlsHandshakeFailure.event_name(),
        events::CAPTURE_TLS_HANDSHAKE_FAILED
    );
    assert!(events::CAPTURE_TLS_HANDSHAKE_FAILED.starts_with("capture_"));
}
