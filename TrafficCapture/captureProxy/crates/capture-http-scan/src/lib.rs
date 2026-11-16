//! Bounded incremental HTTP/1 request framing, response classification, capture suppression, and
//! post-capture header rewriting.
//!
//! # What this crate is responsible for
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`request_scanner`] | HTTP-06 | incremental request framing: head, `Content-Length`, chunked, trailers, end of message |
//! | [`response_classifier`] | HTTP-06 | interim (non-101 1xx) versus final response classification, bytes kept raw |
//! | [`selected_headers`] | HTTP-07 | the only headers that exist to the scanner |
//! | [`suppression`] | HTTP-04 | the three-way capture / ignore / drop decision |
//! | [`rewrite`] | PROXY-09, PROXY-10 | byte-level header add and remove, after capture |
//!
//! # The two invariants everything else serves
//!
//! 1. **Capture sees the original client bytes.** The scanner never mutates the bytes it is fed and
//!    never asks its caller to forward a reserialised message. Rewriting (and only rewriting)
//!    changes the bytes that go to the source, and it happens strictly after capture
//!    (transport-http spec §2.4).
//! 2. **Framing the proxy cannot determine is never forwarded.** Owner decision OD-1 makes a
//!    framing error terminal: the connection emits a `connectionException` observation and closes.
//!    Java instead tunnels those bytes raw and uncaptured for up to five minutes or 250 MiB, which
//!    is the single largest capture gap in the Java data plane (spec §3.7).
//!
//! # Owner decisions this crate implements
//!
//! * **OD-1** — malformed framing is terminal. [`request_scanner::FramingError`] is the complete
//!   list of conditions, and it includes the ones Java silently tunnels: a request line over the
//!   configured budget, a bare LF in strict mode, a `Transfer-Encoding`/`Content-Length` conflict, a
//!   duplicate `Content-Length`, and every chunk framing error. The request-line budget comes from
//!   [`capture_config::RequestLimits::max_request_line_bytes`], never from a parser default.
//! * **OD-2** — [`capture_config::HttpStrictness::Strict`] (CRLF only) is the default;
//!   `LenientLineEndings` additionally accepts a bare LF and relaxes nothing else.
//! * **OD-3** — [`rewrite`] state is scoped to the request being parsed. Nothing in this crate lets
//!   an outbound response perturb rewrite state, so a `100 Continue` arriving mid-head cannot leak a
//!   header configured for removal and pipelined requests are rewritten too.
//!
//! # Suppression patterns
//!
//! Every suppression match goes through [`capture_config::SuppressionPattern`], which matches whole
//! strings the way Java's `Pattern.matches()` does. There is deliberately no [`regex::Regex`] in
//! this crate: orchestration defaults suppression patterns to the empty string, and a search-based
//! match would drop every request and silently disable all capture.
//!
//! [`regex::Regex`]: https://docs.rs/regex/latest/regex/struct.Regex.html

pub mod request_scanner;
pub mod response_classifier;
pub mod rewrite;
pub mod selected_headers;
pub mod suppression;

pub use capture_config::CaptureDirective;
pub use request_scanner::{
    FramingError, HeadComplete, LimitViolation, MessageComplete, RequestScanner, ScanError,
    ScanEvent,
};
pub use response_classifier::{
    Classification, INTERIM_HOLD_LIMIT, ResponseClassifier, ResponsePiece,
};
pub use rewrite::{HeaderRewriter, RewriteConfig, RewriteError};
pub use selected_headers::{SelectedHeaderNames, SelectedHeaders};
pub use suppression::{
    CaptureState, RequestSuppression, SuppressionPolicy, SuppressionPolicyBuilder,
    SuppressionTransition,
};

/// How the body of a request is framed.
///
/// Derived exactly as `HttpObjectDecoder.readHeaders` derives it (spec §3.3), with owner decision
/// OD-1 turning the ambiguous combinations into framing errors instead of silent choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    /// No body at all.
    ///
    /// For a **request** there is no connection-close framing: no `Content-Length` and no
    /// `Transfer-Encoding: chunked` means a zero-length body, whatever the method
    /// (`HttpObjectDecoder:404-412`). There is no method-based body suppression anywhere in the
    /// Java proxy, so `GET` with a `Content-Length` does get a body.
    Empty,
    /// Exactly `len` body bytes follow the head.
    Fixed(u64),
    /// Chunked framing follows the head.
    Chunked,
}

/// An HTTP version as it appeared on the request line.
///
/// Java accepts any `<name>/<major>.<minor>` (`HttpVersion.valueOf`) and the capture predicate
/// matches against the normalised text, which is how the hardcoded `HTTP/2.*` pattern suppresses an
/// h2c prior-knowledge preface (spec §5.3). This type therefore keeps the parsed numbers *and* can
/// render the normalised text without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpVersion {
    name: VersionName,
    major: u16,
    minor: u16,
}

/// The protocol name of a version token, retained inline so rendering never allocates.
#[derive(Clone, Copy)]
struct VersionName {
    bytes: [u8; Self::CAPACITY],
    len: u8,
}

impl VersionName {
    /// Longest protocol name accepted before the `/`. `HTTP` needs four; the slack exists only so a
    /// nonstandard name is reported rather than silently truncated.
    const CAPACITY: usize = 8;

    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > Self::CAPACITY {
            return None;
        }
        let mut stored = [0u8; Self::CAPACITY];
        stored[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: stored,
            // The length was just bounded by CAPACITY, which is far below u8::MAX.
            len: u8::try_from(bytes.len()).expect("length is bounded by CAPACITY"),
        })
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl PartialEq for VersionName {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for VersionName {}

impl std::fmt::Debug for VersionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&String::from_utf8_lossy(self.as_bytes()), f)
    }
}

/// The rendered form of an [`HttpVersion`], e.g. `HTTP/1.1`.
///
/// Dereferences to `str` and allocates nothing: a version token is bounded by construction.
#[derive(Clone, Copy)]
pub struct VersionText {
    bytes: [u8; Self::CAPACITY],
    len: u8,
}

impl VersionText {
    /// `<name up to 8>/<up to 5 digits>.<up to 5 digits>`.
    const CAPACITY: usize = VersionName::CAPACITY + 1 + 5 + 1 + 5;
}

impl std::ops::Deref for VersionText {
    type Target = str;

    fn deref(&self) -> &str {
        // Every byte written is ASCII: a validated protocol name plus digits and punctuation.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("a version token is rendered from ASCII only")
    }
}

impl std::fmt::Debug for VersionText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl std::fmt::Display for VersionText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl HttpVersion {
    /// `HTTP/1.1`.
    pub const HTTP_1_1: Self = Self {
        name: VersionName {
            bytes: *b"HTTP\0\0\0\0",
            len: 4,
        },
        major: 1,
        minor: 1,
    };

    /// `HTTP/1.0`.
    pub const HTTP_1_0: Self = Self {
        name: VersionName {
            bytes: *b"HTTP\0\0\0\0",
            len: 4,
        },
        major: 1,
        minor: 0,
    };

    /// Parses a version token such as `HTTP/1.1`.
    ///
    /// Returns `None` when the token is not `<name>/<digits>.<digits>`, when the name is empty or
    /// longer than eight bytes, or when either number has no digits, more than five digits, or does
    /// not fit a `u16`. Java's `HttpVersion.valueOf` applies the regex `(\S+)/(\d+)\.(\d+)`; the
    /// bounds here exist so the token can be rendered without allocating.
    #[must_use]
    pub fn parse(token: &[u8]) -> Option<Self> {
        let slash = token.iter().position(|byte| *byte == b'/')?;
        let name = VersionName::new(&token[..slash])?;
        let numbers = &token[slash + 1..];
        let dot = numbers.iter().position(|byte| *byte == b'.')?;
        let major = parse_version_number(&numbers[..dot])?;
        let minor = parse_version_number(&numbers[dot + 1..])?;
        Some(Self { name, major, minor })
    }

    /// The protocol name, e.g. `HTTP`.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        self.name.as_bytes()
    }

    /// The major version.
    #[must_use]
    pub fn major(&self) -> u16 {
        self.major
    }

    /// The minor version.
    #[must_use]
    pub fn minor(&self) -> u16 {
        self.minor
    }

    /// True for HTTP/1.0 and anything earlier, which is the version class Netty exempts from the
    /// duplicate-`Content-Length` guard (spec §3.5). Owner decision OD-1 removes that exemption, so
    /// this is reported for diagnostics rather than used to relax a check.
    #[must_use]
    pub fn is_http_1_0_or_earlier(&self) -> bool {
        self.major < 1 || (self.major == 1 && self.minor == 0)
    }

    /// The normalised text Java's capture predicate matches against, e.g. `HTTP/1.1`.
    ///
    /// # Panics
    ///
    /// Panics only if the rendered token exceeds [`VersionText::CAPACITY`], which
    /// [`HttpVersion::parse`]'s bounds make unreachable: a name of at most eight bytes plus two
    /// numbers of at most five digits each always fits.
    #[must_use]
    pub fn text(&self) -> VersionText {
        let mut bytes = [0u8; VersionText::CAPACITY];
        let mut len = 0usize;
        let name = self.name.as_bytes();
        bytes[..name.len()].copy_from_slice(name);
        len += name.len();
        bytes[len] = b'/';
        len += 1;
        len += write_number(&mut bytes[len..], self.major);
        bytes[len] = b'.';
        len += 1;
        len += write_number(&mut bytes[len..], self.minor);
        VersionText {
            bytes,
            // CAPACITY is 20, far below u8::MAX.
            len: u8::try_from(len).expect("a rendered version token is at most CAPACITY bytes"),
        }
    }
}

fn parse_version_number(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() || digits.len() > 5 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u32 = 0;
    for digit in digits {
        value = value * 10 + u32::from(*digit - b'0');
    }
    u16::try_from(value).ok()
}

/// Writes `value`'s decimal form at the start of `out` and returns how many bytes it took.
fn write_number(out: &mut [u8], value: u16) -> usize {
    let mut digits = [0u8; 5];
    let mut count = 0;
    let mut remaining = value;
    loop {
        digits[count] = b'0' + u8::try_from(remaining % 10).expect("a decimal digit fits in u8");
        remaining /= 10;
        count += 1;
        if remaining == 0 {
            break;
        }
    }
    for (index, digit) in digits[..count].iter().rev().enumerate() {
        out[index] = *digit;
    }
    count
}

/// The borrowed view of a request head that a [`CapturePredicate`] decides on.
///
/// This is the complete set of metadata the Java proxy can see, and deliberately no more: `Host`,
/// `Expect`, `Connection`, and `Upgrade` do not exist here because `PassThruHttpHeaders` discards
/// every name outside the preserved set (spec §3.2, §14.3). Nothing in it is owned, so evaluating a
/// predicate allocates nothing.
#[derive(Debug)]
pub struct RequestHead<'b> {
    method: &'b [u8],
    target: &'b [u8],
    version: HttpVersion,
    headers: &'b SelectedHeaders<'b>,
}

impl<'b> RequestHead<'b> {
    /// Builds a head view.
    #[must_use]
    pub fn new(
        method: &'b [u8],
        target: &'b [u8],
        version: HttpVersion,
        headers: &'b SelectedHeaders<'b>,
    ) -> Self {
        Self {
            method,
            target,
            version,
            headers,
        }
    }

    /// The request method, exactly as it appeared on the wire.
    #[must_use]
    pub fn method(&self) -> &'b [u8] {
        self.method
    }

    /// The raw request-target, including any query string. Java matches its `path` pattern against
    /// this, **not** against a decoded path (spec §5.2).
    #[must_use]
    pub fn target(&self) -> &'b [u8] {
        self.target
    }

    /// The request's HTTP version.
    #[must_use]
    pub fn version(&self) -> HttpVersion {
        self.version
    }

    /// The retained headers.
    #[must_use]
    pub fn headers(&self) -> &'b SelectedHeaders<'b> {
        self.headers
    }

    /// The method as UTF-8, or `None` when it is not valid UTF-8.
    ///
    /// A pattern cannot match a non-UTF-8 value, so `None` means "no suppression match".
    #[must_use]
    pub fn method_str(&self) -> Option<&'b str> {
        std::str::from_utf8(self.method).ok()
    }

    /// The request-target as UTF-8, or `None` when it is not valid UTF-8.
    #[must_use]
    pub fn target_str(&self) -> Option<&'b str> {
        std::str::from_utf8(self.target).ok()
    }

    /// True when this request is Critical Mutation Traffic.
    ///
    /// `ProxyChannelInitializer.shouldGuaranteeMessageOffloading` (spec §6.1) is exactly
    /// `POST || PUT || DELETE || PATCH`, compared with `HttpMethod.equals`, so the comparison is
    /// case-sensitive against the canonical names. `GET`, `HEAD`, `OPTIONS`, `TRACE`, `CONNECT`, and
    /// every extension method are not gated.
    #[must_use]
    pub fn is_critical_mutation(&self) -> bool {
        is_critical_mutation(self.method)
    }
}

/// True when `method` is one of the four Critical Mutation Traffic methods.
///
/// See [`RequestHead::is_critical_mutation`].
#[must_use]
pub fn is_critical_mutation(method: &[u8]) -> bool {
    matches!(method, b"POST" | b"PUT" | b"DELETE" | b"PATCH")
}

/// The policy-composition point for capture decisions (traceability row HTTP-08).
///
/// A predicate declares which header names the scanner must retain and then decides, from the
/// borrowed head alone, whether the request is captured. Java's equivalent is
/// `RequestCapturePredicate` plus `HttpHeadersToPreserve`; keeping the two together in one trait is
/// what makes it impossible to configure a value pattern on a header the scanner threw away — a
/// real Java trap, since there the two lists are wired separately
/// (`HeaderValueFilteringCapturePredicate.java:25-29`).
pub trait CapturePredicate {
    /// Header names, beyond `Content-Length`, `Transfer-Encoding`, and `Trailer`, whose first value
    /// the scanner must retain for [`CapturePredicate::evaluate`].
    fn selected_header_names(&self) -> &[Box<str>];

    /// Decides whether to capture the request.
    fn evaluate(&self, head: &RequestHead<'_>) -> CaptureDirective;
}

/// Captures everything.
///
/// The base `RequestCapturePredicate.apply` returns `CAPTURE` unconditionally
/// (`RequestCapturePredicate.java:26-29`), and then the preserved header set is just the three
/// framing names.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureEverything;

impl CapturePredicate for CaptureEverything {
    fn selected_header_names(&self) -> &[Box<str>] {
        &[]
    }

    fn evaluate(&self, _head: &RequestHead<'_>) -> CaptureDirective {
        CaptureDirective::Capture
    }
}

impl<P: CapturePredicate + ?Sized> CapturePredicate for &P {
    fn selected_header_names(&self) -> &[Box<str>] {
        (**self).selected_header_names()
    }

    fn evaluate(&self, head: &RequestHead<'_>) -> CaptureDirective {
        (**self).evaluate(head)
    }
}

impl<P: CapturePredicate + ?Sized> CapturePredicate for Box<P> {
    fn selected_header_names(&self) -> &[Box<str>] {
        (**self).selected_header_names()
    }

    fn evaluate(&self, head: &RequestHead<'_>) -> CaptureDirective {
        (**self).evaluate(head)
    }
}

impl<P: CapturePredicate + ?Sized> CapturePredicate for std::sync::Arc<P> {
    fn selected_header_names(&self) -> &[Box<str>] {
        (**self).selected_header_names()
    }

    fn evaluate(&self, head: &RequestHead<'_>) -> CaptureDirective {
        (**self).evaluate(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_text_round_trips_without_allocating() {
        for token in [
            &b"HTTP/1.1"[..],
            b"HTTP/1.0",
            b"HTTP/2.0",
            b"HTTP/0.9",
            b"HTTP/12.34",
        ] {
            let version = HttpVersion::parse(token).expect("parses");
            assert_eq!(&*version.text(), std::str::from_utf8(token).expect("ascii"));
        }
    }

    #[test]
    fn the_h2c_preface_version_renders_as_the_text_javas_hardcoded_pattern_matches() {
        // CaptureProxy.java:615 hardcodes protocolPattern("HTTP/2.*") and matches it against
        // protocolVersion().text().
        let version = HttpVersion::parse(b"HTTP/2.0").expect("parses");
        let pattern = capture_config::SuppressionPattern::new("HTTP/2.*").expect("compiles");
        assert!(pattern.matches(&version.text()));
        assert!(!pattern.matches(&HttpVersion::HTTP_1_1.text()));
    }

    #[test]
    fn malformed_version_tokens_are_rejected() {
        for token in [
            &b"HTTP/2"[..],
            b"HTTP1.1",
            b"HTTP/.1",
            b"HTTP/1.",
            b"/1.1",
            b"HTTP/1.1.1",
            b"VERYLONGNAME/1.1",
            b"HTTP/111111.1",
        ] {
            assert!(
                HttpVersion::parse(token).is_none(),
                "{:?} must be rejected",
                String::from_utf8_lossy(token)
            );
        }
        // A bare `HTTP/2` failing the regex matches Netty: HttpVersion.valueOf requires a minor.
    }

    #[test]
    fn the_critical_mutation_set_is_exactly_four_methods_case_sensitively() {
        for method in [&b"POST"[..], b"PUT", b"DELETE", b"PATCH"] {
            assert!(is_critical_mutation(method));
        }
        for method in [
            &b"GET"[..],
            b"HEAD",
            b"OPTIONS",
            b"TRACE",
            b"CONNECT",
            b"post",
            b"Post",
            b"POSTX",
            b"",
        ] {
            assert!(
                !is_critical_mutation(method),
                "{:?} must not be gated",
                String::from_utf8_lossy(method)
            );
        }
    }

    #[test]
    fn http_1_0_detection_matches_nettys_duplicate_content_length_exemption_test() {
        assert!(HttpVersion::HTTP_1_0.is_http_1_0_or_earlier());
        assert!(
            HttpVersion::parse(b"HTTP/0.9")
                .expect("parses")
                .is_http_1_0_or_earlier()
        );
        assert!(!HttpVersion::HTTP_1_1.is_http_1_0_or_earlier());
        assert!(
            !HttpVersion::parse(b"HTTP/2.0")
                .expect("parses")
                .is_http_1_0_or_earlier()
        );
    }
}
