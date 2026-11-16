//! Front (client-facing) and destination (source-facing) TLS (traceability row PROXY-12).
//!
//! Ports spec §11. The Java sides are `CaptureProxy.loadSslEngineFromPem` (`CP:471-497`) and
//! `CaptureProxy.loadBacksideSslContext` (`CP:448-459`), with the peer-name rule from
//! `BacksideConnectionPool.createSslEngine` (`BCP:122-124`).
//!
//! # Protocol versions and ciphers
//!
//! The capture proxy does not depend on `netty-tcnative-boringssl-static`, so
//! `SslContext.defaultServerProvider()` resolves to the JDK provider, whose `JdkSslContext`
//! default protocol set is `{TLSv1.3, TLSv1.2}` (`JdkSslContext.java:124-133`, spec §11.1).
//! `rustls` 0.23 offers exactly TLS 1.3 and TLS 1.2 by default, so parity needs no explicit
//! version list here. Cipher suites are not configurable from `CaptureProxy`
//! (`JdkSslContext.java:172`), so this module does not restrict them either.
//!
//! # No ALPN, no SNI
//!
//! Neither side configures an application-protocol list or a server-name dispatcher; see
//! [`FrontTlsAcceptor::alpn_protocols`] for why that absence is load-bearing rather than
//! incidental.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};

/// A TLS configuration could not be assembled, or TLS material could not be loaded.
///
/// Every variant is a startup failure. Java raises either a `ParameterException` (which
/// `CaptureProxy` turns into a non-zero exit before the listener opens) or an `SSLException`; none
/// of these conditions is recoverable at run time, and none of them is a panic.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be opened or read.
    #[error("failed to read TLS material from {path}")]
    ReadFailed {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// A PEM document was syntactically invalid.
    #[error("failed to parse PEM from {source_name}")]
    MalformedPem {
        /// Where the PEM came from: a path, or the inline trust parameter.
        source_name: String,
        /// The underlying parse failure, reported by `rustls_pemfile` as an I/O error.
        #[source]
        source: io::Error,
    },
    /// A certificate-chain file contained no certificates.
    ///
    /// Java fails the equivalent case inside `SslContextBuilder.forServer` (`CP:474`).
    #[error("certificate chain file {path} contains no certificates")]
    EmptyCertificateChain {
        /// The offending file.
        path: PathBuf,
    },
    /// A key file contained no `PKCS#8`, `PKCS#1` or `SEC1` private key.
    #[error("key file {path} contains no supported private key (expected PKCS#8, PKCS#1 or SEC1)")]
    NoPrivateKey {
        /// The offending file.
        path: PathBuf,
    },
    /// Client trust material was supplied but contained no usable certificates.
    #[error("client trust material from {source_name} contains no usable certificates")]
    EmptyTrustMaterial {
        /// Where the trust material came from.
        source_name: String,
    },
    /// Both a trust file and an inline trust PEM were supplied.
    ///
    /// `CP:477-479` — "Only one of --sslTrustCertFile or --sslTrustCertPemEnvVar may be specified."
    /// Pinned on the Java side by `FrontsideMtlsTest.testWithBothTrustCertFileAndInlineTrustCertPemThrows`
    /// (`FrontsideMtlsTest:55-64`).
    #[error("only one of the client trust file or the inline client trust PEM may be specified")]
    ConflictingClientTrust,
    /// Client authentication was required but no trust material was supplied.
    ///
    /// `CP:491-494` — a startup `ParameterException`. Pinned by
    /// `FrontsideMtlsTest.testRequireClientAuthWithoutTrustCertThrows` (`FrontsideMtlsTest:89-96`).
    #[error(
        "client trust material is required when client authentication is required \
         (Java: --sslTrustCertFile or --sslTrustCertPemEnvVar with --requireClientAuth)"
    )]
    ClientAuthWithoutTrustMaterial,
    /// The configured destination peer name is neither a valid DNS name nor an IP address.
    ///
    /// The peer name is the host of `--destinationUri`; see [`DestinationTlsConfig::peer_name`].
    #[error("destination peer name {peer_name:?} is neither a valid DNS name nor an IP address")]
    InvalidPeerName {
        /// The rejected name.
        peer_name: String,
    },
    /// The platform trust store could not be loaded, or held no usable certificates.
    #[error("failed to load the platform trust store: {detail}")]
    NativeTrustStore {
        /// What went wrong.
        detail: String,
    },
    /// A client-certificate verifier could not be built from the supplied trust material.
    #[error("failed to build the client certificate verifier")]
    ClientVerifier(#[source] rustls::server::VerifierBuilderError),
    /// `rustls` rejected the assembled configuration, e.g. a key that does not match the chain.
    #[error("rustls rejected the TLS configuration")]
    Rustls(#[source] rustls::Error),
}

/// Where client (mutual-TLS) trust material comes from.
///
/// `CP:481-490`: exactly one of `--sslTrustCertFile` (a path) or `--sslTrustCertPemEnvVar` (the
/// name of an environment variable holding the PEM, read by `CP:499-509`). Because this enum holds
/// one or the other, the both-set error of `CP:477-479` is unrepresentable here; the CLI layer
/// reaches it through [`ClientTrust::from_cli`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientTrust {
    /// Trust anchors read from a PEM file (`--sslTrustCertFile`).
    File(PathBuf),
    /// Trust anchors supplied inline as PEM text (`--sslTrustCertPemEnvVar`).
    InlinePem(String),
}

impl ClientTrust {
    /// Resolves the mutually exclusive pair of CLI trust parameters.
    ///
    /// # Errors
    ///
    /// [`TlsError::ConflictingClientTrust`] when both are present, reproducing `CP:477-479`. An
    /// empty string or an empty path counts as absent, matching Java's `isEmpty()` checks at
    /// `CP:475-476`.
    pub fn from_cli(
        trust_file: Option<PathBuf>,
        inline_pem: Option<String>,
    ) -> Result<Option<Self>, TlsError> {
        let trust_file = trust_file.filter(|path| !path.as_os_str().is_empty());
        let inline_pem = inline_pem.filter(|pem| !pem.is_empty());
        match (trust_file, inline_pem) {
            (Some(_), Some(_)) => Err(TlsError::ConflictingClientTrust),
            (Some(path), None) => Ok(Some(Self::File(path))),
            (None, Some(pem)) => Ok(Some(Self::InlinePem(pem))),
            (None, None) => Ok(None),
        }
    }

    /// A human-readable name for this trust source, used in error messages.
    #[must_use]
    pub fn source_name(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::InlinePem(_) => "the inline client trust PEM".to_owned(),
        }
    }

    /// Parses the trust material into a root store.
    ///
    /// # Errors
    ///
    /// [`TlsError::ReadFailed`] when a trust file cannot be read, [`TlsError::MalformedPem`] when
    /// the PEM does not parse, and [`TlsError::EmptyTrustMaterial`] when it parses but yields no
    /// usable trust anchor.
    fn load_root_store(&self) -> Result<RootCertStore, TlsError> {
        // Deliberately not `read_certificates`: an empty trust file is reported as empty *trust
        // material*, not as an empty certificate chain, because the two Java parameters fail for
        // different reasons and the operator needs to know which one is wrong.
        let certs = match self {
            Self::File(path) => {
                let bytes = std::fs::read(path).map_err(|source| TlsError::ReadFailed {
                    path: path.clone(),
                    source,
                })?;
                parse_certificates(&mut bytes.as_slice(), &self.source_name())?
            }
            Self::InlinePem(pem) => parse_certificates(&mut pem.as_bytes(), &self.source_name())?,
        };
        let mut roots = RootCertStore::empty();
        let (added, _ignored) = roots.add_parsable_certificates(certs);
        if added == 0 {
            return Err(TlsError::EmptyTrustMaterial {
                source_name: self.source_name(),
            });
        }
        Ok(roots)
    }
}

/// Front (client-facing) TLS inputs, from the CLI/config layer.
///
/// TLS is enabled on the listener if and only if a certificate chain is configured (`CP:528`);
/// when it is, the key is mandatory (`CP:531-533`). Both are **PEM files only** — there is no
/// keystore path and no passphrase parameter anywhere in the Java tree (`CP:104-113`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontTlsConfig {
    /// `--sslCertChainFile`: the server certificate chain, leaf first.
    pub cert_chain_path: PathBuf,
    /// `--sslKeyFile`: the matching private key.
    pub key_path: PathBuf,
    /// `--sslTrustCertFile` or `--sslTrustCertPemEnvVar`, resolved by [`ClientTrust::from_cli`].
    pub trust: Option<ClientTrust>,
    /// `--requireClientAuth`: an arity-0 boolean flag (`CP:124-129`).
    pub require_client_auth: bool,
}

/// Destination (source-facing) TLS inputs.
///
/// Whether the destination is reached over TLS at all is decided by the `--destinationUri` scheme
/// (`CP:450`), which is the caller's business; this struct describes only how the handshake is
/// performed once TLS has been selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationTlsConfig {
    /// The host component of `--destinationUri`.
    ///
    /// **Not SNI and not the `Host` header.** `BCP:122-124` builds the engine with
    /// `newEngine(allocator, backsideUri.getHost(), backsideUri.getPort())`, so both the SNI value
    /// the proxy sends and the name it validates against come from the configured destination URI.
    /// A client-controlled `Host` header can therefore never redirect validation at a different
    /// certificate. Pinned on the Java side by
    /// `BacksideConnectionPoolTest.createsTlsEngineWithDestinationPeer`.
    pub peer_name: String,
    /// `--insecureDestination` (`CP:135-139`): accept any destination certificate.
    pub insecure: bool,
}

/// Accepts front-side TLS connections from clients.
///
/// One configuration is shared by every connection and a fresh session is derived per connection,
/// matching Java's single `SslContext` plus `newEngine` per channel (`CP:496`, `PCI:152`).
///
/// Cloning is cheap: both the acceptor and the configuration behind it are reference counted.
#[derive(Clone)]
pub struct FrontTlsAcceptor {
    acceptor: tokio_rustls::TlsAcceptor,
    config: Arc<ServerConfig>,
    client_auth_required: bool,
}

impl fmt::Debug for FrontTlsAcceptor {
    /// `rustls`' own types are not `Debug`, so this reports the two properties the proxy logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrontTlsAcceptor")
            .field("client_auth_required", &self.client_auth_required)
            .field("alpn_protocol_count", &self.config.alpn_protocols.len())
            .finish_non_exhaustive()
    }
}

impl FrontTlsAcceptor {
    /// Builds the acceptor from PEM material.
    ///
    /// # Client authentication
    ///
    /// Only two states are reachable, exactly as in Java (spec §11.1):
    ///
    /// | trust material | `require_client_auth` | result |
    /// |---|---|---|
    /// | absent | `false` | no client certificate requested (Netty's `ClientAuth.NONE` default) |
    /// | present | `false` | trust material parsed, no client certificate requested |
    /// | present | `true` | client certificate required and verified |
    /// | absent | `true` | startup error, `CP:491-494` |
    ///
    /// Java's `ClientAuth.OPTIONAL` is **never** set anywhere in the Java tree — `grep` for
    /// `ClientAuth` yields only `CP:484` and `CP:489`, both `REQUIRE` — so the "offer but do not
    /// demand a client certificate" state is unreachable and is deliberately **not implemented**
    /// here. Concretely, the trust-material-without-the-flag row uses
    /// `WebPkiClientVerifier::no_client_auth()` rather than `allow_unauthenticated()`, because the
    /// latter would send a `CertificateRequest` and so would be `OPTIONAL`.
    /// `FrontsideMtlsTest.testWithoutClientAuthDoesNotRequireClientCert`
    /// (`FrontsideMtlsTest:67-75`) asserts `getNeedClientAuth() == false` for that row, i.e. no
    /// request is made. The trust material is still parsed so that malformed trust material fails
    /// startup the way Java's `builder.trustManager(file)` does.
    ///
    /// # Errors
    ///
    /// Any [`TlsError`]: unreadable or empty certificate chain, a key file with no `PKCS#8`,
    /// `PKCS#1` or `SEC1` key, trust material that yields no trust anchor,
    /// [`TlsError::ClientAuthWithoutTrustMaterial`] for the last row of the table above, or
    /// [`TlsError::Rustls`] when the key does not match the chain.
    ///
    /// # Panics
    ///
    /// Panics only if the process has no usable `rustls` crypto provider, or more than one
    /// installed — a build misconfiguration, not a reachable run-time state. This crate enables a
    /// single provider.
    pub fn from_config(config: &FrontTlsConfig) -> Result<Self, TlsError> {
        let chain = read_certificates(&config.cert_chain_path)?;
        if chain.is_empty() {
            return Err(TlsError::EmptyCertificateChain {
                path: config.cert_chain_path.clone(),
            });
        }
        let key = read_private_key(&config.key_path)?;

        // Parsed in every trust-material row, so malformed trust material is a startup failure
        // even when no client certificate will be demanded.
        let roots = match &config.trust {
            Some(trust) => Some(trust.load_root_store()?),
            None => None,
        };

        let (verifier, client_auth_required) = match (roots, config.require_client_auth) {
            (Some(roots), true) => (
                WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(TlsError::ClientVerifier)?,
                true,
            ),
            (None, true) => return Err(TlsError::ClientAuthWithoutTrustMaterial),
            // The `ClientAuth.NONE` default: no `CertificateRequest` is sent at all. Note this is
            // `no_client_auth()` rather than `allow_unauthenticated()`, which would be `OPTIONAL`.
            (_, false) => (WebPkiClientVerifier::no_client_auth(), false),
        };

        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(TlsError::Rustls)?;

        // PARITY, not a DEVIATION: no `alpn_protocols` is assigned anywhere above, and that is
        // the whole point. Java sets no `applicationProtocolConfig`, so `h2` is unreachable over
        // TLS, which is what keeps the HTTP/1 scanner's framing assumptions valid. See
        // `FrontTlsAcceptor::alpn_protocols` before changing this.
        debug_assert!(
            server_config.alpn_protocols.is_empty(),
            "the front side must advertise no ALPN protocol"
        );

        let shared = Arc::new(server_config);
        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::clone(&shared)),
            config: shared,
            client_auth_required,
        })
    }

    /// Whether a client certificate is demanded, i.e. whether Java would have set
    /// `ClientAuth.REQUIRE`.
    ///
    /// Equivalent to Java's `SSLEngine.getNeedClientAuth()`, which is what the six-case matrix of
    /// `FrontsideMtlsTest:31-96` asserts.
    #[must_use]
    pub fn client_auth_required(&self) -> bool {
        self.client_auth_required
    }

    /// The advertised ALPN protocol list, which is always empty.
    ///
    /// # Deliberate parity, not an omission
    ///
    /// Java configures no `applicationProtocolConfig` on either side (spec §11.1, §11.2), so no
    /// ALPN extension is sent and `h2` can never be negotiated. That absence is load-bearing: the
    /// capture path scans HTTP/1 framing, and `h2` over TLS would hand the scanner a binary
    /// protocol its message-boundary rules do not describe. Java is protected from that only by
    /// the HTTP/2 **suppression** regex applied to the cleartext upgrade path (`CP:615`,
    /// spec §5.3); it has no ALPN to suppress. Advertising `h2` here would create a reachable
    /// HTTP/2-over-TLS path that Java never had.
    ///
    /// So: do not add ALPN protocols to this configuration without re-deciding what the scanner
    /// does with HTTP/2.
    #[must_use]
    pub fn alpn_protocols(&self) -> &[Vec<u8>] {
        &self.config.alpn_protocols
    }

    /// Performs the server-side handshake on an accepted connection.
    ///
    /// A failure here is a handshake failure: per `PCI:206-215` the caller logs it (deduplicated)
    /// and closes the connection, and deliberately emits **no** `connectionException` capture
    /// observation. Use [`is_tls_handshake_error`] to make that distinction.
    pub fn accept<S>(
        &self,
        stream: S,
    ) -> impl Future<Output = Result<tokio_rustls::server::TlsStream<S>, io::Error>> + Send
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.acceptor.accept(stream)
    }
}

/// Connects to the destination over TLS.
///
/// Cloning is cheap: both the connector and the configuration behind it are reference counted.
#[derive(Clone)]
pub struct DestinationTlsConnector {
    connector: tokio_rustls::TlsConnector,
    config: Arc<ClientConfig>,
    peer_name: String,
    server_name: ServerName<'static>,
    validation_disabled: bool,
}

impl fmt::Debug for DestinationTlsConnector {
    /// `rustls`' own types are not `Debug`, so this reports the properties the proxy logs — in
    /// particular whether certificate validation has been switched off.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DestinationTlsConnector")
            .field("peer_name", &self.peer_name)
            .field("certificate_validation_disabled", &self.validation_disabled)
            .field("alpn_protocol_count", &self.config.alpn_protocols.len())
            .finish_non_exhaustive()
    }
}

impl DestinationTlsConnector {
    /// Builds the connector.
    ///
    /// Validation uses the platform trust store, mirroring Netty's JDK default, unless
    /// `insecure` is set, in which case a verifier that accepts any certificate is installed —
    /// see [`DestinationTlsConnector::certificate_validation_disabled`].
    ///
    /// # Errors
    ///
    /// [`TlsError::InvalidPeerName`] when `peer_name` is neither a DNS name nor an IP address, and
    /// [`TlsError::NativeTrustStore`] when the platform trust store cannot be loaded or is empty
    /// (only in the validating configuration).
    ///
    /// # Panics
    ///
    /// Panics only if the process has no usable `rustls` crypto provider, or more than one
    /// installed — a build misconfiguration, not a reachable run-time state.
    pub fn from_config(config: &DestinationTlsConfig) -> Result<Self, TlsError> {
        let server_name = ServerName::try_from(config.peer_name.clone()).map_err(|_| {
            TlsError::InvalidPeerName {
                peer_name: config.peer_name.clone(),
            }
        })?;

        let builder = ClientConfig::builder();
        let client_config = if config.insecure {
            let provider = Arc::clone(builder.crypto_provider());
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyDestinationCertificate {
                    provider,
                }))
                .with_no_client_auth()
        } else {
            builder
                .with_root_certificates(load_native_root_store()?)
                .with_no_client_auth()
        };

        // PARITY, not a DEVIATION: no `alpn_protocols` is assigned above, matching Java's
        // `SslContextBuilder.forClient()` with no `applicationProtocolConfig`. See
        // `FrontTlsAcceptor::alpn_protocols`.
        debug_assert!(
            client_config.alpn_protocols.is_empty(),
            "the destination side must advertise no ALPN protocol"
        );

        let client_config = Arc::new(client_config);
        Ok(Self {
            connector: tokio_rustls::TlsConnector::from(Arc::clone(&client_config)),
            config: client_config,
            peer_name: config.peer_name.clone(),
            server_name,
            validation_disabled: config.insecure,
        })
    }

    /// The configured destination peer name.
    ///
    /// Always the `--destinationUri` host (`BCP:122-124`), never SNI and never the `Host` header.
    #[must_use]
    pub fn peer_name(&self) -> &str {
        &self.peer_name
    }

    /// Whether destination certificate validation has been disabled.
    ///
    /// True exactly when `--insecureDestination` was given, i.e. when Java would have installed
    /// `InsecureTrustManagerFactory.INSTANCE` (`CP:452-454`). The proxy is expected to log this
    /// loudly at startup: the mode is not test-only in practice — the testcontainer rig always
    /// passes `--insecureDestination` (`CaptureProxyContainer.java:88`), so the flag is a
    /// production-reachable configuration that silently accepts any destination certificate.
    #[must_use]
    pub fn certificate_validation_disabled(&self) -> bool {
        self.validation_disabled
    }

    /// The advertised ALPN protocol list, which is always empty.
    ///
    /// See [`FrontTlsAcceptor::alpn_protocols`] for the reasoning; it applies identically to the
    /// destination side.
    #[must_use]
    pub fn alpn_protocols(&self) -> &[Vec<u8>] {
        &self.config.alpn_protocols
    }

    /// Performs the client-side handshake against the destination.
    ///
    /// `BCP:105-111` completes the pooled connection promise from the **handshake** future rather
    /// than the connect future, so a handshake failure must fail the whole connection attempt;
    /// awaiting this future before handing the connection onward reproduces that.
    pub fn connect<S>(
        &self,
        stream: S,
    ) -> impl Future<Output = Result<tokio_rustls::client::TlsStream<S>, io::Error>> + Send
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.connector.connect(self.server_name.clone(), stream)
    }
}

/// Whether an I/O error came out of a TLS handshake (or any other `rustls` failure).
///
/// Reproduces `PCI.containsCause(cause, SSLException.class)` (`PCI:206-209`, `PCI:234-243`), which
/// walks the whole cause chain. `tokio_rustls` surfaces a [`rustls::Error`] as the inner error of
/// an [`io::Error`], so this walks the source chain looking for one.
///
/// The caller uses this to reproduce `PCI:206-215`: a handshake failure is logged — deduplicated
/// over a one-minute window, `PCI:32` — and the connection is closed, and it deliberately produces
/// **no** `connectionException` capture observation (spec §13-D10). Any other I/O error keeps
/// propagating and does produce one.
#[must_use]
pub fn is_tls_handshake_error(error: &io::Error) -> bool {
    chain_contains_rustls_error(error)
}

/// Whether `error` or anything it is caused by is a [`rustls::Error`].
///
/// Recursive rather than a loop because [`io::Error`] needs a detour: its [`std::error::Error`]
/// implementation forwards `source()` to its *inner* error's source, which means walking `source()`
/// alone steps straight over the inner error itself — exactly where `tokio_rustls` puts the
/// [`rustls::Error`]. Java's `PCI.containsCause` has no such hole to work around because
/// `getCause()` is uniform.
fn chain_contains_rustls_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if error.downcast_ref::<rustls::Error>().is_some() {
        return true;
    }
    if let Some(io_error) = error.downcast_ref::<io::Error>()
        && let Some(inner) = io_error.get_ref()
        && chain_contains_rustls_error(inner)
    {
        return true;
    }
    error.source().is_some_and(chain_contains_rustls_error)
}

/// A [`ServerCertVerifier`] that accepts any destination certificate.
///
/// # This is production-reachable
///
/// It mirrors Netty's `InsecureTrustManagerFactory.INSTANCE` (`CP:452-454`), which the Java proxy
/// installs whenever `--insecureDestination` is given. Although the flag reads like a test switch
/// — and the testcontainer rig does always pass it (`CaptureProxyContainer.java:88`) — nothing
/// stops a deployment from setting it, so this verifier must be treated as reachable in
/// production. [`DestinationTlsConnector::certificate_validation_disabled`] exists so the proxy
/// can report the mode at startup rather than letting it pass unnoticed.
///
/// Handshake **signature** verification is still performed, because Java's insecure trust manager
/// only short-circuits chain validation; the JSSE engine still verifies the handshake signature.
/// Only chain and name checking are skipped.
#[derive(Debug)]
struct AcceptAnyDestinationCertificate {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyDestinationCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl fmt::Display for AcceptAnyDestinationCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("destination certificate validation disabled (--insecureDestination)")
    }
}

/// Loads the platform trust store, mirroring Netty's JDK default trust manager.
fn load_native_root_store() -> Result<RootCertStore, TlsError> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        let detail = if loaded.errors.is_empty() {
            "the platform trust store contains no usable certificates".to_owned()
        } else {
            loaded
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        };
        return Err(TlsError::NativeTrustStore { detail });
    }
    Ok(roots)
}

/// Reads every certificate from a PEM file.
fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::ReadFailed {
        path: path.to_path_buf(),
        source,
    })?;
    let certs = parse_certificates(&mut bytes.as_slice(), &path.display().to_string())?;
    if certs.is_empty() {
        return Err(TlsError::EmptyCertificateChain {
            path: path.to_path_buf(),
        });
    }
    Ok(certs)
}

/// Parses every certificate out of a PEM document.
fn parse_certificates(
    reader: &mut dyn io::BufRead,
    source_name: &str,
) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    rustls_pemfile::certs(reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsError::MalformedPem {
            source_name: source_name.to_owned(),
            source,
        })
}

/// Reads the first private key from a PEM file.
///
/// `rustls_pemfile::private_key` accepts `PKCS#8` (`BEGIN PRIVATE KEY`), `PKCS#1`
/// (`BEGIN RSA PRIVATE KEY`) and `SEC1` (`BEGIN EC PRIVATE KEY`) blocks, which covers everything
/// Netty's PEM reader accepts for an unencrypted key. There is no passphrase parameter anywhere in
/// the Java tree, so an encrypted key is out of scope on both sides.
fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::ReadFailed {
        path: path.to_path_buf(),
        source,
    })?;
    rustls_pemfile::private_key(&mut bytes.as_slice())
        .map_err(|source| TlsError::MalformedPem {
            source_name: path.display().to_string(),
            source,
        })?
        .ok_or_else(|| TlsError::NoPrivateKey {
            path: path.to_path_buf(),
        })
}

#[cfg(test)]
#[allow(clippy::doc_markdown, reason = "test fixtures are not documentation")]
mod tests {
    use std::io::Write as _;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// Self-signed `CN=localhost` leaf with `subjectAltName=DNS:localhost`, 10-year validity.
    /// Generated once at authoring time with `openssl req -x509 -newkey rsa:2048 -nodes`.
    const SERVER_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIDHzCCAgegAwIBAgIUYTb60O5Ce19reIfL66VWijSIA10wDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MTExNjAzMzAwNloXDTM2MTEx
MzAzMzAwNlowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAvfiEnyeewSNtFv3SHHY0qBHA7y7DxyAWA769HpnklP2J
lizeKrZF2rxaBmTJIjriXSOy793SjfXuxa+qcgtuuPq8W7nrmCPCmqxt9RaDNSo1
iigzRbpNbFtsDtnR9x/E+Ri9fvI3M96gsz1bIBPj5Zt+RhRSN7ClkapyGmm/eZsW
axHsuFD/aNFCW3DJIfDRVWV5pS4sxCo/VzPfDyTV6yKv9w4/urgK7aZ1PulzBhVA
rtwcyePr56t+Kx0+DPIfzm56WVoV8V61ypiDqeDMk6W8z3zZxqCdr5vLmYxv4OF9
kZIB7Uy5cJoSUBrQooDpBLQ92+OXV44fWCRGzD4VowIDAQABo2kwZzAdBgNVHQ4E
FgQUnNtMxbF/f7qmQx4focW6QhftjekwHwYDVR0jBBgwFoAUnNtMxbF/f7qmQx4f
ocW6QhftjekwDwYDVR0TAQH/BAUwAwEB/zAUBgNVHREEDTALgglsb2NhbGhvc3Qw
DQYJKoZIhvcNAQELBQADggEBADTKaAF/rFqA5bntSJu29kkagyrbwdrBMgdYOcoq
1pDGIMrLkyz6Slxpo5cBUaZaP4KASYFlg6BRSwBuwFLjt/bFpycSM5yRbeWcn1Sb
p2TnMRmTLdrlT3/Amqjg5cf3unYiT/uJTdIZPVvvmXyfe322OovdFDqtljRJvDnH
dUEFa6sNDn2XVYJH+19owiW6SiDQ8Pr7n84i4I6DZ5MX8HkdpbRAATPpAKW21JRn
uptjybLtPRtmdTB7+qCqC5jqSLCfp4w90ZcJ6vqWZaGZeXui6mdEP7bHkxiRvghQ
/vb/xYk9m06FguFJ14hw6ZKFNeBHTloZoqmjauk/zSWITTM=
-----END CERTIFICATE-----
";

    /// The `SERVER_CERT_PEM` key as PKCS#8 (`BEGIN PRIVATE KEY`).
    const SERVER_KEY_PKCS8_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC9+ISfJ57BI20W
/dIcdjSoEcDvLsPHIBYDvr0emeSU/YmWLN4qtkXavFoGZMkiOuJdI7Lv3dKN9e7F
r6pyC264+rxbueuYI8KarG31FoM1KjWKKDNFuk1sW2wO2dH3H8T5GL1+8jcz3qCz
PVsgE+Plm35GFFI3sKWRqnIaab95mxZrEey4UP9o0UJbcMkh8NFVZXmlLizEKj9X
M98PJNXrIq/3Dj+6uArtpnU+6XMGFUCu3BzJ4+vnq34rHT4M8h/ObnpZWhXxXrXK
mIOp4MyTpbzPfNnGoJ2vm8uZjG/g4X2RkgHtTLlwmhJQGtCigOkEtD3b45dXjh9Y
JEbMPhWjAgMBAAECggEAChClOxGzYKJh7TX51376r5C3A//TWp+qhZkNcqRV63HQ
GUymRPOdNfk9wArcMeaWOZtpxACvGCKDNJg9OuP1LOSWNI98r2e1L8RocJBU16X8
eFCZ8fJsl9LzhVBaxnjyYRpDoVAMVUC0i0xdveW8VGs/WWv0wvx7z3Kpkt/njstC
xOS2r337AAhlexpiY5bPqBsqAfeYBwfzgDfXR+XJYVmlFqUVQ/Vz5blCGvOALbvb
jbdWGr0Gbo566lUbrM5l7Fgbv91KPkhTR4sPTm1oU52rUKXwI8mFEQ4i7hskkbUx
OZYBG02mCCXCw5dynpIGpKBHE7PJRNMhXjupO0ZRFQKBgQD9LmuPJIEKt3nW6wp+
KDHO6CFBSFH/byXMCdrdDI7keRAA5llkUwIBJSmdHooEZp72kdfdU5p4Y44fXS/o
ZHq885FzXl+bWnvZU4elUW3feDiozsGMp0/Az5yWdGkT0AAbyKg+DfJc9VEqryHJ
++H3kahaHirM/jBmYFiIK2WbXQKBgQDAFfHOjG+DdrhLtCwzaNZy4wV8lDvUb+BB
oWuTJflFAT6Yx0UlpRr38uCXxab9pd6ZlupmPrt+e6nBpzLDpPJXaugpS0WwTFKk
ks+yadhQits16xpTgOoQzDcHL+vjRHIAiPZ6HFW8n8UwrrcjvSVfU/jrLTxM/B4H
YNikKThk/wKBgBI97/4GUU3mXl7/3GeumbggvZSok0/mQEDtTNm0d+86Uc7eyf4S
hQh0FwV9F9CtXDLD21vVFEIsWickXblO1BEdM5OubxEQee/0tsdQkKwTS3HOQ9/Q
hkyMKOomRRPqXrehw42FD7tm6Yw7xiF5hpSB6DFmDwtqHS6jLn3RMqb5AoGBAKRJ
4KBtou8I+oXdWzS5RTnMDCRKUIXZDjB0FKMthm7LqrNXNEAHX/KULB4Dclldpe8t
kdvTad+7eyzjiVqmWJQGHv06oScxkDmsS7JoLxhsOyN5qvGiwA+P6rzd8t5V6z+j
5VP5qicDa8/Aj2kk3LMVCkBzVzDtLe3XmlOF4fMXAoGABi6GRzepi1O1s3FHL5SA
MXfGjvIrLNUIU22SKjRjQ8PkIsC9AgItNr27bAMzoUuxfFWgZRTd4eeUw2q875Y3
+tHtgUoM1QuZ4J5zq4vM3f02ehiCgmmAGsw4uat4WOamRvtvUFfLC5pIOd/1mEip
uYpTCWUmL6qAr+9eYfZFYNc=
-----END PRIVATE KEY-----
";

    /// The same key as PKCS#1 (`BEGIN RSA PRIVATE KEY`), produced by `openssl rsa -traditional`.
    const SERVER_KEY_PKCS1_PEM: &str = "\
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAvfiEnyeewSNtFv3SHHY0qBHA7y7DxyAWA769HpnklP2Jlize
KrZF2rxaBmTJIjriXSOy793SjfXuxa+qcgtuuPq8W7nrmCPCmqxt9RaDNSo1iigz
RbpNbFtsDtnR9x/E+Ri9fvI3M96gsz1bIBPj5Zt+RhRSN7ClkapyGmm/eZsWaxHs
uFD/aNFCW3DJIfDRVWV5pS4sxCo/VzPfDyTV6yKv9w4/urgK7aZ1PulzBhVArtwc
yePr56t+Kx0+DPIfzm56WVoV8V61ypiDqeDMk6W8z3zZxqCdr5vLmYxv4OF9kZIB
7Uy5cJoSUBrQooDpBLQ92+OXV44fWCRGzD4VowIDAQABAoIBAAoQpTsRs2CiYe01
+dd++q+QtwP/01qfqoWZDXKkVetx0BlMpkTznTX5PcAK3DHmljmbacQArxgigzSY
PTrj9SzkljSPfK9ntS/EaHCQVNel/HhQmfHybJfS84VQWsZ48mEaQ6FQDFVAtItM
Xb3lvFRrP1lr9ML8e89yqZLf547LQsTktq99+wAIZXsaYmOWz6gbKgH3mAcH84A3
10flyWFZpRalFUP1c+W5QhrzgC272423Vhq9Bm6OeupVG6zOZexYG7/dSj5IU0eL
D05taFOdq1Cl8CPJhREOIu4bJJG1MTmWARtNpgglwsOXcp6SBqSgRxOzyUTTIV47
qTtGURUCgYEA/S5rjySBCrd51usKfigxzughQUhR/28lzAna3QyO5HkQAOZZZFMC
ASUpnR6KBGae9pHX3VOaeGOOH10v6GR6vPORc15fm1p72VOHpVFt33g4qM7BjKdP
wM+clnRpE9AAG8ioPg3yXPVRKq8hyfvh95GoWh4qzP4wZmBYiCtlm10CgYEAwBXx
zoxvg3a4S7QsM2jWcuMFfJQ71G/gQaFrkyX5RQE+mMdFJaUa9/Lgl8Wm/aXemZbq
Zj67fnupwacyw6TyV2roKUtFsExSpJLPsmnYUIrbNesaU4DqEMw3By/r40RyAIj2
ehxVvJ/FMK63I70lX1P46y08TPweB2DYpCk4ZP8CgYASPe/+BlFN5l5e/9xnrpm4
IL2UqJNP5kBA7UzZtHfvOlHO3sn+EoUIdBcFfRfQrVwyw9tb1RRCLFonJF25TtQR
HTOTrm8REHnv9LbHUJCsE0txzkPf0IZMjCjqJkUT6l63ocONhQ+7ZumMO8YheYaU
gegxZg8Lah0uoy590TKm+QKBgQCkSeCgbaLvCPqF3Vs0uUU5zAwkSlCF2Q4wdBSj
LYZuy6qzVzRAB1/ylCweA3JZXaXvLZHb02nfu3ss44lapliUBh79OqEnMZA5rEuy
aC8YbDsjearxosAPj+q83fLeVes/o+VT+aonA2vPwI9pJNyzFQpAc1cw7S3t15pT
heHzFwKBgAYuhkc3qYtTtbNxRy+UgDF3xo7yKyzVCFNtkio0Y0PD5CLAvQICLTa9
u2wDM6FLsXxVoGUU3eHnlMNqvO+WN/rR7YFKDNULmeCec6uLzN39NnoYgoJpgBrM
OLmreFjmpkb7b1BXywuaSDnf9ZhIqbmKUwllJi+qgK/vXmH2RWDX
-----END RSA PRIVATE KEY-----
";

    /// A P-256 self-signed `CN=localhost` leaf, to cover the SEC1 key path.
    const EC_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBkjCCATmgAwIBAgIUY77VDaQWrtnj8/CDq1F2oY2/2jQwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MTExNjAzMzAwNloXDTM2MTExMzAz
MzAwNlowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEHyrCyY55OWk7S/zL4TASJyZtONlww7DL0/zmLpMC4CNsrRS94cn1+IKM
pL6+YcMVR4/S44gbTHVr4YUOOzJeg6NpMGcwHQYDVR0OBBYEFOf4KSGerRARmjEr
CjvbbZRbVmZnMB8GA1UdIwQYMBaAFOf4KSGerRARmjErCjvbbZRbVmZnMA8GA1Ud
EwEB/wQFMAMBAf8wFAYDVR0RBA0wC4IJbG9jYWxob3N0MAoGCCqGSM49BAMCA0cA
MEQCIGF3yBrBeEkMAO65YSK2nytyB5qSezmZb5eRB4QaNa4JAiAYIPA61Zu8BANb
EojoJ6Y1behYgnqtQTePTmY0kzgr8g==
-----END CERTIFICATE-----
";

    /// The `EC_CERT_PEM` key as SEC1 (`BEGIN EC PRIVATE KEY`).
    const EC_KEY_SEC1_PEM: &str = "\
-----BEGIN EC PRIVATE KEY-----
MHcCAQEEILknxJFsJKvrrqE5jILXuvscVAz8FqCoXh8mMCOzoc3zoAoGCCqGSM49
AwEHoUQDQgAEHyrCyY55OWk7S/zL4TASJyZtONlww7DL0/zmLpMC4CNsrRS94cn1
+IKMpL6+YcMVR4/S44gbTHVr4YUOOzJegw==
-----END EC PRIVATE KEY-----
";

    /// An unrelated CA certificate, used only as client trust material.
    const CLIENT_CA_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIDEzCCAfugAwIBAgIUHVjuxabp31h9gzTKyyrTWbW3wPowDQYJKoZIhvcNAQEL
BQAwGTEXMBUGA1UEAwwOVGVzdCBDbGllbnQgQ0EwHhcNMjYxMTE2MDMzMDA2WhcN
MzYxMTEzMDMzMDA2WjAZMRcwFQYDVQQDDA5UZXN0IENsaWVudCBDQTCCASIwDQYJ
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAOJE244DHAlxaTHF1NLkMrRFQvRjbpwa
ePRl9tuxZSJWFNxWXgVkEKFll1LlKdtiPEsedntimez7tsdoroGF2bxJJNFQ0nWI
mR51PUW8CIlsGZ9CmhHa3mK0AqiE9sWF2zbO6+dGaq+WdrOMDnYcRNU+Uh6uHIy9
KcqwIM2Wn1oRq0PPwv9t/X4eQ8EVRCbjoXpFAMjWFlfUti3qas2NV9UFh/wot/p3
CGsnWsRW9OOF/HmnCgrXafUHVecCL1JGk6cLwRLgXbBRBrt2C4wMc20rSDbymu7S
uthhCv6TWne2bP2XBnht1FyzOC2Ji0+zBIFPFz5y3/id973O5l5ZjBECAwEAAaNT
MFEwHQYDVR0OBBYEFAzC1e3ekyeoTPUXSkDpfnw0DSO6MB8GA1UdIwQYMBaAFAzC
1e3ekyeoTPUXSkDpfnw0DSO6MA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL
BQADggEBAMNcvi4dbyEzblgEXDvnCAhe+EbMJQetxI3mif7fmyumeZVPSJtEIBu3
FeG8QVP6mfB0iJsyD5v1mjiQ+g6rijtUl1eVOyKBUViExpDtvksShADKmJG/wAVd
M2ERWVtzPO+o+cIBLrXsGP8l+1g3DAYyg51u+iby+3AJ9b0Ysw7Q9jCsb+5y1na3
so4jLr7TS8l37RlwCvlhR030cm+Tj7GFL9bDjHvaUuRkkxRaNj8yc5yOI4aEJCLg
WnmxLauzaNq9sAIarHnsG+S6Wz41EXYh/a+3XWjgJs9yWa39a/ufJ8x6UkmunyqT
mezHjs3KVMQV0+H8aYqMSeFUP+0IoQo=
-----END CERTIFICATE-----
";

    /// Writes PEM text into a temporary directory and returns the path.
    fn write_pem(dir: &tempfile::TempDir, name: &str, pem: &str) -> PathBuf {
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create temp PEM");
        file.write_all(pem.as_bytes()).expect("write temp PEM");
        path
    }

    fn front_config(dir: &tempfile::TempDir) -> FrontTlsConfig {
        FrontTlsConfig {
            cert_chain_path: write_pem(dir, "server.crt", SERVER_CERT_PEM),
            key_path: write_pem(dir, "server.key", SERVER_KEY_PKCS8_PEM),
            trust: None,
            require_client_auth: false,
        }
    }

    #[test]
    fn a_pkcs8_key_and_chain_build_a_front_acceptor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let acceptor = FrontTlsAcceptor::from_config(&front_config(&dir)).expect("valid PEM");
        assert!(!acceptor.client_auth_required());
    }

    #[test]
    fn a_pkcs1_key_is_accepted() {
        // Netty's PEM reader accepts PKCS#1; rustls_pemfile::private_key must too.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            key_path: write_pem(&dir, "pkcs1.key", SERVER_KEY_PKCS1_PEM),
            ..front_config(&dir)
        };
        FrontTlsAcceptor::from_config(&config).expect("PKCS#1 keys are supported");
    }

    #[test]
    fn a_sec1_elliptic_curve_key_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            cert_chain_path: write_pem(&dir, "ec.crt", EC_CERT_PEM),
            key_path: write_pem(&dir, "ec.key", EC_KEY_SEC1_PEM),
            trust: None,
            require_client_auth: false,
        };
        FrontTlsAcceptor::from_config(&config).expect("SEC1 keys are supported");
    }

    #[test]
    fn an_empty_certificate_chain_is_an_error_and_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            cert_chain_path: write_pem(&dir, "empty.crt", ""),
            ..front_config(&dir)
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::EmptyCertificateChain { .. }));
    }

    #[test]
    fn a_key_file_with_no_private_key_is_an_error_and_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            // A certificate is a well-formed PEM document that is not a key.
            key_path: write_pem(&dir, "notakey.key", SERVER_CERT_PEM),
            ..front_config(&dir)
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::NoPrivateKey { .. }));
    }

    #[test]
    fn a_missing_certificate_file_is_an_error_and_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            cert_chain_path: dir.path().join("absent.crt"),
            ..front_config(&dir)
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::ReadFailed { .. }));
    }

    #[test]
    fn a_key_that_does_not_match_the_chain_is_rejected_by_rustls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            cert_chain_path: write_pem(&dir, "ec.crt", EC_CERT_PEM),
            key_path: write_pem(&dir, "rsa.key", SERVER_KEY_PKCS8_PEM),
            trust: None,
            require_client_auth: false,
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::Rustls(_)));
    }

    // ---- The six-case client-auth matrix of FrontsideMtlsTest:31-96 ----

    #[test]
    fn case_1_trust_file_with_the_flag_requires_a_client_certificate() {
        // FrontsideMtlsTest:31-39 asserts getNeedClientAuth() == true.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            trust: Some(ClientTrust::File(write_pem(&dir, "ca.crt", CLIENT_CA_PEM))),
            require_client_auth: true,
            ..front_config(&dir)
        };
        let acceptor = FrontTlsAcceptor::from_config(&config).expect("valid");
        assert!(acceptor.client_auth_required());
    }

    #[test]
    fn case_2_inline_trust_pem_with_the_flag_requires_a_client_certificate() {
        // FrontsideMtlsTest:42-52 asserts getNeedClientAuth() == true.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            trust: Some(ClientTrust::InlinePem(CLIENT_CA_PEM.to_owned())),
            require_client_auth: true,
            ..front_config(&dir)
        };
        let acceptor = FrontTlsAcceptor::from_config(&config).expect("valid");
        assert!(acceptor.client_auth_required());
    }

    #[test]
    fn case_3_both_a_trust_file_and_an_inline_trust_pem_is_an_error() {
        // FrontsideMtlsTest:55-64 expects a ParameterException; CP:477-479.
        let error = ClientTrust::from_cli(
            Some(PathBuf::from("/tmp/ca.crt")),
            Some(CLIENT_CA_PEM.to_owned()),
        )
        .expect_err("both forms of trust material must be rejected");
        assert!(matches!(error, TlsError::ConflictingClientTrust));
    }

    #[test]
    fn case_4_a_trust_file_without_the_flag_does_not_require_a_client_certificate() {
        // FrontsideMtlsTest:67-75 asserts getNeedClientAuth() == false: the trust store is loaded
        // but no client certificate is demanded, because ClientAuth defaults to NONE.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            trust: Some(ClientTrust::File(write_pem(&dir, "ca.crt", CLIENT_CA_PEM))),
            require_client_auth: false,
            ..front_config(&dir)
        };
        let acceptor = FrontTlsAcceptor::from_config(&config).expect("valid");
        assert!(!acceptor.client_auth_required());
    }

    #[test]
    fn case_5_no_trust_material_and_no_flag_does_not_require_a_client_certificate() {
        // FrontsideMtlsTest:78-86 asserts getNeedClientAuth() == false.
        let dir = tempfile::tempdir().expect("tempdir");
        let acceptor = FrontTlsAcceptor::from_config(&front_config(&dir)).expect("valid");
        assert!(!acceptor.client_auth_required());
    }

    #[test]
    fn case_6_the_flag_without_trust_material_is_an_error() {
        // FrontsideMtlsTest:89-96 expects a ParameterException; CP:491-494.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            require_client_auth: true,
            ..front_config(&dir)
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::ClientAuthWithoutTrustMaterial));
    }

    #[test]
    fn trust_material_that_parses_to_nothing_is_rejected_at_startup() {
        // Java's builder.trustManager(file) parses eagerly, so bad trust material is a startup
        // failure even in the no-flag row.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FrontTlsConfig {
            trust: Some(ClientTrust::File(write_pem(&dir, "empty-ca.crt", ""))),
            require_client_auth: false,
            ..front_config(&dir)
        };
        let error = FrontTlsAcceptor::from_config(&config).expect_err("must be rejected");
        assert!(matches!(error, TlsError::EmptyTrustMaterial { .. }));
    }

    #[test]
    fn from_cli_treats_empty_strings_as_absent_trust_material() {
        assert_eq!(
            ClientTrust::from_cli(Some(PathBuf::new()), Some(String::new())).expect("valid"),
            None
        );
        assert_eq!(
            ClientTrust::from_cli(None, Some(CLIENT_CA_PEM.to_owned())).expect("valid"),
            Some(ClientTrust::InlinePem(CLIENT_CA_PEM.to_owned()))
        );
    }

    // ---- ALPN ----

    #[test]
    fn neither_side_advertises_any_alpn_protocol() {
        // Spec §11.1 and §11.2: Java configures no applicationProtocolConfig, and that absence is
        // exactly what keeps h2 unreachable over TLS, which keeps the HTTP/1 scanner's
        // message-boundary assumptions valid.
        let dir = tempfile::tempdir().expect("tempdir");
        let acceptor = FrontTlsAcceptor::from_config(&front_config(&dir)).expect("valid");
        assert!(acceptor.alpn_protocols().is_empty());

        let connector = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "search.example.test".to_owned(),
            insecure: true,
        })
        .expect("valid");
        assert!(connector.alpn_protocols().is_empty());
    }

    // ---- Destination peer name ----

    #[test]
    fn the_destination_peer_name_is_exactly_what_was_configured() {
        // Ports BacksideConnectionPoolTest.createsTlsEngineWithDestinationPeer, which asserts the
        // engine's peer host comes from the destination URI (BCP:122-124) rather than from SNI or
        // the Host header.
        let connector = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "search.example.test".to_owned(),
            insecure: true,
        })
        .expect("valid");
        assert_eq!(connector.peer_name(), "search.example.test");
    }

    #[test]
    fn an_ip_address_is_a_valid_destination_peer_name() {
        let connector = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "127.0.0.1".to_owned(),
            insecure: true,
        })
        .expect("an IP literal is a valid peer name");
        assert_eq!(connector.peer_name(), "127.0.0.1");
    }

    #[test]
    fn a_peer_name_that_is_neither_dns_nor_ip_is_rejected_at_construction() {
        for bad in [
            "",
            "not a host",
            "http://search.example.test",
            "exa_mple:9200",
        ] {
            let error = DestinationTlsConnector::from_config(&DestinationTlsConfig {
                peer_name: bad.to_owned(),
                insecure: true,
            })
            .expect_err("must be rejected");
            assert!(
                matches!(error, TlsError::InvalidPeerName { .. }),
                "{bad:?} produced {error:?}"
            );
        }
    }

    #[test]
    fn the_insecure_destination_mode_is_reported_so_it_can_be_logged_loudly() {
        let insecure = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "localhost".to_owned(),
            insecure: true,
        })
        .expect("valid");
        assert!(insecure.certificate_validation_disabled());
    }

    #[test]
    fn the_validating_destination_mode_reports_validation_enabled() {
        // Uses the platform trust store, so skip where the host has none to load.
        let validating = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "localhost".to_owned(),
            insecure: false,
        });
        match validating {
            Ok(connector) => assert!(!connector.certificate_validation_disabled()),
            Err(TlsError::NativeTrustStore { .. }) => {
                // No platform trust store on this host; nothing to assert.
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    // ---- Round trip ----

    #[tokio::test]
    async fn the_front_acceptor_and_the_insecure_connector_complete_a_handshake_and_move_bytes() {
        // This is the test that proves the two halves of the module actually interoperate: a real
        // TLS 1.3 handshake plus an application-data round trip over an in-memory duplex.
        let dir = tempfile::tempdir().expect("tempdir");
        let acceptor = FrontTlsAcceptor::from_config(&front_config(&dir)).expect("valid");
        let connector = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "localhost".to_owned(),
            insecure: true,
        })
        .expect("valid");

        let (client_side, server_side) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut stream = acceptor
                .accept(server_side)
                .await
                .expect("server handshake");
            let mut buffer = [0_u8; 5];
            stream.read_exact(&mut buffer).await.expect("server read");
            assert_eq!(&buffer, b"ping!");
            stream.write_all(b"pong!").await.expect("server write");
            stream.flush().await.expect("server flush");
        });

        let mut stream = connector
            .connect(client_side)
            .await
            .expect("client handshake");
        stream.write_all(b"ping!").await.expect("client write");
        stream.flush().await.expect("client flush");
        let mut buffer = [0_u8; 5];
        stream.read_exact(&mut buffer).await.expect("client read");
        assert_eq!(&buffer, b"pong!");

        server.await.expect("server task");
    }

    #[tokio::test]
    async fn a_client_that_does_not_trust_the_certificate_fails_the_handshake_as_a_tls_error() {
        // The validating connector cannot verify a self-signed leaf, so the handshake fails and
        // is::is_tls_handshake_error must recognise it. That is how PCI:206-215 tells a handshake
        // failure (log, dedupe, close, NO connectionException observation) apart from an ordinary
        // I/O error.
        let dir = tempfile::tempdir().expect("tempdir");
        let acceptor = FrontTlsAcceptor::from_config(&front_config(&dir)).expect("valid");
        let Ok(connector) = DestinationTlsConnector::from_config(&DestinationTlsConfig {
            peer_name: "localhost".to_owned(),
            insecure: false,
        }) else {
            // No platform trust store on this host; the assertion below needs one.
            return;
        };

        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move { acceptor.accept(server_side).await.map(|_| ()) });

        let error = connector
            .connect(client_side)
            .await
            .expect_err("an untrusted self-signed leaf must fail validation");
        assert!(
            is_tls_handshake_error(&error),
            "expected a TLS handshake error, got {error:?}"
        );

        // The server side sees the alert and also fails; it must not be mistaken for anything else.
        if let Err(server_error) = server.await.expect("server task") {
            assert!(is_tls_handshake_error(&server_error));
        }
    }

    #[test]
    fn an_ordinary_io_error_is_not_a_tls_handshake_error() {
        let plain = io::Error::from(io::ErrorKind::ConnectionReset);
        assert!(!is_tls_handshake_error(&plain));

        let wrapped_other = io::Error::other("some transport failure");
        assert!(!is_tls_handshake_error(&wrapped_other));
    }

    #[test]
    fn a_rustls_error_is_found_through_the_whole_source_chain() {
        /// One level of unrelated wrapping around the I/O error that carries the TLS failure.
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] io::Error);

        // PCI.containsCause walks the entire getCause() chain (PCI:234-243), so wrapping must not
        // hide the TLS origin.
        let inner = io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::General("handshake exploded".to_owned()),
        );
        assert!(is_tls_handshake_error(&inner));

        let nested = io::Error::other(Outer(inner));
        assert!(is_tls_handshake_error(&nested));
    }
}
