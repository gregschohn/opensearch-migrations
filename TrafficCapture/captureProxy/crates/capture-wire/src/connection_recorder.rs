//! Per-connection capture recorder: the observation catalogue, its legal ordering, the record
//! lifecycle, and the flush deadline (traceability rows WIRE-03 and WIRE-08).
//!
//! # Threading
//!
//! A recorder is owned by exactly one connection and is not `Sync`. Java enforces the same thing at
//! runtime with `requireConnectionEventLoopOwner`; here `&mut self` does it at compile time.
//!
//! # No timer lives here
//!
//! The recorder holds the flush **deadline**, not a timer. The connection's owner reads
//! [`ConnectionRecorder::flush_deadline`], arms whatever scheduler it uses, and calls
//! [`ConnectionRecorder::run_scheduled_flush`] when it fires. That keeps the three rules of
//! `proxyCaptureProtocol.md` §4.2 checkable without a thread:
//!
//! * the deadline is one FIXED monotonic deadline started by the first observation of a new nonempty
//!   record, and is never extended by later activity (step 1 and 3);
//! * an earlier detachment invalidates the deadline, and a stale callback must not flush or create a
//!   successor record (step 7) — enforced with a generation counter;
//! * no timer exists for an empty record and an idle connection emits no empty `TrafficStream`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::WireError;
use crate::protobuf_size::{self as size, CaptureTimestamp, field};
use crate::publish_chain::{PublishChain, PublishFailure, PublishOutcome, PublishTicket};
use crate::record_buffer::EncodedRecord;
use crate::record_encoder::{
    MAX_EXCEPTION_MESSAGE_BYTES, ObservationBody, ObservationPlan, RecordEncoder, RecordHeader,
};
use crate::record_sink::RecordSink;
use crate::segmentation::{ByteObservationKind, max_segment_data_len, segment_record_cost};

/// Production default for `trafficStreamFlushInterval` (`F`).
///
/// Five seconds, matching `KafkaCaptureFactory.DEFAULT_TRAFFIC_STREAM_FLUSH_INTERVAL` and
/// `proxyCaptureProtocol.md` §4.1.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Sentinel the capture layer writes into `EndOfMessageIndication` when the HTTP decoder result is
/// unavailable.
///
/// Java writes both fields unconditionally, so an unknown length becomes a *present* `-1` costing
/// ten varint bytes (wire spec §5.7, defect J-6). The replayer never reads either field. This crate
/// reproduces Java's bytes rather than introducing an unrequested divergence; the decision to leave
/// the fields absent instead belongs to the HTTP layer that supplies the values.
pub const UNKNOWN_MESSAGE_LENGTH: i32 = -1;

/// Monotonic clock used for the record flush deadline only.
///
/// Never for an observation `ts`: observation timestamps are wall-clock
/// ([`CaptureTimestamp::now`]). Wall-clock changes cannot move a flush deadline
/// (`proxyCaptureProtocol.md` §4.1).
pub trait MonotonicClock: std::fmt::Debug + Send + Sync {
    /// Nanoseconds elapsed on a monotonic timeline whose origin is arbitrary but fixed.
    fn now_nanos(&self) -> u64;
}

/// The process monotonic clock.
#[derive(Debug)]
pub struct SystemMonotonicClock {
    origin: Instant,
}

impl SystemMonotonicClock {
    /// Starts a clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemMonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now_nanos(&self) -> u64 {
        // Instant is monotonic, so the elapsed nanoseconds of a live process always fit in u64
        // (584 years).
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A clock a test advances by hand. Lets the `F`-deadline obligations be pinned deterministically.
#[derive(Debug, Default)]
pub struct ManualClock {
    nanos: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    /// A clock reading zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances the clock.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.nanos
            .fetch_add(nanos, std::sync::atomic::Ordering::Relaxed);
    }

    /// Sets the clock to an absolute reading.
    pub fn set_nanos(&self, nanos: u64) {
        self.nanos
            .store(nanos, std::sync::atomic::Ordering::Relaxed);
    }
}

impl MonotonicClock for ManualClock {
    fn now_nanos(&self) -> u64 {
        self.nanos.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The current record's flush deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushDeadline {
    /// Monotonic reading at which the record must be detached.
    pub at_nanos: u64,
    /// Generation of this deadline. Pass it back to [`ConnectionRecorder::run_scheduled_flush`]; a
    /// callback holding a stale generation is dropped.
    pub generation: u64,
}

/// What a scheduled flush did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledFlush {
    /// The callback's generation no longer matches: an earlier detachment invalidated it. Nothing
    /// was published and no record was created.
    Stale,
    /// There is no nonempty record to publish, so nothing was done. An idle connection emits no
    /// empty `TrafficStream`.
    NoRecord,
    /// The callback ran before the deadline. Nothing was published; re-arm for `remaining`.
    Early {
        /// Time still left before the deadline.
        remaining: Duration,
    },
    /// The record was detached and submitted.
    Detached {
        /// How late the callback ran. Lateness alone does not compromise capture.
        lateness: Duration,
        /// The ticket of the submission.
        ticket: PublishTicket,
    },
}

/// Construction parameters for a [`ConnectionRecorder`].
#[derive(Debug, Clone)]
pub struct ConnectionRecorderConfig {
    /// `TrafficStream.connectionId`: Netty's long-form channel id or equivalent. Must be non-empty
    /// and stable for the connection's whole life — partition locality depends on it.
    pub connection_id: Arc<str>,
    /// `TrafficStream.nodeId`: the writer identity, `captureActivationId + ":" +
    /// routingGenerationSequence`. Must be non-empty and byte-identical to the routing decision and
    /// to `WriterPartitionHeartbeat.writerNodeId`.
    pub node_id: Arc<str>,
    /// Ceiling for one enveloped `CaptureRecord` payload, or `None` for an unbounded record.
    ///
    /// This is the publisher's payload limit, i.e. `maximumKafkaMessageSize - 500`. The
    /// `TrafficStream` budget is derived from it by reserving the `CaptureRecord` envelope; see the
    /// `DEVIATION: J-1` note on
    /// [`traffic_stream_budget_for_payload_limit`](crate::protobuf_size::traffic_stream_budget_for_payload_limit).
    pub payload_limit: Option<u32>,
    /// `F`, the record flush interval. Must be positive.
    pub flush_interval: Duration,
}

impl ConnectionRecorderConfig {
    /// A config with the production default flush interval and no payload limit.
    #[must_use]
    pub fn new(connection_id: impl Into<Arc<str>>, node_id: impl Into<Arc<str>>) -> Self {
        Self {
            connection_id: connection_id.into(),
            node_id: node_id.into(),
            payload_limit: None,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
        }
    }

    /// Sets the enveloped payload ceiling.
    #[must_use]
    pub fn with_payload_limit(mut self, payload_limit: u32) -> Self {
        self.payload_limit = Some(payload_limit);
        self
    }

    /// Sets `F`.
    #[must_use]
    pub fn with_flush_interval(mut self, flush_interval: Duration) -> Self {
        self.flush_interval = flush_interval;
        self
    }
}

/// Records one client connection's capture stream.
///
/// # Observation catalogue
///
/// | Observation | Method | Notes |
/// |---|---|---|
/// | `bind`(2), `connect`(3), `disconnect`(10) | [`ConnectionRecorder::record_bind`], [`ConnectionRecorder::record_connect`], [`ConnectionRecorder::record_disconnect`] | No production path emits these; the replayer no-ops them. See the method docs. |
/// | `read`(4) / `readSegment`(5) | [`ConnectionRecorder::record_read`] | Segmented automatically. |
/// | `requestForwarded`(6) | [`ConnectionRecorder::record_request_forwarded`] | End of *forwarding*, not end of message. |
/// | `write`(8) / `writeSegment`(9) | [`ConnectionRecorder::record_write`] | Segmented automatically. |
/// | `close`(11) | [`ConnectionRecorder::close`] | Terminal; publishes the final record in the same operation. |
/// | `connectionException`(12) | [`ConnectionRecorder::record_connection_exception`] | Non-terminal, never segmented. |
/// | `segmentEnd`(14) | emitted automatically | Mandatory after the last segment, absent otherwise. |
/// | `endOfMessageIndicator`(15) | [`ConnectionRecorder::commit_end_of_message`] | The only request-completion trigger. |
/// | `requestDropped`(16) | [`ConnectionRecorder::cancel_capture_for_current_request`] | Advances the request ordinal like an end-of-message does. |
/// | `interimResponse`(17) / `interimResponseSegment`(19) | [`ConnectionRecorder::record_interim_response`] | Segmented automatically. |
#[derive(Debug)]
pub struct ConnectionRecorder<S: RecordSink> {
    config: ConnectionRecorderConfig,
    sink: S,
    chain: PublishChain,
    clock: Arc<dyn MonotonicClock>,
    /// `TrafficStream` content budget after reserving the `CaptureRecord` envelope.
    budget: Option<u32>,
    flush_interval_nanos: u64,
    next_sequence: u64,
    prior_request_boundaries: u32,
    reads_awaiting_end_of_message: bool,
    next_index: u32,
    first_line_byte_length: i32,
    headers_byte_length: i32,
    current: Option<RecordEncoder>,
    deadline: Option<FlushDeadline>,
    generation: u64,
    terminated: bool,
}

impl<S: RecordSink> ConnectionRecorder<S> {
    /// Creates a recorder with the process monotonic clock.
    ///
    /// # Errors
    ///
    /// See [`ConnectionRecorder::with_clock`].
    pub fn new(config: ConnectionRecorderConfig, sink: S) -> Result<Self, WireError> {
        Self::with_clock(config, sink, Arc::new(SystemMonotonicClock::new()))
    }

    /// Creates a recorder with an explicit monotonic clock.
    ///
    /// # Errors
    ///
    /// * [`WireError::EmptyConnectionId`] / [`WireError::EmptyNodeId`] — both identities are
    ///   mandatory and an empty one kills the replayer process.
    /// * [`WireError::NonPositiveFlushInterval`] — `DEVIATION`: `proxyCaptureProtocol.md` §4.1
    ///   requires `F` to be positive and says zero and negative values are rejected. Java rejects
    ///   only negatives and silently treats `Duration.ZERO` as "deadlines disabled", which is the
    ///   value its own three-argument test constructor passes. This crate implements the documented
    ///   behaviour: zero is rejected. A deployment that genuinely wants no deadline must say so with
    ///   a large interval rather than with a value the protocol document calls invalid.
    /// * [`WireError::PayloadLimitTooSmall`] — the limit cannot hold the envelope, the header and
    ///   trailer, and one minimal observation.
    pub fn with_clock(
        config: ConnectionRecorderConfig,
        sink: S,
        clock: Arc<dyn MonotonicClock>,
    ) -> Result<Self, WireError> {
        if config.connection_id.is_empty() {
            return Err(WireError::EmptyConnectionId);
        }
        if config.node_id.is_empty() {
            return Err(WireError::EmptyNodeId);
        }
        // DEVIATION: proxyCaptureProtocol.md §4.1 defines F as positive and states that zero and
        // negative values are rejected. Java rejects only negatives (SER:152-154) and treats
        // Duration.ZERO as a supported "deadlines disabled" mode that its own three-argument test
        // constructor uses. The documented behaviour wins here: zero is rejected, and there is no
        // way to express "no record deadline".
        if config.flush_interval.is_zero() {
            return Err(WireError::NonPositiveFlushInterval(config.flush_interval));
        }

        // DEVIATION: J-1. The TrafficStream budget is the publisher's payload ceiling MINUS the
        // CaptureRecord envelope (1 + varint_len(limit)). Java leaves the envelope unreserved: its
        // per-record protobuf budget (KCF:265,1326) and its payload ceiling (CKP:329) are the same
        // number, while CKP:495 compares the *enveloped* length against it and one byte over is a
        // terminal PAYLOAD_LIMIT capture failure. Java survives only by accidentally
        // double-reserving the closing `index` field. Reserving explicitly here means the record
        // packing boundaries differ from Java's, which the rewrite plan permits absent a consumer
        // dependency; the replayer ignores record indices entirely (wire spec §1.8, D-3).
        let budget = match config.payload_limit {
            None => None,
            Some(limit) => {
                let budget = size::traffic_stream_budget_for_payload_limit(limit).ok_or(
                    WireError::PayloadLimitTooSmall {
                        limit,
                        minimum: limit + 1,
                    },
                )?;
                let header = RecordHeader {
                    connection_id: Arc::clone(&config.connection_id),
                    node_id: Arc::clone(&config.node_id),
                    prior_request_boundaries: 0,
                    reads_awaiting_end_of_message: false,
                };
                // One minimal observation is an empty message: ts + sequence + a one-byte tag and a
                // zero length.
                let empty = ObservationBody::Empty;
                let minimal =
                    ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::CLOSE, &empty)
                        .record_cost();
                let fixed = header.leading_len() + header.trailer_len(1);
                if budget < fixed + minimal {
                    let needed = fixed + minimal;
                    return Err(WireError::PayloadLimitTooSmall {
                        limit,
                        minimum: needed + size::capture_record_envelope_overhead(needed),
                    });
                }
                Some(budget)
            }
        };

        let flush_interval_nanos =
            u64::try_from(config.flush_interval.as_nanos()).unwrap_or(u64::MAX);

        Ok(Self {
            config,
            sink,
            chain: PublishChain::new(),
            clock,
            budget,
            flush_interval_nanos,
            next_sequence: 1,
            prior_request_boundaries: 0,
            reads_awaiting_end_of_message: false,
            next_index: 1,
            first_line_byte_length: UNKNOWN_MESSAGE_LENGTH,
            headers_byte_length: UNKNOWN_MESSAGE_LENGTH,
            current: None,
            deadline: None,
            generation: 0,
            terminated: false,
        })
    }

    // ---------------------------------------------------------------- introspection

    /// The connection id stamped into every record.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.config.connection_id
    }

    /// The writer identity stamped into every record.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.config.node_id
    }

    /// `connectionObservationSequence` the next observation will carry.
    ///
    /// 1-based, contiguous, never reset by a record boundary, by an end-of-message, by a dropped
    /// request, or by segmentation. The replayer rejects an absent field and demands exactly
    /// previous + 1 across record boundaries (wire spec §1.8, discrepancy D-2).
    #[must_use]
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Cumulative request boundaries, completed and intentionally dropped alike.
    ///
    /// Written as `TrafficStream.priorRequestsReceived`, and only when nonzero: an absent field and
    /// a present zero mean different things to the replayer.
    #[must_use]
    pub fn prior_request_boundaries(&self) -> u32 {
        self.prior_request_boundaries
    }

    /// Whether the connection currently has reads that no end-of-message has terminated.
    #[must_use]
    pub fn reads_awaiting_end_of_message(&self) -> bool {
        self.reads_awaiting_end_of_message
    }

    /// The 1-based index the next record will carry in its `index` oneof.
    #[must_use]
    pub fn next_record_index(&self) -> u32 {
        self.next_index
    }

    /// How many observations the open record holds, or 0 when no record is open.
    #[must_use]
    pub fn current_record_observations(&self) -> u32 {
        self.current
            .as_ref()
            .map_or(0, RecordEncoder::observation_count)
    }

    /// Whether the terminal `CloseObservation` has been recorded.
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// The `TrafficStream` content budget per record, after the envelope reservation.
    #[must_use]
    pub fn traffic_stream_budget(&self) -> Option<u32> {
        self.budget
    }

    /// The sink, for inspection.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The sink, mutably.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// The publication chain, for inspection.
    #[must_use]
    pub fn chain(&self) -> &PublishChain {
        &self.chain
    }

    // ---------------------------------------------------------------- deadline

    /// The open record's flush deadline, or `None` when no nonempty record exists.
    #[must_use]
    pub fn flush_deadline(&self) -> Option<FlushDeadline> {
        self.deadline
    }

    /// Time until the open record's deadline, saturating at zero once it has passed.
    #[must_use]
    pub fn time_until_flush_deadline(&self) -> Option<Duration> {
        self.deadline.map(|deadline| {
            Duration::from_nanos(deadline.at_nanos.saturating_sub(self.clock.now_nanos()))
        })
    }

    /// Runs the owner's scheduled flush for `generation`.
    ///
    /// This is the hook that publishes an otherwise-idle record. It never creates a record, so a
    /// stale callback cannot resurrect a connection after its `CloseObservation`.
    ///
    /// # Errors
    ///
    /// Propagates an encoding failure from the detachment it performs.
    pub fn run_scheduled_flush(&mut self, generation: u64) -> Result<ScheduledFlush, WireError> {
        if self.terminated {
            // proxyCaptureProtocol.md §4.2 step 7: a stale callback must not create or flush a
            // successor record, and nothing may follow CloseObservation.
            return Ok(ScheduledFlush::NoRecord);
        }
        if generation != self.generation {
            return Ok(ScheduledFlush::Stale);
        }
        let Some(deadline) = self.deadline else {
            return Ok(ScheduledFlush::NoRecord);
        };
        if self.current.as_ref().is_none_or(RecordEncoder::is_empty) {
            return Ok(ScheduledFlush::NoRecord);
        }
        let now = self.clock.now_nanos();
        if now < deadline.at_nanos {
            return Ok(ScheduledFlush::Early {
                remaining: Duration::from_nanos(deadline.at_nanos - now),
            });
        }
        let lateness = Duration::from_nanos(now - deadline.at_nanos);
        let ticket = self.detach(false)?.ok_or(WireError::EmptyRecord)?;
        Ok(ScheduledFlush::Detached { lateness, ticket })
    }

    // ---------------------------------------------------------------- publication feedback

    /// Reports that the in-flight record was acknowledged, releasing the next queued record.
    pub fn acknowledge_publication(&mut self) -> Option<PublishTicket> {
        self.chain.acknowledge(&mut self.sink)
    }

    /// Reports that the in-flight record failed or had an ambiguous outcome, poisoning the chain.
    pub fn fail_publication(&mut self, failure: PublishFailure) {
        self.chain.fail(failure);
    }

    /// Takes the next completed publication outcome.
    ///
    /// Correctness must not depend on the owner draining these promptly
    /// (`proxyCaptureProtocol.md` §4.2); they exist so failures reach the capture-failure workflow.
    pub fn poll_publication_outcome(&mut self) -> Option<(PublishTicket, PublishOutcome)> {
        self.chain.poll_outcome()
    }

    /// The latched publication failure, if the chain is poisoned.
    #[must_use]
    pub fn publication_failure(&self) -> Option<&PublishFailure> {
        self.chain.failure()
    }

    // ---------------------------------------------------------------- observations

    /// `BindObservation` (field 2).
    ///
    /// No production path in the Java tree emits this, and the replayer no-ops it: it falls through
    /// every branch of its observation dispatch (wire spec §1.7, discrepancy D-4). It exists here so
    /// the catalogue is complete and so the ordering rules apply uniformly; nothing in this crate
    /// calls it.
    ///
    /// # Errors
    ///
    /// See [`ConnectionRecorder::record_request_forwarded`].
    pub fn record_bind(&mut self, timestamp: CaptureTimestamp) -> Result<u64, WireError> {
        self.append_empty(timestamp, field::BIND)
    }

    /// `ConnectObservation` (field 3). Dead on every path; see [`ConnectionRecorder::record_bind`].
    ///
    /// # Errors
    ///
    /// See [`ConnectionRecorder::record_request_forwarded`].
    pub fn record_connect(&mut self, timestamp: CaptureTimestamp) -> Result<u64, WireError> {
        self.append_empty(timestamp, field::CONNECT)
    }

    /// `DisconnectObservation` (field 10). Dead on every path, and explicitly **not** terminal: only
    /// `close` ends a connection. See [`ConnectionRecorder::record_bind`].
    ///
    /// # Errors
    ///
    /// See [`ConnectionRecorder::record_request_forwarded`].
    pub fn record_disconnect(&mut self, timestamp: CaptureTimestamp) -> Result<u64, WireError> {
        self.append_empty(timestamp, field::DISCONNECT)
    }

    /// `ProxyFinishesForwardingRequestObservation` (field 6): the proxy finished forwarding a
    /// captured request to the source.
    ///
    /// Marks end of *forwarding*, not end of message. At most one per captured end-of-message, in
    /// end-of-message order, after its indicator and before the next request's reads. Its existence
    /// is not guaranteed.
    ///
    /// Returns the `connectionObservationSequence` it was stamped with.
    ///
    /// # Errors
    ///
    /// * [`WireError::AfterTerminalClose`] if the connection is already closed.
    /// * [`WireError::ObservationTooLarge`] if it cannot fit an empty record.
    /// * [`WireError::SequenceOverflow`] / [`WireError::RecordIndexOverflow`] on counter exhaustion.
    pub fn record_request_forwarded(
        &mut self,
        timestamp: CaptureTimestamp,
    ) -> Result<u64, WireError> {
        self.append_empty(timestamp, field::REQUEST_FORWARDED)
    }

    /// `ReadObservation`(4), segmented into `ReadSegmentObservation`(5) when needed.
    ///
    /// `data` is the raw plaintext bytes of the whole read, pre-decode and before any proxy header
    /// rewriting. A zero-length read produces one whole observation with the `data` field absent.
    ///
    /// Returns the sequence numbers stamped, in emission order.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_request_forwarded`], plus [`WireError::NoForwardProgress`] if
    /// the record budget cannot hold even one data byte.
    pub fn record_read(
        &mut self,
        timestamp: CaptureTimestamp,
        data: &[u8],
    ) -> Result<Vec<u64>, WireError> {
        self.append_data(ByteObservationKind::Read, timestamp, data)
    }

    /// `WriteObservation`(8), segmented into `WriteSegmentObservation`(9) when needed.
    ///
    /// `data` is pre-TLS plaintext, byte-identical to what reaches the client and never HTTP-decoded.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_read`].
    pub fn record_write(
        &mut self,
        timestamp: CaptureTimestamp,
        data: &[u8],
    ) -> Result<Vec<u64>, WireError> {
        self.append_data(ByteObservationKind::Write, timestamp, data)
    }

    /// `InterimResponseObservation`(17), segmented into `InterimResponseSegmentObservation`(19).
    ///
    /// `data` must be one **complete** non-switching 1xx header block; the replayer raises a
    /// violation for a whole interim response arriving during a segmented one.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_read`].
    pub fn record_interim_response(
        &mut self,
        timestamp: CaptureTimestamp,
        data: &[u8],
    ) -> Result<Vec<u64>, WireError> {
        self.append_data(ByteObservationKind::InterimResponse, timestamp, data)
    }

    /// `ConnectionExceptionObservation` (field 12). Non-terminal and never segmented.
    ///
    /// The message is truncated to [`MAX_EXCEPTION_MESSAGE_BYTES`] on a character boundary.
    /// `DEVIATION: J-5` — Java cannot chunk this observation, so an oversized diagnostic message
    /// becomes a hard capture failure; the replayer never reads the field.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_request_forwarded`].
    pub fn record_connection_exception(
        &mut self,
        timestamp: CaptureTimestamp,
        message: &str,
    ) -> Result<u64, WireError> {
        let mut end = message.len().min(MAX_EXCEPTION_MESSAGE_BYTES);
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = &message[..end];
        let body = ObservationBody::Text {
            field_number: field::EXCEPTION_MESSAGE,
            text: truncated,
        };
        self.append_observation(timestamp, field::CONNECTION_EXCEPTION, &body)
    }

    /// Records `EndOfMessageIndication.firstLineByteLength` for the request being assembled.
    pub fn set_end_of_first_line(&mut self, byte_length: i32) {
        self.first_line_byte_length = byte_length;
    }

    /// Records `EndOfMessageIndication.headersByteLength` for the request being assembled.
    pub fn set_end_of_headers(&mut self, byte_length: i32) {
        self.headers_byte_length = byte_length;
    }

    /// `EndOfMessageIndication` (field 15): the read that completed an HTTP request.
    ///
    /// This is the replayer's **only** request-completion trigger. Both length fields are written
    /// unconditionally, including the [`UNKNOWN_MESSAGE_LENGTH`] sentinel. Committing it clears
    /// `lastObservationWasUnterminatedRead`, advances `priorRequestsReceived`, and resets the two
    /// length fields.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_request_forwarded`].
    pub fn commit_end_of_message(&mut self, timestamp: CaptureTimestamp) -> Result<u64, WireError> {
        let body = ObservationBody::EndOfMessage {
            first_line_byte_length: self.first_line_byte_length,
            headers_byte_length: self.headers_byte_length,
        };
        let sequence =
            self.append_observation(timestamp, field::END_OF_MESSAGE_INDICATOR, &body)?;
        self.complete_request_boundary();
        Ok(sequence)
    }

    /// `RequestIntentionallyDropped` (field 16): capture for the in-flight request was abandoned
    /// after a prefix of it had already been emitted.
    ///
    /// Advances the request ordinal exactly like an end-of-message does but reconstitutes nothing,
    /// which is why `priorRequestsReceived` counts dropped requests too (wire spec §3.2,
    /// discrepancy D-7). Already-written `read` observations are not retracted; the replayer
    /// discards the reads preceding the marker.
    ///
    /// # Errors
    ///
    /// As [`ConnectionRecorder::record_request_forwarded`].
    pub fn cancel_capture_for_current_request(
        &mut self,
        timestamp: CaptureTimestamp,
    ) -> Result<u64, WireError> {
        let sequence = self.append_empty(timestamp, field::REQUEST_DROPPED)?;
        self.complete_request_boundary();
        Ok(sequence)
    }

    fn complete_request_boundary(&mut self) {
        self.reads_awaiting_end_of_message = false;
        self.prior_request_boundaries = self.prior_request_boundaries.saturating_add(1);
        self.first_line_byte_length = UNKNOWN_MESSAGE_LENGTH;
        self.headers_byte_length = UNKNOWN_MESSAGE_LENGTH;
    }

    // ---------------------------------------------------------------- lifecycle

    /// Detaches and submits the open record without ending the connection.
    ///
    /// This is the Critical Mutation Traffic flush: a complete mutating request requires
    /// publication before the proxy forwards it. A no-op when no nonempty record is open.
    ///
    /// # Errors
    ///
    /// Propagates encoding failures.
    pub fn flush(&mut self) -> Result<Option<PublishTicket>, WireError> {
        if self.terminated || self.current.as_ref().is_none_or(RecordEncoder::is_empty) {
            return Ok(None);
        }
        self.detach(false)
    }

    /// `CloseObservation` (field 11) followed immediately by terminal publication.
    ///
    /// The two are one operation on purpose. `KafkaCaptureFactory.isTerminalRecord` enforces a
    /// biconditional — `numberOfThisLastChunk` is set **iff** the record holds exactly one `close`
    /// as its final observation — and violating it declares the whole process unstable. Java leaves
    /// the two steps separable, which is Java defect J-2: a terminal flush with no open record
    /// fabricates an observation-less final record that then fails that very check. Here there is no
    /// API shape that can do it.
    ///
    /// The open record is detached immediately regardless of its `F` deadline
    /// (`proxyCaptureProtocol.md` §4.1).
    ///
    /// # Errors
    ///
    /// * [`WireError::AfterTerminalClose`] if already closed.
    /// * Encoding failures from the close observation or the terminal detachment.
    pub fn close(&mut self, timestamp: CaptureTimestamp) -> Result<PublishTicket, WireError> {
        self.append_empty(timestamp, field::CLOSE)?;
        // The close observation guarantees a nonempty record, so this cannot be EmptyRecord.
        let ticket = self.detach(true)?.ok_or(WireError::EmptyRecord)?;
        self.terminated = true;
        Ok(ticket)
    }

    /// Releases the open record without publishing anything, and refuses further observations.
    ///
    /// Required on every teardown path that does not publish a terminal record — the proxy's
    /// pass-through transition, for instance, where capture has already been compromised. Java has
    /// no such operation, which is Java defect J-3: the open record's pooled buffer leaks for every
    /// affected connection and the connection stays registered forever. Here the buffer is released
    /// by ownership and the recorder is marked terminated.
    ///
    /// Returns how many observations were discarded.
    pub fn abort(&mut self) -> u32 {
        let discarded = self
            .current
            .take()
            .map_or(0, |record| record.observation_count());
        self.invalidate_deadline();
        self.terminated = true;
        discarded
    }

    // ---------------------------------------------------------------- internals

    fn header_snapshot(&self) -> RecordHeader {
        RecordHeader {
            connection_id: Arc::clone(&self.config.connection_id),
            node_id: Arc::clone(&self.config.node_id),
            prior_request_boundaries: self.prior_request_boundaries,
            reads_awaiting_end_of_message: self.reads_awaiting_end_of_message,
        }
    }

    fn arm_deadline(&mut self) {
        self.generation += 1;
        self.deadline = Some(FlushDeadline {
            at_nanos: self
                .clock
                .now_nanos()
                .saturating_add(self.flush_interval_nanos),
            generation: self.generation,
        });
    }

    fn invalidate_deadline(&mut self) {
        self.generation += 1;
        self.deadline = None;
    }

    /// Detaches the open record and hands it to the publication chain.
    fn detach(&mut self, is_final: bool) -> Result<Option<PublishTicket>, WireError> {
        let Some(record) = self.current.take() else {
            // A terminal flush with nothing open must not fabricate a record (defect J-2).
            self.invalidate_deadline();
            return if is_final {
                Err(WireError::EmptyRecord)
            } else {
                Ok(None)
            };
        };
        self.invalidate_deadline();
        let encoded = record.finish(is_final)?;
        Ok(Some(self.chain.submit(encoded, &mut self.sink)))
    }

    /// Detaches the open record if its fixed deadline has passed.
    ///
    /// At-the-deadline counts as expired (`>=`, not `>`), pinned by the Java test
    /// `observationAtDeadlineDetachesExpiredRecordBeforeAppending`.
    fn detach_if_deadline_expired(&mut self) -> Result<(), WireError> {
        let Some(deadline) = self.deadline else {
            return Ok(());
        };
        if self.current.as_ref().is_none_or(RecordEncoder::is_empty) {
            return Ok(());
        }
        if self.clock.now_nanos() >= deadline.at_nanos {
            self.detach(false)?;
        }
        Ok(())
    }

    fn start_record(&mut self) -> Result<(), WireError> {
        let index = self.next_index;
        if i32::try_from(index).is_err() {
            return Err(WireError::RecordIndexOverflow);
        }
        self.next_index += 1;
        let header = self.header_snapshot();
        let capacity = self
            .budget
            .map_or(4096, |budget| budget.min(64 * 1024) as usize);
        let buffer = self.sink.create_buffer(capacity);
        self.current = Some(RecordEncoder::start(buffer, header, index, self.budget));
        Ok(())
    }

    fn fresh_space(&self) -> Option<u32> {
        RecordEncoder::fresh_space(&self.header_snapshot(), self.next_index, self.budget)
    }

    /// Space available for one observation's content right now: the open record's remaining space,
    /// or the space a freshly created record would offer.
    fn available_space(&self) -> Option<u32> {
        match self.current.as_ref() {
            Some(record) => record.space_left(),
            None => self.fresh_space(),
        }
    }

    fn append_empty(
        &mut self,
        timestamp: CaptureTimestamp,
        capture_field: u32,
    ) -> Result<u64, WireError> {
        self.append_observation(timestamp, capture_field, &ObservationBody::Empty)
    }

    /// The single admission path: deadline check, size check, record creation, write.
    fn append_observation(
        &mut self,
        timestamp: CaptureTimestamp,
        capture_field: u32,
        body: &ObservationBody<'_>,
    ) -> Result<u64, WireError> {
        if self.terminated {
            return Err(WireError::AfterTerminalClose);
        }
        // DEVIATION: a poisoned publication chain refuses further observations. Java's serializer
        // keeps accepting them and lets every successor record be built and then released
        // unpublished; the capture-failure decision is taken a layer out, in KafkaCaptureFactory
        // (wire spec §7.4). Failing closed here surfaces the compromise to the capture path
        // immediately instead of silently discarding records, and the chain already guarantees no
        // successor can be submitted (§4.2). The owner can still read the failure through
        // `publication_failure` and apply its own policy.
        if let Some(failure) = self.chain.failure() {
            return Err(WireError::PoisonedPublishChain(failure.clone()));
        }
        // proxyCaptureProtocol.md §4.2 step 2: the deadline check happens BEFORE the observation is
        // admitted, so a new observation can never enter an expired record.
        self.detach_if_deadline_expired()?;
        if self.next_sequence == u64::MAX {
            return Err(WireError::SequenceOverflow);
        }
        let sequence = self.next_sequence;
        let plan = ObservationPlan::new(sequence, timestamp, capture_field, body);

        // Size trigger: detach a record that cannot hold this observation before creating a
        // successor. Never create an empty record only to discover it is too small.
        if let Some(record) = self.current.as_ref()
            && !record.can_admit(plan.observation_content_len)
        {
            self.detach(false)?;
        }
        if self.current.is_none() {
            if let Some(space) = self.fresh_space()
                && plan.record_cost() > space
            {
                return Err(WireError::ObservationTooLarge {
                    needed: plan.record_cost(),
                    budget: self.budget.unwrap_or(u32::MAX),
                });
            }
            self.start_record()?;
            self.arm_deadline();
        }

        let record = self
            .current
            .as_mut()
            .expect("a record was just created or retained");
        record.write_observation(&plan, body);
        self.next_sequence += 1;
        Ok(sequence)
    }

    /// The whole-versus-segmented decision and the segmentation loop.
    fn append_data(
        &mut self,
        kind: ByteObservationKind,
        timestamp: CaptureTimestamp,
        data: &[u8],
    ) -> Result<Vec<u64>, WireError> {
        if self.terminated {
            return Err(WireError::AfterTerminalClose);
        }
        let total = u32::try_from(data.len()).expect("capture payloads fit in u32");

        // An empty buffer is never segmented: it produces one whole observation whose submessage is
        // empty, i.e. the `data` field is absent rather than zero-length.
        if total == 0 {
            let body = ObservationBody::Data {
                field_number: kind.whole_data_field(),
                bytes: data,
            };
            let sequence = self.append_observation(timestamp, kind.whole_field(), &body)?;
            self.mark_read_awaiting(kind);
            return Ok(vec![sequence]);
        }

        self.detach_if_deadline_expired()?;
        let whole_body = ObservationBody::Data {
            field_number: kind.whole_data_field(),
            bytes: data,
        };
        let whole_plan = ObservationPlan::new(
            self.next_sequence,
            timestamp,
            kind.whole_field(),
            &whole_body,
        );

        // An unbounded record is never segmented.
        let fits_whole = if self.budget.is_none() {
            true
        } else {
            let fits_here = self
                .current
                .as_ref()
                .is_some_and(|record| record.can_admit(whole_plan.observation_content_len));
            let fits_fresh = self
                .fresh_space()
                .is_some_and(|space| whole_plan.record_cost() <= space);
            fits_here || fits_fresh
        };
        if fits_whole {
            let sequence = self.append_observation(timestamp, kind.whole_field(), &whole_body)?;
            self.mark_read_awaiting(kind);
            return Ok(vec![sequence]);
        }

        // Segment. Byte order is preserved by construction: the slice is consumed strictly left to
        // right and every segment is admitted before the next one is sized.
        let segment_field = kind.segment_field();
        let segment_data_field = kind.segment_data_field();
        let mut sequences = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            self.detach_if_deadline_expired()?;
            let remaining = total - u32::try_from(offset).expect("offset fits in u32");
            let sequence = self.next_sequence;
            let available = self.available_space().unwrap_or(u32::MAX);
            let mut take = max_segment_data_len(
                available,
                remaining,
                sequence,
                timestamp,
                segment_field,
                segment_data_field,
            );
            if take == 0 {
                // No forward progress in this record. A nonempty record can be detached to try a
                // fresh one; an empty record means the budget cannot hold a single data byte.
                if self
                    .current
                    .as_ref()
                    .is_some_and(|record| !record.is_empty())
                {
                    self.detach(false)?;
                    let available = self.available_space().unwrap_or(u32::MAX);
                    take = max_segment_data_len(
                        available,
                        remaining,
                        sequence,
                        timestamp,
                        segment_field,
                        segment_data_field,
                    );
                }
                if take == 0 {
                    return Err(WireError::NoForwardProgress);
                }
            }
            let end = offset + take as usize;
            let body = ObservationBody::Data {
                field_number: segment_data_field,
                bytes: &data[offset..end],
            };
            debug_assert_eq!(
                ObservationPlan::new(sequence, timestamp, segment_field, &body).record_cost(),
                segment_record_cost(take, sequence, timestamp, segment_field, segment_data_field),
                "the segment solver and the encoder disagree about a segment's cost"
            );
            sequences.push(self.append_observation(timestamp, segment_field, &body)?);
            self.mark_read_awaiting(kind);
            offset = end;
        }

        // Mandatory and emitted exactly once, after the last segment, only on the segmented path.
        // Omitting it leaves the replayer unable to tell the value ended.
        sequences.push(self.append_empty(timestamp, field::SEGMENT_END)?);
        Ok(sequences)
    }

    fn mark_read_awaiting(&mut self, kind: ByteObservationKind) {
        if kind.leaves_read_awaiting_end_of_message() {
            self.reads_awaiting_end_of_message = true;
        }
    }
}

impl<S: RecordSink> ConnectionRecorder<S> {
    /// Consumes the recorder and returns its sink.
    ///
    /// Releases any open record without publishing it, exactly like
    /// [`ConnectionRecorder::abort`].
    #[must_use]
    pub fn into_sink(mut self) -> S {
        self.abort();
        self.sink
    }
}

/// Decodes a batch of encoded records back into `TrafficStream` messages.
///
/// Intended for tests, golden-record tooling, and diagnostics: the production consumer is the Java
/// replayer.
///
/// # Panics
///
/// Panics if a record is not a canonical `CaptureRecord` carrying a `trafficStream` payload. Only a
/// bug in this crate can produce one, so there is no recoverable case to report.
#[must_use]
pub fn decode_traffic_streams(records: &[EncodedRecord]) -> Vec<crate::proto::TrafficStream> {
    use prost::Message;
    records
        .iter()
        .map(|record| {
            let envelope = crate::proto::CaptureRecord::decode(record.payload())
                .expect("every emitted record is a valid CaptureRecord");
            match envelope.payload {
                Some(crate::proto::capture_record::Payload::TrafficStream(stream)) => stream,
                other => panic!("CaptureRecord.payload was not a trafficStream: {other:?}"),
            }
        })
        .collect()
}
