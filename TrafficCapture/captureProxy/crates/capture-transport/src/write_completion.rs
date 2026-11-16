//! `requestForwarded`, emitted only after the outbound write succeeds (traceability row HTTP-09).
//!
//! # What the observation claims, and what it does not
//!
//! Java emits `addRequestForwardedEvent(timestamp)` from the **success branch** of the
//! write-to-source future and before the next client read is granted (`FH:56-60`,
//! `LHH:684-693`):
//!
//! ```java
//! outboundChannel.writeAndFlush(msg).addListener(future -> {
//!     if (future.isSuccess()) {
//!         SourceForwardingCompletion.reportWriteCompleted(ctx.channel(), Instant.now()); // FH:59
//!         ctx.channel().read();                                                          // FH:60
//!     } else { /* close both channels */ }
//! });
//! ```
//!
//! The timestamp is therefore *when the bytes were accepted by the local socket*. It does **not**
//! claim the source application received, parsed, or acted on the request — nothing at this layer
//! can know that. `proxyCaptureProtocol.md:496-498` is worded for exactly this claim, and the
//! Tokio equivalent of "the write future succeeded" is `write_all` followed by `flush` returning
//! `Ok`.
//!
//! Three conditions gate the emission, all three from `LHH:684-693`:
//!
//! 1. the in-flight flag was armed, i.e. this write carried the bytes that completed a **captured**
//!    request;
//! 2. the connection's capture has not already been finalized;
//! 3. the process is still capturing.
//!
//! A failed write emits nothing (Java's else branch closes both channels), and a suppressed request
//! emits nothing because no end-of-message was committed and the flag was never armed.

use std::time::SystemTime;

use crate::{CaptureAbort, CaptureSink};

/// Tracks whether the write currently in flight completes captured requests.
///
/// Armed immediately **before** the write is issued, exactly as Java sets
/// `forwardInFlightCompletesCapturedRequest` before calling `super.channelRead` with the in-source
/// comment "Set first: the write can complete, and report back, before `channelRead` returns"
/// (`LHH:671`). Here the ordering is structural rather than a race to win, but arming before the
/// write keeps the invariant obvious.
#[derive(Debug, Default)]
pub struct ForwardCompletion {
    /// How many captured end-of-message boundaries the in-flight write completes.
    armed: u32,
    /// Set once the connection's capture has been finalized.
    finalized: bool,
    /// Cumulative count of emitted `requestForwarded` observations, for assertions.
    emitted: u64,
}

impl ForwardCompletion {
    /// A fresh, unarmed tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms the tracker for a write that completes `captured_requests` captured requests.
    ///
    /// `0` means the write carries no request-completing bytes, which is the ordinary case for
    /// every read before the last one of a request (`LHH:506-508`, `completesCapturedRequest =
    /// false`).
    ///
    /// # DEVIATION
    ///
    /// Java holds a single boolean, so if one read completed two pipelined captured requests it
    /// would emit one `requestForwarded` for two end-of-message indicators. This counts them, so
    /// the capture stream keeps one forward per captured end of message, in end-of-message order,
    /// as `proxyCaptureProtocol.md` describes. Pipelining is outside the settled protocol contract
    /// either way; emitting the right number is strictly closer to the document.
    pub fn arm(&mut self, captured_requests: u32) {
        self.armed = captured_requests;
    }

    /// Whether the in-flight write completes at least one captured request.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed > 0
    }

    /// Marks the connection's capture finalized, after which no forward is reported.
    ///
    /// Java's equivalent guard is `captureCloseFuture != null` (`LHH:688`).
    pub fn mark_finalized(&mut self) {
        self.finalized = true;
    }

    /// How many `requestForwarded` observations have been emitted.
    #[must_use]
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Drops the armed state without emitting, for a write that failed.
    ///
    /// Java has no explicit equivalent because the failure branch closes both channels and the flag
    /// dies with the connection; being explicit here keeps "a failed forward is never reported"
    /// checkable.
    pub fn discard(&mut self) {
        self.armed = 0;
    }

    /// Reports that the outbound write succeeded, emitting any `requestForwarded` observations.
    ///
    /// Returns how many were emitted. Call this **before** issuing the next read, so a completed
    /// request's forward is captured ahead of the next request's reads.
    ///
    /// # Errors
    ///
    /// [`CaptureAbort`] when required capture failed terminally.
    pub fn report_write_completed<K: CaptureSink>(
        &mut self,
        sink: &mut K,
        at: SystemTime,
    ) -> Result<u32, CaptureAbort> {
        let armed = std::mem::replace(&mut self.armed, 0);
        if armed == 0 || self.finalized || !sink.should_capture() {
            return Ok(0);
        }
        for _ in 0..armed {
            sink.request_forwarded(at)?;
            self.emitted += 1;
        }
        Ok(armed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloseReason;
    use crate::connection::tests::RecordingSink;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    #[test]
    fn a_write_that_completes_no_request_reports_nothing() {
        let mut sink = RecordingSink::new();
        let mut completion = ForwardCompletion::new();
        completion.arm(0);
        assert!(!completion.is_armed());
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            0
        );
        assert!(sink.events().is_empty());
    }

    #[test]
    fn the_armed_flag_is_taken_and_cleared() {
        let mut sink = RecordingSink::new();
        let mut completion = ForwardCompletion::new();
        completion.arm(1);
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            1
        );
        // A second completion for the same armed write must not re-report.
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            0
        );
        assert_eq!(sink.events(), vec!["forwarded".to_owned()]);
        assert_eq!(completion.emitted(), 1);
    }

    #[test]
    fn a_failed_write_reports_nothing() {
        let mut sink = RecordingSink::new();
        let mut completion = ForwardCompletion::new();
        completion.arm(1);
        completion.discard();
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            0
        );
        assert!(sink.events().is_empty());
    }

    #[test]
    fn a_finalized_connection_reports_nothing() {
        let mut sink = RecordingSink::new();
        let mut completion = ForwardCompletion::new();
        completion.arm(1);
        completion.mark_finalized();
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            0
        );
        assert!(sink.events().is_empty());
    }

    #[test]
    fn a_process_that_stopped_capturing_reports_nothing() {
        let mut sink = RecordingSink::new();
        sink.stop_capturing();
        let mut completion = ForwardCompletion::new();
        completion.arm(1);
        assert_eq!(
            completion
                .report_write_completed(&mut sink, now())
                .expect("no capture failure"),
            0
        );
        assert!(sink.events().is_empty());
    }

    #[test]
    fn a_capture_failure_propagates_instead_of_being_swallowed() {
        let mut sink = RecordingSink::new();
        sink.fail_next();
        let mut completion = ForwardCompletion::new();
        completion.arm(1);
        let error = completion
            .report_write_completed(&mut sink, now())
            .expect_err("the sink refused");
        assert!(error.message.contains("forwarded"));
        // The sink is still usable for the terminal close path.
        sink.close(now(), CloseReason::CaptureFailure)
            .expect("close succeeds");
    }
}
