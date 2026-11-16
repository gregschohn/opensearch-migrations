//! The capture-proxy configuration contract.
//!
//! This crate is the shared, authoritative source of the proxy's externally meaningful
//! configuration. Every other crate depends on it, so its types are the stable boundary between
//! subsystems.
//!
//! # What is and is not frozen
//!
//! The rewrite must preserve the **meaning**, defaults, and validation of the migration
//! specification. Exact CLI flag spelling, aliases, Log4j2 property compatibility, logger names,
//! and rendered log lines are explicitly **not** release requirements: the orchestration layer
//! translates the stable migration specification into this model.
//!
//! Note that the Kubernetes/Argo path emits **zero** `--flag` arguments — it passes
//! `["---INLINE-JSON", "<base64 JSON>"]` and relies on the image entrypoint. The literal flag
//! spellings only matter to the older CDK/ECS path and to tests.
//!
//! # Modules
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`pattern`] | HTTP-04, HTTP-08 | capture-suppression regexes with Java whole-string match semantics |
//! | [`limits`] | HTTP-05 | incomplete-request and connection limits |
//! | [`timing`] | — | the `F`, `H`, `E`, `S` protocol intervals and their validation |
//! | [`policy`] | HTTP-01 | capture failure policy and HTTP parsing strictness |
//!
//! # The trap worth repeating
//!
//! Suppression patterns **must** go through [`pattern::SuppressionPattern`]. Java matches them with
//! `Pattern.matches()` (whole string); `regex::Regex::is_match` is a search. Orchestration defaults
//! these patterns to the empty string, so a search-based port would match every request and
//! silently disable all capture.

pub mod limits;
pub mod pattern;
pub mod policy;
pub mod timing;

pub use limits::{LimitsError, RequestLimits};
pub use pattern::{CaptureDirective, PatternError, SuppressionPattern};
pub use policy::{CaptureFailurePolicy, HttpStrictness, PolicyParseError, StrictnessParseError};
pub use timing::{CaptureTiming, TimingError};
