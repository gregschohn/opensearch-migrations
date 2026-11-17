//! The command-line / parameter model for every configuration input (spec §1.1, §1.2).
//!
//! # What is frozen and what is not
//!
//! The **meaning**, **defaults**, and **validation** of every parameter are frozen by the migration
//! specification. The exact flag spelling is not: the Kubernetes/Argo path emits zero `--flag`
//! arguments and passes one `---INLINE-JSON` document instead (spec §2.5, §10.1). Literal flag
//! spellings matter only to the older CDK/ECS path, the docker-compose dev stack, and tests.
//!
//! Therefore every parameter has a consistent kebab-case canonical long name **and** keeps its
//! Java spelling as an alias, so a CDK/ECS invocation and the existing tests keep working
//! unchanged. [`PARAMETERS`] records the full name set and is what the inline-JSON loader matches
//! JSON keys against.
//!
//! # Environment variables
//!
//! Java derives `CAPTURE_PROXY_*` names by stripping dashes, splitting camel case, and
//! uppercasing (`EnvVarParameterPuller.java:158-173`). Every one of those names is declared here
//! verbatim, so an existing deployment's environment keeps working. Java's puller silently
//! supports only strings, numbers, and booleans, which leaves `--capture-failure-policy`,
//! `--setHeader`, and `--suppressCaptureForHeaderMatch` un-settable by environment (spec
//! discrepancy #5); here **every** parameter is environment-settable.
//!
//! # Secrets
//!
//! `--kafkaPassword` and the trust-PEM value are [`SecretString`]s, whose `Debug` and `Display`
//! render [`REDACTED`]. Java echoes its whole argument vector unredacted, twice
//! (`CaptureProxy.java:337`, `:558`), while all four sibling entry points route the echo through
//! `ArgLogUtils.getRedactedArgs` (spec discrepancy #2). Use [`redact_args`] for the startup echo.

use std::convert::Infallible;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::{ArgAction, Parser};

use crate::policy::{CaptureFailurePolicy, HttpStrictness};

/// What a redacted value renders as, in logs and in `Debug` output.
pub const REDACTED: &str = "<redacted>";

/// Default maximum bytes written to a single traffic stream (`CaptureProxy.java:134`).
pub const DEFAULT_MAX_TRAFFIC_BUFFER_SIZE: u32 = 1024 * 1024;

/// Default Kafka traffic topic (`KafkaCaptureFactory.java:53`).
pub const DEFAULT_KAFKA_TOPIC: &str = "logging-traffic-topic";

/// Default `--kafka-producer-count` (owner decision OD-12): sized automatically, capped at four.
pub const DEFAULT_KAFKA_PRODUCER_COUNT: &str = "auto";

/// Default Kafka producer `client.id` (`KafkaConfig.java:16`).
pub const DEFAULT_KAFKA_CLIENT_ID: &str = "HttpCaptureProxyProducer";

/// Default backside connection-pool recycle age (`CaptureProxy.java:169`).
pub const DEFAULT_DESTINATION_CONNECTION_POOL_TIMEOUT: &str = "PT30S";

/// Worker-thread count meaning "derive it from available parallelism" (`CaptureProxy.java:155`).
///
/// Resolution lives in `capture_transport::listener::worker_thread_count`, which this crate
/// deliberately does not duplicate. See spec §4.
pub const WORKER_THREADS_AUTO: usize = 0;

/// Environment variable for `--set-header`.
///
/// The two pair-list parameters cannot use `clap`'s environment support: that would need a value
/// delimiter, and Java deliberately declares both flags with `NoSplitter` so a header value may
/// contain a comma. The value here is therefore a JSON array, either flat and even-length
/// (`["X-A","1","X-B","2"]`) or a list of pairs (`[["X-A","1"],["X-B","2"]]`), applied by
/// [`crate::application::ApplicationConfig::from_cli`]. Java cannot set either flag from the
/// environment at all (spec discrepancy #5).
pub const ENV_SET_HEADER: &str = "CAPTURE_PROXY_SET_HEADER";

/// Environment variable for `--suppress-capture-for-header-match`. See [`ENV_SET_HEADER`].
pub const ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH: &str =
    "CAPTURE_PROXY_SUPPRESS_CAPTURE_FOR_HEADER_MATCH";

/// Environment variable carrying the Kafka password, as the orchestration supplies it
/// (`setupCapture.ts:756-766`).
pub const ENV_KAFKA_PASSWORD: &str = "CAPTURE_PROXY_KAFKA_PASSWORD";

/// Unprefixed fallback for the Kafka password.
///
/// Java reaches this through `ArgNameConstants.POSSIBLE_CREDENTIALS_ARG_FLAG_NAMES`
/// (`ArgNameConstants.java:12-13`), which matches `--kafkaPassword` but, through a spelling
/// mismatch, not `--kafkaUserName` (spec discrepancy #6). Only the password fallback is reproduced.
pub const ENV_KAFKA_PASSWORD_FALLBACK: &str = "KAFKA_PASSWORD";

/// Reading environment variables, injectable so configuration assembly is testable.
///
/// The trust-PEM indirection (`--sslTrustCertPemEnvVar`) and the unprefixed `KAFKA_PASSWORD`
/// fallback both read the environment by a name that is itself configuration, which `clap`'s `env`
/// support cannot express.
pub trait EnvLookup {
    /// The value of `name`, or `None` when it is unset.
    fn get(&self, name: &str) -> Option<String>;
}

/// [`EnvLookup`] backed by the real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A credential-bearing string that never renders itself.
///
/// `Debug` and `Display` both emit [`REDACTED`]. The value is reachable only through
/// [`SecretString::expose`], which makes every use site auditable by grep.
///
/// This type does **not** zero its memory on drop; nothing in the workspace provides that, and
/// `unsafe_code` is forbidden. It defends against the realistic failure mode — a secret reaching a
/// log line, a panic message, or an error chain — not against process-memory inspection.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps `value` as a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Every call site is a deliberate disclosure.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is the empty string.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl FromStr for SecretString {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(value))
    }
}

/// An ISO-8601 duration, as `--destinationConnectionPoolTimeout` is spelled (`"PT30S"`).
///
/// Java parses this with `Duration.parse` and **only when the pool size is nonzero**
/// (`CaptureProxy.java:602-604`), so a malformed value is silently accepted whenever pooling is
/// disabled. Here it is always parsed, so a typo is a startup error rather than a latent one.
#[derive(Clone, PartialEq, Eq)]
pub struct Iso8601Duration {
    source: String,
    duration: Duration,
}

impl Iso8601Duration {
    /// The parsed duration.
    #[must_use]
    pub fn as_duration(&self) -> Duration {
        self.duration
    }

    /// The value exactly as configured.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for Iso8601Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iso8601Duration")
            .field("source", &self.source)
            .field("duration", &self.duration)
            .finish()
    }
}

impl fmt::Display for Iso8601Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl FromStr for Iso8601Duration {
    type Err = DurationParseError;

    /// Parses the `PnDTnHnMn.nS` subset Java's `Duration.parse` accepts.
    ///
    /// Weeks, months, and years are not part of `Duration.parse`'s grammar and are rejected here
    /// too. A negative duration is rejected: Java accepts one, but a negative recycle age cannot
    /// mean anything to a connection pool.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let duration = parse_iso8601_duration(value)?;
        Ok(Self {
            source: value.to_owned(),
            duration,
        })
    }
}

/// An ISO-8601 duration could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{value:?} is not an ISO-8601 duration of the form PnDTnHnMnS (for example PT30S): {detail}"
)]
pub struct DurationParseError {
    /// The rejected value.
    pub value: String,
    /// Why it was rejected.
    pub detail: &'static str,
}

fn parse_iso8601_duration(value: &str) -> Result<Duration, DurationParseError> {
    let reject = |detail: &'static str| DurationParseError {
        value: value.to_owned(),
        detail,
    };

    let upper = value.to_ascii_uppercase();
    let body = upper
        .strip_prefix('P')
        .ok_or_else(|| reject("missing 'P'"))?;
    if body.is_empty() {
        return Err(reject("no components"));
    }

    let (date_part, time_part) = match body.split_once('T') {
        Some((date, time)) => {
            if time.is_empty() {
                return Err(reject("'T' with no time components"));
            }
            (date, Some(time))
        }
        None => (body, None),
    };

    let mut total = Duration::ZERO;
    let mut saw_component = false;

    if !date_part.is_empty() {
        let digits = date_part
            .strip_suffix('D')
            .ok_or_else(|| reject("the date part accepts only days, as in P1D"))?;
        total += Duration::from_secs(
            parse_unsigned(digits, value)?
                .checked_mul(86_400)
                .ok_or_else(|| reject("the duration overflows"))?,
        );
        saw_component = true;
    }

    if let Some(time_part) = time_part {
        let mut rest = time_part;
        for (suffix, multiplier) in [('H', 3600u64), ('M', 60), ('S', 1)] {
            if let Some(index) = rest.find(suffix) {
                let (digits, remainder) = rest.split_at(index);
                rest = &remainder[1..];
                if suffix == 'S' {
                    total += parse_seconds(digits, value)?;
                } else {
                    total += Duration::from_secs(
                        parse_unsigned(digits, value)?
                            .checked_mul(multiplier)
                            .ok_or_else(|| reject("the duration overflows"))?,
                    );
                }
                saw_component = true;
            }
        }
        if !rest.is_empty() {
            return Err(reject("trailing characters after the time components"));
        }
    }

    if !saw_component {
        return Err(reject("no components"));
    }
    Ok(total)
}

fn parse_unsigned(digits: &str, value: &str) -> Result<u64, DurationParseError> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DurationParseError {
            value: value.to_owned(),
            detail: "a component must be a non-negative integer",
        });
    }
    digits.parse::<u64>().map_err(|_| DurationParseError {
        value: value.to_owned(),
        detail: "a component is too large",
    })
}

fn parse_seconds(digits: &str, value: &str) -> Result<Duration, DurationParseError> {
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let seconds = parse_unsigned(whole, value)?;
    let mut duration = Duration::from_secs(seconds);
    if let Some(fraction) = fraction {
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(DurationParseError {
                value: value.to_owned(),
                detail: "a fractional second must have 1 to 9 digits",
            });
        }
        let mut nanos: u32 = fraction.parse().map_err(|_| DurationParseError {
            value: value.to_owned(),
            detail: "a fractional second is not a number",
        })?;
        for _ in 0..(9 - fraction.len()) {
            nanos *= 10;
        }
        duration += Duration::from_nanos(u64::from(nanos));
    }
    Ok(duration)
}

/// One `name value` pair, as `--setHeader` and `--suppressCaptureForHeaderMatch` take.
///
/// # Why a pair type
///
/// Java declares both flags `arity = 2` and flattens every occurrence into one `List<String>`,
/// which `convertPairListToMap` then reads two elements at a time
/// (`CaptureProxy.java:511-520`). An odd-length list throws `IndexOutOfBoundsException` — not a
/// `ParameterException` — so it escapes the exit-2 handler, and the orchestration schema documents
/// the flag as one `"Header-Name: value"` string per element, which the parser would read as a
/// header *name* (spec discrepancy #18). Modelling the pair explicitly makes the odd-length case a
/// real configuration error and the intent unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderPair {
    /// The header name, or for a suppression rule the header name to test.
    pub name: String,
    /// The header value, or for a suppression rule the regex to test the value against.
    pub value: String,
}

impl HeaderPair {
    /// Builds a pair.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Groups a flat, even-length list into pairs.
    ///
    /// This is the shape both flag mode and the legacy JSON array produce.
    ///
    /// # Errors
    ///
    /// [`HeaderPairError::OddLength`] when `values` has an odd number of elements, and
    /// [`HeaderPairError::EmptyName`] when a pair's name is empty. Java panics on the former with
    /// an `IndexOutOfBoundsException` that escapes its own configuration-error handler.
    pub fn from_flat(flag: &'static str, values: &[String]) -> Result<Vec<Self>, HeaderPairError> {
        if !values.len().is_multiple_of(2) {
            return Err(HeaderPairError::OddLength {
                flag,
                count: values.len(),
            });
        }
        values
            .chunks_exact(2)
            .map(|pair| {
                if pair[0].is_empty() {
                    return Err(HeaderPairError::EmptyName { flag });
                }
                Ok(Self::new(pair[0].clone(), pair[1].clone()))
            })
            .collect()
    }
}

/// A `name value` pair list was malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderPairError {
    /// The flattened list had an odd number of elements.
    #[error(
        "{flag} takes a header name followed by a value, so it needs an even number of values; \
         got {count}"
    )]
    OddLength {
        /// Which flag.
        flag: &'static str,
        /// How many values were supplied.
        count: usize,
    },
    /// A pair's name was empty.
    #[error("{flag} was given an empty header name")]
    EmptyName {
        /// Which flag.
        flag: &'static str,
    },
}

/// What shape a parameter's value has, for inline-JSON translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// An arity-0 boolean switch. In JSON it is a boolean; `false` is expressed as
    /// `--flag=false` so a JSON `false` can override an environment variable that says true.
    Switch,
    /// A single value.
    Value,
    /// A repeatable `name value` pair.
    Pair,
}

/// One parameter's identity: its canonical flag, its Java-compatible aliases, and its shape.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    /// The canonical long name, without leading dashes.
    pub flag: &'static str,
    /// Every additional accepted name, without leading dashes. These are the Java spellings.
    pub aliases: &'static [&'static str],
    /// The value shape.
    pub kind: ParamKind,
}

impl ParamSpec {
    /// Every JSON key that selects this parameter.
    ///
    /// Java registers two keys per declared flag name: the name with leading dashes stripped, and
    /// that name converted to camel case (`JsonCommandLineParser.java:395-398`). This reproduces
    /// that for the canonical name and every alias, so `max-connection-duration-seconds` and
    /// `maxConnectionDurationSeconds` both resolve, as do `kafkaBrokers` and `kafkaConnection`.
    pub fn json_keys(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(self.flag)
            .chain(self.aliases.iter().copied())
            .flat_map(|name| [name.to_owned(), to_camel_case(name)])
    }
}

/// Converts a dash- or underscore-separated name to camel case, as Java's `toCamelCase` does
/// (`JsonCommandLineParser.java:420-437`).
#[must_use]
pub(crate) fn to_camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalize = false;
    for c in name.chars() {
        if c == '-' || c == '_' {
            capitalize = true;
        } else if capitalize {
            out.extend(c.to_uppercase());
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Flags whose value must never be echoed.
///
/// Java's own `ArgNameConstants.CENSORED_ARGS` (`ArgNameConstants.java:48-49`) lists
/// `--kafkaPassword`; the proxy is the only entry point that does not consult it (spec
/// discrepancy #2).
pub const CENSORED_FLAGS: &[&str] = &["kafka-password", "kafkaPassword", "kafkaTrafficPassword"];

/// Redacts an argument vector for the startup echo.
///
/// Three things are hidden: the value following a censored flag, the inline value of a
/// `--flag=value` form of a censored flag, and the whole `---INLINE-JSON` / `---JSON-FILE`
/// payload. The payload is hidden because it is the same document that may carry credentials and
/// because Java's own JSON parser deliberately avoids logging it
/// (`JsonCommandLineParser.java:171`) while `CaptureProxy.java:558` echoes it anyway.
///
/// The flag names themselves are preserved: an operator needs to see *which* parameters were set.
#[must_use]
pub fn redact_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    let mut redact_next = false;
    for arg in args {
        let arg = arg.as_ref();
        if redact_next {
            out.push(REDACTED.to_owned());
            redact_next = false;
            continue;
        }
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg, false),
        };
        let bare = name.trim_start_matches('-');
        if CENSORED_FLAGS.contains(&bare) {
            if inline_value {
                out.push(format!("{name}={REDACTED}"));
            } else {
                out.push(arg.to_owned());
                redact_next = true;
            }
            continue;
        }
        if arg == crate::inline_json::INLINE_JSON_FLAG || arg == crate::inline_json::JSON_FILE_FLAG
        {
            out.push(arg.to_owned());
            redact_next = true;
            continue;
        }
        out.push(arg.to_owned());
    }
    out
}

/// Every parameter, with its canonical flag, its Java aliases, and its value shape.
///
/// This table is what the inline-JSON loader matches keys against, and
/// `clap_and_the_parameter_table_agree` asserts it stays in step with [`CliArgs`].
pub const PARAMETERS: &[ParamSpec] = &[
    ParamSpec {
        flag: "no-capture",
        aliases: &["noCapture"],
        kind: ParamKind::Switch,
    },
    ParamSpec {
        flag: "ssl-cert-chain-file",
        aliases: &["sslCertChainFile"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "ssl-key-file",
        aliases: &["sslKeyFile"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "ssl-trust-cert-file",
        aliases: &["sslTrustCertFile"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "ssl-trust-cert-pem-env-var",
        aliases: &["sslTrustCertPemEnvVar"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "require-client-auth",
        aliases: &["requireClientAuth"],
        kind: ParamKind::Switch,
    },
    ParamSpec {
        flag: "max-traffic-buffer-size",
        aliases: &["maxTrafficBufferSize"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "insecure-destination",
        aliases: &["insecureDestination"],
        kind: ParamKind::Switch,
    },
    ParamSpec {
        flag: "destination-uri",
        aliases: &["destinationUri"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "listen-port",
        aliases: &["listenPort"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "num-threads",
        aliases: &["numThreads"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "destination-connection-pool-size",
        aliases: &["destinationConnectionPoolSize"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "destination-connection-pool-timeout",
        aliases: &["destinationConnectionPoolTimeout"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "otel-trace-collector-endpoint",
        aliases: &["otelTraceCollectorEndpoint"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "otel-metrics-collector-endpoint",
        aliases: &["otelMetricsCollectorEndpoint"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "set-header",
        aliases: &["setHeader"],
        kind: ParamKind::Pair,
    },
    ParamSpec {
        flag: "suppress-capture-for-header-match",
        aliases: &["suppressCaptureForHeaderMatch"],
        kind: ParamKind::Pair,
    },
    ParamSpec {
        flag: "suppress-capture-for-method",
        aliases: &["suppressCaptureForMethod"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "suppress-capture-for-uri-path",
        aliases: &["suppressCaptureForUriPath"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "suppress-method-and-path",
        aliases: &["suppressMethodAndPath"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-topic",
        aliases: &["kafkaTopic"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-producer-count",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "require-kafka-topic-partitions",
        aliases: &[
            "minimumKafkaTopicPartitions",
            "minimum-kafka-topic-partitions",
        ],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "traffic-stream-flush-interval-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "heartbeat-interval-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "heartbeat-expiration-interval-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "clock-skew-allowance-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "max-request-assembly-duration-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "max-connection-duration-seconds",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "max-incomplete-request-header-bytes",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "max-incomplete-request-total-bytes",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "max-request-line-bytes",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "capture-failure-policy",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "http-strictness",
        aliases: &[],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-property-file",
        aliases: &["kafkaPropertyFile", "kafkaConfigFile"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-client-id",
        aliases: &["kafkaClientId"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-brokers",
        aliases: &["kafkaBrokers", "kafkaConnection"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "enable-msk-auth",
        aliases: &["enableMSKAuth"],
        kind: ParamKind::Switch,
    },
    ParamSpec {
        flag: "kafka-auth-type",
        aliases: &["kafkaAuthType"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-listener-name",
        aliases: &["kafkaListenerName"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-secret-name",
        aliases: &["kafkaSecretName"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-user-name",
        aliases: &["kafkaUserName"],
        kind: ParamKind::Value,
    },
    ParamSpec {
        flag: "kafka-password",
        aliases: &["kafkaPassword"],
        kind: ParamKind::Value,
    },
];

/// Looks up the parameter a JSON key selects.
#[must_use]
pub fn parameter_for_json_key(key: &str) -> Option<&'static ParamSpec> {
    PARAMETERS
        .iter()
        .find(|spec| spec.json_keys().any(|candidate| candidate == key))
}

/// The raw parameter model: one field per parameter in spec §1.1 and §1.2.
///
/// Values here are **unvalidated**; cross-field rules, regex compilation, endpoint parsing, and TLS
/// resolution all happen in [`crate::application::ApplicationConfig::from_cli`], so that every
/// configuration error reports one exit code (spec discrepancies #1 and #3).
///
/// Boolean switches are declared with an optional value (`--no-capture` or `--no-capture=false`)
/// rather than as plain flags. The bare form is what the CDK/ECS path and the dev stacks pass; the
/// explicit form is what the inline-JSON loader emits, so a JSON `false` overrides an environment
/// variable that says true, exactly as Java's `populateFromJson` assignment does.
// Five of the booleans are the Java flag surface; collapsing them into an enum would break the
// one-field-per-parameter correspondence this struct exists to provide.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Parser)]
#[command(
    name = "capture-proxy",
    version,
    about = "OpenSearch migrations traffic capture proxy",
    long_about = None,
)]
pub struct CliArgs {
    /// Disable all capture. Mutually exclusive with `--kafka-brokers`.
    #[arg(
        long = "no-capture",
        alias = "noCapture",
        env = "CAPTURE_PROXY_NO_CAPTURE",
        num_args = 0..=1,
        require_equals = true,
        default_value_t = false,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    pub no_capture: bool,

    /// PEM certificate chain for the client-facing TLS listener.
    #[arg(
        long = "ssl-cert-chain-file",
        alias = "sslCertChainFile",
        env = "CAPTURE_PROXY_SSL_CERT_CHAIN_FILE",
        value_name = "PATH"
    )]
    pub ssl_cert_chain_file: Option<PathBuf>,

    /// PEM private key matching `--ssl-cert-chain-file`.
    #[arg(
        long = "ssl-key-file",
        alias = "sslKeyFile",
        env = "CAPTURE_PROXY_SSL_KEY_FILE",
        value_name = "PATH"
    )]
    pub ssl_key_file: Option<PathBuf>,

    /// PEM trust anchors for client certificates. Mutually exclusive with
    /// `--ssl-trust-cert-pem-env-var`.
    #[arg(
        long = "ssl-trust-cert-file",
        alias = "sslTrustCertFile",
        env = "CAPTURE_PROXY_SSL_TRUST_CERT_FILE",
        value_name = "PATH"
    )]
    pub ssl_trust_cert_file: Option<PathBuf>,

    /// Name of an environment variable whose value is the client-certificate trust PEM.
    #[arg(
        long = "ssl-trust-cert-pem-env-var",
        alias = "sslTrustCertPemEnvVar",
        env = "CAPTURE_PROXY_SSL_TRUST_CERT_PEM_ENV_VAR",
        value_name = "NAME"
    )]
    pub ssl_trust_cert_pem_env_var: Option<String>,

    /// Require a valid client certificate. Needs one of the two trust parameters.
    #[arg(
        long = "require-client-auth",
        alias = "requireClientAuth",
        env = "CAPTURE_PROXY_REQUIRE_CLIENT_AUTH",
        num_args = 0..=1,
        require_equals = true,
        default_value_t = false,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    pub require_client_auth: bool,

    /// Maximum bytes written to a single traffic stream.
    #[arg(
        long = "max-traffic-buffer-size",
        alias = "maxTrafficBufferSize",
        env = "CAPTURE_PROXY_MAX_TRAFFIC_BUFFER_SIZE",
        default_value_t = DEFAULT_MAX_TRAFFIC_BUFFER_SIZE,
        value_name = "BYTES"
    )]
    pub max_traffic_buffer_size: u32,

    /// Do not verify the destination's certificate.
    #[arg(
        long = "insecure-destination",
        alias = "insecureDestination",
        env = "CAPTURE_PROXY_INSECURE_DESTINATION",
        num_args = 0..=1,
        require_equals = true,
        default_value_t = false,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    pub insecure_destination: bool,

    /// URI of the upstream cluster the proxy forwards to. Required.
    #[arg(
        long = "destination-uri",
        alias = "destinationUri",
        env = "CAPTURE_PROXY_DESTINATION_URI",
        required = true,
        value_name = "URI"
    )]
    pub destination_uri: String,

    /// Port the proxy's client-facing listener binds. Required.
    #[arg(
        long = "listen-port",
        alias = "listenPort",
        env = "CAPTURE_PROXY_LISTEN_PORT",
        required = true,
        value_name = "PORT"
    )]
    pub listen_port: u16,

    /// Worker-thread count. `0` derives it from available parallelism.
    #[arg(
        long = "num-threads",
        alias = "numThreads",
        env = "CAPTURE_PROXY_NUM_THREADS",
        default_value_t = WORKER_THREADS_AUTO,
        value_name = "COUNT"
    )]
    pub num_threads: usize,

    /// Warm backside connections per worker. `0` disables pooling.
    #[arg(
        long = "destination-connection-pool-size",
        alias = "destinationConnectionPoolSize",
        env = "CAPTURE_PROXY_DESTINATION_CONNECTION_POOL_SIZE",
        default_value_t = 0,
        value_name = "COUNT"
    )]
    pub destination_connection_pool_size: u32,

    /// Recycle age for pooled backside connections, as an ISO-8601 duration.
    #[arg(
        long = "destination-connection-pool-timeout",
        alias = "destinationConnectionPoolTimeout",
        env = "CAPTURE_PROXY_DESTINATION_CONNECTION_POOL_TIMEOUT",
        default_value = DEFAULT_DESTINATION_CONNECTION_POOL_TIMEOUT,
        value_name = "DURATION"
    )]
    pub destination_connection_pool_timeout: Iso8601Duration,

    /// OTLP trace endpoint. Omit to disable trace export.
    #[arg(
        long = "otel-trace-collector-endpoint",
        alias = "otelTraceCollectorEndpoint",
        env = "CAPTURE_PROXY_OTEL_TRACE_COLLECTOR_ENDPOINT",
        value_name = "ENDPOINT"
    )]
    pub otel_trace_collector_endpoint: Option<String>,

    /// OTLP metric endpoint. Omit to disable metric export.
    #[arg(
        long = "otel-metrics-collector-endpoint",
        alias = "otelMetricsCollectorEndpoint",
        env = "CAPTURE_PROXY_OTEL_METRICS_COLLECTOR_ENDPOINT",
        value_name = "ENDPOINT"
    )]
    pub otel_metrics_collector_endpoint: Option<String>,

    /// Force an HTTP header on forwarded requests, removing any existing one. Repeatable.
    ///
    /// Values are never split on any character, matching Java's `NoSplitter`
    /// (`CaptureProxy.java:188`), so a header value may contain a comma. Environment support is
    /// therefore not `clap`'s: see [`ENV_SET_HEADER`].
    #[arg(
        long = "set-header",
        alias = "setHeader",
        num_args = 2,
        action = ArgAction::Append,
        value_names = ["NAME", "VALUE"],
    )]
    pub set_header: Vec<String>,

    /// Header name and a regex; a matching request is forwarded but not captured. Repeatable.
    ///
    /// Values are never split, as above; see [`ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH`].
    #[arg(
        long = "suppress-capture-for-header-match",
        alias = "suppressCaptureForHeaderMatch",
        num_args = 2,
        action = ArgAction::Append,
        value_names = ["NAME", "REGEX"],
    )]
    pub suppress_capture_for_header_match: Vec<String>,

    /// Suppress capture when the request method matches this whole-string regex.
    #[arg(
        long = "suppress-capture-for-method",
        alias = "suppressCaptureForMethod",
        env = "CAPTURE_PROXY_SUPPRESS_CAPTURE_FOR_METHOD",
        value_name = "REGEX"
    )]
    pub suppress_capture_for_method: Option<String>,

    /// Suppress capture when the request path matches this whole-string regex.
    #[arg(
        long = "suppress-capture-for-uri-path",
        alias = "suppressCaptureForUriPath",
        env = "CAPTURE_PROXY_SUPPRESS_CAPTURE_FOR_URI_PATH",
        value_name = "REGEX"
    )]
    pub suppress_capture_for_uri_path: Option<String>,

    /// Suppress capture when `"<METHOD> <PATH>"` matches this whole-string regex.
    #[arg(
        long = "suppress-method-and-path",
        alias = "suppressMethodAndPath",
        env = "CAPTURE_PROXY_SUPPRESS_METHOD_AND_PATH",
        value_name = "REGEX"
    )]
    pub suppress_method_and_path: Option<String>,

    /// Kafka topic captured traffic is published to.
    #[arg(
        long = "kafka-topic",
        alias = "kafkaTopic",
        env = "CAPTURE_PROXY_KAFKA_TOPIC",
        default_value = DEFAULT_KAFKA_TOPIC,
        value_name = "TOPIC"
    )]
    pub kafka_topic: String,

    /// Kafka producers publishing captured traffic: `auto`, or a fixed count (owner decision OD-12).
    ///
    /// `auto` runs `min(4, the most assigned partitions led by one broker)` producers, created as
    /// partitions are assigned: one when each broker leads at most one of this proxy's partitions,
    /// more when several share a broker (librdkafka sends one partition per request, and idempotence
    /// caps each producer at five requests in flight per broker). A number fixes the count. Either
    /// way each partition keeps one producer for the life of the process, and each broker's
    /// partitions are spread across producers.
    #[arg(
        long = "kafka-producer-count",
        env = "CAPTURE_PROXY_KAFKA_PRODUCER_COUNT",
        default_value = DEFAULT_KAFKA_PRODUCER_COUNT,
        value_name = "auto|COUNT"
    )]
    pub kafka_producer_count: crate::kafka::ProducerCount,

    /// Partition count the traffic topic must already have. `0` disables the check.
    ///
    /// Owner decision OD-7: the proxy never creates a topic and never changes a partition count.
    /// A topic with fewer partitions than this fails startup before the listener opens.
    #[arg(
        long = "require-kafka-topic-partitions",
        alias = "minimumKafkaTopicPartitions",
        alias = "minimum-kafka-topic-partitions",
        env = "CAPTURE_PROXY_MINIMUM_KAFKA_TOPIC_PARTITIONS",
        default_value_t = 0,
        value_name = "COUNT"
    )]
    pub require_kafka_topic_partitions: u32,

    /// `F`: maximum age of a nonempty connection-local traffic stream, in seconds.
    #[arg(
        long = "traffic-stream-flush-interval-seconds",
        env = "CAPTURE_PROXY_TRAFFIC_STREAM_FLUSH_INTERVAL_SECONDS",
        default_value_t = 5,
        value_name = "SECONDS"
    )]
    pub traffic_stream_flush_interval_seconds: u32,

    /// `H`: writer-partition heartbeat cadence, in seconds.
    #[arg(
        long = "heartbeat-interval-seconds",
        env = "CAPTURE_PROXY_HEARTBEAT_INTERVAL_SECONDS",
        default_value_t = 10,
        value_name = "SECONDS"
    )]
    pub heartbeat_interval_seconds: u32,

    /// `E`: maximum accepted interval between heartbeats, in seconds. Must exceed `H`.
    #[arg(
        long = "heartbeat-expiration-interval-seconds",
        env = "CAPTURE_PROXY_HEARTBEAT_EXPIRATION_INTERVAL_SECONDS",
        default_value_t = 30,
        value_name = "SECONDS"
    )]
    pub heartbeat_expiration_interval_seconds: u32,

    /// `S`: tolerated backward broker-time movement, in seconds (owner decision OD-6).
    #[arg(
        long = "clock-skew-allowance-seconds",
        env = "CAPTURE_PROXY_CLOCK_SKEW_ALLOWANCE_SECONDS",
        default_value_t = 5,
        value_name = "SECONDS"
    )]
    pub clock_skew_allowance_seconds: u32,

    /// Maximum time to assemble one incomplete request, in seconds.
    #[arg(
        long = "max-request-assembly-duration-seconds",
        env = "CAPTURE_PROXY_MAX_REQUEST_ASSEMBLY_DURATION_SECONDS",
        default_value_t = 300,
        value_name = "SECONDS"
    )]
    pub max_request_assembly_duration_seconds: u64,

    /// Maximum lifetime of one client connection, in seconds.
    #[arg(
        long = "max-connection-duration-seconds",
        env = "CAPTURE_PROXY_MAX_CONNECTION_DURATION_SECONDS",
        default_value_t = 3600,
        value_name = "SECONDS"
    )]
    pub max_connection_duration_seconds: u64,

    /// Maximum aggregate header bytes for one incomplete request.
    #[arg(
        long = "max-incomplete-request-header-bytes",
        env = "CAPTURE_PROXY_MAX_INCOMPLETE_REQUEST_HEADER_BYTES",
        default_value_t = 10 * 1024 * 1024,
        value_name = "BYTES"
    )]
    pub max_incomplete_request_header_bytes: u64,

    /// Maximum aggregate wire bytes for one incomplete request. Must be at least the header budget.
    #[arg(
        long = "max-incomplete-request-total-bytes",
        env = "CAPTURE_PROXY_MAX_INCOMPLETE_REQUEST_TOTAL_BYTES",
        default_value_t = 250 * 1024 * 1024,
        value_name = "BYTES"
    )]
    pub max_incomplete_request_total_bytes: u64,

    /// Maximum request-line bytes (owner decision OD-1). Derived from the header budget if unset.
    #[arg(
        long = "max-request-line-bytes",
        env = "CAPTURE_PROXY_MAX_REQUEST_LINE_BYTES",
        value_name = "BYTES"
    )]
    pub max_request_line_bytes: Option<u64>,

    /// What the process does when required capture fails.
    #[arg(
        long = "capture-failure-policy",
        env = "CAPTURE_PROXY_CAPTURE_FAILURE_POLICY",
        default_value_t = CaptureFailurePolicy::FailClosed,
        value_name = "POLICY"
    )]
    pub capture_failure_policy: CaptureFailurePolicy,

    /// HTTP/1 line-parsing strictness (owner decision OD-2).
    #[arg(
        long = "http-strictness",
        env = "CAPTURE_PROXY_HTTP_STRICTNESS",
        default_value_t = HttpStrictness::Strict,
        value_name = "MODE"
    )]
    pub http_strictness: HttpStrictness,

    /// Extra Kafka client properties, loaded before the proxy's own settings are applied.
    #[arg(
        long = "kafka-property-file",
        alias = "kafkaPropertyFile",
        alias = "kafkaConfigFile",
        env = "CAPTURE_PROXY_KAFKA_PROPERTY_FILE",
        value_name = "PATH"
    )]
    pub kafka_property_file: Option<PathBuf>,

    /// Kafka producer `client.id`.
    #[arg(
        long = "kafka-client-id",
        alias = "kafkaClientId",
        env = "CAPTURE_PROXY_KAFKA_CLIENT_ID",
        default_value = DEFAULT_KAFKA_CLIENT_ID,
        value_name = "ID"
    )]
    pub kafka_client_id: String,

    /// Comma-separated Kafka bootstrap brokers. Mutually exclusive with `--no-capture`.
    #[arg(
        long = "kafka-brokers",
        alias = "kafkaBrokers",
        alias = "kafkaConnection",
        env = "CAPTURE_PROXY_KAFKA_BROKERS",
        value_name = "HOST:PORT,..."
    )]
    pub kafka_brokers: Option<String>,

    /// Legacy MSK IAM switch. Only compatible with `--kafka-auth-type=msk-iam`.
    #[arg(
        long = "enable-msk-auth",
        alias = "enableMSKAuth",
        env = "CAPTURE_PROXY_ENABLE_MSK_AUTH",
        num_args = 0..=1,
        require_equals = true,
        default_value_t = false,
        default_missing_value = "true",
        action = ArgAction::Set,
    )]
    pub enable_msk_auth: bool,

    /// Kafka client auth mode: `none`, `ssl`, `scram-sha-512`, or `msk-iam`.
    ///
    /// Kept as a string rather than an enum because the orchestration emits the empty string for
    /// "unset" (spec §10.1), which Java treats as `none` (`KafkaConfig.java:78-84`).
    #[arg(
        long = "kafka-auth-type",
        alias = "kafkaAuthType",
        env = "CAPTURE_PROXY_KAFKA_AUTH_TYPE",
        value_name = "MODE"
    )]
    pub kafka_auth_type: Option<String>,

    /// Kafka listener name chosen by orchestration.
    #[arg(
        long = "kafka-listener-name",
        alias = "kafkaListenerName",
        env = "CAPTURE_PROXY_KAFKA_LISTENER_NAME",
        value_name = "NAME"
    )]
    pub kafka_listener_name: Option<String>,

    /// Name of the secret holding Kafka auth material.
    #[arg(
        long = "kafka-secret-name",
        alias = "kafkaSecretName",
        env = "CAPTURE_PROXY_KAFKA_SECRET_NAME",
        value_name = "NAME"
    )]
    pub kafka_secret_name: Option<String>,

    /// SCRAM principal.
    #[arg(
        long = "kafka-user-name",
        alias = "kafkaUserName",
        env = "CAPTURE_PROXY_KAFKA_USER_NAME",
        value_name = "NAME"
    )]
    pub kafka_user_name: Option<String>,

    /// SCRAM password. Prefer the `CAPTURE_PROXY_KAFKA_PASSWORD` environment variable.
    ///
    /// The orchestration always supplies it by environment, from a `secretKeyRef`, never as a flag
    /// or a JSON key (`setupCapture.ts:756-766`).
    #[arg(
        long = "kafka-password",
        alias = "kafkaPassword",
        env = "CAPTURE_PROXY_KAFKA_PASSWORD",
        value_name = "PASSWORD",
        hide_env_values = true
    )]
    pub kafka_password: Option<SecretString>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(extra: &[&str]) -> CliArgs {
        let mut argv = vec![
            "capture-proxy",
            "--destination-uri",
            "https://source:9200",
            "--listen-port",
            "9200",
        ];
        argv.extend_from_slice(extra);
        CliArgs::try_parse_from(argv).expect("valid arguments")
    }

    #[test]
    fn the_clap_model_accepts_the_java_flag_spellings() {
        // The CDK/ECS path and the dev stacks pass these literal spellings.
        let args = CliArgs::try_parse_from([
            "capture-proxy",
            "--destinationUri",
            "https://source:9200",
            "--listenPort",
            "9200",
            "--insecureDestination",
            "--sslCertChainFile",
            "/usr/share/captureProxy/config/pub.pem",
            "--sslKeyFile",
            "/usr/share/captureProxy/config/key.pem",
            "--kafkaConnection",
            "broker:9092",
            "--minimumKafkaTopicPartitions",
            "3",
            "--enableMSKAuth",
        ])
        .expect("the Java spellings must keep working");
        assert_eq!(args.destination_uri, "https://source:9200");
        assert_eq!(args.listen_port, 9200);
        assert!(args.insecure_destination);
        assert_eq!(args.kafka_brokers.as_deref(), Some("broker:9092"));
        assert_eq!(args.require_kafka_topic_partitions, 3);
        assert!(args.enable_msk_auth);
    }

    #[test]
    fn defaults_match_the_java_field_initializers() {
        let args = parse(&[]);
        assert!(!args.no_capture);
        assert!(!args.require_client_auth);
        assert!(!args.insecure_destination);
        assert!(!args.enable_msk_auth);
        assert_eq!(args.max_traffic_buffer_size, 1_048_576);
        assert_eq!(args.num_threads, 0);
        assert_eq!(args.destination_connection_pool_size, 0);
        assert_eq!(
            args.destination_connection_pool_timeout.as_duration(),
            Duration::from_secs(30)
        );
        assert_eq!(args.kafka_topic, "logging-traffic-topic");
        assert_eq!(args.kafka_client_id, "HttpCaptureProxyProducer");
        assert_eq!(args.require_kafka_topic_partitions, 0);
        assert_eq!(args.traffic_stream_flush_interval_seconds, 5);
        assert_eq!(args.heartbeat_interval_seconds, 10);
        assert_eq!(args.heartbeat_expiration_interval_seconds, 30);
        assert_eq!(args.max_request_assembly_duration_seconds, 300);
        assert_eq!(args.max_connection_duration_seconds, 3600);
        assert_eq!(args.max_incomplete_request_header_bytes, 10_485_760);
        assert_eq!(args.max_incomplete_request_total_bytes, 262_144_000);
        assert_eq!(
            args.capture_failure_policy,
            CaptureFailurePolicy::FailClosed
        );
        assert_eq!(args.http_strictness, HttpStrictness::Strict);
        assert_eq!(args.max_request_line_bytes, None);
        assert_eq!(args.clock_skew_allowance_seconds, 5);
        assert!(args.set_header.is_empty());
        assert!(args.suppress_capture_for_header_match.is_empty());
        assert_eq!(args.suppress_capture_for_method, None);
        assert_eq!(args.otel_metrics_collector_endpoint, None);
    }

    #[test]
    fn the_required_parameters_are_required() {
        assert!(
            CliArgs::try_parse_from(["capture-proxy", "--listen-port", "9200"]).is_err(),
            "--destination-uri is required (CaptureProxy.java:140)"
        );
        assert!(
            CliArgs::try_parse_from(["capture-proxy", "--destination-uri", "https://h"]).is_err(),
            "--listen-port is required (CaptureProxy.java:145)"
        );
    }

    #[test]
    fn a_switch_takes_a_bare_form_and_an_explicit_form() {
        assert!(parse(&["--no-capture"]).no_capture);
        assert!(parse(&["--no-capture=true"]).no_capture);
        assert!(!parse(&["--no-capture=false"]).no_capture);
        // The explicit form is what lets a JSON `false` override an environment variable.
        assert!(!parse(&[]).no_capture);
    }

    #[test]
    fn header_pairs_are_collected_two_at_a_time() {
        let args = parse(&[
            "--set-header",
            "X-A",
            "1",
            "--set-header",
            "X-B",
            "2",
            "--suppressCaptureForHeaderMatch",
            "user-agent",
            "healthcheck",
        ]);
        assert_eq!(args.set_header, ["X-A", "1", "X-B", "2"]);
        let pairs =
            HeaderPair::from_flat("--set-header", &args.set_header).expect("even length pairs");
        assert_eq!(
            pairs,
            vec![HeaderPair::new("X-A", "1"), HeaderPair::new("X-B", "2")]
        );
        assert_eq!(
            HeaderPair::from_flat(
                "--suppress-capture-for-header-match",
                &args.suppress_capture_for_header_match
            )
            .expect("valid"),
            vec![HeaderPair::new("user-agent", "healthcheck")]
        );
    }

    #[test]
    fn a_header_flag_cannot_be_given_an_odd_number_of_values() {
        // clap enforces it on the flag surface...
        assert!(
            CliArgs::try_parse_from([
                "capture-proxy",
                "--destination-uri",
                "https://h:9200",
                "--listen-port",
                "9200",
                "--set-header",
                "X-Only-A-Name",
            ])
            .is_err()
        );
        // ...and the pair grouping reports a real error rather than panicking, which is what Java's
        // IndexOutOfBoundsException does (spec discrepancy #18).
        let error = HeaderPair::from_flat("--set-header", &["only-one".to_owned()])
            .expect_err("odd length must be rejected");
        assert!(matches!(error, HeaderPairError::OddLength { count: 1, .. }));
    }

    #[test]
    fn an_out_of_range_port_is_rejected_rather_than_truncated() {
        assert!(
            CliArgs::try_parse_from([
                "capture-proxy",
                "--destination-uri",
                "https://h:9200",
                "--listen-port",
                "70000",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_negative_thread_count_is_a_configuration_error_not_a_runtime_crash() {
        // Spec §4.4: Java passes a negative value through to Netty, which throws at startup and
        // exits 1. The unsigned type rejects it during parsing instead.
        assert!(
            CliArgs::try_parse_from([
                "capture-proxy",
                "--destination-uri",
                "https://h:9200",
                "--listen-port",
                "9200",
                "--num-threads",
                "-1",
            ])
            .is_err()
        );
    }

    #[test]
    fn the_failure_policy_converter_accepts_only_the_two_spellings() {
        assert_eq!(
            parse(&["--capture-failure-policy", "fail-open"]).capture_failure_policy,
            CaptureFailurePolicy::FailOpen
        );
        assert!(
            CliArgs::try_parse_from([
                "capture-proxy",
                "--destination-uri",
                "https://h:9200",
                "--listen-port",
                "9200",
                "--capture-failure-policy",
                "failopen",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_password_is_never_rendered_in_debug_output() {
        let args = parse(&["--kafka-password", "hunter2"]);
        let rendered = format!("{args:?}");
        assert!(
            !rendered.contains("hunter2"),
            "the password leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains(REDACTED), "{rendered}");
        // The value itself is still reachable, deliberately and visibly.
        assert_eq!(
            args.kafka_password.as_ref().map(SecretString::expose),
            Some("hunter2")
        );
        // Display is redacted too, so a format! in a log line cannot leak it.
        assert_eq!(
            format!("{}", args.kafka_password.expect("set")),
            REDACTED.to_owned()
        );
    }

    #[test]
    fn the_startup_arg_echo_redacts_credentials_and_json_payloads() {
        // Spec discrepancy #2: Java echoes raw args twice, including --kafkaPassword.
        let redacted = redact_args([
            "--destinationUri",
            "https://source:9200",
            "--kafkaPassword",
            "hunter2",
        ]);
        assert_eq!(
            redacted,
            vec![
                "--destinationUri",
                "https://source:9200",
                "--kafkaPassword",
                REDACTED
            ]
        );
        assert_eq!(
            redact_args(["--kafka-password=hunter2"]),
            vec![format!("--kafka-password={REDACTED}")]
        );
        assert_eq!(
            redact_args(["---INLINE-JSON", "eyJsaXN0ZW5Qb3J0Ijo5MjAwfQ=="]),
            vec!["---INLINE-JSON", REDACTED]
        );
        assert_eq!(
            redact_args(["---JSON-FILE", "/config/proxy.json"]),
            vec!["---JSON-FILE", REDACTED]
        );
    }

    #[test]
    fn camel_case_conversion_matches_the_java_helper() {
        assert_eq!(
            to_camel_case("max-connection-duration-seconds"),
            "maxConnectionDurationSeconds"
        );
        assert_eq!(to_camel_case("destinationUri"), "destinationUri");
        assert_eq!(to_camel_case("enableMSKAuth"), "enableMSKAuth");
        assert_eq!(to_camel_case("no-capture"), "noCapture");
    }

    #[test]
    fn every_json_key_the_orchestration_emits_resolves() {
        // Spec §10.1, the complete emitted key set.
        for key in [
            "listenPort",
            "destinationUri",
            "insecureDestination",
            "noCapture",
            "kafkaConnection",
            "kafkaTopic",
            "kafkaListenerName",
            "kafkaAuthType",
            "kafkaSecretName",
            "kafkaUserName",
            "kafkaClientId",
            "kafkaPropertyFile",
            "enableMSKAuth",
            "numThreads",
            "maxTrafficBufferSize",
            "destinationConnectionPoolSize",
            "destinationConnectionPoolTimeout",
            "setHeader",
            "suppressCaptureForHeaderMatch",
            "suppressCaptureForMethod",
            "suppressCaptureForUriPath",
            "suppressMethodAndPath",
            "otelTraceCollectorEndpoint",
            "otelMetricsCollectorEndpoint",
            "sslCertChainFile",
            "sslKeyFile",
            "sslTrustCertFile",
            "sslTrustCertPemEnvVar",
            "requireClientAuth",
        ] {
            assert!(
                parameter_for_json_key(key).is_some(),
                "the orchestration emits {key} and it must resolve"
            );
        }
    }

    #[test]
    fn both_the_dashed_and_camel_spelling_of_every_parameter_resolve() {
        for spec in PARAMETERS {
            assert!(parameter_for_json_key(spec.flag).is_some(), "{}", spec.flag);
            let camel = to_camel_case(spec.flag);
            assert!(parameter_for_json_key(&camel).is_some(), "{camel}");
            for alias in spec.aliases {
                assert!(parameter_for_json_key(alias).is_some(), "{alias}");
                let camel = to_camel_case(alias);
                assert!(parameter_for_json_key(&camel).is_some(), "{camel}");
            }
        }
    }

    #[test]
    fn no_two_parameters_claim_the_same_json_key() {
        let mut seen = std::collections::HashMap::new();
        for spec in PARAMETERS {
            for key in spec.json_keys() {
                if let Some(other) = seen.insert(key.clone(), spec.flag) {
                    assert_eq!(other, spec.flag, "{key} is claimed by two parameters");
                }
            }
        }
    }

    #[test]
    fn clap_and_the_parameter_table_agree() {
        // The table drives inline-JSON key matching, so a parameter missing from it would be
        // silently unsettable from the production configuration path.
        let command = CliArgs::command();
        let mut clap_longs: Vec<String> = Vec::new();
        for arg in command.get_arguments() {
            if let Some(long) = arg.get_long() {
                clap_longs.push(long.to_owned());
            }
        }
        assert_eq!(
            clap_longs.len(),
            PARAMETERS.len(),
            "clap has {} long options, the table has {}",
            clap_longs.len(),
            PARAMETERS.len()
        );
        for spec in PARAMETERS {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(spec.flag))
                .unwrap_or_else(|| panic!("clap has no --{}", spec.flag));
            let aliases: Vec<&str> = arg.get_all_aliases().unwrap_or_default();
            for alias in spec.aliases {
                assert!(
                    aliases.contains(alias),
                    "--{} must keep the alias {alias}",
                    spec.flag
                );
            }
        }
    }

    #[test]
    fn every_parameter_is_environment_settable() {
        // Spec discrepancy #5: Java silently cannot set the failure policy or either header list
        // from the environment. Here the failure policy is a normal clap env parameter and the two
        // pair lists are applied from ENV_SET_HEADER / ENV_SUPPRESS_CAPTURE_FOR_HEADER_MATCH by
        // ApplicationConfig::from_cli, because clap's env support would need a value delimiter and
        // Java's NoSplitter forbids splitting.
        let pair_lists = ["set-header", "suppress-capture-for-header-match"];
        let command = CliArgs::command();
        for arg in command.get_arguments() {
            let long = arg.get_long().expect("every parameter has a long name");
            if pair_lists.contains(&long) {
                assert!(arg.get_env().is_none(), "--{long} handles env itself");
                continue;
            }
            assert!(
                arg.get_env().is_some(),
                "--{long} must be settable by environment variable"
            );
        }
    }

    #[test]
    fn the_environment_variable_names_match_the_java_derivation() {
        let command = CliArgs::command();
        let env_for = |long: &str| {
            command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(long))
                .and_then(clap::Arg::get_env)
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        // Spec §1.0's worked examples.
        assert_eq!(env_for("no-capture"), "CAPTURE_PROXY_NO_CAPTURE");
        assert_eq!(env_for("listen-port"), "CAPTURE_PROXY_LISTEN_PORT");
        assert_eq!(env_for("destination-uri"), "CAPTURE_PROXY_DESTINATION_URI");
        assert_eq!(env_for("kafka-brokers"), "CAPTURE_PROXY_KAFKA_BROKERS");
        assert_eq!(
            env_for("max-connection-duration-seconds"),
            "CAPTURE_PROXY_MAX_CONNECTION_DURATION_SECONDS"
        );
        assert_eq!(
            env_for("otel-metrics-collector-endpoint"),
            "CAPTURE_PROXY_OTEL_METRICS_COLLECTOR_ENDPOINT"
        );
        // Java splits camel case, so the field spelled UserName yields _USER_NAME.
        assert_eq!(env_for("kafka-user-name"), "CAPTURE_PROXY_KAFKA_USER_NAME");
        assert_eq!(env_for("kafka-password"), "CAPTURE_PROXY_KAFKA_PASSWORD");
    }

    #[test]
    fn iso8601_durations_parse_the_java_subset() {
        for (input, expected) in [
            ("PT30S", Duration::from_secs(30)),
            ("PT0S", Duration::ZERO),
            ("PT1M", Duration::from_mins(1)),
            ("PT1H30M", Duration::from_mins(90)),
            ("P1D", Duration::from_hours(24)),
            ("P1DT2H3M4S", Duration::from_secs(93_784)),
            ("PT0.5S", Duration::from_millis(500)),
            ("pt30s", Duration::from_secs(30)),
        ] {
            assert_eq!(
                input
                    .parse::<Iso8601Duration>()
                    .unwrap_or_else(|error| panic!("{input}: {error}"))
                    .as_duration(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn a_malformed_duration_is_rejected_instead_of_silently_accepted() {
        // Java only parses this when the pool size is nonzero (CaptureProxy.java:602-604), so a
        // typo is silently accepted whenever pooling is disabled.
        for input in [
            "30S", "P", "PT", "", "PT30X", "P1W", "-PT30S", "PTS", "PT1.S",
        ] {
            assert!(
                input.parse::<Iso8601Duration>().is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_duration_renders_as_configured() {
        let duration: Iso8601Duration = "PT30S".parse().expect("valid");
        assert_eq!(duration.to_string(), "PT30S");
        assert_eq!(duration.source(), "PT30S");
    }
}
