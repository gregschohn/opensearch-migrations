//! Capture timing intervals `F`, `H`, `E`, and the clock-skew allowance `S`.
//!
//! These are protocol parameters, not tuning knobs. See `proxyCaptureProtocol.md` §4.2 (`F`),
//! §5.2-§5.4 (`H`, `E`), §5.6 (the `0 < H < E` rule), and §6.3 (`S`).

use std::time::Duration;

/// `F` — `trafficStreamFlushInterval`. `KafkaCaptureFactory.java:54`, five seconds.
///
/// The fixed monotonic deadline for detaching a nonempty connection-local `TrafficStream`. It
/// starts with the record's first observation and is never extended by later activity
/// (`proxyCaptureProtocol.md` §4.2).
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// `H` — heartbeat publication interval. `KafkaCaptureFactory.java:55`, ten seconds.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// `E` — heartbeat expiration interval. `KafkaCaptureFactory.java:56`, thirty seconds.
pub const DEFAULT_HEARTBEAT_EXPIRATION: Duration = Duration::from_secs(30);

/// `S` — clock-skew allowance for backward broker-time movement.
///
/// The proxy and replayer must be configured with the same `E` and `S` for one capture-and-replay
/// run (`proxyCaptureProtocol.md` §5.4). `S` is **not** part of the local `H < E` margin (§5.6);
/// owner decision OD-6 uses it as the lower bound on accepted heartbeat movement.
pub const DEFAULT_CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(5);

/// The validated capture timing configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTiming {
    flush_interval: Duration,
    heartbeat_interval: Duration,
    heartbeat_expiration: Duration,
    clock_skew_allowance: Duration,
}

impl Default for CaptureTiming {
    fn default() -> Self {
        Self {
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            heartbeat_expiration: DEFAULT_HEARTBEAT_EXPIRATION,
            clock_skew_allowance: DEFAULT_CLOCK_SKEW_ALLOWANCE,
        }
    }
}

impl CaptureTiming {
    /// Validates the timing configuration.
    ///
    /// # Errors
    ///
    /// Rejects a non-positive `F`, `H`, or `E`, and rejects `H >= E`.
    ///
    /// `proxyCaptureProtocol.md` §5.6 requires `0 < H < E` with equality explicitly invalid: "If
    /// both values were 10 seconds, the next heartbeat would become due at the same instant as the
    /// acknowledgement deadline, leaving no time for scheduler delay, Kafka publication, retries,
    /// or acknowledgement processing." `CaptureProxy.java:305-310` enforces the same rule.
    ///
    /// A non-positive `F` is rejected per §10.1, which requires "a positive configurable
    /// `trafficStreamFlushInterval`". This is deliberately stricter than Java, which accepts
    /// `Duration.ZERO` and silently disables record deadlines
    /// (`StreamChannelConnectionCaptureSerializer.java:152-154`) — see discrepancy D-1. Silently
    /// disabling the record-boundary deadline would void the §4.2 content-boundary invariant.
    ///
    /// `S` may be zero: a deployment with no tolerated backward movement is a valid, stricter
    /// choice.
    pub fn new(
        flush_interval: Duration,
        heartbeat_interval: Duration,
        heartbeat_expiration: Duration,
        clock_skew_allowance: Duration,
    ) -> Result<Self, TimingError> {
        if flush_interval.is_zero() {
            return Err(TimingError::NonPositive { name: "F" });
        }
        if heartbeat_interval.is_zero() {
            return Err(TimingError::NonPositive { name: "H" });
        }
        if heartbeat_expiration.is_zero() {
            return Err(TimingError::NonPositive { name: "E" });
        }
        if heartbeat_interval >= heartbeat_expiration {
            return Err(TimingError::HeartbeatNotBelowExpiration {
                heartbeat_interval,
                heartbeat_expiration,
            });
        }
        Ok(Self {
            flush_interval,
            heartbeat_interval,
            heartbeat_expiration,
            clock_skew_allowance,
        })
    }

    /// `F` — the fixed record-boundary deadline.
    #[must_use]
    pub fn flush_interval(&self) -> Duration {
        self.flush_interval
    }

    /// `H` — the heartbeat publication interval.
    #[must_use]
    pub fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// `E` — the heartbeat expiration interval.
    #[must_use]
    pub fn heartbeat_expiration(&self) -> Duration {
        self.heartbeat_expiration
    }

    /// `S` — the tolerated backward broker-time movement (owner decision OD-6).
    #[must_use]
    pub fn clock_skew_allowance(&self) -> Duration {
        self.clock_skew_allowance
    }

    /// `E - H`, the available healthy-operation latency margin (`proxyCaptureProtocol.md` §5.6).
    ///
    /// The strict inequality is only the minimum configuration check, not a promise that any
    /// positive margin is operationally sufficient.
    #[must_use]
    pub fn latency_margin(&self) -> Duration {
        self.heartbeat_expiration
            .saturating_sub(self.heartbeat_interval)
    }
}

/// A timing configuration was rejected at startup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimingError {
    /// An interval was zero.
    #[error("capture timing interval {name} must be positive")]
    NonPositive {
        /// Which interval: `F`, `H`, or `E`.
        name: &'static str,
    },
    /// `H >= E`.
    #[error(
        "heartbeat interval H ({heartbeat_interval:?}) must be strictly less than the heartbeat \
         expiration interval E ({heartbeat_expiration:?}); equality leaves no margin for scheduler \
         delay, Kafka publication, retries, or acknowledgement processing"
    )]
    HeartbeatNotBelowExpiration {
        /// Configured `H`.
        heartbeat_interval: Duration,
        /// Configured `E`.
        heartbeat_expiration: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_protocol_document() {
        let timing = CaptureTiming::default();
        assert_eq!(timing.flush_interval(), Duration::from_secs(5));
        assert_eq!(timing.heartbeat_interval(), Duration::from_secs(10));
        assert_eq!(timing.heartbeat_expiration(), Duration::from_secs(30));
        assert_eq!(timing.latency_margin(), Duration::from_secs(20));
    }

    #[test]
    fn equal_heartbeat_and_expiration_is_rejected() {
        // proxyCaptureProtocol.md 5.6: "Equality is invalid."
        let error = CaptureTiming::new(
            DEFAULT_FLUSH_INTERVAL,
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::ZERO,
        )
        .expect_err("H == E must be rejected");
        assert!(matches!(
            error,
            TimingError::HeartbeatNotBelowExpiration { .. }
        ));
    }

    #[test]
    fn heartbeat_above_expiration_is_rejected() {
        assert!(matches!(
            CaptureTiming::new(
                DEFAULT_FLUSH_INTERVAL,
                Duration::from_secs(31),
                Duration::from_secs(30),
                Duration::ZERO,
            ),
            Err(TimingError::HeartbeatNotBelowExpiration { .. })
        ));
    }

    #[test]
    fn a_zero_flush_interval_is_rejected_unlike_java() {
        // Discrepancy D-1: Java accepts Duration.ZERO and silently disables record deadlines.
        let error = CaptureTiming::new(
            Duration::ZERO,
            DEFAULT_HEARTBEAT_INTERVAL,
            DEFAULT_HEARTBEAT_EXPIRATION,
            Duration::ZERO,
        )
        .expect_err("F = 0 must be rejected");
        assert_eq!(error, TimingError::NonPositive { name: "F" });
    }

    #[test]
    fn zero_intervals_are_rejected_by_name() {
        assert_eq!(
            CaptureTiming::new(
                DEFAULT_FLUSH_INTERVAL,
                Duration::ZERO,
                DEFAULT_HEARTBEAT_EXPIRATION,
                Duration::ZERO
            )
            .expect_err("H = 0"),
            TimingError::NonPositive { name: "H" }
        );
        assert_eq!(
            CaptureTiming::new(
                DEFAULT_FLUSH_INTERVAL,
                DEFAULT_HEARTBEAT_INTERVAL,
                Duration::ZERO,
                Duration::ZERO
            )
            .expect_err("E = 0"),
            TimingError::NonPositive { name: "E" }
        );
    }

    #[test]
    fn a_zero_clock_skew_allowance_is_valid() {
        // S = 0 means "tolerate no backward broker-time movement at all", a stricter but legitimate
        // deployment choice under OD-6.
        let timing = CaptureTiming::new(
            DEFAULT_FLUSH_INTERVAL,
            DEFAULT_HEARTBEAT_INTERVAL,
            DEFAULT_HEARTBEAT_EXPIRATION,
            Duration::ZERO,
        )
        .expect("S = 0 is valid");
        assert_eq!(timing.clock_skew_allowance(), Duration::ZERO);
    }
}
