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
(`CaptureRoutingState.java:486-518`) has the same one-sided bound.

*Corrected harm model (the original text here claimed a regressed baseline widens the pre-forward
window; that was arithmetically backwards — a smaller baseline makes `observation - baseline`
larger, so the pre-forward check becomes **stricter**, not looser).* The real harm of unbounded
backward movement is on the heartbeat side: the baseline stops reliably tracking broker time, so the
continuity chain — each accepted heartbeat within `E` of the previous — can be satisfied by a clock
that jumps around, and "capture is live" is then no longer actually proven by it. The spec's own D-4
rationale carries the same backwards claim.

`proxyCaptureProtocol.md` §5.4's literal wording also permits the regression, while
`captureAndReplayArchitecture.md` §§8-8.1 bound backward broker-time movement by `S` and the replayer
treats a backward move greater than `S` as immediately fatal.

**Ruling.** Reject backward movement beyond `S`.

**Implementation.** A heartbeat acknowledgement is accepted only when
`-S <= elapsed < E`. Backward movement within the configured clock-skew allowance `S` is tolerated
and does not regress the baseline below its skew-adjusted floor; movement worse than `S`
irreversibly compromises capture and follows `--capture-failure-policy`. This aligns the proxy with
the bound the replayer already enforces fatally.

---

### OD-6 amendment — the lower bound applies to the heartbeat update ONLY

The original ruling said to apply the same `-S` lower bound to the pre-forward critical-mutation
check. **That was wrong and is withdrawn.** The two checks compare different quantities:

* The **heartbeat** check compares two heartbeat timestamps on the same partition, published `H`
  apart. A negative result there means broker time moved backward, which is exactly what `S`
  bounds. The lower bound belongs here.
* The **pre-forward** check compares a *traffic* record's `LogAppendTime` against the last accepted
  *heartbeat* on that partition. These are different records, and the baseline can legitimately
  advance between the traffic record's acknowledgement and the moment its validation runs. So
  `observation - baseline` can be legitimately negative by roughly `H` plus handler delay with no
  clock skew whatsoever. With the defaults `H = 10 s` and `S = 5 s`, a perfectly valid critical
  mutation would be rejected as a `BrokerTime` capture failure and, under fail-closed, exit 78.

Crucially, a negative pre-forward result is the **safe** direction: it means capture authority is
*fresher* than the record being forwarded. The danger the check guards against is the opposite —
an observation appended long after the last proven-live heartbeat, which is what `< E` catches.

The protection OD-6 was actually after is that a *regressed baseline* must not widen the pre-forward
window. Bounding the heartbeat update achieves that completely: once the baseline cannot regress
below its skew-adjusted floor, the pre-forward check needs no lower bound of its own.

**Final rule:**

* heartbeat acknowledgement accepted when `-S <= elapsed < E`;
* pre-forward check accepted when `elapsed < E`, with **no** lower bound, matching Java.

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

## OD-8 — HTTP/1 pipelining is fully supported and specified

**Evidence.** `proxyCaptureProtocol.md` §1.2 places pipelined requests outside the settled
contract, and the rewrite plan says "Do not add HTTP/1 pipelining support incidentally". But OD-3
requires header rewriting to be correct across pipelining, which is only achievable if framing
reports every message boundary. Java holds a single `currentRequest`/`haveParsedFullRequest`, so a
second request arriving in the same read clobbers the first's decoder result and only one
end-of-message is emitted.

**Ruling.** Keep full pipelining support.

**Implementation.** The scanner reports every message boundary; the rewriter resets per request
(tested at 1-byte, 5-byte, and whole-message fragmentations); each captured request gets its own
`EndOfMessageIndication` with its own lengths and its own `requestForwarded`. This resolves the
document's open exclusion toward "specify the behaviour" rather than "forbid it", and
`proxyCaptureProtocol.md` §1.2 should be updated accordingly.

One consequence to record: with a suppressed request and a captured request in the **same read**, the
single `read` observation necessarily contains both requests' bytes. That is unavoidable at one
observation per read and Java has the identical property. The suppressed request gets no indicator;
the captured one does.

---

## OD-9 — Whitespace before a header's colon is rejected

**Evidence.** Netty accepts it for requests and folds it into the header name
(`HttpObjectDecoder.java:1036-1046`), so `Content-Length : 5` becomes a header named
`"Content-Length "` that Netty's own framing ignores while some origin servers honour it. RFC 7230
§3.2.4 mandates rejecting it. `httparse` rejects it natively.

**Ruling.** Reject it.

**Implementation.** A framing error, terminal under OD-1. The reasoning is capture-specific rather
than merely standards-compliance: a header the proxy ignores but the source may honour is exactly
the desync primitive that makes capture-before-forward meaningless, because Kafka would record a
different request than the source executes.

A second, smaller strictness comes free with `httparse`: control characters in a header *value* are
rejected, where Netty validates only the handful of names `PassThruHttpHeaders` retains and forwards
the rest unchecked.

**Externally visible:** a client whose requests Java forwarded is now refused.

---

## OD-10 — The `internetFacing` / client-auth coupling is out of scope

**Evidence.** The capture proxy's Kubernetes Service defaults to `serviceType=LoadBalancer` with
`internetFacing=false`, which annotates the load balancer internal (AWS `scheme=internal`, GCP ILB,
Azure internal). That default is private and compliant. But `internetFacing=true` makes the proxy
reachable from outside the VPC, and nothing couples that to `--requireClientAuth`; the proxy
terminates client TLS and would accept anonymous connections, forwarding them to the source cluster.

**Ruling.** Out of scope for this rewrite. Recorded with remediation, not actioned.

**Remediation for whoever picks it up.** In
`orchestrationSpecs/packages/schemas/src/userSchemas.ts`, the `internetFacing` field (line 716) and
the capture proxy's `requireClientAuth` input should be bound by a schema-level refinement requiring
`requireClientAuth === true` whenever `internetFacing === true`, so an internet-facing capture proxy
cannot be deployed without front mTLS.

Note separately that `--insecureDestination` makes the destination TLS connector accept **any**
certificate. That is deliberate parity with Java's `InsecureTrustManagerFactory` and is
production-reachable; `capture-transport` exposes `certificate_validation_disabled()` so startup can
alarm on it.

---

## OD-11 — HTTP/2 prior-knowledge connections are passed through uncaptured

**Evidence.** See open item D7 (now resolved). Rust closed an `h2c` prior-knowledge connection as
`malformed_framing` and the origin received 0 bytes; Java forwards it uncaptured, through the
`BAD_MESSAGE` tunnel that OD-1 removed for HTTP/1.

**Ruling.** Pass HTTP/2 through rather than surprise a user in production.

**Implementation.** A connection whose first bytes are exactly the 24-byte HTTP/2 client connection
preface (`PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`, RFC 9113 §3.4) is switched, before any capture
observation, into an opaque tunnel for its whole life: bytes are forwarded byte-exact in both
directions, nothing is scanned, rewritten, or captured, and the capture-before-forward gate does
not apply. This is the narrow, explicit form of Java's observable behaviour: only the exact preface
at connection start qualifies, so malformed HTTP/1 still fails closed under OD-1. The connection
still emits its terminal `CloseObservation`, as Java's does, and HTTP/2 requests are not replayed —
the same accepted gap as Java's hardcoded `HTTP/2.*` suppression. HTTP/1.1 `Upgrade` → `101` was
already tunnelled and is unchanged.

---

## OD-12 — Several Kafka producers, one fixed producer per partition

**Evidence.** Phase 1 review §5.5. A single librdkafka producer saturates its in-flight window.
librdkafka sends one partition per ProduceRequest, and idempotence caps each broker connection at
five requests in flight (`RD_KAFKA_IDEMP_MAX_INFLIGHT`, enforced at `rdkafka_conf.c:4325-4333`).

* Under a 750 ms broker delay the producer ran 4.95 requests per round trip, and 202 of 204 sends
  waited for a response to free a slot.
* On a healthy broker at Gate 5 load `waitresp_cnt` was 5, and about half of every acknowledgement
  was queueing.
* Java packs every partition led by a broker into one request and stays inside the same window.

Result: Gate 5's ack p99 fails (1.36–1.52×) and a slow broker costs two round trips per
acknowledgement.

**Ruling.** Use multiple producers, so there is less blocking on in-flight slots.
`captureAndReplayArchitecture.md` §5.1 already anticipated this: *"If one producer becomes a
measured throughput bottleneck, the proxy may add producer instances and assign each producer a
mutually exclusive set of Kafka partitions."*

Raising `max.in.flight` above 5 was considered and rejected:

* librdkafka refuses it with idempotence on, and kafka-clients does too.
* The broker de-duplicates only the last five batches per producer and partition.
* `proxyCaptureProtocol.md` makes `max.in.flight.requests.per.connection <= 5` a correctness
  setting.
* Disabling idempotence to lift the cap would let a retry reorder or duplicate one connection's
  observations.

Several producers give the same extra capacity, 5 × N requests per broker, without weakening any
of this.

**Implementation.**

* `--kafka-producer-count` (`CAPTURE_PROXY_KAFKA_PRODUCER_COUNT`) takes `1..=64`. The default is
  set from rework measurements.
* Partition `p` is always published by producer `p mod N`, a pure function fixed for the process.
  So every `(writerNodeId, partition)` has exactly one producer and ownership never overlaps.
* There is never a handoff. Every handoff obligation in `proxyCaptureProtocol.md` is discharged by
  construction.
* The single publisher actor remains the one lane owner and orders every submission.
* Heartbeats, traffic and capability probes for a partition share its producer, so submission
  order and librdkafka's per-partition delivery-report order are preserved.
* Every producer keeps idempotence, `acks=all`, `max.in.flight=5` and `linger.ms=0`. The
  effective-configuration read-back still gates startup.
* All producers report into one delivery channel; reports for different partitions may
  interleave, which the protocol permits.

---

## Open items still awaiting a ruling

| # | Item |
|---|---|
| D2 | Is bounded, lossless Kafka backpressure mandatory in the parity release? |
| D3 | Qualified deployment targets: Kubernetes only, or Kubernetes plus the older CDK/ECS path? |
| ~~D4~~ | **RESOLVED by OD-12** (producers; the publisher actor stays single). Confirm publisher sharding stays deferred until the single-owner implementation is measured. **Now measured** (Phase 1 review §5.5): a single librdkafka producer saturates its 5-request in-flight window because librdkafka sends one partition per request; the recommended fix is several producers with a fixed partition → producer mapping, keeping the single publisher actor. |
| D5 | `proxyCaptureProtocol.md` §7.3 specifies a controller-managed `COMPROMISED_AWAITING_CONTROLLER_ACK` state that blocks uncaptured forwarding until a controller durably acknowledges. Whether Java implements it, and whether Rust must, is unresolved. |
| D6 | Production Kubernetes sets no `terminationGracePeriodSeconds` (`setupCapture.ts:829-852`), so the default 30 s `SIGKILL` pre-empts the entire tested 60/240/270/300 s shutdown sequence. Does the orchestration get fixed, or does the Rust shutdown budget shrink to fit 30 s? |
| ~~D7~~ | **RESOLVED by OD-11.** HTTP/2 prior knowledge (`h2c`). The plan lists "HTTP/2 traffic is forwarded but suppressed from capture" as current behaviour. Java achieves it only by accident: Netty parses `PRI * HTTP/2.0`, the hardcoded `HTTP/2.*` pattern suppresses its capture, and the following `SM` line drives the decoder into `BAD_MESSAGE`, which then tunnels every later byte raw and uncaptured — the same path OD-1 removed. Measured on Rust (`/tmp/rust-proxy-qual/h2c_probe.py`): the connection is closed as `malformed_framing` and the origin receives 0 bytes. Options: (a) recognise the exact 24-byte preface at connection start and tunnel that connection uncaptured, restoring Java's observable behaviour and adding no capture; (b) keep refusing, since uncaptured HTTP/2 mutations would bypass capture-before-forward. HTTP/1.1 `Upgrade` → `101` (WebSocket, `h2c` upgrade) is unaffected: Rust tunnels it byte-exact after the `101`, as Java does. |
