//! Client to source: scan, capture, gate, rewrite, forward (traceability row PROXY-08).
//!
//! # The order is the contract
//!
//! Java decodes first and captures second (`LHH:476-483`), and that single ordering is what
//! produces the three-way capture distinction: a request whose suppression is known before any byte
//! was committed leaves **no trace at all**, while one whose head straddled reads gets a
//! `requestDropped` marker. [`RequestPump::process`] reproduces it exactly:
//!
//! 1. the suppression state is advanced for the new read (`LHH:124-130`);
//! 2. the scanner is fed the chunk, which is where the capture predicate fires;
//! 3. the read is captured, or not, according to the state the predicate just left behind;
//! 4. each completed message commits its end-of-message indicator;
//! 5. the chunk is rewritten into the bytes to forward, split at message boundaries so rewriting is
//!    stateless per request (owner decision OD-3).
//!
//! This type performs **no I/O**. It returns the bytes to forward and whether the capture-before-
//! forward gate must run, so the caller can hold those bytes back until publication is durable.
//! That is the whole point of the split: the hold is visible in the control flow rather than buried
//! in a callback.

use bytes::Bytes;
use capture_config::{CaptureDirective, HttpStrictness, RequestLimits};
use capture_http_scan::request_scanner::ScanError;
use capture_http_scan::{
    CapturePredicate, HeaderRewriter, RequestScanner, RequestSuppression, RewriteConfig, ScanEvent,
    SuppressionTransition,
};

use crate::{CaptureAbort, CaptureSink};

/// What one client read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestStep {
    /// Messages whose framing completed in this read.
    pub messages_completed: u32,
    /// Messages that completed **and** were captured, i.e. that committed an end-of-message
    /// indicator. This is how many `requestForwarded` observations the write owes.
    pub captured_requests_completed: u32,
    /// Whether a captured Critical Mutation Traffic request completed, so the bytes must not reach
    /// the source until publication is durably acknowledged.
    pub gate_required: bool,
    /// Whether a `requestDropped` observation was emitted for this read.
    pub emitted_drop_marker: bool,
}

/// The pump refused to continue.
#[derive(Debug, thiserror::Error)]
pub enum RequestPumpError {
    /// Framing or a limit. Terminal; map it with [`crate::CloseReason::from`].
    #[error(transparent)]
    Scan(#[from] ScanError),
    /// Required capture failed terminally.
    #[error(transparent)]
    Capture(#[from] CaptureAbort),
}

/// Scans, captures, and rewrites one connection's client bytes.
#[derive(Debug)]
pub struct RequestPump<P: CapturePredicate> {
    scanner: RequestScanner<P>,
    rewriter: HeaderRewriter,
    suppression: RequestSuppression,
    /// Heads whose message end has not been seen yet, in order.
    ///
    /// A head's capture decision belongs to **that** head, not to whatever the connection's state
    /// is when the message happens to end: with two requests in one read, the second head's
    /// decision would otherwise retroactively capture the first request's end of message. Java has
    /// a single `currentRequest` field and a single `haveParsedFullRequest` flag, so a second
    /// request in the same read clobbers the first's decoder result and only one end of message is
    /// emitted — part of the half-supported pipelining the spec flags at §13-D8. Queueing is
    /// strictly closer to the protocol document, which wants one indicator per request.
    pending: std::collections::VecDeque<PendingMessage>,
}

/// A head waiting for its message end.
#[derive(Debug, Clone, Copy)]
struct PendingMessage {
    /// Whether the capture policy wanted this request captured.
    captured: bool,
    /// Whether it is Critical Mutation Traffic.
    critical_mutation: bool,
}

impl<P: CapturePredicate> RequestPump<P> {
    /// Builds a pump for one connection.
    #[must_use]
    pub fn new(
        limits: RequestLimits,
        strictness: HttpStrictness,
        predicate: P,
        rewrite: &RewriteConfig,
    ) -> Self {
        let mut rewriter = HeaderRewriter::new(rewrite);
        rewriter.begin_request();
        Self {
            scanner: RequestScanner::new(limits, strictness, predicate),
            rewriter,
            suppression: RequestSuppression::new(),
            pending: std::collections::VecDeque::new(),
        }
    }

    /// Whether a request is partially assembled, which drives the assembly deadline (spec §4.4).
    #[must_use]
    pub fn is_request_in_progress(&self) -> bool {
        self.scanner.is_request_in_progress()
    }

    /// How many request messages have been framed.
    #[must_use]
    pub fn requests_completed(&self) -> u64 {
        self.scanner.requests_completed()
    }

    /// The suppression state, which the response pump needs in order to honour
    /// `IGNORE_RESPONSE`.
    #[must_use]
    pub fn suppression(&self) -> &RequestSuppression {
        &self.suppression
    }

    /// Called when a response has been fully handled, so the next read resumes capture.
    ///
    /// Java flips `IGNORE_RESPONSE` back to `CAPTURE` on the next inbound read
    /// (`LHH:124-130`); [`RequestPump::process`] does that itself, so this exists only for callers
    /// that need the state advanced without a read.
    pub fn before_read(&mut self) {
        self.suppression.before_read();
    }

    /// Processes one client read.
    ///
    /// Appends the bytes to forward to `out`. They must not be written to the source until the gate
    /// has run, when [`RequestStep::gate_required`] is set.
    ///
    /// # Errors
    ///
    /// * [`RequestPumpError::Scan`] — framing or a limit. Owner decision OD-1 makes framing
    ///   terminal: close, and forward nothing from this read.
    /// * [`RequestPumpError::Capture`] — required capture failed terminally; forward nothing.
    pub fn process<K: CaptureSink>(
        &mut self,
        sink: &mut K,
        at: std::time::SystemTime,
        chunk: &Bytes,
        out: &mut Vec<Bytes>,
    ) -> Result<RequestStep, RequestPumpError> {
        self.suppression.before_read();

        // Decode first: the capture predicate fires inside this call.
        let events = self.scanner.scan(chunk)?;

        // Capture second, against the state the predicate just left behind.
        let mut step = RequestStep::default();
        let mut transition = SuppressionTransition::Unchanged;
        for event in &events {
            if let ScanEvent::HeadComplete(head) = event {
                let applied = self.suppression.apply_directive(head.directive);
                if applied != SuppressionTransition::Unchanged {
                    transition = applied;
                }
                if head.directive != CaptureDirective::Capture {
                    sink.observe_capture_suppressed();
                }
                self.pending.push_back(PendingMessage {
                    captured: head.directive == CaptureDirective::Capture,
                    critical_mutation: head.critical_mutation,
                });
            }
        }

        sink.observe_client_bytes(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let capturing = sink.should_capture();
        if capturing && transition == SuppressionTransition::SuppressedAfterCommittedPrefix {
            // A prefix of this request is already in the capture stream, so the stream needs the
            // marker that tells the replayer to discard it. The read that triggered the decision is
            // itself not captured.
            sink.request_dropped(at)?;
            step.emitted_drop_marker = true;
        }
        if capturing && self.suppression.should_capture_request() && !chunk.is_empty() {
            sink.capture_read(at, chunk)?;
            self.suppression.note_read_captured();
        }

        let mut rewritten_upto = 0usize;
        for event in &events {
            let ScanEvent::MessageComplete(message) = event else {
                continue;
            };
            step.messages_completed += 1;
            sink.observe_request_complete();

            // Rewrite up to the boundary, then start a new request. Driven by the *request*
            // framing, never by a response: owner decision OD-3.
            self.rewriter
                .rewrite(&chunk.slice(rewritten_upto..message.offset), out);
            rewritten_upto = message.offset;
            self.rewriter.begin_request();

            // The decision this message's own head produced, not the connection's current state.
            let pending = self.pending.pop_front().unwrap_or(PendingMessage {
                captured: false,
                critical_mutation: false,
            });
            if capturing && pending.captured {
                sink.set_end_of_message_lengths(
                    message.first_line_byte_length,
                    message.headers_byte_length,
                );
                sink.commit_end_of_message(at)?;
                step.captured_requests_completed += 1;
                if pending.critical_mutation {
                    step.gate_required = true;
                }
            }
            self.suppression.finish_request();
        }
        if rewritten_upto < chunk.len() {
            self.rewriter.rewrite(&chunk.slice(rewritten_upto..), out);
        }

        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::tests::RecordingSink;
    use capture_http_scan::SuppressionPolicy;

    const SMALL_POST: &str = concat!(
        "POST / HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Content-Type: application/x-www-form-urlencoded\r\n",
        "Content-Length: 16\r\n",
        "\r\n",
        "FAKE_UPLOAD_DATA",
    );
    const HEALTH_CHECK: &str = concat!(
        "POST / HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "User-Agent: uploader\r\n",
        "Content-Length: 27\r\n",
        "\r\n",
        "field1=value1&field2=value2",
    );

    fn now() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH
    }

    fn pump() -> RequestPump<SuppressionPolicy> {
        RequestPump::new(
            RequestLimits::default(),
            HttpStrictness::Strict,
            SuppressionPolicy::builder()
                .header("user-Agent", ".*uploader.*")
                .expect("valid pattern")
                .build(),
            &RewriteConfig::new(),
        )
    }

    /// Feeds `bytes` in fragments and returns the forwarded bytes plus the per-read steps.
    fn drive(
        pump: &mut RequestPump<SuppressionPolicy>,
        sink: &mut RecordingSink,
        bytes: &[u8],
        fragment: usize,
    ) -> (Vec<u8>, Vec<RequestStep>) {
        let bytes = Bytes::copy_from_slice(bytes);
        let mut forwarded = Vec::new();
        let mut steps = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let end = offset.saturating_add(fragment).min(bytes.len());
            let mut out = Vec::new();
            let step = pump
                .process(sink, now(), &bytes.slice(offset..end), &mut out)
                .expect("well formed");
            for segment in out {
                forwarded.extend_from_slice(&segment);
            }
            steps.push(step);
            offset = end;
        }
        (forwarded, steps)
    }

    #[test]
    fn a_captured_mutation_commits_its_end_of_message_and_demands_the_gate() {
        // RequestForwardedCaptureTest:28-42: after the request the capture stream holds
        // [read, eom] and nothing else; the forward is reported by the write, not here.
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let (forwarded, steps) = drive(&mut pump, &mut sink, SMALL_POST.as_bytes(), usize::MAX);
        assert_eq!(sink.events(), vec!["read".to_owned(), "eom".to_owned()]);
        assert_eq!(forwarded, SMALL_POST.as_bytes());
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].captured_requests_completed, 1);
        assert!(steps[0].gate_required, "POST is Critical Mutation Traffic");
    }

    #[test]
    fn only_the_read_that_completes_a_request_commits_an_end_of_message() {
        // RequestForwardedCaptureTest:45-60: head-only read yields [read]; the body read then
        // yields [read, eom].
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let head = "POST /thing HTTP/1.1\r\nContent-Length: 2\r\n\r\n";
        let mut out = Vec::new();
        let step = pump
            .process(
                &mut sink,
                now(),
                &Bytes::from_static(head.as_bytes()),
                &mut out,
            )
            .expect("well formed");
        assert_eq!(sink.events(), vec!["read".to_owned()]);
        assert_eq!(step.captured_requests_completed, 0);
        assert!(!step.gate_required);

        let step = pump
            .process(&mut sink, now(), &Bytes::from_static(b"ab"), &mut out)
            .expect("well formed");
        assert_eq!(
            sink.events(),
            vec!["read".to_owned(), "read".to_owned(), "eom".to_owned()]
        );
        assert_eq!(step.captured_requests_completed, 1);
        assert!(step.gate_required);
    }

    #[test]
    fn a_non_mutating_request_is_captured_without_the_gate() {
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let (_, steps) = drive(
            &mut pump,
            &mut sink,
            b"GET /thing HTTP/1.1\r\nHost: source\r\n\r\n",
            usize::MAX,
        );
        assert_eq!(sink.events(), vec!["read".to_owned(), "eom".to_owned()]);
        assert_eq!(steps[0].captured_requests_completed, 1);
        assert!(!steps[0].gate_required, "GET is not gated");
    }

    #[test]
    fn a_suppressed_request_known_in_one_read_leaves_no_trace() {
        // Spec §5.1(b): the whole head arrives in the read that triggers the predicate, so nothing
        // at all is emitted — not even a drop marker.
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let (forwarded, steps) = drive(&mut pump, &mut sink, HEALTH_CHECK.as_bytes(), usize::MAX);
        assert!(sink.events().is_empty(), "{:?}", sink.events());
        assert!(!steps[0].emitted_drop_marker);
        assert_eq!(steps[0].captured_requests_completed, 0);
        assert!(
            !steps[0].gate_required,
            "a suppressed request is never gated"
        );
        assert_eq!(
            forwarded,
            HEALTH_CHECK.as_bytes(),
            "every byte still reaches the source verbatim"
        );
    }

    #[test]
    fn a_suppressed_request_whose_head_straddled_reads_emits_one_drop_marker() {
        // Spec §5.1(c), pinned by testThatHealthCheckCaptureCanBeSuppressed(singleBytes=true):
        // the captured prefix stops at the previous read, and the triggering read is not captured.
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let (forwarded, steps) = drive(&mut pump, &mut sink, HEALTH_CHECK.as_bytes(), 1);
        let events = sink.events();
        assert!(
            events.contains(&"dropped".to_owned()),
            "expected a drop marker in {events:?}"
        );
        assert_eq!(events.iter().filter(|event| *event == "dropped").count(), 1);
        assert!(
            !events.contains(&"eom".to_owned()),
            "a suppressed request has no end of message"
        );
        assert_eq!(
            events.iter().position(|event| event == "dropped"),
            Some(events.len() - 1),
            "nothing is captured after the marker"
        );
        assert_eq!(
            steps.iter().filter(|step| step.emitted_drop_marker).count(),
            1
        );
        assert_eq!(forwarded, HEALTH_CHECK.as_bytes());
    }

    #[test]
    fn suppression_is_scoped_to_one_exchange() {
        // The health check is dropped; the following POST on the same connection is captured.
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let stream = format!("{HEALTH_CHECK}{SMALL_POST}");
        let (forwarded, _) = drive(&mut pump, &mut sink, stream.as_bytes(), usize::MAX);
        assert_eq!(forwarded, stream.as_bytes());
        // The dropped request contributed nothing; the POST contributed a read and an eom.
        assert_eq!(sink.events(), vec!["read".to_owned(), "eom".to_owned()]);
        assert_eq!(pump.requests_completed(), 2);
    }

    #[test]
    fn a_process_that_stopped_capturing_still_forwards_every_byte() {
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        sink.stop_capturing();
        let (forwarded, steps) = drive(&mut pump, &mut sink, SMALL_POST.as_bytes(), 3);
        assert!(sink.events().is_empty());
        assert_eq!(forwarded, SMALL_POST.as_bytes());
        assert!(
            steps.iter().all(|step| !step.gate_required),
            "an uncaptured request cannot be gated on its own publication"
        );
    }

    #[test]
    fn a_framing_error_forwards_nothing_from_that_read() {
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        let mut out = Vec::new();
        let error = pump
            .process(
                &mut sink,
                now(),
                &Bytes::from_static(
                    b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx",
                ),
                &mut out,
            )
            .expect_err("duplicate Content-Length is terminal under OD-1");
        assert!(matches!(error, RequestPumpError::Scan(_)));
        assert!(out.is_empty(), "nothing may be forwarded: {out:?}");
        assert_eq!(
            crate::CloseReason::from(match &error {
                RequestPumpError::Scan(scan) => scan,
                RequestPumpError::Capture(_) => unreachable!(),
            }),
            crate::CloseReason::MalformedFraming
        );
    }

    #[test]
    fn a_terminal_capture_failure_stops_before_anything_is_forwarded() {
        let mut pump = pump();
        let mut sink = RecordingSink::new();
        sink.fail_next();
        let mut out = Vec::new();
        let error = pump
            .process(
                &mut sink,
                now(),
                &Bytes::from_static(b"GET / HTTP/1.1\r\n\r\n"),
                &mut out,
            )
            .expect_err("the sink refused");
        assert!(matches!(error, RequestPumpError::Capture(_)));
        assert!(out.is_empty());
    }

    #[test]
    fn rewriting_applies_to_every_pipelined_request() {
        let mut pump = RequestPump::new(
            RequestLimits::default(),
            HttpStrictness::Strict,
            SuppressionPolicy::permit_everything(),
            &RewriteConfig::new()
                .set_header("host", "my.host")
                .expect("valid"),
        );
        let mut sink = RecordingSink::new();
        let request = "GET / HTTP/1.1\r\nHost: client\r\n\r\n";
        let stream = format!("{request}{request}");
        for fragment in [1usize, 5, usize::MAX] {
            let mut pump_for_fragment = RequestPump::new(
                RequestLimits::default(),
                HttpStrictness::Strict,
                SuppressionPolicy::permit_everything(),
                &RewriteConfig::new()
                    .set_header("host", "my.host")
                    .expect("valid"),
            );
            let (forwarded, _) = drive(
                &mut pump_for_fragment,
                &mut sink,
                stream.as_bytes(),
                fragment,
            );
            assert_eq!(
                String::from_utf8(forwarded).expect("utf8"),
                "GET / HTTP/1.1\r\nhost: my.host\r\n\r\nGET / HTTP/1.1\r\nhost: my.host\r\n\r\n",
                "fragment {fragment}"
            );
        }
        // And the capture stream still holds the ORIGINAL bytes, not the rewritten ones.
        let mut sink = RecordingSink::new();
        let (_, _) = drive(&mut pump, &mut sink, request.as_bytes(), usize::MAX);
        assert_eq!(
            String::from_utf8(sink.captured_reads().concat()).expect("utf8"),
            request
        );
    }
}
