# Capture Proxy

An HTTP/1.1 proxy that sits in front of a source cluster, forwards every byte unchanged, and records
the traffic to Kafka as `CaptureRecord` protobuf envelopes (schema:
[`captureProtobufs/src/main/proto/TrafficCaptureStream.proto`](../captureProtobufs/src/main/proto/TrafficCaptureStream.proto)).
The [Traffic Replayer](../trafficReplayer/README.md) reads that topic. By default a mutating request is
forwarded to the source only after Kafka has acknowledged its capture, so the topic never misses a
change the source applied.

This Rust workspace replaces the Java capture proxy (`trafficCaptureProxyServer` with
`nettyWireLogging`, `captureOffloader` and `captureKafkaOffloader`), which has been deleted. The image
keeps the name `capture_proxy`.

## Layout

| Crate | What it holds |
|---|---|
| `crates/capture-proxy` | The `capture-proxy` binary: start-up ordering, readiness, capture process state, shutdown |
| `crates/capture-config` | Configuration: flags, environment variables, inline JSON, endpoints, TLS and Kafka inputs |
| `crates/capture-transport` | Tokio TCP/TLS listener, source connections, byte-exact forwarding, backpressure |
| `crates/capture-http-scan` | Incremental HTTP/1 framing scanner, capture suppression, header rewriting |
| `crates/capture-wire` | Protobuf envelope encoding, observation sequencing, per-connection publication ordering |
| `crates/capture-kafka` | Kafka capability probe, group membership, routing generations, publisher, heartbeats |
| `crates/capture-observability` | Metric, span and operational-event contracts (`src/contract.rs`) |
| `crates/capture-test-support` | Shared fixtures, fakes, byte corpora and golden records |
| `third_party/rdkafka-sys` | The patched librdkafka build ([README](third_party/rdkafka-sys/README.md)) |
| `qualification/` | Qualification evidence and tooling for the rewrite: owner decisions, traceability, performance harness, reports |

## Building and testing

Prerequisites: [rustup](https://rustup.rs) (the toolchain, with `clippy` and `rustfmt`, is pinned by
`rust-toolchain.toml`), `cmake`, a C/C++ compiler, and the development headers for OpenSSL, Cyrus
SASL, libcurl, zlib and zstd. librdkafka is compiled from source and statically linked; `protoc` is
vendored.

```bash
cd TrafficCapture/captureProxy
third_party/rdkafka-sys/prepare.sh        # once, before any cargo command
cargo build --release --locked --bin capture-proxy
```

The checks CI runs (`.github/workflows/CI.yml`, job `rust-capture-proxy`):

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`cargo test` needs no broker or Docker. The tests in `crates/capture-kafka/tests/real_broker.rs` are
`#[ignore]`d and document the broker they need. The golden records in
`crates/capture-test-support/fixtures/goldens/` are rewritten only with `CAPTURE_GOLDENS_UPDATE=1`;
the Java replayer reads the same files (`RustGoldenRecordCompatibilityTest`), so a change to them is a
wire-format change.

## Image

```bash
# From the repository root. The context is TrafficCapture/ because the build compiles the shared
# .proto; captureProxy/Dockerfile.dockerignore keeps the rest of that tree out of the context.
docker build --file TrafficCapture/captureProxy/Dockerfile --tag <ref> TrafficCapture
```

`./gradlew buildKit_captureProxy_<arch>` (and `:buildImages:buildImagesToRegistry`) build the same
Dockerfile and push it to the configured registry.

The replayer's Kafka evidence tests (tagged `requiresProxyImage`) run this image in a container. Build
it first and pass its reference, for example
`./gradlew :TrafficCapture:trafficReplayer:isolatedTest -PcaptureProxyImage=<ref>`; see
`TrafficCapture/trafficReplayer/build.gradle`.

## Running

```bash
capture-proxy \
  --destination-uri https://source-cluster:9200 \
  --listen-port 9200 \
  --kafka-brokers broker-1:9092,broker-2:9092 \
  --kafka-topic logging-traffic-topic
```

`--no-capture` runs it as a plain forwarding proxy without Kafka. `capture-proxy --help` lists every
parameter. Each one also accepts its Java spelling as an alias (`--kafkaConnection`,
`--destinationUri`, `--listenPort`, `--sslCertChainFile`, ...) and can be set from a `CAPTURE_PROXY_*`
environment variable (for example `CAPTURE_PROXY_KAFKA_BROKERS`). Kafka client properties beyond the
flags come from `--kafka-property-file`; the settings that capture ordering depends on (idempotence,
`acks=all`, five in-flight requests) cannot be overridden.

Records awaiting Kafka acknowledgement are bounded by a publication budget (64 MiB and 4,096 records
by default; `--kafka-publication-budget-bytes`, `--kafka-publication-budget-records`). When it is full
the proxy stops reading from clients until acknowledgements free space, instead of buffering more.
`--capture-failure-policy` chooses what happens when capture fails: `fail-closed` (the default) exits
without forwarding the waiting request, `fail-open` abandons capture for the rest of the process and
keeps forwarding.

### Inline JSON

The Kubernetes/Argo deployment passes the whole configuration as one JSON document instead of flags:

```bash
capture-proxy ---INLINE-JSON '{"destinationUri": "https://source-cluster:9200", "listenPort": 9200,
                               "kafkaConnection": "broker-1:9092", "kafkaTopic": "logging-traffic-topic"}'
capture-proxy ---INLINE-JSON "$(printf '%s' "$JSON" | base64)"   # base64 is accepted too
capture-proxy ---JSON-FILE /config/capture-proxy.json
```

Keys are parameter names, in either spelling. The document must be the only argument; unknown keys
are rejected rather than ignored. A key in the document wins over the matching environment variable,
which wins over the default.

### Exit status

| Status | Meaning |
|---|---|
| 0 | Clean shutdown, or `--help` / `--version` |
| 2 | Configuration error |
| 3 | The destination URI could not be parsed |
| 70 | Start-up failed for another reason: TLS material, telemetry, or bind |
| 78 | Capture was compromised, or capture start-up was refused |

## Java provenance in comments

Many comments and test names cite Java sources by file and line (`CaptureProxy.java:288`,
`CaptureKafkaPublisherTest...`) to record where a behaviour or an expected value came from. Those Java
modules were deleted after commit `5e6819a46`; read a cited file with
`git show 5e6819a46:TrafficCapture/<module>/src/...`. Expected values taken from Java tests
are committed in the Rust tests and fixtures; see the ground-truth list in
[`qualification/reports/phase5-plan.md`](qualification/reports/phase5-plan.md).
