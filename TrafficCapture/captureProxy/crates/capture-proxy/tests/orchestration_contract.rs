//! The Kubernetes orchestration's rendered capture-proxy pods, taken one step further than the
//! configuration loader: the Kafka clients the proxy would build from them, and the pod's shutdown
//! budget.
//!
//! The fixtures are the ones `capture-config`'s contract test reads, generated from the real
//! orchestration code; see `capture-config/tests/support/orchestration_fixture.rs`.

#[path = "../../capture-config/tests/support/orchestration_fixture.rs"]
mod orchestration_fixture;

use std::path::Path;

use capture_config::{ApplicationConfig, EnvLookup};
use capture_kafka::client_config::{ClientRole, KafkaClientSettings};
use capture_proxy::ORDERLY_SHUTDOWN_HARD_STOP;
use capture_proxy::kafka_capture::kafka_settings;
use orchestration_fixture::{Fixture, load_all};

fn fixtures() -> Vec<Fixture> {
    load_all(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../capture-config/tests/fixtures/orchestration"),
    )
}

#[test]
fn every_rendered_pod_outlives_the_proxy_shutdown_budget() {
    // OD-15: SIGKILL must not pre-empt the orderly shutdown, which hard-stops at
    // ORDERLY_SHUTDOWN_HARD_STOP after SIGTERM.
    for fixture in fixtures() {
        let grace = fixture.document["pod"]["terminationGracePeriodSeconds"]
            .as_u64()
            .unwrap_or_else(|| {
                panic!(
                    "{}: the pod sets no terminationGracePeriodSeconds",
                    fixture.name
                )
            });
        assert!(
            grace > ORDERLY_SHUTDOWN_HARD_STOP.as_secs(),
            "{}: a {grace} s grace period kills the proxy before its {} s shutdown completes",
            fixture.name,
            ORDERLY_SHUTDOWN_HARD_STOP.as_secs()
        );
    }
}

#[test]
fn the_kafka_clients_build_from_every_rendered_capture_configuration() {
    let mut built = 0;
    for fixture in fixtures().iter().filter(|fixture| fixture.accepted()) {
        let env = fixture.container_env();
        let mut document = fixture.decoded_inline_json();
        if fixture.expect()["capture"] == "disabled" {
            continue;
        }
        if fixture.expect()["capture"]["auth"] == "msk-iam" {
            // kafka_settings starts the MSK IAM token source, whose preflight resolves real AWS
            // credentials; that is proved against live MSK elsewhere (TRACEABILITY KAFKA-09). What
            // the orchestration owns is the region the token is signed for.
            assert!(
                env.get("AWS_REGION")
                    .is_some_and(|region| !region.is_empty()),
                "{}: MSK IAM needs AWS_REGION in the pod",
                fixture.name
            );
            continue;
        }

        // The property file lives in a ConfigMap volume. Materialize it the way the kubelet would,
        // and point the document at the copy: the only change to what the pod carries.
        let dir = tempfile::tempdir().expect("temp dir");
        if let Some(container_path) = document["kafkaPropertyFile"].as_str().map(str::to_owned) {
            let contents = fixture
                .config_map_file_contents(Path::new(&container_path))
                .unwrap_or_else(|| panic!("{}: {container_path} is not mounted", fixture.name));
            let copy = dir.path().join("client.properties");
            std::fs::write(&copy, contents).expect("write the property file");
            document["kafkaPropertyFile"] = copy.display().to_string().into();
        }

        let config = ApplicationConfig::from_args(
            &["---INLINE-JSON".to_owned(), document.to_string()],
            &env,
        )
        .unwrap_or_else(|error| panic!("{}: {error}", fixture.name));
        let (settings, oauth) = kafka_settings(&config, &env)
            .unwrap_or_else(|error| panic!("{}: {error}", fixture.name));
        assert!(oauth.is_none(), "{}", fixture.name);
        let producer = settings
            .build(ClientRole::Producer)
            .unwrap_or_else(|error| panic!("{}: producer: {error}", fixture.name));
        settings
            .build(ClientRole::MembershipConsumer)
            .unwrap_or_else(|error| panic!("{}: membership consumer: {error}", fixture.name));
        // librdkafka itself accepts every key and keeps the ordering guarantees.
        KafkaClientSettings::assert_effective_ordering(&producer)
            .unwrap_or_else(|error| panic!("{}: {error}", fixture.name));
        if fixture.expect()["capture"]["propertyFile"].is_string() {
            assert_eq!(
                producer.get("ssl.ca.location"),
                Some("/config/kafka-ca/ca.crt"),
                "{}: the orchestration's PEM trust store must become librdkafka's CA location",
                fixture.name
            );
        }
        built += 1;
    }
    assert!(
        built >= 5,
        "only {built} capture configurations were exercised"
    );
}
