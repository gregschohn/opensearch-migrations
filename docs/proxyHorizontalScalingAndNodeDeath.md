# Proxy Horizontal Scaling, Capture Liveness, and Proxy Completion

**Status:** standalone design contract
**Last revised:** 2026-09-10

This document defines horizontal scaling and failure behavior for capture proxies that write traffic
to Kafka for later replay. It covers the controller-less deployment. Managed-fleet terminal-failure
behavior and fresh-run recovery are specified separately in
[`proxyManagedFleetCaptureRecovery.md`](proxyManagedFleetCaptureRecovery.md).

The design deliberately separates three concerns:

1. Kafka group membership assigns partitions for new connections.
2. Exact open-connection manifests establish cycle boundaries for resetting incomplete
   per-connection HTTP accumulation.
3. The capture-before-forward contract prevents a strict-mode proxy from completing a source
   request that Kafka cannot later replay.

Group departure causes a rebalance among the remaining members. That is the complete role of
departure in this protocol. It is not replay evidence and does not authorize another proxy to
declare the departed proxy instance finished.

---

## 1. Goals and accepted boundaries

### 1.1 Required behavior

The system must:

- ensure that every request the proxy allowed to execute at the source has a complete acknowledged
  Kafka representation before an omission may break that connection interval;
- replay every complete request that the capture parser recognized and detached before its
  connection interval was reset;
- capture every mutating request completely before allowing that request to complete at the source
  in strict mode;
- preserve long-lived connections, including connections with long periods of no traffic;
- let the replayer reset incomplete per-connection HTTP accumulation after an exact manifest-cycle
  omission or terminal self `NoMoreWrites`;
- scale proxies horizontally without moving existing connections;
- keep capture and source forwarding ordered correctly during rebalance, shutdown, and Kafka
  failure; and
- alarm loudly when capture develops a gap.

### 1.2 Accepted behavior

The design accepts:

- a complete request may be captured even though it never reaches the source;
- an omitting manifest intentionally prevents the replayer from joining predecessor and successor
  observations into one request; the proxy-side removal barrier guarantees that such a
  cross-boundary sequence is not a source-executed request still owed to the target;
- best-effort bytes that were never source-authorized and become syntactically complete only by
  combining delayed predecessor data at or before an omitting cycle may be discarded by that
  omission;
- an incomplete accumulation from a hard-crashed proxy may remain retained when no later exact
  manifest or terminal self `NoMoreWrites` can settle it;
- hard process death may prevent a final proxy-completion record;
- pass-through mode may create a known interval in which source traffic is not replayable; and
- restoring strict capture after such an interval requires a new capture and replay run; and
- this redesign supports only source request handling that cannot mutate state before the complete
  HTTP request arrives. Streaming source handlers that can act on an incomplete body remain out of
  scope; and
- hardening mutating-request classification beyond the proxy's current HTTP-method predicate is
  deferred. Endpoints that mutate outside that predicate are a known limitation for this round.

The design does not require transactions, a controller, or a peer fencing protocol.

### 1.3 Central end-to-end invariant

For every request `Q` that can mutate source state:

```text
SourceApplied(Q)
    implies
Kafka has durably acknowledged the complete replay representation of Q
```

The converse is intentionally false. Kafka may contain a complete request that the proxy later
chooses not to forward.

The proxy enforces this invariant by withholding the final execution-enabling bytes of a mutating
request until its complete Kafka representation has been acknowledged. After that acknowledgement
and immediately before those source bytes are submitted, the proxy performs the capture-liveness
check in §5.

For this redesign, the invariant assumes that source execution is gated by receipt of the complete
HTTP request. Transfer-Encoding chunking does not violate that assumption by itself; a source
handler that applies effects while consuming an incomplete body does. Such streaming source
semantics are not supported in this round.

More generally, every captured source write requires the corresponding Kafka record to be
acknowledged and the local capture gate to remain open. The complete-request rule is the stronger
condition applied at the source execution boundary.

This ordering is what makes a manifest-cycle reset safe:

- if the source completed the request, Kafka already contains the complete request;
- if Kafka contains only an incomplete request, strict mode could not have completed it at the
  source;
- before a captured client connection is removed from the next manifest, every request that was
  allowed to execute at the source has a complete, acknowledged Kafka representation; and
- observations that race after that removal may begin a new per-connection HTTP accumulation, but
  they cannot describe a source-executed request still owed to the target.

This invariant does **not** by itself make elapsed-time expiration safe. Configured expiration is
disabled as commit authority in this design until the unresolved questions in §6.3 are answered.

---

## 2. Terminology and identities

### 2.1 `processId`

`processId` identifies one operating-system process for logs, metrics, and control-plane
diagnostics. It is generated at process start and is never reused.

### 2.2 `writerNodeId`

`writerNodeId` identifies one capture activation in Kafka traffic records, manifests, and
proxy-completion records. It is generated whenever a process begins a new capture activation and is
never reused, including by the same operating-system process after capture has been abandoned.

The existing wire-field name is retained for compatibility. In prose, this document says **proxy
instance X** rather than “writer.” A capture activation identity outlives any one group generation.

### 2.3 Captured client connection

Every accepted connection has a unique `connectionId`. The proxy records its chosen traffic
partition when the connection is accepted:

```text
connectionId -> trafficPartition
```

That mapping is immutable for the life of the connection.

### 2.4 Capture modes

- **`FAIL_AND_EXIT`** is strict capture. A capture-liveness failure prevents further source
  execution and causes the process to close connections and exit.
- **`AUTONOMOUS_PASS_THROUGH`** permits source forwarding after capture is abandoned. The process
  remains available, alarms continuously, and never resumes capture.

---

## 3. Kafka membership and horizontal scaling

### 3.1 Membership is routing authority only

The proxy group assigns every traffic partition to one member for accepting **new captured client
connections**. This document avoids the overloaded word “admission” for that decision. Membership
does not prove that a departed proxy has stopped all execution, and it does not
authorize peer `NoMoreWrites` records.

Each member publishes enough assignment metadata for every member to compute the same routing
table. The leader uses a custom cooperative assignor so partitions can move gradually.

The implementation must retain the existing minimum-capacity gate:

```text
ACTIVE member count >= minimumActiveProxyCount
```

When the gate is not satisfied, no member accepts new captured connections.

### 3.2 Member phases

Each process has one membership phase:

- **`PROBING`** — outside the consumer group while Kafka capability is tested;
- **`PROBATIONARY`** — in the group but not eligible to accept new captured client connections;
- **`ACTIVE`** — eligible to accept group-assigned new captured client connections;
- **`DRAINING`** — accepts no new connections and finishes existing connections in place; or
- **`CAPTURE_ABANDONED`** — permanently outside capture participation for this process.

`PROBATIONARY` members receive no new captured client connections. They become eligible only after
an assignment containing them as `ACTIVE` has completed. The transition is explicit:

1. the member joins with subscription metadata `PROBATIONARY` after §3.3 succeeds;
2. its first completed group assignment gives it no new-connection traffic ownership;
3. after processing that assignment and rechecking that its local capture gate remains open, the
   member advertises `ACTIVE` in its next subscription and requests one debounced, jittered
   rebalance if no other rebalance is already pending; and
4. only a later completed cooperative assignment may move partitions to it.

The previous owner retains a partition until cooperative revocation. The new owner waits for §5.3's
initial-manifest acknowledgement before accepting a connection. This may create a short
new-connection capacity pause for the transferred partition, but never overlapping ownership or
uncaptured forwarding. The phase transition is a capacity gate, not a death-detection or
replay-settlement protocol.

### 3.3 Startup capability probing

A process must not join the proxy group until it has demonstrated that its producer can complete an
acknowledged Kafka write.

While in `PROBING`, it:

1. refreshes metadata for the traffic topic;
2. chooses one representative traffic partition for every distinct current leader broker;
3. emits a semantically inert `CaptureCapabilityProbe` to each representative partition;
4. waits for all probe acknowledgements; and
5. refreshes metadata again before joining the group as `PROBATIONARY`.

The replayer recognizes `CaptureCapabilityProbe` as a semantically inert record. It never creates
replay work, opens a connection, or affects manifest-cycle interpretation.

This probe qualifies current producer reachability. It does not guarantee future availability.
Normal runtime failures are handled by §5 and §7.

### 3.4 Scale up

When member B joins:

1. B completes §3.3 and joins as `PROBATIONARY`.
2. B completes the no-traffic probationary assignment and advertises `ACTIVE` as specified in §3.2.
3. A later cooperative assignment transfers new-connection ownership.
4. Existing member A stops accepting new connections only when its revocation completes.
5. A continues writing existing connections to their original partitions.
6. B accepts new connections for its assigned partitions only after the
   initial-manifest rule in §5.3 is satisfied.
7. A emits manifests for its remaining connections until each drains.
8. After A's final connection on a transferred partition drains, A emits and acknowledges a final
   empty manifest. It does not emit `NoMoreWrites`, because ordinary assignment may later return
   that partition to A.

Existing connections never migrate between processes or partitions.

### 3.5 Scale down

A planned process retirement first becomes `DRAINING`:

1. it stops accepting new connections;
2. another member becomes the member accepting new captured client connections for its partitions;
3. existing connections continue in place;
4. the draining proxy instance continues traffic and manifest publication;
5. after all connections drain, it follows §4.3 and emits terminal self `NoMoreWrites` for each
   partition the proxy instance used; and
6. it leaves the group and exits after those records are acknowledged, or after the operator's
   drain deadline expires.

If the deadline expires, the process applies the configured strict or pass-through behavior in §7.

### 3.6 Reacquisition

A live proxy instance may later reacquire a partition that it previously drained through an empty
manifest.
It acknowledges a new initial manifest before accepting the first new connection.

A proxy instance must never reacquire a partition after emitting `NoMoreWrites` for that
`(writerNodeId, partition)`. Reuse requires a fresh capture activation and `writerNodeId`. In normal scaling,
`NoMoreWrites` is therefore reserved for permanent proxy-partition retirement, typically process
shutdown or permanent capture abandonment after all related connections have been disconnected.

### 3.7 Kafka 4 and assignment-protocol rollout

The initial implementation uses Kafka's Classic group protocol because the required custom
cooperative assignment metadata and phase transitions are under application control.

If a future group protocol cannot interoperate with the deployed assignor or subscription metadata,
the fleet must not perform an in-place mixed-protocol rollout. The rollout creates a capture
boundary:

1. stop the old capture run;
2. retire or restart any pass-through processes;
3. use a distinct capture run identity and, when required for clean operational separation, a new
   traffic topic;
4. restore strict capture;
5. take a fresh source snapshot; and
6. start a new replay run at the new run's configured offsets.

Traffic from before and after an uncaptured or protocol-incompatible interval must not be presented
as one complete replay run.

---

## 4. Capture records and publisher ordering

### 4.1 Record types

The traffic topic carries:

```text
TrafficRecord {
    writerNodeId
    connectionId
    partition
    manifestCycle
    observations[] {
        connectionObservationSequence
        captureTimestamp
        ...
    }
    ...
}

OpenConnectionManifestChunk {
    writerNodeId
    partition
    manifestCycle
    chunkIndex
    chunkCount
    emittedAt
    connectionIds[]
}

NoMoreWrites {
    writerNodeId
    partition
    emitterNodeId
}

CaptureCapabilityProbe {
    writerNodeId
    probeId
}
```

For `NoMoreWrites`, the emitter must be the same proxy activation:

```text
emitterNodeId == writerNodeId
```

Peer-emitted `NoMoreWrites` is invalid. A consumer may ignore and alarm on such a record; it must
not use it to settle replay work or install a terminal cutoff.

The emitter field may remain in the wire format for compatibility.

`manifestCycle` is a monotonically increasing value scoped to one
`(writerNodeId, partition)`. It is the logical boundary described in §5.1. It is not a wall-clock
timestamp, Kafka offset, group generation, or Kafka producer epoch.

Every reconstruction-relevant observation logically carries the cycle that was current when the
proxy accepted it. Serialization may hoist the value to the `TrafficRecord` envelope only because a
record is required to be homogeneous in `(writerNodeId, partition, connectionId, manifestCycle)`.
A batch is flushed before accepting an observation with a different value. One Kafka record
therefore never mixes predecessor and successor obligations across a manifest boundary.

`connectionObservationSequence` is a monotonically increasing connection-local value assigned by
the connection's single event-loop owner. It validates the order of reconstruction-relevant
observations for that captured client connection. It is not shared across connections and needs no
contended atomic increment.

Reconstruction-relevant records for one connection enter the producer through a connection-local
submission chain in contiguous sequence order. Other connections and manifest chunks may interleave
freely; there is no global packet lock. Producer idempotence preserves the order in which that
connection's records enter `send`. A missing sequence, repeated sequence with different content, or
regression is a protocol failure that closes capture or halts replay; the replayer does not wait
indefinitely for a lower sequence. Truly incidental packet diagnostics that cannot affect HTTP
request completion, expiration, source forwarding, or target replay are outside this sequence and
may be best effort.

### 4.2 Publisher ordering and acknowledgement

Kafka producer ordering is still required within one proxy instance and partition, especially for
terminal self `NoMoreWrites`. It is **not** used to force every ordinary packet, connection
lifecycle change, and manifest callback through one global proxy lock. The logical ordering needed
for manifest omission comes from `manifestCycle`, not from assuming that Kafka append order matches
concurrent proxy execution order.

Broker append order for records submitted in order must remain stable across retries. The producer
therefore requires:

```text
enable.idempotence = true
acks = all
max.in.flight.requests.per.connection <= 5
```

These are correctness settings, not tuning defaults. User configuration must not weaken them, and
startup must validate the effective producer configuration before the process enters `ACTIVE`.
Failure to establish ordering-preserving settings fails capture closed.

The lane distinguishes:

- **accepted** — submission entered the lane;
- **submitted** — the producer API accepted it;
- **acknowledged** — Kafka acknowledged it; and
- **failed or ambiguous** — success cannot be established.

Only acknowledgement advances the proxy's durable publication state.

For permanent proxy-partition retirement, the publisher has a one-way local state:

```text
OPEN -> RETIRING -> RETIRED
```

- `OPEN` accepts captured connection traffic and ordinary periodic manifests.
- `RETIRING` rejects new connection traffic and ordinary manifests. Only the retirement barrier
  may submit the final empty manifest and terminal self `NoMoreWrites`.
- `RETIRED` rejects every submission.

Publisher initialization and shutdown are completion-gated lifecycle operations. The composition
root owns any newly created producer and executor until publisher construction succeeds. A failed
initialization closes and joins both before returning failure; successful construction transfers
their sole lifecycle ownership to `CaptureKafkaPublisher`. Publisher shutdown quiesces callbacks,
closes the producer, terminates its executor, and completes only after those postconditions hold.
There is no optional no-op lifecycle callback or default method that can omit this cleanup.

### 4.3 Terminal self `NoMoreWrites` barrier

Self `NoMoreWrites` permanently retires one `(writerNodeId, partition)`. It is a compact mechanism
for promptly releasing incomplete state when that proxy instance shuts down cleanly.

Its Kafka offset K is the terminal cutoff. Records from that activation and partition below K
remain valid even if a scanner discovers `NoMoreWrites` first. Any traffic or manifest record above
K violates the protocol. Only duplicate self `NoMoreWrites` is idempotent.

Before emitting it for partition P, the proxy:

1. atomically moves P's publisher from `OPEN` to `RETIRING`, permanently preventing new captured
   client connections, connection-originated traffic submission, and ordinary manifest submission
   for P under this `writerNodeId`;
2. asynchronously quiesces the periodic manifest publisher: no future run may start, and any
   callback already running must either have entered the lane before `RETIRING` or finish without
   submitting. The quiescence completion gate must not block a Netty event loop;
3. disconnects every Netty connection capable of producing traffic for P and waits for every
   connection-teardown future to settle;
4. waits for every send previously accepted by P's publisher to succeed and treats any failed
   or ambiguous send as a capture failure under §7;
5. verifies that P's exact open-connection registry is empty;
6. emits and acknowledges a final empty manifest through the retirement barrier;
7. submits `NoMoreWrites` after that barrier; and
8. after its acknowledgement, moves the publisher to `RETIRED` before reporting completion.

The `RETIRING` transition and manifest-publisher quiescence ensure that no delayed periodic task can
submit an ordinary manifest after terminal `NoMoreWrites`. Multiple retirement requests share the
same idempotent completion gate.

The useful guarantee is:

```text
Every source-completable request from this proxy instance on P
has a complete acknowledged Kafka representation before NoMoreWrites.
```

Because emission waits for every related Netty connection and teardown future, no proxy thread
remains able to submit source or capture bytes for P after `NoMoreWrites`. The source service may
still finish processing a request that it received before disconnection, but that request's complete
Kafka representation is already before the proxy-completion record.

After acknowledgement, no code path may reopen capture submission for P under this
`writerNodeId`.

### 4.4 Hard failure

A killed, crashed, or indefinitely suspended process may emit no `NoMoreWrites`. That is expected.
The replayer can still use a later complete manifest-cycle omission if the proxy had published it
before failing. Otherwise the affected incomplete accumulation remains retained until an explicit,
separately designed recovery boundary exists. Group departure does not substitute for completion.

---

## 5. Exact manifests and the pre-forward liveness gate

### 5.1 Exact registry and the manifest-cycle boundary

For every proxy instance and partition, the proxy maintains an exact registry of **all** open captured
connections, including idle connections.

Each partition also has a cache-friendly atomic `manifestCycle` counter. Ordinary connection
activity reads that counter; it does not increment it. Once per periodic manifest cycle, the
manifest task increments it. The lifecycle operations that add or remove a connection and the
manifest task's copy-and-increment boundary are linearized with one small partition-local
coordination construct:

```text
prepareManifest(partition P):
    enter P's lifecycle boundary
    M = manifestCycle[P]
    S = exact copy of P's active captured connections
    manifestCycle[P] = M + 1
    leave P's lifecycle boundary
    publish complete manifest {cycle=M, connections=S}
```

Creating captured client connection C linearizes its addition to the exact registry **before** the
proxy accepts C's first reconstruction-relevant observation. Removal is a short protocol rather
than one callback:

1. atomically close C's one-way source-forward gate so no new source operation can be authorized;
2. wait until every source operation authorized before gate closure has a complete acknowledged
   Kafka representation; and
3. linearize C's registry removal under P's lifecycle boundary.

A manifest prepared while step 2 is waiting still lists C. Ordinary packet capture does not take
the lifecycle boundary and continues concurrently, but it cannot reopen source-forward
authorization after step 1. Preparing manifest M linearizes the exact-registry copy and counter
increment shown above.

Every reconstruction-relevant observation reads the current `manifestCycle[P]` when the proxy
accepts it and carries that value to Kafka. Kafka records may arrive in a different order from the
proxy-side lifecycle operations. The cycle value, rather than Kafka position alone, resolves the
race:

- if C was added before M's lifecycle boundary and not removed before that boundary, M lists C;
- if C was added after the boundary, its observations carry a cycle greater than M, so omission by
  M cannot reset the accumulation begun by those observations;
- if C was removed before M's boundary, M omits C;
- if C was removed after the boundary, M still lists C and a later manifest may omit it.

The Kafka-order inversions are therefore explicit:

1. An open observation accepted before M may arrive after M. If C remains active at M's boundary,
   M lists C and the replayer initializes or continues C normally.
2. C may instead be removed before M's boundary. Then M omits C, and an observation accepted before
   the boundary remains delayed predecessor data because its cycle is at most M.
3. An open observation accepted after M's boundary may arrive before M. It carries a cycle greater
   than M, so M cannot reset the per-connection HTTP accumulation that observation began.

An observation accepted before M's boundary carries a cycle at most M even if Kafka appends it
after M. If M omits C, that observation is delayed predecessor data covered by M's reset; it does
not start a successor accumulation merely because its Kafka offset is later. Only an observation
whose `manifestCycle` is greater than M begins or contributes to the successor.

The close/removal rule is deliberately narrower than “all packets are globally ordered.” The proxy
may still emit a late observation after removal because packet handling is concurrent. Even if that
observation looks reconstruction-relevant in isolation, the proxy guarantees that it cannot
describe a request that was sent to the source but is still owed to the target. A late observation
with a cycle greater than the omitting manifest may begin a fresh per-connection HTTP accumulation;
a delayed observation from the omitted cycle or earlier is settled as predecessor data.

This is the complete linearization requirement. The implementation must not claim or require a
stop-the-world ordering across ordinary packets.

### 5.2 Manifest completeness

Every assigned or still-draining proxy instance emits a complete manifest per partition at
`manifestInterval`, with a default operational target of 30 seconds.

Manifest publication is sequenced per partition. A later cycle is not prepared or submitted until
the previous cycle's complete chunk set has been acknowledged. Overlapping periodic callbacks
coalesce into the next run; they cannot submit M+1 before M. A failed or ambiguous manifest closes
the capture gate, so no later cycle is authoritative.

The manifest may be chunked. All chunks share one `manifestCycle`, `chunkCount`, `writerNodeId`,
partition, and diagnostic emission time. Chunks are numbered exactly `0..chunkCount-1`.

The replayer uses a manifest only after every chunk is present exactly once. A partial,
inconsistent, or duplicate-index manifest is non-authoritative and produces an alarm.

An empty complete manifest is a positive statement that the proxy instance currently has no open
connections on that partition.

### 5.3 Initial manifest

Before accepting the first captured client connection for a newly writable partition, the proxy
must acknowledge an initial complete manifest for that partition. This establishes a cycle
baseline. It does not require all later packet capture to share a publisher lock with the manifest.

### 5.4 Proxy-local acknowledged-manifest freshness

The proxy tracks, per partition:

```text
lastAcknowledgedManifestSubmissionMonotonicTime[P]
```

“The last manifest went out” always means Kafka acknowledged the entire manifest. Enqueue,
serialization, or producer submission is insufficient.

The stored value is the proxy's local monotonic time from when that now-acknowledged manifest was
created or submitted, not the later acknowledgement-callback time. Kafka acknowledgement makes the
value valid; it does not make an old manifest fresh again.

The manifest publisher treats a failed or incomplete manifest as a capture failure. It does not
advance `lastAcknowledgedManifestSubmissionMonotonicTime`.

This value exists only for the proxy's local capture-health check. It is never written into Kafka,
never compared with Kafka broker time, and never used by the replayer.

### 5.5 Pre-forward liveness check

For every source-forwarding step:

1. capture and acknowledge the record that represents the bytes to be forwarded;
2. for execution-enabling bytes of a mutating request, also ensure that the request's complete
   replay representation has been acknowledged;
3. immediately before submitting the corresponding source bytes, read the current capture state
   and `lastAcknowledgedManifestSubmissionMonotonicTime` for the request's partition;
4. permit source submission only if capture remains healthy and the manifest age is within
   `proxyManifestStaleTimeout`; and
5. otherwise apply §7.

The check and source-forwarding decision must share a one-way local capture gate. Once the gate
closes, no thread may newly pass the check. This avoids a simple check-then-transition race inside
the process.

No local check can fence a thread that has already entered an uninterruptible source syscall.
That does not violate the central invariant: the request was completely acknowledged to Kafka
before that syscall was allowed.

### 5.6 Timing relationship

The proxy still requires:

```text
manifestInterval < proxyManifestStaleTimeout
```

with margin for scheduling, Kafka publication, acknowledgement latency, and ordinary transient
retries. Both are proxy-local durations measured with a monotonic clock.

There is currently no sound inequality to a replayer expiration timeout because elapsed-time
expiration is not enabled as commit authority by this design.

---

## 6. Replayer interpretation

### 6.1 Complete requests

A request that the capture parser already recognized as complete and detached from its connection
before an applicable reset is processed normally regardless of later manifests, `NoMoreWrites`, or
proxy health. Bytes that would become syntactically complete only by joining delayed predecessor
observations after an omitting manifest are not such a detached complete request.

Reset and terminal completion apply only to incomplete per-connection HTTP accumulation. They must
never discard an already detached complete request merely because its proxy later became silent.

### 6.2 Manifest-cycle reset and terminal completion

For captured client connection C emitted by proxy instance X on partition P:

| Observation | Replayer action |
|---|---|
| normal connection-close record | Finish C through the ordinary close path |
| complete manifest M from X on P lists C | Preserve C's current per-connection HTTP accumulation |
| complete manifest M omits C and C's current accumulation began in cycle M or earlier | Expire and reset that current incomplete per-connection HTTP accumulation |
| complete manifest M omits C but C's current accumulation began after M | Do nothing; M precedes that accumulation's proxy-side lifecycle interval |
| observation for C with cycle at most M that is consumed after omitting M | Settle it as delayed predecessor data covered by M; do not reopen C from Kafka offset order alone |
| observation for C with cycle greater than M after a manifest reset | Accept it into a newly initialized successor per-connection HTTP accumulation; never join it to the pre-reset accumulation |
| terminal self `NoMoreWrites{X, P, X}` | Settle all incomplete state already known for X on P and permanently retire that proxy-partition identity |
| traffic or manifest from X on P at a Kafka offset above terminal self-completion cutoff K | Halt and alarm on a protocol violation; do not treat it as a new activation |
| duplicate self `NoMoreWrites{X, P, X}` | Treat idempotently as the same terminal retirement |
| peer `NoMoreWrites{X, P, E}` where `E != X` | Ignore for settlement, alarm as invalid provenance |

Manifest omission is not a tombstone for C. It resets only the incomplete per-connection HTTP
accumulation whose proxy-side lifecycle interval ended no later than M. If observations for C later
appear with a cycle greater than M, the replayer starts from the same empty state used for a newly
observed connection. Delayed observations from cycle M or earlier remain predecessor data covered
by the reset. If a later complete manifest again omits C, that later incomplete accumulation may be
reset again.

Only self `NoMoreWrites` permanently retires `(writerNodeId, partition)`. A hard crash emits no such
record.

If scan-ahead discovers self `NoMoreWrites` before normal replay reaches its cutoff K, it may settle
only a blocker whose scan covered every intervening offset through K without finding the required
follow-up. Other accumulations wait until the replay cursor reaches K; then all lower records have
been processed and the remaining incomplete state may settle.

### 6.3 Configured expiration is not yet commit authority

The earlier design allowed elapsed Kafka broker time to expire incomplete state after both traffic
and listing manifests became silent. That rule is not currently sound enough to commit retained
Kafka records:

- committing an incomplete prefix can make a delayed suffix unrecoverable if that suffix later
  completes a captured request;
- Kafka `LogAppendTime` can jump forward with broker-clock error, causing premature expiration;
- a quiet partition provides no durable later broker-time observation at all; and
- safe treatment of delayed observations depends on the exact proxy source-forwarding and
  manifest-cycle guarantees, not elapsed time alone.

Therefore `--packet-timeout-seconds`, if retained, may produce diagnostics and alarms but must not
produce a commit-eligible terminal disposition. Incomplete state lacking a complete manifest-cycle
omission or terminal self `NoMoreWrites` remains commit-blocking.

Enabling bounded expiration requires a separate reviewed design that proves what happens to every
late suffix and defines protection from broker-clock jumps. It must also state whether the system
accepts data loss after an explicit run-abandonment boundary. This document makes no such policy
choice.

### 6.4 Long gaps

A connection with no traffic for hours remains represented because complete manifests continue to
list it. No application-idle timeout is inferred from that silence.

### 6.5 Timestamp domains

The protocol intentionally contains independent values with different meanings:

- `manifestCycle` is a per-proxy, per-partition logical sequence used only for the lifecycle rule in
  §5.1;
- `TrafficObservation.ts` is source event time for replay pacing;
- manifest `emittedAtMillis`, which remains diagnostic only;
- proxy-local monotonic time is used only for the capture-health gate in §5.4; and
- Kafka offsets order records within a partition but do not replace `manifestCycle` when concurrent
  proxy publication reorders a manifest and observation.

No conversion or comparison between these domains is part of manifest reset correctness.

### 6.6 Commit behavior

When a connection's incomplete source state is settled, all of its retained source records follow
the ordinary terminal-disposition and commit-accounting path. No special force-commit bypass is
introduced.

The replayer advances a partition's Kafka commit watermark only across a contiguous prefix of
`Commit` dispositions. A `Retain` disposition closes process-local contexts but deliberately keeps
its offset eligible for redelivery and therefore continues to block that watermark, as does
incomplete state that has not reached a sound terminal disposition.

---

## 7. Proxy failure behavior

### 7.1 Shared transition rule

Manifest staleness, traffic-producer failure, an ambiguous send, or any other loss of trustworthy
capture closes the local capture gate permanently for that process.

Capture never resumes under the same `writerNodeId`.

The process emits a high-severity alarm containing at least:

- `processId` and `writerNodeId`;
- mode;
- affected partitions;
- last acknowledged manifest submission age from the proxy's local monotonic clock;
- last acknowledged traffic offset when available;
- producer error or stale-gate reason; and
- whether source forwarding continued.

### 7.2 Strict mode: `FAIL_AND_EXIT`

After the gate closes:

- the request currently waiting at the pre-forward check is not sent to the source;
- no later request is allowed to execute at the source;
- acceptance of new captured client connections stops;
- existing connections are closed or terminated;
- the process begins immediate exit or restart;
- accepted Kafka sends are drained only while their outcome remains trustworthy; and
- terminal self `NoMoreWrites` is attempted only after every related Netty connection is
  disconnected and the §4.3 barrier succeeds.

Failure to emit self `NoMoreWrites` does not delay exit. The process crash is handled as a crash;
the replayer must not fabricate completion.

### 7.3 Pass-through mode: `AUTONOMOUS_PASS_THROUGH`

After the gate closes:

- acceptance of captured client connections and Kafka traffic submission remain permanently
  closed;
- existing and new source connections may continue without capture;
- connections do not need to be broken merely because replay cannot reconstruct them;
- the process emits a loud, persistent capture-gap alarm; and
- it does not emit terminal `NoMoreWrites` while any retained pass-through connection could still
  contain capture-side work from the retired proxy activation.

The process must not rejoin capture, resume manifests, or generate captured traffic under the same
or a new `writerNodeId`. Recovery requires retiring the process and starting a fresh strict-mode
process.

### 7.4 Process suspension

A process may be suspended after Kafka acknowledgement and before source submission. If it resumes:

- in strict mode, the pre-forward stale-manifest check prevents execution once the threshold is
  exceeded;
- if suspension occurred after the source-forwarding decision, the request was already completely
  captured and remains replayable; and
- in pass-through mode, the capture gap is an accepted and alarmed policy outcome.

This is the meaningful containment boundary without broker transactions or an external controller.

### 7.5 Membership observations

A missing group member, failed health check, or control-plane declaration of node death should
alarm operators and may trigger process replacement. It does not cause replay settlement and does
not cause another proxy to write completion on behalf of the missing proxy activation.

---

## 8. Recovery after a capture gap

Pass-through creates an interval whose source effects cannot be proven complete from Kafka. The
system must not silently splice later strict capture onto the previous replay run.

Recovery is:

1. alarm and record the start of the gap;
2. retire every process that entered pass-through;
3. restore a fully strict capture fleet with fresh process and capture-activation identities;
4. create a new managed capture session, trusted replay-boundary plan, and session-aware
   checkpoints, using either a complete same-topic reset package or a distinct immutable topic as
   that plan's Kafka boundary;
5. establish the source-specific barrier proving that failed-run operations can no longer complete;
6. take a fresh source snapshot appropriate to the replay workflow; and
7. start a new replay run from the trusted boundary.

For the current Kubernetes round, the controller marks the failed capture resource terminal and
does not restore capture or authorize a replacement snapshot within that resource. A fresh workflow
may proceed only after the managed-fleet addendum's Kafka-input-isolation, old-workload retirement,
and source-quiescence preconditions are satisfied. If that fresh workflow reuses the same Kafka
topic, its complete session-fenced reset package is required now. Automatic same-resource reset,
regrant, and coverage restoration remain future work.

---

## 9. Implementation deltas

| Area | Required change |
|---|---|
| Group assignor | Remove designated-witness graphs, peer visibility, and partition-footprint dissemination. Retain cooperative ownership for accepting new captured client connections and the member phases. |
| Startup | Add out-of-group capability probes to representative leader brokers before joining as `PROBATIONARY`. |
| Routing | Persist immutable connection-to-partition choice; move only eligibility to accept new captured client connections during rebalance. |
| Registry | Maintain exact all-open connection sets and one atomic `manifestCycle` per proxy activation and partition. |
| Publisher | Stamp every reconstruction-relevant observation with its current `manifestCycle`; publish complete manifests with the cycle closed by their copy-and-increment boundary; preserve acknowledgement and terminal self-`NoMoreWrites` ordering without globally serializing packet capture. |
| Source forwarding | Enforce complete-capture acknowledgement and the local pre-forward stale-manifest gate for mutating requests. |
| Failure mode | Make capture abandonment irreversible per process; implement strict exit and permanent pass-through transitions. |
| Record validation | Accept terminal self `NoMoreWrites` idempotently; reject peer completion; halt and alarm on later traffic or manifests from a retired proxy-partition identity. |
| Replayer | Reset only the predecessor per-connection HTTP accumulation covered by a complete omitting manifest. Settle delayed observations from that cycle or earlier as predecessor data; accept only greater-cycle observations into a fresh successor accumulation. Keep elapsed-time expiration non-committing. |
| Observability | Replace witness and peer-cutoff metrics with manifest-cycle reset, manifest-age, capture-gate, retained-incomplete-state, and capture-gap metrics. |

### 9.1 Removed concepts

The standalone implementation no longer needs:

- designated witnesses;
- witness count or failure-domain placement;
- `confirmedPeerVisibility`;
- dynamic witness repair;
- peer-emitted `NoMoreWrites`;
- terminal peer cutoffs and zombie-record discard;
- partition-set dissemination for peer completion; or
- quiescence delays intended to make peer completion safer.

### 9.2 Required status

Each proxy should expose:

- process and capture-activation identity;
- membership phase and current new-connection assignment;
- capture mode and whether the capture gate is open;
- current connections by partition;
- proxy-local acknowledged-manifest submission age by partition;
- oldest unacknowledged publisher work;
- whether the process has permanently abandoned capture; and
- capture-gap alarm state.

The replayer should expose:

- incomplete per-connection HTTP accumulations by proxy activation and partition;
- latest complete `manifestCycle` and the Kafka offsets containing its chunks;
- terminal-disposition counts by normal close, manifest-cycle reset, and terminal self
  `NoMoreWrites`;
- retained state for which no sound terminal disposition exists;
- invalid peer-completion records; and
- the oldest commit-blocking state.

---

## 10. Integration issues to resolve during implementation

### 10.1 Manifest-cycle schema and atomicity

Before implementation is declared complete:

- every reconstruction-relevant observation carries `manifestCycle`;
- every manifest chunk carries that same field and no competing `manifestId` or
  `manifestSequence` is used for lifecycle semantics;
- `manifestCycle` is scoped to `(writerNodeId, partition)` and never reused by that capture
  activation;
- connection add, connection remove, and manifest copy-and-increment use the same partition-local
  lifecycle boundary;
- ordinary packet capture only reads the counter and is not serialized by that boundary;
- manifest completeness is validated before omission has any effect; and
- tests cover both Kafka-order inversions from §5.1.

An implementation may use a stronger local serialization mechanism if measurement justifies it,
but correctness claims and interfaces must depend only on the narrow boundary above.

### 10.2 Known limitation: streaming source execution

This redesign assumes that the source application cannot mutate state until it has received the
complete HTTP request. Under that assumption, withholding the final request bytes until complete
Kafka acknowledgement preserves §1.3.

The round does not add full-request buffering or support source handlers that apply effects while
streaming an incomplete body. Deployments with those semantics cannot claim the strict-mode
capture-before-forward guarantee for those endpoints.

HTTP chunked transfer encoding remains supported when it is only wire framing and the source still
waits for the complete request. Supporting true streaming execution later requires buffering or
spooling the complete mutating request, acknowledging its complete Kafka representation, checking
the capture gate, and only then releasing effect-causing bytes to the source.

### 10.3 Deferred hardening: mutating-request classification

The central invariant applies to every request that can mutate source state, but the current proxy
classifies requests through its existing HTTP-method predicate. Replacing that rule with a
source-specific policy, default-mutating classification, or explicit read-only allowlist is
deferred.

This is a known coverage limitation rather than a new guarantee. A future hardening round should
make unknown requests mutating by default and ensure that capture-suppression rules cannot exempt a
potentially mutating request from strict capture-before-forward.

### 10.4 Existing elapsed-time expiration code

The existing `--packet-timeout-seconds` path must not produce a commit-eligible disposition in this
protocol. It may remain temporarily for metrics or diagnostics while the implementation is
converged.

Any future proposal to enable it must first resolve every issue in §6.3 and receive design review.
A completed replayable request must never be culled because its connection stopped receiving
manifests.

### 10.5 Existing proxy-completion schema and consumers

Existing code may already accept `emitterNodeId != writerNodeId` and install a terminal cutoff.
Those paths must be removed or made non-authoritative. Compatibility parsing can remain, but peer
provenance must not settle state.

### 10.6 Managed-fleet addendum

The managed-fleet addendum is realigned with this protocol. Its current-round Kubernetes rule is
terminal: after bounded retry reaches a capture failure, the resource remains incomplete and the
workflow starts over. Every managed fresh run already requires a controller-issued session, trusted
replay-boundary plan, and session-aware checkpoints. The addendum documents future automatic
orchestration around:

- terminal self-only `NoMoreWrites`;
- exact manifest-cycle resets and explicit retention when neither reset nor self completion exists;
- the irreversible local capture gate;
- explicit new-run recovery after a capture gap;
- those session identities fencing delayed old records;
- separate old capture-activation retirement and Kubernetes traffic retirement; and
- immutable pre-grant replay start offsets—reset-derived for a same-topic run or sampled after setup
  for a distinct-topic run—that remain conservative across the new source snapshot.

---

## 11. Validation plan

### 11.1 Deterministic proxy tests

- A mutating request cannot submit execution-enabling source bytes before complete Kafka
  acknowledgement.
- The supported-source contract states that mutating handlers do not apply effects before the
  complete HTTP request arrives; true streaming source execution remains out of scope.
- Mutating-request classification retains the current HTTP-method predicate as a documented
  limitation; conservative default-mutating classification is deferred.
- A stale manifest after capture acknowledgement blocks strict-mode source forwarding.
- Closing the capture gate races safely with many source-forwarding threads.
- A source-forwarding operation allowed before gate closure has a complete Kafka representation.
- Failed or partial manifest publication does not refresh proxy-local acknowledged-manifest
  freshness.
- Publisher initialization failure closes its producer and executor; repeated failed starts leave
  no publisher threads behind.
- A connection added before manifest M's lifecycle boundary and still registered at that boundary
  is listed by M.
- Registry addition precedes acceptance of the connection's first reconstruction-relevant
  observation.
- A connection added after M's boundary stamps observations with a cycle greater than M.
- A connection removed before M's boundary is omitted by M; one removed after it remains listed
  until a later manifest.
- Removal atomically closes source-forward authorization and waits until every previously
  authorized source operation has a complete acknowledged Kafka representation.
- A manifest prepared while removal waits for those acknowledgements still lists the connection.
- An open observation accepted before M but appended after M continues normally when M lists the
  connection; if removal preceded M, it is instead predecessor data covered by M's omission.
- An open observation accepted after M but appended before M is protected from M's omission by its
  greater `manifestCycle`.
- A delayed observation with cycle M or earlier that is appended after omitting M is settled as
  predecessor data; only a greater cycle starts the successor.
- A traffic batch flushes at a manifest-cycle boundary and never creates one Kafka record containing
  both predecessor and successor observations.
- The connection-local submission chain preserves same-connection Kafka order, and
  `connectionObservationSequence` detects a gap, regression, or conflicting duplicate rather than
  asking the replayer to reorder records.
- Repeated periodic callbacks never submit manifest M+1 before M is completely acknowledged.
- Ordinary packets continue concurrently while add, remove, and manifest copy-and-increment are
  repeatedly raced.
- Removing a connection is impossible until every request allowed to execute at the source has a
  complete acknowledged Kafka representation.
- Terminal self `NoMoreWrites` waits for all related Netty connections to disconnect, an empty
  manifest to be acknowledged, every previously accepted send to succeed, and the periodic
  manifest publisher to become quiescent.
- A periodic manifest callback racing permanent retirement either enters the lane before
  `RETIRING` and is drained or exits without submission; it never publishes after terminal
  `NoMoreWrites`.
- Producer configuration cannot disable idempotence, weaken `acks=all`, or set
  `max.in.flight.requests.per.connection` above the ordering-preserving limit.
- Startup fails closed when the effective producer settings cannot preserve same-partition lane
  order across retries.
- Pass-through never resumes capture.
- Strict mode stops new source execution and exits.

### 11.2 Manifest and replayer tests

- Complete manifests list active and idle connections.
- Chunk loss, conflicting metadata, and duplicate indexes make a manifest non-authoritative.
- A listing manifest preserves an idle connection's current per-connection HTTP accumulation.
- Manifest M omitting a connection resets only an accumulation begun in cycle M or earlier.
- A greater-cycle observation after reset starts from an empty per-connection HTTP state machine
  and is never joined to the pre-reset accumulation.
- A later manifest may reset that new incomplete accumulation again.
- Terminal self `NoMoreWrites` settles prior incomplete state and retires the proxy-partition.
- Scan-ahead discovery of self `NoMoreWrites` records its Kafka cutoff; traffic below the cutoff
  remains valid. Every traffic or manifest record above it violates the protocol; only duplicate
  self `NoMoreWrites` is idempotent.
- Later traffic or manifests after terminal self `NoMoreWrites` halt and alarm as a protocol
  violation; duplicate self completion is idempotent.
- Peer `NoMoreWrites` does not settle state or discard later traffic.
- Elapsed-time expiration never produces a commit-eligible disposition.
- `manifestCycle`, source event time, proxy monotonic time, Kafka offset, and Kafka timestamp remain
  separate domains.
- A complete request is replayed even when its connection's later incomplete accumulation is reset.
- Every terminal path settles its process-local accounting exactly once; only contiguous
  `Commit` dispositions advance Kafka's commit watermark.

### 11.3 Membership tests

- A process completes leader-broker probes before joining.
- `PROBATIONARY` members receive no new traffic.
- Scale-up moves eligibility to accept new captured client connections while old connections drain
  in place.
- Temporary assignment drain ends with an acknowledged empty manifest.
- Reacquisition after temporary drain begins only after a new initial manifest is acknowledged.
- Reacquisition after terminal `NoMoreWrites` requires a fresh process and `writerNodeId`.
- The active-capacity gate closes below `minimumActiveProxyCount`.
- Mixed incompatible group protocols are rejected as an in-place rollout.

### 11.4 Failure tests

- Kill a proxy with open idle and active connections and no later omission or self completion;
  verify the incomplete Kafka obligation remains retained and alarms.
- Suspend a proxy beyond the stale threshold and resume it; verify strict source execution is
  blocked after complete Kafka capture.
- Suspend it after source forwarding was allowed; verify the request was already completely
  captured.
- Fail Kafka during manifest publication in strict and pass-through modes.
- Create an ambiguous producer send and verify the capture gate closes.
- Enter pass-through, recover Kafka, and verify that the process still does not resume capture.

### 11.5 Acceptance criteria

The design is complete when:

1. every source-completable mutating request is completely acknowledged to Kafka first;
2. exact manifests preserve open idle connections;
3. manifest-cycle reset and terminal self completion release only the state they are authorized to
   settle;
4. no peer observation authorizes replay settlement;
5. ordinary partition reacquisition works after empty-manifest drain and is forbidden after
   terminal `NoMoreWrites`;
6. strict suspension recovery cannot create an uncaptured completed request;
7. pass-through creates a loud, explicit capture gap and never resumes capture;
8. manifest cycle, Kafka offset, source event time, and local monotonic time are kept as distinct
   domains; and
9. all terminal paths settle their local obligations through the normal accounting path, while
   retained offsets continue to block Kafka commit progress.

---

## 12. Decisions

- Kafka membership assigns new connections; group departure simply triggers rebalance and does not
  prove proxy death.
- Manifests list all open connections and close a precisely defined `manifestCycle`.
- Only connection add, connection remove, and manifest copy-and-increment are linearized; ordinary
  packet capture remains concurrent.
- Kafka append order may differ from proxy lifecycle order, so observations carry
  `manifestCycle`.
- A manifest omission resets only the current incomplete per-connection HTTP accumulation whose
  start cycle is not newer than that manifest.
- A greater-cycle observation may start a new accumulation for the same captured client connection.
- Manifest age advances only after acknowledgement of a complete manifest.
- Capture-before-forward is the primary completeness guarantee.
- Capture-before-forward in this round assumes non-streaming source execution; chunked wire framing
  is allowed only when the source waits for the complete request.
- Hardening mutating-request classification beyond the existing HTTP-method predicate is deferred.
- The proxy checks manifest freshness after Kafka acknowledgement and before source execution.
- `NoMoreWrites` is self-emitted, partition-scoped, ordered, and terminal for that
  proxy-partition identity.
- Kafka producer idempotence and ordering-preserving retry settings are mandatory and cannot be
  weakened by deployment configuration.
- Temporary partition drain uses an acknowledged empty manifest and permits later reacquisition.
- Hard crashes emit no completion. Without a later sound terminal fact, incomplete state remains
  retained.
- Long inactivity is safe because manifests continue to list the connection.
- Strict mode blocks source execution and exits after capture loss.
- Pass-through mode may continue connections uncaptured, alarms loudly, and never rejoins capture.
- Peer departure only causes group rebalance; it has no replay meaning.
- Recovery after a capture gap starts a new capture and replay run.
- In the current managed Kubernetes round, a terminal capture failure terminally fails the whole
  capture workflow; same-resource reset, regrant, and replacement-snapshot automation are deferred.
- Elapsed-time expiration is not commit authority until a separate design resolves late suffixes
  and broker-clock jumps.
- Proxy manifest freshness uses a local monotonic clock only for source capture health.
