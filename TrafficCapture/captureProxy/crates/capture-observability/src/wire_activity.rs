//! Typed request-phase observation handles (HTTP-12, HTTP-14).
//!
//! Java models the wire-capture phases as an interface hierarchy — `ICapturingConnectionContext`,
//! `IHttpMessageContext`, and four phase interfaces — where each phase inherits its instrument
//! family from `CommonScopedMetricInstruments` and its span from `BaseNestedSpanContext`. The
//! phase *semantics* are the contract; the type hierarchy is not. This module keeps the semantics
//! and drops the hierarchy: one instrument set, four distinct handle types, and transitions that
//! consume the handle they leave.
//!
//! # The phase machine
//!
//! ```text
//! RequestActivity ──block()──────────────► BlockingActivity
//!        │                                        │
//!        ├──await_response()──► WaitingForResponseActivity
//!        │                                        │
//!        └──gather_response()──────────────► ResponseActivity ──next_request()──► RequestActivity
//! ```
//!
//! Each handle records `<phase>Count` and `<phase>Duration` **exactly once**: either when a
//! transition consumes it, or on drop if the connection failed mid-phase. That is the invariant
//! `RootWireLoggingContextTest.java:112-118` asserts — one duration observation per phase per
//! request — and making it structural means an error path cannot silently skip it or double-count
//! it.
//!
//! # This is the hot path, and it is allocation-free
//!
//! Telemetry was **49.9% of Java-thread CPU** in the capture proxy and 21% of its allocation, so
//! the per-request cost here is the single largest performance lever in the rewrite. The design
//! consequences are deliberate and must not be regressed:
//!
//! - **A phase handle borrows its [`CapturingConnection`]** rather than holding a reference count.
//!   One `Arc` clone happens per *connection*; none happens per request or per phase transition.
//! - **Phase timing is a plain [`Instant`] in the handle**, which lives on the caller's stack. There
//!   is no process-wide registry of live activities, so starting and finishing a phase touches no
//!   shared container.
//! - **Every instrument here is dimensionless**, which puts each observation on the SDK's
//!   empty-attribute fast path: one atomic update with no hashing, no lock acquisition, and no
//!   allocation (`opentelemetry_sdk-0.31.0/src/metrics/internal/mod.rs:103-107`). An attributed
//!   observation would instead take a read lock and hash the attribute set on every request.
//! - **Instrument handles are resolved once**, in [`WireInstruments::new`] at start-up. Nothing on
//!   the data path looks an instrument up by name.
//! - **Spans are created at `debug`**, so when no trace collector is configured and the console is
//!   at its default `info`, the span macro is a disabled-callsite check and the field expressions
//!   are never evaluated. Span creation is opt-in; the metrics are not.
//!
//! # Where identity lives
//!
//! `connection` and `request` go on the **span**, never on a metric point. Following one request
//! therefore means grepping the logs by `connection=` and `request=`, exactly as the runbook
//! prescribes (`observabilityOperations.md:291-293`), and no amount of traffic can grow the series
//! count.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use opentelemetry::metrics::{Counter, Meter};
use tracing::{Span, debug_span};

use crate::activity::ActivityInstruments;
use crate::contract::{self, Activity};
use crate::instruments;

/// Every instrument the wire-capture phases record.
///
/// Built once at start-up and shared by every connection.
#[derive(Debug)]
pub struct WireInstruments {
    gathering_request: ActivityInstruments,
    blocked: ActivityInstruments,
    waiting_for_response: ActivityInstruments,
    gathering_response: ActivityInstruments,
    unregistered: Counter<u64>,
    removed: Counter<u64>,
    blocking_request: Counter<u64>,
    capture_suppressed: Counter<u64>,
    full_request: Counter<u64>,
    bytes_read: Counter<u64>,
    bytes_written: Counter<u64>,
}

fn activity(name: &str) -> &'static Activity {
    contract::ACTIVITIES
        .iter()
        .find(|activity| activity.name == name)
        .unwrap_or_else(|| panic!("activity {name} is not declared in the metric contract"))
}

impl WireInstruments {
    /// Registers the four phase families and the seven wire counters.
    ///
    /// Call once per process. This is the only place an instrument is resolved by name.
    ///
    /// # Panics
    ///
    /// Panics when any name is absent from the metric contract.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            gathering_request: ActivityInstruments::new(meter, activity("gatheringRequest")),
            blocked: ActivityInstruments::new(meter, activity("blocked")),
            waiting_for_response: ActivityInstruments::new(meter, activity("waitingForResponse")),
            gathering_response: ActivityInstruments::new(meter, activity("gatheringResponse")),
            unregistered: instruments::counter(meter, "unregistered"),
            removed: instruments::counter(meter, "removed"),
            blocking_request: instruments::counter(meter, "blockingRequest"),
            capture_suppressed: instruments::counter(meter, "captureSuppressed"),
            full_request: instruments::counter(meter, "fullRequest"),
            bytes_read: instruments::counter(meter, "bytesRead"),
            bytes_written: instruments::counter(meter, "bytesWritten"),
        }
    }
}

/// The request/response phase a connection is in, for [`WirePhaseTracker`].
#[derive(Debug, Clone, Copy)]
enum WirePhase {
    Idle,
    GatheringRequest(Instant),
    Blocked(Instant),
    WaitingForResponse(Instant),
    GatheringResponse(Instant),
}

/// An owned, callback-driven driver for the wire-phase instruments.
///
/// [`CapturingConnection`]'s phase handles borrow the connection, which suits code that owns a
/// request's whole lifetime on one stack frame. The data plane instead observes a connection
/// through callbacks on a long-lived per-connection object, which cannot hold a borrow of itself.
/// This tracker drives the **same instruments** with the same phase semantics as Java's
/// `WireCaptureContexts`: gathering the request from its first byte to its end; blocked while the
/// capture-before-forward gate holds it; waiting for the response until the first response byte;
/// gathering the response until the next request begins or the connection closes.
///
/// Every operation is O(1) on connection-local state; nothing here is shared or ordered, and the
/// instruments are dimensionless, so each record lands on the SDK's no-attribute fast path.
#[derive(Debug)]
pub struct WirePhaseTracker {
    instruments: Arc<WireInstruments>,
    phase: WirePhase,
}

impl WirePhaseTracker {
    /// Starts tracking one connection.
    #[must_use]
    pub fn new(instruments: Arc<WireInstruments>) -> Self {
        Self {
            instruments,
            phase: WirePhase::Idle,
        }
    }

    fn finish_current(&mut self) {
        let instruments = &self.instruments;
        match self.phase {
            WirePhase::Idle => {}
            WirePhase::GatheringRequest(started) => {
                instruments
                    .gathering_request
                    .record_completed(started.elapsed());
            }
            WirePhase::Blocked(started) => {
                instruments.blocked.record_completed(started.elapsed());
            }
            WirePhase::WaitingForResponse(started) => {
                instruments
                    .waiting_for_response
                    .record_completed(started.elapsed());
            }
            WirePhase::GatheringResponse(started) => {
                instruments
                    .gathering_response
                    .record_completed(started.elapsed());
            }
        }
        self.phase = WirePhase::Idle;
    }

    /// Client bytes were read. Counted whether or not they are captured (`LHH` counts every read).
    pub fn client_bytes(&mut self, bytes: u64) {
        self.instruments.bytes_read.add(bytes, &[]);
        match self.phase {
            WirePhase::GatheringRequest(_) | WirePhase::Blocked(_) => {}
            WirePhase::Idle
            | WirePhase::WaitingForResponse(_)
            | WirePhase::GatheringResponse(_) => {
                self.finish_current();
                self.phase = WirePhase::GatheringRequest(Instant::now());
            }
        }
    }

    /// One request was completely framed.
    pub fn request_complete(&mut self) {
        self.instruments.full_request.add(1, &[]);
        if let WirePhase::GatheringRequest(_) = self.phase {
            self.finish_current();
            self.phase = WirePhase::WaitingForResponse(Instant::now());
        }
    }

    /// The capture-before-forward gate began holding the request.
    pub fn blocking_started(&mut self) {
        self.instruments.blocking_request.add(1, &[]);
        // The waiting phase opened at the end of the request has not really begun: the request
        // is held. Java goes straight from gathering to blocked.
        self.phase = WirePhase::Blocked(Instant::now());
    }

    /// The gate released (or refused) the request.
    pub fn blocking_finished(&mut self) {
        if let WirePhase::Blocked(_) = self.phase {
            self.finish_current();
            self.phase = WirePhase::WaitingForResponse(Instant::now());
        }
    }

    /// Response bytes were handed to the client. Counted whether or not they are captured.
    pub fn source_bytes(&mut self, bytes: u64) {
        self.instruments.bytes_written.add(bytes, &[]);
        if let WirePhase::WaitingForResponse(_) = self.phase {
            self.finish_current();
            self.phase = WirePhase::GatheringResponse(Instant::now());
        }
    }

    /// Capture was suppressed for a request by policy.
    pub fn capture_suppressed(&mut self) {
        self.instruments.capture_suppressed.add(1, &[]);
    }

    /// An exception was observed in the current phase.
    pub fn exception(&mut self) {
        let instruments = &self.instruments;
        match self.phase {
            WirePhase::Idle => {}
            WirePhase::GatheringRequest(_) => instruments.gathering_request.record_exception(),
            WirePhase::Blocked(_) => instruments.blocked.record_exception(),
            WirePhase::WaitingForResponse(_) => instruments.waiting_for_response.record_exception(),
            WirePhase::GatheringResponse(_) => instruments.gathering_response.record_exception(),
        }
    }

    /// The connection closed: completes the open phase and records the handler-lifecycle counters
    /// Java records once per channel teardown.
    pub fn closed(&mut self) {
        self.finish_current();
        self.instruments.unregistered.add(1, &[]);
        self.instruments.removed.add(1, &[]);
    }
}

/// Per-connection wire-capture handle.
///
/// Replaces `ICapturingConnectionContext`: it owns the connection's identity for span correlation,
/// hands out request-phase handles that borrow from it, and records the two handler-lifecycle
/// counters. One of these exists per source connection and is the only reference-counted thing on
/// the request path.
#[derive(Debug)]
pub struct CapturingConnection {
    instruments: Arc<WireInstruments>,
    connection_id: Arc<str>,
    next_request_index: AtomicU64,
}

impl CapturingConnection {
    /// Starts observing one source connection.
    #[must_use]
    pub fn new(instruments: Arc<WireInstruments>, connection_id: impl Into<Arc<str>>) -> Self {
        Self {
            instruments,
            connection_id: connection_id.into(),
            next_request_index: AtomicU64::new(0),
        }
    }

    /// The connection identity used for log and span correlation.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Begins the request-gathering phase for the next request on this connection.
    ///
    /// Takes `&self` so a phase handle can be alive while the connection's own counters are
    /// recorded, and so the read and write halves of a connection can both hold handles. The index
    /// increments per request, matching Java's `sourceRequestIndex`
    /// (`WireCaptureContexts.java:69`); it is a span field, not a metric dimension.
    #[must_use]
    pub fn begin_request(&self) -> RequestActivity<'_> {
        let index = self.next_request_index.fetch_add(1, Ordering::Relaxed);
        RequestActivity::start(self, index)
    }

    /// The capture handlers were unregistered from the channel.
    pub fn record_unregistered(&self) {
        self.instruments.unregistered.add(1, &[]);
    }

    /// The capture handlers were removed from the channel pipeline.
    pub fn record_removed(&self) {
        self.instruments.removed.add(1, &[]);
    }
}

/// The state every phase handle carries. Entirely on the caller's stack.
#[derive(Debug)]
struct PhaseState<'c> {
    connection: &'c CapturingConnection,
    request_index: u64,
    started: Instant,
    span: Span,
}

impl<'c> PhaseState<'c> {
    fn complete(self, family: fn(&WireInstruments) -> &ActivityInstruments) -> PhaseCarry<'c> {
        let elapsed = self.started.elapsed();
        family(&self.connection.instruments).record_completed(elapsed);
        drop(self.span);
        PhaseCarry {
            connection: self.connection,
            request_index: self.request_index,
        }
    }
}

/// What a finished phase hands to the next phase: a borrow and an index, both `Copy`.
#[derive(Debug, Clone, Copy)]
struct PhaseCarry<'c> {
    connection: &'c CapturingConnection,
    request_index: u64,
}

/// Declares one phase handle: its instrument family, its span, and its single completion point.
macro_rules! phase_handle {
    (
        $(#[$meta:meta])*
        $name:ident, $family:ident, $span_name:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub struct $name<'c> {
            state: Option<PhaseState<'c>>,
        }

        impl<'c> $name<'c> {
            fn start(connection: &'c CapturingConnection, request_index: u64) -> Self {
                // Disabled at the default console level with no trace collector, in which case the
                // field expressions below are never evaluated.
                let span = debug_span!(
                    $span_name,
                    connection = %connection.connection_id,
                    request = request_index
                );
                Self {
                    state: Some(PhaseState {
                        connection,
                        request_index,
                        started: Instant::now(),
                        span,
                    }),
                }
            }

            /// The phase's span, for attaching additional correlation fields.
            #[must_use]
            pub fn span(&self) -> Option<&Span> {
                self.state.as_ref().map(|state| &state.span)
            }

            /// How long this phase has been running.
            #[must_use]
            pub fn elapsed(&self) -> Option<std::time::Duration> {
                self.state.as_ref().map(|state| state.started.elapsed())
            }

            /// An exception was observed during this phase.
            ///
            /// Increments `<phase>ExceptionCount`. The phase may still complete afterwards, exactly
            /// as in Java, where the exception counter and the scope counter are independent.
            pub fn record_exception(&self) {
                if let Some(state) = &self.state {
                    state.connection.instruments.$family.record_exception();
                }
            }

            /// Records this phase's completion and returns what the next phase needs.
            fn finish_phase(&mut self) -> Option<PhaseCarry<'c>> {
                let state = self.state.take()?;
                Some(state.complete(|wire| &wire.$family))
            }
        }

        // DEVIATION: Java relies on the caller closing each context explicitly, so an exception
        // path can skip the observation and leave a phase unaccounted for. Closing on drop makes
        // "exactly one observation per phase per request" structural instead of conventional.
        impl Drop for $name<'_> {
            fn drop(&mut self) {
                if let Some(state) = self.state.take() {
                    // The carried state is discarded: an abandoned phase has no successor.
                    let _carry = state.complete(|wire| &wire.$family);
                }
            }
        }
    };
}

phase_handle!(
    /// The request-gathering phase: bytes arriving from the source client.
    ///
    /// Replaces `WireCaptureContexts.RequestContext`. Owns the four request-scoped counters.
    RequestActivity,
    gathering_request,
    "gatheringRequest"
);

phase_handle!(
    /// The blocking phase: the request is held at the pre-forward capture gate.
    ///
    /// Replaces `WireCaptureContexts.BlockingContext`. Part of proxy-added latency, so its duration
    /// histogram is bucketed for the performance gates.
    BlockingActivity,
    blocked,
    "blocked"
);

phase_handle!(
    /// The response-wait phase: the request is forwarded and the source has not answered yet.
    ///
    /// Replaces `WireCaptureContexts.WaitingForResponseContext`. Uses the long bucket set, because
    /// this measures the *source's* latency and a slow source legitimately takes minutes.
    WaitingForResponseActivity,
    waiting_for_response,
    "waitingForResponse"
);

phase_handle!(
    /// The response-gathering phase: bytes being relayed back to the source client.
    ///
    /// Replaces `WireCaptureContexts.ResponseContext`.
    ResponseActivity,
    gathering_response,
    "gatheringResponse"
);

/// Finishes the current phase and starts the named one from its carried state.
macro_rules! transition {
    ($handle:expr, $target:ident) => {{
        let carry = $handle
            .finish_phase()
            .expect("a phase handle is finished exactly once");
        $target::start(carry.connection, carry.request_index)
    }};
}

impl<'c> RequestActivity<'c> {
    /// Request bytes read from the source connection.
    ///
    /// Adds the byte count, not 1 (`WireCaptureContexts.java:173-175`).
    pub fn record_bytes_read(&self, bytes: u64) {
        if let Some(state) = &self.state {
            state.connection.instruments.bytes_read.add(bytes, &[]);
        }
    }

    /// The request will be held at the pre-forward capture gate.
    pub fn record_blocking_request(&self) {
        if let Some(state) = &self.state {
            state.connection.instruments.blocking_request.add(1, &[]);
        }
    }

    /// A suppression rule matched: the request is forwarded but not captured.
    pub fn record_capture_suppressed(&self) {
        if let Some(state) = &self.state {
            state.connection.instruments.capture_suppressed.add(1, &[]);
        }
    }

    /// The request was fully parsed.
    ///
    /// `fullRequest` is the proxy's request-rate signal on both shipped dashboards, so this must be
    /// recorded once per complete request and never on a partial one.
    pub fn record_fully_parsed_request(&self) {
        if let Some(state) = &self.state {
            state.connection.instruments.full_request.add(1, &[]);
        }
    }

    /// Ends request gathering and enters the blocking phase.
    ///
    /// # Panics
    ///
    /// Unreachable. A transition consumes its handle and a handle is finished exactly once; the
    /// internal expectation exists only because the completion state is an `Option`, which is what
    /// lets `Drop` close a phase abandoned by an error path.
    #[must_use]
    pub fn block(mut self) -> BlockingActivity<'c> {
        transition!(self, BlockingActivity)
    }

    /// Ends request gathering and enters the response-wait phase.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`Self::block`].
    #[must_use]
    pub fn await_response(mut self) -> WaitingForResponseActivity<'c> {
        transition!(self, WaitingForResponseActivity)
    }

    /// Ends request gathering and enters the response-gathering phase.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`Self::block`].
    #[must_use]
    pub fn gather_response(mut self) -> ResponseActivity<'c> {
        transition!(self, ResponseActivity)
    }
}

impl<'c> BlockingActivity<'c> {
    /// Ends blocking and enters the response-wait phase.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`RequestActivity::block`].
    #[must_use]
    pub fn await_response(mut self) -> WaitingForResponseActivity<'c> {
        transition!(self, WaitingForResponseActivity)
    }

    /// Ends blocking and enters the response-gathering phase.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`RequestActivity::block`].
    #[must_use]
    pub fn gather_response(mut self) -> ResponseActivity<'c> {
        transition!(self, ResponseActivity)
    }
}

impl<'c> WaitingForResponseActivity<'c> {
    /// The source answered: ends the wait and enters the response-gathering phase.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`RequestActivity::block`].
    #[must_use]
    pub fn gather_response(mut self) -> ResponseActivity<'c> {
        transition!(self, ResponseActivity)
    }
}

impl<'c> ResponseActivity<'c> {
    /// Response bytes written back to the source connection.
    ///
    /// Adds the byte count, not 1 (`WireCaptureContexts.java:274-276`).
    pub fn record_bytes_written(&self, bytes: u64) {
        if let Some(state) = &self.state {
            state.connection.instruments.bytes_written.add(bytes, &[]);
        }
    }

    /// Ends the response and begins the next request on the same connection.
    ///
    /// Mirrors `IHttpMessageContext.createNextRequestContext`: a pipelined connection continues
    /// with the next request index rather than starting a new connection scope.
    ///
    /// # Panics
    ///
    /// Unreachable; see [`RequestActivity::block`].
    #[must_use]
    pub fn next_request(mut self) -> RequestActivity<'c> {
        let carry = self
            .finish_phase()
            .expect("a phase handle is finished exactly once");
        RequestActivity::start(carry.connection, carry.request_index + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SCOPE_NAME;
    use crate::snapshot::{MetricSnapshot, recording_meter_provider};
    use opentelemetry::metrics::MeterProvider;

    fn wire() -> (
        opentelemetry_sdk::metrics::SdkMeterProvider,
        crate::snapshot::MetricRecorder,
        Arc<WireInstruments>,
    ) {
        let (provider, recorder) = recording_meter_provider("wire");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(WireInstruments::new(&meter));
        (provider, recorder, instruments)
    }

    fn durations(snapshot: &MetricSnapshot, phase: &str) -> usize {
        snapshot
            .points_for(&contract::activity_duration_name(phase))
            .len()
    }

    #[test]
    fn the_owned_tracker_records_a_blocked_mutation_lifecycle() {
        // The data plane drives the instruments through WirePhaseTracker callbacks; this is the
        // order the transport and the capture gate call them in for a Critical Mutation Traffic
        // request.
        let (provider, recorder, instruments) = wire();
        let mut tracker = WirePhaseTracker::new(instruments);
        tracker.client_bytes(64);
        tracker.request_complete();
        tracker.blocking_started();
        tracker.blocking_finished();
        tracker.source_bytes(256);
        tracker.closed();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        for phase in [
            "gatheringRequest",
            "blocked",
            "waitingForResponse",
            "gatheringResponse",
        ] {
            assert_eq!(durations(&snapshot, phase), 1, "{phase} must be recorded");
        }
        for counter in [
            "fullRequest",
            "blockingRequest",
            "bytesRead",
            "bytesWritten",
            "unregistered",
            "removed",
        ] {
            assert!(
                snapshot.point_for(counter).is_some(),
                "{counter} must be recorded"
            );
        }
    }

    #[test]
    fn the_owned_tracker_records_no_blocked_phase_for_an_ordinary_request() {
        let (provider, recorder, instruments) = wire();
        let mut tracker = WirePhaseTracker::new(instruments);
        tracker.client_bytes(32);
        tracker.request_complete();
        tracker.source_bytes(64);
        // A second request on the same connection closes the first response phase.
        tracker.client_bytes(32);
        tracker.closed();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(durations(&snapshot, "blocked"), 0);
        assert_eq!(durations(&snapshot, "gatheringRequest"), 1);
        assert_eq!(durations(&snapshot, "gatheringResponse"), 1);
        assert!(snapshot.point_for("blockingRequest").is_none());
    }

    #[test]
    fn a_request_with_a_wait_records_each_phase_once() {
        let (provider, recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-1");
        let request = connection.begin_request();
        request.record_bytes_read(128);
        request.record_fully_parsed_request();
        let waiting = request.await_response();
        let response = waiting.gather_response();
        response.record_bytes_written(512);
        drop(response);

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(durations(&snapshot, "gatheringRequest"), 1);
        assert_eq!(durations(&snapshot, "waitingForResponse"), 1);
        assert_eq!(durations(&snapshot, "gatheringResponse"), 1);
        assert_eq!(
            durations(&snapshot, "blocked"),
            0,
            "a request that never blocked must not record a blocked observation"
        );
        assert!(snapshot.point_for("fullRequest").is_some());
        assert!(snapshot.point_for("bytesRead").is_some());
        assert!(snapshot.point_for("bytesWritten").is_some());
    }

    #[test]
    fn a_blocked_request_records_the_blocked_phase() {
        let (provider, recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-2");
        let request = connection.begin_request();
        request.record_blocking_request();
        let blocking = request.block();
        drop(blocking.await_response());

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(durations(&snapshot, "gatheringRequest"), 1);
        assert_eq!(durations(&snapshot, "blocked"), 1);
        assert_eq!(durations(&snapshot, "waitingForResponse"), 1);
        assert!(snapshot.point_for("blockingRequest").is_some());
    }

    #[test]
    fn an_abandoned_phase_still_records_exactly_one_observation() {
        let (provider, recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-3");
        let request = connection.begin_request();
        request.record_exception();
        drop(request);

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(
            durations(&snapshot, "gatheringRequest"),
            1,
            "a dropped phase must still close its histogram observation"
        );
        assert!(
            snapshot
                .point_for("gatheringRequestExceptionCount")
                .is_some()
        );
    }

    #[test]
    fn pipelined_requests_advance_the_request_index_without_new_series() {
        let (provider, recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-4");
        let first = connection.begin_request();
        let response = first.gather_response();
        let second = response.next_request();
        drop(second.gather_response());

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(durations(&snapshot, "gatheringRequest"), 1);
        assert_eq!(durations(&snapshot, "gatheringResponse"), 1);
        for point in &snapshot.points {
            assert!(
                point.attributes.is_empty(),
                "{} gained dimension keys {:?}",
                point.name,
                point.attributes.keys().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn suppression_and_handler_lifecycle_counters_record() {
        let (provider, recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-5");
        connection.begin_request().record_capture_suppressed();
        connection.record_unregistered();
        connection.record_removed();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert!(snapshot.point_for("captureSuppressed").is_some());
        assert!(snapshot.point_for("unregistered").is_some());
        assert!(snapshot.point_for("removed").is_some());
    }

    /// A phase handle must stay a stack value. The 72 bytes are a connection borrow (8), the
    /// request index (8), an `Instant` (16), a `tracing::Span` handle (32), and the `Option`
    /// discriminant (8). Anything owned added here would be paid for on every request.
    #[test]
    fn a_phase_handle_is_a_small_stack_value() {
        let size = std::mem::size_of::<RequestActivity<'_>>();
        assert!(
            size <= 80,
            "a phase handle grew to {size} bytes; the request path must not carry owned state"
        );
    }

    /// The hand-off between phases must be a borrow and an index, nothing owned: this is what
    /// keeps a four-phase request free of reference-count traffic.
    #[test]
    fn the_phase_hand_off_carries_no_owned_state() {
        const fn assert_copy<T: Copy>() {}
        assert_copy::<PhaseCarry<'_>>();
        assert_eq!(
            std::mem::size_of::<PhaseCarry<'_>>(),
            std::mem::size_of::<&CapturingConnection>() + std::mem::size_of::<u64>(),
            "the phase hand-off gained a field"
        );
    }

    /// Concurrent connections must not contend on shared mutable state. This asserts correctness
    /// under concurrency; the absence of contention is structural — every instrument touched here
    /// is dimensionless, so each observation takes the SDK's lock-free empty-attribute path.
    #[test]
    fn concurrent_connections_record_independently() {
        let (provider, recorder, instruments) = wire();
        let threads: Vec<_> = (0..8)
            .map(|worker| {
                let instruments = Arc::clone(&instruments);
                std::thread::spawn(move || {
                    let connection = CapturingConnection::new(instruments, format!("c-{worker}"));
                    for _ in 0..100 {
                        let request = connection.begin_request();
                        request.record_fully_parsed_request();
                        request.record_bytes_read(10);
                        drop(request.gather_response());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker finished");
        }

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        // 8 workers x 100 requests, all on one dimensionless series each.
        assert_eq!(snapshot.points_for("fullRequest").len(), 1);
        assert_eq!(durations(&snapshot, "gatheringRequest"), 1);
        assert_eq!(durations(&snapshot, "gatheringResponse"), 1);
    }

    /// Each connection numbers its own requests, and the numbering never becomes a metric
    /// dimension.
    #[test]
    fn request_indices_are_per_connection() {
        let (_provider, _recorder, instruments) = wire();
        let connection = CapturingConnection::new(instruments, "c-6");
        for _ in 0..3 {
            drop(connection.begin_request());
        }
        assert_eq!(connection.next_request_index.load(Ordering::Relaxed), 3);
    }
}
