//! The assembled, validated process configuration, and the exit codes it reports (spec §3, §4).
//!
//! # One validation stage, one exit code
//!
//! Java validates in three places with three outcomes:
//!
//! | Check | Java outcome |
//! |---|---|
//! | Parse, arity, required, failure-policy converter | `ParameterException` → exit **2** |
//! | Kafka auth flags (`validateKafkaAuthFlags`) | `IllegalArgumentException`, uncaught → exit **1** (spec discrepancy #1) |
//! | Interval, byte, and capture-sink cross-field rules | exit **2** |
//! | TLS material (`buildSslEngineSupplier`, after `parseArgs` returned) | uncaught → exit **1** (spec discrepancy #3) |
//! | `--destinationUri` parse | `System.exit(3)` |
//!
//! Here **every** configuration error is reported before anything is constructed, and every one of
//! them exits [`CONFIG_ERROR_EXIT_CODE`] — except a destination-URI failure, which keeps Java's
//! distinct [`DESTINATION_URI_EXIT_CODE`] because that code is documented in the operational
//! contract (spec §3.3). Exit 1 is no longer a configuration outcome at all.
//!
//! # Ordering that operators depend on
//!
//! Spec §3.2 step O: the client-facing listener is bound **last**, strictly after Kafka readiness.
//! That is what makes a TCP probe against `--listenPort` a valid readiness signal, and it is the
//! only readiness gate the container has (`setupCapture.ts:747-755`). This module produces
//! configuration only; it binds nothing and opens nothing, which is what lets the process layer
//! keep that order.
//!
//! # Worker sizing (spec §4)
//!
//! `--numThreads` defaults to `0`, meaning "derive it from available parallelism". The spec notes
//! its own caveat that Netty's `2 * availableProcessors` formula is quoted from a flag description
//! and a test comment rather than from repo source, so the **intent** is the contract: an explicit
//! value, or an automatic default derived from available parallelism. This crate carries the
//! configured value verbatim through [`ApplicationConfig::worker_threads`];
//! `capture_transport::listener::worker_thread_count` resolves `0` to `2 ×
//! available_parallelism()`, and this crate deliberately does not duplicate that function.

use std::collections::BTreeMap;
use std::time::Duration;

use clap::Parser as _;

use crate::cli::{
    CliArgs, ENV_KAFKA_PASSWORD, ENV_KAFKA_PASSWORD_FALLBACK, ENV_SET_HEADER,
    ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH, EnvLookup, HeaderPair, HeaderPairError, SecretString,
};
use crate::endpoint::{DestinationEndpoint, EndpointError};
use crate::inline_json::{ConfigSource, InlineJsonError, resolve_args};
use crate::kafka::{KafkaConfig, KafkaConfigError, KafkaInputs};
use crate::limits::{LimitsError, RequestLimits};
use crate::pattern::{PatternError, SuppressionPattern};
use crate::policy::{CaptureFailurePolicy, HttpStrictness};
use crate::timing::{CaptureTiming, TimingError};
use crate::tls::{FrontsideTls, TlsConfigError, TlsInputs};

/// Exit code for a configuration error (`CaptureProxy.java:339`).
pub const CONFIG_ERROR_EXIT_CODE: i32 = 2;

/// Exit code for an unparseable `--destinationUri` (`CaptureProxy.java:443`).
pub const DESTINATION_URI_EXIT_CODE: i32 = 3;

/// Where captured traffic goes.
///
/// The `--noCapture` / `--kafkaBrokers` exclusive-or (`CaptureProxy.java:329-333`) is enforced by
/// construction: exactly one variant exists at a time, so no later code has to re-check it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSink {
    /// `--noCapture`: nothing is captured, and no Kafka client is built.
    Disabled,
    /// Traffic is published to Kafka.
    Kafka(Box<KafkaConfig>),
}

impl CaptureSink {
    /// The Kafka configuration, if capture is enabled.
    #[must_use]
    pub fn kafka(&self) -> Option<&KafkaConfig> {
        match self {
            Self::Disabled => None,
            Self::Kafka(config) => Some(config),
        }
    }

    /// Whether capture is disabled.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
}

/// One header-value suppression rule.
#[derive(Debug, Clone)]
pub struct HeaderSuppression {
    /// The header name, matched ASCII case-insensitively by the scanner.
    pub name: String,
    /// The whole-string pattern the header's first value is tested against.
    pub pattern: SuppressionPattern,
}

/// Every capture-suppression rule, compiled.
///
/// Compiled **here**, at configuration time, for two reasons. First, a pattern that Rust's `regex`
/// cannot express is then a startup error rather than a runtime surprise. Second, every pattern
/// goes through [`SuppressionPattern`], which matches whole strings the way Java's
/// `Pattern.matches()` does: the orchestration defaults all three scalar patterns to the **empty
/// string**, and under a search-based match an empty pattern matches every input, which would
/// suppress every request and silently disable all capture.
///
/// An empty pattern maps to `None` rather than to an inert empty pattern, so "unset" is explicit.
#[derive(Debug, Clone, Default)]
pub struct SuppressionRules {
    /// `--suppressCaptureForMethod`.
    pub method: Option<SuppressionPattern>,
    /// `--suppressCaptureForUriPath`.
    pub uri_path: Option<SuppressionPattern>,
    /// `--suppressMethodAndPath`, tested against `"<METHOD> <PATH>"`.
    pub method_and_path: Option<SuppressionPattern>,
    /// `--suppressCaptureForHeaderMatch`, as name/pattern pairs.
    pub headers: Vec<HeaderSuppression>,
}

/// The backside connection pool's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPool {
    /// Warm connections per worker. `0` disables pooling (`CaptureProxy.java:162`).
    pub size: u32,
    /// Recycle age for a pooled connection.
    pub recycle_after: Duration,
}

/// OTLP export endpoints. An absent endpoint disables that export, as spec §1.1 rows 14-15 require.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryEndpoints {
    /// `--otelTraceCollectorEndpoint`.
    pub traces: Option<String>,
    /// `--otelMetricsCollectorEndpoint`.
    pub metrics: Option<String>,
}

/// The whole validated process configuration.
///
/// `Debug` is safe to log: every credential-bearing field is a [`SecretString`] or is held inside
/// a type whose `Debug` redacts it.
#[derive(Debug, Clone)]
pub struct ApplicationConfig {
    source: ConfigSource,
    destination: DestinationEndpoint,
    listen_port: u16,
    insecure_destination: bool,
    tls: Option<FrontsideTls>,
    sink: CaptureSink,
    limits: RequestLimits,
    timing: CaptureTiming,
    failure_policy: CaptureFailurePolicy,
    strictness: HttpStrictness,
    suppression: SuppressionRules,
    set_header: Vec<HeaderPair>,
    worker_threads: usize,
    max_traffic_buffer_size: u32,
    connection_pool: ConnectionPool,
    telemetry: TelemetryEndpoints,
}

impl ApplicationConfig {
    /// Resolves a raw argument vector — flag mode or JSON mode — into a validated configuration.
    ///
    /// `args` must **exclude** the program name. `env` supplies the values that are themselves
    /// named by configuration: the trust PEM, and the Kafka password fallback.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`]. Call [`ConfigError::exit_code`] for the process exit status.
    pub fn from_args<S: AsRef<str>>(args: &[S], env: &dyn EnvLookup) -> Result<Self, ConfigError> {
        let resolved = resolve_args(args)?;
        let mut parse_input = Vec::with_capacity(resolved.args.len() + 1);
        parse_input.push("capture-proxy".to_owned());
        parse_input.extend(resolved.args);
        let cli = CliArgs::try_parse_from(parse_input).map_err(|error| match error.kind() {
            clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                ConfigError::HelpRequested {
                    message: error.to_string(),
                }
            }
            _ => ConfigError::Usage {
                message: error.to_string(),
            },
        })?;
        Self::from_cli(cli, resolved.source, env)
    }

    /// Validates an already-parsed [`CliArgs`].
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`] except [`ConfigError::Arguments`] and [`ConfigError::Usage`], which only
    /// argument resolution can produce.
    pub fn from_cli(
        cli: CliArgs,
        source: ConfigSource,
        env: &dyn EnvLookup,
    ) -> Result<Self, ConfigError> {
        // Java's order: the Kafka and interval checks first (CaptureProxy.java:298-333), the
        // destination URI at step F, and the TLS material last at step H. The order only decides
        // which error an operator sees first; all of them are reported before anything is built.
        let destination = DestinationEndpoint::parse(&cli.destination_uri)?;
        let sink = resolve_sink(&cli, env)?;

        let limits = RequestLimits::new(
            Duration::from_secs(cli.max_request_assembly_duration_seconds),
            cli.max_incomplete_request_header_bytes,
            cli.max_incomplete_request_total_bytes,
            Duration::from_secs(cli.max_connection_duration_seconds),
            cli.max_request_line_bytes,
        )?;

        let timing = CaptureTiming::new(
            Duration::from_secs(u64::from(cli.traffic_stream_flush_interval_seconds)),
            Duration::from_secs(u64::from(cli.heartbeat_interval_seconds)),
            Duration::from_secs(u64::from(cli.heartbeat_expiration_interval_seconds)),
            Duration::from_secs(u64::from(cli.clock_skew_allowance_seconds)),
        )?;

        if cli.max_traffic_buffer_size == 0 {
            return Err(ConfigError::EmptyTrafficBuffer);
        }

        let set_header = resolve_pairs("--set-header", &cli.set_header, ENV_SET_HEADER, env)?;
        let header_matches = resolve_pairs(
            "--suppress-capture-for-header-match",
            &cli.suppress_capture_for_header_match,
            ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH,
            env,
        )?;

        let suppression = SuppressionRules {
            method: compile_optional(cli.suppress_capture_for_method.as_deref())?,
            uri_path: compile_optional(cli.suppress_capture_for_uri_path.as_deref())?,
            method_and_path: compile_optional(cli.suppress_method_and_path.as_deref())?,
            headers: header_matches
                .into_iter()
                .map(|pair| {
                    Ok(HeaderSuppression {
                        name: pair.name,
                        pattern: SuppressionPattern::new(pair.value)?,
                    })
                })
                .collect::<Result<Vec<_>, PatternError>>()?,
        };

        let tls = TlsInputs {
            cert_chain_file: cli.ssl_cert_chain_file,
            key_file: cli.ssl_key_file,
            trust_cert_file: cli.ssl_trust_cert_file,
            trust_cert_pem_env_var: cli.ssl_trust_cert_pem_env_var,
            require_client_auth: cli.require_client_auth,
        }
        .resolve(env)?;

        Ok(Self {
            source,
            destination,
            listen_port: cli.listen_port,
            insecure_destination: cli.insecure_destination,
            tls,
            sink,
            limits,
            timing,
            failure_policy: cli.capture_failure_policy,
            strictness: cli.http_strictness,
            suppression,
            set_header,
            worker_threads: cli.num_threads,
            max_traffic_buffer_size: cli.max_traffic_buffer_size,
            connection_pool: ConnectionPool {
                size: cli.destination_connection_pool_size,
                recycle_after: cli.destination_connection_pool_timeout.as_duration(),
            },
            telemetry: TelemetryEndpoints {
                traces: non_empty(cli.otel_trace_collector_endpoint),
                metrics: non_empty(cli.otel_metrics_collector_endpoint),
            },
        })
    }

    /// Whether this configuration came from flags, inline JSON, or a JSON file.
    #[must_use]
    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    /// The upstream cluster, with its port resolved.
    #[must_use]
    pub fn destination(&self) -> &DestinationEndpoint {
        &self.destination
    }

    /// The port the client-facing listener binds, last of all.
    #[must_use]
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// `--insecureDestination`: do not verify the destination certificate.
    #[must_use]
    pub fn insecure_destination(&self) -> bool {
        self.insecure_destination
    }

    /// Client-facing TLS, or `None` for a plaintext listener.
    #[must_use]
    pub fn tls(&self) -> Option<&FrontsideTls> {
        self.tls.as_ref()
    }

    /// Where captured traffic goes.
    #[must_use]
    pub fn sink(&self) -> &CaptureSink {
        &self.sink
    }

    /// Incomplete-request and connection limits.
    #[must_use]
    pub fn limits(&self) -> RequestLimits {
        self.limits
    }

    /// The `F`, `H`, `E`, and `S` capture intervals.
    #[must_use]
    pub fn timing(&self) -> CaptureTiming {
        self.timing
    }

    /// What the process does when required capture fails.
    #[must_use]
    pub fn failure_policy(&self) -> CaptureFailurePolicy {
        self.failure_policy
    }

    /// HTTP/1 line-parsing strictness (owner decision OD-2).
    #[must_use]
    pub fn strictness(&self) -> HttpStrictness {
        self.strictness
    }

    /// The compiled capture-suppression rules.
    #[must_use]
    pub fn suppression(&self) -> &SuppressionRules {
        &self.suppression
    }

    /// Headers forced onto every forwarded request, replacing any existing one.
    #[must_use]
    pub fn set_header(&self) -> &[HeaderPair] {
        &self.set_header
    }

    /// The configured worker-thread count, `0` meaning automatic. See the module documentation.
    #[must_use]
    pub fn worker_threads(&self) -> usize {
        self.worker_threads
    }

    /// Whether the worker count is derived from available parallelism.
    #[must_use]
    pub fn worker_threads_are_automatic(&self) -> bool {
        self.worker_threads == crate::cli::WORKER_THREADS_AUTO
    }

    /// Maximum bytes written to a single traffic stream.
    #[must_use]
    pub fn max_traffic_buffer_size(&self) -> u32 {
        self.max_traffic_buffer_size
    }

    /// The backside connection pool's settings.
    #[must_use]
    pub fn connection_pool(&self) -> &ConnectionPool {
        &self.connection_pool
    }

    /// OTLP export endpoints.
    #[must_use]
    pub fn telemetry(&self) -> &TelemetryEndpoints {
        &self.telemetry
    }

    /// The effective Kafka producer properties, given an already-parsed property file.
    ///
    /// Returns `None` when capture is disabled. Parsing the file is
    /// `capture_kafka::client_config::load_property_file`'s job.
    #[must_use]
    pub fn effective_kafka_properties(
        &self,
        file_properties: &BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        self.sink
            .kafka()
            .map(|kafka| kafka.effective_properties(file_properties))
    }
}

/// Resolves the capture sink, enforcing the `--noCapture` / `--kafkaBrokers` exclusive-or.
///
/// `CaptureProxy.java:329-333`. The password is taken from the flag, then the prefixed environment
/// variable, then the unprefixed `KAFKA_PASSWORD` fallback Java reaches through
/// `ArgNameConstants.POSSIBLE_CREDENTIALS_ARG_FLAG_NAMES`. In production the first environment
/// lookup has already been made by `clap`; repeating it here costs nothing and makes the
/// precedence testable with an injected environment.
fn resolve_sink(cli: &CliArgs, env: &dyn EnvLookup) -> Result<CaptureSink, ConfigError> {
    let brokers = cli
        .kafka_brokers
        .as_deref()
        .map(str::trim)
        .filter(|brokers| !brokers.is_empty());
    match (cli.no_capture, brokers) {
        (true, Some(_)) => Err(ConfigError::CaptureSinkConflict),
        (true, None) => Ok(CaptureSink::Disabled),
        (false, None) => Err(ConfigError::NoCaptureSink),
        (false, Some(brokers)) => {
            let password = cli
                .kafka_password
                .clone()
                .or_else(|| env.get(ENV_KAFKA_PASSWORD).map(SecretString::new))
                .or_else(|| env.get(ENV_KAFKA_PASSWORD_FALLBACK).map(SecretString::new));
            Ok(CaptureSink::Kafka(Box::new(KafkaConfig::new(
                KafkaInputs {
                    brokers: Some(brokers.to_owned()),
                    topic: cli.kafka_topic.clone(),
                    client_id: cli.kafka_client_id.clone(),
                    auth_type: cli.kafka_auth_type.clone(),
                    enable_msk_auth: cli.enable_msk_auth,
                    username: cli.kafka_user_name.clone(),
                    password,
                    listener_name: cli.kafka_listener_name.clone(),
                    secret_name: cli.kafka_secret_name.clone(),
                    property_file: cli.kafka_property_file.clone(),
                    required_topic_partitions: cli.require_kafka_topic_partitions,
                },
            )?)))
        }
    }
}

/// Compiles a pattern, mapping the empty string to `None`.
///
/// The orchestration emits `""` for all three scalar suppression patterns (spec §10.1). Under
/// Java's whole-string match an empty pattern is inert; mapping it to `None` makes that explicit
/// instead of relying on the matcher's semantics.
fn compile_optional(pattern: Option<&str>) -> Result<Option<SuppressionPattern>, PatternError> {
    match pattern.filter(|pattern| !pattern.is_empty()) {
        Some(pattern) => Ok(Some(SuppressionPattern::new(pattern)?)),
        None => Ok(None),
    }
}

/// Groups a flat pair list, falling back to a JSON array in `env_var` when no values were given.
///
/// The environment form accepts the same three shapes the inline-JSON loader does, because it goes
/// through the same reader.
fn resolve_pairs(
    flag: &'static str,
    values: &[String],
    env_var: &str,
    env: &dyn EnvLookup,
) -> Result<Vec<HeaderPair>, ConfigError> {
    if !values.is_empty() {
        return Ok(HeaderPair::from_flat(flag, values)?);
    }
    let Some(value) = env.get(env_var).filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let document: serde_json::Value =
        serde_json::from_str(&value).map_err(|error| ConfigError::InvalidEnvironmentValue {
            env_var: env_var.to_owned(),
            detail: error.to_string(),
        })?;
    let pairs = crate::inline_json::pairs_from_value(env_var, &document)?;
    Ok(pairs
        .into_iter()
        .map(|(name, value)| HeaderPair::new(name, value))
        .collect())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Any configuration error.
///
/// [`ConfigError::exit_code`] is the process exit status, and every variant but the destination-URI
/// one reports [`CONFIG_ERROR_EXIT_CODE`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// The argument vector or JSON document could not be resolved.
    #[error(transparent)]
    Arguments(#[from] InlineJsonError),
    /// `clap` rejected the arguments: an unknown flag, a missing required value, a bad number, a
    /// bad failure-policy spelling, an odd `--set-header` occurrence.
    #[error("{message}")]
    Usage {
        /// The formatted `clap` message, which already includes usage text.
        message: String,
    },
    /// `--help` or `--version` was requested. Not a failure: the exit code is 0.
    #[error("{message}")]
    HelpRequested {
        /// The help or version text to print.
        message: String,
    },
    /// `--destinationUri` could not be parsed. The only error with its own exit code.
    #[error(transparent)]
    Destination(#[from] EndpointError),
    /// The TLS parameters are inconsistent.
    #[error(transparent)]
    Tls(#[from] TlsConfigError),
    /// The Kafka parameters are inconsistent.
    #[error(transparent)]
    Kafka(#[from] KafkaConfigError),
    /// A limit was rejected.
    #[error(transparent)]
    Limits(#[from] LimitsError),
    /// A capture interval was rejected.
    #[error(transparent)]
    Timing(#[from] TimingError),
    /// A suppression pattern could not be compiled.
    #[error(transparent)]
    Pattern(#[from] PatternError),
    /// A pair list was malformed.
    #[error(transparent)]
    HeaderPair(#[from] HeaderPairError),
    /// Both `--noCapture` and a broker list were given.
    #[error(
        "--no-capture and --kafka-brokers are mutually exclusive; \
         exactly one capture sink must be selected"
    )]
    CaptureSinkConflict,
    /// Neither `--noCapture` nor a broker list was given.
    #[error(
        "one of --kafka-brokers or --no-capture is required; \
         starting with neither would forward traffic while capturing nothing"
    )]
    NoCaptureSink,
    /// `--maxTrafficBufferSize` was zero.
    #[error(
        "--max-traffic-buffer-size must be at least 1; a zero-byte traffic stream could hold no \
         observation at all"
    )]
    EmptyTrafficBuffer,
    /// An environment variable's value was malformed.
    #[error("the value of ${env_var} is invalid: {detail}")]
    InvalidEnvironmentValue {
        /// The environment variable.
        env_var: String,
        /// What was wrong.
        detail: String,
    },
}

impl ConfigError {
    /// The process exit status for this error.
    ///
    /// [`DESTINATION_URI_EXIT_CODE`] for a destination-URI failure, matching Java's documented
    /// exit 3, and [`CONFIG_ERROR_EXIT_CODE`] for everything else. Java reaches exit 1 for Kafka
    /// auth-flag and TLS errors (spec discrepancies #1 and #3); those report
    /// [`CONFIG_ERROR_EXIT_CODE`] here, so a configuration error is never indistinguishable from
    /// a crash.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::HelpRequested { .. } => 0,
            Self::Destination(_) => DESTINATION_URI_EXIT_CODE,
            _ => CONFIG_ERROR_EXIT_CODE,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    use super::*;
    use crate::cli::REDACTED;
    use crate::inline_json::INLINE_JSON_FLAG;
    use crate::tls::ClientAuth;

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

    fn from_flags(extra: &[&str]) -> Result<ApplicationConfig, ConfigError> {
        from_flags_with_env(extra, &StubEnv::default())
    }

    fn from_flags_with_env(
        extra: &[&str],
        env: &dyn EnvLookup,
    ) -> Result<ApplicationConfig, ConfigError> {
        let mut args = vec![
            "--destination-uri".to_owned(),
            "https://source:9200".to_owned(),
            "--listen-port".to_owned(),
            "9200".to_owned(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        ApplicationConfig::from_args(&args, env)
    }

    fn capturing(extra: &[&str]) -> ApplicationConfig {
        let mut args = vec!["--kafka-brokers", "broker:9092"];
        args.extend_from_slice(extra);
        from_flags(&args).expect("valid configuration")
    }

    #[test]
    fn the_production_inline_json_document_produces_a_complete_configuration() {
        // Exactly the shape setupCapture.ts emits: two argv elements, base64, no flags.
        let document = r#"{
            "listenPort": 9200,
            "destinationUri": "https://source-cluster:9200",
            "insecureDestination": false,
            "noCapture": false,
            "kafkaConnection": "b-1:9096",
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
        let env = StubEnv::with(&[
            (
                "CAPTURE_PROXY_SSL_TRUST_CERT_PEM",
                "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            ),
            ("CAPTURE_PROXY_KAFKA_PASSWORD", "hunter2"),
        ]);
        let config = ApplicationConfig::from_args(
            &[INLINE_JSON_FLAG.to_owned(), BASE64.encode(document)],
            &env,
        )
        .expect("the production document must be accepted");

        assert_eq!(*config.source(), ConfigSource::InlineJson);
        assert_eq!(config.listen_port(), 9200);
        assert_eq!(config.destination().port(), 9200);
        assert!(config.destination().uses_tls());
        assert!(!config.insecure_destination());

        let tls = config.tls().expect("TLS enabled");
        assert_eq!(tls.client_auth(), ClientAuth::Require);
        assert!(tls.trust().is_some());

        let kafka = config.sink().kafka().expect("capture enabled");
        assert_eq!(kafka.brokers(), "b-1:9096");
        assert_eq!(kafka.auth().as_str(), "scram-sha-512");
        assert_eq!(kafka.username(), Some("capture"));
        assert_eq!(
            kafka.password().map(SecretString::expose),
            Some("hunter2"),
            "the password comes from the environment, never from a flag or a JSON key"
        );

        // Spec §10.1: every parameter the orchestration never emits runs at its default.
        assert_eq!(config.failure_policy(), CaptureFailurePolicy::FailClosed);
        assert_eq!(config.timing().flush_interval(), Duration::from_secs(5));
        assert_eq!(
            config.timing().heartbeat_interval(),
            Duration::from_secs(10)
        );
        assert_eq!(
            config.timing().heartbeat_expiration(),
            Duration::from_secs(30)
        );
        assert_eq!(config.limits().max_request_assembly().as_secs(), 300);
        assert_eq!(config.limits().max_connection_duration().as_secs(), 3600);
        assert_eq!(config.limits().max_header_bytes(), 10 * 1024 * 1024);
        assert_eq!(config.limits().max_total_bytes(), 250 * 1024 * 1024);
        assert_eq!(kafka.required_topic_partitions(), 0);
        assert!(config.worker_threads_are_automatic());

        // The empty-string suppression defaults are inert, explicitly.
        assert!(config.suppression().method.is_none());
        assert!(config.suppression().uri_path.is_none());
        assert!(config.suppression().method_and_path.is_none());
        assert!(config.suppression().headers.is_empty());
    }

    #[test]
    fn a_configuration_containing_a_password_does_not_render_it_in_debug_output() {
        let config = from_flags_with_env(
            &[
                "--kafka-brokers",
                "broker:9092",
                "--kafka-auth-type",
                "scram-sha-512",
                "--kafka-user-name",
                "capture",
            ],
            &StubEnv::with(&[("CAPTURE_PROXY_KAFKA_PASSWORD", "hunter2")]),
        )
        .expect("valid configuration");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("hunter2"),
            "the password leaked into the configuration's Debug output: {rendered}"
        );
        assert!(rendered.contains(REDACTED), "{rendered}");
    }

    #[test]
    fn a_trust_pem_is_not_rendered_in_the_configuration_debug_output() {
        let config = from_flags_with_env(
            &[
                "--no-capture",
                "--ssl-cert-chain-file",
                "/etc/proxy-tls/tls.crt",
                "--ssl-key-file",
                "/etc/proxy-tls/tls.key",
                "--ssl-trust-cert-pem-env-var",
                "TRUST_PEM",
            ],
            &StubEnv::with(&[("TRUST_PEM", "-----BEGIN CERTIFICATE-----\nSECRETBLOB\n")]),
        )
        .expect("valid configuration");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("SECRETBLOB"), "{rendered}");
    }

    #[test]
    fn the_unprefixed_kafka_password_fallback_still_works() {
        // ArgNameConstants.java:13 gives --kafkaPassword an unprefixed fallback.
        let config = from_flags_with_env(
            &["--kafka-brokers", "broker:9092"],
            &StubEnv::with(&[("KAFKA_PASSWORD", "from-fallback")]),
        )
        .expect("valid");
        assert_eq!(
            config
                .sink()
                .kafka()
                .and_then(KafkaConfig::password)
                .map(SecretString::expose),
            Some("from-fallback")
        );
    }

    #[test]
    fn the_prefixed_password_wins_over_the_unprefixed_fallback() {
        let config = from_flags_with_env(
            &["--kafka-brokers", "broker:9092"],
            &StubEnv::with(&[
                ("CAPTURE_PROXY_KAFKA_PASSWORD", "prefixed"),
                ("KAFKA_PASSWORD", "unprefixed"),
            ]),
        )
        .expect("valid");
        assert_eq!(
            config
                .sink()
                .kafka()
                .and_then(KafkaConfig::password)
                .map(SecretString::expose),
            Some("prefixed")
        );
    }

    #[test]
    fn the_capture_sink_is_an_exclusive_or() {
        // CaptureProxy.java:329-333.
        let error = from_flags(&["--no-capture", "--kafka-brokers", "broker:9092"])
            .expect_err("both sinks must be refused");
        assert!(matches!(error, ConfigError::CaptureSinkConflict));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);

        let error = from_flags(&[]).expect_err("neither sink must be refused");
        assert!(matches!(error, ConfigError::NoCaptureSink));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);

        assert!(
            from_flags(&["--no-capture"])
                .expect("valid")
                .sink()
                .is_disabled()
        );
        assert!(capturing(&[]).sink().kafka().is_some());
    }

    #[test]
    fn the_orchestration_omits_the_broker_list_exactly_when_no_capture_is_set() {
        // setupCapture.ts:645-651 — which is what makes the exclusive-or satisfiable.
        let document = r#"{"listenPort":9200,"destinationUri":"http://h:9200","noCapture":true}"#;
        let config = ApplicationConfig::from_args(
            &[INLINE_JSON_FLAG.to_owned(), document.to_owned()],
            &StubEnv::default(),
        )
        .expect("valid");
        assert!(config.sink().is_disabled());
    }

    #[test]
    fn an_unparseable_destination_uri_reports_exit_code_3() {
        // CaptureProxy.java:443 — the one configuration error with its own code.
        let args = [
            "--destination-uri".to_owned(),
            "not-a-uri".to_owned(),
            "--listen-port".to_owned(),
            "9200".to_owned(),
            "--no-capture".to_owned(),
        ];
        let error = ApplicationConfig::from_args(&args, &StubEnv::default()).expect_err("bad URI");
        assert!(matches!(error, ConfigError::Destination(_)));
        assert_eq!(error.exit_code(), DESTINATION_URI_EXIT_CODE);
    }

    #[test]
    fn a_kafka_auth_flag_conflict_reports_exit_code_2_not_1() {
        // Spec discrepancy #1: Java throws IllegalArgumentException from inside parseArgs, which
        // its own handler does not catch, so this crashes with exit 1.
        let error = from_flags(&[
            "--kafka-brokers",
            "broker:9092",
            "--enable-msk-auth",
            "--kafka-auth-type",
            "scram-sha-512",
        ])
        .expect_err("incompatible auth flags");
        assert!(matches!(error, ConfigError::Kafka(_)));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn a_tls_material_error_reports_exit_code_2_not_1() {
        // Spec discrepancy #3: Java raises these after parseArgs returned, so they escape the
        // exit-2 handler.
        let error = from_flags(&[
            "--no-capture",
            "--ssl-cert-chain-file",
            "/etc/proxy-tls/tls.crt",
        ])
        .expect_err("a chain needs a key");
        assert!(matches!(error, ConfigError::Tls(_)));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);

        let error = from_flags(&[
            "--no-capture",
            "--ssl-cert-chain-file",
            "/c",
            "--ssl-key-file",
            "/k",
            "--require-client-auth",
        ])
        .expect_err("client auth needs trust material");
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn every_interval_and_byte_rule_is_enforced() {
        // CaptureProxy.java:299-328, in source order.
        for (extra, label) in [
            (
                vec!["--traffic-stream-flush-interval-seconds", "0"],
                "F must be positive",
            ),
            (
                vec!["--heartbeat-interval-seconds", "0"],
                "H must be positive",
            ),
            (
                vec!["--heartbeat-expiration-interval-seconds", "0"],
                "E must be positive",
            ),
            (
                vec![
                    "--heartbeat-interval-seconds",
                    "30",
                    "--heartbeat-expiration-interval-seconds",
                    "30",
                ],
                "E must exceed H",
            ),
            (
                vec!["--max-request-assembly-duration-seconds", "0"],
                "assembly duration must be positive",
            ),
            (
                vec!["--max-connection-duration-seconds", "0"],
                "connection duration must be positive",
            ),
            (
                vec!["--max-incomplete-request-header-bytes", "0"],
                "header bytes must be positive",
            ),
            (
                vec!["--max-incomplete-request-header-bytes", "2147483648"],
                "header bytes must fit in an i32",
            ),
            (
                vec![
                    "--max-incomplete-request-header-bytes",
                    "1048576",
                    "--max-incomplete-request-total-bytes",
                    "1024",
                ],
                "total bytes must be at least header bytes",
            ),
            (
                vec!["--max-traffic-buffer-size", "0"],
                "buffer must hold something",
            ),
        ] {
            let mut args = vec!["--kafka-brokers", "broker:9092"];
            args.extend(extra.iter().copied());
            let error = from_flags(&args).unwrap_err_display(label);
            assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE, "{label}");
        }
    }

    #[test]
    fn a_boundary_value_that_java_accepts_is_still_accepted() {
        // Equality cases: total == header is valid, E = H + 1 is valid.
        capturing(&[
            "--max-incomplete-request-header-bytes",
            "1024",
            "--max-incomplete-request-total-bytes",
            "1024",
            "--heartbeat-interval-seconds",
            "29",
            "--heartbeat-expiration-interval-seconds",
            "30",
        ]);
    }

    #[test]
    fn suppression_patterns_are_compiled_with_whole_string_semantics() {
        let config = capturing(&[
            "--suppress-capture-for-method",
            "GET",
            "--suppress-capture-for-uri-path",
            "/_cluster/health.*",
            "--suppress-method-and-path",
            "GET /_cat/.*",
            "--suppress-capture-for-header-match",
            "user-agent",
            "healthcheck",
        ]);
        let rules = config.suppression();
        let method = rules.method.as_ref().expect("compiled");
        assert!(method.matches("GET"));
        assert!(!method.matches("GET /"), "a search would accept this");
        assert!(
            rules
                .uri_path
                .as_ref()
                .expect("compiled")
                .matches("/_cluster/health?pretty")
        );
        assert!(
            !rules
                .uri_path
                .as_ref()
                .expect("compiled")
                .matches("/foo/_cluster/health")
        );
        assert_eq!(rules.headers.len(), 1);
        assert_eq!(rules.headers[0].name, "user-agent");
        assert!(rules.headers[0].pattern.matches("healthcheck"));
        assert!(!rules.headers[0].pattern.matches("my-healthcheck-agent"));
    }

    #[test]
    fn an_empty_suppression_pattern_is_none_rather_than_a_pattern_that_matches_everything() {
        // The single most dangerous trap in the config surface: the orchestration defaults all
        // three to "", and Regex::is_match is a search.
        let config = capturing(&[
            "--suppress-capture-for-method",
            "",
            "--suppress-capture-for-uri-path",
            "",
            "--suppress-method-and-path",
            "",
        ]);
        assert!(config.suppression().method.is_none());
        assert!(config.suppression().uri_path.is_none());
        assert!(config.suppression().method_and_path.is_none());
    }

    #[test]
    fn an_uncompilable_suppression_pattern_is_a_configuration_error() {
        let error = from_flags(&[
            "--kafka-brokers",
            "broker:9092",
            "--suppress-capture-for-method",
            "(?!GET).*",
        ])
        .expect_err("lookahead is unsupported in Rust regexes");
        assert!(matches!(error, ConfigError::Pattern(_)));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn header_overrides_are_pairs() {
        let config = capturing(&["--set-header", "X-Forwarded-For", "1.2.3.4"]);
        assert_eq!(
            config.set_header(),
            [HeaderPair::new("X-Forwarded-For", "1.2.3.4")].as_slice()
        );
    }

    #[test]
    fn the_pair_lists_are_settable_from_the_environment_as_json() {
        // Spec discrepancy #5: Java cannot set either list from the environment at all.
        let env = StubEnv::with(&[
            (ENV_SET_HEADER, r#"["X-A","one,two"]"#),
            (
                ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH,
                r#"[{"name":"user-agent","value":"healthcheck"}]"#,
            ),
        ]);
        let config = from_flags_with_env(&["--kafka-brokers", "broker:9092"], &env)
            .expect("valid configuration");
        assert_eq!(
            config.set_header(),
            [HeaderPair::new("X-A", "one,two")].as_slice()
        );
        assert_eq!(config.suppression().headers.len(), 1);
        assert_eq!(config.suppression().headers[0].name, "user-agent");
    }

    #[test]
    fn an_explicit_pair_flag_beats_the_environment() {
        let env = StubEnv::with(&[(ENV_SET_HEADER, r#"["X-Env","1"]"#)]);
        let config = from_flags_with_env(
            &[
                "--kafka-brokers",
                "broker:9092",
                "--set-header",
                "X-Flag",
                "2",
            ],
            &env,
        )
        .expect("valid");
        assert_eq!(
            config.set_header(),
            [HeaderPair::new("X-Flag", "2")].as_slice()
        );
    }

    #[test]
    fn a_malformed_pair_environment_value_is_a_configuration_error() {
        let env = StubEnv::with(&[(ENV_SET_HEADER, "X-A: 1")]);
        let error =
            from_flags_with_env(&["--kafka-brokers", "broker:9092"], &env).expect_err("not JSON");
        assert!(matches!(error, ConfigError::InvalidEnvironmentValue { .. }));
        let env = StubEnv::with(&[(ENV_SET_HEADER, r#"["X-A"]"#)]);
        let error =
            from_flags_with_env(&["--kafka-brokers", "broker:9092"], &env).expect_err("odd length");
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn json_mode_and_flag_mode_report_their_source() {
        assert_eq!(*capturing(&[]).source(), ConfigSource::Flags);
    }

    #[test]
    fn unknown_json_keys_are_a_configuration_error() {
        let error = ApplicationConfig::from_args(
            &[
                INLINE_JSON_FLAG.to_owned(),
                r#"{"listenPort":9200,"destinationUri":"http://h:9200","noCapture":true,
                    "newOrchestrationKey":"value"}"#
                    .to_owned(),
            ],
            &StubEnv::default(),
        )
        .expect_err("an unknown key must be rejected");
        assert!(matches!(error, ConfigError::Arguments(_)));
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn the_connection_pool_timeout_is_parsed_even_when_pooling_is_disabled() {
        // CaptureProxy.java:602-604 parses it only when the size is nonzero.
        let config = capturing(&[]);
        assert_eq!(config.connection_pool().size, 0);
        assert_eq!(
            config.connection_pool().recycle_after,
            Duration::from_secs(30)
        );
        let error = from_flags(&[
            "--kafka-brokers",
            "broker:9092",
            "--destination-connection-pool-timeout",
            "30 seconds",
        ])
        .expect_err("a malformed duration must not be silently accepted");
        assert_eq!(error.exit_code(), CONFIG_ERROR_EXIT_CODE);
    }

    #[test]
    fn an_omitted_telemetry_endpoint_disables_that_export() {
        let config = capturing(&[]);
        assert!(config.telemetry().traces.is_none());
        assert!(config.telemetry().metrics.is_none());
        // The orchestration omits the metrics key entirely when it is empty
        // (setupCapture.ts:635-642); an empty string is treated the same way.
        let config = capturing(&["--otel-metrics-collector-endpoint", ""]);
        assert!(config.telemetry().metrics.is_none());
        let config = capturing(&["--otel-metrics-collector-endpoint", "http://otel:4317"]);
        assert_eq!(
            config.telemetry().metrics.as_deref(),
            Some("http://otel:4317")
        );
    }

    #[test]
    fn the_request_line_budget_is_surfaced_and_overridable() {
        // Owner decision OD-1.
        let config = capturing(&[]);
        assert!(config.limits().max_request_line_bytes() > 4096);
        let config = capturing(&["--max-request-line-bytes", "65536"]);
        assert_eq!(config.limits().max_request_line_bytes(), 65536);
    }

    #[test]
    fn the_strictness_mode_defaults_to_strict_and_is_configurable() {
        // Owner decision OD-2.
        assert_eq!(capturing(&[]).strictness(), HttpStrictness::Strict);
        assert_eq!(
            capturing(&["--http-strictness", "lenient-line-endings"]).strictness(),
            HttpStrictness::LenientLineEndings
        );
    }

    #[test]
    fn an_explicit_worker_count_is_carried_verbatim() {
        // Spec §4: CaptureProxyNumThreadsTest asserts 0 round-trips as 0 and 4 as 4.
        assert_eq!(capturing(&[]).worker_threads(), 0);
        assert!(capturing(&[]).worker_threads_are_automatic());
        let config = capturing(&["--num-threads", "4"]);
        assert_eq!(config.worker_threads(), 4);
        assert!(!config.worker_threads_are_automatic());
    }

    #[test]
    fn the_effective_kafka_properties_pin_the_ordering_settings() {
        let config = capturing(&[]);
        let file = [("acks".to_owned(), "1".to_owned())].into_iter().collect();
        let effective = config
            .effective_kafka_properties(&file)
            .expect("capture enabled");
        assert_eq!(effective["acks"], "all");
        assert_eq!(effective["enable.idempotence"], "true");
        assert_eq!(effective["max.in.flight.requests.per.connection"], "5");
        assert_eq!(effective["bootstrap.servers"], "broker:9092");

        assert!(
            from_flags(&["--no-capture"])
                .expect("valid")
                .effective_kafka_properties(&BTreeMap::new())
                .is_none()
        );
    }

    #[test]
    fn the_cdk_invocation_is_accepted_unchanged() {
        // capture-proxy-stack.ts:139-172, with the literal Java flag spellings.
        let args: Vec<String> = [
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
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let config = ApplicationConfig::from_args(&args, &StubEnv::default())
            .expect("the CDK invocation must keep working");
        assert!(config.insecure_destination());
        assert_eq!(config.listen_port(), 9200);
        let kafka = config.sink().kafka().expect("capture enabled");
        assert_eq!(kafka.auth().as_str(), "msk-iam");
        assert_eq!(kafka.required_topic_partitions(), 3);
        assert_eq!(
            config.tls().expect("TLS enabled").client_auth(),
            ClientAuth::None
        );
    }

    #[test]
    fn the_target_proxy_invocation_is_accepted() {
        // stack-composer.ts:588 passes extraArgs "--noCapture" with streaming disabled.
        let args: Vec<String> = [
            "--destinationUri",
            "https://target:9200",
            "--listenPort",
            "9200",
            "--noCapture",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let config = ApplicationConfig::from_args(&args, &StubEnv::default()).expect("valid");
        assert!(config.sink().is_disabled());
    }

    /// Helper so a table-driven failure names its case.
    trait UnwrapErrDisplay {
        fn unwrap_err_display(self, label: &str) -> ConfigError;
    }

    impl UnwrapErrDisplay for Result<ApplicationConfig, ConfigError> {
        fn unwrap_err_display(self, label: &str) -> ConfigError {
            match self {
                Ok(_) => panic!("{label}: expected a configuration error, but it was accepted"),
                Err(error) => error,
            }
        }
    }
}
