//! Per-connection stream acquisition (traceability row PROXY-12).
//!
//! `capture-transport` owns the handshake *mechanics* — [`FrontTlsAcceptor`],
//! [`DestinationTlsConnector`], and [`is_tls_handshake_error`]. This module owns the half it
//! deliberately left to the process: deciding from configuration whether each side is TLS at all,
//! turning `capture-config`'s already-validated trust material into `rustls` trust anchors, and
//! reproducing Java's observability behaviour on a frontside handshake failure.
//!
//! # The three Java behaviours preserved here
//!
//! 1. **Destination TLS is chosen by the URI scheme, never by a flag** (`CaptureProxy.java:450`).
//!    [`capture_config::DestinationEndpoint`] has already resolved the scheme and defaulted the
//!    port (80 for `http`, 443 for `https`), so this module reads both rather than recomputing
//!    either.
//! 2. **The destination peer name is the configured URI host** (`BCP:122-124`), which is what
//!    [`DestinationTlsConnector`] validates against and sends as SNI. A client-controlled `Host`
//!    header can therefore never redirect validation at a different certificate.
//! 3. **A frontside TLS handshake failure is logged, deduplicated, and closed, and produces no
//!    `connectionException` capture observation** (`PCI:206-215`, spec §13-D10). The terminal
//!    `CloseObservation` still happens — see [`ClientAcceptError::Handshake`].
//!
//! # Trust material
//!
//! [`ClientTrust::from_cli`] resolves the mutually exclusive `--ssl-trust-cert-file` /
//! `--ssl-trust-cert-pem-env-var` pair and can fail with
//! [`capture_transport::tls::TlsError::ConflictingClientTrust`]. Calling it here would be the
//! *second* time that exclusivity was enforced, on a value that can no longer express the
//! conflict: [`capture_config::TrustMaterial`] holds one source or the other. So
//! [`client_trust_from`] converts directly and the configuration layer stays the single
//! enforcement point.

use std::net::SocketAddr;

use capture_config::{ApplicationConfig, DestinationEndpoint, TrustMaterial};
use capture_observability::log_deduper::{KnownEvent, LogDeduper};
use capture_transport::tls::{
    ClientTrust, DestinationTlsConfig, DestinationTlsConnector, FrontTlsAcceptor, FrontTlsConfig,
    TlsError, is_tls_handshake_error,
};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::stream::{ClientStream, SourceStream};

/// Converts validated configuration trust material into transport trust material.
///
/// Direct, rather than through [`ClientTrust::from_cli`]: see the module documentation.
#[must_use]
pub fn client_trust_from(material: &TrustMaterial) -> ClientTrust {
    match material {
        TrustMaterial::File(path) => ClientTrust::File(path.clone()),
        // `expose` is the one audited way out of a SecretString. The PEM is public key material;
        // it is a secret only in the sense that it arrived through an environment variable the
        // configuration layer redacts.
        TrustMaterial::InlinePem { pem, .. } => ClientTrust::InlinePem(pem.expose().to_owned()),
    }
}

/// Why a client connection could not be handed to the data plane.
#[derive(Debug)]
pub enum ClientAcceptError {
    /// The frontside TLS handshake failed.
    ///
    /// Already logged, deduplicated over [`LogDeduper`]'s window. The caller must close the
    /// connection and emit the terminal `CloseObservation` **without** a `connectionException`
    /// observation, which is the Java behaviour spec §13-D10 requires be kept.
    Handshake(std::io::Error),
    /// Any other I/O failure on the client socket. This one does produce a `connectionException`.
    Io(std::io::Error),
}

impl std::fmt::Display for ClientAcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handshake(error) => write!(f, "frontside TLS handshake failed: {error}"),
            Self::Io(error) => write!(f, "client connection failed: {error}"),
        }
    }
}

/// Builds the two streams each connection needs.
#[derive(Debug)]
pub struct ConnectionFactory {
    destination_host: String,
    destination_port: u16,
    destination_tls: Option<DestinationTlsConnector>,
    front_tls: Option<FrontTlsAcceptor>,
    handshake_failures: LogDeduper,
}

impl ConnectionFactory {
    /// Builds the factory from validated configuration.
    ///
    /// Every handshake-capable object is constructed **once**, here, at start-up: a certificate
    /// that cannot be loaded or a peer name that is not a valid DNS name or IP address must stop
    /// start-up before the listener binds, not fail the first connection that arrives.
    ///
    /// # Errors
    ///
    /// [`TlsError`] when the frontside certificate, key, or trust material cannot be loaded, or
    /// when the destination host is not a usable TLS peer name.
    pub fn new(config: &ApplicationConfig) -> Result<Self, TlsError> {
        let destination = config.destination();
        Ok(Self {
            destination_host: destination.host().to_owned(),
            destination_port: destination.port(),
            destination_tls: destination_connector(destination, config.insecure_destination())?,
            front_tls: config
                .tls()
                .map(|tls| {
                    FrontTlsAcceptor::from_config(&FrontTlsConfig {
                        cert_chain_path: tls.cert_chain_file().to_path_buf(),
                        key_path: tls.key_file().to_path_buf(),
                        trust: tls.trust().map(client_trust_from),
                        require_client_auth: tls.client_auth().is_required(),
                    })
                })
                .transpose()?,
            // PCI:32 fixes the window at one minute.
            handshake_failures: LogDeduper::new(
                capture_observability::log_deduper::DEFAULT_DEDUPE_WINDOW,
            ),
        })
    }

    /// Whether the frontside listener requires a client certificate.
    ///
    /// The proxy's only caller gate. Drives the start-up exposure warning.
    #[must_use]
    pub fn requires_client_auth(&self) -> bool {
        self.front_tls
            .as_ref()
            .is_some_and(FrontTlsAcceptor::client_auth_required)
    }

    /// Whether the frontside listener terminates TLS.
    #[must_use]
    pub fn front_tls_enabled(&self) -> bool {
        self.front_tls.is_some()
    }

    /// Whether destination certificate validation has been switched off.
    ///
    /// True exactly when `--insecure-destination` was given against an `https` destination, which
    /// makes the proxy accept **any** certificate the destination presents. The start-up path warns
    /// about this; the testcontainer rig always passes the flag, so it is a production-reachable
    /// configuration rather than a test-only one.
    #[must_use]
    pub fn destination_validation_disabled(&self) -> bool {
        self.destination_tls
            .as_ref()
            .is_some_and(DestinationTlsConnector::certificate_validation_disabled)
    }

    /// Whether the destination is reached over TLS.
    #[must_use]
    pub fn destination_tls_enabled(&self) -> bool {
        self.destination_tls.is_some()
    }

    /// The destination authority, for logs.
    #[must_use]
    pub fn destination(&self) -> String {
        format!("{}:{}", self.destination_host, self.destination_port)
    }

    /// Completes the frontside handshake, if one is configured.
    ///
    /// # Errors
    ///
    /// [`ClientAcceptError::Handshake`] for a TLS handshake failure, which has already been logged
    /// through the deduper; [`ClientAcceptError::Io`] for anything else.
    pub async fn accept_client(
        &self,
        stream: TcpStream,
        peer: SocketAddr,
    ) -> Result<ClientStream, ClientAcceptError> {
        let Some(acceptor) = &self.front_tls else {
            return Ok(ClientStream::Plain(stream));
        };
        match acceptor.accept(stream).await {
            Ok(session) => Ok(ClientStream::Tls(Box::new(session))),
            Err(error) if is_tls_handshake_error(&error) => {
                // PCI:206-215: log (deduplicated) and close. No connectionException observation.
                self.handshake_failures.record_and_log(
                    KnownEvent::FrontsideTlsHandshakeFailure,
                    &peer.to_string(),
                    &error,
                );
                Err(ClientAcceptError::Handshake(error))
            }
            Err(error) => Err(ClientAcceptError::Io(error)),
        }
    }

    /// Connects to the destination, completing its TLS handshake when the scheme asked for one.
    ///
    /// The returned stream is usable: `BCP:105-111` completes the pooled connection promise from
    /// the **handshake** future rather than the connect future, so a handshake failure must fail
    /// the whole attempt. Awaiting here reproduces that, and is what lets
    /// [`capture_transport::Connection::new`] require an already-usable source.
    ///
    /// # Errors
    ///
    /// The connect or handshake failure. The caller closes the client connection with
    /// [`capture_transport::CloseReason::SourceIoError`].
    pub async fn connect_source(&self) -> std::io::Result<SourceStream> {
        let socket =
            TcpStream::connect((self.destination_host.as_str(), self.destination_port)).await?;
        // Java sets TCP_NODELAY on the backside channel (BCP:97); without it the proxy adds up to
        // a Nagle delay to every request it forwards.
        socket.set_nodelay(true)?;
        match &self.destination_tls {
            None => Ok(SourceStream::Plain(socket)),
            Some(connector) => Ok(SourceStream::Tls(Box::new(
                connector.connect(socket).await?,
            ))),
        }
    }

    /// Prepares the client socket accepted at `accepted_at`.
    ///
    /// Returns the stream and the instant to hand to
    /// [`capture_transport::Connection::with_lifetime_origin`], which is the accept instant and
    /// **not** the instant the handshakes finished: Java schedules the connection-lifetime deadline
    /// in `handlerAdded` (`LHH:287-303`), before the TLS handshake and before any byte, so the
    /// handshake has to sit inside the budget.
    ///
    /// # Errors
    ///
    /// As [`ConnectionFactory::accept_client`].
    pub async fn prepare_client(
        &self,
        stream: TcpStream,
        peer: SocketAddr,
        accepted_at: Instant,
    ) -> Result<(ClientStream, Instant), ClientAcceptError> {
        // Java sets TCP_NODELAY on the accepted channel too (NSHP:59).
        if let Err(error) = stream.set_nodelay(true) {
            return Err(ClientAcceptError::Io(error));
        }
        let client = self.accept_client(stream, peer).await?;
        Ok((client, accepted_at))
    }
}

/// Builds the destination connector, or `None` for a plaintext destination.
fn destination_connector(
    destination: &DestinationEndpoint,
    insecure: bool,
) -> Result<Option<DestinationTlsConnector>, TlsError> {
    if !destination.uses_tls() {
        return Ok(None);
    }
    DestinationTlsConnector::from_config(&DestinationTlsConfig {
        // BCP:122-124: the configured URI host, which is both the SNI value and the name
        // validated against. Never the client's Host header.
        peer_name: destination.peer_name().to_owned(),
        insecure,
    })
    .map(Some)
}

#[cfg(test)]
mod tests {
    use capture_config::{ProcessEnv, SecretString};

    use super::*;

    fn config(extra: &[&str]) -> ApplicationConfig {
        let mut args = vec!["--listen-port", "9200"];
        args.extend_from_slice(extra);
        ApplicationConfig::from_args(&args, &ProcessEnv).expect("configuration is valid")
    }

    fn factory(extra: &[&str]) -> ConnectionFactory {
        ConnectionFactory::new(&config(extra)).expect("factory builds")
    }

    #[test]
    fn an_http_destination_selects_no_destination_tls_and_port_80() {
        let factory = factory(&["--destination-uri", "http://upstream.test", "--no-capture"]);
        assert!(!factory.destination_tls_enabled());
        assert!(!factory.destination_validation_disabled());
        assert_eq!(factory.destination(), "upstream.test:80");
    }

    #[test]
    fn an_https_destination_selects_destination_tls_and_port_443() {
        let factory = factory(&["--destination-uri", "https://upstream.test", "--no-capture"]);
        assert!(factory.destination_tls_enabled());
        assert_eq!(factory.destination(), "upstream.test:443");
    }

    #[test]
    fn an_explicit_port_overrides_the_scheme_default() {
        let factory = factory(&[
            "--destination-uri",
            "https://upstream.test:9201",
            "--no-capture",
        ]);
        assert_eq!(factory.destination(), "upstream.test:9201");
        assert!(factory.destination_tls_enabled());
    }

    #[test]
    fn insecure_destination_is_reported_so_startup_can_warn_about_it() {
        let guarded = factory(&["--destination-uri", "https://upstream.test", "--no-capture"]);
        assert!(!guarded.destination_validation_disabled());

        let insecure = factory(&[
            "--destination-uri",
            "https://upstream.test",
            "--insecure-destination",
            "--no-capture",
        ]);
        assert!(
            insecure.destination_validation_disabled(),
            "--insecure-destination accepts any destination certificate and must be visible"
        );
    }

    #[test]
    fn insecure_destination_against_a_plaintext_destination_disables_nothing() {
        // There is no handshake to weaken, so the flag is inert rather than dangerous.
        let factory = factory(&[
            "--destination-uri",
            "http://upstream.test",
            "--insecure-destination",
            "--no-capture",
        ]);
        assert!(!factory.destination_validation_disabled());
    }

    #[test]
    fn a_plaintext_frontside_requires_no_client_auth_and_terminates_no_tls() {
        let factory = factory(&["--destination-uri", "http://upstream.test", "--no-capture"]);
        assert!(!factory.front_tls_enabled());
        assert!(
            !factory.requires_client_auth(),
            "a plaintext listener has no caller gate at all"
        );
    }

    #[test]
    fn trust_material_converts_without_reresolving_the_exclusive_pair() {
        let path = std::path::PathBuf::from("/etc/proxy-tls/ca.crt");
        assert_eq!(
            client_trust_from(&TrustMaterial::File(path.clone())),
            ClientTrust::File(path)
        );
        assert_eq!(
            client_trust_from(&TrustMaterial::InlinePem {
                env_var: "TRUST_PEM".to_owned(),
                pem: SecretString::new("-----BEGIN CERTIFICATE-----"),
            }),
            ClientTrust::InlinePem("-----BEGIN CERTIFICATE-----".to_owned())
        );
    }

    #[test]
    fn a_destination_host_that_is_not_a_usable_tls_peer_name_stops_startup() {
        // The whole point of building the connector at start-up: this must not be the first
        // connection's problem.
        let endpoint = DestinationEndpoint::parse("https://not a host/").ok();
        if let Some(endpoint) = endpoint {
            assert!(destination_connector(&endpoint, false).is_err());
        }
    }

    #[tokio::test]
    async fn a_plaintext_frontside_hands_the_socket_through_unchanged() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("local address");
        let factory = factory(&["--destination-uri", "http://upstream.test", "--no-capture"]);

        let connect = tokio::spawn(async move { TcpStream::connect(address).await });
        let (stream, peer) = listener.accept().await.expect("accept");
        let accepted_at = Instant::now();
        let (client, origin) = factory
            .prepare_client(stream, peer, accepted_at)
            .await
            .expect("plaintext needs no handshake");

        assert!(matches!(client, ClientStream::Plain(_)));
        assert_eq!(
            origin, accepted_at,
            "the lifetime origin must be the accept instant, not a post-handshake instant"
        );
        drop(connect.await.expect("connect task").expect("connected"));
    }
}
