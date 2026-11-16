# Phase 1 owner review — Rust capture proxy

Date: 2026-11-16
Status: **DRAFT — performance gate section pending final paired results**

## 1. Revisions, images, environment

| Item | Value |
|---|---|
| Baseline | `opensearch-project/opensearch-migrations` `main` @ `059d7b6483f29759b1ee108b87b87c61b0ce3432` (matches the plan's analyzed baseline) |
| Branch | `rust-capture-proxy` |
| Phase 1 head | `6a2edd703f4f5241606a5cd6ef6fb30e98206ca9` (23 commits) |
| Measurement image | `localhost:5001/migrations/capture_proxy:rust-2ff6080df`, ID `sha256:c1aeeae2a150f1a2f1744a80273d73aafa7c0696ea04f01fb0fdd79bfdde4e1d`, linux/arm64, AL2023, 288 MB, user 10001 |
| Java comparison image | `localhost:5001/migrations/capture_proxy:latest_arm64`, `sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357`, Jib `:TrafficCapture:trafficCaptureProxyServer:jib_arm64` at the baseline |
| Host | Apple M5 Pro, 18 cores, 64 GiB, macOS (Darwin 25.6.0), Docker Desktop 29.7.2 Linux/arm64 VM |
| Toolchains | Rust 1.97.1, Corretto 21.0.12, kafka-clients 4.2.0 (Java), librdkafka 2.12.1 via rdkafka 0.39 |
| Kafka | Apache Kafka 4.0.0 KRaft single broker per test, `message.timestamp.type=LogAppendTime`; plus a live AWS MSK Serverless cluster for IAM |

The commits after `2ff6080df` (`6d9dac9ed`, `6a2edd703`) change Kafka TLS property translation and
the capture-compromise paths only; neither is exercised by the steady-state performance profiles.

Size: 48.2K lines of Rust under `crates/*/src`, 7.3K under `crates/*/tests`. Workspace: **895+ tests
passing, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo fmt --check` clean**
(`unsafe_code` forbidden workspace-wide).

Host honesty: an arm64 macOS Docker VM is not the production Linux/EKS topology. It cannot
reproduce the 128-pod, 152.5K req/s fleet, a 48-node source, a replicated multi-broker Kafka, or
production cAdvisor. It **does** reproduce production's per-pod behaviour closely (§5.1), and every
ratio below is a paired Java/Rust comparison on identical topology.

## 2. Summary

| Gate | Requirement | Result |
|---|---|---|
| 1 | Zero exercised WIRE or PROTOCOL divergence | **PASS** (§3) |
| 2 | Plaintext scan-and-forward throughput/core ≥ Java × 1.20 | PENDING final — interim **2.93×** (9 pairs, CI95 2.80–3.05×) |
| 3 | Double TLS + churn: throughput/core and proxy-added p99 within 5% of Java | PENDING |
| 4 | Thin capture slice ≥ Java × 1.15 requests/core and ≤ 0.75 × steady RSS | PENDING |
| 5 | Kafka records/core and MiB/core ≥ 0.90 × Java; ack p99 ≤ 1.10 × Java | PENDING |
| 6 | Delayed Kafka: bounded memory, lossless backpressure, recovery | PENDING |

## 3. WIRE and PROTOCOL evidence

### 3.1 Java replayer differential (Gate 1)

Method (`/tmp/rust-proxy-qual/specs/wire-proof.md`): a fixed corpus (seed `20261115`, digest
`5d9597e4…4cf5`; keep-alive GETs, POST/PUT/DELETE/PATCH critical mutations, chunked + trailers,
`Expect: 100-continue`, a policy-suppressed request between two captured ones, rich headers, seeded
1–6-segment TCP fragmentation, driven over raw sockets) is sent through each proxy into its own
topic, and each topic is read back by the **unchanged Java replayer** in `dump-both` mode. Transcripts
and an independent decoder's canonical observation stream are compared per connection.

Calibration: six Java passes (including a second, independent Kafka cluster) are mutually identical
after normalisation. A third and fourth Java pass were needed to find a TCP-read-count
nondeterminism that two passes did not reveal. **Negative control:** a single added corpus byte
leaves all per-run assertions passing but is caught by the differential comparison in three
independent views — so the differential, not the per-run assertions, decides Gate 1.

| Comparison | Image | Total differences |
|---|---|---:|
| Java j1 vs Rust r1 | `rust-ba76d762e` | **0** |
| Java j1 vs Rust r2 | `rust-748379d05` (F flush live) | **0** |
| Rust r1 vs Rust r2 | — | **0** |
| Java j1 vs Rust r3 | `rust-2ff6080df` (measurement image) | **0** |

Per-run assertions, all 12 PASS on every Rust pass: every value decodes as `CaptureRecord` with
exactly one payload; **zero Kafka headers**; key = raw `connectionId`; every timestamp **type** is
`LogAppendTime`; one partition per connection; index order agrees with offset order;
`connectionObservationSequence` present and exactly +1; one `CloseObservation` per connection, last in
stream and in the final record; request (11 / 4,549 B), response (4,279 B) and informational (25 B)
bytes **byte-exact**; one `requestForwarded` per end-of-message in order; the suppressed request yields
a `requestDropped` after its captured prefix and no `requestForwarded`.

Replayer reconstruction, Java and Rust identical: **11 REQ, 11 RSP, 1 informational, 5 CLOSED**, zero
replayer exceptions or protocol violations. Record packing happened to be identical too
(c0=1, c1=5, c2=2, c3=2, c4=1, probe=1), although the J-1 envelope deviation permits it to differ.

Segmentation through the Java replayer: a 3 MiB response (forcing size-triggered detaches and
segmented write observations) captured by Rust reconstructs as `RSP[3145863]` (3,145,728 body + 135
header bytes) plus its `REQ` and `CLOSED`.

### 3.2 Protocol behaviour against real Kafka

| Obligation | Evidence | Result |
|---|---|---|
| Java Murmur2 routing over sorted assignment | 68 ids × 8 partition sets generated by kafka-clients 4.2.0 `Utils`: 68 hashes, **544 routing decisions, 0 mismatches** | PASS |
| Broker-assigned time | Probe accepts only `Timestamp::LogAppendTime(t>0)`; a CreateTime topic returned a **positive** `CreateTime(1794802821323)` on Kafka 4.0.0 — the documented "positive timestamp" check would have passed it (OD-4) | PASS |
| CreateTime topic refuses startup (OD-5) | Real broker; listener never binds | PASS |
| Listener opens only after readiness | Dead broker: port polled throughout startup never binds; exit 78 | PASS |
| Capture-before-forward | Mutation test: removing the publication wait fails exactly the test asserting "the gate forwarded a mutation that Kafka had not acknowledged" | PASS |
| Multi-replica rebalance (§3.4/§3.6) | Two proxies, 8 partitions: A gets all 8 (A:1); B joins → A:2 = [0–3], B:1 = [4–7]; A's old connections keep A:1 and their original partitions (incl. 4 and 6, now B's) — verified in logs **and** decoded Kafka records; A:1 heartbeats every 2 s while draining and each partition retires within 12 ms of its last connection's close, never before; both exit 0 `completed`; **Java replayer: 23 REQ / 23 RSP / 10 CLOSED**, matching 23 requests on 10 connections | PASS |
| fail-closed under broker failure (§7.2) | Broker paused, H=2 E=6: exit **78** at 4.89 s; the waiting PUT **never reached the origin** (origin logs every request before responding) | PASS after fix (§6) |
| fail-open under broker failure (§7.3) | Process stays up; waiting PUT, new-connection PUT/GET during the outage and after recovery all forwarded; one-way transition logged once; SIGTERM 0.17 s, `outcome=incomplete`, retirement not claimed | PASS after fix (§6) |
| F periodic flush (§4.2) | Idle partial request appended **2003 ms** after send with F = 2000 ms, connection still open | PASS after fix (§6) |
| Terminal record + deregistration (§5.1) | 3 MiB response then source close: 3/3 `outcome=completed`, writer retired (was 3/3 exit 70) | PASS after fix (§6) |
| Effective producer config | Read back from librdkafka: `enable.idempotence=true`, `acks=-1`, `max.in.flight=5` | PASS |
| No admin operations (OD-7) | Layout from producer metadata only; zero admin calls on a real broker | PASS |

### 3.3 Authentication

| Mode | Evidence | Result |
|---|---|---|
| none | All real-broker tests above | PASS |
| TLS (`ssl`) | Real SSL listener, orchestration's exact `client.properties` shape: capture + PUT/GET 200, exit 0; missing CA → refuses, exit 78 | PASS after fix (§6) |
| SCRAM-SHA-512 | Real SASL_SSL listener: capture + PUT/GET 200, exit 0; wrong password → refuses, exit 78 | PASS after fix (§6) |
| MSK IAM | **Proxy release code, run on an in-VPC EC2 client against the live MSK Serverless cluster**, credentials from the instance profile via IMDSv2 through `aws-config`'s default chain (no static keys in the environment): capture-ready in 4.4 s, assignment `[0, 1, 2]`, PUT and GET 200 through the capture gate, SIGTERM exit 0 `outcome=completed`, 3 writer lanes retired. The CreateTime topic on the same cluster refused startup in 1.6 s with exit 78 and never bound: *"the capability probe for partition 2 was acknowledged with CreateTime(1794840361641), so broker time cannot be proven and capture would be unsound"* (OD-5). Built from commit `6d9dac9ed` shipped as a git bundle over SSM; no AWS resources created for this step | PASS |
| MSK IAM from EKS IRSA | Same code path as IMDS (`aws-config` default chain), but the web-identity provider, SA→role trust, pod reachability and token-rotation soak are unproved; EKS changes were not authorised | NOT RUN |

## 4. Defects found and fixed during Phase 1

Each was found by a real-component test, root-caused in source, fixed, and re-proved by the same
test. None required a WIRE or PROTOCOL deviation.

| # | Defect | Impact | Commit |
|---|---|---|---|
| 1 | F flush deadline had no driver (prior attempt's blocker #3, recurring) | idle records waited for the next observation | `3bcf18e81`, `748379d05` |
| 2 | Terminal record stranded behind an in-flight record when the connection task exited | lost `CloseObservation`, connection never deregistered, shutdown exit 70 after 72 s | `9568cb3b0` |
| 3 | Capture gate polled with a "200 µs" sleep that tokio's ~1 ms timer resolution turned into ~1 ms | critical-mutation latency **1.47 → 0.73 ms** median after fix; would have roughly halved capture throughput | `2ff6080df` |
| 4 | Java `ssl.truststore.*` keys passed to librdkafka | **every SCRAM deployment refused to start** (`No such configuration property: ssl.truststore.type`) | `6d9dac9ed` |
| 5 | fail-closed never terminated | process alive, listener open, refusing traffic | `6a2edd703` |
| 6 | fail-open refused new connections | violated unmanaged pass-through | `6a2edd703` |
| 7 | No bridge from Kafka terminal failure to the capture gate | an idle proxy's heartbeat expiry left it silently refusing traffic | `6a2edd703` |
| 8 | librdkafka `linger.ms` default 5 ms vs Java 0 | 4–9× throughput regression on the measured ack-latency curve | `b00c8670d` |
| 9 | librdkafka `message.max.bytes` default 1,000,000 < 1,048,076 default payload limit | maximum-size record = terminal capture failure out of the box | `b00c8670d` |

Java defects found and deliberately not reproduced (owner rulings OD-1…OD-10): malformed framing
tunnelled uncaptured past the capture gate (incl. any >4096-byte request line); header removal leaked
the removed header when a 100-continue arrived mid-head; the `CaptureRecord` envelope was never
budgeted; backward broker time regressed the heartbeat baseline; whitespace before a header colon
was folded into the header name (desync shape).

## 5. Performance

### 5.1 Java baseline and its fidelity to production

PENDING final table. Fidelity, from the Phase 0 Java scorecard: at 23.01% new-connection churn the
Java proxy used **3.512 cores** (production: 3.54/pod, 1%), Kafka byte amplification **1.045×**
(production: ~1.05×).

### 5.2 Method

Interleaved paired A/B (`/tmp/rust-proxy-qual/harness/run-paired.sh`, `paired_ratio.py`), order
alternated within pairs, every run through `validate_run.py`, ratio computed within each pair. A gate
passes only when the paired median clears it, ≥80% of pairs clear it, and the CI95 lies entirely on
the passing side; otherwise INCONCLUSIVE. Pairing did **not** collapse variance as first assumed —
within-pair noise is independent — so resolution is bought with pair count, priced per gate.

### 5.3–5.8 Gate results

PENDING.

## 6. Open items and risks

PENDING.

## 7. Recommendation

PENDING.
