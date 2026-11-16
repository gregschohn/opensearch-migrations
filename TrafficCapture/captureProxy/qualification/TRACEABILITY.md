# Rust capture-proxy traceability and phase-gate checklist

Live checklist for the in-tree replacement of the Java capture proxy. IDs match
`rust-capture-proxy-rewrite-plan.md`. This file is updated as work lands; it is the single
source of truth for what is done, what is proved, and what is outstanding.

Baseline: `opensearch-project/opensearch-migrations` `main` @
`059d7b6483f29759b1ee108b87b87c61b0ce3432` (matches the plan's analyzed baseline exactly — no
re-audit required).
Implementation branch: `rust-capture-proxy`.

## Status legend

| Mark | Meaning |
|---|---|
| `TODO` | Not started. |
| `WIP` | Implementation in progress. |
| `IMPL` | Implemented, not yet independently proved. |
| `PROVED` | Implemented **and** the named evidence suite passes. Evidence recorded in `reports/`. |
| `BLOCKED` | Cannot proceed; reason recorded. |
| `N/A` | Retired with its remaining obligation discharged elsewhere. |

A row is only `PROVED` when a named command and its output exist in a report under
`qualification/reports/`. "It compiles" and "unit tests pass" are not proof of a protocol row.

## Phase gates

| Phase | Exit gate | Status |
|---|---|---|
| 0 | Agreed correctness and performance scorecard | **MET** (Java scorecard measured; see `reports/`) |
| 1 | Zero exercised WIRE/PROTOCOL divergence + reproducible Java/Rust perf report, owner-reviewed | TODO |
| 2 | Java replayer utilities decode Rust golden records with equivalent fields and exact bytes | TODO |
| 3 | Kafka/rebalance/failure scenarios pass against a real broker with Rust as writer | TODO |
| 4 | Black-box compatibility against recorded Java transcripts/goldens | TODO |
| 5 | No proxy Java implementation/tests remain; surviving modules build; migration spec yields a correct Rust-only deployment | TODO |
| 6 | Every traceability row closed; all correctness gates pass; performance target met without moving the bottleneck to Kafka or the source | TODO |

### Phase 0 detail

| Item | Status | Evidence |
|---|---|---|
| Exact starting commit recorded | PROVED | `059d7b6483f29759b1ee108b87b87c61b0ce3432`; fresh clone, clean tree |
| Java baseline test run | PROVED | 294 standard + 294 slow + 35 isolated tests, 0 failures/errors/skips. `reports/phase0-java-baseline.md` |
| Cargo workspace + prost generation from unchanged `.proto` | PROVED | All field numbers/oneof tags preserved; see commit `a9188bce3` |
| librdkafka (ssl/sasl/zstd) links on target toolchain | PROVED | WITH_SSL/SASL/SASL_OAUTHBEARER/SASL_SCRAM/ZLIB/ZSTD all =1. AL2023 image build needs `libcurl-devel`. |
| Normative contract read and frozen | PROVED | `proxyCaptureProtocol.md` §§1-10 reviewed by integrator; 4 subsystem specs produced with file:line citations |
| Contract-vs-code discrepancies resolved | PROVED | 5 verified Java defects + 5 doc/code disagreements; rulings in `OWNER-DECISIONS.md` (OD-1..OD-7) |
| Java CPU/allocation/memory profile matrix | PROVED | JFR attribution: 49.9% of Java-thread CPU is telemetry, 23.1% in ActiveContextTracker sorted-map work; 45% of allocation is protobuf byte[]. async-profiler NOT available for linux/arm64. |
| Java baseline performance scorecard | PROVED | 38 valid runs across 8 profiles, >=5 reps each. Cross-validates production within ~1%: 3.512 cores at 23.01% churn vs 3.54/pod, 1.045x Kafka amplification vs ~1.05x. |

## Phase 1 continuation gates

| # | Gate | Status | Measured |
|---|---|---|---|
| 1 | Zero exercised WIRE or PROTOCOL divergence | **PASS** | Rust r1/r2/r3 vs Java j1: 0 differences; negative control validated; rebalance + failure-policy + auth proved on real brokers |
| 2 | Plaintext scan-and-forward throughput/used core >= Java x 1.20 | TODO | |
| 3 | Double TLS + churn: throughput/core and proxy-added p99 within 5% of Java | TODO | |
| 4 | Thin capture slice: requests/core >= Java x 1.15 and steady RSS <= Java x 0.75 | TODO | |
| 5 | Kafka records/core and MiB/core >= Java x 0.90; ack p99 <= Java x 1.10 | READY | Java ack p50/p99 = 0.808/4.876 ms with gate buckets; ~2 pairs for records/core, ~20 for ack p99 |
| 6 | Delayed Kafka: bounded memory, lossless read backpressure, recovery to near baseline | READY | byte+record budgets implemented; Java baseline measured (319 MiB under 750ms delay, zero errors) |

## Final release gates

| # | Gate | Status | Measured |
|---|---|---|---|
| F1 | Sustain ~152.5K requests/s with no additional pods | TODO | |
| F2 | >= 30% better throughput/used core (>= 435 req/s/core vs 335 baseline) | TODO | |
| F3 | >= 40% lower mean per-pod working set (<= 0.39 GiB vs 0.65 GiB) | TODO | |
| F4 | <= 5% regression in proxy-added p99 latency | TODO | |
| F5 | Zero lost required captures | TODO | |
| F6 | Zero invalid source forwards | TODO | |
| F7 | Zero unexpected restarts | TODO | |
| F8 | Bounded memory or lossless backpressure during delayed Kafka | TODO | |
| F9 | Existing Java replayer consumes Rust traffic | TODO | |
| F10 | Real-Kafka multi-replica rebalance, drain, failure, auth scenarios pass | TODO | |
| F11 | Clean repository build, no live references to deleted Java proxy modules | TODO | |
| F12 | Correct, diagnosable Rust-only deployment via the existing migration specification | TODO | |

## Source-to-target rows

Evidence suites: PROTO, WIRE, HTTP, KMODEL, KREAL, TRANSPORT, TLS, OPS, DEPLOY, PERF.

### Protobuf and connection recording

| ID | Treatment | Rust owner | Status | Evidence |
|---|---|---|---|---|
| PROTO-01 | Exact | `capture-wire` generated types | PROVED | Java replayer decodes Rust records: 3 Rust passes, 0 differences vs calibrated Java (wire-proof) |
| WIRE-01 | Retired | `capture-wire::record_buffer` | IMPL | RAII buffers; exactly-once release proved on 8 paths |
| WIRE-02 | Exact | `capture-wire::protobuf_size` | IMPL | All 11 Java size vectors + full 38-row CsvSource + proptests vs prost |
| WIRE-03 | Semantic | `capture-wire::connection_recorder` | IMPL | Observation catalogue, contiguous +1 sequence, counters, F deadline |
| WIRE-04 | Semantic | `capture-kafka::connection_factory` | PROVED | route immutability across a replacement generation, real broker |
| WIRE-05 | Semantic | `capture-kafka::readiness` | PROVED | listener admission blocked until capability + first generation usable; proved by construction in the binary (port never binds against a dead broker, exit 78) |
| WIRE-06 | Semantic | `capture-kafka::shutdown` | PROVED | retirement e2e: normal, force-close, large-response-then-close all outcome=completed; compromised path skips retirement per 7.3 |
| WIRE-07 | Exact | `capture-wire::publish_chain` | PROVED | per-connection ack chain; terminal-record drain fixed (gap A) and re-proved 3/3 e2e |
| WIRE-08 | Exact | `capture-wire` recorder/encoder/segmentation | PROVED | 0-difference replayer differential; 3 MiB segmented response reconstructs RSP[3145863]; F flush e2e 2003 ms at F=2000 ms |
| WIRE-09 | Semantic | `capture-wire::record_sink` | IMPL | Kafka-free RecordSink trait |
| WIRE-10 | Operational | `capture-observability::connection_span` | IMPL | connection span at debug; metrics unconditional |
| WIRE-11 | Retired | `capture-observability::RootTelemetry` | N/A | retired: RootTelemetry explicit registry |

### Kafka publication and distributed routing

| ID | Treatment | Rust owner | Status | Evidence |
|---|---|---|---|---|
| KAFKA-01 | Exact | `capture-kafka::capability_probe` | PROVED | OD-4/OD-5: LogAppendTime type accepted, CreateTime refused on a real broker (positive CreateTime value proves the zero-trick fails open) |
| KAFKA-02 | Exact | `capture-kafka::membership` | PROVED | real 2-replica rebalance: disjoint reassignment, old routes immutable, draining heartbeats, retire within 12 ms of last close; replayer 23/23/10 |
| KAFKA-03 | Exact | `capture-kafka` publisher/lane/heartbeat/ack_deadline/retirement | PROVED | 29 deterministic model tests + real-broker generation activation with real heartbeats |
| KAFKA-04 | Exact | `capture-kafka::write_gate` | PROVED | model tests + write-gate trip now bridged to the capture gate; proved by real broker-failure scenario |
| KAFKA-05 | Semantic | `capture-kafka::RoutingGenerationController` | PROVED | initial-heartbeat activation gate on every assigned partition, real broker |
| KAFKA-06 | Exact | `capture-kafka::routing_state` | PROVED | 544 Java routing decisions 0 mismatches; real rebalance route immutability |
| KAFKA-07 | Semantic | `capture-kafka::error::CorruptedCaptureState` | PROVED | closed reason set asserted exhaustively |
| KAFKA-08 | Semantic | `capture-kafka::bootstrap` | PROVED | start_capture end-to-end on a real broker, ready in 135ms; binary proves listener binds only after readiness |
| KAFKA-09 | Semantic | `capture-config::kafka` + `capture-kafka::client_config` | PROVED | effective config readback; TLS and SCRAM-SHA-512 against real SASL_SSL broker with the orchestration property file; MSK IAM against live MSK Serverless via IMDS |
| KAFKA-10 | Exact | `capture-kafka::topic_metadata` | PROVED | OD-7: layout validated with ZERO admin calls, real broker |
| KAFKA-11 | Operational | `capture-observability::kafka_metrics` | IMPL | 16 instruments bridged; 4 queue-depth/latency instruments pending observability counterpart |
| KAFKA-12 | Retired | `capture-observability::RootTelemetry` | N/A | retired: same |
| KAFKA-13 | Operational | `capture-observability::kafka_record_span` | IMPL | span renamed kafkaCommit (no consumer of stream_flush_called); metric names verbatim |

### HTTP scanning, capture policy, wire observation

| ID | Treatment | Rust owner | Status | Evidence |
|---|---|---|---|---|
| HTTP-01 | Exact | `capture-config::CaptureFailurePolicy` | IMPL | CaptureFailurePolicy, FailClosed default as Java |
| HTTP-02 | Exact | `capture-proxy::capture_process_state` | PROVED | fail-closed exit 78 at 4.89 s with waiting PUT never forwarded; fail-open pass-through for existing and new connections |
| HTTP-03 | Exact | `capture-proxy::connection_capture` + `capture-transport::request_pump` | PROVED | gate mutation-tested; event-driven wake (1.47 -> 0.73 ms median); fail-closed refuses the waiting mutation on a real broker failure |
| HTTP-04 | Exact | `capture-http-scan::suppression` | IMPL | three-way capture/ignore/drop; whole-string matching via SuppressionPattern everywhere |
| HTTP-05 | Exact | `capture-config::RequestLimits` | IMPL | Java defaults + validation, plus OD-1 request-line budget |
| HTTP-06 | Exact | `capture-http-scan` scanner/classifier + `capture-proxy` capture/lifecycle | IMPL | scanner + classifier; Java vectors ported (14/711 EOM lengths, interim responses); fragmentation proptests 400 cases each |
| HTTP-07 | Semantic | `capture-http-scan::selected_headers` | IMPL | borrowed selected headers, no general header map |
| HTTP-08 | Semantic | `capture-http-scan::CapturePredicate` | IMPL | CapturePredicate trait + directive enum |
| HTTP-09 | Exact | `capture-transport::write_completion` | IMPL | one requestForwarded per captured EOM after write success |
| HTTP-10 | Operational | `capture-observability::connection_metrics` | IMPL | activeConnection kept (externally consumed, spec was wrong) |
| HTTP-11 | Retired | `capture-observability::RootTelemetry` | N/A | retired: explicit handles replace context inheritance |
| HTTP-12 | Semantic | `capture-observability::wire_activity` | IMPL | borrowed phase handles, zero per-request Arc clones |
| HTTP-13 | Operational | `capture-observability::root` | IMPL | single telemetry registry; 47 instruments / 75 series snapshotted |
| HTTP-14 | Operational | `capture-observability::wire_activity` | IMPL | 18 generated instruments dimensionless on the SDK fast path |

### Listener, TLS, forwarding, rewrite, process

| ID | Treatment | Rust owner | Status | Evidence |
|---|---|---|---|---|
| PROXY-01 | Exact | `capture-proxy::failure_terminator` | PROVED | terminator wired to the capture gate; real broker failure -> exit 78 within E |
| PROXY-02 | Operational | `capture-observability::process_metrics` | IMPL | ported failOpen gap-remains-active test |
| PROXY-03 | Semantic | `capture-proxy` main/app/shutdown | IMPL | 60/240/270/300 absolute deadlines under tokio virtual time |
| PROXY-04 | Retired | `capture-observability::root` | N/A | retired: one explicit registry |
| PROXY-05 | Semantic | `capture-transport::preconnect_pool` | IMPL | shared pool, size IS the warm count; backoff added (ESIP:202 TODO closed) |
| PROXY-06 | Exact | `capture-transport::response_pump` | IMPL | response pump with bidirectional backpressure (deviation, 1MiB hold test) |
| PROXY-07 | Semantic | `capture-transport::preconnect_pool::ExpiringSlotPool` | IMPL | ExpiringSlotPool with Drop shutdown |
| PROXY-08 | Semantic | `capture-transport::request_pump` + `connection` | IMPL | request pump, credit-of-one preserved |
| PROXY-09 | Exact | `capture-http-scan::rewrite::add` | IMPL | verified against origin-recorded bytes in the live binary |
| PROXY-10 | Exact | `capture-http-scan::rewrite::remove` | IMPL | obs-fold continuation removed with its header (Java leaks it) |
| PROXY-11 | Semantic | `capture-transport::listener` | IMPL | bind/accept/registry/stop-accepting/disconnect-all wired in the binary |
| PROXY-12 | Semantic | `capture-proxy::connection_factory` + `capture-transport::tls` | IMPL | front TLS/mTLS + destination TLS; ClientTrust from TrustMaterial; handshake-failure dedupe; lifetime origin at accept |
| PROXY-13 | Operational | `capture-observability::log_deduper` | IMPL | deduper with parameterised window; structured event replaces free text |
| PROXY-14 | Operational | `capture-observability::logging` | IMPL | tracing-subscriber defaults, MIGRATIONS_ROOT_LOG_LEVEL honoured with strict parsing |
| PROXY-15 | Retired | Rust logging component filters | IMPL | component-level filters |
| PROXY-16 | Retired | Native-dependency diagnostics routed through the Rust facade | IMPL | native diagnostics routed through the facade |

### Surviving Java dependencies and consumers

| ID | Status | Evidence |
|---|---|---|
| SHARED-01 | IMPL | inline-JSON loader preserves migration-spec meaning; unknown keys rejected |
| SHARED-02 | IMPL | node_instance_name + terminator + panic hook |
| SHARED-03 | IMPL | DestinationEndpoint with userinfo redaction; exit 3 on parse failure |
| SHARED-04 | IMPL | explicit tracing/OTel instruments; no context hierarchy |
| SHARED-05 | TODO | |
| CONSUMER-01 | TODO | |
| CONSUMER-02 | TODO | |

### Normative documents

| ID | Status | Evidence |
|---|---|---|
| CONTRACT-01 | WIP | `proxyCaptureProtocol.md` §§1–10 reviewed; discrepancy list pending |
| CONTRACT-02 | TODO | |
| CONTRACT-03 | TODO | |

### Deployment, build, qualification

| ID | Status | Evidence |
|---|---|---|
| DEPLOY-01 | TODO | |
| DEPLOY-02 | TODO | |
| DEPLOY-03 | TODO | |
| DEPLOY-04 | TODO | |
| DEPLOY-05 | TODO | |
| DEPLOY-06 | TODO | |
| DEPLOY-07 | TODO | |
| DEPLOY-08 | TODO | Scope decision required: Kubernetes only, or Kubernetes + CDK/ECS |
| DEPLOY-09 | TODO | |
| PERF-00 | TODO | |
| PERF-01 | TODO | |

## Open owner decisions

Tracked here so they are not silently resolved by implementation choice.

| # | Decision | Raised by | Status |
|---|---|---|---|
| D1 | Production HTTP strictness | Plan "Design improvements" §4 | **RESOLVED** OD-2: strict default + explicit opt-in lenient mode |
| D2 | Is bounded lossless Kafka backpressure mandatory in the parity release | Plan §2 | OPEN |
| D3 | Qualified deployment targets: Kubernetes only, or + CDK/ECS | DEPLOY-08 | OPEN |
| D4 | Publisher sharding deferral confirmed until single-owner is measured | Plan §3 | OPEN |
| D5 | MSK IAM proof infrastructure | Phase 1 risk | **RESOLVED**: proved against a real MSK Serverless cluster. SigV4 `kafka-cluster` token through librdkafka OAUTHBEARER. |
