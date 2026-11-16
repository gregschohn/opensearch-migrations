//! Wire-phase telemetry around any capture sink (traceability rows HTTP-12, HTTP-14).
//!
//! Java records the request/blocked/waiting/response phase instruments, `fullRequest`,
//! `bytesRead`/`bytesWritten`, `blockingRequest`, and `captureSuppressed` for **all** traffic: its
//! capture handler is installed even under `--noCapture` (`ProxyChannelInitializer.java:159`) and
//! counts bytes regardless of suppression (`LoggingHttpHandler.java:905-911`). Several of these
//! series are externally consumed: the k6 Grafana dashboard queries `fullRequest_count_total` and
//! the `gatheringRequestDuration`/`gatheringResponseDuration` histograms, and the CDK dashboard and
//! integration tests read `bytesRead` and `fullRequest`.
//!
//! [`MeteredSink`] wraps either sink, drives a [`WirePhaseTracker`] from the transport's
//! unconditional `observe_*` hooks and from the capture gate, and delegates every capture call
//! unchanged. Wrapping rather than embedding keeps the no-capture and capture modes measuring the
//! same thing, which is also what makes a Java/Rust performance comparison fair: an unmetered Rust
//! arm would do less work per request than the Java arm it is compared against.

use std::future::Future;
use std::time::SystemTime;

use capture_observability::wire_activity::WirePhaseTracker;
use capture_transport::{CaptureAbort, CaptureSink, CloseReason, GateOutcome};

/// A capture sink with wire-phase telemetry.
#[derive(Debug)]
pub struct MeteredSink<K> {
    inner: K,
    phases: WirePhaseTracker,
}

impl<K: CaptureSink> MeteredSink<K> {
    /// Wraps `inner`, recording into `phases`.
    #[must_use]
    pub fn new(inner: K, phases: WirePhaseTracker) -> Self {
        Self { inner, phases }
    }

    /// The wrapped sink.
    pub fn inner_mut(&mut self) -> &mut K {
        &mut self.inner
    }
}

impl<K: CaptureSink> CaptureSink for MeteredSink<K> {
    fn should_capture(&self) -> bool {
        self.inner.should_capture()
    }

    fn capture_read(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
        self.inner.capture_read(at, bytes)
    }

    fn request_dropped(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        self.inner.request_dropped(at)
    }

    fn set_end_of_message_lengths(
        &mut self,
        first_line_byte_length: u32,
        headers_byte_length: u32,
    ) {
        self.inner
            .set_end_of_message_lengths(first_line_byte_length, headers_byte_length);
    }

    fn commit_end_of_message(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        self.inner.commit_end_of_message(at)
    }

    fn request_forwarded(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        self.inner.request_forwarded(at)
    }

    fn capture_write(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
        self.inner.capture_write(at, bytes)
    }

    fn capture_interim_response(
        &mut self,
        at: SystemTime,
        bytes: &[u8],
    ) -> Result<(), CaptureAbort> {
        self.inner.capture_interim_response(at, bytes)
    }

    fn connection_exception(&mut self, at: SystemTime, message: &str) -> Result<(), CaptureAbort> {
        self.inner.connection_exception(at, message)
    }

    fn close(&mut self, at: SystemTime, reason: CloseReason) -> Result<(), CaptureAbort> {
        self.inner.close(at, reason)
    }

    fn next_flush_deadline(&self) -> Option<std::time::Instant> {
        self.inner.next_flush_deadline()
    }

    fn run_scheduled_flush(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        self.inner.run_scheduled_flush(at)
    }

    fn await_terminal_acknowledgement(
        &mut self,
    ) -> impl Future<Output = Result<(), CaptureAbort>> + Send {
        self.inner.await_terminal_acknowledgement()
    }

    async fn gate_critical_mutation(&mut self) -> GateOutcome {
        // The transport calls the gate only for a captured Critical Mutation Traffic request, which
        // is exactly when Java opens its blocking context (`shouldCapture && shouldBlock`).
        self.phases.blocking_started();
        let outcome = self.inner.gate_critical_mutation().await;
        self.phases.blocking_finished();
        outcome
    }

    fn observe_client_bytes(&mut self, bytes: u64) {
        self.phases.client_bytes(bytes);
        self.inner.observe_client_bytes(bytes);
    }

    fn observe_request_complete(&mut self) {
        self.phases.request_complete();
        self.inner.observe_request_complete();
    }

    fn observe_capture_suppressed(&mut self) {
        self.phases.capture_suppressed();
        self.inner.observe_capture_suppressed();
    }

    fn observe_source_bytes(&mut self, bytes: u64) {
        self.phases.source_bytes(bytes);
        self.inner.observe_source_bytes(bytes);
    }

    fn observe_exception(&mut self) {
        self.phases.exception();
        self.inner.observe_exception();
    }

    fn observe_closed(&mut self) {
        self.phases.closed();
        self.inner.observe_closed();
    }
}
