# Commit map: pre-sign-off SHAs → published SHAs

Before the branch was first pushed, its unpushed history was rewritten (`git filter-branch`) to add
the author's DCO `Signed-off-by` to every commit and to replace one environment-specific hostname in
`qualification/e2e/msk_iam_in_vpc.sh` with a placeholder. File contents are otherwise identical: the
final tree of the rewritten branch is byte-identical to the original.

Every SHA changed. The documents under `qualification/` have been updated to the new SHAs, with
two exceptions that keep the **old** SHAs on purpose, because they name things that already exist:

* **Docker image tags** such as `rust-03ddf2ab2` were built from the old commits and are recorded
  under the names they were measured with. The table maps each to its published commit.
* **Commit messages** written before the rewrite cite old SHAs.

| Old (image tags, older commit messages) | Published | Subject |
|---|---|---|
| `a9188bce3` | `1f6e7c332` | Add Rust capture-proxy Cargo workspace skeleton |
| `ba8e25337` | `766b9d266` | Prove Java-compatible Murmur2 connection routing in Rust |
| `5802f17c2` | `3d99c3b54` | Make routing-hash casts explicit and satisfy strict clippy |
| `a54afb846` | `a320bb67e` | Implement capture-wire: single-pass canonical encoder, segmentation, publish chain |
| `29306b1b1` | `37c95750d` | Add the shared capture-config contract: patterns, limits, timing, policy |
| `d321c4335` | `c2c4698dd` | Implement the process-wide capture gate (HTTP-02) |
| `1184014c1` | `8574ccd18` | Implement process identity and the hard process boundary (SHARED-02, PROXY-01) |
| `e60b76872` | `72638fd2f` | Implement the orderly shutdown sequence (PROXY-03) |
| `0736dda3f` | `58db41a4e` | Update the live traceability checklist to verified state |
| `a139caa4e` | `936f36eb7` | Implement capture-observability: metric contract, events, logging, spans |
| `0a95b97ac` | `bb630783c` | Implement capture-http-scan and capture-transport |
| `3b8e5c7bb` | `a39b4b8f0` | Complete the configuration surface: CLI, inline JSON, endpoint, TLS, Kafka |
| `5cbdd11ae` | `5a1c4e906` | Add the AL2023 container image and record decisions OD-8..OD-10 |
| `b00c8670d` | `12a5122ed` | Implement capture-kafka: probe, membership, routing, publisher, retirement |
| `6c9e3a41b` | `a6a63a638` | Apply the OD-6 amendment and fix the aws-config feature set |
| `ba76d762e` | `615138ede` | Wire the capture-proxy binary: both modes run end-to-end |
| `6691e9947` | `ff4fe67ac` | Fix two image-build defects and update the traceability checklist |
| `3bcf18e81` | `3e0d623c9` | Drive the F record-flush deadline from the connection owner (gap B, transport half) |
| `748379d05` | `5f642891b` | Close gap B: idle records publish at their fixed F deadline |
| `9568cb3b0` | `ba9872c10` | Close gap A: the connection owner waits for its terminal acknowledgement |
| `2ff6080df` | `2a81acaa3` | Replace the capture gate's sleep-poll with an event-driven wake |
| `6d9dac9ed` | `f9a59ffa7` | Translate Java Kafka TLS store properties so SCRAM deployments start |
| `6a2edd703` | `55b6ad35a` | Make capture compromise actually terminate, pass through, and shut down |
| `d271a50bc` | `4d12d6cfe` | Record the MSK IAM proof through the proxy's own code |
| `a7b8c44b1` | `40582bd93` | Traceability: Gate 1 PASS and eleven rows PROVED on real-component evidence |
| `b60dc220e` | `12ffd2374` | Record wire-phase and kafkaCommit telemetry for all traffic |
| `96d851c88` | `8ec71e5b8` | Record the h2c prior-knowledge divergence as owner decision D7 |
| `03ddf2ab2` | `b42890373` | Pass HTTP/2 through, record connection metrics, sample ack latency once |
| `5b0ba6356` | `34e520f5a` | Phase 1 report: decided gates 1-4 and 6, profiles, Gate 5 first measurement |
| `a11c46b5e` | `d8bc98dbc` | Phase 1 owner review: Gate 5 ack FAIL, window root cause, fixed-rate results |
| `cf1eb57ff` | `9be5f7265` | Publish through several Kafka producers, one fixed producer per partition (OD-12) |
| `67f862088` | `7f24919b6` | Default to one Kafka producer until librdkafka's per-partition cap is resolved |
| `428ef064c` | `82f92cd26` | Build librdkafka with the two upstream idempotent-producer fixes |
| `2bb5df642` | `a5932d68c` | Record producer-window rework results: patched librdkafka, 1 vs 4 producers |
| `e84c1750c` | `909453eac` | Size Kafka producers automatically, capped at four, spread per leader broker |
| `9b882b12d` | `f01a5182e` | Take the MSK bootstrap endpoint from the environment in the IAM proof script |
