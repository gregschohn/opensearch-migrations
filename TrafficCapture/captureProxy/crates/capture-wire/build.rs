//! Generates Rust types from the *unchanged* canonical capture schema.
//!
//! The schema at `TrafficCapture/captureProtobufs/src/main/proto/TrafficCaptureStream.proto` is a
//! shared external contract: the Java replayer consumes the same file. This build script must never
//! copy, fork, or patch it. If the path below stops resolving, fix the path — do not vendor a copy.

use std::path::{Path, PathBuf};

/// Path from this crate's directory to the canonical schema directory.
const PROTO_DIR_FROM_CRATE: &str = "../../../captureProtobufs/src/main/proto";
const PROTO_FILE: &str = "TrafficCaptureStream.proto";

fn main() {
    let crate_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );
    let proto_dir = crate_dir.join(PROTO_DIR_FROM_CRATE);
    let proto_file = proto_dir.join(PROTO_FILE);

    assert!(
        proto_file.is_file(),
        "canonical capture schema not found at {}. The Rust proxy must generate from the shared \
         .proto consumed by the Java replayer; it must not vendor its own copy.",
        proto_file.display()
    );

    // Rebuild whenever the shared contract changes.
    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed=build.rs");

    // Use a vendored protoc so the build does not depend on a host protobuf installation.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc is available");

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    // Map `bytes` fields to `bytes::Bytes` so captured payloads can be sliced from the connection's
    // read buffer without copying. Byte-exactness of captured data is a frozen contract.
    config.bytes([".".to_string()]);
    config
        .compile_protos(&[canonicalize(&proto_file)], &[canonicalize(&proto_dir)])
        .expect("capture schema compiles");
}

fn canonicalize(p: &Path) -> PathBuf {
    p.canonicalize()
        .unwrap_or_else(|e| panic!("cannot canonicalize {}: {e}", p.display()))
}
