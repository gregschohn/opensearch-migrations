//! Bounded diagnostics for failures from unauthenticated clients (PROXY-13).
//!
//! A client that cannot complete the frontside TLS handshake can retry forever, and each attempt
//! produces a stack trace. Logging every one lets an unauthenticated peer dictate the proxy's log
//! volume. This module reproduces `UnauthenticatedClientLogDeduper`'s semantics: the first
//! occurrence always logs, occurrences inside a window are counted instead of logged, and the first
//! occurrence after the window logs with the accumulated suppressed count so nothing is silently
//! lost.
//!
//! # What is preserved, and what is not
//!
//! Preserved, because they are the operational contract:
//!
//! - **State is keyed by a closed event enum, never by client address**
//!   (`UnauthenticatedClientLogDeduper.java:17-19`). Keying by peer would let an attacker allocate
//!   unbounded state, which is the opposite of the point.
//! - **Monotonic time.** A wall-clock step must not open or close the window.
//! - **The first event always logs**, whatever the clock reads at start-up (Java seeds
//!   `nextLogAfterNanos` to `Long.MIN_VALUE`, line 67).
//! - **The suppressed count saturates** rather than wrapping (lines 70-74), and is reported on the
//!   next emitted line.
//! - **It controls log volume only.** It does not authenticate, rate-limit, or change channel
//!   handling. Every failing connection is still closed by the caller.
//!
//! Not preserved, deliberately:
//!
//! - The window is **configurable**, defaulting to the Java constant of one minute
//!   (`ProxyChannelInitializer.java:32`, where it is hard-coded). An operator diagnosing a client
//!   misconfiguration can widen or narrow it.
//! - The emitted line follows this crate's `event=<bounded_name> key=value` convention rather than
//!   Java's free-text `"TLS handshake failed for frontside channel: <channelId>"`, so the gameday
//!   log grep (`event=capture_`) sees it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The default dedupe window, matching `ProxyChannelInitializer.java:32`.
pub const DEFAULT_DEDUPE_WINDOW: Duration = Duration::from_mins(1);

/// Failure categories whose repeated diagnostics may be coalesced independently.
///
/// Closed by construction: the deduper's state is one slot per variant, so its memory is a compile
/// time constant no matter what a client does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownEvent {
    /// The frontside TLS handshake failed before a usable client channel existed.
    FrontsideTlsHandshakeFailure,
}

impl KnownEvent {
    /// Every known event.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::FrontsideTlsHandshakeFailure]
    }

    /// The event's index into the deduper's fixed state array.
    const fn slot(self) -> usize {
        match self {
            Self::FrontsideTlsHandshakeFailure => 0,
        }
    }

    /// The `event=` name emitted for this category.
    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::FrontsideTlsHandshakeFailure => crate::events::CAPTURE_TLS_HANDSHAKE_FAILED,
        }
    }
}

/// What the caller should do with one observed occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDecision {
    /// Emit the diagnostic, reporting how many equivalent events were suppressed since the previous
    /// emitted one.
    Log {
        /// Occurrences coalesced into this line. Zero on the first occurrence.
        suppressed_since_previous: u64,
    },
    /// Do not emit; this occurrence was counted into the running suppressed total.
    Suppress,
}

impl LogDecision {
    /// Whether the caller should emit the diagnostic.
    #[must_use]
    pub const fn should_log(self) -> bool {
        matches!(self, Self::Log { .. })
    }
}

#[derive(Debug, Clone, Copy)]
struct EventState {
    /// `None` until the first occurrence, which is why the first always logs.
    next_log_after: Option<Instant>,
    suppressed_count: u64,
}

impl EventState {
    const fn new() -> Self {
        Self {
            next_log_after: None,
            suppressed_count: 0,
        }
    }
}

/// Rate-limits repetitive diagnostics for pre-authentication failures.
///
/// One instance is shared by every frontside TLS exception handler, so the window applies
/// process-wide per event category rather than per connection.
#[derive(Debug)]
pub struct LogDeduper {
    window: Duration,
    states: Mutex<[EventState; KnownEvent::all().len()]>,
}

impl Default for LogDeduper {
    fn default() -> Self {
        Self::new(DEFAULT_DEDUPE_WINDOW)
    }
}

impl LogDeduper {
    /// Builds a deduper with the given window.
    ///
    /// A zero window is accepted and means "log every occurrence"; Java rejects it
    /// (`UnauthenticatedClientLogDeduper.java:37-39`), but here the configuration layer validates
    /// the value and a zero window is a coherent, if noisy, operator choice.
    // DEVIATION: Java hard-codes the window at ProxyChannelInitializer.java:32 and rejects a
    // non-positive one (UnauthenticatedClientLogDeduper.java:37-39). Here it is a parameter so an
    // operator diagnosing a client misconfiguration can widen or narrow it, and a zero window is
    // accepted as the coherent "log everything" choice rather than a start-up failure.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            states: Mutex::new([EventState::new(); KnownEvent::all().len()]),
        }
    }

    /// The configured dedupe window.
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// Records one occurrence and decides whether its diagnostic should be emitted.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex was poisoned by a previous panic inside this method.
    pub fn record_occurrence(&self, event: KnownEvent) -> LogDecision {
        self.record_occurrence_at(event, Instant::now())
    }

    /// [`Self::record_occurrence`] with an injected monotonic reading, for tests.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex was poisoned.
    pub fn record_occurrence_at(&self, event: KnownEvent, now: Instant) -> LogDecision {
        let mut states = self.states.lock().expect("log deduper state poisoned");
        let state = &mut states[event.slot()];
        if let Some(next_log_after) = state.next_log_after
            && now < next_log_after
        {
            state.suppressed_count = state.suppressed_count.saturating_add(1);
            return LogDecision::Suppress;
        }
        let suppressed_since_previous = state.suppressed_count;
        state.suppressed_count = 0;
        state.next_log_after = Some(now + self.window);
        LogDecision::Log {
            suppressed_since_previous,
        }
    }

    /// Records one occurrence and emits the diagnostic if the window allows it.
    ///
    /// The emitted line is one WARN event carrying the channel identity and the suppressed count.
    /// `detail` is the underlying failure rendered by the caller — for a TLS handshake that is the
    /// handshake error.
    ///
    /// # Panics
    ///
    /// Panics if the state mutex was poisoned.
    pub fn record_and_log(&self, event: KnownEvent, channel: &str, detail: &dyn std::fmt::Display) {
        // DEVIATION: Java emits free text, "TLS handshake failed for frontside channel:
        // <channelId>" (ProxyChannelInitializer.java:31), which the gameday log grep for
        // `event=capture_` cannot see. The same diagnostic is emitted here as a bounded
        // `event=capture_*` line, so an existing operator tool finds it. Volume stays bounded by
        // the window above.
        if let LogDecision::Log {
            suppressed_since_previous,
        } = self.record_occurrence(event)
        {
            tracing::warn!(
                "event={} channel={} suppressedSincePreviousLog={} detail={}",
                event.event_name(),
                channel,
                suppressed_since_previous,
                detail
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of
    /// `UnauthenticatedClientLogDeduperTest.testFirstEventLogsAndRepeatedEventsAreSuppressedUntilWindowExpires`.
    #[test]
    fn first_event_logs_and_repeats_are_suppressed_until_the_window_expires() {
        let deduper = LogDeduper::new(Duration::from_secs(1));
        let start = Instant::now();
        let event = KnownEvent::FrontsideTlsHandshakeFailure;

        assert_eq!(
            deduper.record_occurrence_at(event, start),
            LogDecision::Log {
                suppressed_since_previous: 0
            }
        );
        assert_eq!(
            deduper.record_occurrence_at(event, start + Duration::from_millis(100)),
            LogDecision::Suppress
        );
        assert_eq!(
            deduper.record_occurrence_at(event, start + Duration::from_millis(200)),
            LogDecision::Suppress
        );
        assert_eq!(
            deduper.record_occurrence_at(event, start + Duration::from_secs(1)),
            LogDecision::Log {
                suppressed_since_previous: 2
            },
            "the next emitted line reports exactly what was coalesced into it"
        );
    }

    #[test]
    fn the_first_event_logs_whatever_the_clock_reads() {
        let deduper = LogDeduper::default();
        assert!(
            deduper
                .record_occurrence(KnownEvent::FrontsideTlsHandshakeFailure)
                .should_log()
        );
    }

    #[test]
    fn the_suppressed_count_is_reset_by_each_emitted_line() {
        let deduper = LogDeduper::new(Duration::from_secs(1));
        let start = Instant::now();
        let event = KnownEvent::FrontsideTlsHandshakeFailure;
        deduper.record_occurrence_at(event, start);
        deduper.record_occurrence_at(event, start + Duration::from_millis(1));
        deduper.record_occurrence_at(event, start + Duration::from_secs(1));
        assert_eq!(
            deduper.record_occurrence_at(event, start + Duration::from_secs(2)),
            LogDecision::Log {
                suppressed_since_previous: 0
            }
        );
    }

    #[test]
    fn a_flood_is_bounded_to_one_line_per_window() {
        let deduper = LogDeduper::new(Duration::from_mins(1));
        let start = Instant::now();
        let event = KnownEvent::FrontsideTlsHandshakeFailure;
        let logged = (0..100_000)
            .filter(|offset| {
                deduper
                    .record_occurrence_at(event, start + Duration::from_micros(*offset))
                    .should_log()
            })
            .count();
        assert_eq!(
            logged, 1,
            "100000 occurrences inside one window must produce one line"
        );
    }

    #[test]
    fn a_zero_window_logs_every_occurrence() {
        let deduper = LogDeduper::new(Duration::ZERO);
        let start = Instant::now();
        let event = KnownEvent::FrontsideTlsHandshakeFailure;
        assert!(deduper.record_occurrence_at(event, start).should_log());
        assert!(deduper.record_occurrence_at(event, start).should_log());
    }

    #[test]
    fn event_names_use_the_capture_prefix_the_gameday_script_greps() {
        for event in KnownEvent::all() {
            assert!(
                event.event_name().starts_with("capture_"),
                "{} would be invisible to the gameday log grep",
                event.event_name()
            );
        }
    }
}
