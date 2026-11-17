//! Process-wide capture failure policy (HTTP-01) and HTTP parsing strictness (owner decision OD-2).

use std::fmt;
use std::str::FromStr;

/// What the process does when required capture fails.
///
/// `proxyCaptureProtocol.md` §2.4 and §7. The choice is process-wide and its effect is
/// irreversible: capture never resumes in a compromised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureFailurePolicy {
    /// Terminate the process immediately when required Kafka publication fails or capture is
    /// otherwise compromised.
    ///
    /// The default, matching `CaptureProxy.java:288`. The request waiting at the pre-forward check
    /// is **not** sent to the source, and the process exits with a distinct nonzero
    /// capture-failure status without attempting orderly retirement (§7.2).
    #[default]
    FailClosed,
    /// Permanently abandon capture for the process and continue forwarding uncaptured.
    ///
    /// The process alarms continuously and capture never resumes (§7.3).
    FailOpen,
}

impl CaptureFailurePolicy {
    /// The policy's canonical configuration spelling.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FailClosed => "fail-closed",
            Self::FailOpen => "fail-open",
        }
    }

    /// True when a capture compromise must terminate the process.
    #[must_use]
    pub fn terminates_on_compromise(&self) -> bool {
        matches!(self, Self::FailClosed)
    }
}

impl fmt::Display for CaptureFailurePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CaptureFailurePolicy {
    type Err = PolicyParseError;

    /// Accepts only `fail-open` and `fail-closed`, case-insensitively.
    ///
    /// Matches the Java converter at `CaptureProxy.java:85-96`, which rejects anything else rather
    /// than falling back to a default. A typo must not silently select a policy.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "fail-closed" => Ok(Self::FailClosed),
            "fail-open" => Ok(Self::FailOpen),
            _ => Err(PolicyParseError {
                value: value.to_owned(),
            }),
        }
    }
}

/// An unrecognised `--capture-failure-policy` value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unrecognised capture failure policy {value:?}; expected \"fail-open\" or \"fail-closed\"")]
pub struct PolicyParseError {
    /// The rejected value.
    pub value: String,
}

/// How strictly HTTP/1 message framing is parsed.
///
/// # Owner decision OD-2
///
/// The Java proxy's image (its Jib entry in `buildImages/build.gradle`, line 62 at the time)
/// deliberately omitted Netty's `-Dio.netty...defaultStrictLineParsing=false`, with a comment
/// saying so, making production **strict**. But `trafficCaptureProxyServer/build.gradle:115-118`
/// set it false for that module's tests while `nettyWireLogging`'s tests left it strict, so the two
/// Java suites covering `LoggingHttpHandler` ran under two different HTTP grammars and nothing
/// tested `InvalidLineSeparatorException` at all. (Those Java modules have since been deleted.)
///
/// The owner ruled: strict by default, matching the production image, with an explicit opt-in
/// lenient mode as a documented escape hatch for a non-conforming client. The lenient mode is never
/// selected accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpStrictness {
    /// Require CRLF line terminators. The default and the production contract.
    #[default]
    Strict,
    /// Additionally accept a bare LF as a line terminator.
    ///
    /// This does **not** relax any other check: ambiguous framing such as a `Transfer-Encoding`
    /// and `Content-Length` conflict, or a duplicate `Content-Length`, is still a framing error
    /// under owner decision OD-1.
    LenientLineEndings,
}

impl HttpStrictness {
    /// The canonical configuration spelling.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::LenientLineEndings => "lenient-line-endings",
        }
    }

    /// True when a bare LF terminates a line.
    #[must_use]
    pub fn allows_bare_lf(&self) -> bool {
        matches!(self, Self::LenientLineEndings)
    }
}

impl fmt::Display for HttpStrictness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HttpStrictness {
    type Err = StrictnessParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "lenient-line-endings" => Ok(Self::LenientLineEndings),
            _ => Err(StrictnessParseError {
                value: value.to_owned(),
            }),
        }
    }
}

/// An unrecognised HTTP strictness value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unrecognised HTTP strictness {value:?}; expected \"strict\" or \"lenient-line-endings\"")]
pub struct StrictnessParseError {
    /// The rejected value.
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_is_fail_closed() {
        // CaptureProxy.java:288. Production never sets this flag, so the default is what runs.
        assert_eq!(
            CaptureFailurePolicy::default(),
            CaptureFailurePolicy::FailClosed
        );
        assert!(CaptureFailurePolicy::default().terminates_on_compromise());
    }

    #[test]
    fn policy_parsing_accepts_both_spellings_case_insensitively() {
        for (input, expected) in [
            ("fail-closed", CaptureFailurePolicy::FailClosed),
            ("FAIL-CLOSED", CaptureFailurePolicy::FailClosed),
            ("fail-open", CaptureFailurePolicy::FailOpen),
            ("Fail-Open", CaptureFailurePolicy::FailOpen),
        ] {
            assert_eq!(
                input.parse::<CaptureFailurePolicy>().expect(input),
                expected
            );
        }
    }

    #[test]
    fn policy_parsing_rejects_anything_else_rather_than_defaulting() {
        for input in ["", "failclosed", "fail_closed", "closed", "open", "strict"] {
            assert!(
                input.parse::<CaptureFailurePolicy>().is_err(),
                "{input:?} must be rejected, not silently defaulted"
            );
        }
    }

    #[test]
    fn policy_round_trips_through_its_canonical_spelling() {
        for policy in [
            CaptureFailurePolicy::FailClosed,
            CaptureFailurePolicy::FailOpen,
        ] {
            assert_eq!(
                policy
                    .as_str()
                    .parse::<CaptureFailurePolicy>()
                    .expect("round trip"),
                policy
            );
        }
    }

    #[test]
    fn the_default_strictness_is_strict_like_the_production_image() {
        assert_eq!(HttpStrictness::default(), HttpStrictness::Strict);
        assert!(!HttpStrictness::default().allows_bare_lf());
        assert!(HttpStrictness::LenientLineEndings.allows_bare_lf());
    }

    #[test]
    fn strictness_parsing_rejects_unknown_values() {
        assert!(HttpStrictness::from_str("lenient").is_err());
        assert!(HttpStrictness::from_str("").is_err());
        assert_eq!(
            HttpStrictness::from_str("STRICT").expect("case insensitive"),
            HttpStrictness::Strict
        );
    }
}
