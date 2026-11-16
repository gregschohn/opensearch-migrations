//! Interim-versus-final response classification (traceability row HTTP-06).
//!
//! # What this is and is not
//!
//! The proxy **never runs a response decoder** (spec §3.5, §7). Response classification is a fixed
//! 12-byte prefix test plus a scan for `CR LF CR LF`, and every byte reaches the client exactly as
//! it arrived. This module reproduces `LoggingHttpHandler.classifySourceResponseBytes`
//! (`LHH:856-882`) and `isNonSwitchingInformationalResponse` (`LHH:927-944`) literally, because
//! anything more is a behaviour change: an interim observation is a byte-exact slice of the wire
//! bytes cut at a `CR LF CR LF` boundary, and a final response is emitted as one write containing
//! the entire held buffer, including any bytes beyond the final response's own head.
//!
//! `101 Switching Protocols` is deliberately **not** interim (`LHH:943`). It is an ordinary final
//! write, after which every byte in both directions is forwarded verbatim.
//!
//! # The one deliberate addition
//!
//! Java's hold buffer is an unbounded `ByteArrayOutputStream` (`LHH:230`): a source that sends
//! `"HTTP/1.1 100 "` and then a gigabyte with no `CR LF CR LF` grows proxy heap without limit
//! (spec §13-D11). Here the hold is capped at [`INTERIM_HOLD_LIMIT`]; exceeding it flushes the held
//! bytes as a final write and asks the caller to close.

use std::borrow::Cow;

/// Bytes needed before any classification decision is possible (`LHH:861`).
pub const CLASSIFICATION_PREFIX: usize = 12;

/// Ceiling on held, unclassified response bytes.
///
/// A 1xx head is tiny — the largest realistic one is a `103 Early Hints` with a few `Link` headers
/// — so 8 KiB is generous. `DEVIATION:` Java has no bound at all.
pub const INTERIM_HOLD_LIMIT: usize = 8 * 1024;

/// One classified run of response bytes, borrowed when possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponsePiece<'a> {
    /// A complete non-switching informational (1xx other than 101) response head.
    ///
    /// Captured as an `interimResponse` observation.
    Interim(Cow<'a, [u8]>),
    /// Bytes that are not an interim response head.
    ///
    /// Captured as a `write` observation. The first such piece for a request carries the whole held
    /// buffer, exactly as Java emits it.
    Final(Cow<'a, [u8]>),
}

impl ResponsePiece<'_> {
    /// The bytes of this piece.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Interim(bytes) | Self::Final(bytes) => bytes,
        }
    }

    /// True for an interim piece.
    #[must_use]
    pub fn is_interim(&self) -> bool {
        matches!(self, Self::Interim(_))
    }
}

/// What one outbound write classified into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification<'a> {
    /// Classified runs, in byte order. May be empty when everything is still held.
    pub pieces: Vec<ResponsePiece<'a>>,
    /// The hold exceeded [`INTERIM_HOLD_LIMIT`]. The held bytes have been emitted as a final piece
    /// and the caller must close the connection after writing them.
    pub hold_overflow: bool,
}

/// Classifies one connection's response bytes.
///
/// Reset with [`ResponseClassifier::begin_request`] when the next request's reads start, which is
/// where Java installs a fresh request context (`LHH:511-524`).
#[derive(Debug)]
pub struct ResponseClassifier {
    pending: Vec<u8>,
    final_seen: bool,
    hold_limit: usize,
}

impl Default for ResponseClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseClassifier {
    /// A classifier with the default hold limit.
    #[must_use]
    pub fn new() -> Self {
        Self::with_hold_limit(INTERIM_HOLD_LIMIT)
    }

    /// A classifier with an explicit hold limit. Mainly for tests.
    #[must_use]
    pub fn with_hold_limit(hold_limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            final_seen: false,
            hold_limit,
        }
    }

    /// Whether a final response has already been classified for the current request.
    ///
    /// Once true, every further write takes the fast path with no classification at all, matching
    /// `LHH:771-778`.
    #[must_use]
    pub fn final_response_seen(&self) -> bool {
        self.final_seen
    }

    /// Bytes currently held, unclassified.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Classifies one outbound write.
    pub fn classify<'a>(&mut self, bytes: &'a [u8]) -> Classification<'a> {
        if self.final_seen {
            // Fast path: no classification, no copy, bytes forwarded and captured verbatim.
            let pieces = if bytes.is_empty() {
                Vec::new()
            } else {
                vec![ResponsePiece::Final(Cow::Borrowed(bytes))]
            };
            return Classification {
                pieces,
                hold_overflow: false,
            };
        }

        self.pending.extend_from_slice(bytes);
        let mut pieces = Vec::new();
        while self.pending.len() >= CLASSIFICATION_PREFIX {
            if !is_non_switching_informational_response(&self.pending) {
                // The ENTIRE pending buffer becomes one final write, verbatim, including anything
                // after the final response's own head: classification never parses it.
                pieces.push(ResponsePiece::Final(Cow::Owned(std::mem::take(
                    &mut self.pending,
                ))));
                self.final_seen = true;
                return Classification {
                    pieces,
                    hold_overflow: false,
                };
            }
            let Some(end) = index_after_http_headers(&self.pending) else {
                break;
            };
            pieces.push(ResponsePiece::Interim(Cow::Owned(
                self.pending[..end].to_vec(),
            )));
            self.pending.drain(..end);
        }

        if self.pending.len() > self.hold_limit {
            // DEVIATION: Java holds without bound (spec §13-D11). Treat an over-cap hold as a
            // malformed response: flush what is held as a final write and close.
            pieces.push(ResponsePiece::Final(Cow::Owned(std::mem::take(
                &mut self.pending,
            ))));
            self.final_seen = true;
            return Classification {
                pieces,
                hold_overflow: true,
            };
        }

        Classification {
            pieces,
            hold_overflow: false,
        }
    }

    /// Emits any held bytes as a final write.
    ///
    /// Required on three paths, each of which must emit the residue **before** the observation that
    /// follows it: before the next request's reads (`LHH:841-854`), before the
    /// `connectionException` observation (`LHH:987-993`), and before the terminal `close`
    /// observation (`LHH:356-357`). Held bytes are never lost.
    pub fn flush_held(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    /// Starts classifying a new request's response.
    ///
    /// # Panics
    ///
    /// Panics if bytes are still held: the caller must [`ResponseClassifier::flush_held`] first, or
    /// those bytes would be silently dropped from the capture stream.
    pub fn begin_request(&mut self) {
        assert!(
            self.pending.is_empty(),
            "held response bytes must be flushed before the next request begins"
        );
        self.final_seen = false;
    }
}

/// Java's fixed 12-byte prefix test (`LHH:927-944`).
///
/// `HTTP/1.<digit> 1<digit><digit>`, excluding `101`. Nothing else about the response is examined.
#[must_use]
pub fn is_non_switching_informational_response(bytes: &[u8]) -> bool {
    if bytes.len() < CLASSIFICATION_PREFIX {
        return false;
    }
    &bytes[0..5] == b"HTTP/"
        && bytes[5] == b'1'
        && bytes[6] == b'.'
        && bytes[7].is_ascii_digit()
        && bytes[8] == b' '
        && bytes[9] == b'1'
        && bytes[10].is_ascii_digit()
        && bytes[11].is_ascii_digit()
        && !(bytes[10] == b'0' && bytes[11] == b'1')
}

/// The index just past the first `CR LF CR LF`, or `None` (`LHH:946-956`).
///
/// **Only** `CR LF CR LF`: a bare-LF-separated interim response is never recognised as ending, so
/// it is held and ultimately emitted as a final write. That is Java's behaviour and it is preserved
/// here, bounded by [`INTERIM_HOLD_LIMIT`].
#[must_use]
pub fn index_after_http_headers(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(classification: &Classification<'_>) -> Vec<String> {
        classification
            .pieces
            .iter()
            .map(|piece| {
                let tag = if piece.is_interim() {
                    "interim"
                } else {
                    "write"
                };
                format!("{tag}:{}", String::from_utf8_lossy(piece.bytes()))
            })
            .collect()
    }

    #[test]
    fn fewer_than_twelve_bytes_are_held_with_no_observation() {
        let mut classifier = ResponseClassifier::new();
        let classification = classifier.classify(b"HTTP/1.1 10");
        assert!(classification.pieces.is_empty());
        assert_eq!(classifier.held_bytes(), 11);
    }

    #[test]
    fn an_interim_response_split_across_writes_is_reassembled_exactly() {
        // SourceInterimResponseCaptureTest:34-66, the canonical case.
        let mut classifier = ResponseClassifier::new();
        assert!(classifier.classify(b"HTTP/1.1 100 Cont").pieces.is_empty());
        let classification = classifier.classify(b"inue\r\n\r\n");
        assert_eq!(
            kinds(&classification),
            vec!["interim:HTTP/1.1 100 Continue\r\n\r\n"]
        );
        assert_eq!(classifier.held_bytes(), 0);
        assert!(!classifier.final_response_seen());
    }

    #[test]
    fn one_write_yields_four_interims_and_one_final() {
        // The second half of SourceInterimResponseCaptureTest:34-66.
        let mut classifier = ResponseClassifier::new();
        let write = concat!(
            "HTTP/1.1 102 Processing\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </next>\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        );
        let classification = classifier.classify(write.as_bytes());
        assert_eq!(
            kinds(&classification),
            vec![
                "interim:HTTP/1.1 102 Processing\r\n\r\n",
                "interim:HTTP/1.1 103 Early Hints\r\n\r\n",
                "interim:HTTP/1.1 103 Early Hints\r\nLink: </next>\r\n\r\n",
                "write:HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            ]
        );
        assert!(classifier.final_response_seen());
    }

    #[test]
    fn switching_protocols_and_rejections_are_ordinary_final_writes() {
        // SourceInterimResponseCaptureTest:68-85.
        for response in [
            "HTTP/1.1 101 Switching Protocols\r\n\r\n",
            "HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\n\r\n",
        ] {
            let mut classifier = ResponseClassifier::new();
            let classification = classifier.classify(response.as_bytes());
            assert_eq!(classification.pieces.len(), 1, "{response}");
            assert!(!classification.pieces[0].is_interim(), "{response}");
            assert_eq!(classification.pieces[0].bytes(), response.as_bytes());
        }
    }

    #[test]
    fn once_a_final_response_is_seen_every_later_write_is_a_borrowed_passthrough() {
        let mut classifier = ResponseClassifier::new();
        classifier.classify(b"HTTP/1.1 200 OK\r\n\r\n");
        let classification = classifier.classify(b"HTTP/1.1 100 Continue\r\n\r\n");
        // No classification at all on the fast path: this looks interim but is not treated as such.
        assert_eq!(
            kinds(&classification),
            vec!["write:HTTP/1.1 100 Continue\r\n\r\n"]
        );
        assert!(matches!(
            classification.pieces[0],
            ResponsePiece::Final(Cow::Borrowed(_))
        ));
    }

    #[test]
    fn held_bytes_with_no_header_end_are_flushed_as_a_final_write() {
        // SourceInterimResponseCaptureTest:88-109 asserts the order
        // ["write:HTTP/1.1 100 Cont", "exception", "close"], i.e. the residue is flushed first.
        let mut classifier = ResponseClassifier::new();
        assert!(classifier.classify(b"HTTP/1.1 100 Cont").pieces.is_empty());
        assert_eq!(
            classifier.flush_held().expect("held bytes"),
            b"HTTP/1.1 100 Cont"
        );
        assert_eq!(classifier.flush_held(), None);
    }

    #[test]
    fn an_over_cap_hold_is_flushed_and_asks_for_a_close() {
        let mut classifier = ResponseClassifier::with_hold_limit(32);
        let mut write = b"HTTP/1.1 100 Continue".to_vec();
        write.extend(std::iter::repeat_n(b'x', 64));
        let classification = classifier.classify(&write);
        assert!(classification.hold_overflow);
        assert_eq!(classification.pieces.len(), 1);
        assert_eq!(classification.pieces[0].bytes(), write.as_slice());
        assert_eq!(classifier.held_bytes(), 0);
    }

    #[test]
    fn a_bare_lf_interim_response_is_never_recognised_as_ending() {
        // Java scans for CRLFCRLF only, so this is held and eventually becomes a final write.
        let mut classifier = ResponseClassifier::new();
        let classification = classifier.classify(b"HTTP/1.1 100 Continue\n\n");
        assert!(classification.pieces.is_empty());
        assert_eq!(classifier.held_bytes(), 23);
    }

    #[test]
    fn the_prefix_test_matches_javas_exact_byte_positions() {
        assert!(is_non_switching_informational_response(
            b"HTTP/1.1 100 Continue"
        ));
        assert!(is_non_switching_informational_response(b"HTTP/1.0 199 x"));
        assert!(!is_non_switching_informational_response(
            b"HTTP/1.1 101 Switching"
        ));
        assert!(!is_non_switching_informational_response(b"HTTP/1.1 200 OK"));
        assert!(!is_non_switching_informational_response(b"HTTP/2.0 100 x"));
        assert!(!is_non_switching_informational_response(b"http/1.1 100 x"));
        assert!(!is_non_switching_informational_response(b"HTTP/1.1  100 x"));
        assert!(!is_non_switching_informational_response(b"HTTP/1.1 1x0 x"));
        assert!(!is_non_switching_informational_response(b"HTTP/1.1 10"));
    }

    #[test]
    fn header_end_search_requires_crlfcrlf() {
        assert_eq!(index_after_http_headers(b"ab\r\n\r\ncd"), Some(6));
        assert_eq!(index_after_http_headers(b"ab\n\ncd"), None);
        assert_eq!(index_after_http_headers(b"\r\n\r\n"), Some(4));
        assert_eq!(index_after_http_headers(b"\r\n\r"), None);
    }
}
