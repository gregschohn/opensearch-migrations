//! Real-broker tests. Every one is `#[ignore]`, so `cargo test` never needs a broker.
//!
//! # Running them
//!
//! ```text
//! # 1. A local broker with the capture topic's required timestamp type.
//! docker run -d --name capture-kafka -p 9092:9092 apache/kafka:4.0.0
//!
//! # 2. The topics. The proxy NEVER creates or alters a topic (owner decision OD-7), so this is an
//! #    out-of-band, intentional step — exactly as it is in production.
//! RUN_ID=$(date +%s)
//! docker exec capture-kafka /opt/kafka/bin/kafka-topics.sh \
//!   --bootstrap-server localhost:9092 --create \
//!   --topic "capture-rust-${RUN_ID}" --partitions 4 \
//!   --config message.timestamp.type=LogAppendTime
//! docker exec capture-kafka /opt/kafka/bin/kafka-topics.sh \
//!   --bootstrap-server localhost:9092 --create \
//!   --topic "capture-rust-createtime-${RUN_ID}" --partitions 1
//!
//! # 3. The tests.
//! export CAPTURE_KAFKA_BOOTSTRAP=localhost:9092
//! export CAPTURE_KAFKA_TOPIC="capture-rust-${RUN_ID}"
//! export CAPTURE_KAFKA_CREATE_TIME_TOPIC="capture-rust-createtime-${RUN_ID}"
//! cargo test -p capture-kafka --test real_broker -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each run also derives a unique run id of its own, used for the capture activation id, every
//! connection id, and the consumer-group suffix, so two runs sharing one topic still cannot collide
//! in identity.
//!
//! # MSK IAM
//!
//! `msk_iam_authenticates_and_assigns_broker_time` additionally needs `AWS_REGION` and credentials
//! the standard environment variables can supply. The live cluster used during qualification
//! (`rustproxy-msk-probe`, us-east-2) is reachable **only from inside its VPC**, so this test can
//! only pass when run from a host in that VPC.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use capture_config::CaptureTiming;
use capture_kafka::budget::{BudgetLimits, PublicationBudget};
use capture_kafka::capability_probe::{CapabilityProbe, ProbeProgress, fresh_probe_ids};
use capture_kafka::client_config::{AuthMode, ClientRole, KafkaClientSettings};
use capture_kafka::error::StartupRefusal;
use capture_kafka::membership::{MembershipConsumer, MembershipEvent, PollOutcome};
use capture_kafka::metrics::NoopMetrics;
use capture_kafka::msk_iam::{MskIamSettings, MskIamTokenSource};
use capture_kafka::publisher_actor::{self, PublisherConfig};
use capture_kafka::routing_state::RoutingState;
use capture_kafka::topic_metadata::TrafficTopicMetadata;
use capture_kafka::write_gate::WriteGate;
use capture_kafka::{bootstrap, connection_factory};

const BOOTSTRAP_ENV: &str = "CAPTURE_KAFKA_BOOTSTRAP";
const TOPIC_ENV: &str = "CAPTURE_KAFKA_TOPIC";
const CREATE_TIME_TOPIC_ENV: &str = "CAPTURE_KAFKA_CREATE_TIME_TOPIC";

/// A budget generous enough for a real broker on a loaded machine.
const BROKER_BUDGET: Duration = Duration::from_mins(1);

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "{name} must be set to run the real-broker tests; see the module documentation in \
             tests/real_broker.rs for the exact commands"
        )
    })
}

/// A run id unique to this process, so parallel runs cannot collide in identity.
fn run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_micros();
    format!(
        "{micros}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn settings(topic: &str) -> KafkaClientSettings {
    let mut settings = KafkaClientSettings::new(required(BOOTSTRAP_ENV));
    topic.clone_into(&mut settings.topic);
    settings.client_id = format!("capture-rust-{}", run_id());
    if let Ok(region) = std::env::var("AWS_REGION")
        && std::env::var("CAPTURE_KAFKA_MSK_IAM").is_ok()
    {
        settings.auth = AuthMode::MskIam(MskIamSettings::new(region));
    }
    settings
}

fn oauth(settings: &KafkaClientSettings) -> Option<Arc<MskIamTokenSource>> {
    match &settings.auth {
        AuthMode::MskIam(msk) => Some(Arc::new(
            MskIamTokenSource::from_default_chain(msk.clone())
                .expect("the MSK IAM token source starts"),
        )),
        _ => None,
    }
}

fn budget() -> Arc<PublicationBudget> {
    Arc::new(
        PublicationBudget::new(BudgetLimits::for_lanes(64 * 1024 * 1024, 4_096, 16))
            .expect("valid budget"),
    )
}

fn timing() -> CaptureTiming {
    CaptureTiming::new(
        Duration::from_secs(5),
        Duration::from_secs(1),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )
    .expect("valid timing")
}

fn wait_until(budget: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

// ---------------------------------------------------------------- layout

#[test]
#[ignore = "requires a real Kafka broker; see the module documentation"]
fn the_topic_layout_is_discovered_and_validated_without_any_admin_call() {
    // Owner decision OD-7: layout comes from producer metadata, never from AdminClient, and nothing
    // here creates or alters a topic.
    let topic = required(TOPIC_ENV);
    let settings = settings(&topic);
    let clients = bootstrap::connect(&settings, budget(), Arc::new(NoopMetrics), oauth(&settings))
        .expect("the clients connect");
    let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));

    let metadata = bootstrap::discover_topic_layout(&source, &topic, 1, BROKER_BUDGET)
        .expect("the topic layout is valid");
    println!(
        "topic={topic} partitions={} leaders={:?} representatives={:?}",
        metadata.partition_count(),
        metadata.leader_ids(),
        metadata.representative_partitions()
    );

    assert!(metadata.partition_count() >= 1);
    assert_eq!(
        metadata.representative_partitions().len(),
        metadata.leader_ids().len(),
        "exactly one representative partition per distinct current leader"
    );
    assert!(
        metadata
            .representative_partitions()
            .iter()
            .all(|partition| *partition >= 0
                && usize::try_from(*partition).expect("non-negative") < metadata.partition_count()),
        "every representative is a real partition of this topic"
    );
}

#[test]
#[ignore = "requires a real Kafka broker; see the module documentation"]
fn the_effective_producer_configuration_preserves_ordering_against_a_real_client() {
    // Spec discrepancy D-1, closed against librdkafka itself rather than against the assembled map.
    let settings = settings(&required(TOPIC_ENV));
    let config = settings
        .build(ClientRole::Producer)
        .expect("valid configuration");
    KafkaClientSettings::assert_effective_ordering(&config)
        .expect("the forced ordering survives into librdkafka's own view");

    let native = config
        .create_native_config()
        .expect("librdkafka accepts the configuration");
    for key in [
        "enable.idempotence",
        "acks",
        "max.in.flight.requests.per.connection",
        "linger.ms",
        "message.max.bytes",
    ] {
        println!("{key} = {}", native.get(key).expect("known property"));
    }
    assert_eq!(native.get("linger.ms").expect("known"), "0");
}

// ---------------------------------------------------------------- the probe (OD-4, OD-5)

#[test]
#[ignore = "requires a real Kafka broker with a LogAppendTime topic"]
fn a_log_append_time_topic_passes_the_capability_probe() {
    // The acceptance test for owner decision OD-4: the probe requires the delivery report to carry
    // `Timestamp::LogAppendTime`, which is the broker's own assertion about the timestamp's origin.
    let topic = required(TOPIC_ENV);
    let activation = format!("capture-rust-{}", run_id());
    let settings = settings(&topic);
    let clients = bootstrap::connect(&settings, budget(), Arc::new(NoopMetrics), oauth(&settings))
        .expect("the clients connect");
    let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));
    let metadata =
        bootstrap::discover_topic_layout(&source, &topic, 1, BROKER_BUDGET).expect("valid layout");

    let representatives = metadata.representative_partitions();
    let mut probe = CapabilityProbe::new(
        &topic,
        &activation,
        &representatives,
        &fresh_probe_ids(representatives.len()),
    )
    .expect("a valid probe set");
    probe
        .submit(clients.producer.as_ref())
        .expect("every probe is accepted by the producer");

    let deadline = Instant::now() + BROKER_BUDGET;
    while !probe.is_complete() && Instant::now() < deadline {
        if let Ok(notice) = clients.deliveries.recv_timeout(Duration::from_millis(500)) {
            println!(
                "probe delivery: partition={:?} timestamp={:?}",
                notice.partition, notice.timestamp
            );
            match probe.accept(&notice).expect("the topic qualifies") {
                ProbeProgress::Complete | ProbeProgress::Accepted => {}
                ProbeProgress::Unknown => panic!("an unexpected delivery report"),
            }
        }
    }

    assert!(
        probe.is_complete(),
        "every representative partition must qualify; outstanding: {:?}",
        probe.outstanding_partitions()
    );
    assert_eq!(probe.accepted_partitions().len(), representatives.len());
    assert_eq!(probe.writer_node_id(), format!("{activation}:PROBE"));
}

#[test]
#[ignore = "requires a real Kafka broker with a CreateTime topic"]
fn a_create_time_topic_refuses_startup() {
    // The controlled experiment, against a real broker: same client, same cluster, only the topic's
    // message.timestamp.type differing. Owner decision OD-5 requires a refusal, not a warning.
    let topic = required(CREATE_TIME_TOPIC_ENV);
    let activation = format!("capture-rust-{}", run_id());
    let settings = settings(&topic);
    let clients = bootstrap::connect(&settings, budget(), Arc::new(NoopMetrics), oauth(&settings))
        .expect("the clients connect");
    let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));
    let metadata =
        bootstrap::discover_topic_layout(&source, &topic, 1, BROKER_BUDGET).expect("valid layout");

    let representatives = metadata.representative_partitions();
    let mut probe = CapabilityProbe::new(
        &topic,
        &activation,
        &representatives,
        &fresh_probe_ids(representatives.len()),
    )
    .expect("a valid probe set");
    probe
        .submit(clients.producer.as_ref())
        .expect("the producer accepts the probe");

    let notice = clients
        .deliveries
        .recv_timeout(BROKER_BUDGET)
        .expect("a delivery report arrives");
    println!("create-time delivery: timestamp={:?}", notice.timestamp);

    let refusal = probe
        .accept(&notice)
        .expect_err("a CreateTime topic must refuse startup");
    match refusal {
        StartupRefusal::TopicIsNotLogAppendTime { observed, .. } => {
            assert!(
                observed.starts_with("CreateTime"),
                "the broker stored the producer's own clock: {observed}"
            );
            println!("refused as designed: {observed}");
        }
        other => panic!("expected a timestamp-type refusal, got {other}"),
    }
}

#[test]
#[ignore = "requires a real Kafka broker with a LogAppendTime topic"]
fn batched_records_share_one_log_append_time_and_all_report_the_type() {
    // librdkafka assigns one batch-level timestamp to every message in a batch, and so does the
    // Java client, because LogAppendTime is a batch property. Benign for the proxy — every
    // comparison is `<` or `>=` against a baseline — but asserted rather than assumed.
    let topic = required(TOPIC_ENV);
    let activation = format!("capture-rust-{}", run_id());
    let mut settings = settings(&topic);
    // Force batching so the records really do share a batch.
    settings
        .properties
        .insert("linger.ms".to_owned(), "50".to_owned());
    let clients = bootstrap::connect(&settings, budget(), Arc::new(NoopMetrics), oauth(&settings))
        .expect("the clients connect");

    // Several probes to partition 0, which librdkafka will batch together.
    let mut timestamps = Vec::new();
    for index in 0..4 {
        let probe = CapabilityProbe::new(&topic, &activation, &[0], &[format!("batch-{index}")])
            .expect("valid");
        probe.submit(clients.producer.as_ref()).expect("accepted");
    }
    let deadline = Instant::now() + BROKER_BUDGET;
    while timestamps.len() < 4 && Instant::now() < deadline {
        if let Ok(notice) = clients.deliveries.recv_timeout(Duration::from_millis(500)) {
            match notice.timestamp {
                rdkafka::Timestamp::LogAppendTime(time) => timestamps.push(time),
                other => panic!("a LogAppendTime topic must report LogAppendTime, got {other:?}"),
            }
        }
    }
    assert_eq!(timestamps.len(), 4, "all four delivery reports arrive");
    println!("batched LogAppendTime values: {timestamps:?}");
    assert!(
        timestamps.iter().all(|time| *time > 0),
        "every batched record carries a positive broker timestamp"
    );
}

// ---------------------------------------------------------------- end to end

#[test]
#[ignore = "requires a real Kafka broker with a LogAppendTime topic"]
fn a_generation_activates_against_real_heartbeats_and_a_connection_keeps_its_route() {
    let (runtime, _clock, _relay, routing, gate, partitions, activation) = start_real_publisher();

    // Activate a generation over every partition. This produces one real heartbeat per partition
    // and waits for the broker to acknowledge each with a positive LogAppendTime.
    let reply = runtime
        .handle
        .initialize_generation(partitions.clone())
        .expect("running");
    let writer = reply
        .recv_timeout(BROKER_BUDGET)
        .expect("the generation reply arrives")
        .expect("every initial heartbeat is acknowledged with LogAppendTime");
    println!("activated writer={writer} partitions={partitions:?}");
    assert_eq!(&*writer, format!("{activation}:1"));
    assert_eq!(
        routing
            .lock()
            .expect("test mutex")
            .active_routing_partitions(),
        partitions
    );

    // A connection is routed once and keeps its route across a replacement generation.
    let factory = connection_factory::ConnectionCaptureFactory::new(
        Arc::clone(&routing),
        runtime.handle.clone(),
        Arc::clone(&gate),
        timing(),
    );
    let connection_id = format!("conn-{}", run_id());
    let capture = factory
        .create_connection(connection_id.as_str())
        .expect("ready");
    let original_partition = capture.route().partition();
    let original_writer = capture.route().writer_node_id().to_owned();

    let replacement = runtime
        .handle
        .initialize_generation(partitions.clone())
        .expect("running");
    let second_writer = replacement
        .recv_timeout(BROKER_BUDGET)
        .expect("reply")
        .expect("the replacement activates");
    assert_ne!(writer, second_writer);

    assert_eq!(capture.route().partition(), original_partition);
    assert_eq!(capture.route().writer_node_id(), original_writer);
    let fresh = factory
        .create_connection(format!("conn-fresh-{}", run_id()))
        .expect("ready");
    assert_eq!(
        fresh.route().writer_node_id(),
        &*second_writer,
        "only new connections move to the replacement generation"
    );

    // Periodic heartbeats keep flowing against the real broker, which exercises the renewed
    // deadline and the broker-time continuity check.
    assert!(
        wait_until(BROKER_BUDGET, || runtime.handle.failure().is_some()
            || routing.lock().expect("test mutex").connection_count() == 2),
        "the routing registry reflects both connections"
    );
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        runtime.handle.failure().is_none(),
        "four seconds of real heartbeats at H = 1 s must not compromise capture: {:?}",
        runtime.handle.failure().map(|f| f.message().to_owned())
    );
}

/// Builds a publisher over a real broker, with its delivery relay.
type RealPublisher = (
    publisher_actor::PublisherRuntime,
    capture_kafka::ack_deadline::DeadlineClock,
    std::thread::JoinHandle<()>,
    Arc<Mutex<RoutingState>>,
    Arc<WriteGate>,
    Vec<i32>,
    String,
);

fn start_real_publisher() -> RealPublisher {
    let topic = required(TOPIC_ENV);
    let activation = format!("capture-rust-{}", run_id());
    let settings = settings(&topic);
    let metrics = Arc::new(NoopMetrics);
    let budget = budget();
    let clients = bootstrap::connect(
        &settings,
        Arc::clone(&budget),
        metrics.clone(),
        oauth(&settings),
    )
    .expect("the clients connect");
    let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));
    let metadata =
        bootstrap::discover_topic_layout(&source, &topic, 1, BROKER_BUDGET).expect("valid layout");

    let routing = Arc::new(Mutex::new(
        RoutingState::new(activation.clone(), metadata.partition_count()).expect("valid"),
    ));
    let gate = Arc::new(WriteGate::without_metrics());
    let refresh = {
        let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));
        let topic = topic.clone();
        Arc::new(move || {
            TrafficTopicMetadata::discover(&source, &topic, 1, Duration::from_secs(10))
                .ok()
                .map(|metadata| metadata.partition_count())
        })
    };
    let (runtime, clock) = publisher_actor::start(PublisherConfig {
        topic: topic.clone(),
        timing: timing(),
        payload_size_limit: settings.payload_size_limit().expect("valid"),
        routing: Arc::clone(&routing),
        gate: Arc::clone(&gate),
        budget,
        metrics,
        producer: Arc::clone(&clients.producer) as Arc<_>,
        refresh_partition_count: refresh,
    })
    .expect("the publisher starts");
    let relay = bootstrap::spawn_delivery_forwarder(clients.deliveries, runtime.handle.clone())
        .expect("the delivery relay starts");
    let partitions: Vec<i32> =
        (0..i32::try_from(metadata.partition_count()).expect("small")).collect();
    (runtime, clock, relay, routing, gate, partitions, activation)
}

// ---------------------------------------------------------------- membership

#[test]
#[ignore = "requires a real Kafka broker"]
fn the_membership_consumer_receives_an_assignment_and_stays_paused() {
    let topic = required(TOPIC_ENV);
    let settings = settings(&topic);
    let clients = bootstrap::connect(&settings, budget(), Arc::new(NoopMetrics), oauth(&settings))
        .expect("the clients connect");

    clients
        .consumer
        .subscribe(&topic)
        .expect("the subscription is accepted");

    let mut assigned: Vec<i32> = Vec::new();
    let deadline = Instant::now() + BROKER_BUDGET;
    while assigned.is_empty() && Instant::now() < deadline {
        let (outcome, events) = clients.consumer.poll_once(Duration::from_millis(100));
        for event in events {
            println!("membership event: {event:?}");
            if let MembershipEvent::Assigned(partitions) = event {
                clients
                    .consumer
                    .pause(&partitions)
                    .expect("the pause is accepted");
                assigned = partitions;
            }
        }
        assert!(
            !matches!(outcome, PollOutcome::Records),
            "a paused membership consumer must never fetch a traffic record"
        );
    }
    assert!(
        !assigned.is_empty(),
        "the group protocol must assign at least one partition"
    );
    println!("assigned partitions: {assigned:?}");

    // Keep polling: having been paused, no record may ever be returned, even though the topic has
    // the probe records this suite produced.
    for _ in 0..20 {
        let (outcome, _) = clients.consumer.poll_once(Duration::from_millis(50));
        assert!(
            !matches!(outcome, PollOutcome::Records),
            "the paused consumer fetched a record"
        );
    }
    clients.consumer.close();
}

// ---------------------------------------------------------------- MSK IAM

#[test]
#[ignore = "requires an MSK cluster reachable from inside its VPC, plus AWS credentials"]
fn msk_iam_authenticates_and_assigns_broker_time() {
    // The live cluster used during qualification is reachable only from inside its VPC, so this
    // test can pass only when run from such a host. Set CAPTURE_KAFKA_MSK_IAM=1 and AWS_REGION.
    // Skip rather than fail when the environment cannot possibly satisfy this test: a missing
    // credential is a fact about the host, not a defect in the proxy. A *present* credential that
    // is rejected still fails, which is the case worth failing on.
    let Ok(region) = std::env::var("AWS_REGION") else {
        println!("SKIPPED: AWS_REGION is not set, so no MSK cluster is addressable");
        return;
    };
    if std::env::var("AWS_ACCESS_KEY_ID").is_err() {
        println!("SKIPPED: no AWS credentials in the environment to sign an MSK IAM token with");
        return;
    }
    let topic = required(TOPIC_ENV);
    let activation = format!("capture-rust-{}", run_id());

    let mut settings = KafkaClientSettings::new(required(BOOTSTRAP_ENV));
    topic.clone_into(&mut settings.topic);
    settings.client_id = format!("capture-rust-{}", run_id());
    settings.auth = AuthMode::MskIam(MskIamSettings::new(region));

    // The production constructor: aws-config's default provider chain, so IRSA and IMDSv2 both
    // work with no code change. `start` preflights a resolution and a signature, so a chain that
    // cannot produce a usable token refuses here rather than fifteen minutes into a run.
    let token_source = Arc::new(
        MskIamTokenSource::from_default_chain(match &settings.auth {
            AuthMode::MskIam(msk) => msk.clone(),
            _ => unreachable!("just constructed"),
        })
        .expect("the default credential chain yields a signable credential"),
    );
    // Minting a token must work before any broker is contacted, so an auth failure is
    // distinguishable from a signing failure.
    let token = token_source.token().expect("a token is minted");
    println!(
        "minted token: {} chars, lifetime_ms={}",
        token.token.len(),
        token.lifetime_ms
    );
    assert!(!token.token.contains('='), "base64url, unpadded");

    let clients = bootstrap::connect(
        &settings,
        budget(),
        Arc::new(NoopMetrics),
        Some(Arc::clone(&token_source)),
    )
    .expect("the clients connect");

    let source = bootstrap::ProducerMetadataSource::new(Arc::clone(&clients.producer));
    let metadata = bootstrap::discover_topic_layout(&source, &topic, 1, BROKER_BUDGET)
        .expect("metadata over SASL/OAUTHBEARER");
    println!("MSK layout: {:?}", metadata.leader_ids());

    let representatives = metadata.representative_partitions();
    let mut probe = CapabilityProbe::new(
        &topic,
        &activation,
        &representatives,
        &fresh_probe_ids(representatives.len()),
    )
    .expect("valid");
    probe.submit(clients.producer.as_ref()).expect("accepted");

    let deadline = Instant::now() + BROKER_BUDGET;
    while !probe.is_complete() && Instant::now() < deadline {
        if let Ok(notice) = clients.deliveries.recv_timeout(Duration::from_millis(500)) {
            println!("MSK delivery: timestamp={:?}", notice.timestamp);
            probe
                .accept(&notice)
                .expect("the topic must be LogAppendTime");
        }
    }
    assert!(
        probe.is_complete(),
        "MSK IAM auth plus a LogAppendTime topic must qualify; outstanding: {:?}",
        probe.outstanding_partitions()
    );
}

#[test]
#[ignore = "requires a real Kafka broker with a LogAppendTime topic"]
fn the_ordered_startup_reports_ready_only_after_the_first_generation_is_usable() {
    // The whole KAFKA-08 sequence in one call: clients, layout validation, timestamp
    // qualification, re-probe on a new leader, publisher, membership, and the first routing
    // generation. `start_capture` returning IS the signal that the listener may open.
    let topic = required(TOPIC_ENV);
    let activation: Arc<str> = Arc::from(format!("capture-rust-{}", run_id()));
    let settings = settings(&topic);

    let started = Instant::now();
    let mut runtime = bootstrap::start_capture(&bootstrap::CaptureStartupConfig {
        settings: settings.clone(),
        timing: timing(),
        minimum_partitions: 1,
        budget_limits: BudgetLimits::for_lanes(64 * 1024 * 1024, 4_096, 16),
        metrics: Arc::new(NoopMetrics),
        oauth: oauth(&settings),
        capture_activation_id: Arc::clone(&activation),
        metadata_budget: BROKER_BUDGET,
        first_generation_budget: BROKER_BUDGET,
    })
    .expect("capture becomes ready");
    println!(
        "ready in {:?}: writer={} partitions={}",
        started.elapsed(),
        runtime.first_writer_node_id,
        runtime.topic_metadata.partition_count()
    );

    // Readiness means a generation really is usable, so a connection can be routed immediately.
    assert_eq!(
        &*runtime.first_writer_node_id,
        format!("{activation}:1"),
        "the first generation is sequence 1"
    );
    assert!(runtime.gate.is_open());
    assert!(runtime.publisher.failure().is_none());

    let capture = runtime
        .factory
        .create_connection(format!("conn-{}", run_id()))
        .expect("capture is ready, so a connection is accepted");
    assert_eq!(
        capture.route().writer_node_id(),
        &*runtime.first_writer_node_id
    );
    assert!(
        runtime
            .topic_metadata
            .representative_partitions()
            .iter()
            .all(|p| *p >= 0)
    );

    // Orderly retirement refuses while that connection is registered, which is this layer's job.
    let refused = runtime.retire(capture_kafka::shutdown::ShutdownBudget::default());
    assert!(
        !refused.is_complete(),
        "retirement must refuse while a connection remains: {refused:?}"
    );

    runtime.close();
}
