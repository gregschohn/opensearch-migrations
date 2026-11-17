# Phase 5 — remaining repository replacement work

Date: 2026-11-17
Status: **PLAN, partly done** (§0 lists what is done; §4 and the docker-compose, `/VERSION` and
licence rows of §5 are done). Kubernetes only (OD-14). `deployment/cdk/` is out of scope and is not
touched by any step below.

Plan reference: `rust-capture-proxy-rewrite-plan.md` Phase 5. Owner rulings: OD-14 (Kubernetes is
the only qualified target), OD-15 (330 s grace period).

## 0. Done in this change

| Commit | Item | Where | Evidence |
|---|---|---|---|
| `dc85130ce` | OD-15: proxy pods get `terminationGracePeriodSeconds: 330` | `orchestrationSpecs/.../setupCapture.ts` (`CAPTURE_PROXY_TERMINATION_GRACE_PERIOD_SECONDS`), both Deployment templates | `tests/captureProxyTerminationGracePeriod.test.ts`; `capture-proxy/tests/orchestration_contract.rs` checks every rendered pod against `ORDERLY_SHUTDOWN_HARD_STOP` |
| `264eeb6fb` | `proxyConfig.enableMSKAuth` reaches the proxy as `kafkaAuthType: msk-iam`; the container gets `AWS_REGION` from the `aws-metadata` ConfigMap (optional) | `setupCapture.ts` `makeProxyParamsDict`, container env | Before the fix the orchestration emitted `enableMSKAuth: true` with `kafkaAuthType: none`, which Java and Rust both reject; with the fix reverted the contract test fails (`--enable-msk-auth is only compatible with --kafka-auth-type=msk-iam, not none`). The Rust proxy signs MSK IAM tokens for `AWS_REGION`/`AWS_DEFAULT_REGION` and refuses to start without one; the Java `aws-msk-iam-auth` library resolved the region itself (believed to be from the broker host name; not re-checked here) |
| `fa89db0ea` | Orchestration-to-Rust configuration contract | `migration-workflow-templates/tests/captureProxyRustConfigContract.test.ts`, `tests/support/argoTemplateEvaluator.ts`; fixtures in `crates/capture-config/tests/fixtures/orchestration/`; `capture-config/tests/orchestration_contract.rs`, `capture-proxy/tests/orchestration_contract.rs` | 10 scenarios, fixtures regenerated with `jest -u` and compared byte for byte otherwise |
| `57b7d7033` | `capture_proxy` image built from `TrafficCapture/captureProxy/Dockerfile` | `buildImages/build.gradle` (BuildKit project `captureProxy`), `buildSrc/.../RegistryImageBuildUtils.groovy` (`dockerfile`, `inputPaths`), `trafficCaptureProxyServer/build.gradle` (Jib plugin removed) | Same image name, tag rules and platforms as the Jib image. Built locally for linux/arm64 through both paths (per-image `buildKit_captureProxy_arm64`, and the `captureProxy` target of the Gradle-generated `buildKitBakeAll` file) into the local registry; context transfer 2.5 MB. **linux/amd64 not built** (QEMU build stopped to avoid disturbing a measurement run) |

| `65bd752f0` | §4: shipped dashboards tied to the metric contract | `crates/capture-observability/tests/dashboard_contract.rs` | 11 tests. Every proxy series the dashboards and scripts read is exported (§4 result table); the readers' union equals `Exposure::ExternallyConsumed`. Mutation-checked: re-uniting `fullRequest`, renaming `captureConnectionCloses`' `reason`, or changing `SERVICE_NAME` fails the shipped-file tests |
| `9a75c3dda` | §5: binary version and image `/VERSION` from the repository `VERSION` | `capture-config/src/cli.rs` (`PROXY_VERSION`), `Dockerfile` (`CAPTURE_PROXY_VERSION` build argument), `buildImages/build.gradle` | Image built with the argument: `capture-proxy --version` prints `capture-proxy 3.4.1`; `/VERSION` is byte-identical to the repository file (`3.4.1\n`). The Dockerfile fails the build if the binary reports anything else |
| `498c70167` | docker-compose ports published on `127.0.0.1` only | both compose files; `TrafficCapture/dockerSolution/build.gradle` (`SERVICES_HOST`) | `docker compose config` shows `host_ip: 127.0.0.1` for every port; `lsof` showed the published listeners on `127.0.0.1` only |
| `1237a6cc1` | §5: docker-compose dev TLS certificate generated in the stack | `capture-proxy-dev-tls.sh`, `capture-proxy-tls` init service in both compose files | See the §5 docker-compose row |
| `8ac9634e3` | §5: large-requests stack no longer passes a Java-only producer property | `docker-compose.large-requests.yml`, `kafka-producer.properties` (deleted), `test-large-requests.sh` | See the §5 docker-compose row |
| `8c3698edb` | §5: licence check for the Cargo workspace | `TrafficCapture/captureProxy/deny.toml` | `cargo deny --locked check licenses` (cargo-deny 0.21.0): `licenses ok`, no warnings, 269 crates; removing `Zlib` from the allow list fails it on `foldhash` |

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
| `TrafficCapture/dockerSolution/src/main/docker/docker-compose.yml:36-60` | **Done** (`1237a6cc1`, `8ac9634e3`; see §5). |
| `.github/workflows/` | No workflow names these modules or runs Cargo. Add a Rust job to `CI.yml`: `prepare.sh`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, and `cargo deny --locked check licenses` (config `TrafficCapture/captureProxy/deny.toml`; install with `cargo install --locked cargo-deny`). Without it, deleting the Java tests leaves the proxy with **no** CI coverage at all. `tests/dashboard_contract.rs` reads files outside the Cargo workspace (`deployment/k8s/...`, `migrationConsole/...`), so the job needs a full checkout. |
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

### §4 result (done, `65bd752f0`)

`tests/dashboard_contract.rs` reads the shipped files and checks, per consumer:

- **CloudWatch** (both copies, asserted byte-identical): every metric array and `SEARCH` pinned to
  `OTelLib="captureProxy"`. The instrument must exist; beyond the collector's `OTelLib` and
  `qualifier`, the dimension set must equal the instrument's exactly (CloudWatch matches exact
  dimension sets; `valuesEks.yaml` deletes `service.name`, `service.instance.id` and
  `telemetry.sdk.*` before `resource_to_telemetry_conversion`); filter values (`outcome="failed"`)
  must be in the closed domain. A proxy-looking metric without the `OTelLib` pin fails.
- **Grafana and k6** (`PromQL`, via a small lexer): every selector that is pinned to the proxy, names
  an exported proxy series, or merely looks like one (`capture<Upper>…`, or a contract instrument
  name followed by `_`, which is what catches a re-unit) must be an exported series name with the
  right histogram suffix; label matchers and grouping labels must be instrument dimensions or
  resource labels (`exported_instance`, `exported_job`, `le` on `_bucket`); a proxy-only series
  pinned to another scope or job fails (catches `SCOPE_NAME`/`SERVICE_NAME` drift); every k6 proxy
  selector must carry `exported_job="capture"`.
- **Scripts** (by token): `verify-kafka-observability.py` names **no** proxy series (it checks Kafka
  broker/PVC series and that both dashboards match their generators; the test asserts this so a
  later proxy assertion is noticed); `capture-replay-gameday.sh` and `replayer_tests.py` are
  scanned too.
- The union of instruments read equals `Exposure::ExternallyConsumed` (25), both directions.

Every proxy series a shipped consumer reads is exported by the Rust proxy. **No series is missing;
nothing was implemented and no panel needs dropping.**

| Instrument | Exported series (Prometheus) | Read by | Status |
|---|---|---|---|
| `bytesRead`, `bytesWritten` | `…_bytes_total` | CW, Grafana | exported |
| `fullRequest` | `fullRequest_count_total` | CW, Grafana, k6 | exported |
| `gatheringRequestDuration`, `gatheringResponseDuration` | `…_milliseconds` (`_sum`/`_count`; k6 `_bucket`) | CW, Grafana, k6 | exported |
| `waitingForResponseDuration` | `waitingForResponseDuration_milliseconds` | CW, Grafana (scope-pinned) | exported |
| `kafkaCommitDuration` | `kafkaCommitDuration_milliseconds` (`_sum`/`_count`/`_bucket`) | CW, Grafana | exported; Grafana query **not scope-pinned** (finding below) |
| `gatheringRequestExceptionCount`, `gatheringResponseExceptionCount` | `…_total` | CW, Grafana | exported |
| `kafkaCommitExceptionCount` | `kafkaCommitExceptionCount_total` | CW, Grafana | exported; Grafana query **not scope-pinned** |
| `kafkaCommitCount` | `kafkaCommitCount_total` | `replayer_tests.py` | exported |
| `captureConnectionDuration` | `captureConnectionDuration_milliseconds` | CW | exported |
| `captureConnectionsActive` | `captureConnectionsActive_connections` | CW, Grafana (`by exported_instance`) | exported |
| `captureConnectionCloses` (`reason`) | `captureConnectionCloses_connections_total` | CW, Grafana (`by reason`) | exported |
| `activeConnection` | `activeConnection_count` | k6 | exported |
| `captureGapActive` (`processId`, `captureActivationId`, `mode`, `sourceForwardingContinued`) | `captureGapActive_processes` | CW, Grafana | exported |
| `capturePublishingRecordsAccepted`, `capturePublishingBytesAccepted` (`recordKind`) | `…_records_total`, `…_bytes_total` | CW, Grafana | exported |
| `capturePublishingCompletions` (`recordKind`, `outcome`) | `capturePublishingCompletions_records_total` | CW, Grafana | exported |
| `capturePublishingRecordsAwaitingAcknowledgement` (`recordKind`) | `…_records` | CW, Grafana, gameday script | exported |
| `captureWritersActive`, `captureWritersDraining` | `…_writers` | CW, Grafana | exported |
| `captureWritesRejected` | `captureWritesRejected_writes_total` | CW, Grafana | exported |
| `capturePublisherFailures` (`reason`) | `capturePublisherFailures_failures_total` | CW, Grafana | exported |
| `captureHeartbeatDeadlineViolations` | `…_violations_total` | CW, Grafana | exported |

The KAFKA-11 "pending" instruments (`capturePublishingQueueDepthRecords`/`Bytes`,
`…AcceptToSubmitMillis`, `…SubmitToDeliveryMillis`, `…OldestUnacknowledgedAgeMillis`) are new
Rust-only names that no dashboard reads, so they are not a dashboard gap.

**Finding (open, ratcheted in the test).** The Grafana "Proxy latency" (Kafka publish → ack mean
and p95) and "Proxy faults" (Kafka commit exceptions) queries use `kafkaCommitDuration_*` and
`kafkaCommitExceptionCount_total` **without** `otel_scope_name="captureProxy"`, and the replayer
registers instruments of the same names (`IKafkaConsumerContexts.java:38`,
`RootReplayerContext.java:97`; `replayer_tests.py` expects the replayer to export
`kafkaCommitCount_total`). Its main code does not create that context today, so the panels show
proxy data only in practice. Fix in `generate-capture-replay-grafana-dashboard.py` with
`sel=PROXY` on those three queries (as its `waitingForResponseDuration` query already does) and
regenerate; then remove the names from `KNOWN_UNSCOPED_SHARED_GRAFANA_SERIES`. Not done here: it
is a dashboard change owned with the generators.

Not done: the optional machine-readable series list from the generators (the test parses the
dashboard JSON directly).

## 5. Other Phase 5 items and known gaps

| Item | Files | State / risk |
|---|---|---|
| docker-compose dev stack uses Java flags and Java-baked certificates | `TrafficCapture/dockerSolution/src/main/docker/docker-compose.yml:36-60`, `docker-compose.large-requests.yml` | **Done** (`1237a6cc1`, `8ac9634e3`, `498c70167`). Was broken by the image switch: the Java image had `pub.pem`/`key.pem` generated into it (`generateSelfSignedCerts`, one key shared by every copy), the Rust image has none, and the proxy exited 70. **Fix: generate a certificate in the stack**, not drop TLS, because `services.yaml`, the README, `test-large-requests.sh` and CI's `docker-compose-e2e-test` (`replayer_tests.py`) all address the source as `https://capture-proxy:9200` / `https://localhost:9200`. A `capture-proxy-tls` init service (the stack's own Kafka image, which ships openssl; AL2023 has no openssl CLI; root, no network) runs the committed `capture-proxy-dev-tls.sh`, which writes a self-signed RSA-2048 certificate (SANs `capture-proxy`, `localhost`, `127.0.0.1`) and key into a volume owned by uid 10001 (key 0400) and keeps existing files on re-run; the proxy mounts it read-only at the old path and waits on `service_completed_successfully`, so its flags are unchanged. The large-requests stack also passed `--kafkaPropertyFile` with the Java-only `max.request.size`, which the Rust proxy refuses (exit 78); removed. All published ports are now `127.0.0.1` only, and the Gradle `dockerCompose` block sets `SERVICES_HOST=127.0.0.1` because the avast plugin otherwise probes ports at the Docker network gateway on Linux. **Verified** with the image built here (`localhost:5001/migrations/capture_proxy:p5ops-a5672d249-ver1`) under unique compose projects, proxy and its dependencies only, torn down after: init exits 0; proxy runs, listens on `127.0.0.1:9200` only, presents `CN=capture-proxy`; `GET /` through it returns the Elasticsearch banner and the request and response are on the topic; large-requests stack: a 3.2 MB `_bulk` (3000 docs) returns 200, all documents indexed, largest topic batch 1,034,565 bytes. **Not verified:** Gradle `composeUp` (the avast plugin path, including `SERVICES_HOST`), the replayer and migration-console services, and the full `test-large-requests.sh`. |
| Intermittent capture termination seen in the large-requests stack | `capture-kafka/src/publisher_actor.rs` (heartbeat scheduling, broker-time continuity) | **Needs investigation; not caused by the compose change.** In 2 of 4 runs of `docker-compose.large-requests.yml` (24 partitions, default `H` = 10 s, `E` = 30 s) the proxy exited 78 after 1–3 minutes: `capture_broker_time_violation` then `Heartbeat broker time exceeded the configured expiration interval … previous=…, candidate=… (+66.7 s and +98.7 s), expirationMillis=30000 (reason=broker_time)`. In the 66.7 s case no partition had a record after the startup ones except the client traffic that triggered the check (21 of 24 partitions held exactly one record, where a healthy run holds one per 10 s), so periodic heartbeats had stopped, and no `heartbeat_deadline` failure fired first. Not reproduced in 2 further runs of the same file (heartbeats every 10 s, idle and after the 3.2 MB request), with `--heartbeat-interval-seconds 2` and 24 partitions, or in `docker-compose.yml` (2 partitions) with default or 2 s `H`. In one failing run Kafka CLI tools were running inside the broker container; in the other, clients were connecting while the destination was still down. Root cause unknown; worth reproducing on the qualification harness with 24 partitions before Phase 6. |
| CDK/ECS command line | `deployment/cdk/.../capture-proxy-stack.ts` | Out of scope (OD-14). May not start the Rust proxy; needs the release note OD-14 requires. |
| Image `/VERSION` file | `capture-config/src/cli.rs`, `Dockerfile`, `buildImages/build.gradle` | **Done** (`9a75c3dda`). `capture-proxy --version` reports `CAPTURE_PROXY_VERSION`, read at compile time, falling back to the Cargo version when absent or empty. The Dockerfile takes it as a build argument (the repository `VERSION` is outside the `TrafficCapture/` context), fails the build if the binary reports anything else, and writes the reported version to `/VERSION`, as the Jib images did; `buildImages/build.gradle` passes the trimmed `VERSION` file (evaluated: `3.4.1`). `grep -rn '/VERSION'` finds no reader of `/VERSION` in the proxy image (the console reads `~/VERSION` in its own image; RFS uses `/rfs-app/VERSION`); it is kept for parity. Verified on an image built with the argument; the Gradle BuildKit task itself was only configured (`--dry-run`), not run. |
| Image build time in the cluster job | `deployment/k8s/charts/components/buildImages` → `buildImagesToRegistry` → `buildKitBakeAll` | The Rust release build (thin LTO, one codegen unit, librdkafka from source) runs inside buildkitd (4–8 CPU, 8–12 GiB). With `multiArchNative: false` the non-native architecture builds under QEMU, which can take an hour or more for Rust. Measure on a real cluster; consider `cargo-chef`-style dependency caching in the Dockerfile and `multiArchNative: true`. |
| CI image build | `.github/workflows/k8s-local-test.yml:115` (`buildImagesToRegistry_amd64`) | Now includes the Rust build. First run has no registry cache. |
| SBOM / licences | `TrafficCapture/captureProxy/deny.toml`; `.github/workflows/release-drafter.yml:167` (anchore SBOM on the image) | **Config done** (`8c3698edb`); **CI wiring not done** (another owner has `.github/workflows/`). The image SBOM still works but does not see the crates in the static binary. `cargo deny --locked check licenses` (run after `prepare.sh`, from `TrafficCapture/captureProxy`) passes with cargo-deny 0.21.0: `licenses ok`, no warnings, 269 crates on the four image/developer targets; allow list Apache-2.0, MIT, ISC, BSD-2-Clause, BSD-3-Clause, Unicode-3.0, Zlib, with unused entries denied. **CI should call it** in the Rust job (§3, `.github/workflows/` row). It checks crate metadata only: librdkafka (BSD-2-Clause) and zstd compiled by the `-sys` crates, and the linked OpenSSL, Cyrus SASL and zlib, need their notices in the image's third-party notices. |
| `setHeader` documented format | `orchestrationSpecs/packages/schemas/src/userSchemas.ts:730` says each entry is `'Header-Name: value'`; Java and Rust both read alternating names and values | **Needs an owner ruling.** A spec written as documented is rejected (Rust, exit 2; Java crashed or set a garbage header). Either fix the description (and add an even-length refinement) or translate `Name: value` in the orchestration. The contract test pins today's rejection (`rejected-set-header-in-name-colon-value-form`). Same question for `suppressCaptureForHeaderMatch`, whose description gives no format. |
| Cluster-level `enableMSKAuth` | `KAFKA_EXISTING_CLUSTER_CONFIG.enableMSKAuth` | Drives the console and the CapturedTraffic CR only; the proxy needs `proxyConfig.enableMSKAuth` as well. Pre-existing; document or derive. |
| Kafka TLS without SASL (`ssl`) | — | No migration-spec field produces `kafkaAuthType: ssl`; workflow-managed clusters are plain or SCRAM, existing ones none or SCRAM. The proxy supports it; the orchestration cannot select it. |
| MSK IAM identity for the proxy pod | Deployment has no `serviceAccountName` | EKS Pod Identity must be associated with the namespace's default service account for MSK IAM to work. Unverified. |
| New proxy flags | `--kafka-producer-count`, `--kafka-publication-budget-bytes`, `--kafka-publication-budget-records` | Not exposed in the migration spec; defaults apply. Expose only if an owner wants them tunable per deployment. |
