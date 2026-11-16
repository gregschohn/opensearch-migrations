//! The three-way capture / ignore / drop decision (traceability row HTTP-04).
//!
//! # Three outcomes, not two
//!
//! Java's ordering — decode first, capture second (`LHH:476-483`) — produces three distinct
//! behaviours, and the difference between the last two is externally visible in the capture stream:
//!
//! 1. **Capture.** The read is captured byte-exactly.
//! 2. **Suppressed before anything was committed.** The whole head arrived in the read that first
//!    triggered the predicate, so the decision was known before any byte was captured. The request
//!    **leaves no trace at all** in the capture stream, which is what
//!    `proxyCaptureProtocol.md:484-486` requires.
//! 3. **Suppressed after a captured prefix.** The head was split across reads, so earlier reads
//!    were already captured. The capture stream gets a `requestDropped` observation, and the
//!    triggering read's own bytes are **not** captured — the captured prefix stops at the previous
//!    read. Pinned by `testThatHealthCheckCaptureCanBeSuppressed(singleBytes=true)`.
//!
//! In all suppressed cases there is no end-of-message indicator, and therefore no
//! `requestForwarded` either (`RequestForwardedCaptureTest:83-98`).
//!
//! # Matching semantics
//!
//! Every pattern is a [`SuppressionPattern`], i.e. a Java `Pattern.matches()` whole-string test with
//! no flags. Header **names** match ASCII case-insensitively; header **values** match
//! case-sensitively and whole-string; a multi-valued header is tested on its **first value only**.
//! See [`capture_config::pattern`] for why a search-based match would be a disaster here.

use capture_config::{CaptureDirective, PatternError, SuppressionPattern};

use crate::{CapturePredicate, RequestHead};

/// The protocol pattern Java hardcodes to keep HTTP/2 out of the capture stream.
///
/// `CaptureProxy.java:611-617` always builds the predicate with this value and offers no way to
/// configure it. It matches `HTTP/2.0`, so an h2c prior-knowledge preface is **forwarded to the
/// source but not captured** — suppressed, not rejected. TLS clients never reach it because no
/// ALPN is configured anywhere, which is what keeps `h2` unreachable over TLS.
pub const HTTP_2_PROTOCOL_PATTERN: &str = "HTTP/2.*";

/// Java's `CaptureState`: the per-request capture mode (`LHH:63-80`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureState {
    /// Capture the request and its response.
    #[default]
    Capture,
    /// The request is suppressed; its response has not been reached yet.
    IgnoreRequest,
    /// The request was suppressed and its response is being skipped too.
    IgnoreResponse,
}

/// What applying a [`CaptureDirective`] means for the capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressionTransition {
    /// Nothing changed; keep capturing.
    Unchanged,
    /// Suppressed before any byte of this request was committed. Emit **nothing**: no observation
    /// of any kind, and do not capture the read that carried the head.
    SuppressedBeforeCommit,
    /// Suppressed after a prefix had already been captured. Emit one `requestDropped` observation,
    /// and do not capture the read that carried the rest of the head.
    SuppressedAfterCommittedPrefix,
}

/// Per-connection suppression state.
///
/// Suppression is scoped to exactly one request/response exchange: the state returns to
/// [`CaptureState::Capture`] on the next inbound read (`LHH:124-130`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestSuppression {
    state: CaptureState,
    live_read_observations: bool,
}

impl RequestSuppression {
    /// A fresh state that captures.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> CaptureState {
        self.state
    }

    /// Whether a prefix of the in-flight request has already been committed to the capture stream.
    #[must_use]
    pub fn has_committed_prefix(&self) -> bool {
        self.live_read_observations
    }

    /// Called before each inbound read is processed.
    ///
    /// Promotes [`CaptureState::IgnoreResponse`] back to [`CaptureState::Capture`], which is what
    /// bounds suppression to one exchange (`LHH:124-130`).
    pub fn before_read(&mut self) {
        if self.state == CaptureState::IgnoreResponse {
            self.state = CaptureState::Capture;
        }
    }

    /// Records that a read of the in-flight request was captured.
    pub fn note_read_captured(&mut self) {
        self.live_read_observations = true;
    }

    /// Applies the predicate's decision for a head that has just completed.
    ///
    /// A [`CaptureDirective::Capture`] **installs** the capturing state rather than leaving the
    /// previous one alone, which is what `setShouldCaptureForRequest(predicate.apply(req) ==
    /// CAPTURE)` (`LHH:194-198`) does. It matters when one read carries the end of a suppressed
    /// request and the head of a captured one: without it, the captured request would inherit the
    /// suppressed request's state and silently vanish from the capture stream.
    pub fn apply_directive(&mut self, directive: CaptureDirective) -> SuppressionTransition {
        match directive {
            CaptureDirective::Capture => {
                self.state = CaptureState::Capture;
                SuppressionTransition::Unchanged
            }
            CaptureDirective::Drop => {
                self.state = CaptureState::IgnoreRequest;
                if self.live_read_observations {
                    self.live_read_observations = false;
                    SuppressionTransition::SuppressedAfterCommittedPrefix
                } else {
                    SuppressionTransition::SuppressedBeforeCommit
                }
            }
        }
    }

    /// Whether reads of the in-flight request are captured.
    #[must_use]
    pub fn should_capture_request(&self) -> bool {
        self.state == CaptureState::Capture
    }

    /// Whether the response to the in-flight request is captured.
    ///
    /// Telemetry is **not** suppressed with capture: Java counts every byte handed to the client
    /// against the response span regardless (`LHH:905-911`).
    #[must_use]
    pub fn should_capture_response(&self) -> bool {
        self.state == CaptureState::Capture
    }

    /// Called when a request's end of message is reached.
    ///
    /// `advanceStateModelIntoResponseGather` (`LHH:625`, `LHH:75-79`) promotes any non-capturing
    /// state to [`CaptureState::IgnoreResponse`], which suppresses the response as well.
    pub fn finish_request(&mut self) {
        self.live_read_observations = false;
        if self.state == CaptureState::IgnoreRequest {
            self.state = CaptureState::IgnoreResponse;
        }
    }
}

/// Java's `HeaderValueFilteringCapturePredicate`, with Java's exact matching semantics.
#[derive(Debug, Clone, Default)]
pub struct SuppressionPolicy {
    method: Option<SuppressionPattern>,
    path: Option<SuppressionPattern>,
    protocol: Option<SuppressionPattern>,
    method_and_path: Option<SuppressionPattern>,
    /// Lowercased name paired with its value pattern, in configuration order.
    headers: Vec<(Box<str>, SuppressionPattern)>,
    /// Exactly the header names, for [`CapturePredicate::selected_header_names`].
    header_names: Vec<Box<str>>,
}

impl SuppressionPolicy {
    /// A builder that already suppresses HTTP/2, like Java's always-on protocol pattern.
    #[must_use]
    pub fn builder() -> SuppressionPolicyBuilder {
        SuppressionPolicyBuilder::new()
    }

    /// A policy that suppresses nothing at all, not even HTTP/2.
    ///
    /// Only reachable deliberately; Java has no such configuration.
    #[must_use]
    pub fn permit_everything() -> Self {
        Self::default()
    }

    /// The configured header-name/value pairs.
    #[must_use]
    pub fn header_patterns(&self) -> &[(Box<str>, SuppressionPattern)] {
        &self.headers
    }
}

impl CapturePredicate for SuppressionPolicy {
    fn selected_header_names(&self) -> &[Box<str>] {
        &self.header_names
    }

    /// Returns [`CaptureDirective::Drop`] if **any** test passes, short-circuiting in Java's order
    /// (`HeaderValueFilteringCapturePredicate.java:39-63`).
    fn evaluate(&self, head: &RequestHead<'_>) -> CaptureDirective {
        if let Some(pattern) = &self.method
            && head
                .method_str()
                .is_some_and(|method| pattern.matches(method))
        {
            return CaptureDirective::Drop;
        }
        if let Some(pattern) = &self.path
            && head
                .target_str()
                .is_some_and(|target| pattern.matches(target))
        {
            // The raw request-target, including any query string — not a decoded path.
            return CaptureDirective::Drop;
        }
        if let Some(pattern) = &self.protocol
            && pattern.matches(&head.version().text())
        {
            return CaptureDirective::Drop;
        }
        if let Some(pattern) = &self.method_and_path
            && let (Some(method), Some(target)) = (head.method_str(), head.target_str())
        {
            // Allocates only when this pattern is configured, which is rare.
            let mut combined = String::with_capacity(method.len() + 1 + target.len());
            combined.push_str(method);
            combined.push(' ');
            combined.push_str(target);
            if pattern.matches(&combined) {
                return CaptureDirective::Drop;
            }
        }
        for (name, pattern) in &self.headers {
            // First value only, and an absent header never contributes to a drop.
            let Some(value) = head.headers().value_for(name) else {
                continue;
            };
            let Ok(value) = std::str::from_utf8(value) else {
                continue;
            };
            if pattern.matches(value) {
                return CaptureDirective::Drop;
            }
        }
        CaptureDirective::Capture
    }
}

/// Builds a [`SuppressionPolicy`].
#[derive(Debug, Clone)]
pub struct SuppressionPolicyBuilder {
    policy: SuppressionPolicy,
    suppress_http_2: bool,
}

impl Default for SuppressionPolicyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SuppressionPolicyBuilder {
    /// A builder that suppresses HTTP/2 and nothing else.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: SuppressionPolicy::default(),
            suppress_http_2: true,
        }
    }

    /// Suppresses requests whose method matches `pattern`.
    ///
    /// # Errors
    ///
    /// [`PatternError::Unsupported`] when the pattern is not expressible in Rust's `regex` crate.
    pub fn method(mut self, pattern: &str) -> Result<Self, PatternError> {
        self.policy.method = Some(SuppressionPattern::new(pattern)?);
        Ok(self)
    }

    /// Suppresses requests whose raw request-target matches `pattern`.
    ///
    /// # Errors
    ///
    /// As [`SuppressionPolicyBuilder::method`].
    pub fn path(mut self, pattern: &str) -> Result<Self, PatternError> {
        self.policy.path = Some(SuppressionPattern::new(pattern)?);
        Ok(self)
    }

    /// Suppresses requests whose `method SP request-target` matches `pattern`.
    ///
    /// # Errors
    ///
    /// As [`SuppressionPolicyBuilder::method`].
    pub fn method_and_path(mut self, pattern: &str) -> Result<Self, PatternError> {
        self.policy.method_and_path = Some(SuppressionPattern::new(pattern)?);
        Ok(self)
    }

    /// Replaces the protocol pattern, which otherwise suppresses HTTP/2.
    ///
    /// # Errors
    ///
    /// As [`SuppressionPolicyBuilder::method`].
    pub fn protocol(mut self, pattern: &str) -> Result<Self, PatternError> {
        self.policy.protocol = Some(SuppressionPattern::new(pattern)?);
        self.suppress_http_2 = false;
        Ok(self)
    }

    /// Stops suppressing HTTP/2.
    ///
    /// Java cannot express this; it exists so a deployment that genuinely wants h2c captured can
    /// say so explicitly.
    #[must_use]
    pub fn capture_http_2(mut self) -> Self {
        self.suppress_http_2 = false;
        self
    }

    /// Suppresses requests whose first value of `name` matches `pattern`.
    ///
    /// The name is matched ASCII case-insensitively and is automatically retained by the scanner.
    ///
    /// # Errors
    ///
    /// As [`SuppressionPolicyBuilder::method`].
    pub fn header(mut self, name: &str, pattern: &str) -> Result<Self, PatternError> {
        let lowercased = name.to_ascii_lowercase().into_boxed_str();
        if !self
            .policy
            .header_names
            .iter()
            .any(|existing| **existing == *lowercased)
        {
            self.policy.header_names.push(lowercased.clone());
        }
        self.policy
            .headers
            .push((lowercased, SuppressionPattern::new(pattern)?));
        Ok(self)
    }

    /// Finishes the policy.
    ///
    /// # Panics
    ///
    /// Panics only if [`HTTP_2_PROTOCOL_PATTERN`] — a compile-time constant — stops being a valid
    /// regular expression, which no configuration can cause.
    #[must_use]
    pub fn build(mut self) -> SuppressionPolicy {
        if self.suppress_http_2 && self.policy.protocol.is_none() {
            self.policy.protocol = Some(
                SuppressionPattern::new(HTTP_2_PROTOCOL_PATTERN)
                    .expect("the hardcoded HTTP/2 pattern is a valid regular expression"),
            );
        }
        self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpVersion;
    use crate::selected_headers::{SelectedHeaderNames, SelectedHeaders};

    struct Fixture {
        names: SelectedHeaderNames,
    }

    impl Fixture {
        fn new(policy: &SuppressionPolicy) -> Self {
            Self {
                names: SelectedHeaderNames::new(policy.selected_header_names()),
            }
        }

        fn evaluate(
            &self,
            policy: &SuppressionPolicy,
            method: &[u8],
            target: &[u8],
            version: HttpVersion,
            headers: &[(&'static str, &'static [u8])],
        ) -> CaptureDirective {
            let parsed: Vec<httparse::Header<'_>> = headers
                .iter()
                .map(|(name, value)| httparse::Header { name, value })
                .collect();
            let selected = SelectedHeaders::collect(&self.names, &parsed);
            let head = RequestHead::new(method, target, version, &selected);
            policy.evaluate(&head)
        }
    }

    #[test]
    fn the_anchored_matches_matrix_from_the_java_predicate_test() {
        // HeaderValueFilteringCapturePredicateTest:16-41, with its exact patterns and request.
        type Case = (
            &'static str,
            fn(SuppressionPolicyBuilder) -> SuppressionPolicy,
            CaptureDirective,
        );
        let cases: Vec<Case> = vec![
            (
                "method GET",
                |b| b.method("GET").unwrap().build(),
                CaptureDirective::Drop,
            ),
            (
                "method POST",
                |b| b.method("POST").unwrap().build(),
                CaptureDirective::Capture,
            ),
            (
                "path /.*",
                |b| b.path("/.*").unwrap().build(),
                CaptureDirective::Drop,
            ),
            (
                "path /thing1/.*",
                |b| b.path("/thing1/.*").unwrap().build(),
                CaptureDirective::Drop,
            ),
            (
                "path /_cat",
                |b| b.path("/_cat").unwrap().build(),
                CaptureDirective::Capture,
            ),
            (
                "method+path GET /thing.*",
                |b| b.method_and_path("GET /thing.*").unwrap().build(),
                CaptureDirective::Drop,
            ),
            (
                "method+path POST /thing/.*",
                |b| b.method_and_path("POST /thing/.*").unwrap().build(),
                CaptureDirective::Capture,
            ),
        ];
        for (label, build, expected) in cases {
            let policy = build(SuppressionPolicy::builder());
            let fixture = Fixture::new(&policy);
            assert_eq!(
                fixture.evaluate(
                    &policy,
                    b"GET",
                    b"/thing1/andThenSome",
                    HttpVersion::HTTP_1_1,
                    &[]
                ),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn the_protocol_pattern_is_whole_string_so_http_1_1_is_not_dropped_by_http_1_0() {
        // `HTTP/1.0` as a regex matches only HTTP/1 + any + 0, so HTTP/1.1 fails on the final 0.
        let policy = SuppressionPolicy::builder()
            .protocol("HTTP/1.0")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(
            fixture.evaluate(&policy, b"GET", b"/", HttpVersion::HTTP_1_1, &[]),
            CaptureDirective::Capture
        );
        assert_eq!(
            fixture.evaluate(&policy, b"GET", b"/", HttpVersion::HTTP_1_0, &[]),
            CaptureDirective::Drop
        );
    }

    #[test]
    fn http_2_is_suppressed_by_default_and_http_1_is_not() {
        let policy = SuppressionPolicy::builder().build();
        let fixture = Fixture::new(&policy);
        let h2 = HttpVersion::parse(b"HTTP/2.0").expect("parses");
        // The h2c prior-knowledge preface: forwarded to the source, not captured.
        assert_eq!(
            fixture.evaluate(&policy, b"PRI", b"*", h2, &[]),
            CaptureDirective::Drop
        );
        assert_eq!(
            fixture.evaluate(&policy, b"GET", b"/", HttpVersion::HTTP_1_1, &[]),
            CaptureDirective::Capture
        );
    }

    #[test]
    fn an_empty_pattern_suppresses_nothing() {
        // The trap SuppressionPattern exists for: orchestration defaults these to "".
        let policy = SuppressionPolicy::builder()
            .method("")
            .unwrap()
            .path("")
            .unwrap()
            .method_and_path("")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(
            fixture.evaluate(&policy, b"POST", b"/_bulk", HttpVersion::HTTP_1_1, &[]),
            CaptureDirective::Capture
        );
    }

    #[test]
    fn header_names_match_case_insensitively_and_values_do_not() {
        // ConditionallyReliableLoggingHttpHandlerTest:783 configures "user-Agent" against a wire
        // header of "User-Agent".
        let policy = SuppressionPolicy::builder()
            .header("user-Agent", ".*uploader.*")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"POST",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("User-Agent", b"uploader")]
            ),
            CaptureDirective::Drop
        );
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"POST",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("USER-AGENT", b"the-uploader-v2")]
            ),
            CaptureDirective::Drop
        );
        // Values are case-sensitive: no CASE_INSENSITIVE flag anywhere.
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"POST",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("User-Agent", b"UPLOADER")]
            ),
            CaptureDirective::Capture
        );
    }

    #[test]
    fn only_the_first_value_of_a_repeated_header_is_tested() {
        let policy = SuppressionPolicy::builder()
            .header("user-agent", "uploader")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"POST",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("User-Agent", b"browser"), ("User-Agent", b"uploader"),]
            ),
            CaptureDirective::Capture,
            "a second value that matches must not suppress"
        );
    }

    #[test]
    fn an_absent_header_never_suppresses() {
        let policy = SuppressionPolicy::builder()
            .header("user-agent", ".*")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"POST",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("Host", b"localhost")]
            ),
            CaptureDirective::Capture
        );
    }

    #[test]
    fn any_one_of_several_header_pairs_suppresses() {
        let policy = SuppressionPolicy::builder()
            .header("x-one", "a")
            .unwrap()
            .header("x-two", "b")
            .unwrap()
            .build();
        let fixture = Fixture::new(&policy);
        assert_eq!(policy.selected_header_names().len(), 2);
        assert_eq!(
            fixture.evaluate(
                &policy,
                b"GET",
                b"/",
                HttpVersion::HTTP_1_1,
                &[("X-Two", b"b")]
            ),
            CaptureDirective::Drop
        );
    }

    #[test]
    fn the_three_way_distinction_is_preserved() {
        let mut suppression = RequestSuppression::new();
        suppression.before_read();
        assert!(suppression.should_capture_request());

        // (b) the decision is known before any byte was committed: emit nothing at all.
        assert_eq!(
            suppression.apply_directive(CaptureDirective::Drop),
            SuppressionTransition::SuppressedBeforeCommit
        );
        assert!(!suppression.should_capture_request());

        // The suppressed request's response is suppressed too, then capture resumes.
        suppression.finish_request();
        assert_eq!(suppression.state(), CaptureState::IgnoreResponse);
        assert!(!suppression.should_capture_response());
        suppression.before_read();
        assert_eq!(suppression.state(), CaptureState::Capture);

        // (c) a prefix was captured first: emit exactly one requestDropped marker.
        suppression.note_read_captured();
        assert_eq!(
            suppression.apply_directive(CaptureDirective::Drop),
            SuppressionTransition::SuppressedAfterCommittedPrefix
        );
        assert!(!suppression.has_committed_prefix());
    }

    #[test]
    fn a_captured_request_leaves_the_state_capturing_through_its_response() {
        let mut suppression = RequestSuppression::new();
        suppression.note_read_captured();
        assert_eq!(
            suppression.apply_directive(CaptureDirective::Capture),
            SuppressionTransition::Unchanged
        );
        suppression.finish_request();
        assert_eq!(suppression.state(), CaptureState::Capture);
        assert!(suppression.should_capture_response());
    }
}
