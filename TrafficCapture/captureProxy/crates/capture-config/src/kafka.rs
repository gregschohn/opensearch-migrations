//! Kafka configuration inputs (traceability row KAFKA-09, spec §1.2).
//!
//! # What this module is, and is not
//!
//! This is the **configuration contract**: the parameters, the auth-mode resolution, the
//! validation, and the effective property map that Java's `KafkaConfig.buildKafkaProperties`
//! (`KafkaConfig.java:106-134`) would produce. It builds no client.
//!
//! `capture_kafka::client_config` owns the librdkafka side: it assembles each client's
//! configuration from a role allowlist, because librdkafka hard-fails on a key it does not know
//! where the Java client merely logs and continues. That crate depends on this one, so the
//! property map here is the *input* to that assembly and never the other way round.
//!
//! # The properties the proxy pins
//!
//! `enable.idempotence=true`, `acks=all`, and `max.in.flight.requests.per.connection=5` preserve
//! per-partition append order, which the capture protocol depends on. Together with
//! `bootstrap.servers` and `client.id` they are applied with `put` **after** the property file
//! (`KafkaConfig.java:127-132`), so a property file can never weaken them. The `putIfAbsent`
//! defaults at `KafkaConfig.java:113-125` are genuinely overridable and stay that way; see
//! [`OVERRIDABLE_DEFAULTS`].
//!
//! # Owner decision OD-7: partitions are validated, never changed
//!
//! Java's `--minimumKafkaTopicPartitions` *raises* the partition count through the Kafka Admin API
//! before the listener opens (`CaptureProxy.java:410-413`). The owner ruled that the proxy must
//! never do that: partition increase is always out-of-band and intentional. The parameter survives
//! as [`KafkaConfig::required_topic_partitions`] — a **minimum the topic must already satisfy**,
//! checked at startup from producer metadata, failing startup when the topic has fewer. Its
//! canonical flag is renamed `--require-kafka-topic-partitions` to say so, and
//! `--minimumKafkaTopicPartitions` remains an accepted alias for the CDK/ECS path, which always
//! passes it (`capture-proxy-stack.ts:155-161`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::cli::SecretString;

/// How the Kafka clients authenticate (`--kafkaAuthType`, `KafkaConfig.java:17-20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KafkaAuthType {
    /// No authentication. The effective default when the parameter is unset or blank.
    #[default]
    None,
    /// TLS without SASL.
    Ssl,
    /// SASL/SCRAM-SHA-512.
    ScramSha512,
    /// AWS MSK IAM.
    MskIam,
}

impl KafkaAuthType {
    /// The configuration spelling, exactly as `--kafkaAuthType` accepts it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ssl => "ssl",
            Self::ScramSha512 => "scram-sha-512",
            Self::MskIam => "msk-iam",
        }
    }

    /// Whether this mode needs a username and password.
    #[must_use]
    pub fn needs_credentials(self) -> bool {
        matches!(self, Self::ScramSha512)
    }
}

impl fmt::Display for KafkaAuthType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for KafkaAuthType {
    type Err = KafkaConfigError;

    /// Accepts only the four values `KafkaConfig.java:93-100` accepts, case-sensitively as Java's
    /// `equals` comparisons are.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "ssl" => Ok(Self::Ssl),
            "scram-sha-512" => Ok(Self::ScramSha512),
            "msk-iam" => Ok(Self::MskIam),
            other => Err(KafkaConfigError::UnsupportedAuthType {
                value: other.to_owned(),
            }),
        }
    }
}

/// Kafka client properties the proxy applies **after** the property file, so a property file can
/// never weaken them (`KafkaConfig.java:129-132`).
///
/// `bootstrap.servers` and `client.id` are pinned too but are per-configuration, so they come from
/// [`KafkaConfig::pinned_properties`].
pub const PINNED_PROPERTIES: &[(&str, &str)] = &[
    ("enable.idempotence", "true"),
    ("acks", "all"),
    ("max.in.flight.requests.per.connection", "5"),
];

/// Defaults a property file may override (`putIfAbsent`, `KafkaConfig.java:122-125`).
///
/// The two serializer defaults at `KafkaConfig.java:113-121` are not listed: keys and values are
/// bytes in the Rust publisher, so there is no serializer to configure. `max.block.ms` is listed
/// because an existing property file may set it, even though librdkafka's enqueue call returns
/// immediately instead of blocking and `capture_kafka` therefore drops it with a warning.
pub const OVERRIDABLE_DEFAULTS: &[(&str, &str)] = &[
    ("delivery.timeout.ms", "10000"),
    ("request.timeout.ms", "5000"),
    ("max.block.ms", "10000"),
];

/// The validated Kafka configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KafkaConfig {
    brokers: String,
    topic: String,
    client_id: String,
    auth: KafkaAuthType,
    username: Option<String>,
    password: Option<SecretString>,
    listener_name: Option<String>,
    secret_name: Option<String>,
    property_file: Option<PathBuf>,
    required_topic_partitions: u32,
}

/// The Kafka parameters as configured, before validation.
#[derive(Debug, Clone, Default)]
pub struct KafkaInputs {
    /// `--kafkaBrokers` / `--kafkaConnection`.
    pub brokers: Option<String>,
    /// `--kafkaTopic`.
    pub topic: String,
    /// `--kafkaClientId`.
    pub client_id: String,
    /// `--kafkaAuthType`. Blank is treated as unset, as `KafkaConfig.java:79` does.
    pub auth_type: Option<String>,
    /// `--enableMSKAuth`, the legacy switch.
    pub enable_msk_auth: bool,
    /// `--kafkaUserName`.
    pub username: Option<String>,
    /// `--kafkaPassword`, normally from `CAPTURE_PROXY_KAFKA_PASSWORD`.
    pub password: Option<SecretString>,
    /// `--kafkaListenerName`.
    pub listener_name: Option<String>,
    /// `--kafkaSecretName`.
    pub secret_name: Option<String>,
    /// `--kafkaPropertyFile` / `--kafkaConfigFile`.
    pub property_file: Option<PathBuf>,
    /// `--require-kafka-topic-partitions` (Java: `--minimumKafkaTopicPartitions`).
    pub required_topic_partitions: u32,
}

impl KafkaConfig {
    /// Resolves and validates the Kafka parameters.
    ///
    /// # Errors
    ///
    /// * [`KafkaConfigError::MissingBrokers`] when no broker list was configured. The caller
    ///   reaches this only when capture is enabled; the `--noCapture` exclusive-or lives in
    ///   [`crate::application`].
    /// * [`KafkaConfigError::LegacyMskAuthConflict`] for `--enableMSKAuth` together with an
    ///   explicit auth type other than `msk-iam` (`KafkaConfig.java:88-92`).
    /// * [`KafkaConfigError::UnsupportedAuthType`] for anything outside the four accepted values
    ///   (`KafkaConfig.java:93-100`).
    /// * [`KafkaConfigError::MissingCredential`] when `scram-sha-512` lacks a username or
    ///   password. Java omits the JAAS line entirely in that case (`KafkaConfig.java:217`), which
    ///   authenticates as nobody and fails later with an opaque broker error.
    /// * [`KafkaConfigError::EmptyTopic`] for a blank topic name.
    pub fn new(inputs: KafkaInputs) -> Result<Self, KafkaConfigError> {
        let auth = resolve_auth_type(inputs.auth_type.as_deref(), inputs.enable_msk_auth)?;

        let brokers = inputs
            .brokers
            .map(|brokers| brokers.trim().to_owned())
            .filter(|brokers| !brokers.is_empty())
            .ok_or(KafkaConfigError::MissingBrokers)?;

        let topic = inputs.topic.trim().to_owned();
        if topic.is_empty() {
            return Err(KafkaConfigError::EmptyTopic);
        }
        let client_id = inputs.client_id.trim().to_owned();
        if client_id.is_empty() {
            return Err(KafkaConfigError::EmptyClientId);
        }

        let username = non_empty(inputs.username);
        let password = inputs.password.filter(|password| !password.is_empty());

        if auth.needs_credentials() {
            if username.is_none() {
                return Err(KafkaConfigError::MissingCredential {
                    auth: auth.as_str(),
                    missing: "--kafka-user-name",
                });
            }
            if password.is_none() {
                return Err(KafkaConfigError::MissingCredential {
                    auth: auth.as_str(),
                    missing: "CAPTURE_PROXY_KAFKA_PASSWORD",
                });
            }
        }

        Ok(Self {
            brokers,
            topic,
            client_id,
            auth,
            username,
            password,
            listener_name: non_empty(inputs.listener_name),
            secret_name: non_empty(inputs.secret_name),
            property_file: inputs
                .property_file
                .filter(|path| !path.as_os_str().is_empty()),
            required_topic_partitions: inputs.required_topic_partitions,
        })
    }

    /// `bootstrap.servers`.
    #[must_use]
    pub fn brokers(&self) -> &str {
        &self.brokers
    }

    /// The traffic topic.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The producer `client.id`.
    ///
    /// The membership consumer uses `<id>-membership` and Java's admin client used
    /// `<id>-topic-admin` (`KafkaConfig.java:158`, `:182`); under owner decision OD-7 there is no
    /// admin client any more.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The resolved auth mode.
    #[must_use]
    pub fn auth(&self) -> KafkaAuthType {
        self.auth
    }

    /// The SASL principal, if one was configured.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// The SASL password, if one was configured. Never rendered by `Debug` or `Display`.
    #[must_use]
    pub fn password(&self) -> Option<&SecretString> {
        self.password.as_ref()
    }

    /// The orchestration-selected listener name.
    #[must_use]
    pub fn listener_name(&self) -> Option<&str> {
        self.listener_name.as_deref()
    }

    /// The secret that held the auth material.
    #[must_use]
    pub fn secret_name(&self) -> Option<&str> {
        self.secret_name.as_deref()
    }

    /// The property file whose entries override [`OVERRIDABLE_DEFAULTS`].
    ///
    /// Reading and parsing it is `capture_kafka::client_config::load_property_file`'s job; this
    /// crate does not duplicate that parser.
    #[must_use]
    pub fn property_file(&self) -> Option<&Path> {
        self.property_file.as_deref()
    }

    /// The partition count the topic must already have, or `0` for "do not check".
    ///
    /// Owner decision OD-7: this is a validated minimum and never a mutation. See the module
    /// documentation.
    #[must_use]
    pub fn required_topic_partitions(&self) -> u32 {
        self.required_topic_partitions
    }

    /// Every property the property file may not change.
    #[must_use]
    pub fn pinned_properties(&self) -> BTreeMap<String, String> {
        let mut pinned: BTreeMap<String, String> = PINNED_PROPERTIES
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        pinned.insert("bootstrap.servers".to_owned(), self.brokers.clone());
        pinned.insert("client.id".to_owned(), self.client_id.clone());
        pinned
    }

    /// The effective producer properties: overridable defaults, then the property file, then the
    /// pinned values.
    ///
    /// This is the layering `KafkaConfig.buildKafkaProperties` performs
    /// (`KafkaConfig.java:106-134`), expressed as data. `file_properties` is the already-parsed
    /// property file, or an empty map when none was configured.
    ///
    /// Authentication properties are **not** included: they belong to the auth mode, and in Rust
    /// they differ from Java's (librdkafka has no JAAS and no `AWS_MSK_IAM` mechanism).
    /// `capture_kafka` applies them from [`KafkaConfig::auth`].
    #[must_use]
    pub fn effective_properties(
        &self,
        file_properties: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut effective: BTreeMap<String, String> = OVERRIDABLE_DEFAULTS
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        for (key, value) in file_properties {
            effective.insert(key.clone(), value.clone());
        }
        effective.extend(self.pinned_properties());
        effective
    }

    /// Whether a property-file entry would be overruled by a pinned property.
    ///
    /// Java silently overwrites such an entry. `capture_kafka` refuses it at startup instead, so
    /// an operator who tried to weaken ordering finds out; this predicate is how a caller can
    /// report that before building a client.
    #[must_use]
    pub fn property_is_pinned(key: &str) -> bool {
        key == "bootstrap.servers"
            || key == "client.id"
            || PINNED_PROPERTIES.iter().any(|(pinned, _)| *pinned == key)
    }
}

/// Resolves the effective auth mode from the explicit parameter and the legacy switch.
///
/// Reproduces `validateKafkaAuthFlags` and `getEffectiveKafkaAuthType`
/// (`KafkaConfig.java:73-102`): a blank explicit value means "unset", in which case the legacy
/// switch selects `msk-iam`, otherwise `none`. A blank explicit value also skips the legacy
/// compatibility check, exactly as Java's `isBlank` guard does.
///
/// # Errors
///
/// [`KafkaConfigError::LegacyMskAuthConflict`] and [`KafkaConfigError::UnsupportedAuthType`].
pub fn resolve_auth_type(
    auth_type: Option<&str>,
    enable_msk_auth: bool,
) -> Result<KafkaAuthType, KafkaConfigError> {
    let explicit = auth_type.map(str::trim).filter(|value| !value.is_empty());
    match explicit {
        Some(value) => {
            let auth: KafkaAuthType = value.parse()?;
            if enable_msk_auth && auth != KafkaAuthType::MskIam {
                return Err(KafkaConfigError::LegacyMskAuthConflict {
                    auth_type: auth.as_str(),
                });
            }
            Ok(auth)
        }
        None if enable_msk_auth => Ok(KafkaAuthType::MskIam),
        None => Ok(KafkaAuthType::None),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The Kafka parameters are inconsistent.
///
/// Java raises `IllegalArgumentException` for the first two of these, from inside `parseArgs` but
/// outside the `ParameterException` the handler catches, so they produce a raw stack trace and exit
/// 1 rather than the exit 2 every other configuration error produces (spec discrepancy #1). Here
/// they are ordinary configuration errors with the same exit code as the rest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KafkaConfigError {
    /// `--enableMSKAuth` was combined with a different explicit auth type.
    #[error("--enable-msk-auth is only compatible with --kafka-auth-type=msk-iam, not {auth_type}")]
    LegacyMskAuthConflict {
        /// The conflicting auth type.
        auth_type: &'static str,
    },
    /// The auth type is not one of the four accepted values.
    #[error(
        "unsupported --kafka-auth-type {value:?}; expected none, ssl, scram-sha-512, or msk-iam"
    )]
    UnsupportedAuthType {
        /// The rejected value.
        value: String,
    },
    /// No broker list was configured while capture was enabled.
    #[error(
        "--kafka-brokers is required unless --no-capture is set; \
         capture cannot be published with no broker list"
    )]
    MissingBrokers,
    /// An auth mode needs a credential that was not supplied.
    #[error("--kafka-auth-type={auth} requires {missing}")]
    MissingCredential {
        /// The auth mode.
        auth: &'static str,
        /// What is missing.
        missing: &'static str,
    },
    /// The topic name is blank.
    #[error("--kafka-topic must not be empty")]
    EmptyTopic,
    /// The `client.id` is blank.
    #[error("--kafka-client-id must not be empty")]
    EmptyClientId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> KafkaInputs {
        KafkaInputs {
            brokers: Some("b-1:9096,b-2:9096".to_owned()),
            topic: crate::cli::DEFAULT_KAFKA_TOPIC.to_owned(),
            client_id: crate::cli::DEFAULT_KAFKA_CLIENT_ID.to_owned(),
            ..KafkaInputs::default()
        }
    }

    fn scram_inputs() -> KafkaInputs {
        KafkaInputs {
            auth_type: Some("scram-sha-512".to_owned()),
            username: Some("capture".to_owned()),
            password: Some(SecretString::new("hunter2")),
            ..inputs()
        }
    }

    #[test]
    fn the_defaults_match_the_java_constants() {
        let config = KafkaConfig::new(inputs()).expect("valid");
        assert_eq!(config.topic(), "logging-traffic-topic");
        assert_eq!(config.client_id(), "HttpCaptureProxyProducer");
        assert_eq!(config.auth(), KafkaAuthType::None);
        assert_eq!(config.required_topic_partitions(), 0);
        assert!(config.property_file().is_none());
    }

    #[test]
    fn a_blank_auth_type_resolves_to_none() {
        // KafkaConfig.java:79-84, and the orchestration emits "" for unset (spec §10.1).
        for value in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_auth_type(value, false).expect("valid"),
                KafkaAuthType::None,
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_legacy_switch_selects_msk_iam_when_no_auth_type_is_given() {
        // KafkaConfig.java:83. This is the CDK path, which only ever passes --enableMSKAuth.
        assert_eq!(
            resolve_auth_type(None, true).expect("valid"),
            KafkaAuthType::MskIam
        );
        assert_eq!(
            resolve_auth_type(Some(""), true).expect("valid"),
            KafkaAuthType::MskIam,
            "a blank auth type skips the compatibility check, as Java's isBlank guard does"
        );
    }

    #[test]
    fn the_legacy_switch_agrees_with_an_explicit_msk_iam() {
        assert_eq!(
            resolve_auth_type(Some("msk-iam"), true).expect("valid"),
            KafkaAuthType::MskIam
        );
    }

    #[test]
    fn the_legacy_switch_conflicts_with_any_other_explicit_auth_type() {
        // KafkaConfig.java:88-92, asserted on the Java side by
        // CaptureProxySetupTest.testKafkaAuthFlagsRejectConflictingLegacyAndNormalizedValues.
        for value in ["none", "ssl", "scram-sha-512"] {
            let error = resolve_auth_type(Some(value), true).expect_err("incompatible");
            assert!(
                matches!(error, KafkaConfigError::LegacyMskAuthConflict { .. }),
                "{value} produced {error}"
            );
        }
    }

    #[test]
    fn the_auth_type_allowlist_is_exactly_the_four_java_values() {
        for value in ["none", "ssl", "scram-sha-512", "msk-iam"] {
            let auth = resolve_auth_type(Some(value), false).expect("accepted");
            assert_eq!(auth.as_str(), value);
        }
        for value in ["NONE", "SSL", "scram-sha-256", "sasl_ssl", "iam", "plain"] {
            assert!(
                matches!(
                    resolve_auth_type(Some(value), false),
                    Err(KafkaConfigError::UnsupportedAuthType { .. })
                ),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn an_empty_broker_list_is_rejected() {
        for brokers in [None, Some(String::new()), Some("  ".to_owned())] {
            let error = KafkaConfig::new(KafkaInputs {
                brokers,
                ..inputs()
            })
            .expect_err("capture needs brokers");
            assert_eq!(error, KafkaConfigError::MissingBrokers);
        }
    }

    #[test]
    fn scram_requires_both_credentials() {
        // Java omits the JAAS line when either is missing (KafkaConfig.java:217), which
        // authenticates as nobody and fails later against the broker.
        let error = KafkaConfig::new(KafkaInputs {
            username: None,
            ..scram_inputs()
        })
        .expect_err("scram needs a username");
        assert!(matches!(error, KafkaConfigError::MissingCredential { .. }));

        let error = KafkaConfig::new(KafkaInputs {
            password: None,
            ..scram_inputs()
        })
        .expect_err("scram needs a password");
        assert!(matches!(error, KafkaConfigError::MissingCredential { .. }));

        let error = KafkaConfig::new(KafkaInputs {
            password: Some(SecretString::new("")),
            ..scram_inputs()
        })
        .expect_err("an empty password is not a password");
        assert!(matches!(error, KafkaConfigError::MissingCredential { .. }));
    }

    #[test]
    fn the_other_auth_modes_do_not_require_credentials() {
        for value in ["none", "ssl", "msk-iam"] {
            KafkaConfig::new(KafkaInputs {
                auth_type: Some(value.to_owned()),
                ..inputs()
            })
            .unwrap_or_else(|error| panic!("{value}: {error}"));
        }
    }

    #[test]
    fn a_password_is_never_rendered_in_debug_output() {
        let config = KafkaConfig::new(scram_inputs()).expect("valid");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("hunter2"),
            "the password leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains(crate::cli::REDACTED), "{rendered}");
        assert_eq!(
            config.password().map(SecretString::expose),
            Some("hunter2"),
            "the value is still reachable through the one auditable accessor"
        );
    }

    #[test]
    fn the_pinned_properties_are_the_five_correctness_settings() {
        let config = KafkaConfig::new(inputs()).expect("valid");
        let pinned = config.pinned_properties();
        assert_eq!(pinned.len(), 5);
        assert_eq!(pinned["enable.idempotence"], "true");
        assert_eq!(pinned["acks"], "all");
        assert_eq!(pinned["max.in.flight.requests.per.connection"], "5");
        assert_eq!(pinned["bootstrap.servers"], "b-1:9096,b-2:9096");
        assert_eq!(pinned["client.id"], "HttpCaptureProxyProducer");
    }

    #[test]
    fn a_property_file_cannot_weaken_a_pinned_property() {
        // KafkaConfig.java:127-132 applies these with put(), after the file.
        let config = KafkaConfig::new(inputs()).expect("valid");
        let file: BTreeMap<String, String> = [
            ("enable.idempotence", "false"),
            ("acks", "1"),
            ("max.in.flight.requests.per.connection", "100"),
            ("bootstrap.servers", "attacker:9092"),
            ("client.id", "someone-else"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();

        let effective = config.effective_properties(&file);
        assert_eq!(effective["enable.idempotence"], "true");
        assert_eq!(effective["acks"], "all");
        assert_eq!(effective["max.in.flight.requests.per.connection"], "5");
        assert_eq!(effective["bootstrap.servers"], "b-1:9096,b-2:9096");
        assert_eq!(effective["client.id"], "HttpCaptureProxyProducer");
        for key in file.keys() {
            assert!(KafkaConfig::property_is_pinned(key), "{key}");
        }
    }

    #[test]
    fn the_put_if_absent_defaults_stay_overridable() {
        // KafkaConfig.java:122-125 uses putIfAbsent, so the property file wins.
        let config = KafkaConfig::new(inputs()).expect("valid");
        let empty = BTreeMap::new();
        let defaults = config.effective_properties(&empty);
        assert_eq!(defaults["delivery.timeout.ms"], "10000");
        assert_eq!(defaults["request.timeout.ms"], "5000");
        assert_eq!(defaults["max.block.ms"], "10000");

        let file: BTreeMap<String, String> = [
            ("request.timeout.ms", "9000"),
            ("linger.ms", "20"),
            ("reconnect.backoff.max.ms", "212"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let effective = config.effective_properties(&file);
        assert_eq!(effective["request.timeout.ms"], "9000");
        assert_eq!(effective["delivery.timeout.ms"], "10000");
        assert_eq!(effective["linger.ms"], "20");
        assert_eq!(effective["reconnect.backoff.max.ms"], "212");
        assert!(!KafkaConfig::property_is_pinned("request.timeout.ms"));
    }

    #[test]
    fn the_effective_properties_carry_no_credentials() {
        // The auth mode owns those, and librdkafka's spelling differs from Java's anyway.
        let config = KafkaConfig::new(scram_inputs()).expect("valid");
        let effective = config.effective_properties(&BTreeMap::new());
        let rendered = format!("{effective:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("sasl"), "{rendered}");
    }

    #[test]
    fn the_partition_parameter_is_a_validated_minimum_not_a_mutation() {
        // Owner decision OD-7. The CDK passes taskCount + 1 (capture-proxy-stack.ts:155-161).
        let config = KafkaConfig::new(KafkaInputs {
            required_topic_partitions: 3,
            ..inputs()
        })
        .expect("valid");
        assert_eq!(config.required_topic_partitions(), 3);
        // Nothing in this type can change a partition count; there is no setter and no admin
        // client anywhere in the crate.
    }

    #[test]
    fn a_blank_topic_or_client_id_is_rejected() {
        assert_eq!(
            KafkaConfig::new(KafkaInputs {
                topic: "  ".to_owned(),
                ..inputs()
            })
            .expect_err("blank topic"),
            KafkaConfigError::EmptyTopic
        );
        assert_eq!(
            KafkaConfig::new(KafkaInputs {
                client_id: String::new(),
                ..inputs()
            })
            .expect_err("blank client id"),
            KafkaConfigError::EmptyClientId
        );
    }

    #[test]
    fn blank_orchestration_values_are_treated_as_unset() {
        // The orchestration emits "" for kafkaListenerName, kafkaSecretName, and kafkaUserName.
        let config = KafkaConfig::new(KafkaInputs {
            listener_name: Some(String::new()),
            secret_name: Some("  ".to_owned()),
            username: Some(String::new()),
            ..inputs()
        })
        .expect("valid");
        assert!(config.listener_name().is_none());
        assert!(config.secret_name().is_none());
        assert!(config.username().is_none());
    }
}
