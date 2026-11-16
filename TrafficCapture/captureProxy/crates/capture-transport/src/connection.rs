//! One connection's whole lifecycle (traceability row PROXY-08).
//!
//! # Single-task ownership
//!
//! Java's freedom comes from "one event loop owns the connection":
//! `trafficOffloader.bindToConnectionEventLoop` asserts it (`LHH:279-282`), the serializer throws
//! if bound off-loop, and `connectionObservationSequence` needs no atomics because of it. Here one
//! Tokio task owns the sink, both pumps, and both sockets, so `&mut` does at compile time what Java
//! does at runtime.
//!
//! # Bidirectional credit-of-one
//!
//! The loop below reads from exactly one direction at a time and does not read again until the
//! bytes it read have been accepted by the opposite socket. Client to source that is Java's
//! behaviour (`NSHP:58`, `FH:56-60`), and the capture gate depends on it: while the gate waits for
//! a Kafka acknowledgement no write has been issued, so no read credit is returned and the client
//! cannot send more. Source to client it is a deliberate divergence; see the `// DEVIATION:`
//! comment at the site and the crate documentation.
//!
//! # Teardown ordering
//!
//! Held response bytes, then the `connectionException` if there is one, then the terminal `close`
//! and its final publication (`LHH:343-369`, spec §12.1). A process that has stopped capturing
//! performs **no** terminal capture at all (`LHH:352-355`), which the Java tests assert explicitly.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::{Bytes, BytesMut};
use capture_config::{HttpStrictness, RequestLimits};
use capture_http_scan::{CapturePredicate, RewriteConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

use crate::listener::ConnectionShutdown;
use crate::request_pump::{RequestPump, RequestPumpError};
use crate::response_pump::ResponsePump;
use crate::write_completion::ForwardCompletion;
use crate::{CaptureAbort, CaptureSink, CloseReason, GateOutcome};

/// Default read-buffer size per direction.
///
/// Java sets no `RCVBUF_ALLOCATOR`, so Netty's adaptive allocator applies (1 KiB initial, growing).
/// A fixed 16 KiB is the simpler equivalent and bounds per-connection memory, which is the point of
/// the bidirectional backpressure this module adds.
pub const DEFAULT_READ_BUFFER_BYTES: usize = 16 * 1024;

/// Connection inputs.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Incomplete-request and connection-lifetime limits.
    pub limits: RequestLimits,
    /// HTTP line-ending strictness (owner decision OD-2).
    pub strictness: HttpStrictness,
    /// Header rewrites applied after capture.
    pub rewrite: RewriteConfig,
    /// Read-buffer size per direction.
    pub read_buffer_bytes: usize,
    /// Whether Critical Mutation Traffic is gated on durable publication.
    ///
    /// Production always installs the block predicate (`PCI:190`); the Java tests parameterise it,
    /// which is why it is configurable here.
    pub gate_critical_mutations: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            limits: RequestLimits::default(),
            strictness: HttpStrictness::default(),
            rewrite: RewriteConfig::new(),
            read_buffer_bytes: DEFAULT_READ_BUFFER_BYTES,
            gate_critical_mutations: true,
        }
    }
}

/// What a connection did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOutcome {
    /// The single latched close reason.
    pub close_reason: CloseReason,
    /// Client bytes read.
    pub client_bytes_read: u64,
    /// Bytes written to the source, after rewriting.
    pub source_bytes_written: u64,
    /// Bytes read from the source.
    pub source_bytes_read: u64,
    /// Bytes written to the client.
    pub client_bytes_written: u64,
    /// Request messages whose framing completed.
    pub requests_completed: u64,
    /// `requestForwarded` observations emitted.
    pub requests_forwarded: u64,
    /// The terminal capture failure, if capture was compromised.
    pub capture_abort: Option<CaptureAbort>,
}

/// Why the loop stopped.
#[derive(Debug)]
struct Termination {
    reason: CloseReason,
    /// Message for the `connectionException` observation, when the reason emits one.
    message: Option<String>,
    abort: Option<CaptureAbort>,
}

impl Termination {
    fn reason(reason: CloseReason) -> Self {
        Self {
            reason,
            message: None,
            abort: None,
        }
    }

    fn with_message(reason: CloseReason, message: String) -> Self {
        Self {
            reason,
            message: Some(message),
            abort: None,
        }
    }

    fn capture(abort: CaptureAbort) -> Self {
        Self {
            reason: CloseReason::CaptureFailure,
            message: None,
            abort: Some(abort),
        }
    }
}

/// The mutable per-connection state the client path needs.
///
/// Bundled so the handler takes a handful of arguments instead of eight, and so the borrow of each
/// field stays visibly disjoint.
struct ClientPath<'a, K, P>
where
    P: CapturePredicate,
{
    sink: &'a mut K,
    request: &'a mut RequestPump<P>,
    response: &'a mut ResponsePump,
    completion: &'a mut ForwardCompletion,
    counters: &'a mut Counters,
}

/// Byte counters, threaded through the handlers.
#[derive(Debug, Default)]
struct Counters {
    client_bytes_read: u64,
    source_bytes_written: u64,
    source_bytes_read: u64,
    client_bytes_written: u64,
    requests_forwarded: u64,
}

/// One client connection and its source connection.
#[derive(Debug)]
pub struct Connection<C, S, K, P>
where
    P: CapturePredicate,
{
    client: C,
    source: S,
    sink: K,
    request: RequestPump<P>,
    response: ResponsePump,
    completion: ForwardCompletion,
    config: ConnectionConfig,
    lifetime_origin: Option<Instant>,
}

impl<C, S, K, P> Connection<C, S, K, P>
where
    C: AsyncRead + AsyncWrite + Send + Unpin,
    S: AsyncRead + AsyncWrite + Send + Unpin,
    K: CaptureSink,
    P: CapturePredicate,
{
    /// Builds a connection.
    ///
    /// The source connection must already be usable — connected, and its TLS handshake complete.
    /// Java grants the client's first read only from the backside-connect listener for exactly that
    /// reason (`FH:37-47`, `BCP:105-111`).
    pub fn new(client: C, source: S, sink: K, predicate: P, config: ConnectionConfig) -> Self {
        let request =
            RequestPump::new(config.limits, config.strictness, predicate, &config.rewrite);
        Self {
            client,
            source,
            sink,
            request,
            response: ResponsePump::new(),
            completion: ForwardCompletion::new(),
            config,
            lifetime_origin: None,
        }
    }

    /// Anchors the connection-lifetime deadline at `origin` rather than at the moment this
    /// connection starts running.
    ///
    /// Java schedules that deadline in `handlerAdded` (`LHH:287-303`), i.e. at pipeline install —
    /// **before** the TLS handshake and before any byte. A connection is only constructed here once
    /// its source is usable, so without this the handshake would be excluded from the budget.
    /// The process should pass the instant it accepted the client socket.
    #[must_use]
    pub fn with_lifetime_origin(mut self, origin: Instant) -> Self {
        self.lifetime_origin = Some(origin);
        self
    }

    /// Runs the connection to completion.
    ///
    /// `shutdown`, when present, closes the connection on request — the registry's
    /// disconnect-all.
    #[allow(
        clippy::too_many_lines,
        reason = "the select loop and its teardown are one sequence; splitting them would hide the \
                  ordering the capture stream depends on"
    )]
    pub async fn run(self, shutdown: Option<Arc<ConnectionShutdown>>) -> ConnectionOutcome {
        let Self {
            client,
            source,
            mut sink,
            mut request,
            mut response,
            mut completion,
            config,
            lifetime_origin,
        } = self;

        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        let (mut source_reader, mut source_writer) = tokio::io::split(source);
        let mut client_buf = BytesMut::with_capacity(config.read_buffer_bytes);
        let mut source_buf = vec![0u8; config.read_buffer_bytes];
        let mut counters = Counters::default();

        // Armed at connection start, i.e. before the TLS handshake and before any byte, matching
        // Java's scheduling in `handlerAdded` (`LHH:287-303`). Source activity never extends it.
        let connection_sleep = tokio::time::sleep_until(
            lifetime_origin.unwrap_or_else(Instant::now) + config.limits.max_connection_duration(),
        );
        tokio::pin!(connection_sleep);
        // The assembly deadline is fixed from the first byte of a request and is never extended by
        // progress (spec §4.4); it is re-armed, not refreshed.
        let mut assembly_sleep = Box::pin(tokio::time::sleep(config.limits.max_request_assembly()));
        let mut assembly_armed = false;

        let termination = loop {
            client_buf.reserve(config.read_buffer_bytes);

            // Re-queried every iteration, i.e. after every sink call that can open or detach a
            // record: reads, end-of-message commits, scheduled flushes. The deadline itself is
            // FIXED — `proxyCaptureProtocol.md` §4.2's "continuous traffic cannot postpone it" —
            // so re-creating the sleep each iteration re-arms at the same instant until the record
            // detaches, at which point the sink reports the successor's deadline or `None`.
            let flush_deadline = sink.next_flush_deadline().map(Instant::from_std);

            let termination = tokio::select! {
                () = &mut connection_sleep => Some(Termination::reason(CloseReason::ConnectionDuration)),
                // The F record-flush driver (`proxyCaptureProtocol.md` §4.2): the scheduled
                // callback that detaches and enqueues an otherwise-idle nonempty record. This
                // branch is selectable only while the connection is idle in this select — every
                // read handler runs to completion before the select is polled again — so a flush
                // can never split the capture sequence of a read. The generation guard in the
                // record layer makes a lost race (a read firing at the same instant) harmless: the
                // next iteration re-arms at the unchanged deadline.
                () = flush_sleep_until(flush_deadline), if flush_deadline.is_some() => {
                    sink.run_scheduled_flush(SystemTime::now())
                        .map_err(Termination::capture)
                        .err()
                }
                () = &mut assembly_sleep, if assembly_armed => {
                    Some(Termination::reason(CloseReason::AssemblyDuration))
                }
                () = wait_for_shutdown(shutdown.as_ref()) => {
                    Some(Termination::reason(CloseReason::Disconnected))
                }
                result = client_reader.read_buf(&mut client_buf) => {
                    let was_in_progress = request.is_request_in_progress();
                    let completed_before = request.requests_completed();
                    let outcome = match result {
                        Err(error) => Some(Termination::with_message(
                            CloseReason::ClientIoError,
                            error.to_string(),
                        )),
                        // EOF. There is no half-close handling anywhere in the Java proxy: no
                        // ALLOW_HALF_CLOSURE is configured, so an inbound EOF closes the
                        // connection (spec §12.2).
                        Ok(0) => Some(Termination::reason(CloseReason::Normal)),
                        Ok(read) => {
                            counters.client_bytes_read += read as u64;
                            let chunk = client_buf.split().freeze();
                            handle_client_chunk(
                                &chunk,
                                ClientPath {
                                    sink: &mut sink,
                                    request: &mut request,
                                    response: &mut response,
                                    completion: &mut completion,
                                    counters: &mut counters,
                                },
                                &mut source_writer,
                                &config,
                            )
                            .await
                            .err()
                        }
                    };
                    // Assembly-deadline lifecycle, exactly the six steps of spec §4.4.
                    if request.is_request_in_progress() {
                        let another_began_after_one_completed =
                            was_in_progress && request.requests_completed() > completed_before;
                        if !assembly_armed || another_began_after_one_completed {
                            assembly_sleep
                                .as_mut()
                                .reset(Instant::now() + config.limits.max_request_assembly());
                            assembly_armed = true;
                        }
                    } else {
                        assembly_armed = false;
                    }
                    outcome
                }
                result = source_reader.read(&mut source_buf) => {
                    match result {
                        // A source-side failure closes the client connection rather than firing an
                        // exception on it, so it produces no `connectionException` observation
                        // (`BH:50-56`, spec §12.3).
                        Err(_) => Some(Termination::reason(CloseReason::SourceIoError)),
                        Ok(0) => Some(Termination::reason(CloseReason::Normal)),
                        Ok(read) => {
                            counters.source_bytes_read += read as u64;
                            handle_source_chunk(
                                &source_buf[..read],
                                &mut sink,
                                &request,
                                &mut response,
                                &mut client_writer,
                                &mut counters,
                            )
                            .await
                            .err()
                        }
                    }
                }
            };

            if let Some(termination) = termination {
                break termination;
            }
        };

        // ---- teardown, in the order the capture stream requires ----
        let at = SystemTime::now();
        let mut abort = termination.abort;
        if sink.should_capture() {
            // Held response bytes first: they are earlier on the wire than anything that follows.
            if let Err(failure) = response.flush_held(&mut sink, request.suppression(), at) {
                abort = abort.or(Some(failure));
            }
            if termination.reason.emits_connection_exception() {
                let message = termination
                    .message
                    .clone()
                    .unwrap_or_else(|| termination.reason.label().to_owned());
                if let Err(failure) = sink.connection_exception(at, &message) {
                    abort = abort.or(Some(failure));
                }
            }
            if let Err(failure) = sink.close(at, termination.reason) {
                abort = abort.or(Some(failure));
            }
        }
        completion.mark_finalized();

        if termination.reason != CloseReason::Normal {
            tracing::warn!(
                event = "capture_connection_dropped",
                reason = termination.reason.label(),
                "the connection was dropped"
            );
        }
        // Pending writes drain before the close, which is what Java's `closeAndFlush` does by
        // writing an empty buffer with a CLOSE listener (`FH:104-110`).
        let _ = source_writer.shutdown().await;
        let _ = client_writer.shutdown().await;

        // proxyCaptureProtocol.md 5.1 steps 4-5: the connection owner stays alive -- and the
        // connection stays registered -- until the terminal record and every earlier record are
        // acknowledged. The sockets are already shut down, so the client and source are not held
        // open while Kafka acknowledges. Skipped after a capture abort: there is nothing
        // trustworthy left to wait for.
        if abort.is_none()
            && let Err(failure) = sink.await_terminal_acknowledgement().await
        {
            abort = Some(failure);
        }

        ConnectionOutcome {
            close_reason: termination.reason,
            client_bytes_read: counters.client_bytes_read,
            source_bytes_written: counters.source_bytes_written,
            source_bytes_read: counters.source_bytes_read,
            client_bytes_written: counters.client_bytes_written,
            requests_completed: request.requests_completed(),
            requests_forwarded: counters.requests_forwarded,
            capture_abort: abort,
        }
    }
}

/// Sleeps until the record-flush deadline, or forever when no record is open.
///
/// The `None` arm is unreachable under the select guard but keeps the future total.
async fn flush_sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Waits for a close request, or forever when there is nothing to wait for.
async fn wait_for_shutdown(shutdown: Option<&Arc<ConnectionShutdown>>) {
    match shutdown {
        Some(shutdown) => shutdown.triggered().await,
        None => std::future::pending().await,
    }
}

/// Captures, gates, rewrites, and forwards one client read.
async fn handle_client_chunk<S, K, P>(
    chunk: &Bytes,
    path: ClientPath<'_, K, P>,
    source_writer: &mut S,
    config: &ConnectionConfig,
) -> Result<(), Termination>
where
    S: AsyncWrite + Unpin,
    K: CaptureSink,
    P: CapturePredicate,
{
    let ClientPath {
        sink,
        request,
        response,
        completion,
        counters,
    } = path;
    let at = SystemTime::now();

    // Before this request's reads, any held response bytes become an ordinary `write`
    // observation and the classifier starts the next exchange (`LHH:841-854`).
    if response.has_held_bytes() || response.final_response_seen() {
        response
            .flush_held(sink, request.suppression(), at)
            .map_err(Termination::capture)?;
        response.begin_request();
    }

    let mut out: Vec<Bytes> = Vec::new();
    let step = match request.process(sink, at, chunk, &mut out) {
        Ok(step) => step,
        Err(RequestPumpError::Capture(abort)) => return Err(Termination::capture(abort)),
        Err(RequestPumpError::Scan(error)) => {
            // The read that violated a limit is still captured, which is what Java does: it
            // captures after decoding and only then acts on the violation (`LHH:476-491`), and the
            // Java limit test asserts at least one `read` observation survives. Under OD-1 the
            // framing refusals take the same path, so the refused bytes are visible in the capture
            // stream instead of vanishing.
            if sink.should_capture() && request.suppression().should_capture_request() {
                let _ = sink.capture_read(at, chunk);
            }
            let reason = CloseReason::from(&error);
            return Err(Termination::with_message(reason, error.to_string()));
        }
    };

    // The capture-before-forward gate. Nothing has been written to the source yet, and because no
    // write has been issued no read credit has been returned, so the hold is self-limiting.
    if step.gate_required && config.gate_critical_mutations {
        match sink.gate_critical_mutation().await {
            GateOutcome::Forward => {}
            GateOutcome::Refuse => {
                return Err(Termination::reason(CloseReason::CaptureFailure));
            }
        }
    }

    // Armed before the write is issued, as in Java (`LHH:671`).
    completion.arm(step.captured_requests_completed);
    let mut written = 0u64;
    for segment in &out {
        if source_writer.write_all(segment).await.is_err() {
            completion.discard();
            return Err(Termination::reason(CloseReason::SourceIoError));
        }
        written += segment.len() as u64;
    }
    if source_writer.flush().await.is_err() {
        completion.discard();
        return Err(Termination::reason(CloseReason::SourceIoError));
    }
    counters.source_bytes_written += written;

    // The forward observation is emitted before the next read is granted, which is the ordering
    // Java comments in-source at `FH:58-59`. Returning from this function is what grants the next
    // read, so the ordering is structural.
    let forwarded = completion
        .report_write_completed(sink, SystemTime::now())
        .map_err(Termination::capture)?;
    counters.requests_forwarded += u64::from(forwarded);
    Ok(())
}

/// Classifies, captures, and forwards one source read.
async fn handle_source_chunk<C, K, P>(
    chunk: &[u8],
    sink: &mut K,
    request: &RequestPump<P>,
    response: &mut ResponsePump,
    client_writer: &mut C,
    counters: &mut Counters,
) -> Result<(), Termination>
where
    C: AsyncWrite + Unpin,
    K: CaptureSink,
    P: CapturePredicate,
{
    let at = SystemTime::now();
    // Capture first. A failure here must not reach the client (`LHH:836-839`).
    let step = response
        .process(sink, request.suppression(), at, chunk)
        .map_err(Termination::capture)?;

    if client_writer.write_all(chunk).await.is_err() {
        // A failed write to the client closes both channels and produces no exception
        // observation (`BH:26-33`, pinned by `targetToClientWriteFailureClosesBothChannels...`).
        return Err(Termination::reason(CloseReason::ClientWriteFailure));
    }
    if client_writer.flush().await.is_err() {
        return Err(Termination::reason(CloseReason::ClientWriteFailure));
    }
    counters.client_bytes_written += chunk.len() as u64;

    // DEVIATION: the next source read is issued only now, so the source cannot outrun the client.
    // Java has no backpressure at all in this direction: `FH:70` flips the target channel to
    // auto-read on the first client read, which makes `BCP:92`'s AUTO_READ=false dead
    // configuration, `BH:25` never consults `isWritable()`, and no write watermarks are configured
    // anywhere (spec §9.2, §13-D4). A slow client with a fast source therefore grows the client
    // channel's outbound buffer without bound until the container dies of OOM. Returning here —
    // and only here — is the credit that fixes it, and it is what makes per-connection memory
    // bounded by one read buffer per direction plus two bounded holds.
    if let Some(reason) = step.close_after_write {
        return Err(Termination::reason(reason));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use capture_http_scan::SuppressionPolicy;
    use std::sync::Mutex;

    /// A [`CaptureSink`] that records what it was asked to do, in order.
    ///
    /// The event list is the whole point: the capture stream's legal orderings are what the Java
    /// tests assert, and `"gate"` appears in it so the capture-before-forward ordering is pinned
    /// rather than assumed.
    #[derive(Debug)]
    pub(crate) struct RecordingSink {
        events: Vec<String>,
        reads: Vec<Vec<u8>>,
        writes: Vec<Vec<u8>>,
        interim_responses: Vec<Vec<u8>>,
        end_of_message_lengths: Vec<(u32, u32)>,
        pending_lengths: (u32, u32),
        close_reason: Option<CloseReason>,
        exceptions: Vec<String>,
        capturing: bool,
        fail_next: bool,
        fail_write_capture: bool,
        gate: GateOutcome,
        gate_calls: u32,
        /// `Some` enables the record-deadline model: the first observation after a detach opens a
        /// record whose fixed deadline is now + the interval, exactly the §4.2 shape the wire
        /// layer's `ConnectionRecorder` implements for real.
        flush_interval: Option<std::time::Duration>,
        /// The open record's fixed deadline. `None` means no record is open.
        open_record_deadline: Option<std::time::Instant>,
        /// When each scheduled flush ran, in virtual time under a paused clock.
        flush_times: Vec<std::time::Instant>,
        /// Bytes the source had received when the gate was called, for the ordering assertion.
        source_bytes_at_gate: Option<Arc<Mutex<u64>>>,
    }

    impl RecordingSink {
        pub(crate) fn new() -> Self {
            Self {
                events: Vec::new(),
                reads: Vec::new(),
                writes: Vec::new(),
                interim_responses: Vec::new(),
                end_of_message_lengths: Vec::new(),
                pending_lengths: (0, 0),
                close_reason: None,
                exceptions: Vec::new(),
                capturing: true,
                fail_next: false,
                fail_write_capture: false,
                gate: GateOutcome::Forward,
                gate_calls: 0,
                flush_interval: None,
                open_record_deadline: None,
                flush_times: Vec::new(),
                source_bytes_at_gate: None,
            }
        }

        pub(crate) fn stop_capturing(&mut self) {
            self.capturing = false;
        }

        pub(crate) fn fail_next(&mut self) {
            self.fail_next = true;
        }

        /// Fails every `write` observation, so a response-path capture failure is deterministic
        /// rather than a race against the connection's own progress.
        pub(crate) fn fail_write_capture(&mut self) {
            self.fail_write_capture = true;
        }

        pub(crate) fn refuse_gate(&mut self) {
            self.gate = GateOutcome::Refuse;
        }

        /// Enables the record-deadline model with flush interval `F`.
        ///
        /// Must only be used inside a Tokio runtime: the deadline is taken from the Tokio clock so
        /// paused-time tests measure exact virtual instants.
        pub(crate) fn with_flush_interval(mut self, interval: std::time::Duration) -> Self {
            self.flush_interval = Some(interval);
            self
        }

        pub(crate) fn flush_times(&self) -> Vec<std::time::Instant> {
            self.flush_times.clone()
        }

        /// Any observation on a detached record opens a new one with a FIXED deadline. Later
        /// observations do not move it — that is the §4.2 invariant under test.
        fn note_observation(&mut self) {
            if let Some(interval) = self.flush_interval
                && self.open_record_deadline.is_none()
            {
                self.open_record_deadline = Some(tokio::time::Instant::now().into_std() + interval);
            }
        }

        pub(crate) fn events(&self) -> Vec<String> {
            self.events.clone()
        }

        pub(crate) fn captured_reads(&self) -> Vec<Vec<u8>> {
            self.reads.clone()
        }

        pub(crate) fn captured_writes(&self) -> Vec<Vec<u8>> {
            self.writes.clone()
        }

        pub(crate) fn captured_interim_responses(&self) -> Vec<Vec<u8>> {
            self.interim_responses.clone()
        }

        pub(crate) fn end_of_message_lengths(&self) -> Vec<(u32, u32)> {
            self.end_of_message_lengths.clone()
        }

        pub(crate) fn close_reason(&self) -> Option<CloseReason> {
            self.close_reason
        }

        pub(crate) fn exceptions(&self) -> Vec<String> {
            self.exceptions.clone()
        }

        pub(crate) fn gate_calls(&self) -> u32 {
            self.gate_calls
        }

        fn check(&mut self, operation: &str) -> Result<(), CaptureAbort> {
            if std::mem::take(&mut self.fail_next) {
                return Err(CaptureAbort::new(format!(
                    "the recording sink refused {operation}"
                )));
            }
            Ok(())
        }
    }

    impl CaptureSink for RecordingSink {
        fn should_capture(&self) -> bool {
            self.capturing
        }

        fn capture_read(&mut self, _at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
            self.check("read")?;
            self.events.push("read".to_owned());
            self.note_observation();
            self.reads.push(bytes.to_vec());
            Ok(())
        }

        fn request_dropped(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            self.check("dropped")?;
            self.events.push("dropped".to_owned());
            self.note_observation();
            Ok(())
        }

        fn set_end_of_message_lengths(&mut self, first_line: u32, headers: u32) {
            self.pending_lengths = (first_line, headers);
        }

        fn commit_end_of_message(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            self.check("eom")?;
            self.events.push("eom".to_owned());
            self.note_observation();
            self.end_of_message_lengths.push(self.pending_lengths);
            Ok(())
        }

        fn request_forwarded(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            self.check("forwarded")?;
            self.events.push("forwarded".to_owned());
            self.note_observation();
            Ok(())
        }

        fn capture_write(&mut self, _at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
            if self.fail_write_capture {
                return Err(CaptureAbort::new("the recording sink refused write"));
            }
            self.check("write")?;
            self.events.push("write".to_owned());
            self.note_observation();
            self.writes.push(bytes.to_vec());
            Ok(())
        }

        fn capture_interim_response(
            &mut self,
            _at: SystemTime,
            bytes: &[u8],
        ) -> Result<(), CaptureAbort> {
            self.check("interim")?;
            self.events.push("interim".to_owned());
            self.note_observation();
            self.interim_responses.push(bytes.to_vec());
            Ok(())
        }

        fn connection_exception(
            &mut self,
            _at: SystemTime,
            message: &str,
        ) -> Result<(), CaptureAbort> {
            self.check("exception")?;
            self.events.push("exception".to_owned());
            self.note_observation();
            self.exceptions.push(message.to_owned());
            Ok(())
        }

        fn close(&mut self, _at: SystemTime, reason: CloseReason) -> Result<(), CaptureAbort> {
            self.check("close")?;
            self.events.push("close".to_owned());
            // The terminal close detaches and publishes the final record, invalidating any pending
            // scheduled flush — the wire layer's `ConnectionRecorder::close` does the same.
            self.open_record_deadline = None;
            self.close_reason = Some(reason);
            Ok(())
        }

        async fn gate_critical_mutation(&mut self) -> GateOutcome {
            self.gate_calls += 1;
            self.events.push("gate".to_owned());
            if let Some(observed) = &self.source_bytes_at_gate {
                let written = *observed.lock().expect("not poisoned");
                assert_eq!(
                    written, 0,
                    "no byte of a gated request may reach the source before the gate runs"
                );
            }
            self.gate
        }

        fn next_flush_deadline(&self) -> Option<std::time::Instant> {
            self.open_record_deadline
        }

        fn run_scheduled_flush(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            if self.open_record_deadline.is_none() {
                // Stale callback: the record already detached. The wire layer's generation guard
                // makes this a no-op, and so does this model.
                return Ok(());
            }
            self.check("flush")?;
            self.events.push("flush".to_owned());
            self.flush_times
                .push(tokio::time::Instant::now().into_std());
            self.open_record_deadline = None;
            Ok(())
        }
    }

    const POST: &str = "POST /thing HTTP/1.1\r\nContent-Length: 2\r\n\r\nab";
    const GET: &str = "GET /thing HTTP/1.1\r\nHost: source\r\n\r\n";
    const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

    fn config() -> ConnectionConfig {
        ConnectionConfig::default()
    }

    /// Runs a connection against in-memory pipes, driving the client and source scripts.
    ///
    /// `client_script` is written to the proxy as the client; `source_script` is written back as the
    /// source once it has received `source_wait` bytes.
    async fn run_connection(
        sink: RecordingSink,
        config: ConnectionConfig,
        client_script: &[u8],
        source_script: &[u8],
    ) -> (ConnectionOutcome, RecordingSink, Vec<u8>, Vec<u8>) {
        let (client_side, client_proxy) = tokio::io::duplex(64 * 1024);
        let (source_proxy, source_side) = tokio::io::duplex(64 * 1024);

        let sink = Arc::new(Mutex::new(Some(sink)));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config,
        );
        let proxy = tokio::spawn(connection.run(None));

        let client_script = client_script.to_vec();
        let source_script = source_script.to_vec();
        let expected_from_client = client_script.len();
        let client = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(client_side);
            writer
                .write_all(&client_script)
                .await
                .expect("client write");
            writer.flush().await.expect("client flush");
            let mut received = Vec::new();
            let _ = reader.read_to_end(&mut received).await;
            received
        });
        let source = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(source_side);
            let mut received = vec![0u8; expected_from_client];
            let mut filled = 0;
            while filled < expected_from_client {
                match reader.read(&mut received[filled..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => filled += read,
                }
            }
            received.truncate(filled);
            if !source_script.is_empty() {
                let _ = writer.write_all(&source_script).await;
                let _ = writer.flush().await;
            }
            // Shut the write half down so the proxy sees EOF and tears down. Dropping a split half
            // is not enough: a `tokio::io::split` half only closes the stream when both halves go.
            let _ = writer.shutdown().await;
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest).await;
            received.extend(rest);
            received
        });

        let outcome = proxy.await.expect("the proxy task does not panic");
        let client_received = client.await.expect("client task");
        let source_received = source.await.expect("source task");
        let sink = sink
            .lock()
            .expect("not poisoned")
            .take()
            .expect("the sink is returned");
        (outcome, sink, source_received, client_received)
    }

    /// Shares one [`RecordingSink`] with the test after the connection has consumed it.
    #[derive(Debug)]
    struct SinkHandle(Arc<Mutex<Option<RecordingSink>>>);

    impl SinkHandle {
        fn with<R>(&mut self, body: impl FnOnce(&mut RecordingSink) -> R) -> R {
            let mut guard = self.0.lock().expect("not poisoned");
            body(guard.as_mut().expect("the sink is present"))
        }
    }

    impl CaptureSink for SinkHandle {
        fn should_capture(&self) -> bool {
            self.0
                .lock()
                .expect("not poisoned")
                .as_ref()
                .expect("present")
                .should_capture()
        }

        fn capture_read(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.capture_read(at, bytes))
        }

        fn request_dropped(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.request_dropped(at))
        }

        fn set_end_of_message_lengths(&mut self, first_line: u32, headers: u32) {
            self.with(|sink| sink.set_end_of_message_lengths(first_line, headers));
        }

        fn commit_end_of_message(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.commit_end_of_message(at))
        }

        fn request_forwarded(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.request_forwarded(at))
        }

        fn capture_write(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.capture_write(at, bytes))
        }

        fn capture_interim_response(
            &mut self,
            at: SystemTime,
            bytes: &[u8],
        ) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.capture_interim_response(at, bytes))
        }

        fn connection_exception(
            &mut self,
            at: SystemTime,
            message: &str,
        ) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.connection_exception(at, message))
        }

        fn close(&mut self, at: SystemTime, reason: CloseReason) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.close(at, reason))
        }

        fn next_flush_deadline(&self) -> Option<std::time::Instant> {
            self.0
                .lock()
                .expect("not poisoned")
                .as_ref()
                .expect("present")
                .next_flush_deadline()
        }

        fn run_scheduled_flush(&mut self, at: SystemTime) -> Result<(), CaptureAbort> {
            self.with(|sink| sink.run_scheduled_flush(at))
        }

        async fn gate_critical_mutation(&mut self) -> GateOutcome {
            let mut guard = self.0.lock().expect("not poisoned");
            let sink = guard.as_mut().expect("present");
            sink.gate_calls += 1;
            sink.events.push("gate".to_owned());
            sink.gate
        }
    }

    #[tokio::test]
    async fn a_gated_mutation_captures_then_gates_then_forwards_then_reports() {
        let (outcome, sink, source_received, client_received) = run_connection(
            RecordingSink::new(),
            config(),
            POST.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;

        // The whole ordering, in one assertion: the gate runs after the end-of-message observation
        // and before the forward, and the response is captured after that.
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "gate".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "close".to_owned(),
            ]
        );
        assert_eq!(sink.gate_calls(), 1);
        assert_eq!(sink.end_of_message_lengths(), vec![(20, 17)]);
        assert_eq!(source_received, POST.as_bytes());
        assert_eq!(client_received, RESPONSE.as_bytes());
        assert_eq!(outcome.close_reason, CloseReason::Normal);
        assert_eq!(outcome.requests_forwarded, 1);
        assert_eq!(outcome.requests_completed, 1);
        assert_eq!(outcome.client_bytes_read, POST.len() as u64);
        assert_eq!(outcome.source_bytes_written, POST.len() as u64);
        assert_eq!(outcome.client_bytes_written, RESPONSE.len() as u64);
        assert_eq!(sink.close_reason(), Some(CloseReason::Normal));
    }

    #[tokio::test]
    async fn a_refused_gate_never_lets_the_request_reach_the_source() {
        // The invariant the whole proxy exists for: fail-closed means the mutation is not applied.
        let mut sink = RecordingSink::new();
        sink.refuse_gate();
        let (outcome, sink, source_received, _) =
            run_connection(sink, config(), POST.as_bytes(), RESPONSE.as_bytes()).await;
        assert!(
            source_received.is_empty(),
            "the source received {source_received:?}"
        );
        assert_eq!(outcome.close_reason, CloseReason::CaptureFailure);
        assert_eq!(outcome.requests_forwarded, 0);
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "gate".to_owned(),
                "close".to_owned()
            ]
        );
        assert_eq!(sink.close_reason(), Some(CloseReason::CaptureFailure));
    }

    #[tokio::test]
    async fn a_non_mutating_request_is_not_gated() {
        let (outcome, sink, source_received, _) = run_connection(
            RecordingSink::new(),
            config(),
            GET.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;
        assert_eq!(sink.gate_calls(), 0);
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "close".to_owned()
            ]
        );
        assert_eq!(source_received, GET.as_bytes());
        assert_eq!(outcome.requests_forwarded, 1);
    }

    #[tokio::test]
    async fn malformed_framing_closes_the_connection_and_forwards_nothing() {
        // Owner decision OD-1. Java tunnels these bytes to the source uncaptured for up to five
        // minutes or 250 MiB.
        let request = "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx";
        let (outcome, sink, source_received, _) = run_connection(
            RecordingSink::new(),
            config(),
            request.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;
        assert!(
            source_received.is_empty(),
            "the source received {source_received:?}"
        );
        assert_eq!(outcome.close_reason, CloseReason::MalformedFraming);
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "exception".to_owned(),
                "close".to_owned()
            ],
            "the refused bytes are captured, then the refusal is observable"
        );
        assert_eq!(sink.exceptions().len(), 1);
        assert!(
            sink.exceptions()[0].contains("Content-Length"),
            "{:?}",
            sink.exceptions()
        );
    }

    #[tokio::test]
    async fn interim_responses_are_captured_and_forwarded_verbatim() {
        let response = concat!(
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        );
        let (outcome, sink, _, client_received) = run_connection(
            RecordingSink::new(),
            config(),
            POST.as_bytes(),
            response.as_bytes(),
        )
        .await;
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "gate".to_owned(),
                "forwarded".to_owned(),
                "interim".to_owned(),
                "write".to_owned(),
                "close".to_owned()
            ]
        );
        assert_eq!(
            sink.captured_interim_responses(),
            vec![b"HTTP/1.1 100 Continue\r\n\r\n".to_vec()]
        );
        assert_eq!(
            client_received,
            response.as_bytes(),
            "the client receives exactly what the source sent"
        );
        // SourceInterimResponseCaptureTest:112-126: the interim and final bytes are counted
        // exactly once between them.
        assert_eq!(outcome.client_bytes_written, response.len() as u64);
    }

    #[tokio::test]
    async fn a_response_whose_capture_fails_never_reaches_the_client() {
        // SourceInterimResponseCaptureTest:167-186: a failing final-write capture under
        // FAIL_CLOSED terminates, the channel closes, and the client does not get those bytes
        // (`LHH:836-839` fails the write promise).
        let (client_side, client_proxy) = tokio::io::duplex(64 * 1024);
        let (source_proxy, source_side) = tokio::io::duplex(64 * 1024);
        let mut failing = RecordingSink::new();
        // Deterministic: every response-byte capture fails, so the failure necessarily lands on
        // the response path and not on the request's own read.
        failing.fail_write_capture();
        let sink = Arc::new(Mutex::new(Some(failing)));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        let source = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(source_side);
            let mut request = vec![0u8; GET.len()];
            reader.read_exact(&mut request).await.expect("the request");
            writer
                .write_all(RESPONSE.as_bytes())
                .await
                .expect("respond");
            let _ = writer.flush().await;
            let _ = writer.shutdown().await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client_side);
        client_writer
            .write_all(GET.as_bytes())
            .await
            .expect("request");
        client_writer.flush().await.expect("flush");

        let mut received = Vec::new();
        let _ = client_reader.read_to_end(&mut received).await;
        let outcome = proxy.await.expect("no panic");
        source.await.expect("source task");

        assert_eq!(outcome.close_reason, CloseReason::CaptureFailure);
        assert!(
            received.is_empty(),
            "the client received {received:?} after the capture of those bytes failed"
        );
        assert_eq!(outcome.client_bytes_written, 0);
        assert!(outcome.capture_abort.is_some());
    }

    #[tokio::test]
    async fn held_response_bytes_are_flushed_as_a_write_before_the_terminal_close() {
        // Spec §7.5.4 and §12.1: an unclassifiable prefix is never lost, and it is emitted *before*
        // the close observation. SourceInterimResponseCaptureTest:129-144 pins the same thing.
        let (outcome, sink, _, client_received) = run_connection(
            RecordingSink::new(),
            config(),
            GET.as_bytes(),
            b"HTTP/1.1 100 Cont",
        )
        .await;
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "close".to_owned(),
            ]
        );
        assert_eq!(sink.captured_writes(), vec![b"HTTP/1.1 100 Cont".to_vec()]);
        assert_eq!(
            client_received, b"HTTP/1.1 100 Cont",
            "the held bytes still reached the client when they arrived"
        );
        assert_eq!(outcome.close_reason, CloseReason::Normal);
        assert_eq!(outcome.client_bytes_written, 17);
    }

    #[tokio::test]
    async fn an_ungated_build_forwards_a_mutation_without_waiting() {
        // The Java tests parameterise `blockOnCapture`; production always installs the predicate.
        let mut config = config();
        config.gate_critical_mutations = false;
        let (outcome, sink, source_received, _) = run_connection(
            RecordingSink::new(),
            config,
            POST.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;
        assert_eq!(sink.gate_calls(), 0);
        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "close".to_owned()
            ],
            "the end of message is still captured before the forward"
        );
        assert_eq!(source_received, POST.as_bytes());
        assert_eq!(outcome.requests_forwarded, 1);
    }

    #[tokio::test]
    async fn a_client_eof_closes_the_source_connection_too() {
        // Spec §12.2: no ALLOW_HALF_CLOSURE anywhere, so an inbound EOF is a full close and the
        // other side is closed after its pending writes drain (`FH:83-87`, `FH:104-110`).
        let (client_side, client_proxy) = tokio::io::duplex(64 * 1024);
        let (source_proxy, source_side) = tokio::io::duplex(64 * 1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        let source = tokio::spawn(async move {
            let (mut reader, _writer) = tokio::io::split(source_side);
            let mut request = vec![0u8; GET.len()];
            reader.read_exact(&mut request).await.expect("the request");
            // The client closes next; the proxy must close this side rather than leaving it open.
            let mut rest = Vec::new();
            let read = reader.read_to_end(&mut rest).await.expect("eof");
            (request, read)
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client_side);
        client_writer
            .write_all(GET.as_bytes())
            .await
            .expect("request");
        client_writer.flush().await.expect("flush");
        client_writer.shutdown().await.expect("client EOF");
        let mut drained = Vec::new();
        let _ = client_reader.read_to_end(&mut drained).await;

        let outcome = proxy.await.expect("no panic");
        let (request, trailing) = source.await.expect("source task");
        assert_eq!(request, GET.as_bytes());
        assert_eq!(trailing, 0, "the source side saw EOF, not more bytes");
        assert_eq!(outcome.close_reason, CloseReason::Normal);
    }

    #[tokio::test]
    async fn header_rewrites_reach_the_source_while_capture_keeps_the_original_bytes() {
        let mut config = config();
        config.rewrite = RewriteConfig::new()
            .set_header("host", "my.host")
            .expect("valid");
        let (_, sink, source_received, _) = run_connection(
            RecordingSink::new(),
            config,
            GET.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;
        assert_eq!(
            String::from_utf8(source_received).expect("utf8"),
            "GET /thing HTTP/1.1\r\nhost: my.host\r\n\r\n"
        );
        assert_eq!(
            String::from_utf8(sink.captured_reads().concat()).expect("utf8"),
            GET,
            "Kafka records the ORIGINAL client bytes"
        );
    }

    #[tokio::test]
    async fn a_connection_that_stopped_capturing_performs_no_terminal_capture() {
        // Pinned by failOpenTransitionsTheWholeConnectionToPassThrough (:977-981) and
        // diagnosticCaptureFailureEntersPassThroughAndClosesTheChannel (:529-561).
        let mut sink = RecordingSink::new();
        sink.stop_capturing();
        let (outcome, sink, source_received, client_received) =
            run_connection(sink, config(), POST.as_bytes(), RESPONSE.as_bytes()).await;
        assert!(sink.events().is_empty(), "{:?}", sink.events());
        assert_eq!(sink.close_reason(), None);
        assert_eq!(source_received, POST.as_bytes(), "forwarding continues");
        assert_eq!(client_received, RESPONSE.as_bytes());
        assert_eq!(outcome.requests_forwarded, 0);
    }

    #[tokio::test]
    async fn two_pipelined_requests_each_get_their_own_forward_in_order() {
        let stream = format!("{GET}{GET}");
        let (outcome, sink, source_received, _) = run_connection(
            RecordingSink::new(),
            config(),
            stream.as_bytes(),
            RESPONSE.as_bytes(),
        )
        .await;
        assert_eq!(source_received, stream.as_bytes());
        assert_eq!(
            sink.end_of_message_lengths(),
            vec![(19, 12), (19, 12)],
            "both requests committed an end of message"
        );
        assert_eq!(outcome.requests_forwarded, 2);
    }

    #[tokio::test]
    async fn each_request_on_a_connection_gets_its_own_forward_in_order() {
        // RequestForwardedCaptureTest:63-80 pins the sequence
        // [read, eom, forwarded, write, read, eom, forwarded] for GET, response, POST, complete.
        // The only addition here is the gate, which the POST must pass before it is forwarded.
        let (client_side, client_proxy) = tokio::io::duplex(64 * 1024);
        let (source_proxy, source_side) = tokio::io::duplex(64 * 1024);
        let sink = Arc::new(Mutex::new(Some(RecordingSink::new())));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        let source = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(source_side);
            let mut received = Vec::new();
            for expected in [GET.len(), POST.len()] {
                let mut buffer = vec![0u8; expected];
                reader
                    .read_exact(&mut buffer)
                    .await
                    .expect("the whole request arrives");
                received.extend_from_slice(&buffer);
                writer
                    .write_all(RESPONSE.as_bytes())
                    .await
                    .expect("respond");
                writer.flush().await.expect("flush");
            }
            let _ = writer.shutdown().await;
            received
        });

        let client = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(client_side);
            let mut received = Vec::new();
            for request in [GET, POST] {
                writer.write_all(request.as_bytes()).await.expect("write");
                writer.flush().await.expect("flush");
                let mut buffer = vec![0u8; RESPONSE.len()];
                reader
                    .read_exact(&mut buffer)
                    .await
                    .expect("the response arrives");
                received.extend_from_slice(&buffer);
            }
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest).await;
            received.extend(rest);
            received
        });

        let outcome = proxy.await.expect("no panic");
        let source_received = source.await.expect("source task");
        let client_received = client.await.expect("client task");
        let sink = sink
            .lock()
            .expect("not poisoned")
            .take()
            .expect("the sink is returned");

        assert_eq!(
            sink.events(),
            vec![
                "read".to_owned(),
                "eom".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "read".to_owned(),
                "eom".to_owned(),
                "gate".to_owned(),
                "forwarded".to_owned(),
                "write".to_owned(),
                "close".to_owned(),
            ]
        );
        assert_eq!(sink.end_of_message_lengths(), vec![(19, 12), (20, 17)]);
        assert_eq!(source_received, format!("{GET}{POST}").as_bytes());
        assert_eq!(client_received, format!("{RESPONSE}{RESPONSE}").as_bytes());
        assert_eq!(outcome.requests_forwarded, 2);
    }

    #[tokio::test]
    async fn a_request_arriving_one_byte_at_a_time_produces_the_same_capture_stream() {
        let (client_side, client_proxy) = tokio::io::duplex(64 * 1024);
        let (source_proxy, source_side) = tokio::io::duplex(64 * 1024);
        let sink = Arc::new(Mutex::new(Some(RecordingSink::new())));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        let source = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(source_side);
            let mut received = vec![0u8; POST.len()];
            reader
                .read_exact(&mut received)
                .await
                .expect("the whole request arrives");
            let _ = writer.shutdown().await;
            received
        });
        let client = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(client_side);
            for byte in POST.as_bytes() {
                writer.write_all(&[*byte]).await.expect("write");
                writer.flush().await.expect("flush");
                tokio::task::yield_now().await;
            }
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest).await;
        });

        let outcome = proxy.await.expect("no panic");
        let source_received = source.await.expect("source task");
        client.await.expect("client task");
        let sink = sink
            .lock()
            .expect("not poisoned")
            .take()
            .expect("the sink is returned");

        assert_eq!(
            source_received,
            POST.as_bytes(),
            "every byte reaches the source, in order"
        );
        let events = sink.events();
        // One read observation per read, then exactly one end of message, gate, and forward.
        assert_eq!(events.iter().filter(|event| *event == "eom").count(), 1);
        assert_eq!(events.iter().filter(|event| *event == "gate").count(), 1);
        assert_eq!(
            events.iter().filter(|event| *event == "forwarded").count(),
            1
        );
        let gate = events
            .iter()
            .position(|event| event == "gate")
            .expect("gate");
        let eom = events.iter().position(|event| event == "eom").expect("eom");
        let forwarded = events
            .iter()
            .position(|event| event == "forwarded")
            .expect("forwarded");
        assert!(eom < gate && gate < forwarded, "{events:?}");
        assert_eq!(
            String::from_utf8(sink.captured_reads().concat()).expect("utf8"),
            POST,
            "the concatenated reads reproduce the request exactly"
        );
        assert_eq!(sink.end_of_message_lengths(), vec![(20, 17)]);
        assert_eq!(outcome.requests_forwarded, 1);
    }

    #[tokio::test]
    async fn a_fast_source_cannot_outrun_a_slow_client() {
        // Spec §13-D4, and the T10 addition the spec asks for. Java has no backpressure in this
        // direction at all: `FH:70` makes the target channel auto-read, nothing consults
        // writability, and no watermarks are configured, so a fast source with a slow client grows
        // the client channel's outbound buffer until the container dies of OOM. Under this crate's
        // bidirectional credit-of-one the source simply cannot get that far ahead.
        const PAYLOAD: usize = 1024 * 1024;
        const SOURCE_PIPE: usize = 4096;
        const CLIENT_PIPE: usize = 4096;

        let (client_side, client_proxy) = tokio::io::duplex(CLIENT_PIPE);
        let (source_proxy, source_side) = tokio::io::duplex(SOURCE_PIPE);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = tokio::spawn({
            let pushed = Arc::clone(&pushed);
            async move {
                let (mut reader, mut writer) = tokio::io::split(source_side);
                let mut request = vec![0u8; GET.len()];
                reader.read_exact(&mut request).await.expect("the request");
                let chunk = vec![b'z'; 4096];
                while pushed.load(std::sync::atomic::Ordering::SeqCst) < PAYLOAD {
                    writer.write_all(&chunk).await.expect("push");
                    pushed.fetch_add(chunk.len(), std::sync::atomic::Ordering::SeqCst);
                }
                let _ = writer.shutdown().await;
            }
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client_side);
        client_writer
            .write_all(GET.as_bytes())
            .await
            .expect("request");
        client_writer.flush().await.expect("flush");

        // The client reads nothing for a while. Everything the source managed to push must be
        // sitting in the two pipes and the proxy's single read buffer — not in an unbounded proxy
        // buffer.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let in_flight = pushed.load(std::sync::atomic::Ordering::SeqCst);
        let ceiling = SOURCE_PIPE + CLIENT_PIPE + DEFAULT_READ_BUFFER_BYTES + 4096;
        assert!(
            in_flight <= ceiling,
            "the source pushed {in_flight} bytes while the client read none; \
             bidirectional backpressure should have capped it near {ceiling}"
        );

        // Draining must still be lossless.
        let drained = tokio::spawn(async move {
            let mut total = 0usize;
            let mut buffer = vec![0u8; 8192];
            loop {
                match client_reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => total += read,
                }
            }
            total
        });
        source.await.expect("source task");
        let total = drained.await.expect("drain task");
        let outcome = proxy.await.expect("no panic");
        assert!(
            total >= PAYLOAD,
            "the client received {total} of at least {PAYLOAD} bytes"
        );
        assert_eq!(
            outcome.client_bytes_written, outcome.source_bytes_read,
            "every byte read from the source reached the client"
        );
    }

    #[tokio::test]
    async fn the_connection_lifetime_deadline_closes_an_idle_connection() {
        let mut config = config();
        config.limits = RequestLimits::new(
            std::time::Duration::from_mins(1),
            1024,
            1024,
            std::time::Duration::from_millis(20),
            None,
        )
        .expect("valid limits");

        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, _source_side) = tokio::io::duplex(1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config,
        );
        let outcome = connection.run(None).await;
        assert_eq!(outcome.close_reason, CloseReason::ConnectionDuration);
        drop(client_side);
    }

    #[tokio::test]
    async fn the_assembly_deadline_is_anchored_at_the_first_byte_and_not_extended_by_progress() {
        // ConditionallyReliableLoggingHttpHandlerTest:188-231: a 10 ms limit, a byte, +6 ms,
        // another byte, +5 ms, and the connection must be closed.
        let mut config = config();
        config.limits = RequestLimits::new(
            std::time::Duration::from_millis(60),
            1024,
            1024,
            std::time::Duration::from_mins(1),
            None,
        )
        .expect("valid limits");

        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, source_side) = tokio::io::duplex(1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config,
        );
        let proxy = tokio::spawn(connection.run(None));
        let client = tokio::spawn(async move {
            let (_reader, mut writer) = tokio::io::split(client_side);
            writer
                .write_all(b"POST / HTTP/1.1\r\nContent-Length: 100\r\n\r\nx")
                .await
                .expect("write");
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            // Progress does not extend the deadline.
            let _ = writer.write_all(b"y").await;
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        });
        let outcome = proxy.await.expect("no panic");
        client.abort();
        drop(source_side);
        assert_eq!(outcome.close_reason, CloseReason::AssemblyDuration);
        assert_eq!(outcome.requests_completed, 0);
    }

    #[tokio::test]
    async fn a_completed_request_cancels_its_assembly_deadline() {
        let mut config = config();
        config.limits = RequestLimits::new(
            std::time::Duration::from_millis(50),
            1024,
            1024,
            std::time::Duration::from_mins(1),
            None,
        )
        .expect("valid limits");

        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, source_side) = tokio::io::duplex(1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config,
        );
        let proxy = tokio::spawn(connection.run(None));
        let client = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(client_side);
            writer.write_all(GET.as_bytes()).await.expect("write");
            // Idle far longer than the assembly limit with no request in progress.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let _ = writer.shutdown().await;
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest).await;
        });
        let outcome = proxy.await.expect("no panic");
        client.abort();
        drop(source_side);
        assert_eq!(
            outcome.close_reason,
            CloseReason::Normal,
            "an idle connection with no request in progress is not bounded by the assembly limit"
        );
    }

    #[tokio::test]
    async fn a_disconnect_request_closes_the_connection() {
        let shutdown = Arc::new(ConnectionShutdown::default());
        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, source_side) = tokio::io::duplex(1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            RecordingSink::new(),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(Some(Arc::clone(&shutdown))));
        tokio::task::yield_now().await;
        shutdown.trigger();
        let outcome = proxy.await.expect("no panic");
        assert_eq!(outcome.close_reason, CloseReason::Disconnected);
        drop(client_side);
        drop(source_side);
    }

    #[tokio::test]
    async fn a_byte_limit_violation_captures_the_offending_read_and_forwards_nothing() {
        // ConditionallyReliableLoggingHttpHandlerTest:334-366: a complete request over the total
        // budget is dropped, not forwarded, and the capture stream still shows the read.
        let mut config = config();
        config.limits = RequestLimits::new(
            std::time::Duration::from_mins(1),
            128,
            150,
            std::time::Duration::from_mins(1),
            None,
        )
        .expect("valid limits");
        let request = format!(
            "POST / HTTP/1.1\r\nContent-Length: 200\r\n\r\n{}",
            "x".repeat(200)
        );
        let (outcome, sink, source_received, _) =
            run_connection(RecordingSink::new(), config, request.as_bytes(), b"").await;
        assert_eq!(outcome.close_reason, CloseReason::TotalBytes);
        assert!(
            source_received.is_empty(),
            "the source received {} bytes",
            source_received.len()
        );
        assert_eq!(
            sink.events(),
            vec!["read".to_owned(), "close".to_owned()],
            "a limit violation is a bare close with no exception observation"
        );
        assert_eq!(sink.close_reason(), Some(CloseReason::TotalBytes));
    }

    // ---------------------------------------------------------------- the F flush driver (§4.2)

    /// Yields enough times for the single-threaded paused runtime to run the proxy task to its
    /// next await point without advancing the clock (a ready task prevents auto-advance).
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    fn events_of(sink: &Arc<Mutex<Option<RecordingSink>>>) -> Vec<String> {
        sink.lock()
            .expect("not poisoned")
            .as_ref()
            .expect("present")
            .events()
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_flushes_its_buffered_record_at_exactly_the_interval() {
        // proxyCaptureProtocol.md §4.2: the scheduled callback must detach and enqueue an
        // otherwise-idle nonempty record. The append-side deadline check cannot do this — no
        // further observation ever arrives — so this is the test that fails when the driver is
        // missing, which is exactly the prior rewrite's owner-review blocker #3.
        const F: std::time::Duration = std::time::Duration::from_secs(5);
        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, source_side) = tokio::io::duplex(1024);
        let sink = Arc::new(Mutex::new(Some(
            RecordingSink::new().with_flush_interval(F),
        )));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));
        let start = tokio::time::Instant::now().into_std();

        // A partial head: one captured read, no end of message, then silence.
        let (mut client_reader, mut client_writer) = tokio::io::split(client_side);
        client_writer
            .write_all(b"GET /incomplete HTT")
            .await
            .expect("partial head");
        client_writer.flush().await.expect("flush");
        settle().await;
        assert_eq!(events_of(&sink), vec!["read".to_owned()]);

        // One virtual millisecond before the deadline: nothing has flushed.
        tokio::time::sleep(
            F.checked_sub(std::time::Duration::from_millis(1))
                .expect("F is seconds"),
        )
        .await;
        settle().await;
        assert_eq!(events_of(&sink), vec!["read".to_owned()]);

        // Crossing the deadline publishes the idle record, at exactly F.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        settle().await;
        assert_eq!(
            events_of(&sink),
            vec!["read".to_owned(), "flush".to_owned()]
        );
        {
            let guard = sink.lock().expect("not poisoned");
            let recording = guard.as_ref().expect("present");
            assert_eq!(
                recording.flush_times(),
                vec![start + F],
                "the flush ran at the record's fixed deadline, not when traffic resumed"
            );
            assert_eq!(
                recording.next_flush_deadline(),
                None,
                "the detached record leaves no pending deadline"
            );
        }

        client_writer.shutdown().await.expect("client EOF");
        let mut rest = Vec::new();
        let _ = client_reader.read_to_end(&mut rest).await;
        let outcome = proxy.await.expect("no panic");
        assert_eq!(outcome.close_reason, CloseReason::Normal);
        assert_eq!(
            events_of(&sink),
            vec!["read".to_owned(), "flush".to_owned(), "close".to_owned()]
        );
        drop(source_side);
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_traffic_does_not_postpone_the_flush_deadline() {
        // §4.2: the deadline is fixed by the record's first observation; a connection that never
        // goes quiet still publishes at F. A last-activity-plus-F timer would fail this test.
        const F: std::time::Duration = std::time::Duration::from_secs(5);
        let (client_side, client_proxy) = tokio::io::duplex(4096);
        let (source_proxy, source_side) = tokio::io::duplex(4096);
        let sink = Arc::new(Mutex::new(Some(
            RecordingSink::new().with_flush_interval(F),
        )));
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            SinkHandle(Arc::clone(&sink)),
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));
        let start = tokio::time::Instant::now().into_std();

        // One byte of an ever-growing request line every second, for eight seconds.
        let (mut client_reader, mut client_writer) = tokio::io::split(client_side);
        client_writer.write_all(b"GET /a").await.expect("write");
        client_writer.flush().await.expect("flush");
        settle().await;
        for _ in 0..8 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            settle().await;
            client_writer.write_all(b"a").await.expect("write");
            client_writer.flush().await.expect("flush");
            settle().await;
        }

        {
            let guard = sink.lock().expect("not poisoned");
            let recording = guard.as_ref().expect("present");
            let flush_times = recording.flush_times();
            assert_eq!(
                flush_times.first().copied(),
                Some(start + F),
                "the first flush ran at the FIRST observation's deadline despite continuous traffic"
            );
            let events = recording.events();
            let flush_index = events
                .iter()
                .position(|event| event == "flush")
                .expect("a flush happened");
            let reads_before = events[..flush_index]
                .iter()
                .filter(|event| *event == "read")
                .count();
            let reads_after = events[flush_index..]
                .iter()
                .filter(|event| *event == "read")
                .count();
            assert!(
                reads_before >= 4,
                "traffic kept arriving before the flush: {events:?}"
            );
            assert!(
                reads_after >= 1,
                "traffic kept arriving after the flush, into the successor record: {events:?}"
            );
        }

        client_writer.shutdown().await.expect("client EOF");
        let mut rest = Vec::new();
        let _ = client_reader.read_to_end(&mut rest).await;
        let _ = proxy.await.expect("no panic");
        drop(source_side);
    }

    #[tokio::test]
    async fn a_stale_flush_callback_after_detach_is_a_no_op() {
        let mut sink = RecordingSink::new().with_flush_interval(std::time::Duration::from_secs(5));
        sink.capture_read(SystemTime::UNIX_EPOCH, b"x")
            .expect("captured");
        assert!(sink.next_flush_deadline().is_some());
        sink.run_scheduled_flush(SystemTime::UNIX_EPOCH)
            .expect("flush");
        assert_eq!(sink.events(), vec!["read".to_owned(), "flush".to_owned()]);
        // The record detached; a timer that lost the race fires anyway and must do nothing.
        sink.run_scheduled_flush(SystemTime::UNIX_EPOCH)
            .expect("a stale callback is a no-op");
        assert_eq!(sink.events(), vec!["read".to_owned(), "flush".to_owned()]);
        assert_eq!(sink.next_flush_deadline(), None);
    }

    #[tokio::test]
    async fn a_close_before_the_flush_deadline_cancels_the_pending_flush() {
        // The terminal close detaches and publishes the final record itself; a scheduled flush
        // after it must have nothing to fire on. The wire layer invalidates the deadline in
        // `ConnectionRecorder::close`; this asserts the transport side observes that.
        let sink = RecordingSink::new().with_flush_interval(std::time::Duration::from_mins(5));
        let (outcome, sink, _, _) =
            run_connection(sink, config(), GET.as_bytes(), RESPONSE.as_bytes()).await;
        assert!(
            !sink.events().contains(&"flush".to_owned()),
            "{:?}",
            sink.events()
        );
        assert_eq!(
            sink.next_flush_deadline(),
            None,
            "close invalidated the pending deadline"
        );
        assert_eq!(outcome.close_reason, CloseReason::Normal);
    }

    /// A sink that implements only the required methods, pinning the trait defaults: no deadline,
    /// and a spurious scheduled flush is an `Ok` no-op. This is what keeps the no-capture sink and
    /// every not-yet-wired implementation compiling and correct.
    struct ReactiveOnlySink;

    impl CaptureSink for ReactiveOnlySink {
        fn should_capture(&self) -> bool {
            false
        }
        fn capture_read(&mut self, _at: SystemTime, _bytes: &[u8]) -> Result<(), CaptureAbort> {
            Ok(())
        }
        fn request_dropped(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            Ok(())
        }
        fn set_end_of_message_lengths(&mut self, _first_line: u32, _headers: u32) {}
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
        fn gate_critical_mutation(&mut self) -> impl Future<Output = GateOutcome> + Send {
            std::future::ready(GateOutcome::Forward)
        }
    }

    #[test]
    fn a_sink_relying_on_the_defaults_never_produces_a_deadline() {
        let mut sink = ReactiveOnlySink;
        assert_eq!(sink.next_flush_deadline(), None);
        sink.run_scheduled_flush(SystemTime::UNIX_EPOCH)
            .expect("the default scheduled flush is an Ok no-op");
    }
    /// A capturing sink whose terminal acknowledgement arrives only when the test releases it.
    struct TerminalAckGatedSink {
        closed: Arc<std::sync::atomic::AtomicBool>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    impl CaptureSink for TerminalAckGatedSink {
        fn should_capture(&self) -> bool {
            true
        }
        fn capture_read(&mut self, _at: SystemTime, _bytes: &[u8]) -> Result<(), CaptureAbort> {
            Ok(())
        }
        fn request_dropped(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
            Ok(())
        }
        fn set_end_of_message_lengths(&mut self, _first_line: u32, _headers: u32) {}
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
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn gate_critical_mutation(&mut self) -> impl Future<Output = GateOutcome> + Send {
            std::future::ready(GateOutcome::Forward)
        }
        fn await_terminal_acknowledgement(
            &mut self,
        ) -> impl Future<Output = Result<(), CaptureAbort>> + Send {
            let release = self.release.take();
            async move {
                if let Some(release) = release {
                    let _ = release.await;
                }
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn the_connection_owner_waits_for_the_terminal_acknowledgement() {
        // proxyCaptureProtocol.md 5.1 steps 4-5. Gap A: the owner used to exit right after
        // close(), stranding a terminal record queued behind an unacknowledged one, so the
        // connection was never deregistered and its writer lane could never retire.
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (client_side, client_proxy) = tokio::io::duplex(1024);
        let (source_proxy, source_side) = tokio::io::duplex(1024);
        let connection = Connection::new(
            client_proxy,
            source_proxy,
            TerminalAckGatedSink {
                closed: Arc::clone(&closed),
                release: Some(release_rx),
            },
            SuppressionPolicy::permit_everything(),
            config(),
        );
        let proxy = tokio::spawn(connection.run(None));

        // The client goes away; the connection closes normally.
        drop(client_side);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !closed.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the terminal close observation is recorded");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !proxy.is_finished(),
            "the connection owner must not exit before the terminal record is acknowledged"
        );

        release_tx.send(()).expect("the owner is still waiting");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), proxy)
            .await
            .expect("the owner exits once the acknowledgement arrives")
            .expect("no panic");
        assert!(outcome.capture_abort.is_none());
        drop(source_side);
    }
}
