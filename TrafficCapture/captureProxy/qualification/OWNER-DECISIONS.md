# Owner decisions

Authoritative record of behaviour decisions made by the component owner during the Rust capture-proxy
replacement. Implementation must follow this file. Each entry states the evidence that prompted the
question, the ruling, and what the Rust implementation therefore does.

A decision here may knowingly diverge from the Java implementation. Where it does, the divergence is
deliberate and approved — it is not a parity defect.

---

## OD-1 — Malformed HTTP framing fails closed, and the request-line limit is raised

**Evidence.** `LoggingHttpHandler.java:146-150` recognises only `TooLongHttpHeaderException`.
`TooLongHttpLineException` is a sibling class, not a subclass, and
`LoggingHttpHandler.java:101-103` configures only `setMaxHeaderSize`, leaving Netty's
`maxInitialLineLength` at its 4096 default. A request line over 4096 bytes — routine for long
OpenSearch `_search` URIs — plus bare-LF framing, duplicate `Content-Length`, and any chunk error put
Netty into `BAD_MESSAGE`, which emits no `LastHttpContent`. The critical-mutation gate lives entirely
in `ConditionallyReliableLoggingHttpHandler.channelFinishedReadingAnHttpMessage`, which is only
reached when the decoder completes a message, so none of the gate runs: no block predicate, no
`flushCommitAndResetStream`, no `validateCriticalMutationTrafficAcknowledgement`. Instead
`LoggingHttpHandler.java:506-508` forwards every subsequent byte raw and uncaptured until the
250 MiB / 5 minute limits kill the connection. A POST/PUT/DELETE/PATCH can therefore be applied at
the source with no complete Kafka representation, violating `proxyCaptureProtocol.md` §1.3. There is
no test coverage for any of it.

**Ruling.** Fail closed, and raise the request-line limit.

**Implementation.**

- A framing error is terminal for the connection: emit a `ConnectionException` observation, close the
  connection, and never forward unparsed bytes to the source.
- The maximum request-line length is explicitly configurable with a default well above 4096, derived
  from the configured maximum header bytes rather than left to an implementation default, so long
  URIs keep working and only genuinely malformed framing is refused.
- This is an externally visible change: traffic the Java proxy silently tunnelled is now refused.

---

## OD-2 — Strict HTTP line parsing by default, with an explicit opt-in lenient mode

**Evidence.** `buildImages/build.gradle:62` deliberately omits
`-Dio.netty...defaultStrictLineParsing=false` with a comment saying so, making production **strict**.
`trafficCaptureProxyServer/build.gradle:115-118` sets it **false** for that module's tests, while
`nettyWireLogging`'s tests leave it strict. The two Java suites that cover `LoggingHttpHandler`
therefore run under two different HTTP grammars, and nothing in the repository tests
`InvalidLineSeparatorException`.

**Ruling.** Strict by default, plus a documented opt-in lenient mode.

**Implementation.** Strict CRLF line parsing is the default and matches the production image. A
documented configuration flag enables acceptance of bare-LF line endings as an escape hatch for a
non-conforming client. The lenient mode is explicit and qualified, never accidental. The Java
test-only leniency is not ported as a default.

---

## OD-3 — Header rewriting is stateless per request and ignores responses

**Evidence.** `HeaderRemoverHandler.java:184-193` resets parse state on every outbound `write`, and
`HeaderAdderHandler.java:74-81` does the same; every response chunk is such a write. The class
Javadoc states the precondition ("As long as a response is written for each request... the handler
will reset itself correctly") but nothing enforces it. Concretely, with `Expect: 100-continue` the
interim response arrives mid-request, `resetState` flushes the buffered partial-header prefix, and
the remainder of the header line is forwarded verbatim — so a header configured for removal leaks to
the source, defeating the point of header removal. Pipelined requests get no rewrite at all. Every
existing multi-request Java test writes a complete response between requests; none writes a response
mid-request.

**Ruling.** Stateless per request; responses never perturb rewrite state.

**Implementation.** Rewrite state is scoped to the request currently being parsed. Removal and
addition are correct across `100-continue`, other interim responses, and pipelining. Kafka still
receives the original client bytes and the source receives the rewritten bytes.

---

## OD-4 — The capability probe validates the timestamp **type**, not a positive value

**Evidence.** `proxyCaptureProtocol.md` §3.3 step 3 requires publishing the probe with a producer
timestamp of zero and accepting the acknowledgement only when the broker returns a positive
timestamp, inferring that the broker replaced the zero with `LogAppendTime`. librdkafka makes that
impossible: it substitutes the client wall clock for a zero timestamp
(`rdkafka_msg.c:274-278`; `rdkafka` passes `record.timestamp.unwrap_or(0)` at
`base_producer.rs:481-482`). Proved empirically against a real MSK Serverless cluster: a
zero-timestamp produce to a `CreateTime` topic returned a positive value byte-identical to the
client's own pre-produce wall clock, so the "positive" test would have falsely passed.
`rdkafka 0.39`'s delivery report carries the timestamp *type*
(`Timestamp::LogAppendTime` vs `Timestamp::CreateTime`).

**Ruling.** Require the `LogAppendTime` type. Amend the protocol document to match, as an
owner-approved amendment.

**Implementation.** The probe produces with an ordinary timestamp and accepts the acknowledgement
only when the delivery report reports `Timestamp::LogAppendTime(t)` with `t > 0`. This is strictly
stronger than the zero trick: it reads the broker's own assertion about the timestamp's origin
instead of inferring it, and it correctly fails a `CreateTime` topic that the zero trick passed. The
probe's on-wire `CaptureCapabilityProbe` record is unchanged, including
`writerNodeId = captureActivationId + ":PROBE"`.

`docs/captureAndReplay/proxyCaptureProtocol.md` §3.3 must be updated as part of this work.

---

## OD-5 — A non-`LogAppendTime` traffic topic refuses startup

**Evidence.** MSK Serverless defaults topics to `CreateTime`. Proved by controlled experiment, same
client and cluster with only the topic configuration differing: the `CreateTime` topic stored a bogus
1970 client timestamp verbatim, while the `LogAppendTime` topic overwrote it with broker time. MSK
Serverless does accept `message.timestamp.type=LogAppendTime` as a topic override, so this is opt-in
rather than unavailable. Its `message.timestamp.difference.max.ms` is `i64::MAX`, so the broker will
not reject a skewed client timestamp on a `CreateTime` topic either.

**Ruling.** Refuse to start.

**Implementation.** When the capability probe detects a non-`LogAppendTime` topic, the process exits
with a distinct capture-configuration failure **before the listener opens**. A `CreateTime` topic
makes capture-before-forward validation and replayer expiration unsound, so refusing to start is
safer than capturing. The proxy does **not** attempt to change the topic configuration itself
(see OD-7).

---

## OD-6 — Backward broker-time movement is rejected beyond the skew allowance `S`

**Evidence.** `CaptureRoutingState.java:465-479` computes `elapsed = candidate - previous` and
rejects only `elapsed >= expirationMillis`, with no lower bound, then overwrites
`lastAcceptedHeartbeatLogAppendTime` unconditionally. `CaptureRoutingStateTest.java:73` deliberately
asserts this. `validateCriticalMutationTrafficAcknowledgement`
(`CaptureRoutingState.java:486-518`) has the same one-sided bound. A regressed baseline makes
`observation - baseline < E` easier to satisfy, widening the window in which a stale observation can
release a source write. `proxyCaptureProtocol.md` §5.4's literal wording also permits it, while
`captureAndReplayArchitecture.md` §§8-8.1 bound backward broker-time movement by `S` and the replayer
treats a backward move greater than `S` as immediately fatal.

**Ruling.** Reject backward movement beyond `S`.

**Implementation.** A heartbeat acknowledgement is accepted only when
`-S <= elapsed < E`. Backward movement within the configured clock-skew allowance `S` is tolerated
and does not regress the baseline below its skew-adjusted floor; movement worse than `S`
irreversibly compromises capture and follows `--capture-failure-policy`. This aligns the proxy with
the bound the replayer already enforces fatally.

---

## OD-7 — The proxy never changes the partition count

**Evidence.** `TrafficTopicMetadata.java` implements "increase but never reduce" partition growth,
and the rewrite plan's KAFKA-10 row carries that obligation forward. Separately, `rust-rdkafka`
0.39's `AdminClient` never authenticates over MSK IAM: every admin call timed out and librdkafka
never requested an OAUTHBEARER token, while the producer and consumer both did.

**Owner ruling, verbatim in substance.** The proxy should never increase the partition count.
Partition increase must always be out-of-band and intentional.

**Implementation.**

- The Rust proxy does **not** create topics and does **not** alter the partition count. Ever.
- Startup still **validates** the topic layout: contiguous partitions, a current leader for every
  partition, and a partition count consistent with the configuration. An invalid layout fails
  startup before the listener opens.
- Layout discovery uses producer/consumer metadata (`fetch_metadata`, `MetadataPartition::leader`),
  not `AdminClient`, so the admin IAM defect does not affect the proxy.
- KAFKA-10's "increase but never reduce" obligation is **retired by owner decision**, not by
  implementation convenience. The traceability row records this.
- Because the proxy no longer performs any admin operation, the `rust-rdkafka` admin IAM defect is
  out of scope for this rewrite. It is recorded so nobody re-introduces an admin dependency
  believing it works under IAM.

---

## Open items still awaiting a ruling

| # | Item |
|---|---|
| D2 | Is bounded, lossless Kafka backpressure mandatory in the parity release? |
| D3 | Qualified deployment targets: Kubernetes only, or Kubernetes plus the older CDK/ECS path? |
| D4 | Confirm publisher sharding stays deferred until the single-owner implementation is measured. |
| D5 | `proxyCaptureProtocol.md` §7.3 specifies a controller-managed `COMPROMISED_AWAITING_CONTROLLER_ACK` state that blocks uncaptured forwarding until a controller durably acknowledges. Whether Java implements it, and whether Rust must, is unresolved. |
| D6 | Production Kubernetes sets no `terminationGracePeriodSeconds` (`setupCapture.ts:829-852`), so the default 30 s `SIGKILL` pre-empts the entire tested 60/240/270/300 s shutdown sequence. Does the orchestration get fixed, or does the Rust shutdown budget shrink to fit 30 s? |
