//! AWS MSK IAM authentication over SASL/OAUTHBEARER (traceability row KAFKA-09).
//!
//! # Why OAUTHBEARER and not `AWS_MSK_IAM`
//!
//! The Java proxy sets `sasl.mechanism=AWS_MSK_IAM` with an `IAMLoginModule` JAAS config and an
//! `IAMClientCallbackHandler`. **librdkafka implements none of that**: its mechanism list is
//! `GSSAPI, PLAIN, SCRAM-SHA-256, SCRAM-SHA-512, OAUTHBEARER`, it has no JAAS, and it has no
//! callback-handler class. MSK accepts SASL/OAUTHBEARER with a `SigV4`-presigned token instead, and
//! that is what this module produces.
//!
//! This was proved end-to-end against a live MSK Serverless cluster, not reasoned about: a real
//! produce with real delivery reports, with the token generated exactly as below. MSK Serverless
//! offers **no** non-IAM listener — `get-bootstrap-brokers` returns only
//! `BootstrapBrokerStringSaslIam` — so for such a deployment this is the only way in.
//!
//! # The two traps, each of which cost a failed run
//!
//! 1. [`ClientContext::ENABLE_REFRESH_OAUTH_TOKEN`] must be `true`. Without it librdkafka never
//!    calls `generate_oauth_token` at all, and the client simply waits until its request timeout
//!    with no diagnostic.
//! 2. The refresh callback **must not** block on an async runtime. librdkafka invokes it from a
//!    thread that is already a tokio worker on the consumer path, and `Runtime::block_on` there
//!    panics with *"Cannot start a runtime from within a runtime"*. It reproduced only on the
//!    consumer path, which is what makes it an easy trap to ship. Credential resolution therefore
//!    runs on a dedicated OS thread that this module owns, and the callback blocks on a plain
//!    `std` channel, which is safe from any thread. See [`CredentialResolver`].
//!
//! # Token format
//!
//! Base64url, **no padding**, of a `SigV4`-presigned
//! `GET https://kafka.<region>.amazonaws.com/?Action=kafka-cluster:Connect`:
//!
//! | Property | Value |
//! |---|---|
//! | signing service name | `kafka-cluster`, **not** `kafka` |
//! | host | `kafka.<region>.amazonaws.com`, the regional control-plane host, not the broker host |
//! | method / path | `GET` `/`, with the SHA-256 of an empty body |
//! | `Action` | added **before** signing, so the signature covers it |
//! | `X-Amz-Expires` | 900 seconds; MSK rejects longer |
//! | `X-Amz-SignedHeaders` | `host`, which must be signed or the URL will not validate |
//! | `User-Agent` | appended **after** signing, deliberately not covered by the signature |
//! | token lifetime | `X-Amz-Date` plus `X-Amz-Expires`, handed to librdkafka as `lifetime_ms` |
//!
//! `principal_name` is left empty: MSK derives the principal from the `SigV4` signature, and
//! librdkafka performs no validation on the field beyond requiring it to be non-null.
//!
//! # What is self-contained here, and why
//!
//! `aws-sigv4`, `aws-config`, and `aws-credential-types` are **not** in the workspace's shared
//! dependency set, and this crate may not add to it. `SigV4` presigning is a fully specified
//! construction over SHA-256 and HMAC-SHA256, so it is implemented here and pinned to published
//! test vectors: FIPS 180-4 for SHA-256, RFC 4231 for HMAC-SHA256, and the AWS documentation's own
//! signing-key derivation example. A wrong implementation fails those vectors.
//!
//! **Credential *sourcing* is a trait.** [`EnvCredentialsProvider`] reads the standard environment
//! variables, which covers a container with injected credentials. `IMDSv2` and the IRSA
//! web-identity chain need an HTTP client and `aws-config`; inject a provider that wraps it when
//! that dependency is available. Nothing else changes: the token construction and the librdkafka
//! wiring are credential-source agnostic.

use std::error::Error;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rdkafka::client::OAuthToken;

use crate::error::ClientConfigError;

/// The `SigV4` signing service name. Not `kafka`.
pub const SIGNING_SERVICE: &str = "kafka-cluster";

/// The signed action.
pub const ACTION_VALUE: &str = "kafka-cluster:Connect";

/// The presigned URL's validity, in seconds. MSK rejects a longer value.
pub const DEFAULT_EXPIRES_SECONDS: u64 = 900;

/// The default `User-Agent`, appended after signing.
pub const DEFAULT_USER_AGENT: &str = "rustproxy-msk-iam-signer/1.0/rust";

/// MSK IAM configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MskIamSettings {
    /// The AWS region of the MSK cluster.
    pub region: String,
    /// The presigned URL's validity in seconds.
    pub expires_seconds: u64,
    /// The `User-Agent` query parameter value.
    pub user_agent: String,
}

impl MskIamSettings {
    /// Settings for `region` with the documented defaults.
    #[must_use]
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            expires_seconds: DEFAULT_EXPIRES_SECONDS,
            user_agent: DEFAULT_USER_AGENT.to_owned(),
        }
    }

    /// The regional control-plane host the token is signed against.
    #[must_use]
    pub fn host(&self) -> String {
        format!("kafka.{}.amazonaws.com", self.region)
    }

    /// Validates the settings.
    ///
    /// # Errors
    ///
    /// Rejects an empty region, and an expiry outside `1..=900` — MSK refuses a longer one, and the
    /// failure surfaces as an opaque authentication error rather than a configuration error.
    pub fn validate(&self) -> Result<(), ClientConfigError> {
        if self.region.trim().is_empty() {
            return Err(ClientConfigError::MissingCredential {
                mode: "msk-iam",
                missing: "an AWS region",
            });
        }
        if self.expires_seconds == 0 || self.expires_seconds > DEFAULT_EXPIRES_SECONDS {
            return Err(ClientConfigError::AuthUnavailable {
                mode: "msk-iam",
                detail: format!(
                    "X-Amz-Expires must be between 1 and {DEFAULT_EXPIRES_SECONDS} seconds, not \
                     {}; MSK rejects a longer presigned URL and the refusal surfaces as an opaque \
                     authentication failure",
                    self.expires_seconds
                ),
            });
        }
        Ok(())
    }
}

/// AWS credentials used to sign a token.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub access_key_id: String,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret_access_key: String,
    /// `AWS_SESSION_TOKEN`, present for any temporary credential.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    /// Redacts the secret and the session token.
    ///
    /// A credential must not reach a log through a stray `{:?}`, and the only reliable way to
    /// guarantee that is to make the type unable to print it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &Redacted(self.access_key_id.len()))
            .field("secret_access_key", &Redacted(self.secret_access_key.len()))
            .field("session_token_present", &self.session_token.is_some())
            .finish()
    }
}

struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<REDACTED {} chars>", self.0)
    }
}

/// Resolves AWS credentials.
///
/// Implementations may block: they are always called from a thread this crate owns, never from
/// librdkafka's callback thread directly.
pub trait AwsCredentialsProvider: Send + Sync {
    /// Resolves the current credentials.
    ///
    /// # Errors
    ///
    /// Returns a message describing why credentials are unavailable. The message reaches
    /// librdkafka's token-refresh failure path, so it must not contain a null byte.
    fn credentials(&self) -> Result<AwsCredentials, String>;
}

/// Reads credentials from the standard environment variables.
///
/// Covers a container or pod with injected credentials. It does **not** implement the `IMDSv2` or
/// IRSA web-identity chain: both need an HTTP client. Wrap `aws-config`'s provider chain in a
/// provider of your own for those.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvCredentialsProvider;

impl AwsCredentialsProvider for EnvCredentialsProvider {
    fn credentials(&self) -> Result<AwsCredentials, String> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| "AWS_ACCESS_KEY_ID is not set".to_owned())?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| "AWS_SECRET_ACCESS_KEY is not set".to_owned())?;
        Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
        })
    }
}

/// Resolves credentials through `aws-config`'s **default provider chain**.
///
/// This is the provider production uses, and it is what makes IRSA and `IMDSv2` work with **no proxy
/// code change**: the chain already contains the web-identity provider (`AWS_ROLE_ARN` plus
/// `AWS_WEB_IDENTITY_TOKEN_FILE`, projected by the kubelet) and the `IMDSv2` provider, and it picks
/// whichever the ambient environment supplies.
///
/// # Thread discipline, which is the whole point
///
/// Resolution is async, so this provider owns a current-thread tokio runtime and blocks on it. That
/// is **only** safe off a tokio worker thread, which is exactly why [`CredentialResolver`] exists:
/// librdkafka invokes the token-refresh callback from a thread that is already a tokio worker on
/// the consumer path, and `block_on` there panics with *"Cannot start a runtime from within a
/// runtime"*. Use this provider **only** through [`MskIamTokenSource`], which routes every call
/// through the resolver thread.
///
/// Calling it from inside a runtime returns an error rather than panicking, so the misuse is
/// diagnosable instead of fatal.
///
/// # Required `aws-config` feature
///
/// The chain's web-identity (IRSA) and `IMDSv2` providers both make HTTP calls, so `aws-config`
/// **must** be built with an HTTP client. Without one, resolution does not return an error — it
/// **panics** inside `aws-smithy-runtime`:
///
/// ```text
/// a http_client is required. Enable the `default-https-client` crate feature or configure an
/// HTTP client to fix this.
/// ```
///
/// Observed, not inferred: with `aws-config = { default-features = false, features =
/// ["behavior-version-latest", "rt-tokio"] }` this is exactly what happens. Under the release
/// profile's `panic = "abort"` that is a process abort rather than a capture failure, so the
/// workspace entry needs `"default-https-client"` (or an explicitly configured client) before the
/// MSK IAM path can work at all. [`MskIamTokenSource::start`] performs a preflight resolution so
/// the problem surfaces at startup, before the listener opens, rather than at the first token
/// refresh fifteen minutes into a run.
pub struct AwsConfigCredentialsProvider {
    runtime: tokio::runtime::Runtime,
    provider: std::sync::OnceLock<aws_credential_types::provider::SharedCredentialsProvider>,
}

impl std::fmt::Debug for AwsConfigCredentialsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsConfigCredentialsProvider")
            .field("chain_loaded", &self.provider.get().is_some())
            .finish_non_exhaustive()
    }
}

impl AwsConfigCredentialsProvider {
    /// Builds the provider, deferring the chain load until the first resolution.
    ///
    /// The chain is loaded lazily and then cached, because loading it performs discovery (IMDS
    /// probing, token-file reads) that must not happen once per token mint — librdkafka re-mints
    /// roughly every 15 minutes for the life of the process.
    ///
    /// # Errors
    ///
    /// Returns a message if the single-threaded runtime cannot be created.
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not build the AWS credential runtime: {error}"))?;
        Ok(Self {
            runtime,
            provider: std::sync::OnceLock::new(),
        })
    }
}

impl AwsCredentialsProvider for AwsConfigCredentialsProvider {
    fn credentials(&self) -> Result<AwsCredentials, String> {
        use aws_credential_types::provider::ProvideCredentials as _;

        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(
                "the MSK IAM credential provider was called from inside a tokio runtime; it must \
                 only be used through MskIamTokenSource, which resolves on a dedicated OS thread"
                    .to_owned(),
            );
        }

        let provider = if let Some(provider) = self.provider.get() {
            provider.clone()
        } else {
            let loaded = self.runtime.block_on(async {
                aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await
            });
            let provider = loaded
                .credentials_provider()
                .ok_or_else(|| {
                    "the AWS default provider chain resolved no credentials provider; set \
                     credentials in the environment, attach an instance profile, or configure IRSA"
                        .to_owned()
                })?
                .clone();
            // A concurrent initialiser having won is fine: both values are equivalent.
            let _ = self.provider.set(provider.clone());
            provider
        };

        let resolved = self
            .runtime
            .block_on(provider.provide_credentials())
            .map_err(|error| format!("the AWS default provider chain failed: {error}"))?;

        Ok(AwsCredentials {
            access_key_id: resolved.access_key_id().to_owned(),
            secret_access_key: resolved.secret_access_key().to_owned(),
            session_token: resolved.session_token().map(ToOwned::to_owned),
        })
    }
}

/// Fixed credentials, for tests and for an explicitly configured deployment.
#[derive(Debug, Clone)]
pub struct StaticCredentialsProvider(pub AwsCredentials);

impl AwsCredentialsProvider for StaticCredentialsProvider {
    fn credentials(&self) -> Result<AwsCredentials, String> {
        Ok(self.0.clone())
    }
}

type CredentialReply = mpsc::Sender<Result<AwsCredentials, String>>;

/// Resolves credentials on a dedicated OS thread.
///
/// # Why a thread and a channel
///
/// librdkafka may invoke the OAUTHBEARER refresh callback from a thread that is **already** a tokio
/// worker — this happens on the consumer path — so a callback that drives an async credential
/// provider with `Runtime::block_on` panics with *"Cannot start a runtime from within a runtime"*.
/// Blocking on a plain `std` channel is safe from any thread, so the resolution is pushed onto a
/// thread this type owns. The producer path does not reproduce the panic, which is precisely why
/// this must be designed in rather than discovered.
pub struct CredentialResolver {
    requests: Mutex<Option<mpsc::Sender<CredentialReply>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    resolutions: AtomicU64,
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialResolver")
            .field("resolutions", &self.resolutions.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl CredentialResolver {
    /// Starts the resolver thread.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the thread cannot be spawned.
    pub fn start(provider: Box<dyn AwsCredentialsProvider>) -> std::io::Result<Self> {
        let (requests, incoming) = mpsc::channel::<CredentialReply>();
        let thread = std::thread::Builder::new()
            .name("capture-kafka-msk-iam-creds".to_owned())
            .spawn(move || {
                for reply in incoming {
                    // A closed reply channel means the caller gave up; there is nothing to report.
                    let _ = reply.send(provider.credentials());
                }
            })?;
        Ok(Self {
            requests: Mutex::new(Some(requests)),
            thread: Some(thread),
            resolutions: AtomicU64::new(0),
        })
    }

    /// Resolves credentials, blocking the caller until the resolver thread answers.
    ///
    /// # Errors
    ///
    /// Returns a message when the resolver thread is gone or the provider failed.
    pub fn credentials(&self) -> Result<AwsCredentials, String> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or_else(|| "the MSK IAM credential resolver is shutting down".to_owned())?
            .send(reply)
            .map_err(|_| "the MSK IAM credential resolver thread has stopped".to_owned())?;
        let credentials = answer
            .recv()
            .map_err(|_| "the MSK IAM credential resolver thread dropped the request".to_owned())?;
        self.resolutions.fetch_add(1, Ordering::Relaxed);
        credentials
    }

    /// How many resolutions have been requested, for diagnostics.
    #[must_use]
    pub fn resolutions(&self) -> u64 {
        self.resolutions.load(Ordering::Relaxed)
    }
}

impl Drop for CredentialResolver {
    fn drop(&mut self) {
        // The sender must be *taken out* of the mutex, not merely have its guard dropped: the
        // resolver thread's `for reply in incoming` loop only ends when the last sender is gone,
        // so leaving it inside the mutex would make the join below hang forever.
        drop(
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        if let Some(thread) = self.thread.take() {
            // The resolver thread only ever runs provider code; a panic there is the provider's
            // and is surfaced as a token-refresh failure, not swallowed state.
            drop(thread.join());
        }
    }
}

/// Mints OAUTHBEARER tokens for librdkafka.
///
/// Held by every client context — producer and consumer both need their own refresh callback.
pub struct MskIamTokenSource {
    settings: MskIamSettings,
    resolver: CredentialResolver,
}

impl std::fmt::Debug for MskIamTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MskIamTokenSource")
            .field("region", &self.settings.region)
            .field("resolver", &self.resolver)
            .finish_non_exhaustive()
    }
}

impl MskIamTokenSource {
    /// Starts a token source for `settings` using `provider`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for invalid settings, or if the resolver thread cannot start.
    pub fn start(
        settings: MskIamSettings,
        provider: Box<dyn AwsCredentialsProvider>,
    ) -> Result<Self, ClientConfigError> {
        settings.validate()?;
        let resolver = CredentialResolver::start(provider).map_err(|error| {
            ClientConfigError::AuthUnavailable {
                mode: "msk-iam",
                detail: format!("could not start the credential resolver thread: {error}"),
            }
        })?;

        // Preflight: resolve once, and sign once, here at startup.
        //
        // Without this, an unusable credential chain or an unsignable credential is not discovered
        // until librdkafka first asks for a token — which is after the listener has opened, and
        // then again roughly every fifteen minutes for the life of the process. Failing here
        // instead means an MSK IAM misconfiguration refuses startup, before any traffic can be
        // accepted, which is what the capture protocol requires of a configuration fault.
        let source = Self { settings, resolver };
        source
            .token()
            .map_err(|error| ClientConfigError::AuthUnavailable {
                mode: "msk-iam",
                detail: format!(
                    "could not mint an MSK IAM token at startup: {error}. Check that credentials \
                     are reachable (environment, instance profile, or IRSA) and that aws-config is \
                     built with an HTTP client feature such as \"default-https-client\", which its \
                     web-identity and IMDSv2 providers both require."
                ),
            })?;
        Ok(source)
    }

    /// Mints a token for librdkafka's `generate_oauth_token` callback.
    ///
    /// # Errors
    ///
    /// Returns a message when credentials are unavailable or signing failed. The message must not
    /// contain a null byte, which it cannot: every component is ASCII.
    pub fn token(&self) -> Result<OAuthToken, Box<dyn Error>> {
        let credentials = self.resolver.credentials()?;
        let signed = presign_msk_token(&credentials, &self.settings, SystemTime::now())?;
        Ok(OAuthToken {
            token: signed.token,
            // MSK derives the principal from the SigV4 signature. librdkafka performs no
            // validation on this field beyond requiring it to be non-null.
            principal_name: String::new(),
            lifetime_ms: signed.lifetime_ms,
        })
    }

    /// Starts a token source over `aws-config`'s default provider chain.
    ///
    /// The production constructor: the chain supplies IRSA and `IMDSv2` without code changes, and
    /// resolution is confined to the dedicated resolver thread.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for invalid settings, or if the runtime or the resolver thread
    /// cannot start.
    pub fn from_default_chain(settings: MskIamSettings) -> Result<Self, ClientConfigError> {
        let provider = AwsConfigCredentialsProvider::new().map_err(|detail| {
            ClientConfigError::AuthUnavailable {
                mode: "msk-iam",
                detail,
            }
        })?;
        Self::start(settings, Box::new(provider))
    }

    /// The configured region.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.settings.region
    }
}

/// A minted MSK IAM token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedToken {
    /// The base64url, unpadded token handed to librdkafka.
    pub token: String,
    /// When the token expires, in milliseconds since the Unix epoch.
    pub lifetime_ms: i64,
    /// The presigned URL before encoding. Returned for tests and diagnostics; it contains a
    /// signature and a session token, so it must never be logged.
    pub presigned_url: String,
}

/// Builds the MSK IAM auth token.
///
/// # Errors
///
/// Returns a message when `signed_at` precedes the Unix epoch or the credentials are unusable.
pub fn presign_msk_token(
    credentials: &AwsCredentials,
    settings: &MskIamSettings,
    signed_at: SystemTime,
) -> Result<SignedToken, String> {
    if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
        return Err("MSK IAM requires a non-empty access key id and secret access key".to_owned());
    }
    let signed_at_secs = i64::try_from(
        signed_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "the system clock precedes the Unix epoch".to_owned())?
            .as_secs(),
    )
    .map_err(|_| "the system clock is implausibly far in the future".to_owned())?;

    let amz_date = format_amz_date(signed_at_secs);
    let date_stamp = &amz_date[..8];
    let host = settings.host();
    let credential_scope = format!(
        "{date_stamp}/{}/{SIGNING_SERVICE}/aws4_request",
        settings.region
    );

    // `Action` is signed; `User-Agent` is not. Every other parameter is added by the presigning
    // itself.
    let mut query: Vec<(String, String)> = vec![
        ("Action".to_owned(), ACTION_VALUE.to_owned()),
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        (
            "X-Amz-Credential".to_owned(),
            format!("{}/{credential_scope}", credentials.access_key_id),
        ),
        ("X-Amz-Date".to_owned(), amz_date.clone()),
        (
            "X-Amz-Expires".to_owned(),
            settings.expires_seconds.to_string(),
        ),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    if let Some(session_token) = &credentials.session_token {
        // For a presigned URL the session token is part of the canonical query string, so it is
        // covered by the signature.
        query.push(("X-Amz-Security-Token".to_owned(), session_token.clone()));
    }

    let canonical_query = canonical_query_string(&query);
    let canonical_request = format!(
        "GET\n/\n{canonical_query}\nhost:{host}\n\nhost\n{}",
        hex(&sha256(&[]))
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        hex(&sha256(canonical_request.as_bytes()))
    );
    let signing_key = derive_signing_key(
        &credentials.secret_access_key,
        date_stamp,
        &settings.region,
        SIGNING_SERVICE,
    );
    let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let mut url = format!("https://{host}/?{canonical_query}&X-Amz-Signature={signature}");
    // Appended after signing, exactly as the reference signer does.
    let _ = write!(
        url,
        "&User-Agent={}",
        uri_encode(&settings.user_agent, true)
    );

    let lifetime_ms = signed_at_secs
        .checked_add(
            i64::try_from(settings.expires_seconds)
                .map_err(|_| "X-Amz-Expires is implausibly large".to_owned())?,
        )
        .and_then(|secs| secs.checked_mul(1_000))
        .ok_or_else(|| "the token lifetime overflowed".to_owned())?;

    Ok(SignedToken {
        token: URL_SAFE_NO_PAD.encode(url.as_bytes()),
        lifetime_ms,
        presigned_url: url,
    })
}

/// Builds the canonical query string: every pair URI-encoded, then sorted by encoded key and
/// value, joined with `&`.
fn canonical_query_string(query: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = query
        .iter()
        .map(|(key, value)| (uri_encode(key, true), uri_encode(value, true)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// RFC 3986 percent-encoding as `SigV4` defines it.
///
/// Unreserved characters are `A-Z a-z 0-9 - _ . ~`. Everything else is percent-encoded with
/// **uppercase** hex. `encode_slash` is false only for a canonical URI path, where `/` stays
/// literal; this signer's path is `/` so it never needs that, but the distinction is part of the
/// specification and omitting it is a classic `SigV4` bug.
fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if !encode_slash => encoded.push('/'),
            other => {
                let _ = write!(encoded, "%{other:02X}");
            }
        }
    }
    encoded
}

/// `kSigning = HMAC(HMAC(HMAC(HMAC("AWS4" + secret, date), region), service), "aws4_request")`.
fn derive_signing_key(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> [u8; 32] {
    let initial = format!("AWS4{secret_access_key}");
    let k_date = hmac_sha256(initial.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Formats `%Y%m%dT%H%M%SZ` from a Unix timestamp.
///
/// Implemented with the civil-from-days algorithm rather than a date library: the proxy must agree
/// with AWS to the second and must not gain a dependency to do it. Correct for any timestamp in the
/// proleptic Gregorian calendar.
fn format_amz_date(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Converts days since 1970-01-01 to a civil `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, shifting the era so that March is month 1 and the leap day
/// lands at the end of a 400-year era.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1)
        .expect("a day of month is always 1..=31");
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .expect("a month is always 1..=12");
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// HMAC-SHA256, RFC 2104.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0_u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&sha256(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 32);
    for byte in padded {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message);
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

/// SHA-256 round constants: the first 32 bits of the fractional parts of the cube roots of the
/// first 64 primes (FIPS 180-4 section 4.2.2).
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 initial hash value (FIPS 180-4 section 5.3.3).
const SHA256_H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// SHA-256, FIPS 180-4.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = SHA256_H0;

    // Message padding: 0x80, zeros to 56 mod 64, then the length in bits as a big-endian u64.
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks_exact(64) {
        sha256_compress(&mut h, block);
    }

    let mut digest = [0_u8; 32];
    for (chunk, word) in digest.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// One SHA-256 compression round over a 64-byte block.
fn sha256_compress(h: &mut [u32; 8], block: &[u8]) {
    let mut w = [0_u32; 64];
    for (index, word) in block.chunks_exact(4).enumerate() {
        w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }

    let mut v = *h;
    for index in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let choose = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
        let temp1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(SHA256_K[index])
            .wrapping_add(w[index]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let majority = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let temp2 = s0.wrapping_add(majority);

        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3].wrapping_add(temp1);
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = temp1.wrapping_add(temp2);
    }
    for (slot, value) in h.iter_mut().zip(v) {
        *slot = slot.wrapping_add(value);
    }
}

/// How long before expiry a token should be refreshed.
///
/// librdkafka caches a token for its stated lifetime, so a proxy running for days re-mints roughly
/// every 15 minutes. The projected web-identity token behind an IRSA credential is rotated by the
/// kubelet at about 80% of *its* lifetime, and the interaction between those two cadences is the
/// single most likely place for a long-running proxy to break hours into a run. It needs a soak
/// test, not a one-shot produce.
#[must_use]
pub fn refresh_margin() -> Duration {
    Duration::from_mins(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_fips_180_4_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Multi-block: 1 000 000 'a' characters.
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_handles_every_padding_boundary() {
        // Lengths 55/56/57 and 63/64/65 are where a padding bug hides.
        for length in [0_usize, 1, 54, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128] {
            let data = vec![b'x'; length];
            let digest = sha256(&data);
            assert_eq!(digest.len(), 32, "length {length}");
            // A second call must agree, and a one-byte change must not.
            assert_eq!(digest, sha256(&data));
            let mut changed = data.clone();
            changed.push(b'y');
            assert_ne!(digest, sha256(&changed), "length {length}");
        }
    }

    #[test]
    fn hmac_sha256_matches_the_rfc_4231_vectors() {
        // Test case 1.
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 3.
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // Test case 6: a key longer than the 64-byte block, which must be hashed first.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_signing_key_matches_the_aws_documented_derivation_example() {
        // The example from the AWS SigV4 documentation's "deriving a signing key" section.
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn uri_encoding_follows_rfc_3986_with_uppercase_hex() {
        assert_eq!(
            uri_encode("kafka-cluster:Connect", true),
            "kafka-cluster%3AConnect"
        );
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("-_.~", true), "-_.~", "unreserved stay literal");
        assert_eq!(uri_encode(" ", true), "%20", "not '+'");
        assert_eq!(uri_encode("=", true), "%3D");
    }

    #[test]
    fn amz_dates_format_correctly_across_leap_years_and_epochs() {
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        assert_eq!(format_amz_date(1), "19700101T000001Z");
        // 2015-08-30T12:36:00Z, the AWS documentation example's instant.
        assert_eq!(format_amz_date(1_440_938_160), "20150830T123600Z");
        // 2000-02-29, a leap day in a century that IS a leap year.
        assert_eq!(format_amz_date(951_782_400), "20000229T000000Z");
        // 2100 is NOT a leap year, so 02-28 is followed directly by 03-01.
        assert_eq!(format_amz_date(4_107_456_000), "21000228T000000Z");
        assert_eq!(format_amz_date(4_107_542_400), "21000301T000000Z");
        // Year and day rollovers.
        assert_eq!(format_amz_date(1_735_689_599), "20241231T235959Z");
        assert_eq!(format_amz_date(946_684_799), "19991231T235959Z");
        // 2026-11-16T01:02:09Z, the instant in the recorded MSK proof.
        assert_eq!(format_amz_date(1_794_790_929), "20261116T010209Z");
    }

    fn credentials() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: Some("session-token-value".to_owned()),
        }
    }

    fn decode(token: &str) -> String {
        String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(token)
                .expect("the token is base64url with no padding"),
        )
        .expect("the presigned URL is ASCII")
    }

    #[test]
    fn the_token_has_every_documented_property() {
        let settings = MskIamSettings::new("us-east-2");
        let signed = presign_msk_token(
            &credentials(),
            &settings,
            UNIX_EPOCH + Duration::from_secs(1_794_790_929),
        )
        .expect("signing succeeds");

        assert!(
            !signed.token.contains('='),
            "base64url with no padding: {}",
            signed.token
        );
        assert!(
            !signed.token.contains('+') && !signed.token.contains('/'),
            "url-safe charset only"
        );

        let url = decode(&signed.token);
        assert!(url.starts_with("https://kafka.us-east-2.amazonaws.com/?"));
        assert!(
            url.contains("Action=kafka-cluster%3AConnect"),
            "the action is signed, so it must be in the canonical query: {url}"
        );
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(
            url.contains("%2Fus-east-2%2Fkafka-cluster%2Faws4_request"),
            "the credential scope names the kafka-cluster service, not kafka: {url}"
        );
        assert!(url.contains("X-Amz-Date=20261116T010209Z"));
        assert!(url.contains("X-Amz-Expires=900"));
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        assert!(url.contains("X-Amz-Security-Token=session-token-value"));
        assert!(url.contains("X-Amz-Signature="));

        assert_eq!(
            signed.lifetime_ms,
            (1_794_790_929 + 900) * 1_000,
            "lifetime is X-Amz-Date plus X-Amz-Expires"
        );
    }

    #[test]
    fn the_user_agent_is_appended_after_the_signature() {
        // The reference signer adds it to the already-signed URL, so it must not appear in the
        // canonical query string that precedes X-Amz-Signature.
        let signed = presign_msk_token(
            &credentials(),
            &MskIamSettings::new("us-east-2"),
            UNIX_EPOCH + Duration::from_secs(1_794_790_929),
        )
        .expect("signing succeeds");
        let url = decode(&signed.token);
        let signature_at = url.find("X-Amz-Signature=").expect("signature present");
        let user_agent_at = url.find("User-Agent=").expect("user agent present");
        assert!(
            user_agent_at > signature_at,
            "User-Agent must follow the signature, not be covered by it: {url}"
        );
    }

    #[test]
    fn the_canonical_query_string_is_sorted_by_encoded_key() {
        let canonical = canonical_query_string(&[
            ("X-Amz-Date".to_owned(), "d".to_owned()),
            ("Action".to_owned(), "a".to_owned()),
            ("X-Amz-Algorithm".to_owned(), "b".to_owned()),
        ]);
        assert_eq!(canonical, "Action=a&X-Amz-Algorithm=b&X-Amz-Date=d");
    }

    #[test]
    fn the_signature_covers_the_action_the_date_and_the_session_token() {
        // Changing any signed input must change the signature; changing only the user agent must
        // not.
        let settings = MskIamSettings::new("us-east-2");
        let at = UNIX_EPOCH + Duration::from_secs(1_794_790_929);
        let signature = |creds: &AwsCredentials, settings: &MskIamSettings, at: SystemTime| {
            let url = presign_msk_token(creds, settings, at)
                .expect("signing succeeds")
                .presigned_url;
            let start = url.find("X-Amz-Signature=").expect("present") + "X-Amz-Signature=".len();
            url[start..].split('&').next().expect("value").to_owned()
        };

        let base = signature(&credentials(), &settings, at);
        assert_eq!(base.len(), 64, "a hex SHA-256 HMAC");

        let later = signature(&credentials(), &settings, at + Duration::from_secs(1));
        assert_ne!(base, later, "the date is signed");

        let mut other_session = credentials();
        other_session.session_token = Some("different".to_owned());
        assert_ne!(
            base,
            signature(&other_session, &settings, at),
            "the session token is signed for a presigned URL"
        );

        let mut other_region = settings.clone();
        other_region.region = "us-west-2".to_owned();
        assert_ne!(
            base,
            signature(&credentials(), &other_region, at),
            "the region is in the credential scope"
        );

        let mut other_agent = settings.clone();
        other_agent.user_agent = "something-else/2.0".to_owned();
        assert_eq!(
            base,
            signature(&credentials(), &other_agent, at),
            "the user agent is deliberately not signed"
        );
    }

    #[test]
    fn a_credential_without_a_session_token_omits_the_parameter() {
        let mut creds = credentials();
        creds.session_token = None;
        let signed = presign_msk_token(
            &creds,
            &MskIamSettings::new("us-east-2"),
            UNIX_EPOCH + Duration::from_secs(1_794_790_929),
        )
        .expect("long-lived credentials are valid too");
        assert!(!decode(&signed.token).contains("X-Amz-Security-Token"));
    }

    #[test]
    fn empty_credentials_are_refused_before_signing() {
        let mut creds = credentials();
        creds.secret_access_key = String::new();
        assert!(
            presign_msk_token(&creds, &MskIamSettings::new("us-east-2"), SystemTime::now())
                .is_err(),
            "signing with no secret would produce a token that fails opaquely at the broker"
        );
    }

    #[test]
    fn settings_validation_rejects_an_unusable_region_or_expiry() {
        assert!(MskIamSettings::new("  ").validate().is_err());
        let mut settings = MskIamSettings::new("us-east-2");
        settings.expires_seconds = 0;
        assert!(settings.validate().is_err());
        settings.expires_seconds = 3_600;
        let error = settings.validate().expect_err("MSK rejects over 900 s");
        assert!(error.to_string().contains("900"));
        settings.expires_seconds = 900;
        settings
            .validate()
            .expect("the documented maximum is valid");
    }

    #[test]
    fn the_credential_resolver_answers_without_an_async_runtime() {
        // The callback must be safe to call from any thread, including a tokio worker, so the
        // resolver may never need a runtime of its own on the calling side.
        let resolver =
            CredentialResolver::start(Box::new(StaticCredentialsProvider(credentials())))
                .expect("the resolver thread starts");
        for _ in 0..4 {
            let answer = resolver.credentials().expect("resolution succeeds");
            assert_eq!(answer.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        }
        assert_eq!(resolver.resolutions(), 4);
    }

    #[test]
    fn a_failing_provider_surfaces_its_message_rather_than_panicking() {
        struct Failing;
        impl AwsCredentialsProvider for Failing {
            fn credentials(&self) -> Result<AwsCredentials, String> {
                Err("no instance profile is attached".to_owned())
            }
        }
        let resolver = CredentialResolver::start(Box::new(Failing)).expect("starts");
        let error = resolver.credentials().expect_err("the provider failed");
        assert_eq!(error, "no instance profile is attached");
        assert!(
            !error.contains('\0'),
            "librdkafka rejects a message with an embedded null"
        );
    }

    #[test]
    fn the_token_source_mints_a_token_librdkafka_can_use() {
        let source = MskIamTokenSource::start(
            MskIamSettings::new("us-east-2"),
            Box::new(StaticCredentialsProvider(credentials())),
        )
        .expect("valid settings");
        let token = source.token().expect("minting succeeds");
        assert!(
            token.principal_name.is_empty(),
            "MSK derives the principal from the SigV4 signature"
        );
        assert!(token.lifetime_ms > 0);
        assert!(!token.token.is_empty());
        assert!(
            !token.token.contains('\0'),
            "librdkafka takes the token as a C string"
        );
        assert_eq!(source.region(), "us-east-2");
    }

    #[test]
    fn an_unusable_credential_chain_refuses_startup_rather_than_failing_later() {
        // The preflight: without it, an MSK IAM misconfiguration is not discovered until
        // librdkafka first asks for a token, which is after the listener has opened.
        struct Failing;
        impl AwsCredentialsProvider for Failing {
            fn credentials(&self) -> Result<AwsCredentials, String> {
                Err("no instance profile is attached".to_owned())
            }
        }

        let error = MskIamTokenSource::start(MskIamSettings::new("us-east-2"), Box::new(Failing))
            .expect_err("an unusable chain must refuse startup");
        let text = error.to_string();
        assert!(text.contains("no instance profile is attached"), "{text}");
        assert!(
            text.contains("default-https-client"),
            "the refusal must name the feature its web-identity and IMDSv2 providers need: {text}"
        );
    }

    #[test]
    fn an_unsignable_credential_refuses_startup() {
        // A credential that resolves but cannot be signed with is caught at startup too.
        let error = MskIamTokenSource::start(
            MskIamSettings::new("us-east-2"),
            Box::new(StaticCredentialsProvider(AwsCredentials {
                access_key_id: String::new(),
                secret_access_key: String::new(),
                session_token: None,
            })),
        )
        .expect_err("empty credentials cannot sign");
        assert!(error.to_string().contains("non-empty access key id"));
    }

    #[test]
    fn the_default_chain_provider_refuses_to_run_inside_a_runtime() {
        // librdkafka calls the refresh callback from a thread that is already a tokio worker on
        // the consumer path. Blocking an async chain there panics, so the misuse returns an error
        // instead — and production routes through the resolver thread, where it is safe.
        let provider = AwsConfigCredentialsProvider::new().expect("the runtime builds");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a test runtime");
        let error = runtime.block_on(async {
            provider
                .credentials()
                .expect_err("resolution inside a runtime must not panic")
        });
        assert!(error.contains("inside a tokio runtime"), "{error}");
        assert!(error.contains("MskIamTokenSource"), "{error}");
    }

    #[test]
    fn credentials_never_print_their_secret() {
        let text = format!("{:?}", credentials());
        assert!(!text.contains("wJalrXUtnFEMI"), "{text}");
        assert!(!text.contains("session-token-value"), "{text}");
        assert!(text.contains("REDACTED"), "{text}");
    }
}
