//! Turning validated configuration into the data plane's policy objects.
//!
//! Two translations live here, both small and both with a sharp edge:
//!
//! * the capture-suppression predicate, which is the one place a mistake silently disables all
//!   capture;
//! * the post-capture header rewrite, which is the one place a mistake changes the bytes the
//!   destination sees.

use capture_config::{ApplicationConfig, SuppressionRules};
use capture_http_scan::rewrite::{RewriteConfig, RewriteError};
use capture_http_scan::suppression::{SuppressionPolicy, SuppressionPolicyBuilder};
use capture_transport::ConnectionConfig;
use capture_transport::connection::DEFAULT_READ_BUFFER_BYTES;

/// Builds the capture-suppression predicate.
///
/// Note what [`SuppressionPolicyBuilder::new`] already does: it suppresses `HTTP/2.*`, which Java
/// hardcodes at `CaptureProxy.java:611-617` with no way to configure it. An h2c prior-knowledge
/// preface is therefore forwarded to the destination but **not** captured — suppressed, not
/// rejected — and that is parity, not an oversight.
///
/// AWKWARD: `capture_config::SuppressionRules` hands over
/// [`capture_config::SuppressionPattern`]s that are already compiled, and
/// `SuppressionPolicyBuilder` accepts only `&str`, so each pattern is compiled a second time from
/// [`capture_config::SuppressionPattern::source`]. It is start-up-only work and the recompilation
/// cannot change the semantics — both sides go through `SuppressionPattern::new`, so both get
/// Java's whole-string `Pattern.matches()` behaviour rather than a search. A builder taking
/// compiled patterns would remove the step.
///
/// # Errors
///
/// [`capture_config::PatternError`] if a pattern that compiled in the configuration layer does not
/// compile here, which would mean the two layers disagree about the pattern dialect.
pub fn suppression_policy(
    config: &ApplicationConfig,
) -> Result<SuppressionPolicy, capture_config::PatternError> {
    policy_from_rules(config.suppression())
}

fn policy_from_rules(
    rules: &SuppressionRules,
) -> Result<SuppressionPolicy, capture_config::PatternError> {
    let mut builder = SuppressionPolicyBuilder::new();
    if let Some(pattern) = &rules.method {
        builder = builder.method(pattern.source())?;
    }
    if let Some(pattern) = &rules.uri_path {
        builder = builder.path(pattern.source())?;
    }
    if let Some(pattern) = &rules.method_and_path {
        builder = builder.method_and_path(pattern.source())?;
    }
    for header in &rules.headers {
        builder = builder.header(&header.name, header.pattern.source())?;
    }
    Ok(builder.build())
}

/// Builds the per-connection transport configuration.
///
/// `gate_critical_mutations` is `true` unconditionally, matching production Java, which always
/// installs the block predicate (`PCI:190`). In `--no-capture` mode the gate still runs, and
/// [`crate::capture_sink::NoCaptureSink`]'s implementation of it is a future that resolves
/// immediately without yielding — so parity costs nothing measurable, and the flag keeps one
/// meaning instead of two.
///
/// # Errors
///
/// [`RewriteError`] when a `--set-header` name or value is not a legal HTTP header field.
pub fn connection_config(config: &ApplicationConfig) -> Result<ConnectionConfig, RewriteError> {
    let mut rewrite = RewriteConfig::new();
    for pair in config.set_header() {
        // `set_header` is remove-then-add, which is what makes a configured header authoritative
        // over whatever the client sent.
        rewrite = rewrite.set_header(&pair.name, &pair.value)?;
    }
    Ok(ConnectionConfig {
        limits: config.limits(),
        strictness: config.strictness(),
        rewrite,
        read_buffer_bytes: DEFAULT_READ_BUFFER_BYTES,
        gate_critical_mutations: true,
    })
}

#[cfg(test)]
mod tests {
    use capture_config::ProcessEnv;
    use capture_http_scan::selected_headers::{SelectedHeaderNames, SelectedHeaders};
    use capture_http_scan::{CaptureDirective, CapturePredicate, HttpVersion, RequestHead};

    use super::*;

    fn config(extra: &[&str]) -> ApplicationConfig {
        let mut args = vec![
            "--destination-uri",
            "http://upstream.test",
            "--listen-port",
            "9200",
            "--no-capture",
        ];
        args.extend_from_slice(extra);
        ApplicationConfig::from_args(&args, &ProcessEnv).expect("configuration is valid")
    }

    fn evaluate(
        policy: &SuppressionPolicy,
        method: &[u8],
        target: &[u8],
        version: &[u8],
    ) -> CaptureDirective {
        let names = SelectedHeaderNames::new(policy.selected_header_names());
        let headers = SelectedHeaders::collect(&names, &[]);
        policy.evaluate(&RequestHead::new(
            method,
            target,
            HttpVersion::parse(version).expect("a valid version"),
            &headers,
        ))
    }

    fn directive(policy: &SuppressionPolicy, method: &str, target: &str) -> CaptureDirective {
        evaluate(policy, method.as_bytes(), target.as_bytes(), b"HTTP/1.1")
    }

    #[test]
    fn the_default_policy_captures_everything_except_http_2() {
        let policy = suppression_policy(&config(&[])).expect("policy compiles");
        assert_eq!(
            directive(&policy, "GET", "/_cluster/health"),
            CaptureDirective::Capture
        );

        assert_ne!(
            evaluate(&policy, b"PRI", b"*", b"HTTP/2.0"),
            CaptureDirective::Capture,
            "CaptureProxy.java:611-617 hardcodes HTTP/2 suppression"
        );
    }

    #[test]
    fn an_empty_suppression_pattern_does_not_suppress_everything() {
        // The trap this whole layer exists to avoid: orchestration defaults these patterns to the
        // empty string. A search-based match would match every request and silently disable all
        // capture. Whole-string matching means the empty pattern matches only the empty string.
        let policy = suppression_policy(&config(&["--suppress-capture-for-method", ""]))
            .expect("policy compiles");
        assert_eq!(
            directive(&policy, "GET", "/"),
            CaptureDirective::Capture,
            "an empty pattern must not suppress a non-empty method"
        );
    }

    #[test]
    fn a_configured_method_pattern_suppresses_only_that_method() {
        let policy = suppression_policy(&config(&["--suppress-capture-for-method", "GET"]))
            .expect("policy compiles");
        assert_ne!(directive(&policy, "GET", "/"), CaptureDirective::Capture);
        assert_eq!(directive(&policy, "POST", "/"), CaptureDirective::Capture);
    }

    #[test]
    fn a_configured_path_pattern_is_matched_whole() {
        let policy = suppression_policy(&config(&[
            "--suppress-capture-for-uri-path",
            "/_cluster/health",
        ]))
        .expect("policy compiles");
        assert_ne!(
            directive(&policy, "GET", "/_cluster/health"),
            CaptureDirective::Capture
        );
        assert_eq!(
            directive(&policy, "GET", "/_cluster/health/extra"),
            CaptureDirective::Capture,
            "Pattern.matches is a whole-string test, not a prefix search"
        );
    }

    #[test]
    fn a_header_suppression_rule_retains_the_header_it_matches_on() {
        // The Java trap: the preserved-header list and the value patterns are wired separately
        // there, so a value pattern can be configured on a header the scanner threw away.
        let policy = suppression_policy(&config(&[
            "--suppress-capture-for-header-match",
            "User-Agent",
            "kube-probe/.*",
        ]))
        .expect("policy compiles");
        assert!(
            policy
                .selected_header_names()
                .iter()
                .any(|name| &**name == "user-agent"),
            "the matched header must be retained by the scanner"
        );
    }

    #[test]
    fn the_connection_config_carries_the_configured_limits_and_strictness() {
        let config = config(&["--max-connection-duration-seconds", "123"]);
        let connection = connection_config(&config).expect("rewrite compiles");
        assert_eq!(connection.limits, config.limits());
        assert_eq!(connection.strictness, config.strictness());
        assert_eq!(connection.read_buffer_bytes, DEFAULT_READ_BUFFER_BYTES);
        assert!(
            connection.gate_critical_mutations,
            "production always installs the block predicate (PCI:190)"
        );
    }

    #[test]
    fn set_header_becomes_a_rewrite_and_no_set_header_becomes_a_noop() {
        let bare = connection_config(&config(&[])).expect("rewrite compiles");
        assert!(
            bare.rewrite.is_empty(),
            "no --set-header must rewrite nothing"
        );

        let rewritten = connection_config(&config(&["--set-header", "Host", "upstream.test"]))
            .expect("rewrite compiles");
        assert!(!rewritten.rewrite.is_empty());
        assert_eq!(rewritten.rewrite.addition_count(), 1);
        assert_eq!(
            rewritten.rewrite.removals().len(),
            1,
            "set-header is remove-then-add, so a client's own Host cannot survive alongside it"
        );
    }
}
