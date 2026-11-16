//! The capture-proxy data plane: listener, source acquisition, and the two byte-exact forwarding
//! pumps, with write-driven backpressure in **both** directions.
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`listener`] | PROXY-11 | bind, accept, worker sizing intent, active-connection registry, stop-accepting, disconnect-all, wait-for-close |
//! | [`connection`] | PROXY-08 | one connection's whole lifecycle: both pumps, deadlines, close reasons, teardown ordering |
//! | [`request_pump`] | PROXY-08 | client to source: scan, capture, gate, rewrite, forward, read credit |
//! | [`response_pump`] | PROXY-06 | source to client: classify, capture, forward, read credit |
//! | [`write_completion`] | HTTP-09 | `requestForwarded`, emitted only after the outbound write succeeds |
//!
//! # Where capture lives
//!
//! This crate owns I/O, framing invocation, ordering, and flow control. It does **not** own the
//! capture stream: every capture side effect goes through the [`CaptureSink`] trait, which the
//! process-level wiring implements against the wire layer. That boundary is what keeps the
//! capture-before-forward gate explicit — [`CaptureSink::gate_critical_mutation`] is the only
//! `await` in the request path that can refuse a forward.
//!
//! # Backpressure: a deliberate divergence
//!
//! Java's backpressure is one-directional. Client to source is a clean credit-of-one: the accepted
//! channel has `AUTO_READ=false` (`NSHP:58`) and a new read is granted only from the success branch
//! of the write-to-source future (`FH:56-60`), which is exactly what the capture gate depends on.
//! Source to client has none at all: `FH:70` flips the target channel to `setAutoRead(true)` on the
//! first client read, which makes `BCP:92`'s `AUTO_READ=false` dead configuration, `BH:25` never
//! consults writability, and no write watermarks are configured anywhere, so Netty's default 32/64
//! KiB marks only toggle a flag nothing reads. A slow client with a fast source grows the client
//! channel's outbound buffer without bound, and `pendingSourceResponseBytes` is an unbounded hold
//! buffer on top of it (spec §9.2, §13-D4, §13-D11).
//!
//! This crate implements **bidirectional** credit-of-one: in each direction, the next read is
//! issued only after the previous chunk has been accepted by the opposite socket. Look for
//! `// DEVIATION:` at the two sites. Memory per connection is then bounded by one read buffer per
//! direction plus two small, bounded holds (the scanner's head buffer and the response
//! classifier's).
//!
//! # Exposure
//!
//! [`listener::ListenerConfig`] defaults to a **loopback** bind. Java binds the wildcard address
//! (`NSHP.start` calls `bind(int port)`, and `CaptureProxy` has no bind-address parameter), which is
//! the right shape for an in-cluster deployment but is an exposure decision rather than an
//! implementation detail — the spec flags it at §1.1 as something the owner should ratify
//! explicitly rather than inherit. Choosing it here therefore requires saying so:
//! [`listener::ListenerConfig::with_bind_address`]. The listener itself performs no client
//! authentication; front mTLS ([`tls::FrontTlsConfig`]) is the only caller gate the proxy has.

pub mod connection;
pub mod listener;
pub mod preconnect_pool;
pub mod request_pump;
pub mod response_pump;
pub mod tls;
pub mod write_completion;

use std::time::SystemTime;

use capture_http_scan::request_scanner::{FramingError, LimitViolation, ScanError};

pub use connection::{Connection, ConnectionConfig, ConnectionOutcome};
pub use listener::{ConnectionRegistry, Listener, ListenerConfig, worker_thread_count};
pub use write_completion::ForwardCompletion;

/// Why a connection closed.
///
/// The first five are Java's `CaptureConnectionMetrics.CloseReason`
/// (`CaptureConnectionMetrics.java:19-26`). The rest exist because owner decision OD-1 makes
/// malformed framing an observable, terminal event instead of a silent tunnel: Java has no close
/// reason for any of them because it never closes for any of them.
///
/// Exactly one reason is reported per connection: the latch is first-writer-wins, like
/// `markCloseReason` (`LHH:758-764`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// An ordinary close: either peer finished.
    Normal,
    /// The request-assembly deadline fired.
    AssemblyDuration,
    /// One request's header bytes exceeded their budget.
    HeaderBytes,
    /// One request's total bytes exceeded their budget.
    TotalBytes,
    /// The connection-lifetime deadline fired.
    ConnectionDuration,
    /// Required capture failed and the policy refused to forward.
    CaptureFailure,
    /// OD-1: the request line exceeded its configured budget.
    ///
    /// Java leaves Netty's `maxInitialLineLength` at 4096 and does not recognise
    /// `TooLongHttpLineException`, so it tunnels the request instead (spec §3.7).
    RequestLineBytes,
    /// OD-1: framing the proxy could not determine, including a bare LF in strict mode, a
    /// `Transfer-Encoding`/`Content-Length` conflict, a duplicate `Content-Length`, and every chunk
    /// framing error.
    MalformedFraming,
    /// The source's response could not be classified within the bounded hold (spec §13-D11).
    MalformedResponse,
    /// The client's socket failed while being read.
    ClientIoError,
    /// A write to the client failed.
    ///
    /// Distinct from [`CloseReason::ClientIoError`] because Java's paths differ: an inbound failure
    /// becomes a pipeline exception and therefore an observation, while a failed write back to the
    /// client closes both channels silently (`BH:26-33`, pinned by
    /// `targetToClientWriteFailureClosesBothChannelsWithTerminalCapture`).
    ClientWriteFailure,
    /// The source's socket failed.
    SourceIoError,
    /// The owner asked for this connection to be disconnected.
    Disconnected,
}

impl CloseReason {
    /// The lowercase label the operational log line carries.
    ///
    /// Java's `event=capture_connection_dropped reason=<lowercase>` line (`LHH:596-607`), whose
    /// exact spelling the Java test asserts (`assertConnectionDropEvent`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::AssemblyDuration => "assembly_duration",
            Self::HeaderBytes => "header_bytes",
            Self::TotalBytes => "total_bytes",
            Self::ConnectionDuration => "connection_duration",
            Self::CaptureFailure => "capture_failure",
            Self::RequestLineBytes => "request_line_bytes",
            Self::MalformedFraming => "malformed_framing",
            Self::MalformedResponse => "malformed_response",
            Self::ClientIoError => "client_io_error",
            Self::ClientWriteFailure => "client_write_failure",
            Self::SourceIoError => "source_io_error",
            Self::Disconnected => "disconnected",
        }
    }

    /// Whether this close emits a `connectionException` observation before the terminal `close`.
    ///
    /// Java emits one for any `Throwable` that reaches `LoggingHttpHandler.exceptionCaught`, which
    /// means client-side pipeline failures but **not** limit violations, **not** source-side
    /// failures (`BH:50-56` closes the client channel rather than firing an exception on it), and
    /// **not** decoder failures, which never become pipeline exceptions at all (spec §12.3).
    ///
    /// OD-1 adds the framing reasons to the list: a refusal nobody can see would be exactly the
    /// silent capture gap the protocol document requires the proxy to alarm about.
    #[must_use]
    pub fn emits_connection_exception(self) -> bool {
        matches!(
            self,
            Self::RequestLineBytes | Self::MalformedFraming | Self::ClientIoError
        )
    }
}

impl From<&ScanError> for CloseReason {
    fn from(error: &ScanError) -> Self {
        match error {
            ScanError::Limit(LimitViolation::HeaderBytes { .. }) => Self::HeaderBytes,
            ScanError::Limit(LimitViolation::TotalBytes { .. }) => Self::TotalBytes,
            ScanError::Framing(FramingError::RequestLineTooLong { .. }) => Self::RequestLineBytes,
            ScanError::Framing(_) | ScanError::AlreadyFaulted => Self::MalformedFraming,
        }
    }
}

/// Required capture failed terminally: the caller must forward nothing further and close.
///
/// This is `runRequiredCaptureOperation` returning `false` (`LHH:962-981`), which happens only when
/// the capture-failure policy resolved to `TERMINATING`. Under `FAIL_OPEN` a sink instead returns
/// `Ok` and stops capturing, so the transport keeps forwarding uncaptured — the decision belongs to
/// the capture layer, not here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("required capture failed terminally: {message}")]
pub struct CaptureAbort {
    /// What failed, for the operational log.
    pub message: String,
}

impl CaptureAbort {
    /// Builds an abort.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// What the capture-before-forward gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// Publication is durable (or the policy permits forwarding uncaptured). Forward the request.
    Forward,
    /// Forwarding is refused. The caller closes with [`CloseReason::CaptureFailure`] and the
    /// request never reaches the source.
    Refuse,
}

/// Every capture side effect the data plane can have.
///
/// Implemented by the process wiring over the wire layer's per-connection recorder. The methods are
/// in the order the request path calls them, and the order is itself part of the contract: the
/// capture stream's legal orderings are asserted by the replayer.
///
/// `at` is always a wall-clock observation timestamp (`Instant.now()` in Java). Deadlines use a
/// monotonic clock instead and never appear here.
pub trait CaptureSink: Send {
    /// Whether the process is still capturing at all.
    ///
    /// False after an irreversible transition to pass-through, where teardown must perform **no**
    /// terminal capture (`LHH:352-355`).
    fn should_capture(&self) -> bool;

    /// A `read` observation: the raw client bytes of one read, pre-rewrite.
    ///
    /// # Errors
    ///
    /// [`CaptureAbort`] when required capture failed terminally.
    fn capture_read(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort>;

    /// A `requestDropped` observation: capture for the in-flight request is abandoned after a
    /// prefix of it was already committed.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn request_dropped(&mut self, at: SystemTime) -> Result<(), CaptureAbort>;

    /// Records the two `EndOfMessageIndication` lengths for the request being assembled.
    fn set_end_of_message_lengths(&mut self, first_line_byte_length: u32, headers_byte_length: u32);

    /// An `endOfMessageIndicator` observation: the replayer's only request-completion trigger.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn commit_end_of_message(&mut self, at: SystemTime) -> Result<(), CaptureAbort>;

    /// A `requestForwarded` observation.
    ///
    /// Emitted only after the outbound write to the source **succeeded**, and before the next read
    /// is granted. See [`write_completion`].
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn request_forwarded(&mut self, at: SystemTime) -> Result<(), CaptureAbort>;

    /// A `write` observation: response bytes handed to the client, pre-TLS.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn capture_write(&mut self, at: SystemTime, bytes: &[u8]) -> Result<(), CaptureAbort>;

    /// An `interimResponse` observation: one complete non-switching 1xx head.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn capture_interim_response(
        &mut self,
        at: SystemTime,
        bytes: &[u8],
    ) -> Result<(), CaptureAbort>;

    /// A `connectionException` observation. Diagnostic and non-terminal; the terminal `close`
    /// still follows.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn connection_exception(&mut self, at: SystemTime, message: &str) -> Result<(), CaptureAbort>;

    /// The terminal `close` observation and its final publication.
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn close(&mut self, at: SystemTime, reason: CloseReason) -> Result<(), CaptureAbort>;

    /// The fixed monotonic deadline of the **current nonempty record**, or `None` when no record
    /// is open.
    ///
    /// `proxyCaptureProtocol.md` §4.2: the flush deadline is one fixed monotonic deadline, started
    /// by the first observation of a new nonempty record, and **continuous traffic cannot postpone
    /// it**. The value therefore changes only when a record opens (a new deadline) or detaches
    /// (`None`, or the successor record's deadline), which is why the connection task re-queries it
    /// after every sink call rather than caching it.
    ///
    /// The default is `None`: a sink with no record deadline — the no-capture sink, or any sink
    /// that has not been wired to the record layer yet — is simply never scheduled.
    fn next_flush_deadline(&self) -> Option<std::time::Instant> {
        None
    }

    /// Runs the owner's scheduled record flush: detaches and submits the open record **iff** its
    /// deadline generation is still current.
    ///
    /// This is the driver that makes `--traffic-stream-flush-interval-seconds` real for an
    /// otherwise-idle connection: the record layer's append-side deadline check can only fire when
    /// another observation arrives, so a buffered record on an idle connection would otherwise sit
    /// unpublished forever. The generation guard lives in the record layer
    /// (`ConnectionRecorder::run_scheduled_flush`), which makes a stale callback a no-op — this
    /// method may be called spuriously without harm, and it never creates a record, so a stale
    /// timer cannot resurrect a connection after its terminal close.
    ///
    /// The default is a no-op `Ok`, paired with the `None` default of
    /// [`CaptureSink::next_flush_deadline`].
    ///
    /// # Errors
    ///
    /// As [`CaptureSink::capture_read`].
    fn run_scheduled_flush(&mut self, _at: SystemTime) -> Result<(), CaptureAbort> {
        Ok(())
    }

    /// The capture-before-forward gate for Critical Mutation Traffic.
    ///
    /// Called after the end-of-message observation is committed and **before** the bytes that
    /// complete the request are written to the source. It must not return [`GateOutcome::Forward`]
    /// until publication is durably acknowledged and the acknowledgement has been validated. There
    /// is no timeout here, matching Java: the only bounds are the assembly and connection
    /// deadlines, which close the connection.
    fn gate_critical_mutation(&mut self) -> impl Future<Output = GateOutcome> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_reason_labels_match_the_java_metric_values() {
        // The five Java reasons keep Java's spelling, which its own log assertion pins.
        assert_eq!(CloseReason::Normal.label(), "normal");
        assert_eq!(CloseReason::AssemblyDuration.label(), "assembly_duration");
        assert_eq!(CloseReason::HeaderBytes.label(), "header_bytes");
        assert_eq!(CloseReason::TotalBytes.label(), "total_bytes");
        assert_eq!(
            CloseReason::ConnectionDuration.label(),
            "connection_duration"
        );
        assert_eq!(CloseReason::CaptureFailure.label(), "capture_failure");
    }

    #[test]
    fn scan_errors_map_to_their_close_reasons() {
        assert_eq!(
            CloseReason::from(&ScanError::Limit(LimitViolation::HeaderBytes { limit: 1 })),
            CloseReason::HeaderBytes
        );
        assert_eq!(
            CloseReason::from(&ScanError::Limit(LimitViolation::TotalBytes { limit: 1 })),
            CloseReason::TotalBytes
        );
        assert_eq!(
            CloseReason::from(&ScanError::Framing(FramingError::RequestLineTooLong {
                limit: 4096
            })),
            CloseReason::RequestLineBytes
        );
        assert_eq!(
            CloseReason::from(&ScanError::Framing(FramingError::BareLineFeed)),
            CloseReason::MalformedFraming
        );
        assert_eq!(
            CloseReason::from(&ScanError::Framing(
                FramingError::TransferEncodingWithContentLength
            )),
            CloseReason::MalformedFraming
        );
    }

    #[test]
    fn only_framing_and_client_failures_emit_an_exception_observation() {
        // Java parity: a limit violation is a bare close with no exception observation, and a
        // source-side failure closes the client channel without firing an exception on it.
        assert!(!CloseReason::HeaderBytes.emits_connection_exception());
        assert!(!CloseReason::TotalBytes.emits_connection_exception());
        assert!(!CloseReason::AssemblyDuration.emits_connection_exception());
        assert!(!CloseReason::SourceIoError.emits_connection_exception());
        assert!(!CloseReason::Normal.emits_connection_exception());
        // OD-1 requires the refusal to be observable.
        assert!(CloseReason::MalformedFraming.emits_connection_exception());
        assert!(CloseReason::RequestLineBytes.emits_connection_exception());
        assert!(CloseReason::ClientIoError.emits_connection_exception());
    }
}
