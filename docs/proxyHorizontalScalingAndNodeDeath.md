# Proxy Horizontal Scaling and Node-Death Declarations

**Status: sketch, for review.** Nothing here is implemented.

It supersedes `replayer-expiration-hardening.md` §5.4.5 and §7's "A stable per-host nodeId"
rejection, and it closes the wall-clock backstop that both that document's §5.4 and
`replayerHardenedArchitectureDesign.md` §10.8 leave as their residual. It revisits
`replayerHardenedArchitectureDesign.md` §19.7 (how a proxy chooses its partitions).

It does not change the expiry *policy*, the omission *predicate*, or §10.8's decision to declare
all open connections rather than idle-only. What it changes is the set of admissible signals.
§2.1 states that split precisely, and reviewers should start there — "the absence proof is
unchanged" is true of the predicate and misleading about the system.

## 1. What problem this solves

Two problems that turned out to be the same problem.

**Scaling capture.** Today a proxy hashes its own `nodeId` to pick a starting partition and
claims `shardWidth` consecutive partitions from there (`PartitionRoutingPlan.forTopic`).
Ranges collide at random, growing the topic reshuffles every proxy's range, and there is no
way to add capture capacity deliberately.

**Proving a proxy is dead.** The absence-proof rule (`replayer-expiration-hardening.md` §5.4.1)
can prove an open connection is gone using two consecutive manifests from its own node. It
cannot prove anything about a node that stopped emitting manifests, because absence of
manifests is indistinguishable from a slow node. The residual is a wall-clock timeout — the
same inference that made PR #3207 unsafe.

The connection: if the fleet has a membership protocol, then a departure is an *event* a
surviving member observes, not a silence it has to interpret.

## 2. Core reframe

Two ideas carry the whole design.

**Death is declared per node, not per partition.** The assertion we need is "node X will
never write another record." That is a fact about X. Earlier drafts tied it to partition
ownership — a new owner's arrival declared the old owner's connections dead — which is
unsound, because ownership legitimately transfers while the previous owner is still alive
and still writing to those partitions (§4.2).

**Partition assignment is load balancing, not a lock.** The assignor decides where *newly
opened* connections go. It says nothing about existing ones and grants no exclusivity.
Multiple nodes writing one partition concurrently is normal and safe, because every
liveness conclusion is already keyed per `(nodeId, partition)`.

A consequence worth stating up front: **the assignor is not safety-critical.** A bug that
assigns one partition to two nodes costs evenness, not correctness. A partition assigned to
nobody is simply unused.

### 2.1 What changes and what doesn't

"The absence proof is unchanged" is true of the predicate and misleading about the system, so
the layers are separated here.

| Layer | Status |
|---|---|
| **Expiry policy** — what a confirmation authorizes (commit vs. retain, per the disposition matrix) | **Unchanged.** A declaration-based confirmation produces the same `ConfirmedAbsent` verdict and takes the same downstream path. |
| **Omission predicate** — two consecutive omitting manifests from one node on one partition, with the connection's last record before the first span | **Unchanged**, and still the mechanism for expiring a *live* node's connections. |
| **Admissible signals** | **Changed.** One is added, one is nearly removed. |
| **Discard rule** | **New.** Not an absence proof at all. |

The signal change is not that omission got stronger. Omission is untouched. Rather:

- A **positive declaration** (`Release` / `NodeDeath`) is added as an independent evidence type.
  It is strictly stronger than omission: one record instead of two manifests, and no reasoning
  about flush latency or weakly-consistent map iteration, because it is an assertion by or about
  the writer rather than an inference from what is missing.
- The **wall-clock backstop** stops being the general answer for a node that went quiet and
  narrows to the single case in §6.2 — the last proxy crashing with no survivor to observe it.
  It does not disappear.

The **discard rule** is genuinely new policy and deserves its own line rather than riding along
under "absence proofs are unchanged": records arriving from a declared-dead node after its
declaration offset are treated as inert. That is what makes a zombie's late writes harmless to
the commit decision, and it is also the direct cause of the completeness gap in §6.1 — the
traffic reached the source and is then deliberately dropped. Nothing in the absence-proof
framework implies it.

So: the policy is constant, the predicate is constant, the evidence set is solidified, and one
new rule is introduced whose cost is stated in §6.1.

## 3. Protocol

### 3.1 Static setup

- One topic, `M` partitions, provisioned for the **peak** fleet size. Kafka can add
  partitions but never remove them, and vacant capacity is free here, so oversizing `M` up
  front means scaling proxies never requires a Kafka change.
- One consumer group, members = proxies. Each proxy `subscribe()`s to the traffic topic,
  `pause()`es every assigned partition immediately, and `poll()`s about every 100ms. It
  consumes nothing; membership is the only thing being used, and pausing means `poll()`
  issues no fetches.
- `CooperativeStickyAssignor`, or a custom cooperative assignor if we later want block
  shapes. Cooperative is required, not an optimization: the eager protocol revokes every
  partition from every member on every rebalance.
- Each proxy publishes its `nodeId` in the subscription `userData` so members can name each
  other.

### 3.2 Per-proxy state

| State | Source | Used for |
|---|---|---|
| `nodeId` | fresh per process | attribution; never reused |
| `assignedPartitions` | assignor callbacks | where **new** connections may be placed |
| connection → partition | chosen at open, **immutable** | routing every subsequent record |
| partitions with live connections | `ProxyLivenessRegistry` key set | manifest targets, `Release` timing |

The connection→partition entry is chosen once and stored. Nothing recomputes it. This is
what makes adoption unable to remap anything: there is no hash whose inputs could shift.
`CaptureKafkaPublisher`'s existing consistency check becomes a map lookup rather than a
recomputation, keeping the assertion without the hazard.

### 3.3 Record types

Traffic records and manifest chunks as today, plus two declarations:

```
Release   { nodeId, partition }        // "I have no more connections on this partition"
NodeDeath { nodeId, observerNodeId }   // "this node will never write again, anywhere"
```

Neither carries a generation or epoch. `nodeId` is fresh per process, so both are
unambiguous forever without a counter.

### 3.4 Manifest emission

Every quantum, for each partition in `assignedPartitions ∪ partitionsWithLiveConnections`,
emit a manifest listing **all** of that node's open connections on that partition — not just the idle
ones. This inherits `replayerHardenedArchitectureDesign.md` §10.8's reasoning unchanged: "it was
active before the first snapshot" does not imply it will emit a record after that snapshot, so an
intentional omission combined with one concurrent-map miss can falsely prove death. It also means the
fan-out in §8's transient-spike note is over the full open set, not a filtered one.

- Assigned with zero connections → emit an **empty** manifest. That is a positive statement
  (§5.4.2), and it is what distinguishes an idle-but-alive node from a dead one.
- Not assigned and zero connections → emit `Release{nodeId, partition}` and stop touching
  that partition. Safe precisely because new connections only go to assigned partitions, so
  the count cannot rise again.
- Never `Release` a partition that is still assigned.

`Release` must land after the node's last traffic record on that partition. The idempotent
producer (default) guarantees per-partition ordering for a single producer, so "sent after"
is "lands after" — but the final records must be handed to the producer before the
`Release` is.

### 3.5 Lifecycles

**Startup.** Join the group, receive an assignment, begin accepting connections. No claim
record; adoption declares nothing.

**Scale-up.** B joins; the assignor moves some partitions from A to B. A's existing
connections on those partitions **drain in place** — A keeps writing their traffic records
and keeps emitting manifests listing them, on partitions B now owns for new connections.
A `Release`s each one as its last connection there closes. No client connection is
disturbed by a scale-up.

**Graceful shutdown.** Deregister from the load balancer → drain connections → `Release`
every partition → **leave the group last**. Leaving early turns an orderly departure into
an eviction.

**Cooperative revoke.** `onPartitionsRevoked` only removes partitions from
`assignedPartitions`. It does not need to drain, because drain-in-place is legal — so the
callback returns immediately and never risks blowing `max.poll.interval.ms`.

**Eviction or stall.** `onPartitionsLost`, or a local staleness gate (see §5), trips a
one-way latch: this process never writes again, and per an operator flag either exits or
degrades to pass-through. A surviving member writes `NodeDeath`.

**Crash.** No `Release`. A surviving member observes the membership change and writes
`NodeDeath{X}` to all `M` partitions. Every member computes departures from its own
membership view and any that notice may write; duplicate `NodeDeath` records are idempotent,
so redundancy is harmless.

## 4. Replayer rules

The replayer needs **no** group membership access, no admin API, and no write permission.
Everything is in-band and read-only.

### 4.1 Settling

| Observation | Conclusion |
|---|---|
| `Release{X, P}` at offset O | every connection of X on P is dead as of O |
| `NodeDeath{X}` at offset O on P | every connection of X on P is dead as of O |
| two consecutive manifests from X on P omitting C, after C's last record | C is dead (§5.4.1, unchanged) |

Records arriving after a `Release`/`NodeDeath` offset from that node on that partition are
**discarded** and counted. That counter is the detection mechanism for the completeness gap
in §6.1.

### 4.2 Why adoption proves nothing

Stated explicitly because an earlier draft got it wrong. With 120 partitions and one proxy
A, A owns all 120 and has connections spread across them. B joins and takes 60. A still
has live connections on those 60 and must keep writing there. So "B is now the owner of P"
cannot imply "A's connections on P are finished." Only A can say that (`Release`), or a
survivor observing A's departure (`NodeDeath`).

Two nodes' manifests interleaving on one partition is already handled: the scanner keys
streams by `(nodeId, partition, routingPlanId)`, so A's connections can only be proven dead
by A's own manifests, and B's stream is a different key.

## 5. Local zombie containment

Broker-side fencing (`transactional.id` + producer epoch) would make a stale node's writes
*fail* rather than land-and-be-discarded. We are deliberately not doing that, because it
puts `read_committed` visibility latency and transaction-commit round trips on the whole
capture path to close a sub-second window, and the commit decision is already protected by
§4.1's discard rule. Documented as the upgrade if the notification gap proves to matter.

Instead, locally:

- **One-way latch** on eviction or staleness. Never clearable; requires a restart.
- **Staleness gate on the capture path.** A volatile `lastSuccessfulPollNanos`, read on the
  Netty thread; refuse to hand bytes to the producer if it is staler than budget. This
  closes the window from the near side — records are never created rather than created and
  discarded.
- **Bounded `delivery.timeout.ms`**, so batches cannot sit in the accumulator through a
  stall and flush after the handover.

A self-imposed watchdog is legitimate *here* and not for declaring others dead, because the
failure direction is safe: not firing leaves you no worse off, and firing spuriously
recycles a healthy proxy, which costs availability rather than correctness.

## 6. Failure audit

### 6.1 Capture completeness in the zombie window — detected by metric presence

A stalled node's records land after its `NodeDeath` offset and are discarded. That traffic
reached the source but was never durably captured, so the target diverges. No protocol at
this layer can fix it, because the source already applied the mutation.

What determines the cost is the operator flag:

- **fail-closed** — the proxy fails the client request, so the source never applies it and
  there is nothing to diverge.
- **fail-open** — the proxy passes traffic through uncaptured, deliberately accepting
  divergence during proxy failures.

So the flag is availability versus **migration fidelity**, not availability versus
strictness. That framing belongs in customer-facing docs.

Detected by the discarded-record counter being non-zero.

### 6.2 The last proxy crashing — detected by metric absence

No survivor, so no `NodeDeath`. Its connections are never settled, retain forever, and pin
the commit head. A fleet with zero proxies has no incoming traffic, so an operator-visible
stall is a defensible default, with a configurable abandon-after-N for unattended runs.

Detected by "commit head not advancing AND no `NodeDeath` for the node holding it" — a clean
alarm rather than an ambiguous one, because an idle-but-alive node still emits empty
manifests.

### 6.3 Not separate holes

- **Evicted-but-healthy node** writing during the discovery window: same discarded-record
  counter as §6.1. Not a distinct case.
- **Assignor bugs**: benign, per §2.

## 7. Code deltas

| Change | Where |
|---|---|
| Remove the chunk-contiguity requirement | `KafkaLivenessScanner.java:508` — `(nextChunkIndex > 0 && offset != lastOffset + 1)`. **Mandatory**, not optional: with two nodes writing one partition, manifest chunks are always interleaved with traffic, so no snapshot would ever reassemble. Only implementable because reassembly now survives across reads. |
| Record the partition per connection instead of hashing | replaces `PartitionRoutingPlan.partitionFor`; the registry already stores it |
| Delete level-1 routing (nodeId hash → shard start) | `PartitionRoutingPlan.forTopic`; `selectedPartitions` becomes the assignment |
| Drop `topicPartitionCount` from the plan digest, or drop `routingPlanId` outright | `PartitionRoutingPlan.makePlanId`. The mapping is fully determined by the stored per-connection partition, so the guard collapses to "a connection's partition stamp never changes", which `KafkaTrafficCaptureSource.java:648` already checks. |
| Group membership client | new, in the proxy: subscribe, pause, poll, callbacks, `userData` |
| `Release` / `NodeDeath` records | `TrafficCaptureStream.proto`; emitted by `CaptureKafkaPublisher` |
| Settle-on-declaration evidence | new `ScanEvidence` variant; `KafkaLivenessScanner` |
| Discard superseded records + counter | `KafkaTrafficCaptureSource` |
| Latch, staleness gate, failure-mode flag | `CaptureProxy`, `KafkaCaptureFactory` |

## 8. Out of scope for this round

- Broker-side fencing via `transactional.id` (§5).
- A custom block assignor. `CooperativeStickyAssignor` is sufficient now that vacancy is
  not needed for correctness.
- Static membership (`group.instance.id`). Would avoid rebalances on planned restarts, but
  needs a stable identity from outside the process, which is the deployment coupling we are
  avoiding.
- Deliberate expiry of over-old connections, to dampen the post-scale-up manifest fan-out
  faster.

## 9. Open questions

1. If every prior member dies simultaneously, nobody writes `NodeDeath` and §6.2's backstop
   is the only recovery. Acceptable, or worth a persisted membership hint?
2. Manifest fan-out doubles transiently after a scale-up, decaying as drained connections
   close and bounded by the request timeout. Worth a cap?
3. Does the replayer need to distinguish `Release` from `NodeDeath`? They settle
   identically; separate types are currently only for diagnostics.
4. `session.timeout.ms` sets how fast a crash is noticed. Lower is faster detection and more
   spurious rebalances under GC pressure. Starting value?
