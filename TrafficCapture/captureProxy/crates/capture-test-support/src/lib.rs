//! Shared fixtures, fakes, byte corpora, and golden records for capture-proxy tests.
//!
//! Fixtures here are *cross-language evidence*: several were produced by running the real Java
//! implementation at the baseline commit, so a Rust test that consumes them is comparing against
//! Java rather than against another Rust implementation of the same guess.

pub mod routing_vectors;
