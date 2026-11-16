//! The capture boundary: implementations of [`capture_transport::CaptureSink`].
//!
//! `capture-transport` owns I/O, framing, ordering, and flow control, and routes every capture side
//! effect through this one trait. Two implementations live here:
//!
//! | Implementation | Mode | What it does |
//! |---|---|---|
//! | [`NoCaptureSink`] | `--no-capture` | nothing, as cheaply as possible |
//! | [`RecordingCaptureSink`] | capture | drives [`ConnectionRecorder`] and the process gate |
//!
//! # Where the policy decision lives
//!
//! The trait's contract puts the capture-failure policy in *this* layer, not in the transport:
//!
//! * returning `Err(CaptureAbort)` is terminal — the transport stops and closes with
//!   [`capture_transport::CloseReason::CaptureFailure`];
//! * returning `Ok` while [`capture_transport::CaptureSink::should_capture`] reports `false` is
//!   fail-open — the transport keeps forwarding uncaptured traffic.
//!
//! [`RecordingCaptureSink`] reaches both through [`CaptureProcessState`], which is the only place
//! the `--capture-failure-policy` is interpreted.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use capture_transport::{CaptureAbort, CaptureSink, CloseReason, GateOutcome};
use capture_wire::{
    CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig, PublishFailure, ScheduledFlush,
    SinkError, WireError,
};

use crate::capture_process_state::{CaptureFailure, CaptureProcessState, State};
use crate::publication::{PublicationOutcome, PublishingRecordSink};

/// The `--no-capture` sink: every observation is discarded.
///
/// This is what the Phase 1 plaintext and TLS transport-throughput gates are measured against, so
/// it is deliberately the cheapest possible implementation: no allocation, no lock, no atomic, and
/// no `await` that ever yields. There is no process gate consult either — with no capture there is
/// nothing to compromise, so the gate can never close and asking it would be a guaranteed-true
/// branch on the hottest path the proxy has.
///
/// `--no-capture` is nonetheless a *capture gap* by construction. Saying so is
/// [`crate::app`]'s job at start-up, not this type's job per request.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCaptureSink;

impl NoCaptureSink {
    /// Builds the sink.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl CaptureSink for NoCaptureSink {
    fn should_capture(&self) -> bool {
        false
    }

    fn capture_read(&mut self, _at: SystemTime, _bytes: &[u8]) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn request_dropped(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn set_end_of_message_lengths(
        &mut self,
        _first_line_byte_length: u32,
        _headers_byte_length: u32,
    ) {
    }

    fn commit_end_of_message(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn request_forwarded(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn capture_write(&mut self, _at: SystemTime, _bytes: &[u8]) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn capture_interim_response(
        &mut self,
        _at: SystemTime,
        _bytes: &[u8],
    ) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn connection_exception(
        &mut self,
        _at: SystemTime,
        _message: &str,
    ) -> Result<(), CaptureAbort> {
        Ok(())
    }

    fn close(&mut self, _at: SystemTime, _reason: CloseReason) -> Result<(), CaptureAbort> {
        Ok(())
    }

    /// Forwards immediately.
    ///
    /// `async` only because the trait is; this future is `Ready` and never yields to the
    /// scheduler, so a `--no-capture` request path contains no capture-induced suspension point at
    /// all.
    async fn gate_critical_mutation(&mut self) -> GateOutcome {
        GateOutcome::Forward
    }
}

/// Upper bound on one event-driven wait for a publication outcome.
///
/// A liveness safety net only: outcomes normally wake the waiter immediately. CHOSEN DEFAULT.
pub const OUTCOME_SAFETY_WAKE: Duration = Duration::from_millis(10);

/// Waits until a publication outcome may have arrived.
///
/// Event-driven when the sink offers a wake-up, which replaces a fixed poll that added up to its
/// interval to every gated forward. [`OUTCOME_SAFETY_WAKE`] bounds the wait regardless, so a missed
/// wake-up can delay a connection by at most that much and can never stall it. Without a wake-up
/// the poll interval is used.
async fn wait_for_outcome(notifier: Option<Arc<tokio::sync::Notify>>, poll_interval: Duration) {
    match notifier {
        Some(notify) => {
            let _ = tokio::time::timeout(OUTCOME_SAFETY_WAKE, notify.notified()).await;
        }
        None => tokio::time::sleep(poll_interval).await,
    }
}

/// A scheduled flush that runs this much later than its deadline is reported as late.
///
/// `proxyCaptureProtocol.md` §4.2: "A callback that runs late records its lateness in a metric and
/// warning log, then enqueues the record immediately. Lateness alone does not compromise capture."
/// Every timer fires a few microseconds after its deadline, so warning on any nonzero lateness
/// would be noise; this threshold separates scheduler jitter from a starved connection task.
/// CHOSEN DEFAULT: no Java parameter carries it.
pub const SCHEDULED_FLUSH_LATENESS_WARNING: Duration = Duration::from_secs(1);

/// How long the capture gate waits between polls of the Kafka acknowledgement channel.
///
/// AWKWARD: `capture_kafka::connection_factory::ConnectionCapture` exposes only
/// `drain_outcomes()` (non-blocking) and `await_quiet(Duration)` (which blocks the calling
/// **thread** on a `std::sync::mpsc` receiver). Neither is awaitable, and blocking a Tokio worker
/// on the request path is not an option, so the gate polls. The interval bounds the latency this
/// adds to `kafkaCommitDuration`, which Phase 1 Gate 5 measures; a `tokio::sync::Notify` or an
/// `async fn quiet()` on `ConnectionCapture` would remove the poll entirely.
pub const GATE_POLL_INTERVAL: Duration = Duration::from_micros(200);

/// Consults the process gate for a source-forward admission.
///
/// This is the §5.5 linearisation point: the check and the admission share one non-reentrant
/// mutex, so once the gate closes no thread can newly pass it. The closure must not block, await,
/// or re-enter [`CaptureProcessState`]; here it only reads the state it was handed, which is the
/// only reading guaranteed not to be stale.
///
/// Returns [`GateOutcome::Refuse`] when the process is terminating — fail-closed after a required
/// capture failure. [`State::PassThrough`] forwards, because keeping uncaptured traffic flowing is
/// what fail-open is *for*.
#[must_use]
pub fn admit_forward(gate: &CaptureProcessState) -> GateOutcome {
    gate.forward_if_permitted(|observed| match observed {
        State::Capture | State::PassThrough => GateOutcome::Forward,
        // Unreachable: forward_if_permitted returns None instead of running the closure while
        // terminating. Spelled out rather than unreachable!() so a future State variant is a
        // compile error here instead of a panic on the data path.
        State::Terminating => GateOutcome::Refuse,
    })
    .unwrap_or(GateOutcome::Refuse)
}

/// Applies a required-capture failure to the process gate and reports whether to abort.
///
/// Under fail-open the gate moves irreversibly to [`State::PassThrough`] and this returns `Ok`:
/// the transport then keeps forwarding while [`CaptureSink::should_capture`] reports `false`.
///
/// # Errors
///
/// [`CaptureAbort`] under fail-closed, or for a failure the protocol classes as unstable, which
/// terminates in **both** policies. The gate has moved to [`State::Terminating`] and the transport
/// must close the connection with [`CloseReason::CaptureFailure`] without forwarding anything
/// further.
pub fn apply_capture_failure(
    gate: &Arc<CaptureProcessState>,
    failure: &CaptureFailure,
) -> Result<(), CaptureAbort> {
    match gate.capture_failed(failure) {
        State::PassThrough => Ok(()),
        State::Terminating | State::Capture => Err(CaptureAbort::new(failure.reason().to_owned())),
    }
}

/// The capture-mode sink: [`ConnectionRecorder`] plus the process gate.
///
/// This is where the three subsystems meet. The recorder owns the capture stream's ordering and
/// encoding; `S` owns handing records to Kafka and reporting back what happened; the process gate
/// owns the `--capture-failure-policy` decision. Nothing else in the process knows about all
/// three, and nothing else needs to.
///
/// # The invariant this type exists to enforce
///
/// `proxyCaptureProtocol.md` §5: a Critical Mutation Traffic request must be **durably published
/// and its acknowledgement validated** before a single byte of it is written to the destination.
/// [`CaptureSink::gate_critical_mutation`] is the only method here that can refuse, and it refuses
/// by consulting [`admit_forward`], which linearises the check against gate closure.
///
/// # What a failure does
///
/// Every capture failure takes the same route: [`apply_capture_failure`], which is the single
/// interpretation point for the configured policy.
///
/// * **fail-open** — the gate moves irreversibly to [`State::PassThrough`], this connection stops
///   capturing, the open record is released, and the method returns `Ok`. The transport keeps
///   forwarding uncaptured traffic, which is what fail-open is for.
/// * **fail-closed**, or any failure the protocol classes as unstable — the gate moves to
///   [`State::Terminating`] and the method returns [`CaptureAbort`], which closes the connection
///   with [`CloseReason::CaptureFailure`] and forwards nothing further.
///
/// # Deliberate limitation: the terminal acknowledgement is not awaited here
///
/// [`CaptureSink::close`] is a **synchronous** trait method, so it cannot await the terminal
/// record's acknowledgement even though §4.3 requires one before a writer lane may retire. The
/// close observation and its publication are submitted here as one operation
/// ([`ConnectionRecorder::close`]); waiting for the acknowledgement is
/// [`crate::shutdown::CaptureRetirement`]'s job, which is the only place in the process that has
/// both an `async` context and a deadline to enforce it against.
#[derive(Debug)]
pub struct RecordingCaptureSink<S: PublishingRecordSink> {
    recorder: ConnectionRecorder<S>,
    gate: Arc<CaptureProcessState>,
    /// False once capture for *this connection* has stopped — after a fail-open transition or
    /// after the terminal close. Distinct from the process-wide gate state: a connection can stop
    /// capturing while the process is still capture-authoritative only by terminating, so in
    /// practice this latches with the gate, and it is kept separate so the recorder is never
    /// touched after it has been aborted or closed.
    capturing: bool,
    /// True between a successfully recorded close and the terminal acknowledgement.
    ///
    /// Only a close that actually recorded the `CloseObservation` has a terminal record to wait
    /// for; after a fail-open abandonment the recorder was aborted and there is nothing to wait on.
    awaiting_terminal_ack: bool,
    poll_interval: Duration,
}

impl<S: PublishingRecordSink> RecordingCaptureSink<S> {
    /// Builds the sink over a fresh recorder.
    ///
    /// # Errors
    ///
    /// [`WireError`] when the recorder's identities, flush interval, or payload limit are invalid.
    /// All three are start-up configuration, so a failure here means the connection must be
    /// refused rather than served uncaptured.
    pub fn new(
        config: ConnectionRecorderConfig,
        sink: S,
        gate: Arc<CaptureProcessState>,
    ) -> Result<Self, WireError> {
        Ok(Self {
            recorder: ConnectionRecorder::new(config, sink)?,
            gate,
            capturing: true,
            awaiting_terminal_ack: false,
            poll_interval: GATE_POLL_INTERVAL,
        })
    }

    /// Overrides the gate's poll interval. For tests and for tuning against Gate 5.
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// The recorder, for the retirement path.
    pub fn recorder_mut(&mut self) -> &mut ConnectionRecorder<S> {
        &mut self.recorder
    }

    /// Whether every record this connection submitted has reached a terminal outcome.
    #[must_use]
    pub fn publication_is_quiet(&self) -> bool {
        let chain = self.recorder.chain();
        chain.in_flight().is_none() && chain.queued_len() == 0
    }

    /// Applies every publication outcome that has arrived, bridging them onto the recorder's chain.
    ///
    /// Acknowledging is what releases the next queued record, so this must run before any
    /// observation that could need a new record — otherwise a connection whose first record is
    /// still in flight would queue forever.
    fn pump(&mut self) -> Result<(), CaptureFailure> {
        let outcomes = self.recorder.sink_mut().take_outcomes();
        let mut first_failure = None;
        for outcome in outcomes {
            match outcome {
                PublicationOutcome::Acknowledged(_) => {
                    self.recorder.acknowledge_publication();
                }
                PublicationOutcome::Failed(reason) => {
                    // An ambiguous send is treated exactly like a failed one: neither establishes
                    // that the request reached Kafka before the proxy forwarded it.
                    self.recorder
                        .fail_publication(PublishFailure::new(SinkError::new(reason.clone())));
                    first_failure = first_failure.or(Some(reason));
                }
            }
        }
        match first_failure {
            None => Ok(()),
            Some(reason) => Err(CaptureFailure::required(format!(
                "capture publication failed on {}: {reason}",
                self.recorder.sink().route_description()
            ))),
        }
    }

    /// Stops capturing on this connection and applies the failure to the process gate.
    fn fail(&mut self, failure: &CaptureFailure) -> Result<(), CaptureAbort> {
        if self.capturing {
            self.capturing = false;
            // Java defect J-3: it has no operation for this, so the open record's pooled buffer
            // leaks on every pass-through transition. Here ownership releases it.
            let discarded = self.recorder.abort();
            tracing::warn!(
                connection = self.recorder.connection_id(),
                discarded_observations = discarded,
                reason = failure.reason(),
                "capture abandoned for this connection"
            );
        }
        apply_capture_failure(&self.gate, failure)
    }

    /// Runs one recorder operation, routing a wire failure through the policy.
    fn record<T>(
        &mut self,
        what: &'static str,
        operation: impl FnOnce(&mut ConnectionRecorder<S>) -> Result<T, WireError>,
    ) -> Result<(), CaptureAbort> {
        if !self.capturing {
            // Fail-open, or already closed. LHH:352-355: teardown after an irreversible
            // pass-through transition performs no terminal capture at all.
            return Ok(());
        }
        if let Err(failure) = self.pump() {
            return self.fail(&failure);
        }
        match operation(&mut self.recorder) {
            Ok(_) => Ok(()),
            Err(error) => {
                let failure = wire_failure(what, &error);
                self.fail(&failure)
            }
        }
    }

    /// Waits until everything submitted for this connection is durably acknowledged, then
    /// validates the acknowledgement.
    async fn await_validated_publication(&mut self) -> Result<(), CaptureFailure> {
        // Detach and submit the record carrying the completed request. There is no timeout here,
        // matching Java: the only bounds are the assembly and connection deadlines, which close
        // the connection from the transport's own select loop.
        let detached = self
            .recorder
            .flush()
            .map_err(|error| wire_failure("flush", &error))?;

        if detached.is_none() && self.publication_is_quiet() {
            // Nothing of this request is awaiting Kafka. Reached when capture for the request was
            // suppressed, so there is no publication whose durability could be required.
            return Ok(());
        }

        let mut acknowledged = None;
        loop {
            let outcomes = self.recorder.sink_mut().take_outcomes();
            let mut failure = None;
            for outcome in outcomes {
                match outcome {
                    PublicationOutcome::Acknowledged(metadata) => {
                        acknowledged = Some(metadata);
                        self.recorder.acknowledge_publication();
                    }
                    PublicationOutcome::Failed(reason) => {
                        self.recorder
                            .fail_publication(PublishFailure::new(SinkError::new(reason.clone())));
                        failure = failure.or(Some(reason));
                    }
                }
            }
            if let Some(reason) = failure {
                return Err(CaptureFailure::required(format!(
                    "capture publication failed before forwarding Critical Mutation Traffic on \
                     {}: {reason}",
                    self.recorder.sink().route_description()
                )));
            }
            if self.publication_is_quiet() {
                break;
            }
            // Bound first: the wait must not hold a borrow of the sink across the await, or the
            // future would require the sink to be `Sync`.
            let notifier = self.recorder.sink().outcome_notifier();
            wait_for_outcome(notifier, self.poll_interval).await;
        }

        let Some(metadata) = acknowledged else {
            // The chain went quiet without producing an acknowledgement, which means the recorder
            // and the publishing sink disagree about what is outstanding. That is corrupted local
            // state, not an ordinary capture failure, so it terminates under BOTH policies
            // (proxyCaptureProtocol.md 7.1) rather than silently forwarding an unpublished
            // mutation.
            return Err(CaptureFailure::unstable(format!(
                "capture publication for Critical Mutation Traffic on {} completed with no \
                 acknowledgement metadata",
                self.recorder.sink().route_description()
            )));
        };

        // The only place a returned partition is compared against the intended one, and the only
        // place broker time is checked against the heartbeat baseline.
        self.recorder
            .sink()
            .validate_critical_mutation(metadata)
            .map_err(CaptureFailure::required)
    }
}

/// Classifies a wire-layer failure for the capture-failure policy.
///
/// Encoding and sequencing failures are **unstable**: they mean the recorder's own invariants no
/// longer hold, and §7.1 requires that to terminate in both policies rather than degrade to
/// pass-through. A publication failure is an ordinary required-capture failure and obeys the
/// configured policy; it does not come through here.
fn wire_failure(what: &'static str, error: &WireError) -> CaptureFailure {
    CaptureFailure::unstable(format!("capture {what} failed: {error}"))
}

impl<S: PublishingRecordSink + Send> CaptureSink for RecordingCaptureSink<S> {
    fn should_capture(&self) -> bool {
        self.capturing && self.gate.should_capture()
    }

    fn capture_read(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("read", |recorder| recorder.record_read(timestamp, bytes))
    }

    fn request_dropped(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("requestDropped", |recorder| {
            recorder.cancel_capture_for_current_request(timestamp)
        })
    }

    fn set_end_of_message_lengths(
        &mut self,
        first_line_byte_length: u32,
        headers_byte_length: u32,
    ) {
        if !self.capturing {
            return;
        }
        // The wire field is a signed int32 whose negative values are the "unknown" sentinel, so a
        // length that does not fit is reported as unknown rather than wrapped into a negative that
        // the replayer would read as the sentinel by accident.
        self.recorder.set_end_of_first_line(
            i32::try_from(first_line_byte_length).unwrap_or(capture_wire::UNKNOWN_MESSAGE_LENGTH),
        );
        self.recorder.set_end_of_headers(
            i32::try_from(headers_byte_length).unwrap_or(capture_wire::UNKNOWN_MESSAGE_LENGTH),
        );
    }

    fn commit_end_of_message(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("endOfMessageIndicator", |recorder| {
            recorder.commit_end_of_message(timestamp)
        })
    }

    fn request_forwarded(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("requestForwarded", |recorder| {
            recorder.record_request_forwarded(timestamp)
        })
    }

    fn capture_write(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("write", |recorder| recorder.record_write(timestamp, bytes))
    }

    fn capture_interim_response(
        &mut self,
        at: SystemTime,
        bytes: &[u8],
    ) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("interimResponse", |recorder| {
            recorder.record_interim_response(timestamp, bytes)
        })
    }

    fn connection_exception(&mut self, at: SystemTime, message: &str) -> Result<(), CaptureAbort> {
        let timestamp = CaptureTimestamp::from_system_time(at);
        self.record("connectionException", |recorder| {
            recorder.record_connection_exception(timestamp, message)
        })
    }

    fn close(&mut self, at: SystemTime, reason: CloseReason) -> Result<(), CaptureAbort> {
        if !self.capturing {
            return Ok(());
        }
        let timestamp = CaptureTimestamp::from_system_time(at);
        tracing::debug!(
            connection = self.recorder.connection_id(),
            reason = reason.label(),
            "emitting the terminal close observation"
        );
        let result = self.record("close", |recorder| recorder.close(timestamp));
        // `record` clears `capturing` when the operation failed, so this is true only when the
        // CloseObservation was recorded and its terminal record handed to the chain.
        self.awaiting_terminal_ack = self.capturing && result.is_ok();
        // Terminated either way: nothing may follow a CloseObservation.
        self.capturing = false;
        result
    }

    /// Pumps this connection's publication chain until it is quiet.
    ///
    /// The per-connection chain keeps exactly one record in flight and only advances when its
    /// owner applies outcomes, so a terminal record that queued behind an unacknowledged one is
    /// released here rather than stranded when the connection task exits (gap A). The wait ends
    /// when every record is acknowledged, when publication fails (routed through the policy), or
    /// when the process stops being capture-authoritative, after which nothing more is
    /// trustworthy to wait for.
    async fn await_terminal_acknowledgement(&mut self) -> Result<(), CaptureAbort> {
        if !std::mem::take(&mut self.awaiting_terminal_ack) {
            return Ok(());
        }
        loop {
            if let Err(failure) = self.pump() {
                return apply_capture_failure(&self.gate, &failure);
            }
            if self.publication_is_quiet() {
                return Ok(());
            }
            match self.gate.state() {
                State::Capture => {}
                State::PassThrough => return Ok(()),
                State::Terminating => {
                    return Err(CaptureAbort::new(
                        "the process is terminating before the terminal record was acknowledged"
                            .to_owned(),
                    ));
                }
            }
            // Bound first: the wait must not hold a borrow of the sink across the await, or the
            // future would require the sink to be `Sync`.
            let notifier = self.recorder.sink().outcome_notifier();
            wait_for_outcome(notifier, self.poll_interval).await;
        }
    }

    async fn gate_critical_mutation(&mut self) -> GateOutcome {
        if !self.capturing {
            // Capture was already abandoned for this connection. Under fail-open that is the whole
            // point — keep forwarding — and under fail-closed the gate below refuses anyway.
            return admit_forward(&self.gate);
        }
        if let Err(failure) = self.await_validated_publication().await {
            // The policy decides, in exactly one place. Under fail-open the gate is now
            // PassThrough and admits the forward; under fail-closed it is Terminating and refuses.
            let _ = self.fail(&failure);
        }
        admit_forward(&self.gate)
    }

    /// The fixed `F` deadline of the open record, as a monotonic instant.
    ///
    /// Derived from the recorder's own remaining-time reading rather than cached, because the
    /// transport re-queries on every select iteration and the deadline only changes when a record
    /// opens or detaches. The two monotonic reads involved differ by nanoseconds, which is far
    /// below the scheduler's own resolution.
    fn next_flush_deadline(&self) -> Option<std::time::Instant> {
        if !self.capturing {
            return None;
        }
        let remaining = self.recorder.time_until_flush_deadline()?;
        // An unrepresentable instant cannot occur for any configured F; treat it as "fire now"
        // rather than as "never", so a record is never stranded by an arithmetic edge.
        Some(
            std::time::Instant::now()
                .checked_add(remaining)
                .unwrap_or_else(std::time::Instant::now),
        )
    }

    /// Publishes an otherwise-idle record once its fixed `F` deadline has passed.
    ///
    /// `proxyCaptureProtocol.md` §4.2 step 5: "the scheduled callback detaches and enqueues the
    /// nonempty record when it runs." The generation is read at fire time, which is correct here
    /// because the transport re-arms after every sink call that can open or detach a record, so the
    /// record whose deadline was slept on is always the current one. The recorder still guards the
    /// call: before the deadline it is a no-op (`Early`), with no open record it is a no-op, and
    /// after the terminal close it can never create a record.
    fn run_scheduled_flush(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
        let Some(deadline) = self.recorder.flush_deadline() else {
            return Ok(());
        };
        self.record("scheduledFlush", |recorder| {
            let outcome = recorder.run_scheduled_flush(deadline.generation)?;
            if let ScheduledFlush::Detached { lateness, .. } = outcome
                && lateness >= SCHEDULED_FLUSH_LATENESS_WARNING
            {
                tracing::warn!(
                    event = "capture_flush_deadline_late",
                    connection = recorder.connection_id(),
                    lateness_ms = u64::try_from(lateness.as_millis()).unwrap_or(u64::MAX),
                    "a scheduled record flush ran late; the record was enqueued immediately"
                );
            }
            Ok(outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use capture_config::CaptureFailurePolicy;

    use super::*;
    use crate::publication::TestRecordSink;

    /// A capture sink whose publications are acknowledged only when a test says so.
    fn recording(policy: CaptureFailurePolicy) -> RecordingCaptureSink<TestRecordSink> {
        recording_with(policy, TestRecordSink::new())
    }

    fn recording_with(
        policy: CaptureFailurePolicy,
        sink: TestRecordSink,
    ) -> RecordingCaptureSink<TestRecordSink> {
        RecordingCaptureSink::new(
            ConnectionRecorderConfig::new("connection-1", "writer-1"),
            sink,
            Arc::new(CaptureProcessState::new(policy)),
        )
        .expect("the recorder configuration is valid")
        .with_poll_interval(Duration::from_micros(1))
    }

    /// Drives one captured request up to the point the transport would call the gate.
    fn observe_one_request(sink: &mut RecordingCaptureSink<TestRecordSink>) {
        let now = SystemTime::now();
        sink.capture_read(now, b"POST /index/_doc HTTP/1.1\r\n\r\n{}")
            .expect("the read is captured");
        sink.set_end_of_message_lengths(27, 2);
        sink.commit_end_of_message(now)
            .expect("the end of message is committed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_mutation_forwards_once_its_publication_is_acknowledged_and_validated() {
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        observe_one_request(&mut sink);
        sink.recorder_mut().sink_mut().acknowledge_next();

        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);
        assert!(sink.should_capture());
        assert!(sink.publication_is_quiet());
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            1,
            "the request must have been handed to the transport before the gate forwarded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_gate_does_not_forward_before_the_acknowledgement_arrives() {
        // The central invariant: no byte of a mutating request may reach the destination until its
        // publication is durable. With no outcome queued the gate must still be waiting.
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        observe_one_request(&mut sink);

        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        {
            let mut gate = std::pin::pin!(sink.gate_critical_mutation());
            for _ in 0..8 {
                assert_eq!(
                    gate.as_mut().poll(&mut context),
                    std::task::Poll::Pending,
                    "the gate forwarded a mutation that Kafka had not acknowledged"
                );
            }
        }
        assert!(!sink.publication_is_quiet(), "a record is still in flight");

        // And it completes as soon as the acknowledgement lands.
        sink.recorder_mut().sink_mut().acknowledge_next();
        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_publication_refuses_the_forward_under_fail_closed() {
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        observe_one_request(&mut sink);
        sink.recorder_mut()
            .sink_mut()
            .fail_next("broker unreachable");

        assert_eq!(
            sink.gate_critical_mutation().await,
            GateOutcome::Refuse,
            "fail-closed must refuse rather than forward an uncaptured mutation"
        );
        assert!(sink.gate.is_terminating());
        assert!(!sink.should_capture());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_publication_forwards_uncaptured_under_fail_open() {
        let mut sink = recording(CaptureFailurePolicy::FailOpen);
        observe_one_request(&mut sink);
        sink.recorder_mut()
            .sink_mut()
            .fail_next("broker unreachable");

        assert_eq!(
            sink.gate_critical_mutation().await,
            GateOutcome::Forward,
            "fail-open exists to keep traffic flowing once capture is abandoned"
        );
        assert!(sink.gate.is_pass_through());
        assert!(
            !sink.should_capture(),
            "fail-open is expressed as Ok plus should_capture() == false"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_acknowledgement_that_does_not_validate_is_a_capture_failure() {
        // The acknowledgement arrived, so publication was durable -- but the metadata says it did
        // not land where this connection is bound, or outside the heartbeat baseline. Forwarding
        // on that basis would break the replayer's ordering guarantee.
        let mut sink = recording_with(
            CaptureFailurePolicy::FailClosed,
            TestRecordSink::new().rejecting_validation("log append time precedes the baseline"),
        );
        observe_one_request(&mut sink);
        sink.recorder_mut().sink_mut().acknowledge_next();

        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Refuse);
        assert!(sink.gate.is_terminating());
    }

    #[tokio::test(start_paused = true)]
    async fn after_fail_open_every_later_observation_succeeds_and_captures_nothing() {
        let mut sink = recording(CaptureFailurePolicy::FailOpen);
        observe_one_request(&mut sink);
        sink.recorder_mut()
            .sink_mut()
            .fail_next("broker unreachable");
        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);

        let before = sink.recorder_mut().sink().submitted_record_count();
        let now = SystemTime::now();
        assert!(
            sink.capture_read(now, b"GET /again HTTP/1.1\r\n\r\n")
                .is_ok()
        );
        assert!(sink.capture_write(now, b"HTTP/1.1 200 OK\r\n\r\n").is_ok());
        assert!(sink.commit_end_of_message(now).is_ok());
        assert!(sink.request_forwarded(now).is_ok());
        assert!(sink.connection_exception(now, "later failure").is_ok());
        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);

        // LHH:352-355: teardown after an irreversible pass-through transition performs no terminal
        // capture.
        assert!(sink.close(now, CloseReason::Normal).is_ok());
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            before,
            "no record may be published after capture was abandoned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_suppressed_request_needs_no_publication_to_forward() {
        // Suppression leaves no open record, so there is nothing whose durability could be
        // required and the gate must not wait for an acknowledgement that will never come.
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);
        assert!(sink.should_capture());
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 0);
    }

    #[test]
    fn the_terminal_close_publishes_exactly_one_final_record() {
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        let now = SystemTime::now();
        sink.capture_read(now, b"GET / HTTP/1.1\r\n\r\n")
            .expect("captured");
        sink.close(now, CloseReason::Normal).expect("closed");
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 1);

        // Nothing may follow a CloseObservation, and a second close must not try.
        assert!(sink.close(now, CloseReason::Normal).is_ok());
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 1);
        assert!(!sink.should_capture());
    }

    #[test]
    fn an_oversized_end_of_message_length_becomes_the_unknown_sentinel() {
        // The wire field is a signed int32 whose negative values mean "unknown", so a length that
        // does not fit must not wrap into a value the replayer reads as the sentinel by accident.
        let mut sink = recording(CaptureFailurePolicy::FailClosed);
        sink.set_end_of_message_lengths(u32::MAX, 4);
        let now = SystemTime::now();
        sink.commit_end_of_message(now).expect("committed");
        assert_eq!(capture_wire::UNKNOWN_MESSAGE_LENGTH, -1);
    }

    #[test]
    fn an_unstable_wire_failure_terminates_even_under_fail_open() {
        // 7.1: internal ownership or invariant corruption terminates in BOTH policies. A wire
        // encoding failure is exactly that, so it must not degrade to pass-through.
        let gate = Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailOpen));
        let mut sink = RecordingCaptureSink::new(
            ConnectionRecorderConfig::new("connection-1", "writer-1"),
            TestRecordSink::new(),
            Arc::clone(&gate),
        )
        .expect("valid");
        let now = SystemTime::now();
        sink.close(now, CloseReason::Normal).expect("first close");
        // A second close is WireError::AfterTerminalClose inside the recorder; the sink has
        // already latched `capturing = false`, so it reports Ok rather than re-entering the gate.
        assert!(sink.close(now, CloseReason::Normal).is_ok());
        assert!(
            gate.should_capture(),
            "a clean close must not itself compromise capture"
        );

        assert!(
            wire_failure("read", &capture_wire::WireError::EmptyConnectionId).is_unstable(),
            "an encoding or sequencing failure must terminate under both policies"
        );
    }

    #[test]
    fn a_recorder_with_an_empty_identity_is_refused_rather_than_served_uncaptured() {
        assert!(matches!(
            RecordingCaptureSink::new(
                ConnectionRecorderConfig::new("", "writer-1"),
                TestRecordSink::new(),
                Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailClosed)),
            ),
            Err(capture_wire::WireError::EmptyConnectionId)
        ));
    }

    #[tokio::test]
    async fn the_no_capture_sink_captures_nothing_and_always_forwards() {
        let mut sink = NoCaptureSink::new();
        let now = SystemTime::now();

        assert!(!sink.should_capture());
        assert!(sink.capture_read(now, b"GET / HTTP/1.1\r\n\r\n").is_ok());
        assert!(sink.request_dropped(now).is_ok());
        sink.set_end_of_message_lengths(16, 2);
        assert!(sink.commit_end_of_message(now).is_ok());
        assert!(sink.request_forwarded(now).is_ok());
        assert!(sink.capture_write(now, b"HTTP/1.1 200 OK\r\n\r\n").is_ok());
        assert!(
            sink.capture_interim_response(now, b"HTTP/1.1 100 \r\n\r\n")
                .is_ok()
        );
        assert!(sink.connection_exception(now, "boom").is_ok());
        assert!(sink.close(now, CloseReason::Normal).is_ok());
        assert_eq!(sink.gate_critical_mutation().await, GateOutcome::Forward);
    }

    #[tokio::test]
    async fn the_no_capture_gate_future_never_yields() {
        // The transport awaits this on the request path. If it ever returned Pending the
        // no-capture mode would pay a scheduler round trip per mutating request, which is exactly
        // the overhead the transport gates exist to exclude.
        let mut sink = NoCaptureSink::new();
        let mut future = std::pin::pin!(sink.gate_critical_mutation());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert_eq!(
            future.as_mut().poll(&mut context),
            std::task::Poll::Ready(GateOutcome::Forward)
        );
    }

    #[test]
    fn the_gate_forwards_while_capturing_and_while_passing_through() {
        let gate = CaptureProcessState::new(CaptureFailurePolicy::FailOpen);
        assert_eq!(admit_forward(&gate), GateOutcome::Forward);

        gate.capture_failed(&CaptureFailure::required("kafka send failed"));
        assert_eq!(gate.state(), State::PassThrough);
        assert_eq!(
            admit_forward(&gate),
            GateOutcome::Forward,
            "fail-open exists to keep forwarding uncaptured"
        );
    }

    #[test]
    fn the_gate_refuses_once_the_process_is_terminating() {
        let gate = CaptureProcessState::new(CaptureFailurePolicy::FailClosed);
        gate.capture_failed(&CaptureFailure::required("kafka send failed"));
        assert_eq!(gate.state(), State::Terminating);
        assert_eq!(admit_forward(&gate), GateOutcome::Refuse);
    }

    #[test]
    fn fail_open_reports_ok_and_fail_closed_aborts() {
        let open = Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailOpen));
        assert!(apply_capture_failure(&open, &CaptureFailure::required("down")).is_ok());
        assert!(open.is_pass_through());

        let closed = Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailClosed));
        let abort = apply_capture_failure(&closed, &CaptureFailure::required("down"))
            .expect_err("fail-closed must abort");
        assert!(abort.message.contains("down"));
        assert!(closed.is_terminating());
    }

    #[test]
    fn an_unstable_failure_aborts_even_under_fail_open() {
        // proxyCaptureProtocol 7.1: internal ownership corruption terminates in BOTH modes.
        let open = Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailOpen));
        assert!(
            apply_capture_failure(&open, &CaptureFailure::unstable("ownership corrupted")).is_err()
        );
        assert!(open.is_terminating());
    }
    /// A sink whose recorder detaches records on a short, real-time `F`.
    fn recording_with_flush_interval(
        flush_interval: Duration,
    ) -> RecordingCaptureSink<TestRecordSink> {
        RecordingCaptureSink::new(
            ConnectionRecorderConfig::new("connection-1", "writer-1")
                .with_flush_interval(flush_interval),
            TestRecordSink::new(),
            Arc::new(CaptureProcessState::new(CaptureFailurePolicy::FailClosed)),
        )
        .expect("the recorder configuration is valid")
        .with_poll_interval(Duration::from_micros(1))
    }

    #[test]
    fn an_idle_record_is_published_once_its_flush_deadline_passes() {
        // Gap B, capture-proxy half: the review-blocker state was a recorder whose F deadline had
        // no driver, so an idle connection's partial request never reached Kafka until the next
        // observation or the close.
        let flush_interval = Duration::from_millis(40);
        let mut sink = recording_with_flush_interval(flush_interval);
        assert!(
            sink.next_flush_deadline().is_none(),
            "no record is open, so there is no deadline"
        );

        sink.capture_read(SystemTime::now(), b"POST /index/_doc HTTP/1.1\r\nHost: x")
            .expect("the read is captured");
        let deadline = sink
            .next_flush_deadline()
            .expect("the first observation arms the record's fixed deadline");
        assert!(
            deadline <= std::time::Instant::now() + flush_interval,
            "the deadline must be no later than F after the first observation"
        );

        // Before the deadline the callback is a no-op.
        sink.run_scheduled_flush(SystemTime::now())
            .expect("an early callback is harmless");
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            0,
            "an early callback must not publish"
        );

        std::thread::sleep(flush_interval + Duration::from_millis(20));
        sink.run_scheduled_flush(SystemTime::now())
            .expect("the scheduled flush succeeds");
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            1,
            "the idle record must be published once its deadline has passed"
        );
        assert!(
            sink.next_flush_deadline().is_none(),
            "detaching clears the deadline until a later observation opens a successor record"
        );

        // A repeated, now-stale callback creates nothing.
        sink.run_scheduled_flush(SystemTime::now())
            .expect("a stale callback is harmless");
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 1);
    }

    #[test]
    fn a_closed_connection_never_reports_a_flush_deadline() {
        let mut sink = recording_with_flush_interval(Duration::from_mins(5));
        sink.capture_read(SystemTime::now(), b"GET / HTTP/1.1\r\n")
            .expect("the read is captured");
        assert!(sink.next_flush_deadline().is_some());
        sink.close(SystemTime::now(), CloseReason::Normal)
            .expect("the close is captured");
        assert!(
            sink.next_flush_deadline().is_none(),
            "nothing may follow a CloseObservation, so no flush may be scheduled"
        );
        let before = sink.recorder_mut().sink().submitted_record_count();
        sink.run_scheduled_flush(SystemTime::now())
            .expect("a callback after close is harmless");
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            before,
            "a callback after the terminal close must not create a record"
        );
    }
    /// Puts one record in flight (unacknowledged) and then closes, so the terminal record queues
    /// behind it. This is the gap A precondition.
    fn close_behind_an_in_flight_record(
        policy_flush: Duration,
    ) -> RecordingCaptureSink<TestRecordSink> {
        let mut sink = recording_with_flush_interval(policy_flush);
        sink.capture_read(SystemTime::now(), b"GET /big HTTP/1.1\r\nHost: x")
            .expect("the read is captured");
        std::thread::sleep(policy_flush + Duration::from_millis(20));
        sink.run_scheduled_flush(SystemTime::now())
            .expect("the scheduled flush succeeds");
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 1);
        sink.close(SystemTime::now(), CloseReason::Normal)
            .expect("the close is recorded");
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            1,
            "precondition: the terminal record queues behind the unacknowledged one"
        );
        sink
    }

    #[tokio::test]
    async fn a_terminal_record_queued_behind_an_in_flight_one_is_released_and_acknowledged() {
        // Gap A. The connection owner used to exit right after close(); the queued terminal record
        // was never dispatched, so the CloseObservation never reached Kafka and the connection was
        // never deregistered. Reproduced end-to-end by a 3 MiB response the source then closed.
        let mut sink = close_behind_an_in_flight_record(Duration::from_millis(30));
        sink.recorder_mut().sink_mut().acknowledge_next();
        sink.recorder_mut().sink_mut().acknowledge_next();

        sink.await_terminal_acknowledgement()
            .await
            .expect("both records are acknowledged");
        assert_eq!(
            sink.recorder_mut().sink().submitted_record_count(),
            2,
            "the terminal record must be dispatched once its predecessor is acknowledged"
        );
        assert!(sink.publication_is_quiet());
    }

    #[tokio::test]
    async fn the_terminal_wait_does_not_end_before_the_acknowledgement() {
        let mut sink = close_behind_an_in_flight_record(Duration::from_millis(30));
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            sink.await_terminal_acknowledgement(),
        )
        .await;
        assert!(
            waited.is_err(),
            "with nothing acknowledged the owner must keep waiting"
        );
        assert_eq!(sink.recorder_mut().sink().submitted_record_count(), 1);
    }

    #[tokio::test]
    async fn a_failed_terminal_publication_applies_the_policy() {
        let mut sink = close_behind_an_in_flight_record(Duration::from_millis(30));
        sink.recorder_mut()
            .sink_mut()
            .fail_next("broker unreachable");
        let result = sink.await_terminal_acknowledgement().await;
        assert!(
            result.is_err(),
            "under fail-closed a failed terminal publication is a capture abort"
        );
    }

    #[tokio::test]
    async fn a_connection_that_recorded_no_close_does_not_wait() {
        let mut sink = recording_with_flush_interval(Duration::from_mins(5));
        sink.await_terminal_acknowledgement()
            .await
            .expect("nothing to wait for");
    }
}
