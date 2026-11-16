//! The configuration contract, exercised through the public API the process layer uses.
//!
//! The unit tests inside each module cover the rules in detail; this file pins the handful of
//! facts a deployment depends on, from outside the crate:
//!
//! * the production Kubernetes invocation — two argv elements, base64 JSON, zero flags — produces
//!   a complete configuration;
//! * the CDK/ECS invocation, with the literal Java flag spellings, still works;
//! * every configuration error reports a defined exit code, and never exit 1;
//! * nothing credential-bearing is rendered by `Debug`, `Display`, or the startup argument echo.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use capture_config::application::{
    ApplicationConfig, CONFIG_ERROR_EXIT_CODE, ConfigError, DESTINATION_URI_EXIT_CODE,
};
use capture_config::cli::{EnvLookup, REDACTED, SecretString, redact_args};
use capture_config::inline_json::{ConfigSource, INLINE_JSON_FLAG};
use capture_config::kafka::{KafkaAuthType, KafkaConfig};
use capture_config::policy::{CaptureFailurePolicy, HttpStrictness};
use capture_config::tls::ClientAuth;

/// A stub environment, so no test mutates the process environment.
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

const TRUST_PEM: &str = "-----BEGIN CERTIFICATE-----\nTRUSTBLOB\n-----END CERTIFICATE-----\n";

/// The document `makeProxyParamsDict` emits for a SCRAM cluster with TLS (spec §10.1).
const ORCHESTRATION_DOCUMENT: &str = r#"{
    "listenPort": 9200,
    "destinationUri": "https://source-cluster:9200",
    "insecureDestination": false,
    "noCapture": false,
    "kafkaConnection": "b-1.msk:9096,b-2.msk:9096",
    "kafkaTopic": "logging-traffic-topic",
    "kafkaListenerName": "CLIENT_SASL_SCRAM",
    "kafkaAuthType": "scram-sha-512",
    "kafkaSecretName": "proxy-kafka-user",
    "kafkaUserName": "capture",
    "kafkaClientId": "HttpCaptureProxyProducer",
    "kafkaPropertyFile": "/config/kafka-auth/client.properties",
    "enableMSKAuth": false,
    "numThreads": 0,
    "maxTrafficBufferSize": 1048576,
    "destinationConnectionPoolSize": 0,
    "destinationConnectionPoolTimeout": "PT30S",
    "suppressCaptureForHeaderMatch": [],
    "suppressCaptureForMethod": "",
    "suppressCaptureForUriPath": "",
    "suppressMethodAndPath": "",
    "otelMetricsCollectorEndpoint": "http://otel-collector:4317",
    "sslCertChainFile": "/etc/proxy-tls/tls.crt",
    "sslKeyFile": "/etc/proxy-tls/tls.key",
    "sslTrustCertPemEnvVar": "CAPTURE_PROXY_SSL_TRUST_CERT_PEM",
    "requireClientAuth": true
}"#;

fn production_env() -> StubEnv {
    StubEnv::with(&[
        ("CAPTURE_PROXY_SSL_TRUST_CERT_PEM", TRUST_PEM),
        ("CAPTURE_PROXY_KAFKA_PASSWORD", "hunter2"),
    ])
}

fn load(args: &[&str], env: &dyn EnvLookup) -> Result<ApplicationConfig, ConfigError> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    ApplicationConfig::from_args(&owned, env)
}

#[test]
fn the_production_kubernetes_invocation_produces_a_complete_configuration() {
    let payload = BASE64.encode(ORCHESTRATION_DOCUMENT);
    let config = load(&[INLINE_JSON_FLAG, &payload], &production_env())
        .expect("the orchestration document must be accepted");

    assert_eq!(*config.source(), ConfigSource::InlineJson);
    assert_eq!(config.listen_port(), 9200);
    assert_eq!(config.destination().host(), "source-cluster");
    assert_eq!(config.destination().port(), 9200);
    assert!(config.destination().uses_tls());
    assert!(!config.insecure_destination());

    let tls = config.tls().expect("the TLS variant enables TLS");
    assert_eq!(tls.client_auth(), ClientAuth::Require);
    assert!(tls.trust().is_some());

    let kafka = config.sink().kafka().expect("capture is enabled");
    assert_eq!(kafka.brokers(), "b-1.msk:9096,b-2.msk:9096");
    assert_eq!(kafka.topic(), "logging-traffic-topic");
    assert_eq!(kafka.client_id(), "HttpCaptureProxyProducer");
    assert_eq!(kafka.auth(), KafkaAuthType::ScramSha512);
    assert_eq!(kafka.username(), Some("capture"));
    assert_eq!(
        kafka.password().map(SecretString::expose),
        Some("hunter2"),
        "the password arrives only by environment variable"
    );
}

#[test]
fn every_parameter_the_orchestration_omits_runs_at_its_documented_default() {
    // Spec §10.1: in production Kubernetes these are all defaults, so changing one silently
    // changes production behaviour.
    let payload = BASE64.encode(ORCHESTRATION_DOCUMENT);
    let config = load(&[INLINE_JSON_FLAG, &payload], &production_env()).expect("valid");

    assert_eq!(config.failure_policy(), CaptureFailurePolicy::FailClosed);
    assert_eq!(config.strictness(), HttpStrictness::Strict);
    assert_eq!(config.timing().flush_interval().as_secs(), 5);
    assert_eq!(config.timing().heartbeat_interval().as_secs(), 10);
    assert_eq!(config.timing().heartbeat_expiration().as_secs(), 30);
    assert_eq!(config.limits().max_request_assembly().as_secs(), 300);
    assert_eq!(config.limits().max_connection_duration().as_secs(), 3600);
    assert_eq!(config.limits().max_header_bytes(), 10 * 1024 * 1024);
    assert_eq!(config.limits().max_total_bytes(), 250 * 1024 * 1024);
    assert_eq!(config.max_traffic_buffer_size(), 1024 * 1024);
    assert_eq!(config.connection_pool().size, 0);
    assert_eq!(config.connection_pool().recycle_after.as_secs(), 30);
    assert!(config.worker_threads_are_automatic());
    assert!(config.telemetry().traces.is_none());
    assert_eq!(
        config
            .sink()
            .kafka()
            .map(KafkaConfig::required_topic_partitions),
        Some(0),
        "the Kubernetes path never asks for a partition count"
    );
}

#[test]
fn the_three_empty_suppression_patterns_the_orchestration_emits_are_inert() {
    // The single most dangerous trap in the configuration surface: Regex::is_match is a search,
    // so an empty pattern compiled naively would suppress every request.
    let payload = BASE64.encode(ORCHESTRATION_DOCUMENT);
    let config = load(&[INLINE_JSON_FLAG, &payload], &production_env()).expect("valid");
    let suppression = config.suppression();
    assert!(suppression.method.is_none());
    assert!(suppression.uri_path.is_none());
    assert!(suppression.method_and_path.is_none());
    assert!(suppression.headers.is_empty());
}

#[test]
fn the_cdk_ecs_invocation_with_the_java_flag_spellings_still_works() {
    // capture-proxy-stack.ts:139-172.
    let config = load(
        &[
            "--destinationUri",
            "https://source:9200",
            "--insecureDestination",
            "--listenPort",
            "9200",
            "--sslCertChainFile",
            "/usr/share/captureProxy/config/pub.pem",
            "--sslKeyFile",
            "/usr/share/captureProxy/config/key.pem",
            "--kafkaConnection",
            "broker:9092",
            "--minimumKafkaTopicPartitions",
            "3",
            "--enableMSKAuth",
            "--otelTraceCollectorEndpoint",
            "http://localhost:4317",
            "--otelMetricsCollectorEndpoint",
            "http://localhost:4317",
        ],
        &StubEnv::default(),
    )
    .expect("the CDK invocation must keep working");

    assert_eq!(*config.source(), ConfigSource::Flags);
    assert!(config.insecure_destination());
    let kafka = config.sink().kafka().expect("capture is enabled");
    assert_eq!(kafka.auth(), KafkaAuthType::MskIam);
    assert_eq!(kafka.required_topic_partitions(), 3);
    assert_eq!(
        config.tls().expect("TLS enabled").client_auth(),
        ClientAuth::None
    );
}

#[test]
fn flag_mode_and_json_mode_cannot_be_mixed() {
    let error = load(
        &[
            INLINE_JSON_FLAG,
            r#"{"listenPort":9200,"destinationUri":"http://h:9200","noCapture":true}"#,
            "--num-threads",
        ],
        &StubEnv::default(),
    )
    .expect_err("a flag may not accompany a JSON document");
    assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
}

#[test]
fn a_drifted_orchestration_key_fails_startup_rather_than_being_ignored() {
    let error = load(
        &[
            INLINE_JSON_FLAG,
            r#"{"listenPort":9200,"destinationUri":"http://h:9200","noCapture":true,
                "suppressCaptureForMehtod":"POST"}"#,
        ],
        &StubEnv::default(),
    )
    .expect_err("a misspelled key must be rejected");
    let message = error.to_string();
    assert!(message.contains("suppressCaptureForMehtod"), "{message}");
    assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
}

#[test]
fn an_unparseable_destination_uri_keeps_its_own_exit_code() {
    // CaptureProxy.java:443 — the one configuration error with a distinct documented code.
    let error = load(
        &[
            "--destinationUri",
            "not-a-uri",
            "--listenPort",
            "9200",
            "--noCapture",
        ],
        &StubEnv::default(),
    )
    .map(|_| ())
    .expect_err("an unparseable destination URI must be rejected");
    assert_eq!(error.exit_code(), DESTINATION_URI_EXIT_CODE);
}

#[test]
fn every_other_configuration_error_reports_the_config_error_exit_code() {
    // Java reports exit 1 for Kafka auth-flag and TLS errors (spec discrepancies #1 and #3), which
    // makes a misconfiguration indistinguishable from a crash.
    let cases: &[(&[&str], i32, &str)] = &[
        (
            &["--destinationUri", "http://h:9200", "--listenPort", "9200"],
            CONFIG_ERROR_EXIT_CODE,
            "neither capture sink selected",
        ),
        (
            &[
                "--destinationUri",
                "http://h:9200",
                "--listenPort",
                "9200",
                "--noCapture",
                "--kafkaBrokers",
                "broker:9092",
            ],
            CONFIG_ERROR_EXIT_CODE,
            "both capture sinks selected",
        ),
        (
            &[
                "--destinationUri",
                "http://h:9200",
                "--listenPort",
                "9200",
                "--kafkaBrokers",
                "broker:9092",
                "--enableMSKAuth",
                "--kafkaAuthType",
                "scram-sha-512",
            ],
            CONFIG_ERROR_EXIT_CODE,
            "incompatible Kafka auth flags",
        ),
        (
            &[
                "--destinationUri",
                "http://h:9200",
                "--listenPort",
                "9200",
                "--noCapture",
                "--sslCertChainFile",
                "/c",
            ],
            CONFIG_ERROR_EXIT_CODE,
            "a certificate chain without a key",
        ),
        (
            &[
                "--destinationUri",
                "http://h:9200",
                "--listenPort",
                "9200",
                "--noCapture",
                "--sslCertChainFile",
                "/c",
                "--sslKeyFile",
                "/k",
                "--requireClientAuth",
            ],
            CONFIG_ERROR_EXIT_CODE,
            "client auth without trust material",
        ),
        (
            &[
                "--destinationUri",
                "http://h:9200",
                "--listenPort",
                "9200",
                "--noCapture",
                "--heartbeat-interval-seconds",
                "30",
                "--heartbeat-expiration-interval-seconds",
                "30",
            ],
            CONFIG_ERROR_EXIT_CODE,
            "H must be strictly below E",
        ),
        (
            &["--listenPort", "9200", "--noCapture"],
            CONFIG_ERROR_EXIT_CODE,
            "a missing required parameter",
        ),
    ];

    for (args, expected, label) in cases {
        let error = load(args, &StubEnv::default())
            .map(|_| ())
            .expect_err(label);
        assert_eq!(error.exit_code(), *expected, "{label}: {error}");
        assert_ne!(
            error.exit_code(),
            1,
            "{label}: a config error is never exit 1"
        );
    }
}

#[test]
fn nothing_credential_bearing_is_rendered_anywhere_an_operator_would_see_it() {
    let payload = BASE64.encode(ORCHESTRATION_DOCUMENT);
    let config = load(&[INLINE_JSON_FLAG, &payload], &production_env()).expect("valid");

    let debugged = format!("{config:?}");
    assert!(
        !debugged.contains("hunter2"),
        "the Kafka password leaked into Debug output: {debugged}"
    );
    assert!(
        !debugged.contains("TRUSTBLOB"),
        "the trust PEM leaked into Debug output: {debugged}"
    );
    assert!(debugged.contains(REDACTED), "{debugged}");

    // The startup argument echo, which Java performs twice without redaction
    // (CaptureProxy.java:337 and :558).
    let echoed = redact_args([INLINE_JSON_FLAG, &payload]);
    assert_eq!(echoed, vec![INLINE_JSON_FLAG, REDACTED]);
    let echoed = redact_args(["--kafkaPassword", "hunter2", "--listenPort", "9200"]);
    assert_eq!(
        echoed,
        vec!["--kafkaPassword", REDACTED, "--listenPort", "9200"]
    );
}

#[test]
fn the_owner_decisions_with_configuration_consequences_are_reachable() {
    let config = load(
        &[
            "--destinationUri",
            "http://h:9200",
            "--listenPort",
            "9200",
            "--kafkaBrokers",
            "broker:9092",
            // OD-1: an explicit request-line budget.
            "--max-request-line-bytes",
            "65536",
            // OD-2: the strictness mode.
            "--http-strictness",
            "lenient-line-endings",
            // OD-7: a required partition count, validated and never changed.
            "--require-kafka-topic-partitions",
            "6",
        ],
        &StubEnv::default(),
    )
    .expect("valid");

    assert_eq!(config.limits().max_request_line_bytes(), 65536);
    assert_eq!(config.strictness(), HttpStrictness::LenientLineEndings);
    assert_eq!(
        config
            .sink()
            .kafka()
            .map(KafkaConfig::required_topic_partitions),
        Some(6)
    );
    // The default request-line budget is far above Netty's silent 4096.
    let defaulted = load(
        &[
            "--destinationUri",
            "http://h:9200",
            "--listenPort",
            "9200",
            "--noCapture",
        ],
        &StubEnv::default(),
    )
    .expect("valid");
    assert!(defaulted.limits().max_request_line_bytes() > 4096);
}
