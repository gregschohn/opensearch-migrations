//! Destination-endpoint parsing (traceability row SHARED-03).
//!
//! Replaces `coreUtilities.URIHelper.parseUriWithDefaultPort` (`URIHelper.java:21-58`), which
//! `CaptureProxy.convertStringToUri` calls at `CaptureProxy.java:437-446` and which hard-exits
//! **3** on any failure.
//!
//! # The rule, in full
//!
//! 1. The input must parse as a URI with an authority, so a host is present.
//! 2. A scheme is required.
//! 3. When no port is given, `https` defaults to 443 and `http` to 80. Any other scheme **must**
//!    carry an explicit port.
//!
//! # Why the host matters beyond connecting
//!
//! The host is also the destination TLS peer name. `BacksideConnectionPool.createSslEngine`
//! (`BacksideConnectionPool.java:122-124`) builds the engine with
//! `newEngine(allocator, backsideUri.getHost(), backsideUri.getPort())`, so both the name the
//! proxy validates the destination certificate against and the SNI value it sends come from this
//! configured URI — **never** from a client-supplied `Host` header. A client therefore cannot
//! redirect certificate validation at a different certificate. [`DestinationEndpoint::peer_name`]
//! is that value.
//!
//! # Differences from Java, deliberately
//!
//! * A port above 65535 is rejected. Java's `URI` stores the port as an `int` and accepts it, then
//!   fails later inside the socket layer.
//! * Userinfo is retained but never rendered: `Debug` and `Display` redact it, because
//!   `https://user:password@host` in a startup log is a credential leak.

use std::fmt;

use crate::cli::REDACTED;

/// The default port for `http` (`URIHelper.java:38-39`).
pub const DEFAULT_HTTP_PORT: u16 = 80;

/// The default port for `https` (`URIHelper.java:36-37`).
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/// The destination URI's scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheme {
    /// Cleartext HTTP. Default port 80.
    Http,
    /// HTTP over TLS. Default port 443.
    Https,
    /// Any other scheme. Valid only with an explicit port, as in Java.
    Other(String),
}

impl Scheme {
    /// The scheme as configured, lowercased.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Other(scheme) => scheme,
        }
    }

    /// The default port, if this scheme has one.
    #[must_use]
    pub fn default_port(&self) -> Option<u16> {
        match self {
            Self::Http => Some(DEFAULT_HTTP_PORT),
            Self::Https => Some(DEFAULT_HTTPS_PORT),
            Self::Other(_) => None,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed `--destinationUri` with a definite port.
#[derive(Clone, PartialEq, Eq)]
pub struct DestinationEndpoint {
    scheme: Scheme,
    /// Present only so it can be preserved and redacted; the proxy never sends it.
    userinfo: Option<String>,
    host: String,
    is_ip_literal: bool,
    port: u16,
    remainder: String,
}

impl DestinationEndpoint {
    /// Parses `input`, applying the scheme's default port when none is given.
    ///
    /// # Errors
    ///
    /// Every variant of [`EndpointError`] is a startup failure. Java exits **3** for all of them
    /// (`CaptureProxy.java:443`), and [`crate::application::DESTINATION_URI_EXIT_CODE`] keeps that
    /// code.
    pub fn parse(input: &str) -> Result<Self, EndpointError> {
        if input.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(EndpointError::Malformed {
                input: redact_userinfo(input),
                detail: "a URI may not contain whitespace or control characters",
            });
        }

        // Java reports a missing host first (URIHelper.java:28-33), and any input without an
        // authority yields a null host there, so it lands in the same place.
        let Some((scheme_text, after_scheme)) = input.split_once("://") else {
            return Err(EndpointError::MissingHost {
                input: redact_userinfo(input),
            });
        };
        if scheme_text.is_empty() {
            return Err(EndpointError::Malformed {
                input: redact_userinfo(input),
                detail: "the scheme is empty",
            });
        }
        if !is_valid_scheme(scheme_text) {
            return Err(EndpointError::Malformed {
                input: redact_userinfo(input),
                detail: "the scheme must start with a letter and contain only letters, digits, \
                         '+', '-', or '.'",
            });
        }
        let scheme = match scheme_text.to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => Scheme::Other(other.to_owned()),
        };

        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let (authority, remainder) = after_scheme.split_at(authority_end);

        let (userinfo, host_port) = match authority.rsplit_once('@') {
            Some((userinfo, host_port)) => (Some(userinfo.to_owned()), host_port),
            None => (None, authority),
        };

        let (host, explicit_port, is_ip_literal) = split_host_port(host_port, input)?;
        if host.is_empty() {
            return Err(EndpointError::MissingHost {
                input: redact_userinfo(input),
            });
        }
        if !is_ip_literal && !is_valid_reg_name(&host) {
            return Err(EndpointError::InvalidHost { host });
        }

        let port = match explicit_port {
            Some(port) => port,
            None => scheme
                .default_port()
                .ok_or_else(|| EndpointError::SchemeWithoutPort {
                    scheme: scheme.as_str().to_owned(),
                })?,
        };

        Ok(Self {
            scheme,
            userinfo,
            host,
            is_ip_literal,
            port,
            remainder: remainder.to_owned(),
        })
    }

    /// The scheme.
    #[must_use]
    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    /// The host, with any IPv6 brackets removed.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port, defaulted from the scheme when it was not given.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the destination is reached over TLS, i.e. the scheme is `https`.
    ///
    /// `CaptureProxy.java:450` makes exactly this test.
    #[must_use]
    pub fn uses_tls(&self) -> bool {
        matches!(self.scheme, Scheme::Https)
    }

    /// Whether the host is an IP literal rather than a name.
    #[must_use]
    pub fn is_ip_literal(&self) -> bool {
        self.is_ip_literal
    }

    /// The name the destination certificate is validated against, and the SNI value sent.
    ///
    /// This is the configured host and nothing else; see the module documentation.
    #[must_use]
    pub fn peer_name(&self) -> &str {
        &self.host
    }

    /// `host:port`, with brackets restored around an IPv6 literal.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.is_ip_literal && self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for DestinationEndpoint {
    /// Renders the endpoint with userinfo redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme)?;
        if self.userinfo.is_some() {
            write!(f, "{REDACTED}@")?;
        }
        write!(f, "{}{}", self.authority(), self.remainder)
    }
}

impl fmt::Debug for DestinationEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DestinationEndpoint")
            .field("uri", &self.to_string())
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Splits `host:port`, handling a bracketed IPv6 literal.
///
/// Returns the bracket-stripped host, the explicit port if one was given, and whether the host was
/// an IP literal. An empty port (`host:`) counts as absent, which is what Java's `URI` does.
fn split_host_port(
    host_port: &str,
    input: &str,
) -> Result<(String, Option<u16>, bool), EndpointError> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let Some((literal, after)) = rest.split_once(']') else {
            return Err(EndpointError::Malformed {
                input: redact_userinfo(input),
                detail: "an IPv6 literal must be enclosed in brackets",
            });
        };
        if literal.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(EndpointError::InvalidHost {
                host: literal.to_owned(),
            });
        }
        let port = parse_port(after.strip_prefix(':').unwrap_or(""), input)?;
        if !after.is_empty() && !after.starts_with(':') {
            return Err(EndpointError::Malformed {
                input: redact_userinfo(input),
                detail: "unexpected characters after an IPv6 literal",
            });
        }
        return Ok((literal.to_owned(), port, true));
    }

    let (host, port_text) = match host_port.rsplit_once(':') {
        Some((host, port_text)) => (host, port_text),
        None => (host_port, ""),
    };
    let port = parse_port(port_text, input)?;
    let is_ip_literal = host.parse::<std::net::Ipv4Addr>().is_ok();
    Ok((host.to_owned(), port, is_ip_literal))
}

fn parse_port(text: &str, input: &str) -> Result<Option<u16>, EndpointError> {
    if text.is_empty() {
        // Java's URI reports -1 for an empty port, so the scheme default applies.
        return Ok(None);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(EndpointError::Malformed {
            input: redact_userinfo(input),
            detail: "the port must be numeric",
        });
    }
    text.parse::<u16>()
        .map(Some)
        .map_err(|_| EndpointError::PortOutOfRange {
            port: text.to_owned(),
        })
}

fn is_valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(char::is_alphabetic)
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether `host` is an acceptable registered name.
///
/// Deliberately permissive about `_`, which `URI.getHost()` rejects: a Docker Compose service name
/// may contain one, and rejecting it would be a startup failure for a legitimate destination. It is
/// not permissive about anything that could change where the proxy connects or what certificate it
/// validates.
fn is_valid_reg_name(host: &str) -> bool {
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        && host.chars().any(char::is_alphanumeric)
        && !host.starts_with('.')
        && !host.contains("..")
}

/// Removes userinfo from a string being put into an error message.
fn redact_userinfo(input: &str) -> String {
    let Some((prefix, rest)) = input.split_once("://") else {
        return input.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, remainder) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_, host_port)) => format!("{prefix}://{REDACTED}@{host_port}{remainder}"),
        None => input.to_owned(),
    }
}

/// A destination URI could not be parsed.
///
/// Java raises `IllegalArgumentException` for all of these and `CaptureProxy.convertStringToUri`
/// prints it and calls `System.exit(3)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// No host was present.
    #[error("hostname not present for destination URI {input:?}")]
    MissingHost {
        /// The rejected input, with userinfo redacted.
        input: String,
    },
    /// The host was present but is not a usable name.
    #[error("destination host {host:?} is not a valid hostname or IP address")]
    InvalidHost {
        /// The rejected host.
        host: String,
    },
    /// The scheme has no default port and none was given.
    #[error(
        "port not present for destination URI and scheme {scheme:?} has no default; \
         only http (80) and https (443) have one"
    )]
    SchemeWithoutPort {
        /// The scheme that needs an explicit port.
        scheme: String,
    },
    /// The port is not in `0..=65535`.
    #[error("destination port {port:?} is outside the range 0-65535")]
    PortOutOfRange {
        /// The rejected port text.
        port: String,
    },
    /// The input is not a URI at all.
    #[error("invalid destination URI {input:?}: {detail}")]
    Malformed {
        /// The rejected input, with userinfo redacted.
        input: String,
        /// What was wrong.
        detail: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> DestinationEndpoint {
        DestinationEndpoint::parse(input).unwrap_or_else(|error| panic!("{input}: {error}"))
    }

    #[test]
    fn the_java_test_vectors_round_trip() {
        // CaptureProxySetupTest.java:296-320, verbatim.
        let explicit = parse("https://search-my-domain.us-east-1.es.amazonaws.com:9200");
        assert_eq!(explicit.port(), 9200);
        assert_eq!(explicit.scheme().as_str(), "https");

        let tls_without_port = parse("https://search-my-domain.us-east-1.es.amazonaws.com");
        assert_eq!(tls_without_port.port(), 443);
        assert_eq!(tls_without_port.scheme().as_str(), "https");

        let cleartext_without_port = parse("http://search-my-domain.us-east-1.es.amazonaws.com");
        assert_eq!(cleartext_without_port.port(), 80);
        assert_eq!(cleartext_without_port.scheme().as_str(), "http");

        let explicit_443 = parse("https://search-my-domain.us-east-1.es.amazonaws.com:443");
        assert_eq!(explicit_443.port(), 443);
    }

    #[test]
    fn tls_is_selected_by_the_scheme_alone() {
        assert!(parse("https://h:9200").uses_tls());
        assert!(!parse("http://h:9200").uses_tls());
        // CaptureProxy.java:450 tests the scheme, not the port.
        assert!(!parse("http://h:443").uses_tls());
        assert!(parse("https://h:80").uses_tls());
    }

    #[test]
    fn the_peer_name_is_the_configured_host() {
        // BacksideConnectionPool.java:122-124: a client Host header can never reach this.
        let endpoint = parse("https://source-cluster.internal:9200/some/path?q=1");
        assert_eq!(endpoint.peer_name(), "source-cluster.internal");
        assert_eq!(endpoint.host(), "source-cluster.internal");
        assert_eq!(endpoint.authority(), "source-cluster.internal:9200");
    }

    #[test]
    fn a_scheme_without_a_default_port_needs_an_explicit_one() {
        // URIHelper.java:40-42.
        assert_eq!(parse("tcp://h:9200").port(), 9200);
        let error = DestinationEndpoint::parse("tcp://h").expect_err("no default port for tcp");
        assert!(matches!(error, EndpointError::SchemeWithoutPort { .. }));
    }

    #[test]
    fn a_missing_scheme_or_host_is_rejected() {
        for input in [
            "",
            "search-my-domain:9200",
            "/some/path",
            "http:/h",
            "h:9200",
        ] {
            let error = DestinationEndpoint::parse(input).expect_err("must be rejected");
            assert!(
                matches!(
                    error,
                    EndpointError::MissingHost { .. } | EndpointError::Malformed { .. }
                ),
                "{input:?} produced {error}"
            );
        }
        assert!(matches!(
            DestinationEndpoint::parse("://h:9200").expect_err("empty scheme"),
            EndpointError::Malformed { .. }
        ));
        assert!(matches!(
            DestinationEndpoint::parse("https://:9200").expect_err("empty host"),
            EndpointError::MissingHost { .. }
        ));
    }

    #[test]
    fn whitespace_and_control_characters_are_rejected() {
        for input in [
            "https://h :9200",
            " https://h:9200",
            "https://h:9200\n",
            "https://h\t:9200",
        ] {
            assert!(
                DestinationEndpoint::parse(input).is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_non_numeric_or_out_of_range_port_is_rejected() {
        assert!(matches!(
            DestinationEndpoint::parse("https://h:nine").expect_err("not numeric"),
            EndpointError::Malformed { .. }
        ));
        // Java's URI accepts 99999 and fails later in the socket layer.
        assert!(matches!(
            DestinationEndpoint::parse("https://h:99999").expect_err("out of range"),
            EndpointError::PortOutOfRange { .. }
        ));
    }

    #[test]
    fn an_empty_port_falls_back_to_the_scheme_default() {
        // Java's URI reports -1 for "https://h:", so the default applies.
        assert_eq!(parse("https://h:").port(), 443);
    }

    #[test]
    fn an_ipv4_literal_is_accepted() {
        let endpoint = parse("http://127.0.0.1:9200");
        assert_eq!(endpoint.host(), "127.0.0.1");
        assert!(endpoint.is_ip_literal());
        assert_eq!(endpoint.authority(), "127.0.0.1:9200");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_only_in_the_authority() {
        let endpoint = parse("https://[2001:db8::1]:9200");
        assert_eq!(
            endpoint.host(),
            "2001:db8::1",
            "the bracketless form is what a TLS peer name needs"
        );
        assert!(endpoint.is_ip_literal());
        assert_eq!(endpoint.authority(), "[2001:db8::1]:9200");
        assert_eq!(parse("https://[::1]").port(), 443);
    }

    #[test]
    fn a_malformed_ipv6_literal_is_rejected() {
        for input in ["https://[2001:db8::1:9200", "https://[not-an-address]:9200"] {
            assert!(
                DestinationEndpoint::parse(input).is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_path_and_query_are_preserved_but_do_not_affect_the_authority() {
        let endpoint = parse("https://h/path?query=1#frag");
        assert_eq!(endpoint.port(), 443);
        assert_eq!(endpoint.authority(), "h:443");
        assert_eq!(endpoint.to_string(), "https://h:443/path?query=1#frag");
    }

    #[test]
    fn a_docker_compose_style_host_with_an_underscore_is_accepted() {
        // URI.getHost() rejects this, which would exit 3 for a legitimate service name.
        assert_eq!(
            parse("http://opensearch_target:9200").host(),
            "opensearch_target"
        );
    }

    #[test]
    fn an_invalid_host_is_rejected() {
        for input in [
            "https://ho st:9200",
            "https://.leading-dot:9200",
            "https://double..dot:9200",
            "https://-:9200",
        ] {
            assert!(
                DestinationEndpoint::parse(input).is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    #[test]
    fn userinfo_is_never_rendered() {
        let endpoint = parse("https://user:hunter2@source:9200");
        assert_eq!(endpoint.host(), "source");
        let rendered = endpoint.to_string();
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains(REDACTED), "{rendered}");
        let debugged = format!("{endpoint:?}");
        assert!(!debugged.contains("hunter2"), "{debugged}");
    }

    #[test]
    fn an_error_message_redacts_userinfo_too() {
        let error = DestinationEndpoint::parse("https://user:hunter2@:9200").expect_err("no host");
        let message = error.to_string();
        assert!(!message.contains("hunter2"), "{message}");
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        assert_eq!(parse("HTTPS://h").port(), 443);
        assert!(parse("HtTpS://h").uses_tls());
        assert_eq!(parse("HTTP://h").port(), 80);
    }
}
