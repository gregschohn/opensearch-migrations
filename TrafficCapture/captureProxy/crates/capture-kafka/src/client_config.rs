//! Allowlisted Kafka client configuration and authentication (traceability row KAFKA-09).
//!
//! # Why an allowlist and not a passthrough
//!
//! librdkafka **hard-fails at client creation** on a configuration key it does not recognise, where
//! the Java client logs `was supplied but isn't a known config` and carries on. This is not
//! hypothetical: Java's own membership-consumer properties retain
//! `max.in.flight.requests.per.connection` (`KafkaConfig.java:142-147` removes six producer keys but
//! not that one), so a copy-and-delete port of the property file refuses to start (spec discrepancy
//! D-8). Each client's configuration is therefore **assembled** from:
//!
//! 1. role-appropriate defaults a property file may override;
//! 2. the property file, filtered through that role's allowlist;
//! 3. forced values the property file may never weaken;
//! 4. the auth mode's own properties, which the property file may never supply.
//!
//! A key on **another** role's allowlist is ignored for this role — that is Java's "remove the
//! producer-only keys" behaviour, and it keeps a shared property file working. A key on **no**
//! role's allowlist is a startup error, so a typo cannot silently do nothing. A recognised
//! Java-only key with no librdkafka equivalent is dropped with a warning, so an existing
//! deployment's property file migrates.
//!
//! # The three ordering properties
//!
//! `enable.idempotence=true`, `acks=all`, and `max.in.flight.requests.per.connection=5` are forced
//! and a property file attempting to weaken any of them is a **startup error**, not a silent
//! override. Java forces them too, but only while *building* properties: `KafkaCaptureFactory`
//! receives an already-constructed producer and never inspects it, so an embedder that builds its
//! own producer gets no ordering guarantee at all (spec discrepancy D-1). Here the producer is
//! constructed inside this crate and its **effective** configuration is read back from librdkafka
//! before the process becomes capture-authoritative, which is what the protocol document asks for
//! and what Java never did.
//!
//! # What has no librdkafka equivalent
//!
//! * `key.serializer` / `value.serializer` — keys and values are bytes here, so the whole class of
//!   bug behind spec discrepancy D-6 (the repo's own fixture replaces `key.serializer` with one
//!   that would `ClassCastException` at send time) cannot exist.
//! * `max.block.ms` — librdkafka's enqueue call returns `QueueFull` immediately instead of
//!   blocking, so the `max.block.ms + E` worst-case window of spec discrepancy D-18 does not
//!   exist. Backpressure is applied by [`crate::budget`] instead.
//! * `sasl.jaas.config` and the MSK IAM callback-handler class — librdkafka has no JAAS and no
//!   `AWS_MSK_IAM` mechanism; see [`crate::msk_iam`].
//!
//! # `message.max.bytes`
//!
//! Set from the configured maximum message size. Java never configures it (spec discrepancy D-7),
//! so a deployment raising `--maxTrafficBufferSize` above 1 MiB gets runtime
//! `RecordTooLargeException`s, which are terminal capture failures, from a producer still using the
//! 1 MiB default.
//!
//! # Two librdkafka defaults that differ from the Java client, and what each costs
//!
//! Both were established by reading the values back out of librdkafka, not from documentation or
//! memory. Both are the class of defect that reads in a benchmark as "Rust is slower" or "Rust is
//! broken" and gets attributed to the language rather than to a default.
//!
//! | Property | librdkafka default | Java client default | Forced here | Consequence of inheriting librdkafka's |
//! |---|---|---|---|---|
//! | `linger.ms` (alias `queue.buffering.max.ms`) | **5 ms** | **0** | **0**, overridable | Up to 5 ms added to *every* acknowledgement. Under the default fail-closed policy each critical mutation blocks on its own acknowledgement, and measured on the Java baseline over 38 valid runs with only acknowledgement latency varying — 0.70 ms → 32,245 req/s, 2.73 ms → 7,396, 5.54 ms → 3,595, 808 ms → 95 — that is a **4-9x throughput regression** against the Java baseline, for no protocol benefit. |
//! | `message.max.bytes` | **1,000,000** | 1,048,576 | the configured maximum message size | The default payload limit is `1 MiB - 500 = 1,048,076`, which **exceeds** librdkafka's default. A maximum-size record is then rejected *locally by the producer*, before reaching any broker, and that rejection is a **terminal capture failure** — out of the box, with no misconfiguration. |
//!
//! Both are asserted in both directions: that librdkafka's bare default really is what is claimed
//! (so a librdkafka change is caught rather than silently absorbed), and that the effective value
//! after this module runs is the intended one. `socket.nagle.disable` was checked the same way and
//! already defaults to `true`, matching the Java client's `TCP_NODELAY`, so it needs no override —
//! and that is asserted too.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use rdkafka::ClientConfig;

use crate::error::ClientConfigError;
use crate::msk_iam::MskIamSettings;

/// The default producer `client.id`, matching `KafkaConfig.java:16`.
pub const DEFAULT_CLIENT_ID: &str = "HttpCaptureProxyProducer";

pub use crate::producer_pool::{AUTO_PRODUCER_CAP, MAX_FIXED_PRODUCERS, ProducerSizing};

/// The default traffic topic, matching `KafkaCaptureFactory.java:53`.
pub const DEFAULT_TOPIC: &str = "logging-traffic-topic";

/// Kafka record overhead reserved outside the payload limit, matching
/// `KafkaCaptureFactory.java:61`.
pub const KAFKA_MESSAGE_OVERHEAD_BYTES: usize = 500;

/// The consumer group name prefix. The group is per topic, matching `KafkaConfig.java:159`.
pub const MEMBERSHIP_GROUP_PREFIX: &str = "capture-proxy-membership-";

/// The membership consumer's `client.id` suffix, matching `KafkaConfig.java:158`.
pub const MEMBERSHIP_CLIENT_ID_SUFFIX: &str = "-membership";

/// Which client is being configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRole {
    /// The traffic producer, which owns every published record.
    Producer,
    /// The paused membership consumer, which only ever reads its assignment.
    MembershipConsumer,
}

impl ClientRole {
    /// The role name used in configuration error messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::MembershipConsumer => "membership consumer",
        }
    }
}

/// How the capture clients authenticate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// No authentication and no transport security. `security.protocol` stays `plaintext`.
    None,
    /// TLS without SASL.
    Tls(TlsMaterial),
    /// SASL/SCRAM-SHA-512 over TLS.
    ///
    /// Java omits the JAAS line entirely when either credential is missing
    /// (`KafkaConfig.java:217`), which authenticates as nobody and fails later with an opaque
    /// broker error. Both credentials are required here.
    ScramSha512 {
        /// SASL username.
        username: String,
        /// SASL password.
        password: String,
        /// Optional TLS material for the `SASL_SSL` transport.
        tls: TlsMaterial,
    },
    /// AWS MSK IAM, via SASL/OAUTHBEARER with a `SigV4`-presigned token.
    ///
    /// librdkafka does not implement the `AWS_MSK_IAM` mechanism that the Java client uses, and
    /// there is no `sasl.client.callback.handler.class` equivalent. MSK accepts OAUTHBEARER with a
    /// `SigV4` token instead, which is what [`crate::msk_iam`] produces.
    MskIam(MskIamSettings),
}

impl AuthMode {
    /// The configuration spelling of this mode, matching `--kafkaAuthType`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tls(_) => "ssl",
            Self::ScramSha512 { .. } => "scram-sha-512",
            Self::MskIam(_) => "msk-iam",
        }
    }

    /// Whether this mode needs an OAUTHBEARER token-refresh callback on every client.
    #[must_use]
    pub fn needs_oauth_token(&self) -> bool {
        matches!(self, Self::MskIam(_))
    }
}

/// TLS material for an `ssl` or `sasl_ssl` transport.
///
/// Java supplies **no** `ssl.*` properties for `--kafkaAuthType=ssl` and relies entirely on the
/// property file (`KafkaConfig.java:224` sets only `security.protocol`), so these fields exist to
/// make the material explicit and configurable. Leaving all of them empty uses librdkafka's default
/// trust store, and `ssl.*` keys from the property file still apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsMaterial {
    /// `ssl.ca.location`.
    pub ca_location: Option<String>,
    /// `ssl.certificate.location`, for mutual TLS.
    pub certificate_location: Option<String>,
    /// `ssl.key.location`, for mutual TLS.
    pub key_location: Option<String>,
    /// `ssl.key.password`.
    pub key_password: Option<String>,
}

impl TlsMaterial {
    fn apply(&self, config: &mut ClientConfig) {
        for (key, value) in [
            ("ssl.ca.location", self.ca_location.as_ref()),
            (
                "ssl.certificate.location",
                self.certificate_location.as_ref(),
            ),
            ("ssl.key.location", self.key_location.as_ref()),
            ("ssl.key.password", self.key_password.as_ref()),
        ] {
            if let Some(value) = value {
                config.set(key, value);
            }
        }
    }
}

/// Everything needed to build the capture clients.
#[derive(Debug, Clone)]
pub struct KafkaClientSettings {
    /// `bootstrap.servers`, from `--kafkaBrokers` / `--kafkaConnection`.
    pub bootstrap_servers: String,
    /// The base `client.id`. The membership consumer appends `-membership`.
    pub client_id: String,
    /// The traffic topic.
    pub topic: String,
    /// How to authenticate.
    pub auth: AuthMode,
    /// The maximum Kafka message size, including the reserved record overhead.
    pub max_message_bytes: usize,
    /// Property-file entries, already parsed.
    pub properties: BTreeMap<String, String>,
    /// How many independent traffic producers to run, and how partitions are spread over them
    /// (OD-12; see [`crate::producer_pool`]).
    pub producer_sizing: ProducerSizing,
}

impl KafkaClientSettings {
    /// Settings with the Java defaults and no property file.
    #[must_use]
    pub fn new(bootstrap_servers: impl Into<String>) -> Self {
        Self {
            bootstrap_servers: bootstrap_servers.into(),
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            topic: DEFAULT_TOPIC.to_owned(),
            auth: AuthMode::None,
            max_message_bytes: 1024 * 1024,
            properties: BTreeMap::new(),
            producer_sizing: ProducerSizing::default(),
        }
    }

    /// The payload limit a serialized `CaptureRecord` must not exceed.
    ///
    /// # Errors
    ///
    /// Rejects a maximum message size that leaves no room for Kafka's own record overhead, matching
    /// `CaptureKafkaPublisher.java:326-329`.
    pub fn payload_size_limit(&self) -> Result<usize, ClientConfigError> {
        self.max_message_bytes
            .checked_sub(KAFKA_MESSAGE_OVERHEAD_BYTES)
            .filter(|limit| *limit > 0)
            .ok_or(ClientConfigError::EffectiveConfigMismatch {
                key: "message.max.bytes",
                actual: self.max_message_bytes.to_string(),
                expected: "greater than the 500-byte Kafka record overhead",
            })
    }

    /// The membership consumer's group id, `capture-proxy-membership-<topic>`.
    #[must_use]
    pub fn membership_group_id(&self) -> String {
        format!("{MEMBERSHIP_GROUP_PREFIX}{}", self.topic)
    }

    /// The `client.id` for `role`.
    #[must_use]
    pub fn client_id_for(&self, role: ClientRole) -> String {
        match role {
            ClientRole::Producer => self.client_id.clone(),
            ClientRole::MembershipConsumer => {
                format!("{}{MEMBERSHIP_CLIENT_ID_SUFFIX}", self.client_id)
            }
        }
    }

    /// Assembles the librdkafka configuration for `role`.
    ///
    /// # Errors
    ///
    /// * [`ClientConfigError::MissingBootstrapServers`] for an empty broker list.
    /// * [`ClientConfigError::PropertyNotAllowed`] for a key on no role's allowlist.
    /// * [`ClientConfigError::CannotWeakenOrdering`] for an attempt to weaken a forced property.
    /// * [`ClientConfigError::MissingCredential`] for an incomplete SASL credential.
    pub fn build(&self, role: ClientRole) -> Result<ClientConfig, ClientConfigError> {
        if self.bootstrap_servers.trim().is_empty() {
            return Err(ClientConfigError::MissingBootstrapServers);
        }
        let mut config = ClientConfig::new();

        // 1. Overridable defaults.
        for (key, value) in role_defaults(role, self) {
            config.set(key, value);
        }

        // 2. The property file, filtered, after translating Java TLS store keys.
        let properties = translate_java_tls_properties(&self.properties)?;
        for (key, value) in &properties {
            let key = key.as_str();
            if let Some(forced) = forced_ordering_value(key) {
                if !ordering_value_matches(key, value, forced) {
                    return Err(ClientConfigError::CannotWeakenOrdering {
                        key: forced_ordering_key(key),
                        attempted: value.clone(),
                        forced,
                    });
                }
                continue;
            }
            if JAVA_ONLY_IGNORED.contains(&key) {
                tracing::warn!(
                    property = key,
                    role = role.as_str(),
                    "event=capture_kafka_property_ignored reason=no_librdkafka_equivalent"
                );
                continue;
            }
            if AUTH_OWNED.iter().any(|owned| key.starts_with(owned)) {
                // Auth properties come from the auth mode, never from the file, so a stale
                // credential in a property file cannot silently take precedence.
                if key.starts_with("ssl.") {
                    config.set(key, value);
                    continue;
                }
                return Err(ClientConfigError::PropertyNotAllowed {
                    key: key.to_owned(),
                    role: role.as_str(),
                });
            }
            if ROLE_OWNED.contains(&key) {
                return Err(ClientConfigError::PropertyNotAllowed {
                    key: key.to_owned(),
                    role: role.as_str(),
                });
            }
            if is_allowed(role, key) {
                config.set(key, value);
            } else if is_allowed_for_any_role(key) {
                // Another role's key in a shared property file: ignore it for this role, exactly
                // as Java's "remove the producer-only keys" step does.
                tracing::debug!(
                    property = key,
                    role = role.as_str(),
                    "event=capture_kafka_property_skipped reason=other_client_role"
                );
            } else {
                return Err(ClientConfigError::PropertyNotAllowed {
                    key: key.to_owned(),
                    role: role.as_str(),
                });
            }
        }

        // 3. Forced values.
        config.set("bootstrap.servers", &self.bootstrap_servers);
        config.set("client.id", self.client_id_for(role));
        match role {
            ClientRole::Producer => {
                for (key, value) in FORCED_PRODUCER_ORDERING {
                    config.set(*key, *value);
                }
                // Measured librdkafka default: 1,000,000. Default payload limit:
                // 1 MiB - 500 = 1,048,076, which exceeds it. Leaving the default in place makes a
                // maximum-size record fail locally in the producer, before any broker sees it, and
                // that is a terminal capture failure with no misconfiguration anywhere. Java never
                // sets this at all (spec discrepancy D-7).
                config.set("message.max.bytes", self.max_message_bytes.to_string());
            }
            ClientRole::MembershipConsumer => {
                config.set("group.id", self.membership_group_id());
                config.set("enable.auto.commit", "false");
                config.set("auto.offset.reset", "latest");
            }
        }

        // 4. Auth.
        self.apply_auth(&mut config)?;
        Ok(config)
    }

    fn apply_auth(&self, config: &mut ClientConfig) -> Result<(), ClientConfigError> {
        match &self.auth {
            AuthMode::None => {}
            AuthMode::Tls(tls) => {
                config.set("security.protocol", "ssl");
                tls.apply(config);
            }
            AuthMode::ScramSha512 {
                username,
                password,
                tls,
            } => {
                if username.is_empty() {
                    return Err(ClientConfigError::MissingCredential {
                        mode: "scram-sha-512",
                        missing: "a SASL username",
                    });
                }
                if password.is_empty() {
                    return Err(ClientConfigError::MissingCredential {
                        mode: "scram-sha-512",
                        missing: "a SASL password",
                    });
                }
                config.set("security.protocol", "sasl_ssl");
                config.set("sasl.mechanism", "SCRAM-SHA-512");
                config.set("sasl.username", username);
                config.set("sasl.password", password);
                tls.apply(config);
            }
            AuthMode::MskIam(settings) => {
                settings.validate()?;
                config.set("security.protocol", "sasl_ssl");
                config.set("sasl.mechanism", "OAUTHBEARER");
            }
        }
        Ok(())
    }

    /// Reads librdkafka's **effective** view of the producer's ordering properties back and
    /// confirms it preserves per-partition append order.
    ///
    /// This is what `proxyCaptureProtocol.md` §6.4 asks for and Java never does: "startup must
    /// validate the effective producer configuration before the process becomes
    /// capture-authoritative."
    ///
    /// The values are read from librdkafka itself, not from the map this crate assembled, so a
    /// default or an internal adjustment is visible. librdkafka also errors at client creation when
    /// idempotence conflicts with an explicit `acks` or in-flight value, so a disagreement cannot
    /// survive startup either way.
    ///
    /// # Errors
    ///
    /// * [`ClientConfigError::ClientCreation`] when librdkafka rejects the configuration, which is
    ///   also how an unknown or unsupported property key surfaces.
    /// * [`ClientConfigError::EffectiveConfigMismatch`] when an ordering property is not what the
    ///   protocol requires.
    pub fn assert_effective_ordering(config: &ClientConfig) -> Result<(), ClientConfigError> {
        let native =
            config
                .create_native_config()
                .map_err(|error| ClientConfigError::ClientCreation {
                    role: "producer",
                    detail: error.to_string(),
                })?;
        for (key, expected) in FORCED_PRODUCER_ORDERING {
            let actual = native
                .get(key)
                .map_err(|error| ClientConfigError::ClientCreation {
                    role: "producer",
                    detail: format!("could not read back {key}: {error}"),
                })?;
            if !ordering_value_matches(key, &actual, expected) {
                return Err(ClientConfigError::EffectiveConfigMismatch {
                    key,
                    actual,
                    expected,
                });
            }
        }
        Ok(())
    }
}

/// The three properties that preserve per-partition append order.
///
/// librdkafka's names: `acks` is accepted as an alias for `request.required.acks`, and
/// `max.in.flight.requests.per.connection` as an alias for `max.in.flight`. Setting all three
/// explicitly rather than relying on `enable.idempotence` to force them makes a conflicting
/// property file a startup error instead of a silent override.
pub const FORCED_PRODUCER_ORDERING: &[(&str, &str)] = &[
    ("enable.idempotence", "true"),
    ("acks", "all"),
    ("max.in.flight.requests.per.connection", "5"),
];

fn forced_ordering_key(key: &str) -> &'static str {
    FORCED_PRODUCER_ORDERING
        .iter()
        .find(|(forced, _)| *forced == key)
        .map_or("enable.idempotence", |(forced, _)| *forced)
}

fn forced_ordering_value(key: &str) -> Option<&'static str> {
    FORCED_PRODUCER_ORDERING
        .iter()
        .find(|(forced, _)| *forced == key)
        .map(|(_, value)| *value)
}

/// Compares a configuration value against the required one, tolerating the spellings librdkafka
/// itself reports.
fn ordering_value_matches(key: &str, actual: &str, expected: &str) -> bool {
    let actual = actual.trim();
    if actual.eq_ignore_ascii_case(expected) {
        return true;
    }
    // librdkafka reports `acks=all` as `-1`, which is the same wire value.
    key == "acks" && expected == "all" && actual == "-1"
}

/// Properties the proxy owns and a property file may never set.
const ROLE_OWNED: &[&str] = &[
    "bootstrap.servers",
    "metadata.broker.list",
    "client.id",
    "group.id",
    "enable.auto.commit",
    "auto.commit.interval.ms",
    "auto.offset.reset",
    "message.max.bytes",
    "group.instance.id",
];

/// Translates Java client TLS store properties into their librdkafka equivalents.
///
/// The shipped orchestration mounts this exact property file for SCRAM
/// (`setupCapture.ts:244-245`):
///
/// ```text
/// ssl.truststore.type=PEM
/// ssl.truststore.location=/.../ca.crt
/// ```
///
/// librdkafka has no `ssl.truststore.*` properties at all and hard-fails on an unknown key, so
/// passing these through made every SCRAM deployment refuse to start. The mapping is:
///
/// | Java | librdkafka |
/// |---|---|
/// | `ssl.truststore.location` with `ssl.truststore.type=PEM` | `ssl.ca.location` |
/// | `ssl.truststore.certificates` (inline PEM) | `ssl.ca.pem` |
/// | `ssl.truststore.type=JKS` or `PKCS12` | refused: librdkafka cannot read a Java trust store |
/// | `ssl.truststore.password` | dropped: PEM trust material has no password |
/// | `ssl.keystore.location` with `ssl.keystore.type=PKCS12` | `ssl.keystore.location` (PKCS#12) |
/// | `ssl.keystore.type=JKS` | refused |
/// | `ssl.keystore.type` / `ssl.truststore.type` | consumed by the mapping |
///
/// The Java default trust-store type is JKS, so a location with no type is treated as JKS and
/// refused rather than guessed at. Keys librdkafka already understands (`ssl.ca.location`,
/// `ssl.endpoint.identification.algorithm`, ...) pass through unchanged.
///
/// # Errors
///
/// [`ClientConfigError::UnsupportedJavaTlsStore`] for a store librdkafka cannot read.
fn translate_java_tls_properties(
    properties: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ClientConfigError> {
    const PEM_ONLY: &str = "librdkafka reads PEM trust material only; mount the CA certificate \
        as PEM and set ssl.truststore.type=PEM (or ssl.ca.location directly)";
    const NO_JKS_KEYSTORE: &str = "librdkafka cannot read a Java key store; provide the client \
        certificate as PKCS#12 (ssl.keystore.type=PKCS12) or as PEM via ssl.certificate.location \
        and ssl.key.location";

    let mut translated = properties.clone();
    let truststore_type = translated
        .remove("ssl.truststore.type")
        .map(|value| value.trim().to_ascii_uppercase());
    if let Some(location) = translated.remove("ssl.truststore.location") {
        match truststore_type.as_deref() {
            Some("PEM") => {
                translated.insert("ssl.ca.location".to_owned(), location);
            }
            other => {
                return Err(ClientConfigError::UnsupportedJavaTlsStore {
                    key: "ssl.truststore.type".to_owned(),
                    value: other.unwrap_or("JKS (the Java default)").to_owned(),
                    remedy: PEM_ONLY,
                });
            }
        }
    }
    if let Some(pem) = translated.remove("ssl.truststore.certificates") {
        translated.insert("ssl.ca.pem".to_owned(), pem);
    }
    if translated.remove("ssl.truststore.password").is_some() {
        tracing::debug!(
            "event=capture_kafka_property_ignored property=ssl.truststore.password \
             reason=pem_trust_has_no_password"
        );
    }
    let keystore_type = translated
        .remove("ssl.keystore.type")
        .map(|value| value.trim().to_ascii_uppercase());
    if translated.contains_key("ssl.keystore.location") {
        match keystore_type.as_deref() {
            Some("PKCS12") => {}
            other => {
                return Err(ClientConfigError::UnsupportedJavaTlsStore {
                    key: "ssl.keystore.type".to_owned(),
                    value: other.unwrap_or("JKS (the Java default)").to_owned(),
                    remedy: NO_JKS_KEYSTORE,
                });
            }
        }
    }
    Ok(translated)
}

/// Prefixes owned by the auth mode. An `ssl.*` key is still accepted from the property file,
/// because the Java path supplies none of them and a deployment may legitimately keep its trust
/// material there.
const AUTH_OWNED: &[&str] = &["security.protocol", "sasl.", "ssl."];

/// Java/Kafka-client keys with no librdkafka equivalent. Dropped with a warning so an existing
/// property file migrates instead of blocking startup.
const JAVA_ONLY_IGNORED: &[&str] = &[
    "key.serializer",
    "value.serializer",
    "key.deserializer",
    "value.deserializer",
    "max.block.ms",
    "buffer.memory",
    "send.buffer.bytes.java",
    "partitioner.class",
    "interceptor.classes",
    "metric.reporters",
    "metrics.sample.window.ms",
    "metrics.num.samples",
    "metrics.recording.level",
    "security.providers",
    "connections.max.idle.ms.java",
    "client.dns.lookup.java",
    "max.poll.records",
];

/// Properties either client may tune.
const COMMON_ALLOWED: &[&str] = &[
    // librdkafka's own diagnostic contexts (e.g. `broker,protocol,msg`). Routed through the
    // tracing facade like every other native diagnostic (PROXY-16); needed to see request-level
    // produce pipelining, which is not otherwise observable.
    "debug",
    "api.version.request",
    "api.version.request.timeout.ms",
    "api.version.fallback.ms",
    "broker.address.family",
    "broker.address.ttl",
    "broker.version.fallback",
    "client.dns.lookup",
    "client.rack",
    "connections.max.idle.ms",
    "enable.random.seed",
    "log.connection.close",
    "log_level",
    "metadata.max.age.ms",
    "reconnect.backoff.jitter.ms",
    "reconnect.backoff.max.ms",
    "reconnect.backoff.ms",
    "socket.connection.setup.timeout.ms",
    "socket.keepalive.enable",
    "socket.max.fails",
    "socket.nagle.disable",
    "socket.receive.buffer.bytes",
    "socket.send.buffer.bytes",
    "socket.timeout.ms",
    "statistics.interval.ms",
    "topic.metadata.propagation.max.ms",
    "topic.metadata.refresh.fast.cnt",
    "topic.metadata.refresh.fast.interval.ms",
    "topic.metadata.refresh.interval.ms",
    "topic.metadata.refresh.sparse",
];

/// Producer-only tunables.
const PRODUCER_ALLOWED: &[&str] = &[
    "batch.num.messages",
    "batch.size",
    "compression.codec",
    "compression.level",
    "compression.type",
    "delivery.timeout.ms",
    "linger.ms",
    "message.send.max.retries",
    "message.timeout.ms",
    "queue.buffering.backpressure.threshold",
    "queue.buffering.max.kbytes",
    "queue.buffering.max.messages",
    "queue.buffering.max.ms",
    "request.timeout.ms",
    "retries",
    "retry.backoff.max.ms",
    "retry.backoff.ms",
    "transaction.timeout.ms",
];

/// Membership-consumer-only tunables.
///
/// `partition.assignment.strategy` is deliberately **not** set by the proxy and **is** preserved
/// from the property file: the capture protocol requires no custom assignor, and two Java tests pin
/// that the key is absent unless a property file supplies it.
const CONSUMER_ALLOWED: &[&str] = &[
    "coordinator.query.interval.ms",
    "enable.partition.eof",
    "fetch.error.backoff.ms",
    "fetch.max.bytes",
    "fetch.message.max.bytes",
    "fetch.min.bytes",
    "fetch.queue.backoff.ms",
    "fetch.wait.max.ms",
    "group.protocol",
    "group.remote.assignor",
    "heartbeat.interval.ms",
    "max.partition.fetch.bytes",
    "max.poll.interval.ms",
    "partition.assignment.strategy",
    "queued.max.messages.kbytes",
    "queued.min.messages",
    "session.timeout.ms",
];

fn is_allowed(role: ClientRole, key: &str) -> bool {
    if COMMON_ALLOWED.contains(&key) {
        return true;
    }
    match role {
        ClientRole::Producer => PRODUCER_ALLOWED.contains(&key),
        ClientRole::MembershipConsumer => CONSUMER_ALLOWED.contains(&key),
    }
}

fn is_allowed_for_any_role(key: &str) -> bool {
    COMMON_ALLOWED.contains(&key)
        || PRODUCER_ALLOWED.contains(&key)
        || CONSUMER_ALLOWED.contains(&key)
}

/// The overridable defaults for `role`.
///
/// These are the Java `putIfAbsent` values that have a librdkafka equivalent: a property file wins
/// over them, matching Java's precedence exactly.
fn role_defaults(role: ClientRole, _settings: &KafkaClientSettings) -> Vec<(&'static str, String)> {
    match role {
        ClientRole::Producer => vec![
            // KafkaConfig.java:123-124.
            ("delivery.timeout.ms", "10000".to_owned()),
            ("request.timeout.ms", "5000".to_owned()),
            // DEVIATION from librdkafka's default, to match the Java client's effective default.
            //
            // librdkafka defaults `linger.ms` (alias `queue.buffering.max.ms`) to **5 ms**; the
            // Java producer defaults it to **0**. Java never sets the property, so the Java proxy
            // runs with 0 and a port that leaves librdkafka's default in place silently adds up to
            // 5 ms to *every* acknowledgement.
            //
            // That is not a micro-optimisation. Under the default fail-closed policy every critical
            // mutation blocks on its own acknowledgement, so proxy throughput is inversely
            // proportional to acknowledgement latency. Measured on the Java baseline over 38 valid
            // runs, with only acknowledgement latency varying:
            //
            // | ack latency | throughput |
            // |---|---|
            // | 0.70 ms | 32,245 req/s |
            // | 2.73 ms | 7,396 req/s |
            // | 5.54 ms | 3,595 req/s |
            // | 808 ms | 95 req/s |
            //
            // So inheriting a 5 ms linger would cost roughly a factor of four to nine in
            // throughput against the Java baseline, for no protocol benefit. Batching still
            // happens whenever records are genuinely concurrent, because a batch forms from
            // whatever is queued when the producer thread wakes.
            //
            // Overridable: a deployment that is bandwidth-bound rather than latency-bound may
            // legitimately raise it.
            ("linger.ms", "0".to_owned()),
        ],
        // Java sets none of session.timeout.ms, heartbeat.interval.ms, or max.poll.interval.ms, so
        // neither does this.
        ClientRole::MembershipConsumer => Vec::new(),
    }
}

/// Parses a Java `.properties`-style file.
///
/// Supports `#` and `!` comments, `key=value` and `key:value`, surrounding whitespace, and blank
/// lines. Deliberately does **not** support line continuations or Unicode escapes: a capture proxy
/// should fail loudly on a property file it does not fully understand rather than guess.
///
/// # Errors
///
/// * [`ClientConfigError::PropertyFileUnreadable`] when the file cannot be read. A missing or
///   unreadable property file is a hard startup failure, matching `KafkaConfig.java:201`.
/// * [`ClientConfigError::PropertyFileMalformed`] for a line with no separator.
pub fn load_property_file(path: &Path) -> Result<BTreeMap<String, String>, ClientConfigError> {
    let content = std::fs::read_to_string(path).map_err(|error| {
        ClientConfigError::PropertyFileUnreadable {
            path: path.display().to_string(),
            detail: error.to_string(),
        }
    })?;
    parse_properties(&content, &path.display().to_string())
}

/// Parses property-file `content` that came from `path`.
///
/// # Errors
///
/// [`ClientConfigError::PropertyFileMalformed`] for a line with no `=` or `:` separator.
pub fn parse_properties(
    content: &str,
    path: &str,
) -> Result<BTreeMap<String, String>, ClientConfigError> {
    let mut properties = BTreeMap::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let separator = line
            .find('=')
            .into_iter()
            .chain(line.find(':'))
            .min()
            .ok_or_else(|| ClientConfigError::PropertyFileMalformed {
                path: path.to_owned(),
                line: index + 1,
                content: raw.to_owned(),
            })?;
        let (key, value) = line.split_at(separator);
        properties.insert(key.trim().to_owned(), value[1..].trim().to_owned());
    }
    Ok(properties)
}

/// Generates a process activation id.
///
/// Every writer identity and the probe identity derive from this, so it must be unique per process
/// start. A collision would make two processes' records indistinguishable to the replayer.
#[must_use]
pub fn new_capture_activation_id() -> Arc<str> {
    Arc::from(uuid::Uuid::new_v4().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> KafkaClientSettings {
        KafkaClientSettings::new("broker-1:9092")
    }

    fn effective(config: &ClientConfig, key: &str) -> String {
        config
            .create_native_config()
            .expect("librdkafka accepts the configuration")
            .get(key)
            .unwrap_or_else(|error| panic!("librdkafka knows {key}: {error}"))
    }

    #[test]
    fn the_property_file_cannot_weaken_idempotence_acks_or_in_flight() {
        // KafkaConfigTest.producerPropertiesCannotWeakenOrderingSettings (:78-102).
        for (key, value) in [
            ("enable.idempotence", "false"),
            ("acks", "1"),
            ("max.in.flight.requests.per.connection", "100"),
        ] {
            let mut settings = settings();
            settings.properties.insert(key.to_owned(), value.to_owned());
            let error = settings
                .build(ClientRole::Producer)
                .expect_err("a weakening property must be refused at startup");
            match error {
                ClientConfigError::CannotWeakenOrdering { key: forced, .. } => {
                    assert_eq!(forced, key);
                }
                other => panic!("expected an ordering refusal for {key}, got {other}"),
            }
        }
    }

    #[test]
    fn restating_a_forced_value_is_accepted() {
        let mut settings = settings();
        settings
            .properties
            .insert("acks".to_owned(), "all".to_owned());
        settings
            .properties
            .insert("enable.idempotence".to_owned(), "true".to_owned());
        let config = settings
            .build(ClientRole::Producer)
            .expect("restating the forced value is not weakening it");
        assert_eq!(effective(&config, "enable.idempotence"), "true");
    }

    #[test]
    fn the_effective_producer_configuration_preserves_ordering() {
        // Spec discrepancy D-1: Java never reads the effective configuration back. These values
        // come from librdkafka, not from the map this crate assembled.
        let config = settings()
            .build(ClientRole::Producer)
            .expect("valid configuration");
        KafkaClientSettings::assert_effective_ordering(&config)
            .expect("the forced ordering survives into librdkafka's own view");

        assert_eq!(effective(&config, "enable.idempotence"), "true");
        assert!(
            matches!(effective(&config, "acks").as_str(), "all" | "-1"),
            "acks must be all (librdkafka reports it as -1)"
        );
        assert_eq!(
            effective(&config, "max.in.flight.requests.per.connection"),
            "5"
        );
    }

    #[test]
    fn an_unknown_property_key_is_a_startup_error_with_a_clear_message() {
        // Spec discrepancy D-8: librdkafka hard-fails on an unknown key where Java logs and
        // continues, so a typo must be caught here rather than at client creation.
        let mut settings = settings();
        settings
            .properties
            .insert("lingr.ms".to_owned(), "5".to_owned());
        let error = settings
            .build(ClientRole::Producer)
            .expect_err("a typo must not be silently ignored");
        let text = error.to_string();
        assert!(text.contains("lingr.ms"), "{text}");
        assert!(text.contains("allowlist"), "{text}");
    }

    #[test]
    fn a_shared_property_file_keeps_working_across_both_roles() {
        // Java removes the producer-only keys from the consumer properties, and leaves
        // max.in.flight.requests.per.connection behind, which librdkafka would reject outright.
        let mut settings = settings();
        settings
            .properties
            .insert("linger.ms".to_owned(), "7".to_owned());
        settings
            .properties
            .insert("session.timeout.ms".to_owned(), "6000".to_owned());
        settings.properties.insert(
            "key.serializer".to_owned(),
            "whatever.Serializer".to_owned(),
        );

        let producer = settings
            .build(ClientRole::Producer)
            .expect("the consumer key is skipped, not rejected");
        assert_eq!(effective(&producer, "linger.ms"), "7");

        let consumer = settings
            .build(ClientRole::MembershipConsumer)
            .expect("the producer key is skipped, not rejected");
        assert_eq!(effective(&consumer, "session.timeout.ms"), "6000");
    }

    #[test]
    fn a_property_file_may_not_claim_a_proxy_owned_key() {
        for key in [
            "bootstrap.servers",
            "group.id",
            "client.id",
            "auto.offset.reset",
        ] {
            let mut settings = settings();
            settings
                .properties
                .insert(key.to_owned(), "hijacked".to_owned());
            assert!(
                matches!(
                    settings.build(ClientRole::MembershipConsumer),
                    Err(ClientConfigError::PropertyNotAllowed { .. })
                ),
                "{key} must be owned by the proxy"
            );
        }
    }

    #[test]
    fn a_property_file_may_not_supply_sasl_credentials() {
        let mut settings = settings();
        settings
            .properties
            .insert("sasl.password".to_owned(), "from-the-file".to_owned());
        assert!(matches!(
            settings.build(ClientRole::Producer),
            Err(ClientConfigError::PropertyNotAllowed { .. })
        ));
    }

    #[test]
    fn a_property_file_may_still_supply_tls_material() {
        // The Java path supplies no ssl.* properties at all, so a deployment's trust material
        // legitimately lives in the property file.
        let mut settings = settings();
        settings.auth = AuthMode::Tls(TlsMaterial::default());
        settings
            .properties
            .insert("ssl.ca.location".to_owned(), "/etc/kafka/ca.pem".to_owned());
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "ssl.ca.location"), "/etc/kafka/ca.pem");
        assert_eq!(effective(&config, "security.protocol"), "ssl");
    }

    #[test]
    fn the_overridable_defaults_match_java_and_the_file_wins() {
        // KafkaConfig.java:123-124 uses putIfAbsent, so the property file takes precedence.
        let config = settings().build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "delivery.timeout.ms"), "10000");
        assert_eq!(effective(&config, "request.timeout.ms"), "5000");

        let mut settings = settings();
        settings
            .properties
            .insert("request.timeout.ms".to_owned(), "9000".to_owned());
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "request.timeout.ms"), "9000");
    }

    #[test]
    fn compression_is_unset_by_default_and_overridable() {
        let config = settings().build(ClientRole::Producer).expect("valid");
        assert_eq!(
            effective(&config, "compression.codec"),
            "none",
            "Java never sets compression, so the effective value is none"
        );

        let mut settings = settings();
        settings
            .properties
            .insert("compression.type".to_owned(), "zstd".to_owned());
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "compression.codec"), "zstd");
    }

    #[test]
    fn message_max_bytes_follows_the_configured_maximum() {
        // Spec discrepancy D-7: Java never configures this, so raising the buffer size above 1 MiB
        // produces RecordTooLargeException at runtime, which is a terminal capture failure.
        let mut settings = settings();
        settings.max_message_bytes = 8 * 1024 * 1024;
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(
            effective(&config, "message.max.bytes"),
            (8 * 1024 * 1024).to_string()
        );
        assert_eq!(
            settings.payload_size_limit().expect("valid"),
            8 * 1024 * 1024 - KAFKA_MESSAGE_OVERHEAD_BYTES
        );
    }

    #[test]
    fn linger_is_zero_by_default_because_librdkafka_would_otherwise_add_five_milliseconds() {
        // Measured: librdkafka defaults linger.ms to 5 while the Java client defaults it to 0, and
        // under fail-closed every critical mutation blocks on its acknowledgement, so 5 ms of
        // added latency costs roughly 4-9x throughput against the Java baseline.
        let bare = ClientConfig::new()
            .create_native_config()
            .expect("an empty configuration is valid");
        assert_eq!(
            bare.get("linger.ms").expect("librdkafka knows linger.ms"),
            "5",
            "if librdkafka's default ever becomes 0 this override can be dropped"
        );

        let config = settings().build(ClientRole::Producer).expect("valid");
        assert_eq!(
            effective(&config, "linger.ms"),
            "0",
            "the proxy must not inherit a 5 ms batching delay the Java proxy never had"
        );

        let mut settings = settings();
        settings
            .properties
            .insert("linger.ms".to_owned(), "20".to_owned());
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(
            effective(&config, "linger.ms"),
            "20",
            "a bandwidth-bound deployment may still raise it"
        );
    }

    #[test]
    fn nagle_is_already_disabled_by_default_matching_the_java_client() {
        // The Java client sets TCP_NODELAY; librdkafka already defaults socket.nagle.disable to
        // true, so no override is needed. Asserted so a librdkafka change is caught.
        let bare = ClientConfig::new().create_native_config().expect("valid");
        assert_eq!(bare.get("socket.nagle.disable").expect("known"), "true");
    }

    #[test]
    fn the_default_payload_limit_does_not_exceed_librdkafka_s_own_message_limit() {
        // librdkafka defaults message.max.bytes to 1,000,000, which is BELOW the default payload
        // limit of 1,048,576 - 500 = 1,048,076. Without the explicit override a maximum-size
        // record is rejected locally by the producer, before it ever reaches a broker, which is a
        // terminal capture failure caused purely by a default mismatch.
        let bare = ClientConfig::new().create_native_config().expect("valid");
        assert_eq!(bare.get("message.max.bytes").expect("known"), "1000000");

        let settings = settings();
        let limit = settings.payload_size_limit().expect("valid");
        assert!(
            limit > 1_000_000,
            "the default payload limit {limit} really does exceed librdkafka's default"
        );
        let config = settings.build(ClientRole::Producer).expect("valid");
        let effective_limit: usize = effective(&config, "message.max.bytes")
            .parse()
            .expect("a number");
        assert!(
            effective_limit >= limit + KAFKA_MESSAGE_OVERHEAD_BYTES,
            "message.max.bytes {effective_limit} must admit a maximum-size payload {limit}"
        );
    }

    #[test]
    fn a_message_size_below_the_record_overhead_is_rejected() {
        let mut settings = settings();
        settings.max_message_bytes = KAFKA_MESSAGE_OVERHEAD_BYTES;
        assert!(settings.payload_size_limit().is_err());
        settings.max_message_bytes = KAFKA_MESSAGE_OVERHEAD_BYTES + 1;
        assert_eq!(settings.payload_size_limit().expect("valid"), 1);
    }

    #[test]
    fn the_consumer_group_identity_is_per_topic_and_does_not_force_an_assignor() {
        // KafkaConfigTest.membershipConsumerPropertiesSetGroupIdentityWithoutForcingAnAssignmentStrategy
        // (:105-128) asserts the assignor key is absent.
        let mut settings = settings();
        settings.topic = "traffic".to_owned();
        let config = settings
            .build(ClientRole::MembershipConsumer)
            .expect("valid");
        assert_eq!(
            effective(&config, "group.id"),
            "capture-proxy-membership-traffic"
        );
        assert_eq!(
            effective(&config, "client.id"),
            "HttpCaptureProxyProducer-membership"
        );
        assert_eq!(effective(&config, "enable.auto.commit"), "false");
        assert_eq!(
            effective(&config, "auto.offset.reset"),
            "largest",
            "librdkafka's canonical spelling of `latest`; `latest` is an accepted alias and the \
             two select the same behaviour"
        );
        assert!(
            config.get("partition.assignment.strategy").is_none(),
            "the proxy must not force an assignor"
        );
    }

    #[test]
    fn an_explicit_assignor_from_the_property_file_is_preserved() {
        // KafkaConfigTest.membershipConsumerPropertiesPreserveAnExplicitKafkaAssignmentStrategy
        // (:131-150).
        let mut settings = settings();
        settings.properties.insert(
            "partition.assignment.strategy".to_owned(),
            "cooperative-sticky".to_owned(),
        );
        let config = settings
            .build(ClientRole::MembershipConsumer)
            .expect("valid");
        assert_eq!(
            effective(&config, "partition.assignment.strategy"),
            "cooperative-sticky"
        );
    }

    #[test]
    fn each_auth_mode_sets_exactly_the_properties_it_owns() {
        // KafkaConfigTest.java:24,:33,:41,:50. The SCRAM JAAS string becomes
        // sasl.username/sasl.password, because librdkafka has no JAAS.
        let mut settings = settings();
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(
            effective(&config, "security.protocol"),
            "plaintext",
            "auth mode none sets nothing"
        );

        settings.auth = AuthMode::Tls(TlsMaterial::default());
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "security.protocol"), "ssl");

        settings.auth = AuthMode::ScramSha512 {
            username: "capture".to_owned(),
            password: "secret".to_owned(),
            tls: TlsMaterial::default(),
        };
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "security.protocol"), "sasl_ssl");
        assert_eq!(effective(&config, "sasl.mechanism"), "SCRAM-SHA-512");
        assert_eq!(effective(&config, "sasl.username"), "capture");
    }

    #[test]
    fn an_incomplete_scram_credential_is_a_startup_error_rather_than_anonymous_access() {
        // Java omits the JAAS line when either credential is null (KafkaConfig.java:217), which
        // authenticates as nobody.
        for (username, password) in [("", "secret"), ("capture", "")] {
            let mut settings = settings();
            settings.auth = AuthMode::ScramSha512 {
                username: username.to_owned(),
                password: password.to_owned(),
                tls: TlsMaterial::default(),
            };
            assert!(matches!(
                settings.build(ClientRole::Producer),
                Err(ClientConfigError::MissingCredential { .. })
            ));
        }
    }

    #[test]
    fn msk_iam_uses_oauthbearer_because_librdkafka_has_no_aws_msk_iam_mechanism() {
        let mut settings = settings();
        settings.auth = AuthMode::MskIam(MskIamSettings::new("us-east-2"));
        let config = settings.build(ClientRole::Producer).expect("valid");
        assert_eq!(effective(&config, "security.protocol"), "sasl_ssl");
        assert_eq!(
            effective(&config, "sasl.mechanism"),
            "OAUTHBEARER",
            "librdkafka's mechanisms are GSSAPI, PLAIN, SCRAM-SHA-256/512, and OAUTHBEARER"
        );
        assert!(settings.auth.needs_oauth_token());
    }

    #[test]
    fn an_empty_broker_list_is_refused() {
        let mut settings = settings();
        settings.bootstrap_servers = "   ".to_owned();
        assert!(matches!(
            settings.build(ClientRole::Producer),
            Err(ClientConfigError::MissingBootstrapServers)
        ));
    }

    #[test]
    fn property_parsing_handles_the_java_properties_shapes() {
        let parsed = parse_properties(
            "# a comment\n! another\n\nlinger.ms = 5\nrequest.timeout.ms:7000\n  \n",
            "test.properties",
        )
        .expect("well-formed");
        assert_eq!(parsed.get("linger.ms").map(String::as_str), Some("5"));
        assert_eq!(
            parsed.get("request.timeout.ms").map(String::as_str),
            Some("7000")
        );
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn a_malformed_property_line_names_its_line_number() {
        let error = parse_properties("linger.ms = 5\nthis is not a property\n", "test.properties")
            .expect_err("no separator");
        match error {
            ClientConfigError::PropertyFileMalformed { line, .. } => assert_eq!(line, 2),
            other => panic!("expected a malformed-line error, got {other}"),
        }
    }

    #[test]
    fn an_unreadable_property_file_is_a_hard_startup_failure() {
        // KafkaConfig.java:201 rethrows the IOException.
        let error = load_property_file(Path::new("/nonexistent/capture-kafka.properties"))
            .expect_err("a missing property file is not an empty property file");
        assert!(matches!(
            error,
            ClientConfigError::PropertyFileUnreadable { .. }
        ));
    }

    #[test]
    fn activation_ids_are_unique_per_process_start() {
        let first = new_capture_activation_id();
        let second = new_capture_activation_id();
        assert_ne!(first, second);
        assert_eq!(first.len(), 36, "a hyphenated UUID");
    }

    #[test]
    fn the_orchestration_scram_property_file_translates_to_a_librdkafka_ca_location() {
        // Regression: setupCapture.ts:244-245 emits exactly these two keys for SCRAM. Passed
        // through verbatim, librdkafka rejected them and every SCRAM deployment refused to start.
        let mut file = BTreeMap::new();
        file.insert("ssl.truststore.type".to_owned(), "PEM".to_owned());
        file.insert(
            "ssl.truststore.location".to_owned(),
            "/etc/kafka-ca/ca.crt".to_owned(),
        );
        let translated = translate_java_tls_properties(&file).expect("PEM trust translates");
        assert_eq!(
            translated.get("ssl.ca.location").map(String::as_str),
            Some("/etc/kafka-ca/ca.crt")
        );
        assert!(
            !translated
                .keys()
                .any(|key| key.starts_with("ssl.truststore."))
        );
    }

    #[test]
    fn a_java_trust_store_librdkafka_cannot_read_is_refused_not_guessed() {
        for store_type in [None, Some("JKS"), Some("PKCS12")] {
            let mut file = BTreeMap::new();
            file.insert(
                "ssl.truststore.location".to_owned(),
                "/etc/kafka/truststore".to_owned(),
            );
            if let Some(store_type) = store_type {
                file.insert("ssl.truststore.type".to_owned(), store_type.to_owned());
            }
            assert!(
                matches!(
                    translate_java_tls_properties(&file),
                    Err(ClientConfigError::UnsupportedJavaTlsStore { .. })
                ),
                "trust store type {store_type:?} must be refused"
            );
        }
    }

    #[test]
    fn a_pkcs12_client_keystore_passes_through_and_a_jks_one_is_refused() {
        let mut file = BTreeMap::new();
        file.insert(
            "ssl.keystore.location".to_owned(),
            "/etc/kafka/client.p12".to_owned(),
        );
        file.insert("ssl.keystore.type".to_owned(), "PKCS12".to_owned());
        let translated = translate_java_tls_properties(&file).expect("PKCS#12 is supported");
        assert_eq!(
            translated.get("ssl.keystore.location").map(String::as_str),
            Some("/etc/kafka/client.p12")
        );
        assert!(!translated.contains_key("ssl.keystore.type"));

        file.insert("ssl.keystore.type".to_owned(), "JKS".to_owned());
        assert!(matches!(
            translate_java_tls_properties(&file),
            Err(ClientConfigError::UnsupportedJavaTlsStore { .. })
        ));
    }

    #[test]
    fn librdkafka_native_tls_keys_pass_through_unchanged() {
        let mut file = BTreeMap::new();
        file.insert("ssl.ca.location".to_owned(), "/etc/ca.pem".to_owned());
        file.insert(
            "ssl.endpoint.identification.algorithm".to_owned(),
            "https".to_owned(),
        );
        assert_eq!(translate_java_tls_properties(&file).expect("native"), file);
    }
}
