//! Source to client: classify, capture, forward (traceability row PROXY-06).
//!
//! # What reaches the client
//!
//! Exactly the bytes that arrived from the source, in order, unmodified — even when the capture
//! stream split them into several observations. `LHH:790-792` calls `super.write` on the *original*
//! message after capturing, and the rewrite handlers never touch response bytes. This type captures
//! first and hands the untouched chunk back to its caller to write, which keeps both halves of that
//! property in one place.
//!
//! Capture failure on this path fails the write (`LHH:836-839` fails the promise with an
//! `IOException`), so the client does **not** receive bytes the capture stream could not record.
//! That is why [`ResponsePump::process`] returns an error instead of writing: the caller must not
//! write when it fails.
//!
//! # Held bytes are never lost
//!
//! An unclassifiable prefix is held, and flushed as an ordinary `write` observation on three paths,
//! always **before** whatever observation follows it (spec §7.5.4): before the next request's reads
//! (`LHH:841-854`), before a `connectionException` (`LHH:987-993`), and before the terminal `close`
//! (`LHH:356-357`).

use std::time::SystemTime;

use capture_http_scan::{RequestSuppression, ResponseClassifier, ResponsePiece};

use crate::{CaptureAbort, CaptureSink, CloseReason};

/// What one source read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponseStep {
    /// Interim (non-101 1xx) responses captured from this read.
    pub interim_responses: u32,
    /// `write` observations captured from this read.
    pub writes: u32,
    /// Close the connection once this chunk has been written, with this reason.
    pub close_after_write: Option<CloseReason>,
}

/// Classifies and captures one connection's response bytes.
#[derive(Debug, Default)]
pub struct ResponsePump {
    classifier: ResponseClassifier,
}

impl ResponsePump {
    /// Builds a pump for one connection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether unclassified bytes are held.
    #[must_use]
    pub fn has_held_bytes(&self) -> bool {
        self.classifier.held_bytes() > 0
    }

    /// Whether a final response has been classified for the current request.
    #[must_use]
    pub fn final_response_seen(&self) -> bool {
        self.classifier.final_response_seen()
    }

    /// Processes one source read. The caller writes the **original** chunk to the client only if
    /// this returns `Ok`.
    ///
    /// # Errors
    ///
    /// [`CaptureAbort`] when required capture failed terminally. The chunk must then not be written
    /// to the client, matching `failRequiredResponseCapture` (`LHH:836-839`).
    pub fn process<K: CaptureSink>(
        &mut self,
        sink: &mut K,
        suppression: &RequestSuppression,
        at: SystemTime,
        chunk: &[u8],
    ) -> Result<ResponseStep, CaptureAbort> {
        let classification = self.classifier.classify(chunk);
        let mut step = ResponseStep::default();
        // Telemetry is not suppressed with capture (`LHH:905-911` counts every byte handed to the
        // client regardless); only the observations are.
        sink.observe_source_bytes(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let capturing = sink.should_capture() && suppression.should_capture_response();
        for piece in &classification.pieces {
            match piece {
                ResponsePiece::Interim(bytes) => {
                    if capturing {
                        sink.capture_interim_response(at, bytes)?;
                    }
                    step.interim_responses += 1;
                }
                ResponsePiece::Final(bytes) => {
                    if capturing {
                        sink.capture_write(at, bytes)?;
                    }
                    step.writes += 1;
                }
            }
        }
        if classification.hold_overflow {
            // Spec §13-D11: Java holds without bound. The held bytes have already been emitted as
            // a final write above, so the capture stream is intact; the connection then closes.
            step.close_after_write = Some(CloseReason::MalformedResponse);
        }
        Ok(step)
    }

    /// Flushes held bytes as a `write` observation.
    ///
    /// Call before the next request's reads, before a `connectionException`, and before the
    /// terminal `close`. Returns whether anything was flushed.
    ///
    /// # Errors
    ///
    /// As [`ResponsePump::process`].
    pub fn flush_held<K: CaptureSink>(
        &mut self,
        sink: &mut K,
        suppression: &RequestSuppression,
        at: SystemTime,
    ) -> Result<bool, CaptureAbort> {
        let Some(residue) = self.classifier.flush_held() else {
            return Ok(false);
        };
        if sink.should_capture() && suppression.should_capture_response() {
            sink.capture_write(at, &residue)?;
        }
        Ok(true)
    }

    /// Starts classifying the next request's response.
    ///
    /// # Panics
    ///
    /// Panics if bytes are still held; [`ResponsePump::flush_held`] must run first, or those bytes
    /// would vanish from the capture stream.
    pub fn begin_request(&mut self) {
        self.classifier.begin_request();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::tests::RecordingSink;
    use capture_config::CaptureDirective;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    fn capturing_suppression() -> RequestSuppression {
        RequestSuppression::new()
    }

    #[test]
    fn an_interim_response_split_across_reads_is_captured_once_and_exactly() {
        // SourceInterimResponseCaptureTest:34-66.
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let suppression = capturing_suppression();

        let step = pump
            .process(&mut sink, &suppression, now(), b"HTTP/1.1 100 Cont")
            .expect("no capture failure");
        assert_eq!(step.interim_responses, 0);
        assert_eq!(step.writes, 0);
        assert!(sink.events().is_empty());
        assert!(pump.has_held_bytes());

        let step = pump
            .process(&mut sink, &suppression, now(), b"inue\r\n\r\n")
            .expect("no capture failure");
        assert_eq!(step.interim_responses, 1);
        assert_eq!(sink.events(), vec!["interim".to_owned()]);
        assert_eq!(
            sink.captured_interim_responses(),
            vec![b"HTTP/1.1 100 Continue\r\n\r\n".to_vec()]
        );
        assert!(!pump.has_held_bytes());
    }

    #[test]
    fn the_java_interim_response_sequence_yields_four_interims_and_one_final() {
        // SourceInterimResponseCaptureTest:34-66 in full: two writes carrying one `100 Continue`,
        // then a single write carrying `102`, `103`, `103`+header and the final `200`. Four interim
        // observations in total, and exactly one final write.
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let suppression = capturing_suppression();
        let mut interims = 0;
        interims += pump
            .process(&mut sink, &suppression, now(), b"HTTP/1.1 100 Cont")
            .expect("no capture failure")
            .interim_responses;
        interims += pump
            .process(&mut sink, &suppression, now(), b"inue\r\n\r\n")
            .expect("no capture failure")
            .interim_responses;
        let write = concat!(
            "HTTP/1.1 102 Processing\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </next>\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        );
        let step = pump
            .process(&mut sink, &suppression, now(), write.as_bytes())
            .expect("no capture failure");
        interims += step.interim_responses;
        assert_eq!(interims, 4);
        assert_eq!(step.writes, 1);
        assert_eq!(sink.captured_interim_responses().len(), 4);
        assert_eq!(
            sink.captured_writes(),
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec()]
        );
    }

    #[test]
    fn a_suppressed_exchange_captures_no_response_observation() {
        // testThatHealthCheckCaptureCanBeSuppressed:846-862: two responses are written and only the
        // captured request's response appears as a Write observation.
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let mut suppression = RequestSuppression::new();
        suppression.apply_directive(CaptureDirective::Drop);
        suppression.finish_request();
        assert!(!suppression.should_capture_response());

        let step = pump
            .process(
                &mut sink,
                &suppression,
                now(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            )
            .expect("no capture failure");
        assert_eq!(step.writes, 1, "the bytes still reach the client");
        assert!(sink.events().is_empty(), "but nothing is captured");
    }

    #[test]
    fn held_bytes_are_flushed_as_a_write_before_whatever_follows() {
        // SourceInterimResponseCaptureTest:88-109 pins the order
        // ["write:HTTP/1.1 100 Cont", "exception", "close"].
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let suppression = capturing_suppression();
        pump.process(&mut sink, &suppression, now(), b"HTTP/1.1 100 Cont")
            .expect("no capture failure");
        assert!(
            pump.flush_held(&mut sink, &suppression, now())
                .expect("no capture failure")
        );
        sink.connection_exception(now(), "source connection broke")
            .expect("no capture failure");
        sink.close(now(), CloseReason::Normal)
            .expect("no capture failure");
        assert_eq!(
            sink.events(),
            vec![
                "write".to_owned(),
                "exception".to_owned(),
                "close".to_owned()
            ]
        );
        assert_eq!(sink.captured_writes(), vec![b"HTTP/1.1 100 Cont".to_vec()]);
        assert!(
            !pump
                .flush_held(&mut sink, &suppression, now())
                .expect("no capture failure"),
            "flushing twice must not duplicate the bytes"
        );
    }

    #[test]
    fn an_over_cap_hold_asks_for_a_close_after_the_bytes_are_written() {
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let suppression = capturing_suppression();
        let mut write = b"HTTP/1.1 100 Continue".to_vec();
        write.extend(std::iter::repeat_n(
            b'x',
            capture_http_scan::INTERIM_HOLD_LIMIT + 1,
        ));
        let step = pump
            .process(&mut sink, &suppression, now(), &write)
            .expect("no capture failure");
        assert_eq!(step.close_after_write, Some(CloseReason::MalformedResponse));
        assert_eq!(step.writes, 1, "the held bytes are not lost");
        assert_eq!(sink.captured_writes(), vec![write]);
    }

    #[test]
    fn a_capture_failure_on_the_response_path_stops_the_write() {
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        sink.fail_next();
        let suppression = capturing_suppression();
        pump.process(
            &mut sink,
            &suppression,
            now(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        )
        .expect_err("the sink refused, so the client must not get these bytes");
    }

    #[test]
    fn the_next_request_resumes_classification() {
        let mut pump = ResponsePump::new();
        let mut sink = RecordingSink::new();
        let suppression = capturing_suppression();
        pump.process(
            &mut sink,
            &suppression,
            now(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        )
        .expect("no capture failure");
        assert!(pump.final_response_seen());
        pump.begin_request();
        assert!(!pump.final_response_seen());
        let step = pump
            .process(
                &mut sink,
                &suppression,
                now(),
                b"HTTP/1.1 100 Continue\r\n\r\n",
            )
            .expect("no capture failure");
        assert_eq!(
            step.interim_responses, 1,
            "the next exchange classifies again"
        );
    }
}
