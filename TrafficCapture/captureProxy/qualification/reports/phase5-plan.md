# Phase 5 — remaining repository replacement work

Date: 2026-11-17
Status: **PLAN.** Kubernetes only (OD-14). `deployment/cdk/` is out of scope and is not touched by
any step below.

Plan reference: `rust-capture-proxy-rewrite-plan.md` Phase 5. Owner rulings: OD-14 (Kubernetes is
the only qualified target), OD-15 (330 s grace period).

## 0. Done in this change

| Commit | Item | Where | Evidence |
|---|---|---|---|
| `dc85130ce` | OD-15: proxy pods get `terminationGracePeriodSeconds: 330` | `orchestrationSpecs/.../setupCapture.ts` (`CAPTURE_PROXY_TERMINATION_GRACE_PERIOD_SECONDS`), both Deployment templates | `tests/captureProxyTerminationGracePeriod.test.ts`; `capture-proxy/tests/orchestration_contract.rs` checks every rendered pod against `ORDERLY_SHUTDOWN_HARD_STOP` |
| `264eeb6fb` | `proxyConfig.enableMSKAuth` reaches the proxy as `kafkaAuthType: msk-iam`; the container gets `AWS_REGION` from the `aws-metadata` ConfigMap (optional) | `setupCapture.ts` `makeProxyParamsDict`, container env | Before the fix the orchestration emitted `enableMSKAuth: true` with `kafkaAuthType: none`, which Java and Rust both reject; with the fix reverted the contract test fails (`--enable-msk-auth is only compatible with --kafka-auth-type=msk-iam, not none`). The Rust proxy signs MSK IAM tokens for `AWS_REGION`/`AWS_DEFAULT_REGION` and refuses to start without one; the Java `aws-msk-iam-auth` library resolved the region itself (believed to be from the broker host name; not re-checked here) |
| `fa89db0ea` | Orchestration-to-Rust configuration contract | `migration-workflow-templates/tests/captureProxyRustConfigContract.test.ts`, `tests/support/argoTemplateEvaluator.ts`; fixtures in `crates/capture-config/tests/fixtures/orchestration/`; `capture-config/tests/orchestration_contract.rs`, `capture-proxy/tests/orchestration_contract.rs` | 10 scenarios, fixtures regenerated with `jest -u` and compared byte for byte otherwise |
| `57b7d7033` | `capture_proxy` image built from `TrafficCapture/captureProxy/Dockerfile` | `buildImages/build.gradle` (BuildKit project `captureProxy`), `buildSrc/.../RegistryImageBuildUtils.groovy` (`dockerfile`, `inputPaths`), `trafficCaptureProxyServer/build.gradle` (Jib plugin removed) | Same image name, tag rules and platforms as the Jib image. Built locally for linux/arm64 through both paths (per-image `buildKit_captureProxy_arm64`, and the `captureProxy` target of the Gradle-generated `buildKitBakeAll` file) into the local registry; context transfer 2.5 MB. **linux/amd64 not built** (QEMU build stopped to avoid disturbing a measurement run) |

The in-cluster build job (`deployment/k8s/charts/components/buildImages`, which runs
`buildImagesToRegistry`) was not run: it needs a cluster with the EKS BuildKit backend.

Sections 1-3 below have since been carried out:

| Commit | Section | Notes |
|---|---|---|
| `14b769c43` | §1 `KafkaConfig` into `libraries/kafkaUtils` | Copied rather than moved in this commit, because the Java proxy still compiled against the original until §3 deleted it. `aws-msk-iam-auth` is a direct `runtimeOnly` dependency, guarded by `KafkaUtilsRuntimeClasspathTest` |
| `5e6819a46` | §2 `RustCaptureProxyContainer` | Tag `requiresProxyImage`; `-PcaptureProxyImage`, `-PproxyImageTests=include\|exclude\|only`; CI job `capture-proxy-image-tests` builds the image once and the stripes exclude those tests. The last commit that has the Java modules |
| the §3 commit | §3 delete the four modules | §3.1 below is the ground-truth check done before it. `TrafficCapture/dockerSolution/` (the docker-compose item) is owned elsewhere and was not changed |

## 1. Move the Java Kafka configuration that `libraries/kafkaUtils` uses

`libraries/kafkaUtils` is the only surviving consumer of `captureKafkaOffloader`. It uses exactly
one class, `org.opensearch.migrations.trafficcapture.kafkaoffloader.KafkaConfig`:

- `KafkaLoaderFromFile.java:46` — `KafkaConfig.KafkaParameters` (the JCommander parameter block).
- `KafkaLoaderFromFile.java:80` — `KafkaConfig.buildAdminProperties(...)`.
- `KafkaLoader.java:44,60` — `KafkaConfig.buildKafkaProperties(...)` (6-argument form) and the
  `AUTH_TYPE_*` constants.

The replayer does **not** use it (it has its own `replay/kafka/KafkaSaslAuthHelper.java`), and
nothing else imports the `kafkaoffloader` package.

**Target.** Move `KafkaConfig` into `libraries/kafkaUtils` itself, package
`org.opensearch.migrations.utils.kafka`. It is a single-consumer utility; putting it in
`coreUtilities` would give every module a `kafka-clients` dependency for no reason.

| File | Change |
|---|---|
| `TrafficCapture/captureKafkaOffloader/src/main/java/.../kafkaoffloader/KafkaConfig.java` | `git mv` to `libraries/kafkaUtils/src/main/java/org/opensearch/migrations/utils/kafka/KafkaConfig.java`; change the package. Drop `buildMembershipConsumerProperties` (proxy-only) unless a kafkaUtils caller appears. |
| `TrafficCapture/captureKafkaOffloader/src/test/java/.../kafkaoffloader/KafkaConfigTest.java` | `git mv` beside it; drop the membership-consumer cases with the method. |
| `libraries/kafkaUtils/src/main/java/.../KafkaLoader.java`, `KafkaLoaderFromFile.java` | Import from the new package. |
| `libraries/kafkaUtils/build.gradle` | Remove `implementation project(':TrafficCapture:captureKafkaOffloader')`. Add `runtimeOnly libs.aws.msk.iam.auth` (today it arrives transitively from `captureKafkaOffloader`'s `implementation`, and `KafkaConfig` names `software.amazon.msk.auth.iam.IAMLoginModule` only as a string, so nothing fails to compile without it — it fails at run time). Add `testImplementation` for whatever `KafkaConfigTest` needs (`libs.kafka.clients` is already there). |
| `DEVELOPER_GUIDE.md:129` | Remove the `captureKafkaOffloader` Maven coordinate example or replace it with a surviving module. |
| `kafkaAuthWiringPlan.md:33,48` | Historical plan; update the paths or mark the file historical. |

Risks:

- **MSK IAM silently lost from kafkaUtils.** The only symptom of forgetting the runtime dependency
  is `ClassNotFoundException: IAMLoginModule` when the migration console imports to MSK. Add a
  classpath test in kafkaUtils like the replayer's `TrafficReplayerRuntimeClasspathTest`.
- **`DEFAULT_KAFKA_CLIENT_ID = "HttpCaptureProxyProducer"`.** kafkaUtils inherits the proxy's
  client id. Keep it (broker-side quotas or ACLs may key on it) and say so in a comment, or rename
  deliberately with a release note.
- **Behaviour drift.** `KafkaConfigTest` is the only coverage; move it in the same commit.

## 2. Replace the replayer's dependency on the Java proxy test fixtures

`TrafficCapture/trafficReplayer/build.gradle:71`:

```groovy
testFixturesImplementation testFixtures(project(path: ':TrafficCapture:trafficCaptureProxyServer'))
```

It exists for one class, `ProxyWrittenTopic`
(`trafficReplayer/src/testFixtures/java/.../replay/kafka/ProxyWrittenTopic.java`), which builds a
Kafka topic written by `CaptureProxyContainer`. Despite its name, `CaptureProxyContainer`
(`trafficCaptureProxyServer/src/testFixtures/java/.../testcontainers/CaptureProxyContainer.java`)
runs the Java `CaptureProxy` **in-process on a thread**, not in a container. Users of
`ProxyWrittenTopic`:

- `replay/kafka/SourceAssemblyEvidenceTest.java`
- `replay/kafka/KafkaTopicDumperEvidenceTest.java`
- `replay/kafka/ProxyWrittenTopicSelfTest.java`
- `replay/TrafficReplayerTopLevelConstructionTest.java`

**Target.** A real container: `RustCaptureProxyContainer extends GenericContainer`, in the
replayer's own test fixtures, running the `capture_proxy` image with the flag form the proxy keeps
for exactly this (`--kafka-brokers`/`--kafkaConnection`, `--destination-uri`, `--listen-port`,
`--insecure-destination`, `--no-capture`). Readiness is the listen port (the proxy binds it only
after Kafka is ready). Kafka and the destination must be reachable from the container, so the
fixture puts all three on one Testcontainers `Network` with aliases instead of today's
`host:mappedPort` URIs.

| File | Change |
|---|---|
| `trafficReplayer/src/testFixtures/java/.../replay/kafka/RustCaptureProxyContainer.java` | New. Image from `SharedDockerImageNames`; arguments as above; `waitingFor(Wait.forListeningPort())`; exposes the mapped listen port. |
| `testHelperFixtures/src/testFixtures/java/.../testutils/SharedDockerImageNames.java` | Add `CAPTURE_PROXY`, defaulting to `migrations/capture_proxy:latest` and overridable by a system property so CI can point at the image it just built. |
| `trafficReplayer/src/testFixtures/java/.../replay/kafka/ProxyWrittenTopic.java` | Construct `RustCaptureProxyContainer`; put Kafka, the `SimpleNettyHttpServer` destination and the proxies on one network. The destination is a host process today, so either run it in a container or use `Testcontainers.exposeHostPorts` and `host.testcontainers.internal`. |
| `trafficReplayer/build.gradle:66-71` | Remove the `trafficCaptureProxyServer` test-fixtures dependency and its comment. Add a test-task dependency that guarantees the image exists (for example `dependsOn ':buildKit_captureProxy_<arch>'` behind a property, or a documented prerequisite), and pass the image reference as a system property. |
| `.github/gradle-test-images/infrastructure.txt` | Not applicable as is (it lists pulled images); CI must instead **build** the proxy image before the replayer test stripes that need it, or those tests are tagged and run in a job that does. |
| `.github/workflows/CI.yml` (`gradle-tests`) | Build or load the proxy image before the stripes; or tag the four tests (`@Tag("requiresProxyImage")`) and run them in one job after the image build. |

Risks:

- **CI cost.** Every replayer test stripe currently runs the proxy for free in the JVM. A Rust image
  build (release, LTO) is minutes; do it once per workflow run and share it, never per stripe.
- **Networking.** Containers cannot reach `localhost` of the host. Every URI in `ProxyWrittenTopic`
  changes, which is the most likely source of flaky failures.
- **Evidence semantics.** These tests are the replayer's evidence that it reads a topic written by
  *the* proxy (G1). After this change that is the Rust proxy, which is the point, but the Java
  replayer then never sees a Java-written topic in-repo again. Keep one committed Java-written
  fixture topic (or the existing golden transcripts) if a regression test against Java-format
  records is still wanted.
- **Ordering.** Item 2 must land before item 3, or `trafficReplayer` stops compiling.

## 3. Delete `trafficCaptureProxyServer`, `nettyWireLogging`, `captureOffloader`, `captureKafkaOffloader`

Dependency order today: `trafficCaptureProxyServer` → `nettyWireLogging` → `captureOffloader`, and
`trafficCaptureProxyServer`, `kafkaUtils` → `captureKafkaOffloader` → `captureOffloader`. After items 1
and 2, nothing outside the four depends on them.

| File | Change |
|---|---|
| `TrafficCapture/trafficCaptureProxyServer/`, `TrafficCapture/nettyWireLogging/`, `TrafficCapture/captureOffloader/`, `TrafficCapture/captureKafkaOffloader/` | Delete. |
| `settings.gradle:71,72,75,76` | Remove the four `include` lines. |
| `libraries/kafkaUtils/build.gradle:13` | Already removed by item 1. |
| `TrafficCapture/trafficReplayer/build.gradle:71` | Already removed by item 2. |
| `buildImages/build.gradle:61-62` | Remove the comment pointing at the Java module. |
| `TrafficCapture/README.md:35` | Point "Capture Proxy" at `captureProxy/` (a Rust README is needed: build, run, flags, inline JSON). |
| `TrafficCapture/README.md:324` | Remove the Capture Kafka Offloader section or replace it with the Rust Kafka publisher (`crates/capture-kafka`). |
| `SECURITY.md:17` | The link to `CaptureProxy.java` and the "10 seconds by default" Kafka blocking statement describe Java; restate for the Rust proxy's bounded publication budget (OD-13) and its configuration. |
| `docs/LoadTestingBackground.md:58,234,472` | Rename `trafficCaptureProxyServer`/`captureKafkaOffloader` to the Rust proxy. |
| `DEVELOPER_GUIDE.md` | Add the Cargo workspace: toolchain (`rust-toolchain.toml`), `third_party/rdkafka-sys/prepare.sh`, `cargo test --workspace --locked`, clippy/fmt gates. |
| `TrafficCapture/captureProxy/crates/capture-config/src/policy.rs:81` | Comment cites `trafficCaptureProxyServer/build.gradle`; reword as historical. Other Rust comments cite Java file/line numbers as provenance; leave them, but add one note in the workspace README that the cited Java sources were deleted at commit X. |
| `TrafficCapture/dockerSolution/src/main/docker/docker-compose.yml:36-60` | Already broken by the image switch (see §5); must be fixed by this step at the latest. |
| `.github/workflows/` | No workflow names these modules or runs Cargo. Add a Rust job to `CI.yml`: `prepare.sh`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`. Without it, deleting the Java tests leaves the proxy with **no** CI coverage at all. |
| `sonar-project.properties`, `whitesource.config` | No module paths today; check exclusions after the delete. |

Not changed (OD-14): `deployment/cdk/opensearch-service-migration/options.md:18` links to
`CaptureProxy.java`, and `lib/service-stacks/capture-proxy-stack.ts` builds a Java-style command
line. Both become stale; OD-14 accepts that. As amended (`a5672d249`), the accepted-risk note goes
in the release notes and `TrafficCapture/README.md`, and no file under `deployment/cdk/` is edited.

Risks:

- **Hidden test fixtures.** `captureOffloader`'s `testFixtures` are consumed by `nettyWireLogging`
  and `trafficCaptureProxyServer` only; confirm with `./gradlew projects` plus a full
  `./gradlew build -x test` after the delete, then a full test run.
- **Lost Java ground truth.** The Rust goldens (`capture-wire/tests/record_encoding.rs`,
  `capture-kafka/tests/routing_hash_golden.rs`) embed their expected bytes, so they survive the
  delete. What does not survive is the ability to re-derive them: the Java serializer
  (`captureOffloader`) and the Java proxy tests that produced the recorded transcripts are the
  reference. Before deleting, list every Rust test that cites a Java test as its source and check its
  expected values are committed, not regenerated. `qualification/java-fixtures/GenVectors.java`
  depends only on `kafka-clients` (Murmur2) and stays runnable.
- **Qualification tooling.** `qualification/perf/` and its specs build the Java comparison image
  with `:TrafficCapture:trafficCaptureProxyServer:jib_arm64` (that task no longer exists after this
  change). Pin the comparison image by digest (`phase1-owner-review.md` records
  `sha256:8abd1f4d…8357`) so Phase 6 comparisons still run. Done: `perf/lib.sh` and
  `perf/measurement-patch/build.sh` default `JAVA_PROXY_IMAGE` to the digest, and `perf/README.md`
  documents it.
- **Gradle build cache and IDE metadata** reference the module paths; harmless, but a clean
  checkout must build.

### 3.1 Lost Java ground truth: the check done before the delete

Checked against `5e6819a46`, the last commit that has the four Java modules (read a
cited Java file with `git show 5e6819a46:<path>`). Paths below are relative to
`TrafficCapture/captureProxy/crates/`.

**Result.** Nothing in the Cargo workspace reads, runs or regenerates from a file in the four
modules, at build time or test time. No `include_str!`/`include_bytes!`, `CARGO_MANIFEST_DIR`-relative
or `../..` path reaches them. The only path out of the workspace is `capture-wire/build.rs` to
`TrafficCapture/captureProtobufs`, which stays. Every expected value taken from a Java test is
written into the Rust test source or into a committed fixture. None is regenerated from Java.

Every Rust test that cites a Java test as its source (CO = `captureOffloader`, CKO =
`captureKafkaOffloader`, NWL = `nettyWireLogging`, TCPS = `trafficCaptureProxyServer`; all the cited
Java files were in those modules):

| Rust file | Tests | Cited Java test | Where the expected values are |
|---|---|---|---|
| `capture-wire/tests/record_encoding.rs` | whole file (B1-B21, K2) | `StreamChannelConnectionCaptureSerializerTest`, `KafkaCaptureFactoryTest` (CO, CKO) | Literal expected `TrafficStream` structs (e.g. B11 at :118-163); byte identity is checked against prost re-encoding, not against stored bytes |
| `capture-wire/tests/protobuf_size_vectors.rs` | T1-T11, `CSV_SOURCE` rows, boundary table | `CodedOutputStreamSizeUtilTest`, `StreamChannelConnectionCaptureSerializerTest` (CO) | Literal: the T vectors, the 38-row `CSV_SOURCE` (:136-177), the table (:287-310). The test at :187 checks invariants against a Rust port of the Java test's oracle |
| `capture-wire/tests/flush_deadline.rs` | B13-B15 | `StreamChannelConnectionCaptureSerializerTest` (CO) | Literal, with a manual clock |
| `capture-wire/tests/publish_chain_ordering.rs` | O1, O2 | `OrderedStreamLifecyleManagerTest` (CO) | Literal |
| `capture-kafka/tests/publisher_model.rs` | whole file | `CaptureKafkaPublisherTest`, `KafkaCaptureFactoryTest`, `CaptureRoutingStateTest`, `CapturePublishingMetricsTest` (CKO) | Literal, driven by a scripted producer |
| `capture-kafka/src/routing_state.rs` (tests) | 15 tests | `CaptureRoutingStateTest` (CKO) | Literal (one deliberate deviation, OD-6) |
| `capture-kafka/src/records.rs`, `write_gate.rs`, `membership.rs`, `topic_metadata.rs`, `capability_probe.rs` (tests) | 2, 2, 6, 3, 4 tests | `CaptureKafkaPublisherTest`, `CaptureKafkaCapabilityProbeTest`, `CaptureKafkaWriteGateTest`, `CaptureKafkaMembershipTest`, `TrafficTopicMetadataTest` (CKO) | Literal |
| `capture-kafka/src/client_config.rs` (tests) | 4 tests (:839, :1112, :1142, :1160) | `KafkaConfigTest` (CKO), including the two membership-consumer cases that stayed with the proxy when `KafkaConfigTest` moved to `libraries/kafkaUtils` | Literal |
| `capture-transport/src/request_pump.rs`, `response_pump.rs`, `connection.rs` (tests) | 3, 4, 7 tests | `RequestForwardedCaptureTest`, `SourceInterimResponseCaptureTest`, `ConditionallyReliableLoggingHttpHandlerTest` (NWL) | Literal. `connection.rs:1752` uses 60/40/60 ms where the cited Java test used 10/6/5 ms |
| `capture-transport/src/tls.rs` (tests) | cases 1-6, destination peer name | `FrontsideMtlsTest`, `BacksideConnectionPoolTest` (TCPS) | Literal; the PEMs are inline constants |
| `capture-transport/src/listener.rs` | worker sizing | `CaptureProxyNumThreadsTest` (TCPS) | Computed (`available_parallelism() * 2`) |
| `capture-config/src/kafka.rs`, `endpoint.rs`, `application.rs` | legacy auth conflict; Java URI vectors; explicit worker count | `CaptureProxySetupTest`, `CaptureProxyNumThreadsTest` (TCPS) | Literal (the URI vectors copied verbatim from `CaptureProxySetupTest.java:296-320`) |
| `capture-observability/tests/operational_events.rs` | startup summary line | `CaptureProxySetupTest` (TCPS) | Literal `JAVA_STARTUP_LINE` |
| `capture-observability/src/process_metrics.rs`, `log_deduper.rs` (tests) | 3, 1 tests | `CaptureProcessMetricsTest`, `UnauthenticatedClientLogDeduperTest` (TCPS) | Literal |
| `capture-http-scan/tests/framing_vectors.rs` | T1, T2, T5, T6 and the request vectors | `ChunkedTransferEncodingCaptureTest`, `RequestForwardedCaptureTest`, `ConditionallyReliableLoggingHttpHandlerTest` with `SimpleRequests.java` (NWL); `NettyScanningHttpProxyTest` (TCPS) | Literal byte vectors |
| `capture-http-scan/tests/fragmentation_properties.rs` | baseline; 3 property tests | `HeaderRemoverHandlerTest` (TCPS) | Literal baseline; the property tests compare fragmented runs with the Rust baseline (Java's `new Random(i)` seeds were not ported) |
| `capture-http-scan/src/response_classifier.rs`, `rewrite/remove.rs`, `rewrite/add.rs`, `suppression.rs` (tests) | 4, 3, 2, 2 tests | `SourceInterimResponseCaptureTest`, `HeaderValueFilteringCapturePredicateTest`, `ConditionallyReliableLoggingHttpHandlerTest` (NWL); `HeaderRemoverHandlerTest`, `TestHeaderRewrites`, `HeaderAdderHandlerTest` (TCPS) | Literal |

Two fixtures are not ports of a Java test but matter here:

- `capture-test-support/fixtures/murmur2-golden.json` (read by `capture-kafka/tests/routing_hash_golden.rs`)
  is the cross-language ground truth for routing. Its murmur2 values come from kafka-clients 4.2.0, not
  from the deleted modules, and `qualification/java-fixtures/GenVectors.java` (kafka-clients and `java.*`
  only) still regenerates it. Its `selected` partitions are computed inside `GenVectors.java:49`
  (`pos % parts.size()`), mirroring the deleted `CaptureRoutingState.java`.
- `capture-test-support/fixtures/goldens/*.json` are **not** Java-derived. `CAPTURE_GOLDENS_UPDATE=1`
  rewrites them from the Rust encoder; the Java replayer's `RustGoldenRecordCompatibilityTest` checks
  them and survives.

What is lost is the ability to re-check these values against Java, not the values:
`protobuf_size_vectors.rs:187` becomes a check against a Rust port of the oracle only, and every
`...Test.java:NN` line citation now resolves only through `git show`. The specifications behind
the case labels (wire spec §11, kafka spec §17.1, transport-http spec §15) are not in the
repository. Corrections to the risk text above: `record_encoding.rs` embeds expected decoded
structs, not bytes; `rewrite/remove.rs:339` cites `makeThinMessage :201-209`, which was at
`HeaderRemoverHandlerTest.java:196-203`.

## 4. Metric contracts used by dashboards and alarms

Shipped consumers of proxy metrics:

| Consumer | What it reads |
|---|---|
| `deployment/k8s/dashboards/capture-replay-dashboard.json` and its chart copy `deployment/k8s/charts/aggregates/migrationAssistantWithArgo/files/cloudwatch-dashboards/capture-replay-dashboard.json` (generated by `deployment/k8s/scripts/generate-capture-replay-dashboard.py`) | CloudWatch, `OTelLib="captureProxy"`: `bytesRead`, `bytesWritten`, `captureConnectionDuration`, `captureConnectionsActive`, `captureConnectionCloses`, `captureGapActive`, `captureHeartbeatDeadlineViolations`, `capturePublisherFailures`, `capturePublishingBytesAccepted`, `capturePublishingCompletions` (`outcome`), `capturePublishingRecordsAccepted`, `capturePublishingRecordsAwaitingAcknowledgement`, `captureWritersActive`, `captureWritersDraining`, `captureWritesRejected`, `fullRequest`, `gatheringRequestDuration`, `gatheringRequestExceptionCount`, `gatheringResponseDuration`, `gatheringResponseExceptionCount`, `kafkaCommitDuration`, `kafkaCommitExceptionCount`, `waitingForResponseDuration`; dimensions `qualifier`, `reason`, `recordKind`, `outcome`, `captureActivationId`, `mode`, `processId`, `sourceForwardingContinued` |
| `deployment/k8s/charts/aggregates/migrationAssistantWithArgo/templates/resources/grafanaDashboard.yaml` (generated by `generate-capture-replay-grafana-dashboard.py`) | Prometheus series names with unit suffixes, e.g. `capturePublishingRecordsAccepted_records_total`, `capturePublishingBytesAccepted_bytes_total`, `capturePublishingRecordsAwaitingAcknowledgement_records`, `captureConnectionsActive_connections`, `captureWritersActive_writers`, `captureConnectionCloses_connections_total` (`reason`), grouped by `exported_instance` |
| `deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json` | `fullRequest_count_total{exported_job="capture"}` — depends on `service.name="capture"` becoming the Prometheus `job` |
| `deployment/k8s/scripts/verify-kafka-observability.py`, helm `migration.validateKafkaObservability` | Kafka observability wiring; check what proxy series it asserts |

The Rust side already pins the contract: `crates/capture-observability/src/contract.rs`
(`SCOPE_NAME = "captureProxy"`, `SERVICE_NAME = "capture"`, the instrument table with Prometheus
series names, 1 s export interval) and `tests/metric_contract.rs` (47-instrument snapshot,
pipeline-vs-table in both directions, closed cardinality). What is missing is the link to the
**shipped files**: nothing fails if a dashboard references a series the proxy does not export.

| File | Change |
|---|---|
| `crates/capture-observability/tests/dashboard_contract.rs` (new) | Read the two CloudWatch JSON files, the Grafana JSON embedded in `grafanaDashboard.yaml`, and `load-test.json` from the repository; extract every metric they query for the proxy (`OTelLib="captureProxy"` / `otel_scope_name="captureProxy"` / `exported_job="capture"`); assert each name, Prometheus series name and dimension key exists in `contract::INSTRUMENTS`. A repo-relative path from `CARGO_MANIFEST_DIR`, like the orchestration fixtures. |
| `deployment/k8s/scripts/generate-capture-replay-*.py` | Optional: emit a machine-readable list of proxy series alongside the dashboards, so the Rust test reads that instead of parsing dashboard JSON. |
| `deployment/k8s/dashboards/capture-replay-dashboard.json` vs the chart copy | Identical today; the test should read both or assert they match. |

Risks:

- **Java-only series.** Any series a dashboard reads that Java emitted and Rust does not (the
  TRACEABILITY rows KAFKA-11 "4 queue-depth/latency instruments pending", HTTP-10) shows as an empty
  panel, not an error. The new test makes that a failure; decide per series whether to implement it
  or drop the panel.
- **Units.** Grafana series names embed the unit (`_records_total`, `_bytes_total`); a unit change
  in Rust renames the series. `metric_contract.rs` snapshots units, the new test ties them to the
  dashboards.
- **Rate arithmetic.** CloudWatch panels divide `Sum` by the period, which is only correct with the
  1 s export interval (`METRIC_EXPORT_INTERVAL`); keep it pinned.
- **`exported_job="capture"`.** Comes from `service.name`; an OTel resource change in Rust breaks
  the k6 dashboard silently today.

## 5. Other Phase 5 items not yet done, and known gaps

| Item | Files | State / risk |
|---|---|---|
| docker-compose dev stack uses Java flags and Java-baked certificates | `TrafficCapture/dockerSolution/src/main/docker/docker-compose.yml:36-60`, `docker-compose.large-requests.yml` | **Broken by the image switch.** The flags are accepted (Java aliases), but `--sslCertChainFile /usr/share/captureProxy/config/pub.pem` and `key.pem` were generated into the Java image by `trafficCaptureProxyServer`'s `generateSelfSignedCerts`; the Rust image has no such files. Verified with the image built here (with `--no-capture` instead of Kafka): it exits 70, `failed to read TLS material from /usr/share/captureProxy/config/pub.pem`. Fix: generate a certificate in the compose stack and mount it, or drop TLS on the dev proxy. Not Kubernetes, so not in this change. |
| CDK/ECS command line | `deployment/cdk/.../capture-proxy-stack.ts` | Out of scope (OD-14). May not start the Rust proxy; needs the release note OD-14 requires. |
| Image `/VERSION` file | Java Jib images carried the repo `VERSION` at `/VERSION`; the Rust image does not, and `capture-proxy --version` reports the Cargo version `0.1.0` | Decide whether the binary version should come from the repo `VERSION` (build arg into `Cargo` env) and whether anything reads `/VERSION` from this image. |
| Image build time in the cluster job | `deployment/k8s/charts/components/buildImages` → `buildImagesToRegistry` → `buildKitBakeAll` | The Rust release build (thin LTO, one codegen unit, librdkafka from source) runs inside buildkitd (4–8 CPU, 8–12 GiB). With `multiArchNative: false` the non-native architecture builds under QEMU, which can take an hour or more for Rust. Measure on a real cluster; consider `cargo-chef`-style dependency caching in the Dockerfile and `multiArchNative: true`. |
| CI image build | `.github/workflows/k8s-local-test.yml:115` (`buildImagesToRegistry_amd64`) | Now includes the Rust build. First run has no registry cache. |
| SBOM / licences | `.github/workflows/release-drafter.yml:167` (anchore SBOM on the image) | The image SBOM still works (it scans the image). Cargo crate licences are not in it as a Rust binary is static: add `cargo about`/`cargo deny check licenses` to the Rust CI job and attach the result. |
| `setHeader` documented format | `orchestrationSpecs/packages/schemas/src/userSchemas.ts:730` says each entry is `'Header-Name: value'`; Java and Rust both read alternating names and values | **Needs an owner ruling.** A spec written as documented is rejected (Rust, exit 2; Java crashed or set a garbage header). Either fix the description (and add an even-length refinement) or translate `Name: value` in the orchestration. The contract test pins today's rejection (`rejected-set-header-in-name-colon-value-form`). Same question for `suppressCaptureForHeaderMatch`, whose description gives no format. |
| Cluster-level `enableMSKAuth` | `KAFKA_EXISTING_CLUSTER_CONFIG.enableMSKAuth` | Drives the console and the CapturedTraffic CR only; the proxy needs `proxyConfig.enableMSKAuth` as well. Pre-existing; document or derive. |
| Kafka TLS without SASL (`ssl`) | — | No migration-spec field produces `kafkaAuthType: ssl`; workflow-managed clusters are plain or SCRAM, existing ones none or SCRAM. The proxy supports it; the orchestration cannot select it. |
| MSK IAM identity for the proxy pod | Deployment has no `serviceAccountName` | EKS Pod Identity must be associated with the namespace's default service account for MSK IAM to work. Unverified. |
| New proxy flags | `--kafka-producer-count`, `--kafka-publication-budget-bytes`, `--kafka-publication-budget-records` | Not exposed in the migration spec; defaults apply. Expose only if an owner wants them tunable per deployment. |
