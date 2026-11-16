//! Client construction and safe startup ordering (traceability row KAFKA-08).
//!
//! # The ordering requirement
//!
//! The listener must **not** open until capability, membership, and the first routing generation
//! are all usable. A client that connects before that point is refused by the operating system,
//! which is the only refusal that cannot lose traffic. So startup runs to completion *before* the
//! socket is bound:
//!
//! 1. generate the process activation id — every writer identity and the probe identity derive
//!    from it;
//! 2. validate the timing intervals and the message size;
//! 3. build the producer and the membership consumer from allowlisted configuration, and read the
//!    producer's **effective** ordering properties back from librdkafka;
//! 4. discover and validate the topic layout, retrying only the transient failures;
//! 5. qualify the topic's timestamp behaviour, one probe per distinct current leader;
//! 6. refresh the layout and re-probe if a **new** leader appeared;
//! 7. construct the routing state, the write gate, and the publisher;
//! 8. start membership, which produces the first assignment;
//! 9. wait for the first routing generation to activate;
//! 10. only then report readiness.
//!
//! Steps 4-6 are the ones with retries, and the split between "retry forever" and "fail now" is
//! deliberate: an absent topic, a leaderless partition, and unreachable metadata are transient
//! facts about the cluster, while a non-contiguous layout, a count below the configured minimum,
//! and any probe failure need a human.
//!
//! # No admin client
//!
//! Nothing here constructs an `AdminClient`, and nothing creates a topic or alters a partition
//! count (owner decision OD-7). That is also why `rust-rdkafka` 0.39's admin IAM defect — every
//! admin call times out because librdkafka never requests an OAUTHBEARER token for it — cannot
//! affect this proxy.
//!
//! # Where the delivery report runs
//!
//! [`CaptureProducerContext::delivery`] runs on librdkafka's own polling thread, never on the
//! publisher actor. It does three things there, in this order, before the actor is involved at all:
//! claims the acknowledgement deadline, releases the publication budget, and samples the
//! submit-to-delivery latency. Only then does it hand a [`DeliveryNotice`] to the actor. The first
//! of those is a protocol requirement — it is what lets an acknowledgement that physically arrived
//! before `E` win even when the actor is backlogged past `E` — and the second is what lets
//! backpressure lift the moment Kafka acknowledges.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rdkafka::TopicPartitionList;
use rdkafka::client::{ClientContext, OAuthToken};
use rdkafka::consumer::{BaseConsumer, Consumer, ConsumerContext, Rebalance};
use rdkafka::error::KafkaError;
use rdkafka::message::Message as _;
use rdkafka::producer::{BaseRecord, DeliveryResult, Producer, ProducerContext, ThreadedProducer};

use crate::budget::{BudgetClass, PublicationBudget};
use crate::client_config::{ClientRole, KafkaClientSettings};
use crate::error::{ClientConfigError, StartupRefusal, TopicLayoutError};
use crate::membership::{MembershipConsumer, MembershipEvent, PollOutcome};
use crate::metrics::{CaptureMetrics, RecordKind};
use crate::msk_iam::MskIamTokenSource;
use crate::publisher_actor::{DeliveryNotice, OutboundRecord, ProduceError, RecordProducer};
use crate::routing_state::RoutingState;
use crate::topic_metadata::{MetadataSource, PartitionLayout, TrafficTopicMetadata};
use crate::write_gate::WriteGate;

/// How long to wait between topic-metadata discovery attempts.
pub const METADATA_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How long a single metadata fetch may take.
pub const METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared client behaviour: the OAUTHBEARER token-refresh callback.
///
/// `ENABLE_REFRESH_OAUTH_TOKEN` is an associated constant, so it cannot vary per instance and is
/// always `true`. That is harmless when the auth mode is not MSK IAM: librdkafka only raises a
/// token-refresh event when the configured mechanism is OAUTHBEARER, so the callback is simply
/// never invoked. The alternative — two context types differing only in a constant — would
/// duplicate every callback for no behavioural gain.
pub struct CaptureClientContext {
    oauth: Option<Arc<MskIamTokenSource>>,
    label: &'static str,
}

impl std::fmt::Debug for CaptureClientContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureClientContext")
            .field("label", &self.label)
            .field("oauth", &self.oauth.is_some())
            .finish()
    }
}

impl CaptureClientContext {
    /// A context for `label`, minting MSK IAM tokens from `oauth` when configured.
    #[must_use]
    pub fn new(label: &'static str, oauth: Option<Arc<MskIamTokenSource>>) -> Self {
        Self { oauth, label }
    }
}

impl ClientContext for CaptureClientContext {
    /// Without this, librdkafka never calls [`ClientContext::generate_oauth_token`] at all and an
    /// MSK IAM client waits until its request timeout with no diagnostic.
    const ENABLE_REFRESH_OAUTH_TOKEN: bool = true;

    fn generate_oauth_token(
        &self,
        _config: Option<&str>,
    ) -> Result<OAuthToken, Box<dyn std::error::Error>> {
        let source = self
            .oauth
            .as_ref()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                "librdkafka requested an OAUTHBEARER token for the capture {} client, but no MSK \
                 IAM token source is configured",
                self.label
            )
            .into()
            })?;
        let token = source.token()?;
        tracing::debug!(
            client = self.label,
            lifetime_ms = token.lifetime_ms,
            "event=capture_kafka_oauth_token_issued"
        );
        Ok(token)
    }
}

/// What the delivery-report thread needs in order to settle one submission without the actor.
#[derive(Debug)]
pub struct DeliveryOpaqueData {
    send_id: u64,
    kind: RecordKind,
    budget_class: BudgetClass,
    bytes: u64,
    submitted_at: Instant,
    deadline: Option<Arc<crate::ack_deadline::AcknowledgementDeadline>>,
}

/// The producer context. Its `delivery` runs on librdkafka's polling thread.
pub struct CaptureProducerContext {
    base: CaptureClientContext,
    deliveries: mpsc::Sender<DeliveryNotice>,
    budget: Arc<PublicationBudget>,
    metrics: Arc<dyn CaptureMetrics>,
    reports: AtomicU64,
}

impl std::fmt::Debug for CaptureProducerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureProducerContext")
            .field("reports", &self.reports.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ClientContext for CaptureProducerContext {
    const ENABLE_REFRESH_OAUTH_TOKEN: bool = true;

    fn generate_oauth_token(
        &self,
        config: Option<&str>,
    ) -> Result<OAuthToken, Box<dyn std::error::Error>> {
        self.base.generate_oauth_token(config)
    }
}

impl ProducerContext for CaptureProducerContext {
    type DeliveryOpaque = Box<DeliveryOpaqueData>;

    fn delivery(&self, result: &DeliveryResult<'_>, opaque: Self::DeliveryOpaque) {
        self.reports.fetch_add(1, Ordering::Relaxed);

        let (partition, timestamp, error) = match result {
            Ok(message) => (Some(message.partition()), message.timestamp(), None),
            Err((error, message)) => (
                Some(message.partition()),
                message.timestamp(),
                Some(error.to_string()),
            ),
        };

        // 1. Claim the deadline HERE, on this thread, before anything is queued to the actor. This
        //    is the whole reason the delivery report is not simply forwarded: an acknowledgement
        //    that physically arrived before `E` must win the race even if the actor does not get
        //    round to processing it until after `E`.
        let (deadline_claimed, deadline_status) = match &opaque.deadline {
            Some(deadline) => {
                let claimed = deadline.acknowledge();
                (Some(claimed), Some(deadline.status()))
            }
            None => (None, None),
        };

        // 2. Release the budget here too, so backpressure lifts the moment Kafka acknowledges
        //    rather than when the actor drains its mailbox.
        self.budget.release(opaque.budget_class, opaque.bytes);

        // 3. Sample the latency where it actually happened.
        self.metrics
            .submit_to_delivery(opaque.kind, opaque.submitted_at.elapsed());

        // Only now is the actor involved, and only for state it alone may mutate.
        drop(self.deliveries.send(DeliveryNotice {
            send_id: opaque.send_id,
            partition,
            timestamp,
            error,
            deadline_claimed,
            deadline_status,
        }));
    }
}

/// The [`RecordProducer`] implementation over librdkafka.
///
/// Uses [`ThreadedProducer`], which owns a polling thread, so delivery reports are serviced
/// promptly and without the actor. **Not** `FutureProducer`: its `DeliveryFuture` yields only
/// `(partition, offset)` and **discards the timestamp**, which would make the entire broker-time
/// protocol unimplementable.
///
/// # Several producers, one owner per partition (owner decision OD-12)
///
/// librdkafka sends one partition per `ProduceRequest`, and idempotence caps each broker connection
/// at five requests in flight. One producer therefore saturates its window long before the Java
/// client, which packs every partition led by a broker into one request (Phase 1 review §5.5).
/// The sink holds [`KafkaClientSettings::producer_count`] independent producers, each with its own
/// broker connections and window.
///
/// Every record for partition `p` goes to producer `p mod N`, a pure function of the partition
/// fixed for the life of the process. That is exactly the ownership rule of
/// `captureAndReplayArchitecture.md` §5.1 and `proxyCaptureProtocol.md`: for each
/// `(writerNodeId, partition)` exactly one producer and one lane owner (the single publisher
/// actor, which still orders every submission), ownership never overlaps, and because a
/// partition's producer never changes there is never a handoff to drain. Heartbeats, traffic and
/// capability probes for a partition all take the same producer, so their submission order — and
/// librdkafka's per-partition delivery-report order — is preserved.
pub struct KafkaProducerSink {
    producers: Vec<ThreadedProducer<CaptureProducerContext>>,
}

impl std::fmt::Debug for KafkaProducerSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaProducerSink")
            .field("producers", &self.producers.len())
            .finish_non_exhaustive()
    }
}

impl KafkaProducerSink {
    /// How many independent producers this sink owns.
    #[must_use]
    pub fn producer_count(&self) -> usize {
        self.producers.len()
    }

    /// The producer that owns `partition` for the life of the process.
    fn owner(&self, partition: i32) -> &ThreadedProducer<CaptureProducerContext> {
        &self.producers[producer_index(partition, self.producers.len())]
    }
}

/// The fixed partition → producer mapping: `partition mod count`.
///
/// Kafka partitions are non-negative; `rem_euclid` keeps the mapping total for any `i32` anyway.
fn producer_index(partition: i32, count: usize) -> usize {
    debug_assert!(
        count > 0,
        "a producer sink always owns at least one producer"
    );
    let count = i64::try_from(count).unwrap_or(i64::MAX).max(1);
    usize::try_from(i64::from(partition).rem_euclid(count)).unwrap_or(0)
}

impl RecordProducer for KafkaProducerSink {
    fn enqueue(&self, record: OutboundRecord) -> Result<(), ProduceError> {
        let opaque = Box::new(DeliveryOpaqueData {
            send_id: record.send_id,
            kind: if record.deadline.is_some() {
                RecordKind::Heartbeat
            } else {
                RecordKind::Traffic
            },
            budget_class: record.budget_class,
            bytes: record.bytes,
            submitted_at: Instant::now(),
            deadline: record.deadline.clone(),
        });

        // An explicit partition, no headers, and no producer timestamp. librdkafka copies the key
        // and payload into its own queue, so the borrow ends when `send` returns.
        let base = BaseRecord::with_opaque_to(&record.topic, opaque)
            .partition(record.partition)
            .key(&record.key)
            .payload(&record.value);

        match self.owner(record.partition).send(base) {
            Ok(()) => Ok(()),
            Err((KafkaError::MessageProduction(code), _)) if is_queue_full(code) => {
                Err(ProduceError::QueueFull)
            }
            Err((error, _)) => Err(ProduceError::Fatal(error.to_string())),
        }
    }

    fn flush(&self, timeout: Duration) -> Result<(), String> {
        // One deadline across all producers, not `timeout` each: shutdown budgets are absolute.
        let deadline = Instant::now() + timeout;
        for (index, producer) in self.producers.iter().enumerate() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            producer
                .flush(remaining)
                .map_err(|error| format!("producer {index}: {error}"))?;
        }
        Ok(())
    }
}

fn is_queue_full(code: rdkafka::types::RDKafkaErrorCode) -> bool {
    matches!(code, rdkafka::types::RDKafkaErrorCode::QueueFull)
}

/// The membership consumer context. Collects rebalance events for the poll loop to drain.
pub struct CaptureConsumerContext {
    base: CaptureClientContext,
    events: Mutex<Vec<MembershipEvent>>,
}

impl std::fmt::Debug for CaptureConsumerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureConsumerContext")
            .finish_non_exhaustive()
    }
}

impl ClientContext for CaptureConsumerContext {
    const ENABLE_REFRESH_OAUTH_TOKEN: bool = true;

    fn generate_oauth_token(
        &self,
        config: Option<&str>,
    ) -> Result<OAuthToken, Box<dyn std::error::Error>> {
        self.base.generate_oauth_token(config)
    }
}

impl ConsumerContext for CaptureConsumerContext {
    /// Called after librdkafka has applied the assignment, which is the analogue of pausing inside
    /// Java's `onPartitionsAssigned`.
    fn post_rebalance(&self, consumer: &BaseConsumer<Self>, rebalance: &Rebalance<'_>) {
        let event = match rebalance {
            Rebalance::Assign(partitions) => MembershipEvent::Assigned(partition_ids(partitions)),
            Rebalance::Revoke(partitions) => {
                // `Rebalance` has no `Lost` variant: librdkafka signals loss as a revoke with
                // `assignment_lost()` set. The distinction is kept for logging only, because
                // Java's revoke and loss handlers are byte-identical and both merely remove from
                // the accumulator.
                let ids = partition_ids(partitions);
                if Consumer::assignment_lost(consumer) {
                    MembershipEvent::Lost(ids)
                } else {
                    MembershipEvent::Revoked(ids)
                }
            }
            Rebalance::Error(error) => MembershipEvent::Error(error.to_string()),
        };
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

fn partition_ids(partitions: &TopicPartitionList) -> Vec<i32> {
    partitions
        .elements()
        .iter()
        .map(rdkafka::topic_partition_list::TopicPartitionListElem::partition)
        .collect()
}

/// The [`MembershipConsumer`] implementation over librdkafka.
pub struct KafkaMembershipConsumer {
    consumer: BaseConsumer<CaptureConsumerContext>,
}

impl std::fmt::Debug for KafkaMembershipConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaMembershipConsumer")
            .finish_non_exhaustive()
    }
}

impl MembershipConsumer for KafkaMembershipConsumer {
    fn subscribe(&self, topic: &str) -> Result<(), String> {
        // `subscribe`, never `assign`: the group protocol performs the assignment, and the capture
        // protocol explicitly requires no custom assignor.
        Consumer::subscribe(&self.consumer, &[topic]).map_err(|error| error.to_string())
    }

    fn poll_once(&self, timeout: Duration) -> (PollOutcome, Vec<MembershipEvent>) {
        let polled = self.consumer.poll(timeout);
        let events = std::mem::take(
            &mut *self
                .consumer
                .context()
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let outcome = match polled {
            None => PollOutcome::Idle,
            // A paused consumer must never fetch a record.
            Some(Ok(_)) => PollOutcome::Records,
            Some(Err(error)) => {
                if is_retriable_consumption_error(&error) {
                    PollOutcome::Retriable(error.to_string())
                } else {
                    PollOutcome::Fatal(error.to_string())
                }
            }
        };
        (outcome, events)
    }

    fn pause(&self, partitions: &[i32]) -> Result<(), String> {
        if partitions.is_empty() {
            return Ok(());
        }
        let assignment = Consumer::assignment(&self.consumer).map_err(|e| e.to_string())?;
        Consumer::pause(&self.consumer, &assignment).map_err(|error| error.to_string())
    }

    fn close(&self) {
        // `BaseConsumer`'s drop closes the queue and leaves the group when a `group.id` is set,
        // which covers Java's `consumer.close(30s)`. There is no `wakeup` in the consumer trait, so
        // the poll loop relies on its short poll timeout plus the closed flag instead.
        Consumer::unsubscribe(&self.consumer);
    }
}

/// Whether a consumption error is the analogue of Kafka's `RetriableException`.
fn is_retriable_consumption_error(error: &KafkaError) -> bool {
    match error {
        KafkaError::MessageConsumption(code) => {
            use rdkafka::types::RDKafkaErrorCode as Code;
            matches!(
                code,
                Code::BrokerTransportFailure
                    | Code::AllBrokersDown
                    | Code::RequestTimedOut
                    | Code::NotCoordinator
                    | Code::CoordinatorNotAvailable
                    | Code::CoordinatorLoadInProgress
                    | Code::RebalanceInProgress
                    | Code::LeaderNotAvailable
                    | Code::NotLeaderForPartition
            )
        }
        _ => false,
    }
}

/// The constructed clients.
pub struct CaptureClients {
    /// The traffic producers (one sink, several producers; see [`KafkaProducerSink`]).
    pub producer: Arc<KafkaProducerSink>,
    /// The paused membership consumer.
    pub consumer: Arc<KafkaMembershipConsumer>,
    /// Delivery reports, to be forwarded to the publisher actor.
    pub deliveries: mpsc::Receiver<DeliveryNotice>,
}

impl std::fmt::Debug for CaptureClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureClients").finish_non_exhaustive()
    }
}

/// Builds the producer and the membership consumer.
///
/// # Errors
///
/// * [`ClientConfigError`] for a rejected configuration, including an unknown property key, which
///   librdkafka surfaces at client creation.
/// * [`ClientConfigError::EffectiveConfigMismatch`] when the producer's **effective** ordering
///   properties do not preserve per-partition append order.
pub fn connect(
    settings: &KafkaClientSettings,
    budget: Arc<PublicationBudget>,
    metrics: Arc<dyn CaptureMetrics>,
    oauth: Option<Arc<MskIamTokenSource>>,
) -> Result<CaptureClients, ClientConfigError> {
    let producer_config = settings.build(ClientRole::Producer)?;
    // Closes spec discrepancy D-1: the effective values are read back from librdkafka before this
    // process can become capture-authoritative, which Java never does.
    KafkaClientSettings::assert_effective_ordering(&producer_config)?;

    // Every producer reports into the same channel. Per-partition report order is preserved
    // because a partition only ever has one producer.
    let (deliveries_tx, deliveries_rx) = mpsc::channel();
    let count = settings.producer_count.max(1);
    // One shared budget and metrics handle per producer; the last producer takes the originals.
    let mut shared: Vec<_> = (1..count)
        .map(|_| (Arc::clone(&budget), Arc::clone(&metrics)))
        .collect();
    shared.push((budget, metrics));
    let mut producers = Vec::with_capacity(count);
    for (budget, metrics) in shared {
        let producer_context = CaptureProducerContext {
            base: CaptureClientContext::new("producer", oauth.clone()),
            deliveries: deliveries_tx.clone(),
            budget,
            metrics,
            reports: AtomicU64::new(0),
        };
        let producer: ThreadedProducer<CaptureProducerContext> = producer_config
            .create_with_context(producer_context)
            .map_err(|error| ClientConfigError::ClientCreation {
                role: "producer",
                detail: error.to_string(),
            })?;
        producers.push(producer);
    }
    // The forwarder ends when the last producer is dropped, which closes the channel.
    drop(deliveries_tx);

    let consumer_config = settings.build(ClientRole::MembershipConsumer)?;
    let consumer_context = CaptureConsumerContext {
        base: CaptureClientContext::new("membership consumer", oauth),
        events: Mutex::new(Vec::new()),
    };
    let consumer: BaseConsumer<CaptureConsumerContext> = consumer_config
        .create_with_context(consumer_context)
        .map_err(|error| ClientConfigError::ClientCreation {
            role: "membership consumer",
            detail: error.to_string(),
        })?;

    Ok(CaptureClients {
        producer: Arc::new(KafkaProducerSink { producers }),
        consumer: Arc::new(KafkaMembershipConsumer { consumer }),
        deliveries: deliveries_rx,
    })
}

/// Forwards delivery reports from the producer's polling thread to the publisher actor.
///
/// The forwarder is a plain relay and deliberately does **no** work: every decision that must
/// happen before the actor — claiming the acknowledgement deadline, releasing the publication
/// budget, sampling the submit-to-delivery latency — has already happened in
/// [`CaptureProducerContext::delivery`], on librdkafka's thread. Putting any of it here would
/// reintroduce exactly the queueing this design exists to avoid.
///
/// The thread ends when the producer is dropped, which closes the channel.
///
/// # Errors
///
/// Returns the OS error if the thread cannot be spawned.
pub fn spawn_delivery_forwarder(
    deliveries: mpsc::Receiver<DeliveryNotice>,
    publisher: crate::publisher_actor::PublisherHandle,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("capture-kafka-delivery-relay".to_owned())
        .spawn(move || {
            for notice in deliveries {
                // A closed actor needs no delivery reports; the publication futures it owned were
                // already failed by its own failure path.
                if publisher.deliver(notice).is_err() {
                    break;
                }
            }
        })
}

/// A [`MetadataSource`] over a producer handle.
///
/// Producer metadata, never `AdminClient`: see the module documentation and owner decision OD-7.
pub struct ProducerMetadataSource(Arc<KafkaProducerSink>);

impl ProducerMetadataSource {
    /// Wraps `producer`.
    #[must_use]
    pub fn new(producer: Arc<KafkaProducerSink>) -> Self {
        Self(producer)
    }
}

impl MetadataSource for ProducerMetadataSource {
    fn fetch_partitions(
        &self,
        topic: &str,
        timeout: Duration,
    ) -> Result<Vec<PartitionLayout>, String> {
        // Any producer sees the same cluster metadata; the first always exists.
        let metadata = self.0.producers[0]
            .client()
            .fetch_metadata(Some(topic), timeout)
            .map_err(|error| error.to_string())?;
        Ok(crate::topic_metadata::partitions_from_metadata(
            &metadata, topic,
        ))
    }
}

/// Discovers and validates the topic layout, retrying only the transient failures.
///
/// # Errors
///
/// [`StartupRefusal::InvalidTopicLayout`] for a layout only an operator can fix: a non-contiguous
/// partition list, or a count below the configured minimum (owner decision OD-7 — the proxy will
/// never grow the topic, so retrying would loop forever).
///
/// A transient failure — no partitions yet, a leaderless partition, unreachable metadata — is
/// retried until `budget` elapses, then reported as that transient failure so the caller can decide
/// whether to keep waiting.
pub fn discover_topic_layout(
    source: &dyn MetadataSource,
    topic: &str,
    minimum_partitions: usize,
    budget: Duration,
) -> Result<TrafficTopicMetadata, StartupRefusal> {
    let deadline = Instant::now() + budget;
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        match TrafficTopicMetadata::discover(
            source,
            topic,
            minimum_partitions,
            METADATA_FETCH_TIMEOUT,
        ) {
            Ok(metadata) => {
                tracing::info!(
                    topic,
                    attempts,
                    partitions = metadata.partition_count(),
                    leaders = ?metadata.leader_ids(),
                    representative_partitions = ?metadata.representative_partitions(),
                    "event=capture_topic_layout_validated"
                );
                return Ok(metadata);
            }
            Err(error) if error.is_retriable() => {
                if Instant::now() >= deadline {
                    return Err(StartupRefusal::InvalidTopicLayout(error));
                }
                tracing::warn!(
                    topic,
                    attempts,
                    detail = %error,
                    "event=capture_topic_metadata_unavailable reason=retrying"
                );
                std::thread::sleep(METADATA_RETRY_DELAY);
            }
            Err(error) => {
                // Not retriable. The proxy never creates a topic and never grows one, so waiting
                // would never change the answer.
                return Err(StartupRefusal::InvalidTopicLayout(error));
            }
        }
    }
}

/// Reports why a layout error is or is not worth retrying, for the startup log.
#[must_use]
pub fn layout_retry_advice(error: &TopicLayoutError) -> &'static str {
    if error.is_retriable() {
        "transient: the cluster may still converge"
    } else {
        "an operator must act: the proxy never creates or grows the traffic topic"
    }
}

// ---------------------------------------------------------------- the ordered startup

/// The channel the ordered startup waits on for its first usable routing generation.
type FirstGenerationReply = mpsc::Sender<Result<Arc<str>, StartupRefusal>>;

/// Everything the ordered startup needs.
pub struct CaptureStartupConfig {
    /// Broker, topic, auth, and property-file configuration.
    pub settings: KafkaClientSettings,
    /// The validated `F`/`H`/`E`/`S` intervals.
    pub timing: capture_config::CaptureTiming,
    /// The partition count the deployment requires. The proxy validates it and never provides it
    /// (owner decision OD-7).
    pub minimum_partitions: usize,
    /// Bounded publication memory.
    pub budget_limits: crate::budget::BudgetLimits,
    /// The metric sink.
    pub metrics: Arc<dyn CaptureMetrics>,
    /// The MSK IAM token source, when the auth mode needs one.
    pub oauth: Option<Arc<MskIamTokenSource>>,
    /// This process's activation id. Every writer identity and the probe identity derive from it.
    pub capture_activation_id: Arc<str>,
    /// How long to keep retrying transient metadata failures.
    pub metadata_budget: Duration,
    /// How long to wait for the first routing generation to become usable.
    pub first_generation_budget: Duration,
}

impl std::fmt::Debug for CaptureStartupConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureStartupConfig")
            .field("topic", &self.settings.topic)
            .field("auth", &self.settings.auth.as_str())
            .field("minimum_partitions", &self.minimum_partitions)
            .finish_non_exhaustive()
    }
}

/// A started, ready capture subsystem. **The listener may open only once this exists.**
pub struct CaptureRuntime {
    /// Creates per-connection capture.
    pub factory: crate::connection_factory::ConnectionCaptureFactory,
    /// The publisher handle.
    pub publisher: crate::publisher_actor::PublisherHandle,
    /// The shared routing state.
    pub routing: Arc<Mutex<RoutingState>>,
    /// The process-wide capture write gate.
    pub gate: Arc<WriteGate>,
    /// The validated topic layout as it was at startup.
    pub topic_metadata: TrafficTopicMetadata,
    /// The writer identity of the first activated generation.
    pub first_writer_node_id: Arc<str>,
    publisher_runtime: crate::publisher_actor::PublisherRuntime,
    deadline_clock: crate::ack_deadline::DeadlineClock,
    membership: crate::membership::Membership,
    delivery_relay: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for CaptureRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureRuntime")
            .field("topic", &self.topic_metadata.topic())
            .field("first_writer_node_id", &self.first_writer_node_id)
            .field("gate", &self.gate.state_label())
            .finish_non_exhaustive()
    }
}

impl CaptureRuntime {
    /// Begins orderly retirement and reports the outcome.
    #[must_use]
    pub fn retire(
        &self,
        budget: crate::shutdown::ShutdownBudget,
    ) -> crate::shutdown::ShutdownOutcome {
        crate::shutdown::retire_all_writers(&self.publisher, &self.routing, budget)
    }

    /// Stops membership, the publisher, and the deadline clock.
    pub fn close(&mut self) {
        self.membership.close();
        drop(self.publisher.close());
        self.publisher_runtime.join();
        self.deadline_clock.stop();
        if let Some(relay) = self.delivery_relay.take() {
            drop(relay.join());
        }
    }
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        self.close();
    }
}

/// Runs the whole ordered startup and returns only when capture is **ready**.
///
/// On success the caller may bind its listener. Until then a client must be refused by the
/// operating system, which is the only refusal that cannot lose traffic.
///
/// The order is the one the module documentation lists, and the sequencing is load-bearing: the
/// topic's timestamp behaviour is qualified before any traffic can arrive, and the first routing
/// generation is activated — meaning an initial heartbeat was acknowledged **and accepted** on
/// every assigned partition — before readiness is reported.
///
/// # Errors
///
/// Any [`StartupRefusal`]. Every one of them happens before the listener opens, so no traffic is
/// ever accepted by a process that reports one. In particular
/// [`StartupRefusal::TopicIsNotLogAppendTime`] is the owner decision OD-5 refusal.
pub fn start_capture(config: &CaptureStartupConfig) -> Result<CaptureRuntime, StartupRefusal> {
    let topic = config.settings.topic.clone();
    let payload_size_limit = config.settings.payload_size_limit()?;
    let budget = Arc::new(
        crate::budget::PublicationBudget::new(config.budget_limits).map_err(|error| {
            StartupRefusal::Configuration(ClientConfigError::AuthUnavailable {
                mode: "publication budget",
                detail: error.to_string(),
            })
        })?,
    );

    // Steps 3 to 6: clients, layout validation, and timestamp qualification. All of it must
    // succeed before any state that could accept traffic exists.
    let (clients, refreshed) = connect_and_qualify(config, Arc::clone(&budget))?;

    // The membership consumer handle is taken before the delivery receiver is moved out of
    // `clients`, so the two can be used independently.
    let consumer = Arc::clone(&clients.consumer);

    // 7. Routing state, the write gate, and the publisher.
    let routing = Arc::new(Mutex::new(
        RoutingState::new(
            Arc::clone(&config.capture_activation_id),
            refreshed.partition_count(),
        )
        .map_err(|error| StartupRefusal::FirstGeneration(error.to_string()))?,
    ));
    let gate = Arc::new(WriteGate::new(Box::new(GateMetrics(Arc::clone(
        &config.metrics,
    )))));
    let refresh_partition_count = {
        let source = ProducerMetadataSource::new(Arc::clone(&clients.producer));
        let topic = topic.clone();
        let minimum = config.minimum_partitions;
        Arc::new(move || {
            TrafficTopicMetadata::discover(&source, &topic, minimum, METADATA_FETCH_TIMEOUT)
                .ok()
                .map(|metadata| metadata.partition_count())
        })
    };
    let (publisher_runtime, deadline_clock) =
        crate::publisher_actor::start(crate::publisher_actor::PublisherConfig {
            topic: topic.clone(),
            timing: config.timing,
            payload_size_limit,
            routing: Arc::clone(&routing),
            gate: Arc::clone(&gate),
            budget,
            metrics: Arc::clone(&config.metrics),
            producer: Arc::clone(&clients.producer) as Arc<_>,
            refresh_partition_count,
        })
        .map_err(|error| {
            StartupRefusal::FirstGeneration(format!("could not start the publisher: {error}"))
        })?;
    let publisher = publisher_runtime.handle.clone();
    let delivery_relay =
        spawn_delivery_forwarder(clients.deliveries, publisher.clone()).map_err(|error| {
            StartupRefusal::FirstGeneration(format!("could not start the delivery relay: {error}"))
        })?;

    // Steps 8 and 9: membership produces the first assignment, and the first routing
    // generation must activate before readiness is reported.
    let (membership, first_writer_node_id) =
        start_membership_and_await_first_generation(config, &consumer, &publisher, &gate, &topic)?;

    // 10. Ready. The listener may open.
    tracing::info!(
        topic,
        capture_activation_id = %config.capture_activation_id,
        writer_node_id = %first_writer_node_id,
        partitions = refreshed.partition_count(),
        "event=capture_ready"
    );

    let factory = crate::connection_factory::ConnectionCaptureFactory::new(
        Arc::clone(&routing),
        publisher.clone(),
        Arc::clone(&gate),
        config.timing,
    );
    Ok(CaptureRuntime {
        factory,
        publisher,
        routing,
        gate,
        topic_metadata: refreshed,
        first_writer_node_id,
        publisher_runtime,
        deadline_clock,
        membership,
        delivery_relay: Some(delivery_relay),
    })
}

/// Starts membership and waits for the first routing generation to become usable.
///
/// Split out of [`start_capture`] because it is the step that gates readiness: until it
/// returns, a client must still be refused by the operating system.
///
/// # Errors
///
/// [`StartupRefusal::Membership`] when the poll thread cannot start or membership fails before
/// the first usable assignment, and [`StartupRefusal::FirstGeneration`] when no generation
/// becomes usable within the configured budget.
fn start_membership_and_await_first_generation(
    config: &CaptureStartupConfig,
    consumer: &Arc<KafkaMembershipConsumer>,
    publisher: &crate::publisher_actor::PublisherHandle,
    gate: &Arc<WriteGate>,
    topic: &str,
) -> Result<(crate::membership::Membership, Arc<str>), StartupRefusal> {
    // 8. Membership, which produces the first assignment.
    let tracker = Arc::new(Mutex::new(crate::membership::MembershipTracker::new()));
    let (ready_tx, ready_rx) = mpsc::channel();
    let observer: Arc<dyn crate::membership::MembershipObserver> = Arc::new(StartupObserver {
        publisher: publisher.clone(),
        tracker: Arc::clone(&tracker),
        first_result: Mutex::new(Some(ready_tx)),
        gate: Arc::clone(gate),
    });
    let membership = crate::membership::Membership::start_with_tracker(
        Arc::clone(consumer) as Arc<_>,
        topic.to_owned(),
        Arc::clone(&observer),
        Arc::clone(&tracker),
    )
    .map_err(|error| {
        StartupRefusal::Membership(format!("could not start the membership thread: {error}"))
    })?;

    // 9. Wait for the first routing generation to become usable.
    let first_writer_node_id = match ready_rx.recv_timeout(config.first_generation_budget) {
        Ok(Ok(writer)) => writer,
        Ok(Err(refusal)) => return Err(refusal),
        Err(_) => {
            return Err(StartupRefusal::FirstGeneration(format!(
                "no routing generation became usable within {:?}; the group assigned no partition, \
                 or an initial heartbeat was never acknowledged",
                config.first_generation_budget
            )));
        }
    };

    Ok((membership, first_writer_node_id))
}

/// Builds the clients, validates the topic layout, and qualifies the topic's timestamp behaviour.
///
/// Split out of [`start_capture`] because it is the part with retries and re-probing, and because
/// it must all succeed before any state that could accept traffic exists.
fn connect_and_qualify(
    config: &CaptureStartupConfig,
    budget: Arc<crate::budget::PublicationBudget>,
) -> Result<(CaptureClients, TrafficTopicMetadata), StartupRefusal> {
    let topic = config.settings.topic.clone();

    // 3. Clients, with the producer's effective ordering read back from librdkafka.
    let clients = connect(
        &config.settings,
        budget,
        Arc::clone(&config.metrics),
        config.oauth.clone(),
    )?;

    // 4. Layout discovery and validation. No admin call, no topic creation, no partition change.
    let source = ProducerMetadataSource::new(Arc::clone(&clients.producer));
    let topic_metadata = discover_topic_layout(
        &source,
        &topic,
        config.minimum_partitions,
        config.metadata_budget,
    )?;

    // 5. Qualify the topic's timestamp behaviour, one probe per distinct current leader.
    qualify_topic(
        &clients,
        &topic,
        &config.capture_activation_id,
        &topic_metadata,
    )?;

    // 6. Refresh, and re-probe if a *new* leader appeared: a broker that was never qualified must
    //    not be accepting capture writes. A leader that merely disappeared needs no re-probe.
    let refreshed = discover_topic_layout(
        &source,
        &topic,
        config.minimum_partitions,
        config.metadata_budget,
    )?;
    if !topic_metadata.covers_leaders_of(&refreshed) {
        tracing::info!(
            topic,
            probed_leaders = ?topic_metadata.leader_ids(),
            refreshed_leaders = ?refreshed.leader_ids(),
            "event=capture_topic_leadership_changed reason=reprobing"
        );
        qualify_topic(&clients, &topic, &config.capture_activation_id, &refreshed)?;
    }
    Ok((clients, refreshed))
}

/// Produces one probe per distinct current leader and requires every acknowledgement to carry
/// `Timestamp::LogAppendTime` with a positive value (owner decisions OD-4 and OD-5).
fn qualify_topic(
    clients: &CaptureClients,
    topic: &str,
    capture_activation_id: &str,
    metadata: &TrafficTopicMetadata,
) -> Result<(), StartupRefusal> {
    let representatives = metadata.representative_partitions();
    let mut probe = crate::capability_probe::CapabilityProbe::new(
        topic,
        capture_activation_id,
        &representatives,
        &crate::capability_probe::fresh_probe_ids(representatives.len()),
    )?;
    probe.submit(clients.producer.as_ref())?;

    let deadline = Instant::now() + METADATA_FETCH_TIMEOUT * 3;
    while !probe.is_complete() {
        if Instant::now() >= deadline {
            return Err(StartupRefusal::CapabilityProbe(format!(
                "the capability probe for {topic} did not complete; partitions {:?} were never \
                 acknowledged",
                probe.outstanding_partitions()
            )));
        }
        if let Ok(notice) = clients.deliveries.recv_timeout(Duration::from_millis(250)) {
            probe.accept(&notice)?;
        }
    }
    tracing::info!(
        topic,
        qualified_partitions = ?probe.accepted_partitions(),
        "event=capture_topic_qualified"
    );
    Ok(())
}

/// Adapts the shared metric sink for the write gate, which owns its own boxed sink.
struct GateMetrics(Arc<dyn CaptureMetrics>);

impl CaptureMetrics for GateMetrics {
    fn write_gate_tripped(&self, reason: crate::error::FailureReason) {
        self.0.write_gate_tripped(reason);
    }

    fn write_rejected(&self) {
        self.0.write_rejected();
    }
}

/// Turns membership assignments into routing generations, and latches the first usable one.
struct StartupObserver {
    publisher: crate::publisher_actor::PublisherHandle,
    tracker: Arc<Mutex<crate::membership::MembershipTracker>>,
    first_result: Mutex<Option<FirstGenerationReply>>,
    gate: Arc<WriteGate>,
}

impl StartupObserver {
    /// Reports the first generation's outcome exactly once.
    fn report_first(&self, outcome: Result<Arc<str>, StartupRefusal>) {
        if let Some(sender) = self
            .first_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            drop(sender.send(outcome));
        }
    }
}

impl crate::membership::MembershipObserver for StartupObserver {
    fn request_generation(&self, partitions: Vec<i32>) {
        // Must not block: the poll loop has to keep heartbeating to the group, and a generation
        // that waited here would stall membership and provoke another rebalance. A generation
        // happens at most once per rebalance, so a short-lived thread per request is cheap.
        let reply = match self.publisher.initialize_generation(partitions) {
            Ok(reply) => reply,
            Err(failure) => {
                self.report_first(Err(StartupRefusal::FirstGeneration(
                    failure.message().to_owned(),
                )));
                return;
            }
        };
        let tracker = Arc::clone(&self.tracker);
        let publisher = self.publisher.clone();
        let first_result = {
            let mut guard = self
                .first_result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.take()
        };
        let gate = Arc::clone(&self.gate);
        let spawned = std::thread::Builder::new()
            .name("capture-kafka-generation-watch".to_owned())
            .spawn(move || {
                let outcome = reply.recv();
                match outcome {
                    Ok(Ok(writer)) => {
                        // Latch "the first routing generation is usable" so a later membership
                        // failure takes the post-startup path and does not fail capture.
                        tracker
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .mark_initial_routing_ready();
                        if let Some(sender) = first_result {
                            drop(sender.send(Ok(writer)));
                        }
                    }
                    Ok(Err(failure)) => {
                        if let Some(sender) = first_result {
                            drop(sender.send(Err(StartupRefusal::FirstGeneration(
                                failure.message().to_owned(),
                            ))));
                        }
                    }
                    Err(_) => {
                        let detail = publisher.failure().map_or_else(
                            || "the publisher stopped before the generation activated".to_owned(),
                            |failure| failure.message().to_owned(),
                        );
                        drop(gate.failure_if_not_writable());
                        if let Some(sender) = first_result {
                            drop(sender.send(Err(StartupRefusal::FirstGeneration(detail))));
                        }
                    }
                }
            });
        if spawned.is_err() {
            self.report_first(Err(StartupRefusal::FirstGeneration(
                "could not start the generation watch thread".to_owned(),
            )));
        }
    }

    fn initial_routing_ready(&self) {}

    fn startup_failed(&self, refusal: StartupRefusal) {
        self.report_first(Err(refusal));
    }

    fn degraded_after_startup(&self, failure: crate::error::CaptureFailure) {
        // Membership is explicitly not a capture gate, so this does **not** fail capture. It is
        // recorded with an attributable reason so the state is alarmable: the proxy now runs
        // permanently with the last usable assignment and no membership.
        tracing::error!(
            reason = failure.reason().as_str(),
            detail = failure.message(),
            "event=capture_membership_dead capture_continues=true"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::budget::BudgetLimits;
    use crate::metrics::NoopMetrics;
    use crate::topic_metadata::FnMetadataSource;

    fn budget() -> Arc<PublicationBudget> {
        Arc::new(
            PublicationBudget::new(BudgetLimits::for_lanes(1024 * 1024, 128, 4))
                .expect("valid budget"),
        )
    }

    #[test]
    fn each_partition_has_exactly_one_producer_for_the_life_of_the_process() {
        // OD-12 / architecture §5.1: one owner per (writer, partition), never overlapping. The
        // mapping is a pure function of the partition, so asking twice — or after any rebalance —
        // always names the same producer.
        for count in 1..=8 {
            for partition in 0..256 {
                let owner = producer_index(partition, count);
                assert!(owner < count);
                assert_eq!(owner, producer_index(partition, count));
            }
        }
        // Contiguous assignments (librdkafka's default `range` assignor) spread evenly.
        let mut load = [0_usize; 4];
        for partition in 4..12 {
            load[producer_index(partition, 4)] += 1;
        }
        assert_eq!(load, [2, 2, 2, 2]);
        // Total for any i32, though Kafka never produces a negative partition.
        assert!(producer_index(-1, 4) < 4);
        assert!(producer_index(i32::MIN, 3) < 3);
    }

    #[test]
    fn the_sink_builds_the_configured_number_of_producers() {
        let mut settings = KafkaClientSettings::new("localhost:9092");
        settings.producer_count = 3;
        let clients = connect(&settings, budget(), Arc::new(NoopMetrics), None)
            .expect("clients construct without a reachable broker");
        assert_eq!(clients.producer.producer_count(), 3);
    }

    #[test]
    fn the_clients_are_constructed_with_ordering_preserved() {
        // The whole configuration path exercised against librdkafka, including the effective
        // read-back. No broker is contacted by client creation.
        let settings = KafkaClientSettings::new("localhost:9092");
        let clients = connect(&settings, budget(), Arc::new(NoopMetrics), None)
            .expect("librdkafka accepts the capture configuration");
        // The channel exists and is empty: no delivery report can have arrived.
        assert!(clients.deliveries.try_recv().is_err());
    }

    #[test]
    fn an_unknown_property_key_is_refused_before_a_client_is_created() {
        let mut settings = KafkaClientSettings::new("localhost:9092");
        settings
            .properties
            .insert("not.a.real.property".to_owned(), "1".to_owned());
        let error = connect(&settings, budget(), Arc::new(NoopMetrics), None)
            .expect_err("an unknown key must not reach librdkafka");
        assert!(matches!(
            error,
            ClientConfigError::PropertyNotAllowed { .. }
        ));
    }

    #[test]
    fn a_weakened_ordering_property_is_refused_before_a_client_is_created() {
        let mut settings = KafkaClientSettings::new("localhost:9092");
        settings
            .properties
            .insert("enable.idempotence".to_owned(), "false".to_owned());
        assert!(matches!(
            connect(&settings, budget(), Arc::new(NoopMetrics), None),
            Err(ClientConfigError::CannotWeakenOrdering { .. })
        ));
    }

    #[test]
    fn msk_iam_clients_are_constructed_with_the_oauth_callback_enabled() {
        // The constant is what makes librdkafka call the callback at all; without it an MSK IAM
        // client waits silently until its request timeout.
        // Compile-time: librdkafka never requests a token without this constant, so a regression
        // here must fail the build rather than a test run.
        const _: () = assert!(<CaptureClientContext as ClientContext>::ENABLE_REFRESH_OAUTH_TOKEN);
        const _: () =
            assert!(<CaptureProducerContext as ClientContext>::ENABLE_REFRESH_OAUTH_TOKEN);
        const _: () =
            assert!(<CaptureConsumerContext as ClientContext>::ENABLE_REFRESH_OAUTH_TOKEN);
    }

    #[test]
    fn a_token_request_without_a_configured_source_fails_with_a_clear_message() {
        let context = CaptureClientContext::new("producer", None);
        // `OAuthToken` is not Debug, so the error is extracted rather than unwrapped.
        let text = match context.generate_oauth_token(None) {
            Ok(_) => panic!("a token must not be minted without a configured source"),
            Err(error) => error.to_string(),
        };
        assert!(text.contains("MSK IAM token source"), "{text}");
        assert!(
            !text.contains('\0'),
            "librdkafka rejects a message with an embedded null"
        );
    }

    #[test]
    fn discovery_retries_a_transient_failure_and_then_succeeds() {
        let attempts = AtomicU64::new(0);
        let source = FnMetadataSource(|_topic: &str, _timeout: Duration| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                // No leader yet: an election in progress.
                Ok(vec![PartitionLayout {
                    partition: 0,
                    leader: -1,
                }])
            } else {
                Ok(vec![PartitionLayout {
                    partition: 0,
                    leader: 3,
                }])
            }
        });
        let metadata = discover_topic_layout(&source, "traffic", 1, Duration::from_secs(30))
            .expect("the second attempt succeeds");
        assert_eq!(metadata.partition_count(), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn discovery_does_not_retry_a_layout_only_an_operator_can_fix() {
        // Owner decision OD-7: the proxy never grows the topic, so retrying would loop forever.
        let attempts = AtomicU64::new(0);
        let source = FnMetadataSource(|_topic: &str, _timeout: Duration| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PartitionLayout {
                partition: 0,
                leader: 1,
            }])
        });
        let error = discover_topic_layout(&source, "traffic", 2, Duration::from_secs(30))
            .expect_err("one partition, two required");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a non-retriable layout is reported immediately, not polled"
        );
        match error {
            StartupRefusal::InvalidTopicLayout(layout) => {
                assert!(!layout.is_retriable());
                assert_eq!(
                    layout_retry_advice(&layout),
                    "an operator must act: the proxy never creates or grows the traffic topic"
                );
            }
            other => panic!("expected a layout refusal, got {other}"),
        }
    }

    #[test]
    fn discovery_gives_up_on_a_transient_failure_once_its_budget_elapses() {
        let polled = Arc::new(AtomicBool::new(false));
        let source = {
            let polled = Arc::clone(&polled);
            FnMetadataSource(move |_topic: &str, _timeout: Duration| {
                polled.store(true, Ordering::SeqCst);
                Err("Broker transport failure".to_owned())
            })
        };
        // A zero budget still makes exactly one attempt, so a one-shot caller gets a real answer.
        let error = discover_topic_layout(&source, "traffic", 0, Duration::ZERO)
            .expect_err("the broker is unreachable");
        assert!(polled.load(Ordering::SeqCst));
        match error {
            StartupRefusal::InvalidTopicLayout(layout) => {
                assert!(layout.is_retriable());
                assert_eq!(
                    layout_retry_advice(&layout),
                    "transient: the cluster may still converge"
                );
            }
            other => panic!("expected a layout refusal, got {other}"),
        }
    }

    #[test]
    fn queue_full_is_classified_as_retriable_rather_than_fatal() {
        // A full producer queue must never be a capture failure: the record is held and offered
        // again, which is lossless.
        assert!(is_queue_full(rdkafka::types::RDKafkaErrorCode::QueueFull));
        assert!(!is_queue_full(
            rdkafka::types::RDKafkaErrorCode::MessageSizeTooLarge
        ));
    }

    #[test]
    fn consumption_errors_are_split_into_retriable_and_fatal() {
        use rdkafka::types::RDKafkaErrorCode as Code;
        for code in [
            Code::BrokerTransportFailure,
            Code::AllBrokersDown,
            Code::RebalanceInProgress,
            Code::CoordinatorNotAvailable,
            Code::NotCoordinator,
        ] {
            assert!(
                is_retriable_consumption_error(&KafkaError::MessageConsumption(code)),
                "{code:?} is a transient coordinator or transport condition"
            );
        }
        for code in [
            Code::TopicAuthorizationFailed,
            Code::GroupAuthorizationFailed,
            Code::UnknownTopicOrPartition,
        ] {
            assert!(
                !is_retriable_consumption_error(&KafkaError::MessageConsumption(code)),
                "{code:?} needs an operator"
            );
        }
    }
}
