//! Incomplete-request and connection limits (traceability row HTTP-05).
//!
//! Defaults and validation follow `IncompleteRequestLimits.java:23-25`,
//! `LoggingHttpHandler.java:48`, and the startup checks at `CaptureProxy.java:311-328`.

use std::time::Duration;

/// Maximum time to assemble one incomplete request.
///
/// `IncompleteRequestLimits.java:23` — `Duration.ofMinutes(5)`, i.e. 300 seconds.
pub const DEFAULT_MAX_REQUEST_ASSEMBLY: Duration = Duration::from_mins(5);

/// Maximum aggregate header bytes for one incomplete request.
///
/// `IncompleteRequestLimits.java:24` — `10 * 1024 * 1024`.
pub const DEFAULT_MAX_HEADER_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum aggregate wire bytes for one incomplete request.
///
/// `IncompleteRequestLimits.java:25` — `250 * 1024 * 1024`.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 250 * 1024 * 1024;

/// Maximum lifetime of one client connection.
///
/// `LoggingHttpHandler.java:48` — `Duration.ofMinutes(60)`, i.e. 3600 seconds.
pub const DEFAULT_MAX_CONNECTION_DURATION: Duration = Duration::from_hours(1);

/// The scanner's header limit is an `i32` in Java, so the configured value must fit.
///
/// `CaptureProxy.java:317-322` rejects anything above `Integer.MAX_VALUE`.
const HEADER_BYTES_CEILING: u64 = 2_147_483_647;

/// Divisor applied to the header-byte budget to derive the default request-line budget.
const REQUEST_LINE_FRACTION_OF_HEADERS: u64 = 16;

/// Floor for the derived request-line budget, so a modest header budget still permits a request
/// line far longer than the 4096 bytes Netty silently allowed.
const MIN_DERIVED_REQUEST_LINE_BYTES: u64 = 8192;

/// Bounds on assembling one incomplete request, and on a connection's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    request_assembly: Duration,
    header_bytes: u64,
    total_bytes: u64,
    connection_duration: Duration,
    request_line_bytes: u64,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            request_assembly: DEFAULT_MAX_REQUEST_ASSEMBLY,
            header_bytes: DEFAULT_MAX_HEADER_BYTES,
            total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            connection_duration: DEFAULT_MAX_CONNECTION_DURATION,
            request_line_bytes: derive_request_line_limit(DEFAULT_MAX_HEADER_BYTES),
        }
    }
}

/// Derives the request-line budget from the header budget.
///
/// # Owner decision OD-1
///
/// Java configures only Netty's `maxHeaderSize` (`LoggingHttpHandler.java:101-103`), leaving
/// `maxInitialLineLength` at Netty's 4096 default. Because Java recognises only
/// `TooLongHttpHeaderException` and not its sibling `TooLongHttpLineException`
/// (`LoggingHttpHandler.java:146-150`), a request line over 4096 bytes is not reported as a limit
/// violation. It puts the decoder into `BAD_MESSAGE`, which never produces a `LastHttpContent`, so
/// the capture-before-forward gate never runs and the bytes are forwarded raw and uncaptured.
///
/// The owner ruled that the Rust proxy fails closed on malformed framing **and** raises this limit,
/// so long but legitimate request lines — routine for `OpenSearch` `_search` URIs — keep working.
///
/// The result is clamped to the header budget: a request line is part of the head, so a budget
/// above the header budget could never be reached and would be rejected as unenforceable.
fn derive_request_line_limit(header_bytes: u64) -> u64 {
    (header_bytes / REQUEST_LINE_FRACTION_OF_HEADERS)
        .max(MIN_DERIVED_REQUEST_LINE_BYTES)
        .min(header_bytes)
}

impl RequestLimits {
    /// Builds limits, validating them the way `CaptureProxy.java:311-328` does.
    ///
    /// `request_line_bytes` of `None` derives the value from `header_bytes`.
    ///
    /// # Errors
    ///
    /// Mirrors the Java startup checks, which reject a non-positive duration or byte count, a
    /// header budget above `Integer.MAX_VALUE`, and a total-byte budget below the header budget.
    /// Also rejects an explicit request-line budget that exceeds the header budget, which would be
    /// unenforceable.
    pub fn new(
        request_assembly: Duration,
        header_bytes: u64,
        total_bytes: u64,
        connection_duration: Duration,
        request_line_bytes: Option<u64>,
    ) -> Result<Self, LimitsError> {
        if request_assembly.is_zero() {
            return Err(LimitsError::NonPositiveDuration {
                name: "max-request-assembly-duration",
            });
        }
        if connection_duration.is_zero() {
            return Err(LimitsError::NonPositiveDuration {
                name: "max-connection-duration",
            });
        }
        if header_bytes == 0 {
            return Err(LimitsError::NonPositiveBytes {
                name: "max-incomplete-request-header-bytes",
            });
        }
        if header_bytes > HEADER_BYTES_CEILING {
            return Err(LimitsError::HeaderBytesTooLarge {
                value: header_bytes,
                ceiling: HEADER_BYTES_CEILING,
            });
        }
        // CaptureProxy.java:323-328 requires total >= header, not total > header.
        if total_bytes < header_bytes {
            return Err(LimitsError::TotalBelowHeader {
                total: total_bytes,
                header: header_bytes,
            });
        }

        let request_line_bytes = match request_line_bytes {
            Some(explicit) => {
                if explicit == 0 {
                    return Err(LimitsError::NonPositiveBytes {
                        name: "max-request-line-bytes",
                    });
                }
                if explicit > header_bytes {
                    return Err(LimitsError::RequestLineAboveHeader {
                        request_line: explicit,
                        header: header_bytes,
                    });
                }
                explicit
            }
            None => derive_request_line_limit(header_bytes),
        };

        Ok(Self {
            request_assembly,
            header_bytes,
            total_bytes,
            connection_duration,
            request_line_bytes,
        })
    }

    /// Maximum time to assemble one incomplete request.
    #[must_use]
    pub fn max_request_assembly(&self) -> Duration {
        self.request_assembly
    }

    /// Maximum aggregate header bytes for one incomplete request.
    #[must_use]
    pub fn max_header_bytes(&self) -> u64 {
        self.header_bytes
    }

    /// Maximum aggregate wire bytes for one incomplete request.
    #[must_use]
    pub fn max_total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Maximum lifetime of one client connection.
    #[must_use]
    pub fn max_connection_duration(&self) -> Duration {
        self.connection_duration
    }

    /// Maximum bytes in a request line. See [`derive_request_line_limit`] and owner decision OD-1.
    #[must_use]
    pub fn max_request_line_bytes(&self) -> u64 {
        self.request_line_bytes
    }
}

/// A limits configuration was rejected at startup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    /// A duration limit was zero.
    #[error("{name} must be positive")]
    NonPositiveDuration {
        /// Which limit.
        name: &'static str,
    },
    /// A byte limit was zero.
    #[error("{name} must be positive")]
    NonPositiveBytes {
        /// Which limit.
        name: &'static str,
    },
    /// The header budget exceeds what the scanner can represent.
    #[error("max-incomplete-request-header-bytes {value} exceeds the maximum of {ceiling}")]
    HeaderBytesTooLarge {
        /// Configured value.
        value: u64,
        /// Maximum accepted value.
        ceiling: u64,
    },
    /// The total budget is below the header budget.
    #[error(
        "max-incomplete-request-total-bytes {total} must be at least \
         max-incomplete-request-header-bytes {header}"
    )]
    TotalBelowHeader {
        /// Configured total.
        total: u64,
        /// Configured header budget.
        header: u64,
    },
    /// The request-line budget exceeds the header budget, so it could never be reached.
    #[error(
        "max-request-line-bytes {request_line} must not exceed \
         max-incomplete-request-header-bytes {header}"
    )]
    RequestLineAboveHeader {
        /// Configured request-line budget.
        request_line: u64,
        /// Configured header budget.
        header: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_java_constants() {
        let limits = RequestLimits::default();
        // Asserted in seconds so the test pins the number, not the constructor used.
        assert_eq!(limits.max_request_assembly().as_secs(), 300);
        assert_eq!(limits.max_header_bytes(), 10 * 1024 * 1024);
        assert_eq!(limits.max_total_bytes(), 250 * 1024 * 1024);
        assert_eq!(limits.max_connection_duration().as_secs(), 3600);
    }

    #[test]
    fn the_default_request_line_limit_is_far_above_nettys_4096() {
        // OD-1: Netty's silent 4096 default is precisely what made malformed framing tunnel.
        let limits = RequestLimits::default();
        assert_eq!(limits.max_request_line_bytes(), 10 * 1024 * 1024 / 16);
        assert!(limits.max_request_line_bytes() > 4096);
    }

    #[test]
    fn a_small_header_budget_still_yields_a_usable_request_line_floor() {
        let limits = RequestLimits::new(
            Duration::from_secs(1),
            64 * 1024,
            64 * 1024,
            Duration::from_secs(1),
            None,
        )
        .expect("valid");
        assert_eq!(
            limits.max_request_line_bytes(),
            MIN_DERIVED_REQUEST_LINE_BYTES
        );
        assert!(limits.max_request_line_bytes() > 4096);
    }

    #[test]
    fn a_header_budget_below_the_floor_clamps_rather_than_failing() {
        // Regression: an unclamped 8192-byte floor made every header budget under 8192 fail
        // validation, including the legitimate total == header == 1024 case below.
        let limits = RequestLimits::new(
            Duration::from_secs(1),
            1024,
            1024,
            Duration::from_secs(1),
            None,
        )
        .expect("a small header budget is valid");
        assert_eq!(limits.max_request_line_bytes(), 1024);
    }

    #[test]
    fn total_bytes_below_header_bytes_is_rejected() {
        let error = RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            10 * 1024 * 1024,
            1024,
            DEFAULT_MAX_CONNECTION_DURATION,
            None,
        )
        .expect_err("total below header must be rejected");
        assert!(matches!(error, LimitsError::TotalBelowHeader { .. }));
    }

    #[test]
    fn total_bytes_equal_to_header_bytes_is_accepted() {
        RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            1024,
            1024,
            DEFAULT_MAX_CONNECTION_DURATION,
            None,
        )
        .expect("total == header is valid");
    }

    #[test]
    fn a_header_budget_above_i32_max_is_rejected() {
        let over = HEADER_BYTES_CEILING + 1;
        let error = RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            over,
            over,
            DEFAULT_MAX_CONNECTION_DURATION,
            None,
        )
        .expect_err("must be rejected");
        assert!(matches!(error, LimitsError::HeaderBytesTooLarge { .. }));
    }

    #[test]
    fn the_header_ceiling_is_exactly_java_integer_max_value() {
        assert_eq!(HEADER_BYTES_CEILING, u64::from(i32::MAX.cast_unsigned()));
        RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            HEADER_BYTES_CEILING,
            HEADER_BYTES_CEILING,
            DEFAULT_MAX_CONNECTION_DURATION,
            None,
        )
        .expect("the ceiling itself is accepted");
    }

    #[test]
    fn non_positive_values_are_rejected() {
        assert!(matches!(
            RequestLimits::new(
                Duration::ZERO,
                1024,
                1024,
                DEFAULT_MAX_CONNECTION_DURATION,
                None
            ),
            Err(LimitsError::NonPositiveDuration { .. })
        ));
        assert!(matches!(
            RequestLimits::new(
                DEFAULT_MAX_REQUEST_ASSEMBLY,
                1024,
                1024,
                Duration::ZERO,
                None
            ),
            Err(LimitsError::NonPositiveDuration { .. })
        ));
        assert!(matches!(
            RequestLimits::new(
                DEFAULT_MAX_REQUEST_ASSEMBLY,
                0,
                1024,
                DEFAULT_MAX_CONNECTION_DURATION,
                None
            ),
            Err(LimitsError::NonPositiveBytes { .. })
        ));
    }

    #[test]
    fn an_unreachable_explicit_request_line_budget_is_rejected() {
        let error = RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            4096,
            8192,
            DEFAULT_MAX_CONNECTION_DURATION,
            Some(8192),
        )
        .expect_err("request line above header budget is unenforceable");
        assert!(matches!(error, LimitsError::RequestLineAboveHeader { .. }));
    }

    #[test]
    fn a_zero_explicit_request_line_budget_is_rejected() {
        assert!(matches!(
            RequestLimits::new(
                DEFAULT_MAX_REQUEST_ASSEMBLY,
                DEFAULT_MAX_HEADER_BYTES,
                DEFAULT_MAX_TOTAL_BYTES,
                DEFAULT_MAX_CONNECTION_DURATION,
                Some(0),
            ),
            Err(LimitsError::NonPositiveBytes { .. })
        ));
    }

    #[test]
    fn an_explicit_request_line_budget_overrides_the_derived_one() {
        let limits = RequestLimits::new(
            DEFAULT_MAX_REQUEST_ASSEMBLY,
            DEFAULT_MAX_HEADER_BYTES,
            DEFAULT_MAX_TOTAL_BYTES,
            DEFAULT_MAX_CONNECTION_DURATION,
            Some(65536),
        )
        .expect("valid");
        assert_eq!(limits.max_request_line_bytes(), 65536);
    }
}
