//! Byte-level header removal (traceability row PROXY-10).
//!
//! Mirrors `HeaderRemoverHandler` (`HRH:112-175`) in behaviour, with two deliberate corrections.
//!
//! # What is preserved from Java
//!
//! * Matching is ASCII case-insensitive and prefix-anchored on the name **including its colon**, so
//!   `host:` removes `Host:`, `HOST:`, and `HosT:` but leaves `hoststays:` and `hos:` alone —
//!   exactly the cases `HeaderRemoverHandlerTest` pins with 1000 seeded random fragmentations.
//! * The whole matched line is dropped, up to and including its LF.
//! * Both `\n` and `\r\n` work, and the surviving terminators are preserved verbatim.
//! * Once the header block ends, the handler is a pure pass-through, so a body containing a
//!   `host:` line or another request line is never modified.
//! * A fragmented head is handled by buffering only the partially matched name prefix, bounded by
//!   the longest configured name, and on mismatch those bytes are emitted **before** any byte of
//!   the current read.
//!
//! # The two corrections
//!
//! 1. **Reset is driven by request framing, not by an outbound response** (owner decision OD-3).
//!    Java's `write`-triggered `resetState` flushes a partially matched name to the source and then
//!    forwards the rest of the line verbatim, so an interim response arriving mid-head makes the
//!    header configured for removal **leak** (spec §8.3-F2). Nothing in this module is reachable
//!    from the response direction.
//! 2. **An obs-fold continuation of a removed header is removed with it.** Java drops the header
//!    line, then sees the continuation's leading space, fails to match it, and copies it verbatim,
//!    leaving a dangling folded line attached to whatever header precedes it (spec §8.2, last
//!    bullet). No Java test covers that.

use bytes::Bytes;

/// Where in a request the remover is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Copying the request line, which is never matched against: Java reaches its matching branch
    /// only after the first line terminator (`HRH:120-141`).
    RequestLine,
    /// At the first byte of a header line.
    LineStart,
    /// Matching a header name against the configured names.
    Matching,
    /// Copying the rest of a header line that did not match.
    Copying,
    /// Dropping the rest of a header line that did match.
    Dropping,
    /// The header block has ended; everything else is forwarded untouched.
    AfterHead,
}

/// What one byte did to the match in progress.
enum MatchStep {
    /// A configured name, colon included, is now fully matched.
    Matched,
    /// Still a prefix of at least one configured name.
    Partial,
    /// No configured name can match this line.
    Mismatch,
}

/// Removes configured headers from each request.
#[derive(Debug, Clone)]
pub struct HeaderRemover {
    /// Lowercased `name:` prefixes, colon included.
    names: Vec<Box<[u8]>>,
    state: State,
    /// Indices of names still matching the prefix consumed so far.
    candidates: Vec<usize>,
    /// How many bytes of the prefix have matched.
    matched: usize,
    /// The matched prefix bytes, populated only when a name straddles a read boundary. Bounded by
    /// the longest configured name.
    held: Vec<u8>,
    /// Whether the header line that just ended was dropped, so its continuation lines must be too.
    dropped_previous_line: bool,
}

impl HeaderRemover {
    /// Builds a remover.
    pub(crate) fn new(names: Vec<Box<[u8]>>) -> Self {
        Self {
            names,
            state: State::RequestLine,
            candidates: Vec::new(),
            matched: 0,
            held: Vec::new(),
            dropped_previous_line: false,
        }
    }

    /// True when nothing is configured to remove.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.names.is_empty()
    }

    /// Bytes currently held because a configured name straddled a read boundary.
    ///
    /// Bounded by the longest configured name, which is the whole reason this is safe to hold.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.held.len()
    }

    /// Starts a new request.
    ///
    /// # Panics
    ///
    /// Panics if a partially matched name is still held, which can only happen if a request was
    /// declared complete in the middle of a header line — a framing impossibility, since a complete
    /// head ends with a blank line.
    pub fn begin_request(&mut self) {
        assert!(
            self.held.is_empty(),
            "a request cannot end in the middle of a header name"
        );
        self.state = State::RequestLine;
        self.candidates.clear();
        self.matched = 0;
        self.dropped_previous_line = false;
    }

    /// Processes one chunk, appending the bytes to forward to `out`.
    #[allow(
        clippy::too_many_lines,
        reason = "one byte-level state machine reads best whole"
    )]
    pub fn process(&mut self, chunk: &Bytes, out: &mut Vec<Bytes>) {
        if self.names.is_empty() || self.state == State::AfterHead {
            if !chunk.is_empty() {
                out.push(chunk.clone());
            }
            return;
        }

        // Start of an open run of bytes to copy out of this chunk.
        let mut segment_start: Option<usize> = None;
        // Where the name currently being matched started in this chunk.
        let mut match_start = 0usize;
        let mut cursor = 0usize;

        while cursor < chunk.len() {
            match self.state {
                State::RequestLine | State::Copying => {
                    if segment_start.is_none() {
                        segment_start = Some(cursor);
                    }
                    match chunk[cursor..].iter().position(|byte| *byte == b'\n') {
                        Some(index) => {
                            cursor += index + 1;
                            self.state = State::LineStart;
                            self.dropped_previous_line = false;
                        }
                        None => cursor = chunk.len(),
                    }
                }
                State::Dropping => {
                    flush(out, chunk, &mut segment_start, cursor);
                    match chunk[cursor..].iter().position(|byte| *byte == b'\n') {
                        Some(index) => {
                            cursor += index + 1;
                            self.state = State::LineStart;
                            self.dropped_previous_line = true;
                        }
                        None => cursor = chunk.len(),
                    }
                }
                State::LineStart => {
                    let byte = chunk[cursor];
                    if byte == b'\n' || byte == b'\r' {
                        // The terminating blank line. Netty's remover latches on either byte in
                        // this position (`HRH:129-130`), so `\n\n`, `\r\n\r\n`, and `\n\r` all end
                        // the header block. The blank line and everything after it are forwarded
                        // verbatim.
                        if segment_start.is_none() {
                            segment_start = Some(cursor);
                        }
                        cursor = chunk.len();
                        self.state = State::AfterHead;
                    } else if matches!(byte, b' ' | b'\t') {
                        // An obs-fold continuation line belongs to the previous header, so it
                        // shares that header's fate.
                        //
                        // DEVIATION: Java drops the matched header line, then fails to match the
                        // continuation's leading space and copies it verbatim, leaving a dangling
                        // folded line attached to whatever header precedes it (spec §8.2, last
                        // bullet). No Java test covers it. Removing a header must remove all of it.
                        self.state = if self.dropped_previous_line {
                            State::Dropping
                        } else {
                            State::Copying
                        };
                    } else {
                        self.state = State::Matching;
                        self.candidates.clear();
                        self.candidates.extend(0..self.names.len());
                        self.matched = 0;
                        self.held.clear();
                        match_start = cursor;
                    }
                }
                State::Matching => {
                    match self.match_step(chunk[cursor]) {
                        MatchStep::Matched => {
                            // Close the copy run before the matched name, then drop the line.
                            flush(out, chunk, &mut segment_start, match_start);
                            self.held.clear();
                            cursor += 1;
                            self.state = State::Dropping;
                        }
                        MatchStep::Partial => cursor += 1,
                        MatchStep::Mismatch => {
                            // Bytes held from earlier reads are emitted first; no byte of this
                            // chunk can precede them, because `held` is only ever populated when a
                            // chunk ran out mid-name.
                            if !self.held.is_empty() {
                                out.push(Bytes::copy_from_slice(&self.held));
                                self.held.clear();
                            }
                            if segment_start.is_none() {
                                segment_start = Some(match_start);
                            }
                            self.state = State::Copying;
                        }
                    }
                }
                State::AfterHead => {
                    if segment_start.is_none() {
                        segment_start = Some(cursor);
                    }
                    cursor = chunk.len();
                }
            }
        }

        if self.state == State::Matching {
            // Carry the partially matched prefix into the next read.
            flush(out, chunk, &mut segment_start, match_start);
            self.held.extend_from_slice(&chunk[match_start..]);
        } else {
            flush(out, chunk, &mut segment_start, chunk.len());
        }
    }

    fn match_step(&mut self, byte: u8) -> MatchStep {
        let lowered = byte.to_ascii_lowercase();
        let matched = self.matched;
        let names = &self.names;
        self.candidates
            .retain(|index| names[*index].get(matched) == Some(&lowered));
        if self.candidates.is_empty() {
            return MatchStep::Mismatch;
        }
        self.matched += 1;
        let matched = self.matched;
        if self
            .candidates
            .iter()
            .any(|index| names[*index].len() == matched)
        {
            MatchStep::Matched
        } else {
            MatchStep::Partial
        }
    }
}

/// Closes an open copy run at `end`.
fn flush(out: &mut Vec<Bytes>, chunk: &Bytes, segment_start: &mut Option<usize>, end: usize) {
    if let Some(start) = segment_start.take()
        && end > start
    {
        out.push(chunk.slice(start..end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::{HeaderRewriter, RewriteConfig};

    fn remover(names: &[&str]) -> HeaderRewriter {
        let mut config = RewriteConfig::new();
        for name in names {
            config = config.remove_header(name).expect("valid");
        }
        HeaderRewriter::new(&config)
    }

    fn run_sizes(message: &str, sizes: &[usize]) -> String {
        let mut rewriter = remover(&["host"]);
        rewriter.begin_request();
        let bytes = Bytes::copy_from_slice(message.as_bytes());
        let mut out = Vec::new();
        let mut offset = 0;
        let mut index = 0;
        while offset < bytes.len() {
            let size = sizes[index.min(sizes.len() - 1)].max(1);
            index += 1;
            let end = offset.saturating_add(size).min(bytes.len());
            rewriter.rewrite(&bytes.slice(offset..end), &mut out);
            offset = end;
        }
        String::from_utf8(out.concat()).expect("utf8")
    }

    /// `HeaderRemoverHandlerTest.makeInterlacedMessage` (`:186-194`).
    fn interlaced(with_hosts: bool, line_ending: &str) -> String {
        let host_one = if with_hosts {
            format!("HOST: begone{line_ending}")
        } else {
            String::new()
        };
        let host_two = if with_hosts {
            format!("HosT: begone{line_ending}")
        } else {
            String::new()
        };
        format!(
            "GET / HTTP/1.1{line_ending}hoststays: v1{line_ending}{host_one}different: v2{line_ending}{host_two}keeper: v3{line_ending}{line_ending}"
        )
    }

    /// `HeaderRemoverHandlerTest.makeConsecutiveMessage` (`:206-218`).
    fn consecutive(with_hosts: bool, line_ending: &str) -> String {
        let hosts = |pair: [&str; 2]| {
            if with_hosts {
                format!("{}{line_ending}{}{line_ending}", pair[0], pair[1])
            } else {
                String::new()
            }
        };
        format!(
            "GET / HTTP/1.1{line_ending}hoststays: a1{line_ending}different: b2{line_ending}{}e2: c3{line_ending}hos: d4{line_ending}{}X: Y{line_ending}{line_ending}",
            hosts(["HOST: strike", "HosT: begone"]),
            hosts(["HOST: foo", "HosT: bar"]),
        )
    }

    /// `HeaderRemoverHandlerTest.makeThinMessage` (`:201-209`).
    fn thin(with_hosts: bool, line_ending: &str) -> String {
        let host_one = if with_hosts {
            format!("HOST: b{line_ending}")
        } else {
            String::new()
        };
        let host_two = if with_hosts {
            format!("HosT: e{line_ending}")
        } else {
            String::new()
        };
        format!("G{line_ending}h: a{line_ending}{host_one}d: c{line_ending}{host_two}{line_ending}")
    }

    #[test]
    fn the_java_fragment_size_matrix_removes_only_exact_names() {
        for line_ending in ["\n", "\r\n"] {
            for maker in [interlaced, consecutive, thin] {
                let source = maker(true, line_ending);
                let expected = maker(false, line_ending);
                for sizes in [
                    vec![usize::MAX],
                    vec![1],
                    vec![8, 22, 22, 22, 22, 9999],
                    vec![8, 27, 9999],
                ] {
                    assert_eq!(
                        run_sizes(&source, &sizes),
                        expected,
                        "sizes {sizes:?} line ending {line_ending:?} message {source:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn similarly_named_headers_survive() {
        let output = run_sizes(
            "GET / HTTP/1.1\r\nhoststays: v1\r\nhos: d4\r\nhostx: y\r\nhost: gone\r\n\r\n",
            &[1],
        );
        assert_eq!(
            output,
            "GET / HTTP/1.1\r\nhoststays: v1\r\nhos: d4\r\nhostx: y\r\n\r\n"
        );
    }

    #[test]
    fn a_single_byte_message_with_no_terminator_passes_through() {
        // HeaderRemoverHandlerTest.trivialSingleByte.
        assert_eq!(run_sizes("A", &[1]), "A");
    }

    #[test]
    fn the_body_is_never_rewritten_even_when_it_contains_the_header() {
        // HeaderRemoverHandlerTest:65-117 and TestHeaderRewrites.testBodyDoesntRewrite.
        let body = "line1\r\nHost: fake.value\r\nPOST /oops HTTP/1.1\r\n\r\nEND";
        let request = format!(
            "POST /submit HTTP/1.1\r\nHost: example.com\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let expected = format!(
            "POST /submit HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        for sizes in [vec![usize::MAX], vec![1], vec![7]] {
            assert_eq!(run_sizes(&request, &sizes), expected, "sizes {sizes:?}");
        }
    }

    #[test]
    fn an_interim_response_cannot_make_a_removed_header_leak() {
        // OD-3 / spec §8.3-F2: read 1 ends mid-name, a response is written, read 2 carries the
        // rest. Java flushes "Hos" to the source and then forwards "t: secret", so the source sees
        // `Host: secret`. Here the held prefix survives the response untouched.
        let mut rewriter = remover(&["host"]);
        rewriter.begin_request();
        let mut out = Vec::new();
        rewriter.rewrite(&Bytes::from_static(b"GET / HTTP/1.1\r\nHos"), &mut out);
        // ... `HTTP/1.1 100 Continue\r\n\r\n` is written back to the client here ...
        rewriter.rewrite(
            &Bytes::from_static(b"t: secret\r\nAccept: */*\r\n\r\n"),
            &mut out,
        );
        let forwarded = String::from_utf8(out.concat()).expect("utf8");
        assert_eq!(forwarded, "GET / HTTP/1.1\r\nAccept: */*\r\n\r\n");
        assert!(!forwarded.contains("secret"));
    }

    #[test]
    fn an_obs_folded_continuation_of_a_removed_header_is_removed_too() {
        // Java leaves the continuation line attached to the previous header; no Java test covers
        // it. The task and OD-3 require removing it.
        for sizes in [vec![usize::MAX], vec![1], vec![4]] {
            assert_eq!(
                run_sizes(
                    "GET / HTTP/1.1\r\nKeep: v\r\nHost: a\r\n continued\r\n\tmore\r\nAccept: */*\r\n\r\n",
                    &sizes
                ),
                "GET / HTTP/1.1\r\nKeep: v\r\nAccept: */*\r\n\r\n",
                "sizes {sizes:?}"
            );
        }
    }

    #[test]
    fn an_obs_folded_continuation_of_a_kept_header_survives() {
        assert_eq!(
            run_sizes(
                "GET / HTTP/1.1\r\nKeep: a\r\n continued\r\nHost: x\r\n\r\n",
                &[1]
            ),
            "GET / HTTP/1.1\r\nKeep: a\r\n continued\r\n\r\n"
        );
    }

    #[test]
    fn several_names_are_removed_in_one_pass() {
        let mut rewriter = remover(&["host", "x-forwarded-for"]);
        rewriter.begin_request();
        let mut out = Vec::new();
        rewriter.rewrite(
            &Bytes::from_static(
                b"GET / HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 1.2.3.4\r\nKeep: yes\r\n\r\n",
            ),
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out.concat()).expect("utf8"),
            "GET / HTTP/1.1\r\nKeep: yes\r\n\r\n"
        );
    }

    #[test]
    fn the_held_prefix_is_bounded_by_the_longest_configured_name() {
        let mut rewriter = remover(&["host"]);
        rewriter.begin_request();
        let mut out = Vec::new();
        // Feed one byte at a time through a long non-matching header; nothing accumulates.
        let message =
            Bytes::from_static(b"GET / HTTP/1.1\r\nhoststays: aaaaaaaaaaaaaaaaaaaa\r\n\r\n");
        for index in 0..message.len() {
            rewriter.rewrite(&message.slice(index..=index), &mut out);
        }
        assert_eq!(
            String::from_utf8(out.concat()).expect("utf8"),
            "GET / HTTP/1.1\r\nhoststays: aaaaaaaaaaaaaaaaaaaa\r\n\r\n"
        );
    }
}
