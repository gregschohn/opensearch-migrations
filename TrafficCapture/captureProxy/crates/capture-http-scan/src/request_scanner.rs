//! Bounded incremental HTTP/1 request framing (traceability row HTTP-06).
//!
//! # Shape of the scan
//!
//! One [`RequestScanner`] belongs to one connection and is fed every client byte in arrival order.
//! It answers two questions and nothing else: *has a request head completed, and should it be
//! captured?* and *has a request message ended?* It never holds the body, never reserialises
//! anything, and never asks its caller to forward bytes other than the ones it was given.
//!
//! The head is scanned **incrementally for the line terminator only**, which is what
//! `HttpObjectDecoder.HeaderParser` does (`HOD:1170-1216`: find the next LF from the read cursor,
//! bounded by the remaining budget). `httparse` is called **exactly once per head**, when the
//! terminating blank line has been seen, so a head arriving one byte per read costs one parse, not
//! one parse per byte. Reparsing a growing fragmented head would be O(n²) over a 10 MiB budget;
//! the Java tests deliberately exercise single-byte reads
//! (`ConditionallyReliableLoggingHttpHandlerTest:766-773`), so that cost is not theoretical.
//!
//! The head buffer is a parse-only side buffer bounded by
//! [`capture_config::RequestLimits::max_header_bytes`]. The body never enters it.
//!
//! # Owner decision OD-1: framing the proxy cannot determine is never forwarded
//!
//! Every [`FramingError`] is terminal for the connection. In Java each of these instead puts
//! Netty's decoder into `BAD_MESSAGE`, which emits no `LastHttpContent`, so
//! `channelFinishedReadingAnHttpMessage` — where the entire capture-before-forward gate lives — is
//! never reached, and `LoggingHttpHandler:506-508` forwards every subsequent byte raw and
//! uncaptured until a 250 MiB or five-minute limit kills the connection (spec §3.7). That includes
//! a request line over 4096 bytes, which is routine for an `OpenSearch` `_search` URI.
//!
//! # Request-line budget
//!
//! Java configures only `setMaxHeaderSize` and leaves Netty's `maxInitialLineLength` at its 4096
//! default (`LHH:100-103`, `HttpDecoderConfig:34`). This scanner uses
//! [`capture_config::RequestLimits::max_request_line_bytes`], which OD-1 requires to be an explicit
//! configured value well above 4096.

use capture_config::{CaptureDirective, HttpStrictness, RequestLimits};

use crate::selected_headers::{
    FORBIDDEN_TRAILER_NAMES, SelectedHeaderNames, SelectedHeaders, trim_ows,
};
use crate::{BodyFraming, CaptureEverything, CapturePredicate, HttpVersion, RequestHead};

/// Slack Netty allows a line to exceed its budget by, so a line whose content exactly fills the
/// budget still fits with its CR LF (`HOD:1176-1178`, "adding 2 to account for both CR and LF").
const LINE_TERMINATOR_SLACK: u64 = 2;

/// What the scanner observed inside one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    /// A request head completed. Reported before any [`ScanEvent::MessageComplete`] for the same
    /// request, and — matching Java, which decodes before it captures (`LHH:476-483`) — before the
    /// caller decides whether to capture the read that carried it.
    HeadComplete(HeadComplete),
    /// A request message ended.
    MessageComplete(MessageComplete),
}

/// A request head completed within the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadComplete {
    /// Index just past the head's last byte, within the slice that was fed.
    pub offset: usize,
    /// Whether the capture policy wants this request captured.
    pub directive: CaptureDirective,
    /// Whether this request is Critical Mutation Traffic (`POST`, `PUT`, `DELETE`, `PATCH`).
    pub critical_mutation: bool,
    /// How the body is framed.
    pub body: BodyFraming,
}

/// A request message ended within the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageComplete {
    /// Index just past the message's last byte, within the slice that was fed.
    pub offset: usize,
    /// `EndOfMessageIndication.firstLineByteLength`: the request-line length **excluding its
    /// CR LF**.
    ///
    /// `LineParser.size` is `endOfSeqIncluded - readerIndex`, where `endOfSeqIncluded` points at
    /// the CR of a CR LF pair (`HOD:1192-1216`). Pinned at `14` for `GET / HTTP/1.1` by
    /// `NettyScanningHttpProxyTest:152-154`.
    pub first_line_byte_length: u32,
    /// `EndOfMessageIndication.headersByteLength`: the sum over header lines of each line's length
    /// **excluding its CR LF**, and **excluding the terminating blank line**.
    ///
    /// An empty line returns early from `HeaderParser.parse` before `size` is increased
    /// (`HOD:1204-1208`), which is why the blank line contributes nothing. An obs-fold continuation
    /// line is a separate `parse` call and therefore *does* contribute its own length. Pinned at
    /// `711` by `NettyScanningHttpProxyTest:152-154`.
    pub headers_byte_length: u32,
}

/// The scanner refused the connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanError {
    /// Framing the proxy could not determine. Terminal under owner decision OD-1: emit a
    /// `connectionException` observation, close, and forward nothing further.
    #[error("malformed HTTP framing: {0}")]
    Framing(#[from] FramingError),
    /// A configured incomplete-request limit was exceeded. Terminal, with no HTTP response — every
    /// Java limit is a bare TCP close (spec §4.2).
    #[error("incomplete-request limit exceeded: {0}")]
    Limit(#[from] LimitViolation),
    /// The scanner already refused this connection and will not look at further bytes.
    #[error("the scanner already refused this connection")]
    AlreadyFaulted,
}

/// A framing condition the proxy refuses to guess at.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    /// A line ended with a bare LF while [`HttpStrictness::Strict`] was in force.
    ///
    /// Netty's production configuration rejects this too
    /// (`InvalidLineSeparatorException`, `HOD:1192-1200`), but Java never maps the rejection to a
    /// limit violation, so the connection becomes a tunnel instead of closing (spec §3.7).
    #[error("a line ended with a bare LF, which strict HTTP line parsing rejects")]
    BareLineFeed,
    /// The request line exceeded [`capture_config::RequestLimits::max_request_line_bytes`].
    #[error("the request line exceeded its {limit}-byte budget")]
    RequestLineTooLong {
        /// The configured budget.
        limit: u64,
    },
    /// The request line is not `method SP request-target SP version`.
    #[error("malformed request line: {reason}")]
    MalformedRequestLine {
        /// Why it was refused.
        reason: &'static str,
    },
    /// The header block could not be parsed.
    ///
    /// Includes two deliberate divergences from Netty, both stricter and both flagged to the owner:
    /// whitespace before a header's colon (Netty folds it into the name; RFC 7230 §3.2.4 mandates
    /// a 400), and control characters in a header value (Netty validates only the handful of names
    /// `PassThruHttpHeaders` retains, so it forwards the rest unchecked).
    #[error("malformed header block: {reason}")]
    MalformedHeaders {
        /// Why it was refused.
        reason: &'static str,
    },
    /// Both `Transfer-Encoding: chunked` and `Content-Length` were present.
    ///
    /// Netty resolves this silently in chunked's favour and deletes the `Content-Length`
    /// (`HOD:817-821`, `HOD:851-854`). OD-1 refuses it: it is the classic request-smuggling
    /// ambiguity, and the proxy must not forward a message whose framing two implementations can
    /// read differently.
    #[error("both Transfer-Encoding: chunked and Content-Length were present")]
    TransferEncodingWithContentLength,
    /// More than one `Content-Length`, or one carrying a comma-separated list.
    ///
    /// OD-1 applies this to every version, dropping Netty's HTTP/1.0-and-earlier exemption
    /// (`HttpUtil.normalizeAndGetContentLength`).
    #[error("more than one Content-Length value was present")]
    DuplicateContentLength,
    /// A `Content-Length` was not a bare non-negative decimal number.
    #[error("Content-Length {value:?} is not a non-negative decimal number")]
    InvalidContentLength {
        /// The offending value, lossily rendered.
        value: String,
    },
    /// A `Transfer-Encoding` was present that does not include `chunked`, with no `Content-Length`
    /// to frame the body.
    ///
    /// Netty treats this as a zero-length body (`HOD:404-412`), so the body bytes are then parsed
    /// as a new request line and the connection degrades into the tunnel of spec §3.7. The framing
    /// is genuinely undeterminable, so OD-1 refuses it.
    #[error("Transfer-Encoding {value:?} does not include chunked and there is no Content-Length")]
    UnsupportedTransferEncoding {
        /// The offending value, lossily rendered.
        value: String,
    },
    /// A chunk-size line was not a hexadecimal number with optional extensions.
    #[error("malformed chunk size: {reason}")]
    InvalidChunkSize {
        /// Why it was refused.
        reason: &'static str,
    },
    /// A chunk size exceeded `i32::MAX`, which Netty cannot represent (`HOD:928-957`).
    #[error("chunk size overflowed the maximum of 2147483647")]
    ChunkSizeOverflow,
    /// The bytes after a chunk's data were not its line terminator.
    #[error("a chunk's data was not followed by its line terminator")]
    InvalidChunkTermination,
    /// A chunk-size or trailer line exceeded its budget.
    #[error("a chunked-framing line exceeded its {limit}-byte budget")]
    ChunkLineTooLong {
        /// The configured budget.
        limit: u64,
    },
    /// A trailer line was malformed, or carried a name trailers may not carry.
    ///
    /// `HOD:891-894` refuses `Content-Length`, `Transfer-Encoding`, and `Trailer` inside trailers.
    #[error("malformed trailer: {reason}")]
    MalformedTrailer {
        /// Why it was refused.
        reason: &'static str,
    },
}

/// A configured incomplete-request limit was exceeded.
///
/// These are Java's limit violations and keep Java's semantics: a bare TCP close with a latched
/// close reason and no `connectionException` observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitViolation {
    /// The accumulated header-block bytes of one request exceeded
    /// [`capture_config::RequestLimits::max_header_bytes`].
    #[error("the request's header bytes exceeded their {limit}-byte budget")]
    HeaderBytes {
        /// The configured budget.
        limit: u64,
    },
    /// The accumulated wire bytes of one request exceeded
    /// [`capture_config::RequestLimits::max_total_bytes`].
    ///
    /// Java checks this **before** the end-of-message reset (`LHH:153-162`), so a read that both
    /// completes a request and pushes it over the limit is dropped rather than forwarded. This
    /// scanner reports the violation before it reports the end of message, preserving that order.
    #[error("the request's total bytes exceeded their {limit}-byte budget")]
    TotalBytes {
        /// The configured budget.
        limit: u64,
    },
}

/// Where in a message the scanner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the request line. Leading control and whitespace bytes are skipped here, which is how
    /// a CR LF left over from a previous message is tolerated (`HOD:1242-1264`).
    AwaitingRequestLine,
    /// Accumulating head lines, looking for the terminating blank line.
    Head,
    /// Framing the body.
    Body,
}

/// Body framing progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyScan {
    /// `Content-Length` framing with `remaining` bytes to go.
    Fixed {
        /// Body bytes not yet seen.
        remaining: u64,
    },
    /// Awaiting a chunk-size line.
    ChunkSize,
    /// Reading one chunk's data.
    ChunkData {
        /// Chunk bytes not yet seen.
        remaining: u64,
    },
    /// Awaiting the CR LF after a chunk's data.
    ChunkTermination {
        /// Whether the CR has been seen.
        carriage_return: bool,
    },
    /// Awaiting trailers and the final blank line.
    Trailers,
}

/// Incremental HTTP/1 request framing for one connection.
///
/// `P` is the capture policy, which is also the authority on which header names the scanner must
/// retain; see [`CapturePredicate`].
#[derive(Debug)]
pub struct RequestScanner<P: CapturePredicate = CaptureEverything> {
    limits: RequestLimits,
    strictness: HttpStrictness,
    predicate: P,
    names: SelectedHeaderNames,

    state: State,
    body: Option<BodyScan>,
    faulted: bool,

    /// The head being accumulated. Cleared, and its allocation reused, at the end of every head.
    head: Vec<u8>,
    /// Scratch used only when a head contains an obs-fold continuation line.
    unfolded: Vec<u8>,
    /// Partial chunk-size or trailer line.
    line: Vec<u8>,

    /// Index in `head` where the line currently being accumulated starts.
    line_start: usize,
    /// Request-line content length, excluding its terminator.
    first_line_len: u32,
    /// Index in `head` where the header block starts, i.e. just past the request line's terminator.
    header_block_start: usize,
    /// Accumulated header-line content lengths, excluding terminators and the blank line.
    headers_len: u64,
    /// Header lines seen, including obs-fold continuation lines.
    header_lines: usize,
    /// Obs-fold continuation lines seen.
    fold_lines: usize,
    /// Trailer bytes seen for the current message, budgeted like header bytes.
    trailer_len: u64,

    /// Wire bytes attributed to the request being assembled.
    request_bytes: u64,
    /// Control and whitespace bytes skipped before the current request line.
    skipped_bytes: u64,
    /// Requests whose message end has been reported.
    requests_completed: u64,
}

impl RequestScanner<CaptureEverything> {
    /// A scanner that captures every request.
    #[must_use]
    pub fn capture_everything(limits: RequestLimits, strictness: HttpStrictness) -> Self {
        Self::new(limits, strictness, CaptureEverything)
    }
}

impl<P: CapturePredicate> RequestScanner<P> {
    /// Builds a scanner for one connection.
    ///
    /// The retained header-name set is taken from the predicate, so a value pattern can never be
    /// configured against a name the scanner discarded.
    #[must_use]
    pub fn new(limits: RequestLimits, strictness: HttpStrictness, predicate: P) -> Self {
        let names = SelectedHeaderNames::new(predicate.selected_header_names());
        Self {
            limits,
            strictness,
            predicate,
            names,
            state: State::AwaitingRequestLine,
            body: None,
            faulted: false,
            head: Vec::new(),
            unfolded: Vec::new(),
            line: Vec::new(),
            line_start: 0,
            first_line_len: 0,
            header_block_start: 0,
            headers_len: 0,
            header_lines: 0,
            fold_lines: 0,
            trailer_len: 0,
            request_bytes: 0,
            skipped_bytes: 0,
            requests_completed: 0,
        }
    }

    /// The capture policy.
    pub fn predicate(&self) -> &P {
        &self.predicate
    }

    /// Wire bytes attributed to the request currently being assembled.
    #[must_use]
    pub fn request_bytes(&self) -> u64 {
        self.request_bytes
    }

    /// Whether a request is partially assembled.
    ///
    /// Java's equivalent is `currentRequestBytes > 0`, which drives the assembly-deadline lifecycle
    /// (`LHH:526-543`, spec §4.4). Bytes skipped before a request line count, exactly as they do in
    /// Java, where the decoder's read cursor has already moved past them.
    #[must_use]
    pub fn is_request_in_progress(&self) -> bool {
        self.request_bytes > 0
    }

    /// How many request messages have ended.
    #[must_use]
    pub fn requests_completed(&self) -> u64 {
        self.requests_completed
    }

    /// Whether the scanner has refused the connection.
    #[must_use]
    pub fn is_faulted(&self) -> bool {
        self.faulted
    }

    /// Feeds one read's bytes and reports the framing boundaries inside it.
    ///
    /// The returned events are in byte order. A read can complete several messages: pipelining is
    /// supported rather than rejected, because owner decision OD-3 requires header rewriting to be
    /// correct for pipelined requests, which is only possible if framing reports every boundary.
    ///
    /// # Errors
    ///
    /// * [`ScanError::Framing`] — terminal under OD-1. The caller must emit a `connectionException`
    ///   observation, close the connection, and forward none of this read.
    /// * [`ScanError::Limit`] — terminal, with the matching close reason and no HTTP response.
    /// * [`ScanError::AlreadyFaulted`] — the scanner was fed more bytes after refusing.
    pub fn scan(&mut self, bytes: &[u8]) -> Result<Vec<ScanEvent>, ScanError> {
        if self.faulted {
            return Err(ScanError::AlreadyFaulted);
        }
        match self.scan_inner(bytes) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.faulted = true;
                Err(error)
            }
        }
    }

    fn scan_inner(&mut self, bytes: &[u8]) -> Result<Vec<ScanEvent>, ScanError> {
        let mut events = Vec::new();
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            match self.state {
                State::AwaitingRequestLine => {
                    cursor += self.skip_leading_control(&bytes[cursor..])?;
                }
                State::Head => {
                    let (consumed, complete) = self.feed_head(&bytes[cursor..])?;
                    cursor += consumed;
                    if let Some(parsed) = complete {
                        let offset = cursor;
                        events.push(ScanEvent::HeadComplete(HeadComplete {
                            offset,
                            directive: parsed.directive,
                            critical_mutation: parsed.critical_mutation,
                            body: parsed.body,
                        }));
                        self.state = State::Body;
                        self.body = Some(match parsed.body {
                            BodyFraming::Empty => BodyScan::Fixed { remaining: 0 },
                            BodyFraming::Fixed(len) => BodyScan::Fixed { remaining: len },
                            BodyFraming::Chunked => BodyScan::ChunkSize,
                        });
                        if self.body == Some(BodyScan::Fixed { remaining: 0 }) {
                            events.push(ScanEvent::MessageComplete(self.finish_message(offset)));
                        }
                    }
                }
                State::Body => {
                    let (consumed, complete) = self.feed_body(&bytes[cursor..])?;
                    cursor += consumed;
                    if complete {
                        events.push(ScanEvent::MessageComplete(self.finish_message(cursor)));
                    }
                }
            }
        }
        Ok(events)
    }

    /// Skips control and whitespace bytes before a request line, bounded like Netty's
    /// `skipControlChars` (`HOD:1242-1264`).
    fn skip_leading_control(&mut self, bytes: &[u8]) -> Result<usize, ScanError> {
        let skipped = bytes
            .iter()
            .position(|byte| !is_control_or_whitespace(*byte))
            .unwrap_or(bytes.len());
        if skipped > 0 {
            self.consume(skipped)?;
            self.skipped_bytes += skipped as u64;
            let limit = self.limits.max_request_line_bytes();
            if self.skipped_bytes > limit {
                return Err(FramingError::RequestLineTooLong { limit }.into());
            }
        }
        if skipped < bytes.len() {
            self.state = State::Head;
        }
        Ok(skipped)
    }

    /// Accumulates head bytes, consuming at most up to and including the head's terminating blank
    /// line. Returns how many bytes were consumed and, when the head completed, its parse.
    fn feed_head(&mut self, bytes: &[u8]) -> Result<(usize, Option<HeadParse>), ScanError> {
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let remaining = &bytes[cursor..];
            match remaining.iter().position(|byte| *byte == b'\n') {
                None => {
                    self.append_head(remaining)?;
                    cursor += remaining.len();
                    self.check_partial_line_budget()?;
                    return Ok((cursor, None));
                }
                Some(index) => {
                    let through_lf = &remaining[..=index];
                    self.append_head(through_lf)?;
                    cursor += through_lf.len();
                    if self.finish_head_line()? {
                        let parsed = self.parse_head()?;
                        return Ok((cursor, Some(parsed)));
                    }
                }
            }
        }
        Ok((cursor, None))
    }

    fn append_head(&mut self, bytes: &[u8]) -> Result<(), ScanError> {
        self.consume(bytes.len())?;
        self.head.extend_from_slice(bytes);
        Ok(())
    }

    /// Enforces the in-progress line's budget before its terminator has arrived, so a line that
    /// will never end cannot grow the head buffer without bound.
    fn check_partial_line_budget(&self) -> Result<(), ScanError> {
        let pending = (self.head.len() - self.line_start) as u64;
        if self.line_start == 0 {
            let limit = self.limits.max_request_line_bytes();
            if pending > limit + LINE_TERMINATOR_SLACK {
                return Err(FramingError::RequestLineTooLong { limit }.into());
            }
        } else {
            let limit = self.limits.max_header_bytes();
            if self.headers_len + pending > limit + LINE_TERMINATOR_SLACK {
                return Err(LimitViolation::HeaderBytes { limit }.into());
            }
        }
        Ok(())
    }

    /// Accounts for a head line that has just been terminated. Returns true when that line was the
    /// terminating blank line, i.e. the head is complete.
    fn finish_head_line(&mut self) -> Result<bool, ScanError> {
        let line = &self.head[self.line_start..];
        let terminator = self.terminator_len(line)?;
        let content_len = (line.len() - terminator) as u64;

        if self.line_start == 0 {
            // The request line. An empty one cannot happen: leading CR LF is skipped before the
            // head starts, so the first head byte is not a terminator.
            let limit = self.limits.max_request_line_bytes();
            if content_len > limit {
                return Err(FramingError::RequestLineTooLong { limit }.into());
            }
            self.first_line_len = u32::try_from(content_len)
                .map_err(|_| FramingError::RequestLineTooLong { limit })?;
            self.header_block_start = self.head.len();
            self.line_start = self.head.len();
            return Ok(false);
        }

        if content_len == 0 {
            // The terminating blank line. It contributes nothing to headersByteLength.
            return Ok(true);
        }

        let limit = self.limits.max_header_bytes();
        if self.headers_len + content_len > limit {
            return Err(LimitViolation::HeaderBytes { limit }.into());
        }
        self.headers_len += content_len;
        self.header_lines += 1;
        if matches!(self.head[self.line_start], b' ' | b'\t') {
            self.fold_lines += 1;
        }
        self.line_start = self.head.len();
        Ok(false)
    }

    /// The length of `line`'s terminator: 2 for CR LF, 1 for a bare LF when strictness allows it.
    ///
    /// DEVIATION: this is applied to **every** HTTP/1 line — request line, header line, chunk-size
    /// line, chunk-data terminator, and trailer. Netty gates only the start line and header lines
    /// on `strictLineParsing`, while the chunk-size line (`HOD:476`) and the chunk-data terminator
    /// (`HOD:513-524`) are unconditionally strict. Applying the one configured grammar uniformly is
    /// simpler to explain and to configure, and it leaves the default — strict, which is what the
    /// production image runs — byte-for-byte identical to Netty. Only the explicitly opted-in
    /// lenient mode differs, and there it is the more useful behaviour: a client that emits bare-LF
    /// request lines emits bare-LF chunk lines too.
    fn terminator_len(&self, line: &[u8]) -> Result<usize, FramingError> {
        debug_assert_eq!(line.last(), Some(&b'\n'), "a finished line ends with LF");
        if line.len() >= 2 && line[line.len() - 2] == b'\r' {
            return Ok(2);
        }
        if self.strictness.allows_bare_lf() {
            Ok(1)
        } else {
            Err(FramingError::BareLineFeed)
        }
    }

    /// Parses the completed head: request line by hand, header block with one `httparse` call.
    fn parse_head(&mut self) -> Result<HeadParse, ScanError> {
        // The buffers are taken out so the parse can borrow them while `self` is borrowed
        // immutably for the predicate; both allocations are restored and reused.
        let head = std::mem::take(&mut self.head);
        let mut unfolded = std::mem::take(&mut self.unfolded);
        let result = self.parse_head_borrowed(&head, &mut unfolded);
        self.head = head;
        self.head.clear();
        self.unfolded = unfolded;
        self.unfolded.clear();
        self.line_start = 0;
        result.map_err(ScanError::from)
    }

    fn parse_head_borrowed(
        &self,
        head: &[u8],
        unfolded: &mut Vec<u8>,
    ) -> Result<HeadParse, FramingError> {
        let request_line = &head[..self.first_line_len as usize];
        let (method, target, version_token) = split_request_line(request_line)?;
        let version =
            HttpVersion::parse(version_token).ok_or(FramingError::MalformedRequestLine {
                reason: "the version is not <name>/<major>.<minor>",
            })?;

        let block = &head[self.header_block_start..];
        let block = if self.fold_lines > 0 {
            unfold(block, unfolded);
            unfolded.as_slice()
        } else {
            block
        };

        // Sized from the line count the incremental scan already produced, so `httparse` can never
        // report TooManyHeaders and the parse happens exactly once.
        let capacity = self.header_lines - self.fold_lines;
        let mut storage = vec![httparse::EMPTY_HEADER; capacity];
        let headers = match httparse::parse_headers(block, &mut storage) {
            Ok(httparse::Status::Complete((_, headers))) => headers,
            Ok(httparse::Status::Partial) => {
                return Err(FramingError::MalformedHeaders {
                    reason: "the header block ended without a blank line",
                });
            }
            Err(error) => {
                // DEVIATION: two header-block checks here are stricter than Netty, and both are
                // flagged to the owner rather than decided quietly. (1) Whitespace before a colon:
                // Netty accepts it for requests and folds it into the name, so `"Content-Length :
                // 5"` becomes a header named `"Content-Length "` that framing ignores while some
                // origin servers would honour it — a request-smuggling shape, and RFC 7230 §3.2.4
                // mandates a 400. (2) Control characters in a header value: Netty validates only
                // the handful of names `PassThruHttpHeaders` retains, so it forwards the rest
                // unchecked. Both are refused.
                return Err(FramingError::MalformedHeaders {
                    reason: describe_httparse_error(error),
                });
            }
        };

        let selected = SelectedHeaders::collect(&self.names, headers);
        let body = resolve_body_framing(&selected)?;
        let head_view = RequestHead::new(method, target, version, &selected);
        Ok(HeadParse {
            directive: self.predicate.evaluate(&head_view),
            critical_mutation: head_view.is_critical_mutation(),
            body,
        })
    }

    /// Frames body bytes. Returns how many were consumed and whether the message ended.
    fn feed_body(&mut self, bytes: &[u8]) -> Result<(usize, bool), ScanError> {
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let remaining = &bytes[cursor..];
            match self.body.expect("a body scan exists in State::Body") {
                BodyScan::Fixed { remaining: left } => {
                    let take = usize::try_from(left)
                        .unwrap_or(usize::MAX)
                        .min(remaining.len());
                    self.consume(take)?;
                    cursor += take;
                    let left = left - take as u64;
                    self.body = Some(BodyScan::Fixed { remaining: left });
                    if left == 0 {
                        return Ok((cursor, true));
                    }
                }
                BodyScan::ChunkSize => {
                    let Some(consumed) = self.feed_framing_line(remaining)? else {
                        cursor += remaining.len();
                        return Ok((cursor, false));
                    };
                    cursor += consumed;
                    let terminator = self.terminator_len(&self.line)?;
                    let size = parse_chunk_size(&self.line[..self.line.len() - terminator])?;
                    self.line.clear();
                    self.body = Some(if size == 0 {
                        BodyScan::Trailers
                    } else {
                        BodyScan::ChunkData { remaining: size }
                    });
                }
                BodyScan::ChunkData { remaining: left } => {
                    let take = usize::try_from(left)
                        .unwrap_or(usize::MAX)
                        .min(remaining.len());
                    self.consume(take)?;
                    cursor += take;
                    let left = left - take as u64;
                    self.body = Some(if left == 0 {
                        BodyScan::ChunkTermination {
                            carriage_return: false,
                        }
                    } else {
                        BodyScan::ChunkData { remaining: left }
                    });
                }
                BodyScan::ChunkTermination { carriage_return } => {
                    self.consume(1)?;
                    cursor += 1;
                    let byte = remaining[0];
                    if carriage_return {
                        if byte != b'\n' {
                            return Err(FramingError::InvalidChunkTermination.into());
                        }
                        self.body = Some(BodyScan::ChunkSize);
                    } else if byte == b'\r' {
                        self.body = Some(BodyScan::ChunkTermination {
                            carriage_return: true,
                        });
                    } else if byte == b'\n' && self.strictness.allows_bare_lf() {
                        self.body = Some(BodyScan::ChunkSize);
                    } else {
                        return Err(FramingError::InvalidChunkTermination.into());
                    }
                }
                BodyScan::Trailers => {
                    let Some(consumed) = self.feed_framing_line(remaining)? else {
                        cursor += remaining.len();
                        return Ok((cursor, false));
                    };
                    cursor += consumed;
                    let terminator = self.terminator_len(&self.line)?;
                    let content_len = self.line.len() - terminator;
                    if content_len == 0 {
                        self.line.clear();
                        return Ok((cursor, true));
                    }
                    let limit = self.limits.max_header_bytes();
                    self.trailer_len += content_len as u64;
                    if self.trailer_len > limit {
                        return Err(LimitViolation::HeaderBytes { limit }.into());
                    }
                    check_trailer_line(&self.line[..content_len])?;
                    self.line.clear();
                }
            }
        }
        Ok((cursor, false))
    }

    /// Accumulates a chunk-size or trailer line into `self.line`. Returns how many bytes were
    /// consumed once the line is complete, or `None` when it is still incomplete.
    fn feed_framing_line(&mut self, bytes: &[u8]) -> Result<Option<usize>, ScanError> {
        let limit = self.limits.max_request_line_bytes();
        match bytes.iter().position(|byte| *byte == b'\n') {
            None => {
                self.consume(bytes.len())?;
                self.line.extend_from_slice(bytes);
                if self.line.len() as u64 > limit + LINE_TERMINATOR_SLACK {
                    return Err(FramingError::ChunkLineTooLong { limit }.into());
                }
                Ok(None)
            }
            Some(index) => {
                let through_lf = &bytes[..=index];
                self.consume(through_lf.len())?;
                self.line.extend_from_slice(through_lf);
                if self.line.len() as u64 > limit + LINE_TERMINATOR_SLACK {
                    return Err(FramingError::ChunkLineTooLong { limit }.into());
                }
                Ok(Some(through_lf.len()))
            }
        }
    }

    /// Attributes `count` wire bytes to the request being assembled and enforces the total-byte
    /// budget. Called before any state transition that could report an end of message, which is
    /// what makes a request that completes *and* overflows get dropped rather than forwarded
    /// (spec §4.3).
    fn consume(&mut self, count: usize) -> Result<(), ScanError> {
        self.request_bytes += count as u64;
        let limit = self.limits.max_total_bytes();
        if self.request_bytes > limit {
            return Err(LimitViolation::TotalBytes { limit }.into());
        }
        Ok(())
    }

    fn finish_message(&mut self, offset: usize) -> MessageComplete {
        let complete = MessageComplete {
            offset,
            first_line_byte_length: self.first_line_len,
            headers_byte_length: u32::try_from(self.headers_len)
                .expect("headersByteLength is bounded by the header budget, which is at most i32"),
        };
        self.state = State::AwaitingRequestLine;
        self.body = None;
        self.head.clear();
        self.line.clear();
        self.unfolded.clear();
        self.line_start = 0;
        self.first_line_len = 0;
        self.header_block_start = 0;
        self.headers_len = 0;
        self.header_lines = 0;
        self.fold_lines = 0;
        self.trailer_len = 0;
        self.request_bytes = 0;
        self.skipped_bytes = 0;
        self.requests_completed += 1;
        complete
    }
}

/// What a completed head told the scanner.
#[derive(Debug, Clone, Copy)]
struct HeadParse {
    directive: CaptureDirective,
    critical_mutation: bool,
    body: BodyFraming,
}

/// `Character.isISOControl(b) || isWhitespace(b)`, Netty's `SKIP_CONTROL_CHARS_BYTES`
/// (`HOD:1281-1287`).
fn is_control_or_whitespace(byte: u8) -> bool {
    byte <= 0x20 || byte == 0x7f
}

/// Netty's "SP lenient" separator set for the request line (`HOD:1081-1116`): SP, HTAB, VT, FF, CR.
fn is_sp_lenient(byte: u8) -> bool {
    matches!(byte, b' ' | 0x09 | 0x0b | 0x0c | 0x0d)
}

/// Method, request-target, and version token, borrowed from the request line.
type RequestLineFields<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Splits a request line into method, request-target, and version token.
///
/// Mirrors `HttpObjectDecoder.splitInitialLine` (`HOD:961-1000`): separators are SP-lenient, and a
/// request line ending in a control or whitespace byte is refused outright.
fn split_request_line(line: &[u8]) -> Result<RequestLineFields<'_>, FramingError> {
    if line.is_empty() {
        return Err(FramingError::MalformedRequestLine {
            reason: "the request line is empty",
        });
    }
    if is_control_or_whitespace(line[line.len() - 1]) {
        return Err(FramingError::MalformedRequestLine {
            reason: "the request line ends with a control or whitespace byte",
        });
    }
    let mut fields = [&line[0..0]; 3];
    let mut cursor = 0usize;
    for field in &mut fields {
        while cursor < line.len() && is_sp_lenient(line[cursor]) {
            cursor += 1;
        }
        let start = cursor;
        while cursor < line.len() && !is_sp_lenient(line[cursor]) {
            cursor += 1;
        }
        if start == cursor {
            return Err(FramingError::MalformedRequestLine {
                reason: "the request line does not have three fields",
            });
        }
        *field = &line[start..cursor];
    }
    while cursor < line.len() && is_sp_lenient(line[cursor]) {
        cursor += 1;
    }
    if cursor != line.len() {
        return Err(FramingError::MalformedRequestLine {
            reason: "the request line has more than three fields",
        });
    }
    Ok((fields[0], fields[1], fields[2]))
}

/// Rewrites a header block so obs-fold continuation lines become part of the previous value.
///
/// Netty accepts a folded header and joins the parts with exactly one space, after trimming the
/// continuation (`HOD:755-761`). `httparse` rejects a folded block outright, so a folded head is
/// unfolded into scratch first. This runs only when the incremental scan already saw a
/// continuation line, so a request without folding pays nothing.
fn unfold(block: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(block.len());
    let mut rest = block;
    while !rest.is_empty() {
        let Some(lf) = rest.iter().position(|byte| *byte == b'\n') else {
            out.extend_from_slice(rest);
            return;
        };
        let (line, remainder) = rest.split_at(lf + 1);
        let continuation = remainder
            .first()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'));
        if continuation {
            // Drop this line's terminator, trim its trailing OWS, and let the continuation's
            // leading whitespace collapse into the single joining space.
            let content_end = if line.len() >= 2 && line[line.len() - 2] == b'\r' {
                line.len() - 2
            } else {
                line.len() - 1
            };
            out.extend_from_slice(trim_trailing_ows(&line[..content_end]));
            out.push(b' ');
            let skip = remainder
                .iter()
                .position(|byte| !matches!(byte, b' ' | b'\t'))
                .unwrap_or(remainder.len());
            rest = &remainder[skip..];
        } else {
            out.extend_from_slice(line);
            rest = remainder;
        }
    }
}

fn trim_trailing_ows(value: &[u8]) -> &[u8] {
    let end = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(0, |index| index + 1);
    &value[..end]
}

fn describe_httparse_error(error: httparse::Error) -> &'static str {
    match error {
        httparse::Error::HeaderName => {
            "a header name is not a token, or whitespace preceded its colon"
        }
        httparse::Error::HeaderValue => "a header value contains a control character",
        httparse::Error::NewLine => "a line terminator is misplaced",
        httparse::Error::Token => "a token contains an illegal character",
        httparse::Error::TooManyHeaders => "more header lines than the scan counted",
        httparse::Error::Status => "a status line appeared where headers were expected",
        httparse::Error::Version => "a version appeared where headers were expected",
    }
}

/// Derives body framing, refusing the combinations OD-1 declares undeterminable.
fn resolve_body_framing(headers: &SelectedHeaders<'_>) -> Result<BodyFraming, FramingError> {
    let chunked = headers.is_transfer_encoding_chunked();
    let content_lengths = headers.content_length();
    if chunked {
        if !content_lengths.is_empty() {
            return Err(FramingError::TransferEncodingWithContentLength);
        }
        return Ok(BodyFraming::Chunked);
    }
    if content_lengths.len() > 1 {
        return Err(FramingError::DuplicateContentLength);
    }
    if let Some(raw) = content_lengths.first() {
        let value = trim_ows(raw);
        // Netty treats a comma-separated list in a single field as a duplicate
        // (`HttpUtil.normalizeAndGetContentLength`), which OD-1 keeps and makes terminal.
        if value.contains(&b',') {
            return Err(FramingError::DuplicateContentLength);
        }
        return Ok(BodyFraming::Fixed(parse_content_length(value)?));
    }
    if !headers.transfer_encoding().is_empty() {
        // DEVIATION: Netty frames this as a zero-length body (`HOD:404-412`), so the body bytes are
        // then read as a new request line and the connection degrades into the uncaptured tunnel of
        // spec §3.7. The framing is genuinely undeterminable, so OD-1's rule applies: refuse rather
        // than guess. A `Transfer-Encoding` that is not chunked but comes WITH a `Content-Length`
        // keeps Netty's behaviour and is framed by the `Content-Length` above.
        let value = headers.transfer_encoding().first().unwrap_or(b"");
        return Err(FramingError::UnsupportedTransferEncoding {
            value: String::from_utf8_lossy(value).into_owned(),
        });
    }
    Ok(BodyFraming::Empty)
}

/// Parses a `Content-Length`, refusing a sign or any non-digit.
///
/// Netty requires the first character to be an ASCII digit specifically to block `+` and `-`,
/// citing the Squid request-smuggling advisory, then parses and requires non-negative.
fn parse_content_length(value: &[u8]) -> Result<u64, FramingError> {
    let invalid = || FramingError::InvalidContentLength {
        value: String::from_utf8_lossy(value).into_owned(),
    };
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let mut length: u64 = 0;
    for digit in value {
        length = length
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(u64::from(digit - b'0')))
            .ok_or_else(invalid)?;
    }
    Ok(length)
}

/// Parses a chunk-size line, mirroring `HttpObjectDecoder.getChunkSize` (`HOD:928-957`).
///
/// Leading whitespace is skipped; hex nibbles accumulate; the first non-hex byte accepts the value
/// so far when it is `;` (a chunk extension) or a control/whitespace byte, and is otherwise an
/// error. An empty size is an error, and so is a value above `i32::MAX`, which Netty's `int`
/// accumulator cannot hold.
fn parse_chunk_size(line: &[u8]) -> Result<u64, FramingError> {
    let start = line
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(line.len());
    let digits = &line[start..];
    if digits.is_empty() {
        return Err(FramingError::InvalidChunkSize {
            reason: "the chunk size is empty",
        });
    }
    let mut value: u64 = 0;
    for (index, byte) in digits.iter().enumerate() {
        let Some(nibble) = hex_nibble(*byte) else {
            if *byte == b';' || is_control_or_whitespace(*byte) {
                if index == 0 {
                    return Err(FramingError::InvalidChunkSize {
                        reason: "the chunk size is empty",
                    });
                }
                return Ok(value);
            }
            return Err(FramingError::InvalidChunkSize {
                reason: "the chunk size contains a non-hexadecimal character",
            });
        };
        value = value * 16 + u64::from(nibble);
        if value > i32::MAX as u64 {
            return Err(FramingError::ChunkSizeOverflow);
        }
    }
    Ok(value)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Validates one trailer line.
///
/// A continuation line is accepted (`HOD:878-888`). Otherwise the line must be `name: value` with a
/// token name, and the three names `HOD:891-894` refuses inside trailers are refused here too.
fn check_trailer_line(line: &[u8]) -> Result<(), FramingError> {
    if matches!(line[0], b' ' | b'\t') {
        return Ok(());
    }
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return Err(FramingError::MalformedTrailer {
            reason: "no colon found",
        });
    };
    let name = &line[..colon];
    if name.is_empty() {
        return Err(FramingError::MalformedTrailer {
            reason: "the trailer name is empty",
        });
    }
    if !name.iter().all(|byte| is_token_byte(*byte)) {
        return Err(FramingError::MalformedTrailer {
            reason: "the trailer name is not a token, or whitespace preceded its colon",
        });
    }
    let Ok(name) = std::str::from_utf8(name) else {
        return Err(FramingError::MalformedTrailer {
            reason: "the trailer name is not a token",
        });
    };
    if FORBIDDEN_TRAILER_NAMES
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
    {
        return Err(FramingError::MalformedTrailer {
            reason: "Content-Length, Transfer-Encoding, and Trailer may not appear in trailers",
        });
    }
    Ok(())
}

/// RFC 7230 §3.2.6 `tchar`.
pub(crate) fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_split_like_netty() {
        let (method, target, version) = split_request_line(b"GET /a?b=c HTTP/1.1").expect("splits");
        assert_eq!(method, b"GET");
        assert_eq!(target, b"/a?b=c");
        assert_eq!(version, b"HTTP/1.1");

        // SP lenient: HTAB, VT, FF, and CR are separators too.
        let (method, target, version) = split_request_line(b"GET\t/a\x0bHTTP/1.1").expect("splits");
        assert_eq!(
            (method, target, version),
            (&b"GET"[..], &b"/a"[..], &b"HTTP/1.1"[..])
        );

        for line in [
            &b""[..],
            b"GET",
            b"GET /",
            b"GET / HTTP/1.1 extra",
            b"GET / HTTP/1.1 ",
        ] {
            assert!(
                split_request_line(line).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn chunk_sizes_follow_nettys_getchunksize() {
        assert_eq!(parse_chunk_size(b"24").expect("hex"), 0x24);
        assert_eq!(parse_chunk_size(b"0").expect("zero"), 0);
        assert_eq!(parse_chunk_size(b"  1f").expect("leading ws"), 0x1f);
        assert_eq!(parse_chunk_size(b"1F").expect("upper"), 0x1f);
        // Extensions are parsed and ignored.
        assert_eq!(parse_chunk_size(b"a;name=value").expect("ext"), 10);
        assert_eq!(parse_chunk_size(b"a ").expect("trailing ws"), 10);
        assert_eq!(
            parse_chunk_size(b"7fffffff").expect("i32::MAX"),
            0x7fff_ffff
        );

        assert!(matches!(
            parse_chunk_size(b"80000000"),
            Err(FramingError::ChunkSizeOverflow)
        ));
        for line in [&b""[..], b"   ", b";ext", b"xyz", b"1g"] {
            assert!(
                parse_chunk_size(line).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn content_lengths_refuse_signs_and_junk() {
        assert_eq!(parse_content_length(b"0").expect("zero"), 0);
        assert_eq!(parse_content_length(b"007").expect("leading zeros"), 7);
        for value in [&b""[..], b"+1", b"-1", b"1x", b"0x10", b" 1"] {
            assert!(
                parse_content_length(value).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[test]
    fn unfolding_joins_with_exactly_one_space() {
        let mut out = Vec::new();
        unfold(b"Host: a \r\n   b\r\nX: y\r\n\r\n", &mut out);
        assert_eq!(out, b"Host: a b\r\nX: y\r\n\r\n");
        unfold(b"Host: a\n\tb\nX: y\n\n", &mut out);
        assert_eq!(out, b"Host: a b\nX: y\n\n");
    }

    #[test]
    fn trailers_refuse_the_three_framing_names() {
        check_trailer_line(b"X-Checksum: abc").expect("ordinary trailer");
        check_trailer_line(b"  continuation").expect("obs-fold continuation");
        for line in [
            &b"Content-Length: 1"[..],
            b"content-length: 1",
            b"Transfer-Encoding: chunked",
            b"Trailer: X",
            b"no-colon",
            b": value",
            b"Bad Name: v",
        ] {
            assert!(
                check_trailer_line(line).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(line)
            );
        }
    }
}
