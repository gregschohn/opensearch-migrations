//! Capture mode: Kafka readiness at start-up, per-connection capture, and retirement.
//!
//! This module is the adapter layer between `capture-config`'s configuration vocabulary,
//! `capture-kafka`'s client vocabulary, and `capture-observability`'s metric vocabulary. None of
//! those three crates depends on the others, which is the right shape — but it means every
//! translation between them lives here.
//!
//! # Start-up ordering
//!
//! [`CaptureMode::start`] wraps `capture_kafka::bootstrap::start_capture`, whose **return is the
//! signal that the listener may open**: it returns only after the topic layout is validated, the
//! topic's timestamp type is qualified (owner decision OD-5), the publisher is running, group
//! membership has produced an assignment, and the first routing generation is *usable* — meaning an
//! initial heartbeat was acknowledged and accepted on every assigned partition. A client that
//! connects before that is refused by the operating system, which is the only refusal that cannot
//! lose traffic.
//!
//! # Known gap: the `F` record-flush deadline has no driver
//!
//! `capture_wire::ConnectionRecorder` arms a flush deadline `F`
//! (`--traffic-stream-flush-interval-seconds`) after each observation, and expects its owner to
//! call `run_scheduled_flush` when the deadline expires. **Nothing calls it**, and nothing in this
//! process can: the recorder is owned by [`crate::capture_sink::RecordingCaptureSink`], which the
//! transport owns by value, and `capture_transport::CaptureSink` has no timer hook — every method
//! is called in reaction to a byte or a framing event. `capture_transport::Connection::run`'s
//! select loop therefore has nothing to select the sink's own deadline on.
//!
//! The consequence is bounded and is *not* a correctness failure of the capture-before-forward
//! gate: a Critical Mutation Traffic record is detached and published synchronously by
//! [`capture_sink::RecordingCaptureSink::gate_critical_mutation`], so no mutation is ever forwarded
//! ahead of its publication. What is lost is *freshness* — a partially filled record on an idle
//! connection is not published until the next observation or the terminal close, where Java would
//! have published it after `F`. `--traffic-stream-flush-interval-seconds` is consequently inert.
//!
//! Closing it needs an owner decision, because the fix is in a crate this one does not own:
//! `CaptureSink` would need something like `fn next_deadline(&self) -> Option<Instant>` plus
//! `fn run_deadline(&mut self) -> Result<(), CaptureAbort>`, selected in `Connection::run`
//! alongside the assembly and lifetime deadlines it already has. Sharing the recorder with a timer
//! task instead does not work: the sink's trait methods are synchronous, so the shared lock would
//! have to be blocking, and `gate_critical_mutation` holds the recorder across an `.await`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture_config::{ApplicationConfig, KafkaAuthType, KafkaConfig};
use capture_kafka::bootstrap::{self, CaptureRuntime, CaptureStartupConfig};
use capture_kafka::budget::BudgetLimits;
use capture_kafka::client_config::{AuthMode, KafkaClientSettings, TlsMaterial};
use capture_kafka::connection_factory::{ConnectionCapture, ConnectionCaptureFactory};
use capture_kafka::msk_iam::{MskIamSettings, MskIamTokenSource};
use capture_kafka::publisher_actor::AcknowledgedRecord;
use capture_observability::root::RootTelemetry;
use capture_wire::{
    ConnectionRecorderConfig, EncodedRecord, HeapRecycler, RecordBuf, RecordSink, SinkError,
};

use crate::capture_sink::RecordingCaptureSink;
use crate::publication::{Acknowledgement, PublicationOutcome, PublishingRecordSink};
use crate::shutdown::{CaptureRetirement, ShutdownTiming};

// The publication budget (bytes and records held awaiting acknowledgement) is configured by
// `--kafka-publication-budget-bytes` / `--kafka-publication-budget-records` (owner decision OD-13);
// defaults live in `capture_config::cli`. Java has no equivalent: its bound is implicit and
// effectively unbounded.

/// How long start-up keeps retrying transient Kafka metadata failures before refusing.
///
/// CHOSEN DEFAULT: finite on purpose. An unbounded retry would leave the pod alive with no
/// listener and no traffic, which looks identical to a hang; refusing makes the container restart
/// and the failure visible.
pub const METADATA_BUDGET: Duration = Duration::from_mins(1);

/// How long start-up waits for the first routing generation to become usable.
pub const FIRST_GENERATION_BUDGET: Duration = Duration::from_mins(1);

/// Why capture mode could not be configured.
#[derive(Debug, thiserror::Error)]
pub enum KafkaWiringError {
    /// `--kafka-brokers` was absent, so this is not capture mode.
    #[error("capture mode requires --kafka-brokers")]
    NotCaptureMode,
    /// The property file could not be read or parsed.
    #[error(transparent)]
    ClientConfig(#[from] capture_kafka::ClientConfigError),
    /// `--kafka-auth-type msk-iam` was selected but no AWS region is available.
    #[error(
        "--kafka-auth-type msk-iam requires an AWS region and neither AWS_REGION nor \
         AWS_DEFAULT_REGION is set; the Kubernetes deployment takes AWS_REGION from the \
         `aws-metadata` ConfigMap (key `region`), so check that it exists in this namespace"
    )]
    MissingAwsRegion,
    /// Kafka refused to become ready, so the listener must never open.
    #[error("capture startup was refused: {0}")]
    Refused(#[from] capture_kafka::StartupRefusal),
    /// The recorder configuration derived from the Kafka settings is unusable.
    #[error(transparent)]
    Wire(#[from] capture_wire::WireError),
}

impl KafkaWiringError {
    /// Whether this is an operator configuration mistake (exit 2) rather than a failure to bring
    /// capture to readiness (exit 78).
    ///
    /// A missing region, a malformed or disallowed property, or an impossible derived limit cannot
    /// be fixed by retrying, so the process reports a configuration error. A broker that refuses
    /// readiness, or credentials that cannot be obtained from the environment at this moment, are
    /// capture failures.
    #[must_use]
    pub fn is_configuration_error(&self) -> bool {
        match self {
            Self::NotCaptureMode | Self::MissingAwsRegion | Self::Wire(_) => true,
            Self::ClientConfig(error) => !matches!(
                error,
                capture_kafka::ClientConfigError::AuthUnavailable { .. }
            ),
            Self::Refused(_) => false,
        }
    }
}

/// Builds the Kafka client settings and, when the auth mode needs one, the MSK IAM token source.
///
/// # Errors
///
/// [`KafkaWiringError`] when the property file is unreadable or malformed, or when MSK IAM is
/// selected without a region.
pub fn kafka_settings(
    config: &ApplicationConfig,
    environment: &dyn capture_config::EnvLookup,
) -> Result<(KafkaClientSettings, Option<Arc<MskIamTokenSource>>), KafkaWiringError> {
    let kafka = config
        .sink()
        .kafka()
        .ok_or(KafkaWiringError::NotCaptureMode)?;

    let file_properties = match kafka.property_file() {
        None => BTreeMap::new(),
        Some(path) => capture_kafka::client_config::load_property_file(path)?,
    };

    let mut settings = KafkaClientSettings::new(kafka.brokers());
    kafka.topic().clone_into(&mut settings.topic);
    kafka.client_id().clone_into(&mut settings.client_id);
    settings.properties = file_properties;
    settings.auth = auth_mode(kafka, environment)?;
    settings.producer_sizing = match kafka.producer_count() {
        capture_config::kafka::ProducerCount::Auto => {
            capture_kafka::client_config::ProducerSizing::Auto {
                cap: capture_kafka::client_config::AUTO_PRODUCER_CAP,
            }
        }
        // Validated to 1..=64 by capture-config, so the conversion cannot fail in practice.
        capture_config::kafka::ProducerCount::Fixed(count) => {
            capture_kafka::client_config::ProducerSizing::Fixed(usize::try_from(count).unwrap_or(1))
        }
    };

    // `message.max.bytes` is what bounds a record on the wire. `--max-traffic-buffer-size` is the
    // configured traffic-stream size. Both are ceilings, so the smaller one binds; taking the
    // Kafka one from the effective properties keeps a property-file override honest.
    if let Some(configured) = settings
        .properties
        .get("message.max.bytes")
        .and_then(|value| value.parse::<usize>().ok())
    {
        settings.max_message_bytes = configured;
    }

    let oauth = match &settings.auth {
        AuthMode::MskIam(msk) => Some(Arc::new(MskIamTokenSource::from_default_chain(
            msk.clone(),
        )?)),
        AuthMode::None | AuthMode::Tls(_) | AuthMode::ScramSha512 { .. } => None,
    };

    Ok((settings, oauth))
}

/// Maps the configured auth type onto `capture-kafka`'s auth mode.
///
/// The two vocabularies differ for a reason worth keeping visible: librdkafka has no JAAS string
/// and no `AWS_MSK_IAM` mechanism, so SCRAM credentials become explicit fields and MSK IAM becomes
/// SASL/OAUTHBEARER with a `SigV4` token. A missing SCRAM credential is rejected rather than
/// silently omitted, which is where Java authenticates as nobody and fails later with an opaque
/// broker error (`KafkaConfig.java:217`).
fn auth_mode(
    kafka: &KafkaConfig,
    environment: &dyn capture_config::EnvLookup,
) -> Result<AuthMode, KafkaWiringError> {
    match kafka.auth() {
        KafkaAuthType::None => Ok(AuthMode::None),
        KafkaAuthType::Ssl => Ok(AuthMode::Tls(TlsMaterial::default())),
        KafkaAuthType::ScramSha512 => Ok(AuthMode::ScramSha512 {
            // The configuration layer already enforced that both are present for this mode
            // (`KafkaConfig::new` rejects it otherwise), so an empty string here would be a
            // configuration-layer bug rather than an operator error.
            username: kafka.username().unwrap_or_default().to_owned(),
            password: kafka
                .password()
                .map(|secret| secret.expose().to_owned())
                .unwrap_or_default(),
            tls: TlsMaterial::default(),
        }),
        KafkaAuthType::MskIam => {
            let region = environment
                .get("AWS_REGION")
                .or_else(|| environment.get("AWS_DEFAULT_REGION"))
                .filter(|region| !region.is_empty())
                .ok_or(KafkaWiringError::MissingAwsRegion)?;
            Ok(AuthMode::MskIam(MskIamSettings::new(region)))
        }
    }
}

/// Bridges `capture_kafka`'s metric calls onto the contracted instruments.
///
/// AWKWARD: `capture-kafka` and `capture-observability` each declare their own copy of
/// `RecordKind`, `PublishOutcome`, `RoutingTransition`, and `FailureReason` — identical variants
/// and identical label strings — because neither crate depends on the other. That independence is
/// deliberate and good, but it makes this translation the only thing standing between the sixteen
/// Kafka instruments and a blank dashboard panel. Every match below is exhaustive on purpose: if
/// either side gains a variant, this file stops compiling instead of silently dropping a series.
#[derive(Debug)]
struct ContractedKafkaMetrics(Arc<RootTelemetry>);

impl capture_kafka::CaptureMetrics for ContractedKafkaMetrics {
    fn publication_accepted(&self, kind: capture_kafka::RecordKind, bytes: u64) {
        self.0
            .kafka()
            .publication_accepted(record_kind(kind), bytes);
    }

    fn publication_completed(
        &self,
        kind: capture_kafka::RecordKind,
        outcome: capture_kafka::PublishOutcome,
        bytes: u64,
    ) {
        self.0
            .kafka()
            .publication_completed(record_kind(kind), publish_outcome(outcome), bytes);
        // kafkaCommitExceptionCount: traffic records only, like Java's KafkaRecordContext. The
        // failed record's latency was already recorded by `submit_to_delivery`.
        if matches!(kind, capture_kafka::RecordKind::Traffic)
            && matches!(outcome, capture_kafka::PublishOutcome::Failed)
        {
            self.0.record_kafka_commit_failure();
        }
    }

    fn submit_to_delivery(&self, kind: capture_kafka::RecordKind, elapsed: std::time::Duration) {
        // kafkaCommitCount / kafkaCommitDuration: Java's KafkaRecordContext opens at `send` and
        // closes in the producer callback, for traffic records. This is the Phase 1 Gate 5
        // acknowledgement-latency instrument, bucketed by GATE_LATENCY_BUCKETS_MS.
        if matches!(kind, capture_kafka::RecordKind::Traffic) {
            self.0.record_kafka_commit(elapsed);
        }
    }

    fn heartbeat_deadline_violated(&self) {
        self.0.kafka().heartbeat_deadline_violated();
    }

    fn broker_time_violated(&self) {
        self.0.kafka().broker_time_violated();
    }

    fn routing_transition(&self, transition: capture_kafka::RoutingTransition) {
        self.0
            .kafka()
            .routing_transition(routing_transition(transition));
    }

    fn writers_activated(&self, activated: u64, previously_active: u64) {
        self.0.kafka().writers_activated(
            u32::try_from(activated).unwrap_or(u32::MAX),
            u32::try_from(previously_active).unwrap_or(u32::MAX),
        );
    }

    fn active_writers_began_draining(&self, count: u64) {
        self.0
            .kafka()
            .active_writers_began_draining(u32::try_from(count).unwrap_or(u32::MAX));
    }

    fn writer_retired(&self) {
        self.0.kafka().writer_retired();
    }

    fn write_gate_tripped(&self, reason: capture_kafka::FailureReason) {
        self.0.kafka().write_gate_tripped(failure_reason(reason));
    }

    fn write_rejected(&self) {
        self.0.kafka().write_rejected();
    }

    fn publisher_failed(&self, reason: capture_kafka::FailureReason) {
        self.0.kafka().publisher_failed(failure_reason(reason));
    }
}

fn record_kind(
    kind: capture_kafka::RecordKind,
) -> capture_observability::kafka_metrics::RecordKind {
    use capture_observability::kafka_metrics::RecordKind as Contract;
    match kind {
        capture_kafka::RecordKind::Traffic => Contract::Traffic,
        capture_kafka::RecordKind::Heartbeat => Contract::Heartbeat,
    }
}

fn publish_outcome(
    outcome: capture_kafka::PublishOutcome,
) -> capture_observability::kafka_metrics::PublishOutcome {
    use capture_observability::kafka_metrics::PublishOutcome as Contract;
    match outcome {
        capture_kafka::PublishOutcome::Acknowledged => Contract::Acknowledged,
        capture_kafka::PublishOutcome::Failed => Contract::Failed,
    }
}

fn routing_transition(
    transition: capture_kafka::RoutingTransition,
) -> capture_observability::kafka_metrics::RoutingTransition {
    use capture_observability::kafka_metrics::RoutingTransition as Contract;
    match transition {
        capture_kafka::RoutingTransition::AssignmentReceived => Contract::AssignmentReceived,
        capture_kafka::RoutingTransition::GenerationPrepared => Contract::GenerationPrepared,
        capture_kafka::RoutingTransition::GenerationActivated => Contract::GenerationActivated,
    }
}

fn failure_reason(
    reason: capture_kafka::FailureReason,
) -> capture_observability::kafka_metrics::FailureReason {
    use capture_observability::kafka_metrics::FailureReason as Contract;
    match reason {
        capture_kafka::FailureReason::HeartbeatDeadline => Contract::HeartbeatDeadline,
        capture_kafka::FailureReason::BrokerTime => Contract::BrokerTime,
        capture_kafka::FailureReason::PayloadLimit => Contract::PayloadLimit,
        capture_kafka::FailureReason::KafkaSend => Contract::KafkaSend,
        capture_kafka::FailureReason::CorruptedState => Contract::CorruptedState,
        capture_kafka::FailureReason::ExecutorRejected => Contract::ExecutorRejected,
        capture_kafka::FailureReason::Membership => Contract::Membership,
        capture_kafka::FailureReason::Other => Contract::Other,
    }
}

/// One connection's Kafka-backed record sink.
///
/// See [`crate::publication`] for why this exists alongside
/// [`capture_kafka::connection_factory::ConnectionCapture`] rather than being it.
#[derive(Debug)]
pub struct KafkaPublishingSink {
    capture: ConnectionCapture,
    recycler: Arc<HeapRecycler>,
}

impl RecordSink for KafkaPublishingSink {
    fn create_buffer(&mut self, capacity: usize) -> RecordBuf {
        RecordBuf::with_recycler(capacity, Arc::clone(&self.recycler) as Arc<_>)
    }

    fn submit(&mut self, record: EncodedRecord) -> Result<(), SinkError> {
        // Infallible by shape: `ConnectionCapture::submit` enqueues onto its own chain and reports
        // a transport refusal through `drain_outcomes` instead of here.
        let _ticket = self.capture.submit(record);
        Ok(())
    }
}

impl PublishingRecordSink for KafkaPublishingSink {
    fn take_outcomes(&mut self) -> Vec<PublicationOutcome> {
        let applied = self.capture.drain_outcomes();
        if applied == 0 {
            return Vec::new();
        }
        // LOSSY, and bounded by the architecture rather than by luck:
        // `ConnectionCapture::drain_outcomes` returns only a count, so a batch that mixed an
        // acknowledgement with a failure would be reported here as all-failed. It cannot mix,
        // because `ConnectionRecorder`'s own chain keeps exactly one record in flight, so `applied`
        // is always 0 or 1. A `drain_outcomes` returning the outcomes themselves would remove the
        // reasoning step; see the `capture_proxy::publication` module docs.
        if self.capture.is_poisoned() {
            return vec![PublicationOutcome::Failed(format!(
                "the capture publication chain for {} is poisoned",
                self.capture.route()
            ))];
        }
        match self.capture.last_acknowledgement() {
            Some(acknowledged) => (0..applied)
                .map(|_| {
                    PublicationOutcome::Acknowledged(Acknowledgement {
                        partition: acknowledged.partition,
                        log_append_time: acknowledged.log_append_time,
                    })
                })
                .collect(),
            None => vec![PublicationOutcome::Failed(format!(
                "a capture publication for {} completed with no acknowledgement metadata",
                self.capture.route()
            ))],
        }
    }

    fn validate_critical_mutation(&self, acknowledged: Acknowledgement) -> Result<(), String> {
        self.capture
            .validate_critical_mutation(AcknowledgedRecord {
                partition: acknowledged.partition,
                log_append_time: acknowledged.log_append_time,
            })
            .map_err(|failure| failure.to_string())
    }

    fn route_description(&self) -> String {
        self.capture.route().to_string()
    }

    fn outcome_notifier(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(self.capture.outcome_notifier())
    }
}

/// A ready capture subsystem, plus everything per-connection capture needs from it.
pub struct CaptureMode {
    /// Held only for retirement and close, which need `&mut` and must be serialised.
    runtime: Mutex<CaptureRuntime>,
    /// Built from the runtime's shared routing state, publisher, and gate, so the accept path
    /// never takes the runtime lock.
    factory: ConnectionCaptureFactory,
    /// capture-kafka's process-wide write gate, held so its terminal failure can be bridged onto
    /// the proxy's capture gate without taking the runtime lock.
    write_gate: Arc<capture_kafka::write_gate::WriteGate>,
    topic: String,
    node_id: Arc<str>,
    payload_limit: u32,
    flush_interval: Duration,
    /// How many times `retire` has been entered. The caller gives it two chances, and the second
    /// follows a force-close, so the two must behave differently.
    retire_attempts: AtomicUsize,
}

impl std::fmt::Debug for CaptureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureMode")
            .field("topic", &self.topic)
            .field("node_id", &self.node_id)
            .field("payload_limit", &self.payload_limit)
            .finish_non_exhaustive()
    }
}

impl CaptureMode {
    /// Brings Kafka to readiness. Returning means the listener may open.
    ///
    /// Blocking by nature: `start_capture` polls metadata, membership, and the first generation
    /// against finite budgets. The caller runs it under `block_in_place` so it cannot stall the
    /// runtime's other workers.
    ///
    /// # Errors
    ///
    /// [`KafkaWiringError`]. Every variant happens **before** the listener binds, so no traffic is
    /// ever accepted by a process that reports one.
    pub fn start(
        config: &ApplicationConfig,
        telemetry: &Arc<RootTelemetry>,
        environment: &dyn capture_config::EnvLookup,
    ) -> Result<Self, KafkaWiringError> {
        let kafka = config
            .sink()
            .kafka()
            .ok_or(KafkaWiringError::NotCaptureMode)?;
        let (settings, oauth) = kafka_settings(config, environment)?;
        let partitions = kafka.required_topic_partitions();

        let payload_limit = payload_limit(&settings, config.max_traffic_buffer_size())?;
        let activation: Arc<str> = Arc::from(telemetry.identity().capture_activation_id());

        let runtime = bootstrap::start_capture(&CaptureStartupConfig {
            settings: settings.clone(),
            timing: config.timing(),
            minimum_partitions: usize::try_from(partitions).unwrap_or(1),
            budget_limits: BudgetLimits::for_lanes(
                kafka.publication_budget().bytes,
                kafka.publication_budget().records,
                u64::from(partitions).max(1),
            ),
            metrics: Arc::new(ContractedKafkaMetrics(Arc::clone(telemetry))),
            oauth,
            capture_activation_id: Arc::clone(&activation),
            metadata_budget: METADATA_BUDGET,
            first_generation_budget: FIRST_GENERATION_BUDGET,
        })?;

        tracing::info!(
            event = "capture_generation_activated",
            topic = settings.topic,
            partitions = runtime.topic_metadata.partition_count(),
            writer_node_id = %runtime.first_writer_node_id,
            "capture is ready; the listener may now open"
        );

        // A sibling factory over the same shared state, so routing a connection does not contend
        // with retirement for the runtime lock.
        let factory = ConnectionCaptureFactory::new(
            Arc::clone(&runtime.routing),
            runtime.publisher.clone(),
            Arc::clone(&runtime.gate),
            config.timing(),
        );
        let node_id = Arc::clone(&runtime.first_writer_node_id);
        let write_gate = Arc::clone(&runtime.gate);

        Ok(Self {
            runtime: Mutex::new(runtime),
            factory,
            write_gate,
            topic: settings.topic,
            node_id,
            payload_limit,
            flush_interval: config.timing().flush_interval(),
            retire_attempts: AtomicUsize::new(0),
        })
    }

    /// The traffic topic, for the start-up summary.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Makes capture-kafka's process-wide terminal failure close the proxy's capture gate.
    ///
    /// `proxyCaptureProtocol.md` §7.1: the first Kafka failure, timeout, ambiguous result, or
    /// heartbeat-deadline expiry "atomically closes every capture-authoritative gate and notifies
    /// the acceptor, connections, heartbeat scheduler, and publisher lane". Without this bridge the
    /// proxy's gate learned of a failure only when some connection happened to pump a failed
    /// outcome, so a heartbeat-deadline expiry on an idle proxy left the process alive, listener
    /// open, refusing every connection under both policies.
    ///
    /// Corruption maps to an unstable failure, which terminates under both policies; everything
    /// else is a required-capture failure that obeys `--capture-failure-policy`. The listener fires
    /// exactly once, immediately if the write gate has already tripped.
    pub fn bridge_terminal_failure(&self, gate: &Arc<crate::CaptureProcessState>) {
        let gate = Arc::clone(gate);
        self.write_gate
            .add_terminal_failure_listener(Box::new(move |failure| {
                let reason = failure.reason();
                let detail = format!("{} (reason={})", failure.message(), reason.as_str());
                let failure = if reason == capture_kafka::error::FailureReason::CorruptedState {
                    crate::CaptureFailure::unstable(detail)
                } else {
                    crate::CaptureFailure::required(detail)
                };
                gate.capture_failed(&failure);
            }));
    }

    /// Routes `connection_id` and builds its capture sink.
    ///
    /// # Errors
    ///
    /// The refusal reason. A refusal means the connection must be **closed**, not served
    /// uncaptured: serving it would be exactly the silent capture gap the protocol forbids.
    pub fn create_sink(
        &self,
        connection_id: &str,
        gate: Arc<crate::CaptureProcessState>,
    ) -> Result<RecordingCaptureSink<KafkaPublishingSink>, String> {
        let capture = self
            .factory
            .create_connection(connection_id)
            .map_err(|refusal| refusal.to_string())?;

        let recorder_config = ConnectionRecorderConfig::new(
            connection_id,
            // The writer identity of the generation this connection was routed to, which is what
            // every record of this connection is stamped with.
            Arc::from(capture.route().writer_node_id()),
        )
        .with_payload_limit(self.payload_limit)
        .with_flush_interval(self.flush_interval);

        RecordingCaptureSink::new(
            recorder_config,
            KafkaPublishingSink {
                capture,
                recycler: Arc::new(HeapRecycler),
            },
            gate,
        )
        .map_err(|error| error.to_string())
    }

    /// How many connections the routing state still has routed to a writer lane.
    fn routed_connection_count(&self) -> usize {
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let routing = runtime
            .routing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routing.connection_count()
    }

    /// Waits until no connection is routed to a writer lane.
    ///
    /// **This wait is load-bearing, and leaving it out is an active hazard rather than a missing
    /// nicety.** `capture_kafka::shutdown::retire_all_writers` refuses outright while any
    /// connection is still routed — "Cannot retire proxy writers while captured connections
    /// remain" — and the publisher treats that refusal as corrupted state: it trips the process
    /// write gate closed with `reason=corrupted_state` and fails the publisher permanently. So
    /// calling it one connection too early does not merely report an incomplete shutdown, it
    /// destroys capture for whatever is still draining.
    ///
    /// The ordering has to be repaired here because neither side can be changed:
    /// [`crate::shutdown::perform_orderly_shutdown`] composes `retire()` **before**
    /// `wait_until_idle()`, and `capture-kafka` requires the opposite. Waiting inside `retire()`
    /// reconciles them, and composes with the caller's deadlines rather than fighting them.
    ///
    /// # Why the first attempt is unbounded and later ones are not
    ///
    /// The caller gives `retire()` two chances. On the **first**, connections are still being
    /// served and are entitled to the whole natural-drain interval, so waiting without an inner
    /// budget is correct: the caller's own `natural_drain` timeout expires, it force-closes every
    /// remaining connection, and it calls `retire()` again. Returning an error on an inner timeout
    /// instead would latch the shutdown as incomplete before the force-close had its chance.
    ///
    /// On a **later** attempt the connections have already been force-closed, so anything still
    /// routed is not draining — it is stuck. Waiting on it until the 240-second retirement deadline
    /// adds four minutes of silence and changes nothing, so the wait is bounded by
    /// [`FORCED_RETIREMENT_WAIT`] and the caller is told promptly and precisely.
    async fn await_no_routed_connections(&self, bound: Option<Duration>) -> Result<(), usize> {
        let deadline = bound.map(|bound| tokio::time::Instant::now() + bound);
        let mut announced = false;
        loop {
            let remaining = self.routed_connection_count();
            if remaining == 0 {
                if announced {
                    tracing::info!("every captured connection has retired; retiring writer lanes");
                }
                return Ok(());
            }
            if !announced {
                announced = true;
                tracing::info!(
                    remaining_connections = remaining,
                    bounded = bound.is_some(),
                    "waiting for captured connections to retire before retiring writer lanes"
                );
            }
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return Err(remaining);
            }
            tokio::time::sleep(RETIREMENT_POLL_INTERVAL).await;
        }
    }
}

/// How long retirement waits for force-closed connections to leave the routing state.
///
/// Only applies after the caller has already force-closed every connection, so this is not a
/// drain budget — it is how long a *stuck* routing entry is given before the shutdown says so.
/// Short, because the shipped Kubernetes deployment gets the default 30-second
/// `terminationGracePeriodSeconds` (open owner decision D6), so a four-minute wait would be
/// pre-empted by `SIGKILL` and the operator would never see the diagnostic at all.
pub const FORCED_RETIREMENT_WAIT: Duration = Duration::from_secs(10);

/// How often retirement re-checks whether the captured connections have drained.
///
/// Short enough that an idle proxy shuts down promptly — the common case in Kubernetes, where the
/// load balancer has already stopped sending traffic — and long enough not to spin on the routing
/// mutex while connections are genuinely draining.
pub const RETIREMENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Resolves the per-record payload ceiling.
///
/// Two independent ceilings apply and the smaller binds: Kafka's own `message.max.bytes`, minus the
/// record overhead Kafka adds (`CaptureKafkaPublisher.java:326-329`), and the configured
/// `--max-traffic-buffer-size`.
fn payload_limit(
    settings: &KafkaClientSettings,
    max_traffic_buffer_size: u32,
) -> Result<u32, KafkaWiringError> {
    let kafka_ceiling = settings.payload_size_limit()?;
    let kafka_ceiling = u32::try_from(kafka_ceiling).unwrap_or(u32::MAX);
    Ok(kafka_ceiling.min(max_traffic_buffer_size))
}

impl CaptureRetirement for CaptureMode {
    /// Drains registered connections and retires every publisher lane.
    ///
    /// §4.3: a lane reaches `RETIRED` only after every connection using it has been fully retired,
    /// including Kafka's acknowledgement of its terminal observation. That acknowledgement cannot be
    /// awaited in `CaptureSink::close`, which is synchronous, so this is where it happens.
    async fn retire(&self) -> Result<(), String> {
        // Must precede `retire_all_writers`; see `await_no_routed_connections` for why this is a
        // safety requirement and not an optimisation. The first attempt is unbounded so the
        // natural drain runs its course; later attempts follow a force-close, so they are bounded.
        let attempt = self.retire_attempts.fetch_add(1, Ordering::SeqCst);
        let bound = if attempt == 0 {
            None
        } else {
            Some(FORCED_RETIREMENT_WAIT)
        };
        if let Err(remaining) = self.await_no_routed_connections(bound).await {
            // Deliberately does NOT call `retire_all_writers`: that call would be refused while a
            // connection is still routed, and the refusal is reported as corrupted state, which
            // trips the process write gate. Reporting an incomplete retirement is the honest
            // outcome; manufacturing a corruption event on the way out is not.
            return Err(format!(
                "capture retirement cannot proceed: {remaining} connection(s) are still routed to \
                 a writer lane {FORCED_RETIREMENT_WAIT:?} after every connection was force-closed, \
                 so no writer lane can reach RETIRED (proxyCaptureProtocol 4.3). Captured data is \
                 not lost — each connection's terminal record was published before this point — \
                 but the retirement bookkeeping did not complete"
            ));
        }

        let timing = ShutdownTiming::default();
        let budget = capture_kafka::shutdown::ShutdownBudget::new(
            timing.natural_drain(),
            timing.retirement_deadline(),
            timing.hook_completion(),
            timing.hard_stop(),
        )
        .map_err(|error| error.to_string())?;

        // `retire_all_writers` polls and blocks; `block_in_place` moves this off the worker so the
        // connections still draining keep being served.
        let outcome = tokio::task::block_in_place(|| {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runtime.retire(budget)
        });

        if outcome.is_complete() {
            Ok(())
        } else {
            Err(format!(
                "capture retirement ended {} with {} writer(s) unretired",
                outcome.label(),
                capture_kafka::shutdown::unretired_writers(
                    &self
                        .runtime
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .routing
                )
                .len()
            ))
        }
    }

    fn capture_compromised(&self) -> bool {
        self.write_gate.failure_if_not_writable().is_some()
    }

    async fn close(&self) -> Result<(), String> {
        tokio::task::block_in_place(|| {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runtime.close();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use capture_config::EnvLookup;

    use super::*;

    #[derive(Default)]
    struct StubEnv(HashMap<String, String>);

    impl StubEnv {
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            )
        }
    }

    impl EnvLookup for StubEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn config(extra: &[&str]) -> ApplicationConfig {
        let mut args = vec![
            "--destination-uri",
            "http://upstream.test",
            "--listen-port",
            "9200",
            "--kafka-brokers",
            "broker-1:9092",
        ];
        args.extend_from_slice(extra);
        ApplicationConfig::from_args(&args, &StubEnv::default()).expect("configuration is valid")
    }

    #[test]
    fn plaintext_kafka_needs_no_auth_material_and_no_token_source() {
        let (settings, oauth) =
            kafka_settings(&config(&[]), &StubEnv::default()).expect("settings resolve");
        assert_eq!(settings.bootstrap_servers, "broker-1:9092");
        assert_eq!(settings.topic, "logging-traffic-topic");
        assert!(matches!(settings.auth, AuthMode::None));
        assert!(oauth.is_none());
    }

    #[test]
    fn scram_carries_both_credentials_rather_than_omitting_the_jaas_line() {
        // KafkaConfig.java:217 omits the JAAS line when either credential is missing, which
        // authenticates as nobody and fails later with an opaque broker error.
        let config = config(&[
            "--kafka-auth-type",
            "scram-sha-512",
            "--kafka-user-name",
            "capture",
            "--kafka-password",
            "hunter2",
        ]);
        let (settings, oauth) =
            kafka_settings(&config, &StubEnv::default()).expect("settings resolve");
        match settings.auth {
            AuthMode::ScramSha512 {
                username, password, ..
            } => {
                assert_eq!(username, "capture");
                assert_eq!(password, "hunter2");
            }
            other => panic!("expected SCRAM, got {}", other.as_str()),
        }
        assert!(oauth.is_none());
    }

    #[test]
    fn msk_iam_without_a_region_is_refused_at_startup() {
        let config = config(&["--kafka-auth-type", "msk-iam"]);
        let Err(error) = kafka_settings(&config, &StubEnv::default()) else {
            panic!("no region is a wiring error");
        };
        assert!(matches!(error, KafkaWiringError::MissingAwsRegion));
        assert!(
            error.is_configuration_error(),
            "a missing region is exit 2, not 78"
        );
        assert!(error.to_string().contains("aws-metadata"));
    }

    #[test]
    fn msk_iam_takes_the_region_from_either_aws_variable() {
        for variable in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
            let kafka = config(&["--kafka-auth-type", "msk-iam"]);
            let kafka = kafka.sink().kafka().expect("capture mode");
            let mode = auth_mode(kafka, &StubEnv::with(&[(variable, "us-east-2")]))
                .expect("the region resolves");
            match mode {
                AuthMode::MskIam(settings) => assert_eq!(settings.region, "us-east-2"),
                other => panic!("expected MSK IAM, got {}", other.as_str()),
            }
        }
    }

    #[test]
    fn ssl_selects_tls_without_sasl() {
        let config = config(&["--kafka-auth-type", "ssl"]);
        let (settings, _) = kafka_settings(&config, &StubEnv::default()).expect("resolves");
        assert!(matches!(settings.auth, AuthMode::Tls(_)));
    }

    #[test]
    fn the_payload_limit_is_the_smaller_of_the_two_ceilings() {
        let mut settings = KafkaClientSettings::new("broker-1:9092");
        settings.max_message_bytes = 1024 * 1024;
        let kafka_ceiling = u32::try_from(
            settings
                .payload_size_limit()
                .expect("the default message size leaves room for overhead"),
        )
        .expect("fits");

        // The traffic buffer is the binding ceiling.
        assert_eq!(
            payload_limit(&settings, 4_096).expect("resolves"),
            4_096,
            "a smaller --max-traffic-buffer-size must win"
        );
        // Kafka's ceiling is the binding one.
        assert_eq!(
            payload_limit(&settings, u32::MAX).expect("resolves"),
            kafka_ceiling,
            "message.max.bytes minus Kafka's record overhead must win"
        );
    }

    #[test]
    fn a_message_max_bytes_property_override_lowers_the_payload_limit() {
        let config = config(&[]);
        let (mut settings, _) = kafka_settings(&config, &StubEnv::default()).expect("resolves");
        let default_limit = payload_limit(&settings, u32::MAX).expect("resolves");
        settings.max_message_bytes = 64 * 1024;
        let lowered = payload_limit(&settings, u32::MAX).expect("resolves");
        assert!(
            lowered < default_limit,
            "a lower message.max.bytes must lower the record ceiling: {lowered} vs {default_limit}"
        );
    }

    #[test]
    fn a_missing_property_file_is_a_startup_failure_not_an_empty_map() {
        let config = config(&["--kafka-property-file", "/nonexistent/capture.properties"]);
        assert!(matches!(
            kafka_settings(&config, &StubEnv::default()),
            Err(KafkaWiringError::ClientConfig(_))
        ));
    }

    #[test]
    fn no_capture_mode_is_rejected_rather_than_half_configured() {
        let config = ApplicationConfig::from_args(
            &[
                "--destination-uri",
                "http://upstream.test",
                "--listen-port",
                "9200",
                "--no-capture",
            ],
            &StubEnv::default(),
        )
        .expect("valid");
        assert!(matches!(
            kafka_settings(&config, &StubEnv::default()),
            Err(KafkaWiringError::NotCaptureMode)
        ));
    }
}
