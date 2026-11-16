//! Post-capture header rewriting (traceability rows PROXY-09 and PROXY-10).
//!
//! # Where this sits
//!
//! Capture records the **original** client bytes; the source receives the **rewritten** bytes. That
//! asymmetry is structural in Java — the capture handler is installed before the rewrite handlers
//! are inserted after it (`PCI:159` versus `CP:964`/`CP:969`) — and it is the reason this module
//! exists as a separate step that runs on bytes the capture layer has already seen.
//!
//! Rewriting is byte-level. Nothing is reserialised, bodies are never touched, and a fragmented
//! head is handled without buffering more than the longest configured header name.
//!
//! # Owner decision OD-3: stateless per request
//!
//! Java resets both rewrite handlers on **every outbound write** (`HAH:74-81`, `HRH:184-193`), and
//! every response chunk is such a write (`BH:25`). The handlers' own Javadoc states the unenforced
//! precondition — "as long as a response is written for each request... the handler will reset
//! itself correctly" — and this proxy does not guarantee it. Three reachable failures follow
//! (spec §8.3):
//!
//! * an `Expect: 100-continue` interim response mid-request un-latches the adder, so the next `\n`
//!   in the **body** splices a header into the body and the forwarded request no longer matches its
//!   own `Content-Length`;
//! * an interim response arriving while the remover holds a partially matched header name flushes
//!   that prefix to the source and forwards the rest of the line verbatim, so a header configured
//!   for removal **leaks**;
//! * two pipelined requests with no response between them get no rewriting at all.
//!
//! Here, rewrite state is scoped to the request being parsed. [`HeaderRewriter::begin_request`] is
//! driven by the **request** framing — the caller calls it at the end-of-message boundary the
//! scanner reports — and the response direction cannot reach this module at all.

pub mod add;
pub mod remove;

use bytes::Bytes;

pub use add::HeaderAdder;
pub use remove::HeaderRemover;

/// A header rewrite could not be configured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RewriteError {
    /// The header name is empty or is not an RFC 7230 token.
    ///
    /// Java's remover instead requires the caller to pass `name + ":"` and rejects anything not
    /// ending in a colon (`HRH:36-41`), which is why `"h: "` is refused there. Here the colon is
    /// appended internally and the name itself is validated.
    #[error("header name {name:?} is empty or is not a token")]
    InvalidHeaderName {
        /// The offending name.
        name: String,
    },
    /// The header value contains a byte that would break out of its header line.
    ///
    /// Refusing CR, LF, and NUL at configuration time is what makes `--setHeader` incapable of
    /// injecting a second header or a body into every forwarded request.
    #[error("header value for {name:?} contains a control character")]
    InvalidHeaderValue {
        /// The name whose value was refused.
        name: String,
    },
}

/// One header line to insert, pre-rendered with each line terminator.
#[derive(Debug, Clone)]
pub(crate) struct AddedHeaderLine {
    pub(crate) with_crlf: Bytes,
    pub(crate) with_lf: Bytes,
}

/// What to rewrite in each forwarded request.
#[derive(Debug, Clone, Default)]
pub struct RewriteConfig {
    /// Lowercased `name:` prefixes to remove, including the colon.
    removals: Vec<Box<[u8]>>,
    additions: Vec<AddedHeaderLine>,
}

impl RewriteConfig {
    /// An empty configuration, which rewrites nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `--setHeader name value`: remove every existing `name` header, then insert `name: value`.
    ///
    /// Java always pairs each `--setHeader` with both a remover and an adder (`CP:968-971`), which
    /// is what makes it a *set* rather than an *add*; the adder alone never inspects header names
    /// and would happily duplicate one.
    ///
    /// # Errors
    ///
    /// [`RewriteError::InvalidHeaderName`] or [`RewriteError::InvalidHeaderValue`].
    pub fn set_header(self, name: &str, value: &str) -> Result<Self, RewriteError> {
        self.remove_header(name)?.add_header(name, value)
    }

    /// Removes every header named `name` from each forwarded request.
    ///
    /// # Errors
    ///
    /// [`RewriteError::InvalidHeaderName`].
    pub fn remove_header(mut self, name: &str) -> Result<Self, RewriteError> {
        validate_name(name)?;
        let mut prefix = name.to_ascii_lowercase().into_bytes();
        prefix.push(b':');
        let prefix = prefix.into_boxed_slice();
        if !self.removals.contains(&prefix) {
            self.removals.push(prefix);
        }
        Ok(self)
    }

    /// Inserts `name: value` into each forwarded request.
    ///
    /// # Errors
    ///
    /// [`RewriteError::InvalidHeaderName`] or [`RewriteError::InvalidHeaderValue`].
    pub fn add_header(mut self, name: &str, value: &str) -> Result<Self, RewriteError> {
        validate_name(name)?;
        if value
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0 || byte == 0x7f)
        {
            return Err(RewriteError::InvalidHeaderValue {
                name: name.to_owned(),
            });
        }
        // DEVIATION: several added headers are inserted in configuration order. Java installs one
        // handler per `--setHeader` and each inserts after the first line terminator it sees, so
        // the last handler in the chain ends up first and the forwarded order is reversed. Header
        // order carries no meaning for distinct names (RFC 7230 §3.2.2 constrains order only among
        // same-name fields), and `--setHeader` pairs each addition with a removal of the same name,
        // so no same-name pair can arise.
        let line = format!("{name}: {value}");
        let mut with_crlf = line.clone().into_bytes();
        with_crlf.extend_from_slice(b"\r\n");
        let mut with_lf = line.into_bytes();
        with_lf.push(b'\n');
        self.additions.push(AddedHeaderLine {
            with_crlf: Bytes::from(with_crlf),
            with_lf: Bytes::from(with_lf),
        });
        Ok(self)
    }

    /// True when nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.removals.is_empty() && self.additions.is_empty()
    }

    /// The lowercased `name:` prefixes that will be removed.
    #[must_use]
    pub fn removals(&self) -> &[Box<[u8]>] {
        &self.removals
    }

    /// How many header lines will be inserted.
    #[must_use]
    pub fn addition_count(&self) -> usize {
        self.additions.len()
    }
}

fn validate_name(name: &str) -> Result<(), RewriteError> {
    if name.is_empty() || !name.bytes().all(crate::request_scanner::is_token_byte) {
        return Err(RewriteError::InvalidHeaderName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Removes and then adds headers in each forwarded request, byte for byte.
///
/// Removal runs first and addition second, matching the Java pipeline order where the whole remove
/// family precedes the whole add family (spec §2.3).
#[derive(Debug)]
pub struct HeaderRewriter {
    remover: HeaderRemover,
    adder: HeaderAdder,
    scratch: Vec<Bytes>,
}

impl HeaderRewriter {
    /// Builds a rewriter for one connection.
    #[must_use]
    pub fn new(config: &RewriteConfig) -> Self {
        Self {
            remover: HeaderRemover::new(config.removals.clone()),
            adder: HeaderAdder::new(config.additions.clone()),
            scratch: Vec::new(),
        }
    }

    /// True when this rewriter forwards every chunk unchanged.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.remover.is_noop() && self.adder.is_noop()
    }

    /// Starts a new request.
    ///
    /// Call this at the end-of-message boundary the request scanner reports, and **only** there.
    /// Owner decision OD-3 forbids coupling rewrite state to the response direction.
    pub fn begin_request(&mut self) {
        self.remover.begin_request();
        self.adder.begin_request();
    }

    /// Rewrites one chunk of client bytes, appending the bytes to forward to `out`.
    ///
    /// The output is a list of slices so nothing is copied except the few bytes of a header name
    /// that straddled a read boundary. Concatenating `out` across every call of one request yields
    /// the rewritten request.
    pub fn rewrite(&mut self, chunk: &Bytes, out: &mut Vec<Bytes>) {
        if self.is_noop() {
            if !chunk.is_empty() {
                out.push(chunk.clone());
            }
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        self.remover.process(chunk, &mut scratch);
        for segment in scratch.drain(..) {
            self.adder.process(&segment, out);
        }
        self.scratch = scratch;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite_all(config: &RewriteConfig, requests: &[&[u8]], fragment: usize) -> Vec<Vec<u8>> {
        let mut rewriter = HeaderRewriter::new(config);
        let mut outputs = Vec::new();
        for request in requests {
            rewriter.begin_request();
            let mut out = Vec::new();
            let bytes = Bytes::copy_from_slice(request);
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + fragment).min(bytes.len());
                rewriter.rewrite(&bytes.slice(offset..end), &mut out);
                offset = end;
            }
            outputs.push(out.concat());
        }
        outputs
    }

    #[test]
    fn set_header_removes_then_adds() {
        let config = RewriteConfig::new()
            .set_header("host", "my.host")
            .expect("valid");
        let request = b"GET / HTTP/1.1\r\nHost: client.value\r\nAccept: */*\r\n\r\nbody";
        let expected = b"GET / HTTP/1.1\r\nhost: my.host\r\nAccept: */*\r\n\r\nbody";
        for fragment in [1usize, 3, 7, 1024] {
            assert_eq!(
                rewrite_all(&config, &[request], fragment)[0],
                expected.to_vec(),
                "fragment size {fragment}"
            );
        }
    }

    #[test]
    fn an_empty_config_is_a_noop_and_shares_the_original_buffer() {
        let config = RewriteConfig::new();
        let mut rewriter = HeaderRewriter::new(&config);
        assert!(rewriter.is_noop());
        let chunk = Bytes::from_static(b"GET / HTTP/1.1\r\n\r\n");
        let mut out = Vec::new();
        rewriter.rewrite(&chunk, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], chunk);
    }

    #[test]
    fn names_and_values_are_validated() {
        assert!(matches!(
            RewriteConfig::new().remove_header(""),
            Err(RewriteError::InvalidHeaderName { .. })
        ));
        assert!(matches!(
            RewriteConfig::new().remove_header("host:"),
            Err(RewriteError::InvalidHeaderName { .. })
        ));
        assert!(matches!(
            RewriteConfig::new().remove_header("h ost"),
            Err(RewriteError::InvalidHeaderName { .. })
        ));
        // Java rejects "h: " because it does not end in a colon; here the equivalent input is a
        // name with a space, refused for the same reason.
        assert!(matches!(
            RewriteConfig::new().add_header("x", "a\r\nInjected: yes"),
            Err(RewriteError::InvalidHeaderValue { .. })
        ));
        assert!(matches!(
            RewriteConfig::new().add_header("x", "a\nb"),
            Err(RewriteError::InvalidHeaderValue { .. })
        ));
        RewriteConfig::new()
            .set_header("X-new-header", "insignificant value")
            .expect("ordinary values are accepted");
    }

    #[test]
    fn a_duplicate_removal_name_is_configured_once() {
        let config = RewriteConfig::new()
            .remove_header("Host")
            .expect("valid")
            .remove_header("host")
            .expect("valid");
        assert_eq!(config.removals().len(), 1);
    }

    #[test]
    fn pipelined_requests_are_each_rewritten() {
        // OD-3. Java rewrites only the first of two pipelined requests, because its reset is driven
        // by an outbound response that never comes.
        let config = RewriteConfig::new()
            .set_header("host", "my.host")
            .expect("valid");
        let request = b"GET / HTTP/1.1\r\nHost: client\r\n\r\n";
        let outputs = rewrite_all(&config, &[request, request], 5);
        for output in outputs {
            assert_eq!(output, b"GET / HTTP/1.1\r\nhost: my.host\r\n\r\n".to_vec());
        }
    }
}
