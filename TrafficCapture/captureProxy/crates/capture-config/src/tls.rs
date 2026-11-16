//! TLS configuration inputs for the client-facing listener (spec §1.1 rows 2-6).
//!
//! This module **resolves and validates** the TLS parameters; it does not read a PEM file or build
//! a `rustls` configuration. Loading lives in `capture_transport::tls`, which this crate cannot
//! depend on. The split matters for one reason: in Java the equivalent checks run inside
//! `buildSslEngineSupplier` at `CaptureProxy.java:577`, **after** `parseArgs` has already returned
//! at `:562`, so the `ParameterException`s they raise escape the exit-2 handler and surface as a
//! bare stack trace with exit 1 (spec discrepancy #3). Here they are part of configuration
//! validation and report the same exit code as every other configuration error.
//!
//! # The rules
//!
//! | Rule | Java |
//! |---|---|
//! | A certificate chain requires a key | `CaptureProxy.java:531-533` |
//! | At most one of a trust file or a trust-PEM environment variable | `CaptureProxy.java:477-479` |
//! | `--requireClientAuth` requires one of them | `CaptureProxy.java:491-494` |
//! | The named environment variable must exist and be non-empty | `CaptureProxy.java:499-509` |
//!
//! # There is no `OPTIONAL` client-auth mode
//!
//! Netty's `ClientAuth.OPTIONAL` is never reachable in the Java proxy: `CaptureProxy.java:491-497`
//! selects `REQUIRE` when `--requireClientAuth` is set and `NONE` otherwise, and nothing else
//! writes that field. [`ClientAuth`] therefore has two variants and no third mode is invented.
//!
//! # Trust material by value
//!
//! `--sslTrustCertPemEnvVar` does not carry a PEM; it carries the **name** of an environment
//! variable that does. The Kubernetes path always sets `CAPTURE_PROXY_SSL_TRUST_CERT_PEM` and names
//! it here (`setupCapture.ts:767-771`, spec §10.1). The resolved PEM is held as a
//! [`SecretString`]: it is not a private key, but it is configuration an operator supplied by
//! value, and a multi-kilobyte blob in a log line is useless noise at best.

use std::path::{Path, PathBuf};

use crate::cli::{EnvLookup, SecretString};

/// Whether a client certificate is required.
///
/// Two variants, because Java reaches only two; see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientAuth {
    /// No client certificate is requested. The default.
    #[default]
    None,
    /// A valid client certificate is required; the handshake fails without one.
    Require,
}

impl ClientAuth {
    /// Whether a client certificate is required.
    #[must_use]
    pub fn is_required(self) -> bool {
        matches!(self, Self::Require)
    }
}

/// Where client-certificate trust anchors come from.
///
/// Holding one or the other makes the "both set" error of `CaptureProxy.java:477-479`
/// unrepresentable once resolution has succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustMaterial {
    /// `--sslTrustCertFile`: a path to a PEM file.
    File(PathBuf),
    /// `--sslTrustCertPemEnvVar`: a PEM supplied by value through a named environment variable.
    InlinePem {
        /// The environment variable that carried it, retained for diagnostics.
        env_var: String,
        /// The PEM itself, never rendered.
        pem: SecretString,
    },
}

impl TrustMaterial {
    /// A human-readable description of where the material came from, safe to log.
    #[must_use]
    pub fn source_name(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::InlinePem { env_var, .. } => format!("the PEM in ${env_var}"),
        }
    }
}

/// Resolved, validated TLS inputs for the client-facing listener.
///
/// TLS is enabled if and only if a certificate chain is configured (`CaptureProxy.java:528`), so
/// the absence of this struct *is* the plaintext listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontsideTls {
    cert_chain_file: PathBuf,
    key_file: PathBuf,
    trust: Option<TrustMaterial>,
    client_auth: ClientAuth,
}

impl FrontsideTls {
    /// `--sslCertChainFile`.
    #[must_use]
    pub fn cert_chain_file(&self) -> &Path {
        &self.cert_chain_file
    }

    /// `--sslKeyFile`.
    #[must_use]
    pub fn key_file(&self) -> &Path {
        &self.key_file
    }

    /// The client-certificate trust anchors, if mutual TLS is configured.
    #[must_use]
    pub fn trust(&self) -> Option<&TrustMaterial> {
        self.trust.as_ref()
    }

    /// Whether a client certificate is required.
    #[must_use]
    pub fn client_auth(&self) -> ClientAuth {
        self.client_auth
    }
}

/// The TLS parameters as configured, before resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsInputs {
    /// `--sslCertChainFile`.
    pub cert_chain_file: Option<PathBuf>,
    /// `--sslKeyFile`.
    pub key_file: Option<PathBuf>,
    /// `--sslTrustCertFile`.
    pub trust_cert_file: Option<PathBuf>,
    /// `--sslTrustCertPemEnvVar`: the **name** of an environment variable.
    pub trust_cert_pem_env_var: Option<String>,
    /// `--requireClientAuth`.
    pub require_client_auth: bool,
}

impl TlsInputs {
    /// Resolves the TLS parameters, reading the named trust-PEM environment variable through `env`.
    ///
    /// Returns `None` when no certificate chain is configured, i.e. a plaintext listener.
    ///
    /// # Errors
    ///
    /// Every variant of [`TlsConfigError`]. Three of them are stricter than Java, which silently
    /// ignores the misconfiguration instead:
    ///
    /// * [`TlsConfigError::KeyWithoutCertificateChain`] — Java tests only the certificate chain
    ///   (`CaptureProxy.java:528`), so a key configured without one disables TLS and says nothing.
    /// * [`TlsConfigError::ClientTrustWithoutTls`] and
    ///   [`TlsConfigError::ClientAuthWithoutTls`] — the trust and client-auth checks live inside
    ///   `loadSslEngineFromPem` (`CaptureProxy.java:471-497`), which is never called when no
    ///   certificate chain is configured. A deployment that asks for mutual TLS and gets a
    ///   plaintext listener would be a silent security downgrade.
    pub fn resolve(&self, env: &dyn EnvLookup) -> Result<Option<FrontsideTls>, TlsConfigError> {
        // Java treats an empty string as absent (CaptureProxy.java:475-476, :528).
        let cert_chain_file = non_empty_path(self.cert_chain_file.as_ref());
        let key_file = non_empty_path(self.key_file.as_ref());
        let trust_cert_file = non_empty_path(self.trust_cert_file.as_ref());
        let trust_env_var = self
            .trust_cert_pem_env_var
            .as_deref()
            .filter(|name| !name.is_empty());

        let trust = match (trust_cert_file, trust_env_var) {
            (Some(_), Some(_)) => return Err(TlsConfigError::ConflictingClientTrust),
            (Some(path), None) => Some(TrustMaterial::File(path)),
            (None, Some(env_var)) => {
                let pem = env
                    .get(env_var)
                    .filter(|pem| !pem.trim().is_empty())
                    .ok_or(TlsConfigError::MissingTrustPem {
                        env_var: env_var.to_owned(),
                    })?;
                Some(TrustMaterial::InlinePem {
                    env_var: env_var.to_owned(),
                    pem: SecretString::new(pem),
                })
            }
            (None, None) => None,
        };

        let Some(cert_chain_file) = cert_chain_file else {
            if key_file.is_some() {
                return Err(TlsConfigError::KeyWithoutCertificateChain);
            }
            if let Some(trust) = trust {
                return Err(TlsConfigError::ClientTrustWithoutTls {
                    source_name: trust.source_name(),
                });
            }
            if self.require_client_auth {
                return Err(TlsConfigError::ClientAuthWithoutTls);
            }
            return Ok(None);
        };

        let key_file = key_file.ok_or(TlsConfigError::CertificateChainWithoutKey)?;

        if self.require_client_auth && trust.is_none() {
            return Err(TlsConfigError::ClientAuthWithoutTrustMaterial);
        }

        Ok(Some(FrontsideTls {
            cert_chain_file,
            key_file,
            trust,
            client_auth: if self.require_client_auth {
                ClientAuth::Require
            } else {
                ClientAuth::None
            },
        }))
    }
}

fn non_empty_path(path: Option<&PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty()).cloned()
}

/// The TLS parameters are inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TlsConfigError {
    /// A certificate chain was configured without a key.
    #[error("--ssl-cert-chain-file requires --ssl-key-file")]
    CertificateChainWithoutKey,
    /// A key was configured without a certificate chain.
    #[error(
        "--ssl-key-file requires --ssl-cert-chain-file; a key alone would leave the listener in \
         plaintext"
    )]
    KeyWithoutCertificateChain,
    /// Both trust sources were configured.
    #[error("only one of --ssl-trust-cert-file or --ssl-trust-cert-pem-env-var may be specified")]
    ConflictingClientTrust,
    /// The named trust-PEM environment variable is unset or empty.
    #[error(
        "--ssl-trust-cert-pem-env-var names ${env_var}, which is unset or empty; \
         client trust material cannot be loaded"
    )]
    MissingTrustPem {
        /// The environment variable that was named.
        env_var: String,
    },
    /// Client authentication was required without any trust material.
    #[error(
        "--require-client-auth requires --ssl-trust-cert-file or --ssl-trust-cert-pem-env-var to \
         supply the trusted certificate authority for client certificates"
    )]
    ClientAuthWithoutTrustMaterial,
    /// Client trust material was configured for a plaintext listener.
    #[error(
        "client trust material ({source_name}) was configured without --ssl-cert-chain-file, so \
         the listener would be plaintext and the material would never be used"
    )]
    ClientTrustWithoutTls {
        /// Where the unused material came from.
        source_name: String,
    },
    /// Client authentication was required for a plaintext listener.
    #[error(
        "--require-client-auth was set without --ssl-cert-chain-file, so the listener would be \
         plaintext and no client certificate could ever be requested"
    )]
    ClientAuthWithoutTls,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A stub environment, so no test mutates the process environment.
    #[derive(Default)]
    struct StubEnv(HashMap<String, String>);

    impl StubEnv {
        fn with(name: &str, value: &str) -> Self {
            let mut map = HashMap::new();
            map.insert(name.to_owned(), value.to_owned());
            Self(map)
        }
    }

    impl EnvLookup for StubEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    fn inputs() -> TlsInputs {
        TlsInputs {
            cert_chain_file: Some(PathBuf::from("/etc/proxy-tls/tls.crt")),
            key_file: Some(PathBuf::from("/etc/proxy-tls/tls.key")),
            ..TlsInputs::default()
        }
    }

    #[test]
    fn no_certificate_chain_means_a_plaintext_listener() {
        let resolved = TlsInputs::default()
            .resolve(&StubEnv::default())
            .expect("valid");
        assert!(resolved.is_none());
    }

    #[test]
    fn an_empty_path_counts_as_absent() {
        // CaptureProxy.java:475-476, :528 all test isEmpty(), and the orchestration emits empty
        // strings for unset optional values.
        let resolved = TlsInputs {
            cert_chain_file: Some(PathBuf::new()),
            key_file: Some(PathBuf::new()),
            trust_cert_file: Some(PathBuf::new()),
            trust_cert_pem_env_var: Some(String::new()),
            require_client_auth: false,
        }
        .resolve(&StubEnv::default())
        .expect("all empty means plaintext");
        assert!(resolved.is_none());
    }

    #[test]
    fn a_certificate_chain_and_key_enable_tls_without_client_auth() {
        let tls = inputs()
            .resolve(&StubEnv::default())
            .expect("valid")
            .expect("TLS enabled");
        assert_eq!(tls.cert_chain_file(), Path::new("/etc/proxy-tls/tls.crt"));
        assert_eq!(tls.key_file(), Path::new("/etc/proxy-tls/tls.key"));
        assert_eq!(tls.client_auth(), ClientAuth::None);
        assert!(!tls.client_auth().is_required());
        assert!(tls.trust().is_none());
    }

    #[test]
    fn a_certificate_chain_without_a_key_is_rejected() {
        // CaptureProxy.java:531-533.
        let error = TlsInputs {
            key_file: None,
            ..inputs()
        }
        .resolve(&StubEnv::default())
        .expect_err("a chain needs a key");
        assert_eq!(error, TlsConfigError::CertificateChainWithoutKey);
    }

    #[test]
    fn a_key_without_a_certificate_chain_is_rejected_rather_than_ignored() {
        let error = TlsInputs {
            cert_chain_file: None,
            ..inputs()
        }
        .resolve(&StubEnv::default())
        .expect_err("a key alone must not silently disable TLS");
        assert_eq!(error, TlsConfigError::KeyWithoutCertificateChain);
    }

    #[test]
    fn trust_material_may_be_a_file() {
        let tls = TlsInputs {
            trust_cert_file: Some(PathBuf::from("/config/kafka-ca/ca.crt")),
            require_client_auth: true,
            ..inputs()
        }
        .resolve(&StubEnv::default())
        .expect("valid")
        .expect("TLS enabled");
        assert_eq!(
            tls.trust(),
            Some(&TrustMaterial::File(PathBuf::from(
                "/config/kafka-ca/ca.crt"
            )))
        );
        assert_eq!(tls.client_auth(), ClientAuth::Require);
    }

    #[test]
    fn trust_material_may_arrive_by_value_through_a_named_environment_variable() {
        // CaptureProxy.java:499-509, and the only shape the Kubernetes path uses.
        let env = StubEnv::with("CAPTURE_PROXY_SSL_TRUST_CERT_PEM", PEM);
        let tls = TlsInputs {
            trust_cert_pem_env_var: Some("CAPTURE_PROXY_SSL_TRUST_CERT_PEM".to_owned()),
            require_client_auth: true,
            ..inputs()
        }
        .resolve(&env)
        .expect("valid")
        .expect("TLS enabled");
        match tls.trust().expect("trust material") {
            TrustMaterial::InlinePem { env_var, pem } => {
                assert_eq!(env_var, "CAPTURE_PROXY_SSL_TRUST_CERT_PEM");
                assert_eq!(pem.expose(), PEM);
            }
            other @ TrustMaterial::File(_) => panic!("expected an inline PEM, got {other:?}"),
        }
    }

    #[test]
    fn the_trust_pem_is_not_rendered_in_debug_output() {
        let env = StubEnv::with("TRUST_PEM", PEM);
        let tls = TlsInputs {
            trust_cert_pem_env_var: Some("TRUST_PEM".to_owned()),
            ..inputs()
        }
        .resolve(&env)
        .expect("valid")
        .expect("TLS enabled");
        let rendered = format!("{tls:?}");
        assert!(
            !rendered.contains("BEGIN CERTIFICATE"),
            "the trust PEM leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains("TRUST_PEM"), "the name is still useful");
    }

    #[test]
    fn the_two_trust_sources_are_mutually_exclusive() {
        // CaptureProxy.java:477-479.
        let error = TlsInputs {
            trust_cert_file: Some(PathBuf::from("/ca.crt")),
            trust_cert_pem_env_var: Some("TRUST_PEM".to_owned()),
            ..inputs()
        }
        .resolve(&StubEnv::with("TRUST_PEM", PEM))
        .expect_err("both trust sources must be refused");
        assert_eq!(error, TlsConfigError::ConflictingClientTrust);
    }

    #[test]
    fn a_named_environment_variable_that_is_unset_or_empty_is_rejected() {
        // CaptureProxy.java:505-507.
        for env in [StubEnv::default(), StubEnv::with("TRUST_PEM", "   ")] {
            let error = TlsInputs {
                trust_cert_pem_env_var: Some("TRUST_PEM".to_owned()),
                ..inputs()
            }
            .resolve(&env)
            .expect_err("an empty PEM is not trust material");
            assert_eq!(
                error,
                TlsConfigError::MissingTrustPem {
                    env_var: "TRUST_PEM".to_owned()
                }
            );
        }
    }

    #[test]
    fn requiring_client_auth_without_trust_material_is_rejected() {
        // CaptureProxy.java:491-494.
        let error = TlsInputs {
            require_client_auth: true,
            ..inputs()
        }
        .resolve(&StubEnv::default())
        .expect_err("client auth needs trust material");
        assert_eq!(error, TlsConfigError::ClientAuthWithoutTrustMaterial);
    }

    #[test]
    fn mutual_tls_configured_for_a_plaintext_listener_is_rejected() {
        // Java never runs these checks without a certificate chain, so the operator gets a
        // plaintext listener and no warning.
        let error = TlsInputs {
            cert_chain_file: None,
            key_file: None,
            trust_cert_file: Some(PathBuf::from("/ca.crt")),
            ..TlsInputs::default()
        }
        .resolve(&StubEnv::default())
        .expect_err("trust material without TLS must be refused");
        assert!(matches!(
            error,
            TlsConfigError::ClientTrustWithoutTls { .. }
        ));

        let error = TlsInputs {
            require_client_auth: true,
            ..TlsInputs::default()
        }
        .resolve(&StubEnv::default())
        .expect_err("client auth without TLS must be refused");
        assert_eq!(error, TlsConfigError::ClientAuthWithoutTls);
    }

    #[test]
    fn there_are_exactly_two_client_auth_modes() {
        // Netty's ClientAuth.OPTIONAL is unreachable in the Java proxy, so no third mode exists.
        assert_eq!(ClientAuth::default(), ClientAuth::None);
        assert!(!ClientAuth::None.is_required());
        assert!(ClientAuth::Require.is_required());
    }
}
