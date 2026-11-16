//! Byte-level header insertion (traceability row PROXY-09).
//!
//! Mirrors `HeaderAdderHandler` (`HAH:37-65`): the header lines are inserted **immediately after
//! the first line terminator of the request**, so they become the first headers, ahead of every
//! client header. A read containing no LF is forwarded unchanged and the insertion simply happens
//! in whichever later read first contains one — deferral, not buffering — and a read ending in CR
//! still produces a CR LF when the LF arrives in the next read.
//!
//! Body safety comes from the one-shot latch: once the insertion has happened, every remaining byte
//! of the request is forwarded untouched. Under owner decision OD-3 that latch is cleared only by
//! [`HeaderAdder::begin_request`], never by a response, which is what stops Java's `100 Continue`
//! body-corruption failure (spec §8.3-F1) from being reachable here.

use bytes::Bytes;

use super::AddedHeaderLine;

/// Inserts configured header lines into each request.
#[derive(Debug, Clone)]
pub struct HeaderAdder {
    lines: Vec<AddedHeaderLine>,
    inserted: bool,
    pending_carriage_return: bool,
}

impl HeaderAdder {
    /// Builds an adder.
    pub(crate) fn new(lines: Vec<AddedHeaderLine>) -> Self {
        Self {
            lines,
            inserted: false,
            pending_carriage_return: false,
        }
    }

    /// True when nothing is configured to add.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether the insertion has already happened for the current request.
    #[must_use]
    pub fn has_inserted(&self) -> bool {
        self.inserted
    }

    /// Starts a new request, clearing the latch.
    ///
    /// Unlike Java, the CR-pending flag is cleared too. Java's `useCarriageReturn` is sticky for
    /// the life of the connection (`HAH:45-46`), which is harmless only because a connection does
    /// not normally mix terminators; scoping it to the request removes the dependency on that
    /// assumption.
    pub fn begin_request(&mut self) {
        self.inserted = false;
        self.pending_carriage_return = false;
    }

    /// Processes one chunk, appending the bytes to forward to `out`.
    pub fn process(&mut self, chunk: &Bytes, out: &mut Vec<Bytes>) {
        if self.lines.is_empty() || self.inserted {
            if !chunk.is_empty() {
                out.push(chunk.clone());
            }
            return;
        }
        match chunk.iter().position(|byte| *byte == b'\n') {
            None => {
                if let Some(last) = chunk.last() {
                    self.pending_carriage_return = *last == b'\r';
                    out.push(chunk.clone());
                }
            }
            Some(index) => {
                let carriage_return = if index > 0 {
                    chunk[index - 1] == b'\r'
                } else {
                    self.pending_carriage_return
                };
                out.push(chunk.slice(..=index));
                for line in &self.lines {
                    out.push(if carriage_return {
                        line.with_crlf.clone()
                    } else {
                        line.with_lf.clone()
                    });
                }
                if index + 1 < chunk.len() {
                    out.push(chunk.slice(index + 1..));
                }
                self.inserted = true;
                self.pending_carriage_return = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::{HeaderRewriter, RewriteConfig};

    /// `HeaderAdderHandlerTest.makeMessage` (`:124-130`).
    fn make_message(line_ending: &str, extra_header: &str) -> Vec<u8> {
        format!(
            "GET / HTTP/1.1{line_ending}{extra_header}NICEHeader: v1{line_ending}silLYHeader: yyy{line_ending}{line_ending}"
        )
        .into_bytes()
    }

    fn run(sizes: &[usize], line_ending: &str) -> Vec<u8> {
        let config = RewriteConfig::new()
            .add_header("host", "my.host")
            .expect("valid");
        let mut rewriter = HeaderRewriter::new(&config);
        rewriter.begin_request();
        let message = Bytes::from(make_message(line_ending, ""));
        let mut out = Vec::new();
        let mut offset = 0;
        let mut index = 0;
        while offset < message.len() {
            let size = sizes[index.min(sizes.len() - 1)];
            index += 1;
            let end = offset.saturating_add(size).min(message.len());
            rewriter.rewrite(&message.slice(offset..end), &mut out);
            offset = end;
        }
        out.concat()
    }

    #[test]
    fn the_java_fragment_size_matrix_inserts_after_the_request_line() {
        // HeaderAdderHandlerTest:25-38, :80-102 — sizes {MAX}, {1,1,..}, {8,27,9999}, {8,12,16,999}
        // for both line endings.
        for line_ending in ["\n", "\r\n"] {
            let expected = make_message(line_ending, &format!("host: my.host{line_ending}"));
            for sizes in [
                vec![usize::MAX],
                vec![1],
                vec![8, 27, 9999],
                vec![8, 12, 16, 999],
            ] {
                assert_eq!(
                    run(&sizes, line_ending),
                    expected,
                    "sizes {sizes:?} line ending {line_ending:?}"
                );
            }
        }
    }

    #[test]
    fn a_read_ending_in_cr_still_produces_crlf() {
        let config = RewriteConfig::new()
            .add_header("host", "my.host")
            .expect("valid");
        let mut rewriter = HeaderRewriter::new(&config);
        rewriter.begin_request();
        let mut out = Vec::new();
        rewriter.rewrite(&Bytes::from_static(b"GET / HTTP/1.1\r"), &mut out);
        rewriter.rewrite(&Bytes::from_static(b"\nHost: a\r\n\r\n"), &mut out);
        assert_eq!(
            out.concat(),
            b"GET / HTTP/1.1\r\nhost: my.host\r\nHost: a\r\n\r\n".to_vec()
        );
    }

    #[test]
    fn the_body_is_never_touched_across_repeated_requests() {
        // HeaderAdderHandlerTest:41-78: a body containing CRLFs, a `Host:` line, and another
        // request line, five times, with a full response written between each.
        let body = "line1\r\nHost: fake.value\r\nPOST /oops HTTP/1.1\r\n\r\nEND";
        let request = format!(
            "POST /submit HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let expected = format!(
            "POST /submit HTTP/1.1\r\nhost: my.host\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let config = RewriteConfig::new()
            .add_header("host", "my.host")
            .expect("valid");
        let mut rewriter = HeaderRewriter::new(&config);
        for _ in 0..5 {
            rewriter.begin_request();
            let mut out = Vec::new();
            rewriter.rewrite(&Bytes::copy_from_slice(request.as_bytes()), &mut out);
            assert_eq!(String::from_utf8(out.concat()).expect("utf8"), expected);
        }
    }

    #[test]
    fn an_interim_response_cannot_unlatch_the_insertion() {
        // OD-3 / spec §8.3-F1. In Java, HAH.write resets `insertedHeader` on the interim response,
        // and the first `\n` in the body then splices the header into the body, so the forwarded
        // request no longer matches its own Content-Length. There is no API here that a response
        // can use to do that: only begin_request clears the latch.
        let config = RewriteConfig::new()
            .set_header("host", "my.host")
            .expect("valid");
        let mut rewriter = HeaderRewriter::new(&config);
        rewriter.begin_request();
        let mut out = Vec::new();
        rewriter.rewrite(
            &Bytes::from_static(
                b"POST / HTTP/1.1\r\nHost: client\r\nExpect: 100-continue\r\nContent-Length: 12\r\n\r\n",
            ),
            &mut out,
        );
        // ... the source replies `HTTP/1.1 100 Continue\r\n\r\n` here; nothing happens to state ...
        rewriter.rewrite(&Bytes::from_static(b"a\nb\nc\nd\ne\nf\n"), &mut out);
        let forwarded = out.concat();
        assert!(
            forwarded.ends_with(b"\r\n\r\na\nb\nc\nd\ne\nf\n"),
            "the body must be byte-identical: {:?}",
            String::from_utf8_lossy(&forwarded)
        );
        assert_eq!(
            forwarded,
            b"POST / HTTP/1.1\r\nhost: my.host\r\nExpect: 100-continue\r\nContent-Length: 12\r\n\r\na\nb\nc\nd\ne\nf\n"
                .to_vec()
        );
    }
}
