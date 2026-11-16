//! The only headers that exist to the scanner (traceability row HTTP-07).
//!
//! # Why a general header map is wrong here
//!
//! Java runs its side-channel decoder with `PassThruHttpHeaders`, which keeps
//! `{Content-Length, Transfer-Encoding, Trailer}` plus every key of
//! `--suppressCaptureForHeaderMatch` and **silently drops every other name**
//! (`PassThruHttpHeaders.java:19-33`, `:53-123`). That is not an optimisation detail, it is the
//! contract:
//!
//! * framing depends only on the retained names, so framing stays faithful;
//! * `Host`, `Expect`, `Connection`, and `Upgrade` are invisible to framing, which is why
//!   `Expect: 100-continue` is not special-cased anywhere in the proxy;
//! * a value pattern configured against a name that was not retained can never match.
//!
//! This module reproduces exactly that set, borrowed from the parsed head, with no owned map and —
//! in the common case of no configured suppression header and at most one value per retained name —
//! no allocation at all.

/// Header names retained unconditionally, lowercased for case-insensitive comparison.
///
/// `RequestCapturePredicate.java:18-20`.
pub const ALWAYS_RETAINED: [&str; 3] = ["content-length", "transfer-encoding", "trailer"];

/// Names refused inside a chunked message's trailers.
///
/// `HttpObjectDecoder:891-894` refuses exactly these three. Java turns the refusal into an invalid
/// message, which under owner decision OD-1 is terminal here.
pub const FORBIDDEN_TRAILER_NAMES: [&str; 3] = ALWAYS_RETAINED;

/// The configured set of header names whose first value the scanner retains.
///
/// Comparison is ASCII case-insensitive, matching `DefaultHttpHeaders`' case-insensitive name
/// hashing, which `--suppressCaptureForHeaderMatch` documents (`CaptureProxy.java:196`) and which
/// the Java test relies on by configuring `user-Agent` against a wire header of `User-Agent`.
#[derive(Debug, Clone, Default)]
pub struct SelectedHeaderNames {
    /// Lowercased, de-duplicated, in configuration order.
    lowercased: Vec<Box<str>>,
}

impl SelectedHeaderNames {
    /// Builds the set, lowercasing and de-duplicating while keeping configuration order.
    ///
    /// Names in [`ALWAYS_RETAINED`] are accepted but not stored twice: they are reachable through
    /// the dedicated accessors instead.
    #[must_use]
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut lowercased: Vec<Box<str>> = Vec::new();
        for name in names {
            let lower = name.as_ref().to_ascii_lowercase();
            if lowercased.iter().any(|existing| **existing == *lower) {
                continue;
            }
            lowercased.push(lower.into_boxed_str());
        }
        Self { lowercased }
    }

    /// The configured names, lowercased, in configuration order.
    #[must_use]
    pub fn names(&self) -> &[Box<str>] {
        &self.lowercased
    }

    /// How many names are configured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lowercased.len()
    }

    /// True when nothing beyond the always-retained names is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lowercased.is_empty()
    }

    /// The index of `name` in this set, comparing ASCII case-insensitively.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.lowercased
            .iter()
            .position(|configured| configured.eq_ignore_ascii_case(name))
    }
}

/// Zero, one, or many borrowed values for one retained header name.
///
/// Inline for the zero and one cases, which is every well-formed request: more than one
/// `Content-Length` is a framing error and more than one `Transfer-Encoding` is rare.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HeaderValues<'b> {
    /// The header was absent.
    #[default]
    Absent,
    /// Exactly one value.
    One(&'b [u8]),
    /// Several values, in wire order.
    Many(Vec<&'b [u8]>),
}

impl<'b> HeaderValues<'b> {
    /// Appends a value in wire order.
    pub fn push(&mut self, value: &'b [u8]) {
        *self = match std::mem::take(self) {
            Self::Absent => Self::One(value),
            Self::One(first) => Self::Many(vec![first, value]),
            Self::Many(mut values) => {
                values.push(value);
                Self::Many(values)
            }
        };
    }

    /// The first value, which is the only one Java's header matcher ever looks at (spec §5.2).
    #[must_use]
    pub fn first(&self) -> Option<&'b [u8]> {
        match self {
            Self::Absent => None,
            Self::One(value) => Some(value),
            Self::Many(values) => values.first().copied(),
        }
    }

    /// How many values were present.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Absent => 0,
            Self::One(_) => 1,
            Self::Many(values) => values.len(),
        }
    }

    /// True when the header was absent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Iterates the values in wire order.
    pub fn iter(&self) -> impl Iterator<Item = &'b [u8]> + '_ {
        let (single, many): (Option<&'b [u8]>, &[&'b [u8]]) = match self {
            Self::Absent => (None, &[]),
            Self::One(value) => (Some(value), &[]),
            Self::Many(values) => (None, values.as_slice()),
        };
        single.into_iter().chain(many.iter().copied())
    }
}

impl<'a, 'b> IntoIterator for &'a HeaderValues<'b> {
    type Item = &'b [u8];
    type IntoIter = Box<dyn Iterator<Item = &'b [u8]> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// The retained headers of one request head, borrowed from the parsed head bytes.
#[derive(Debug, Default)]
pub struct SelectedHeaders<'b> {
    content_length: HeaderValues<'b>,
    transfer_encoding: HeaderValues<'b>,
    trailer: HeaderValues<'b>,
    /// Parallel to [`SelectedHeaderNames::names`]: the configured lowercased name paired with its
    /// first value, if present. First value only, like `headers().get(name)`.
    suppression: Vec<(&'b str, Option<&'b [u8]>)>,
}

impl<'b> SelectedHeaders<'b> {
    /// Collects the retained headers from a fully parsed head.
    ///
    /// `headers` must be the result of **one** `httparse` call over the complete head; this method
    /// copies the few borrowed slices it keeps and looks at nothing else.
    #[must_use]
    pub fn collect(names: &'b SelectedHeaderNames, headers: &[httparse::Header<'b>]) -> Self {
        let mut selected = Self {
            suppression: names
                .names()
                .iter()
                .map(|name| (&**name, None))
                .collect::<Vec<_>>(),
            ..Self::default()
        };
        for header in headers {
            if header.name.eq_ignore_ascii_case("content-length") {
                selected.content_length.push(header.value);
            } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                selected.transfer_encoding.push(header.value);
            } else if header.name.eq_ignore_ascii_case("trailer") {
                selected.trailer.push(header.value);
            }
            if let Some(index) = names.index_of(header.name) {
                // First value only: `headers().get(name)` returns the first and later occurrences
                // are never tested (spec §5.2).
                let slot = &mut selected.suppression[index].1;
                if slot.is_none() {
                    *slot = Some(header.value);
                }
            }
        }
        selected
    }

    /// Every `Content-Length` value, in wire order.
    #[must_use]
    pub fn content_length(&self) -> &HeaderValues<'b> {
        &self.content_length
    }

    /// Every `Transfer-Encoding` value, in wire order.
    #[must_use]
    pub fn transfer_encoding(&self) -> &HeaderValues<'b> {
        &self.transfer_encoding
    }

    /// Every `Trailer` value, in wire order.
    #[must_use]
    pub fn trailer(&self) -> &HeaderValues<'b> {
        &self.trailer
    }

    /// The first value of the configured suppression header at `index`, if the header was present.
    #[must_use]
    pub fn suppression_value(&self, index: usize) -> Option<&'b [u8]> {
        self.suppression.get(index).and_then(|(_, value)| *value)
    }

    /// The first value of the configured header `name`, if it was configured **and** present.
    ///
    /// Looking up by name rather than by index is deliberate: in Java the retained-name list and
    /// the value-pattern map are wired separately, so a value pattern can silently reference a name
    /// that was discarded. Here the only way to ask is by name, and a name that was never retained
    /// simply has no slot.
    #[must_use]
    pub fn value_for(&self, name: &str) -> Option<&'b [u8]> {
        self.suppression
            .iter()
            .find(|(configured, _)| configured.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| *value)
    }

    /// True when any `Transfer-Encoding` value contains a `chunked` element.
    ///
    /// `HttpUtil.isTransferEncodingChunked` delegates to `HttpHeaders.containsValue(..., true)`,
    /// which splits each value on commas, trims each element, and compares ASCII
    /// case-insensitively. So `Transfer-Encoding: gzip, chunked` is chunked, and — matching Netty
    /// rather than RFC 7230 — so is `chunked, gzip`.
    #[must_use]
    pub fn is_transfer_encoding_chunked(&self) -> bool {
        self.transfer_encoding
            .iter()
            .flat_map(|value| value.split(|byte| *byte == b','))
            .any(|element| trim_ows(element).eq_ignore_ascii_case(b"chunked"))
    }
}

/// Trims leading and trailing spaces and horizontal tabs.
#[must_use]
pub fn trim_ows(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(start, |index| index + 1);
    &value[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers<'b>(pairs: &[(&'b str, &'b [u8])]) -> Vec<httparse::Header<'b>> {
        pairs
            .iter()
            .map(|(name, value)| httparse::Header { name, value })
            .collect()
    }

    #[test]
    fn only_the_retained_names_survive() {
        let names = SelectedHeaderNames::default();
        let parsed = headers(&[
            ("Host", b"localhost"),
            ("Content-Length", b"16"),
            ("Expect", b"100-continue"),
            ("Trailer", b"X-Checksum"),
        ]);
        let selected = SelectedHeaders::collect(&names, &parsed);
        assert_eq!(selected.content_length().first(), Some(&b"16"[..]));
        assert_eq!(selected.trailer().first(), Some(&b"X-Checksum"[..]));
        assert!(selected.transfer_encoding().is_empty());
        // Nothing else is reachable at all: there is no map to look in.
        assert_eq!(selected.suppression_value(0), None);
    }

    #[test]
    fn retained_names_are_matched_case_insensitively() {
        let names = SelectedHeaderNames::default();
        let parsed = headers(&[("CONTENT-LENGTH", b"3"), ("transfer-ENCODING", b"chunked")]);
        let selected = SelectedHeaders::collect(&names, &parsed);
        assert_eq!(selected.content_length().first(), Some(&b"3"[..]));
        assert!(selected.is_transfer_encoding_chunked());
    }

    #[test]
    fn a_configured_suppression_header_keeps_only_its_first_value() {
        // Spec §5.2: headers().get(name) returns the first value; later occurrences are never
        // tested. A second value that would match must therefore NOT suppress.
        let names = SelectedHeaderNames::new(["user-Agent"]);
        let parsed = headers(&[("User-Agent", b"first"), ("user-agent", b"second")]);
        let selected = SelectedHeaders::collect(&names, &parsed);
        assert_eq!(selected.suppression_value(0), Some(&b"first"[..]));
    }

    #[test]
    fn duplicate_configured_names_collapse_to_one_slot() {
        let names = SelectedHeaderNames::new(["User-Agent", "user-agent", "X-Other"]);
        assert_eq!(names.len(), 2);
        assert_eq!(names.index_of("USER-AGENT"), Some(0));
        assert_eq!(names.index_of("x-other"), Some(1));
        assert_eq!(names.index_of("absent"), None);
    }

    #[test]
    fn chunked_detection_splits_comma_lists_like_netty() {
        let names = SelectedHeaderNames::default();
        for (value, expected) in [
            (&b"chunked"[..], true),
            (b"gzip, chunked", true),
            (b"chunked, gzip", true),
            (b"  CHUNKED  ", true),
            (b"\tchunked\t", true),
            (b"gzip", false),
            (b"chunkedx", false),
            (b"xchunked", false),
            (b"", false),
        ] {
            let parsed = headers(&[("Transfer-Encoding", value)]);
            let selected = SelectedHeaders::collect(&names, &parsed);
            assert_eq!(
                selected.is_transfer_encoding_chunked(),
                expected,
                "Transfer-Encoding: {:?}",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[test]
    fn multiple_transfer_encoding_headers_are_all_considered() {
        let names = SelectedHeaderNames::default();
        let parsed = headers(&[
            ("Transfer-Encoding", b"gzip"),
            ("Transfer-Encoding", b"chunked"),
        ]);
        let selected = SelectedHeaders::collect(&names, &parsed);
        assert!(selected.is_transfer_encoding_chunked());
        assert_eq!(selected.transfer_encoding().len(), 2);
    }

    #[test]
    fn header_values_keeps_wire_order() {
        let mut values = HeaderValues::Absent;
        assert!(values.is_empty());
        values.push(b"a");
        values.push(b"b");
        values.push(b"c");
        assert_eq!(values.len(), 3);
        assert_eq!(values.first(), Some(&b"a"[..]));
        assert_eq!(
            values.iter().collect::<Vec<_>>(),
            vec![&b"a"[..], b"b", b"c"]
        );
    }

    #[test]
    fn ows_trimming_leaves_inner_whitespace_alone() {
        assert_eq!(trim_ows(b"  a b \t"), b"a b");
        assert_eq!(trim_ows(b"   "), b"");
        assert_eq!(trim_ows(b""), b"");
    }
}
