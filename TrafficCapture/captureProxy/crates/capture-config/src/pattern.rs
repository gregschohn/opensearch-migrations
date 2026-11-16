//! Capture-suppression patterns with **Java `Pattern.matches()` semantics**.
//!
//! # Why this type exists
//!
//! This is the single most dangerous porting trap in the whole rewrite.
//!
//! Java's `HeaderValueFilteringCapturePredicate` tests every suppression pattern with
//! `pattern.matcher(value).matches()` — a **whole-string match**. Rust's [`regex::Regex::is_match`]
//! is a **search**: it returns true if the pattern matches *anywhere* in the haystack.
//!
//! The orchestration layer emits `suppressCaptureForMethod`, `suppressCaptureForUriPath`, and
//! `suppressCaptureForMethodAndPath` with a schema default of the **empty string**. Under Java's
//! whole-string match an empty pattern matches only the empty string, so it is inert. Under a
//! search it matches *every* input — which would mark every request as
//! [`CaptureDirective::Drop`] and **silently disable all traffic capture** while the proxy
//! otherwise appeared healthy.
//!
//! Therefore every pattern in this crate is compiled anchored, and the raw [`regex::Regex`] is not
//! exposed. Construct suppression patterns only through [`SuppressionPattern`].
//!
//! # Java regex compatibility
//!
//! Java's `Pattern` and Rust's `regex` are not the same language. `regex` has no backreferences,
//! lookaround, or atomic groups, and it rejects them at compile time rather than silently
//! misbehaving. A configuration using those constructs is reported as a configuration error at
//! startup instead of being quietly reinterpreted.

use std::fmt;

use regex::Regex;

/// What the capture policy decided for one request.
///
/// Mirrors Java's `RequestCapturePredicate.CaptureDirective`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDirective {
    /// Capture the request normally.
    Capture,
    /// Do not capture the request, but still forward it to the source.
    Drop,
}

/// A suppression regex that matches the way Java's `Pattern.matches()` does.
///
/// The pattern is compiled with explicit anchors, so [`Self::matches`] is a whole-string test. An
/// empty pattern therefore matches only the empty string, exactly as in Java.
#[derive(Clone)]
pub struct SuppressionPattern {
    /// The pattern exactly as configured, retained for diagnostics.
    source: String,
    /// The anchored compiled form.
    anchored: Regex,
}

impl SuppressionPattern {
    /// Compiles `source` with Java whole-string match semantics.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::Unsupported`] when the pattern uses a Java `Pattern` construct that
    /// Rust's `regex` crate does not support (backreferences, lookaround, atomic groups), or is
    /// otherwise not a valid regular expression. Surfacing this as a configuration error is
    /// deliberate: silently reinterpreting an operator's suppression rule could either capture
    /// traffic they intended to suppress or suppress traffic they intended to capture.
    pub fn new(source: impl Into<String>) -> Result<Self, PatternError> {
        let source = source.into();
        // `(?s:...)` makes `.` match a newline, matching Java's behaviour under `Pattern.matches()`
        // where the whole input must be consumed. The non-capturing group keeps any top-level
        // alternation inside the anchors: without it, `^a|b$` would mean `(^a)|(b$)`.
        let anchored_source = format!("^(?s:{source})$");
        let anchored = Regex::new(&anchored_source).map_err(|error| PatternError::Unsupported {
            pattern: source.clone(),
            message: error.to_string(),
        })?;
        Ok(Self { source, anchored })
    }

    /// Returns true when `value` matches the pattern **in its entirety**.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        self.anchored.is_match(value)
    }

    /// The pattern as originally configured.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for SuppressionPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuppressionPattern")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// A suppression pattern could not be compiled.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PatternError {
    /// The pattern is not expressible in Rust's `regex` crate.
    #[error(
        "capture-suppression pattern {pattern:?} is not supported: {message}. \
         Rust regular expressions do not support backreferences, lookaround, or atomic groups"
    )]
    Unsupported {
        /// The offending pattern.
        pattern: String,
        /// The underlying compiler message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_pattern_matches_only_the_empty_string() {
        // This is the whole reason this type exists. `Regex::new("").is_match(x)` is true for every
        // x; orchestration defaults these patterns to "", so a search-based port would drop every
        // request and silently disable all capture.
        let pattern = SuppressionPattern::new("").expect("empty pattern compiles");
        assert!(pattern.matches(""));
        assert!(!pattern.matches("GET"));
        assert!(!pattern.matches("/"));
        assert!(!pattern.matches("POST /_bulk"));
    }

    #[test]
    fn matching_is_whole_string_not_a_search() {
        let pattern = SuppressionPattern::new("GET").expect("compiles");
        assert!(pattern.matches("GET"));
        // A search would accept all of these.
        assert!(!pattern.matches("GETS"));
        assert!(!pattern.matches("XGET"));
        assert!(!pattern.matches("GET /"));
    }

    #[test]
    fn top_level_alternation_stays_inside_the_anchors() {
        // Without the non-capturing group, `^GET|POST$` would mean `(^GET)|(POST$)` and would
        // accept "GET /foo" and "/foo POST".
        let pattern = SuppressionPattern::new("GET|POST").expect("compiles");
        assert!(pattern.matches("GET"));
        assert!(pattern.matches("POST"));
        assert!(!pattern.matches("GET /foo"));
        assert!(!pattern.matches("/foo POST"));
    }

    #[test]
    fn dot_matches_a_newline_so_the_whole_input_is_consumable() {
        let pattern = SuppressionPattern::new(".*").expect("compiles");
        assert!(pattern.matches("a\nb"));
    }

    #[test]
    fn health_check_style_patterns_behave_as_an_operator_expects() {
        let pattern = SuppressionPattern::new("/_cluster/health.*").expect("compiles");
        assert!(pattern.matches("/_cluster/health"));
        assert!(pattern.matches("/_cluster/health?pretty"));
        assert!(!pattern.matches("/foo/_cluster/health"));
    }

    #[test]
    fn the_protocol_pattern_java_hardcodes_for_http2_still_works() {
        // CaptureProxy.java:615 hardcodes .protocolPattern("HTTP/2.*").
        let pattern = SuppressionPattern::new("HTTP/2.*").expect("compiles");
        assert!(pattern.matches("HTTP/2.0"));
        assert!(pattern.matches("HTTP/2"));
        assert!(!pattern.matches("HTTP/1.1"));
    }

    #[test]
    fn unsupported_java_constructs_are_reported_not_reinterpreted() {
        // Lookahead: valid Java, unsupported in Rust's regex crate.
        let error = SuppressionPattern::new("(?!GET).*").expect_err("lookahead is unsupported");
        assert!(matches!(error, PatternError::Unsupported { .. }));
        // Backreference: likewise.
        assert!(SuppressionPattern::new(r"(a)\1").is_err());
    }

    #[test]
    fn the_configured_source_is_retained_for_diagnostics() {
        let pattern = SuppressionPattern::new("^/foo$").expect("compiles");
        assert_eq!(pattern.source(), "^/foo$");
        // Operator-supplied anchors are harmless: the extra anchors are redundant, not conflicting.
        assert!(pattern.matches("/foo"));
    }
}
