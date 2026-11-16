//! The logging facade (PROXY-14, PROXY-15, PROXY-16).
//!
//! One `tracing-subscriber` stack writing to **stderr**, with a usable default level, a UTC
//! microsecond timestamp, the severity, the target, the thread name, and the enclosing span's
//! correlation fields. When a trace collector is configured, a `tracing-opentelemetry` layer on the
//! same stack turns the proxy's spans into OTLP traces, so one `tracing` call site feeds both the
//! operator's log stream and the trace backend.
//!
//! # What is preserved from `log4j2.properties`
//!
//! - **Everything goes to stderr**, not stdout (line 7). The container log collectors are configured
//!   for that stream.
//! - **The root level defaults to `info`** and is overridden by [`ROOT_LOG_LEVEL_ENV`] (line 12).
//! - **Timestamps are UTC with microsecond precision** (`%d{DEFAULT_MICROS}{UTC}`).
//! - The thread name and the component target are on every line (`[%t] %c{1}`).
//!
//! Log4j2 **property-file compatibility is explicitly not a requirement**, and neither is the
//! rendered layout. `logging.properties` configured `java.util.logging`, was referenced by nothing,
//! and has no successor here.
//!
//! # What is deliberately better
//!
//! - **Per-component levels.** `log4j2.properties` declares no named loggers at all, so raising one
//!   component to debug means raising everything — on a proxy at full traffic that is not a usable
//!   diagnostic. [`COMPONENT_LOG_LEVELS_ENV`] takes `target=level` directives, so
//!   `MIGRATIONS_COMPONENT_LOG_LEVELS=librdkafka=debug` raises exactly one component (PROXY-15).
//! - **A JSON layout**, selected by `CONSOLE_LOG_FORMAT=json`. The orchestration already sets that
//!   variable for sibling containers while the Java proxy ignores it, so a mixed-format log stream
//!   was the status quo.
//! - **Native-dependency diagnostics are in the same stream** (PROXY-16). `librdkafka` and `rustls`
//!   emit through the `log` crate; installing the `log` bridge routes them into this subscriber
//!   under the targets in [`NATIVE_DIAGNOSTIC_TARGETS`], so they are filterable and formatted like
//!   everything else instead of escaping to a destination nobody collects.
//!
//! # A misconfigured level can neither silence the proxy nor stop it
//!
//! `EnvFilter` interprets a bare word it does not recognise as a *target* name, so
//! `MIGRATIONS_ROOT_LOG_LEVEL=warning` would be read as "the target `warning` at trace level" and
//! leave every other event filtered out — a silently mute proxy, which is the worst possible
//! outcome for an observability knob. [`LoggingConfig::from_values`] therefore parses the root and
//! span levels strictly, falls back to the defaults, and records the rejection in
//! [`LoggingConfig::rejected_env_values`]; [`install`] re-emits those rejections as warnings once
//! the subscriber is live. Unparseable component directives fall back the same way, to the root
//! level alone. The proxy always logs, always starts, and always says what it ignored.
//!
//! # Log pressure cannot stall traffic
//!
//! Java made every logger asynchronous through an LMAX Disruptor whose default queue-full policy is
//! to **block the calling thread**, so sustained log pressure could stall a Netty event loop. This
//! stack writes synchronously to stderr and keeps the volume bounded at the source instead: event
//! names are bounded, identities live in span fields, and repetitive pre-authentication diagnostics
//! go through [`crate::log_deduper`].

use std::fmt::Write as _;

use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::time::SystemTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::contract::SCOPE_NAME;

/// Environment variable setting the root log level (`log4j2.properties:12`).
pub const ROOT_LOG_LEVEL_ENV: &str = "MIGRATIONS_ROOT_LOG_LEVEL";

/// Environment variable adding per-component level directives.
///
/// A comma-separated list of `target=level` pairs, for example
/// `librdkafka=debug,capture_kafka::routing_state=trace`. Applied after the root level.
pub const COMPONENT_LOG_LEVELS_ENV: &str = "MIGRATIONS_COMPONENT_LOG_LEVELS";

/// Environment variable selecting the console layout: `json`, or anything else for text.
pub const CONSOLE_LOG_FORMAT_ENV: &str = "CONSOLE_LOG_FORMAT";

/// Environment variable setting the minimum span level handed to the trace exporter.
///
/// Separate from the console level on purpose. Per-request phase spans are created at `debug`, so
/// tying span export to the console level would mean an operator could not get traces without also
/// getting a debug-level log flood.
pub const SPAN_EXPORT_LEVEL_ENV: &str = "MIGRATIONS_SPAN_EXPORT_LEVEL";

/// The root level when [`ROOT_LOG_LEVEL_ENV`] is unset (`log4j2.properties:12`).
pub const DEFAULT_ROOT_LEVEL: LevelFilter = LevelFilter::INFO;

/// The span-export level when [`SPAN_EXPORT_LEVEL_ENV`] is unset.
pub const DEFAULT_SPAN_EXPORT_LEVEL: LevelFilter = LevelFilter::DEBUG;

/// The event name emitted when a logging environment value was rejected.
pub const CAPTURE_LOG_CONFIG_REJECTED: &str = "capture_log_config_rejected";

/// `tracing` targets that native dependencies log through (PROXY-16).
///
/// `rdkafka` logs librdkafka's callbacks to the `log` crate under the target `librdkafka`
/// (`rdkafka-0.39.0/src/client.rs:68-88`), and `rustls` logs under `rustls::*`. Both arrive in this
/// subscriber once the `log` bridge is installed, which [`install`] does.
pub const NATIVE_DIAGNOSTIC_TARGETS: &[&str] = &["librdkafka", "rustls"];

/// The console layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleFormat {
    /// One human-readable line per event.
    #[default]
    Text,
    /// One JSON object per event.
    Json,
}

impl ConsoleFormat {
    /// Parses the [`CONSOLE_LOG_FORMAT_ENV`] value.
    ///
    /// Anything other than `json` (case-insensitively) selects text, matching how the orchestration
    /// uses the variable: it sets `json` or leaves it unset.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("json") {
            Self::Json
        } else {
            Self::Text
        }
    }
}

/// An environment value that did not parse and was replaced by a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEnvValue {
    /// The environment variable name.
    pub variable: &'static str,
    /// The value that was rejected.
    pub value: String,
    /// What was used instead.
    pub used: String,
}

/// How the logging stack should be configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    /// The root level. Strictly typed, so a typo cannot become a target directive.
    pub root_level: LevelFilter,
    /// Extra `target=level` directives, applied after the root level.
    pub component_directives: Option<String>,
    /// The console layout.
    pub format: ConsoleFormat,
    /// The minimum span level handed to the trace exporter.
    pub span_export_level: LevelFilter,
    /// Environment values that were rejected, warned about by [`install`].
    pub rejected_env_values: Vec<RejectedEnvValue>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            root_level: DEFAULT_ROOT_LEVEL,
            component_directives: None,
            format: ConsoleFormat::Text,
            span_export_level: DEFAULT_SPAN_EXPORT_LEVEL,
            rejected_env_values: Vec::new(),
        }
    }
}

impl LoggingConfig {
    /// Builds the configuration from already-read environment values.
    ///
    /// Separated from [`Self::from_env`] so it is testable without mutating the process
    /// environment. Blank values are treated as unset.
    // DEVIATION: three changes from log4j2.properties, all of which it could not express.
    //  1. Per-component levels. The Java file declares no named loggers, so raising one component
    //     to debug meant raising everything, which is unusable on a proxy under load (PROXY-15).
    //  2. A JSON layout gated on CONSOLE_LOG_FORMAT=json. The orchestration already sets that
    //     variable for sibling containers while the Java proxy ignores it.
    //  3. A level that does not parse falls back to the default and is warned about, rather than
    //     being passed through. EnvFilter reads an unrecognised bare word as a *target* directive,
    //     so passing `MIGRATIONS_ROOT_LOG_LEVEL=warning` through would leave the proxy mute.
    #[must_use]
    pub fn from_values(
        root_level: Option<&str>,
        component_directives: Option<&str>,
        console_format: Option<&str>,
        span_export_level: Option<&str>,
    ) -> Self {
        let mut rejected_env_values = Vec::new();
        let root_level = parse_level(
            ROOT_LOG_LEVEL_ENV,
            root_level,
            DEFAULT_ROOT_LEVEL,
            &mut rejected_env_values,
        );
        let span_export_level = parse_level(
            SPAN_EXPORT_LEVEL_ENV,
            span_export_level,
            DEFAULT_SPAN_EXPORT_LEVEL,
            &mut rejected_env_values,
        );
        Self {
            root_level,
            component_directives: non_blank(component_directives),
            format: console_format.map_or(ConsoleFormat::Text, ConsoleFormat::parse),
            span_export_level,
            rejected_env_values,
        }
    }

    /// Reads the configuration from the environment.
    #[must_use]
    pub fn from_env() -> Self {
        let root = std::env::var(ROOT_LOG_LEVEL_ENV).ok();
        let components = std::env::var(COMPONENT_LOG_LEVELS_ENV).ok();
        let format = std::env::var(CONSOLE_LOG_FORMAT_ENV).ok();
        let span = std::env::var(SPAN_EXPORT_LEVEL_ENV).ok();
        Self::from_values(
            root.as_deref(),
            components.as_deref(),
            format.as_deref(),
            span.as_deref(),
        )
    }

    /// The filter directive string this configuration produces.
    ///
    /// Exposed so a test can assert the composition without installing a global subscriber.
    #[must_use]
    pub fn filter_directives(&self) -> String {
        let mut directives = self.root_level.to_string();
        if let Some(components) = self.component_directives.as_deref() {
            let _ = write!(directives, ",{components}");
        }
        directives
    }
}

fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_level(
    variable: &'static str,
    value: Option<&str>,
    default: LevelFilter,
    rejected: &mut Vec<RejectedEnvValue>,
) -> LevelFilter {
    let Some(value) = non_blank(value) else {
        return default;
    };
    if let Ok(level) = value.parse::<LevelFilter>() {
        level
    } else {
        rejected.push(RejectedEnvValue {
            variable,
            value,
            used: default.to_string(),
        });
        default
    }
}

/// The logging stack could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A global subscriber was already installed.
    #[error("a global tracing subscriber is already installed: {0}")]
    AlreadyInstalled(String),
}

/// Installs the global logging stack.
///
/// Also installs the `log` crate bridge, which is what puts `librdkafka` and `rustls` diagnostics
/// into the same stream (PROXY-16).
///
/// Call once, as early as possible: events emitted before this runs are discarded, including the
/// argument-parsing diagnostics.
///
/// Unparseable component directives are dropped in favour of the root level alone rather than
/// failing, and every rejected environment value is warned about immediately after the subscriber
/// is live.
///
/// # Errors
///
/// Returns [`InstallError::AlreadyInstalled`] when a global subscriber already exists.
///
/// # Panics
///
/// Unreachable. The fallback filter is built from a [`LevelFilter`], whose `Display` output is
/// always a valid single directive, so the only `expect` here cannot fire. A level that did not
/// parse was already replaced in [`LoggingConfig::from_values`].
pub fn install(
    config: &LoggingConfig,
    tracer_provider: Option<&SdkTracerProvider>,
) -> Result<(), InstallError> {
    let directives = config.filter_directives();
    let (filter, rejected_directives) = if let Ok(filter) = EnvFilter::try_new(&directives) {
        (filter, None)
    } else {
        let fallback = config.root_level.to_string();
        let filter = EnvFilter::try_new(&fallback)
            .expect("a LevelFilter always renders as a valid directive");
        (
            filter,
            Some(RejectedEnvValue {
                variable: COMPONENT_LOG_LEVELS_ENV,
                value: config
                    .component_directives
                    .clone()
                    .unwrap_or_else(|| directives.clone()),
                used: fallback,
            }),
        )
    };

    let console: Box<dyn Layer<Registry> + Send + Sync> = match config.format {
        ConsoleFormat::Text => Box::new(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_level(true)
                .with_target(true)
                .with_thread_names(true)
                .with_timer(SystemTime)
                .with_filter(filter),
        ),
        ConsoleFormat::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(std::io::stderr)
                .with_level(true)
                .with_target(true)
                .with_thread_names(true)
                .with_timer(SystemTime)
                .with_filter(filter),
        ),
    };

    let otel = tracer_provider.map(|provider| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer(SCOPE_NAME))
            .with_filter(config.span_export_level)
    });

    tracing_subscriber::registry()
        .with(console)
        .with(otel)
        .try_init()
        .map_err(|error| InstallError::AlreadyInstalled(error.to_string()))?;

    for rejected in config
        .rejected_env_values
        .iter()
        .chain(rejected_directives.iter())
    {
        crate::capture_event!(
            warn,
            CAPTURE_LOG_CONFIG_REJECTED,
            variable = rejected.variable,
            value = rejected.value,
            using = rejected.used,
        );
    }
    Ok(())
}

/// Routes one native-dependency diagnostic through this facade.
///
/// `librdkafka` and `rustls` reach the subscriber on their own through the `log` bridge. This
/// function is for a native callback that has no `log` path of its own — a custom `rdkafka`
/// `ClientContext`, for instance — so such a diagnostic still lands in the operator's one log
/// stream with a filterable component field.
pub fn native_diagnostic(component: &str, level: Level, message: &str) {
    match level {
        Level::ERROR => tracing::error!(component, "{message}"),
        Level::WARN => tracing::warn!(component, "{message}"),
        Level::INFO => tracing::info!(component, "{message}"),
        Level::DEBUG => tracing::debug!(component, "{message}"),
        Level::TRACE => tracing::trace!(component, "{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_root_level_is_info() {
        assert_eq!(LoggingConfig::default().filter_directives(), "info");
        assert_eq!(DEFAULT_ROOT_LEVEL, LevelFilter::INFO);
    }

    #[test]
    fn an_unset_environment_yields_the_java_defaults() {
        let config = LoggingConfig::from_values(None, None, None, None);
        assert_eq!(config.root_level, LevelFilter::INFO);
        assert_eq!(config.format, ConsoleFormat::Text);
        assert_eq!(config.span_export_level, LevelFilter::DEBUG);
        assert!(config.rejected_env_values.is_empty());
        assert_eq!(config.filter_directives(), "info");
    }

    #[test]
    fn the_root_level_is_honoured() {
        let config = LoggingConfig::from_values(Some("debug"), None, None, None);
        assert_eq!(config.root_level, LevelFilter::DEBUG);
        assert_eq!(config.filter_directives(), "debug");
    }

    #[test]
    fn component_directives_are_appended_after_the_root_level() {
        let config = LoggingConfig::from_values(
            Some("warn"),
            Some("librdkafka=debug,capture_kafka=trace"),
            None,
            None,
        );
        assert_eq!(
            config.filter_directives(),
            "warn,librdkafka=debug,capture_kafka=trace"
        );
        EnvFilter::try_new(config.filter_directives()).expect("directives parse");
    }

    /// A bare unrecognised word is a valid `EnvFilter` *target* directive, so passing the raw value
    /// through would leave the proxy mute. It must be rejected and replaced.
    #[test]
    fn a_root_level_typo_falls_back_to_info_instead_of_silencing_the_proxy() {
        let config = LoggingConfig::from_values(Some("warning"), None, None, None);
        assert_eq!(config.root_level, LevelFilter::INFO);
        assert_eq!(config.filter_directives(), "info");
        assert_eq!(
            config.rejected_env_values,
            vec![RejectedEnvValue {
                variable: ROOT_LOG_LEVEL_ENV,
                value: "warning".to_owned(),
                used: "info".to_owned(),
            }]
        );
    }

    #[test]
    fn a_span_export_level_typo_falls_back_to_debug() {
        let config = LoggingConfig::from_values(None, None, None, Some("verbose"));
        assert_eq!(config.span_export_level, LevelFilter::DEBUG);
        assert_eq!(config.rejected_env_values.len(), 1);
        assert_eq!(
            config.rejected_env_values[0].variable,
            SPAN_EXPORT_LEVEL_ENV
        );
    }

    #[test]
    fn blank_values_are_treated_as_unset() {
        let config = LoggingConfig::from_values(Some("   "), Some("  "), Some(""), Some(" "));
        assert_eq!(config.root_level, LevelFilter::INFO);
        assert_eq!(config.component_directives, None);
        assert_eq!(config.format, ConsoleFormat::Text);
        assert!(config.rejected_env_values.is_empty());
        assert_eq!(config.filter_directives(), "info");
    }

    #[test]
    fn the_console_format_is_json_only_on_an_explicit_request() {
        assert_eq!(ConsoleFormat::parse("json"), ConsoleFormat::Json);
        assert_eq!(ConsoleFormat::parse("JSON"), ConsoleFormat::Json);
        assert_eq!(ConsoleFormat::parse(" json "), ConsoleFormat::Json);
        assert_eq!(ConsoleFormat::parse("text"), ConsoleFormat::Text);
        assert_eq!(ConsoleFormat::parse(""), ConsoleFormat::Text);
        assert_eq!(ConsoleFormat::default(), ConsoleFormat::Text);
        assert_eq!(
            LoggingConfig::from_values(None, None, Some("json"), None).format,
            ConsoleFormat::Json
        );
    }

    #[test]
    fn every_native_target_is_filterable_by_name() {
        for target in NATIVE_DIAGNOSTIC_TARGETS {
            let config =
                LoggingConfig::from_values(None, Some(&format!("{target}=debug")), None, None);
            EnvFilter::try_new(config.filter_directives())
                .unwrap_or_else(|_| panic!("{target} must be filterable"));
        }
    }

    /// A component directive naming a level that does not exist makes the whole composition
    /// unparseable. `install` must fall back to the root level rather than refuse to start or run
    /// without any filter.
    #[test]
    fn broken_component_directives_leave_the_root_level_intact() {
        let config =
            LoggingConfig::from_values(Some("warn"), Some("librdkafka=notalevel"), None, None);
        assert_eq!(config.root_level, LevelFilter::WARN);
        assert!(
            EnvFilter::try_new(config.filter_directives()).is_err(),
            "the composition must be the thing that fails"
        );
        assert!(
            EnvFilter::try_new(config.root_level.to_string()).is_ok(),
            "the root level alone must always remain a usable fallback"
        );
    }

    #[test]
    fn levels_are_accepted_case_insensitively() {
        for value in ["INFO", "Info", "info"] {
            let config = LoggingConfig::from_values(Some(value), None, None, None);
            assert_eq!(config.root_level, LevelFilter::INFO, "{value}");
            assert!(config.rejected_env_values.is_empty(), "{value}");
        }
    }
}
