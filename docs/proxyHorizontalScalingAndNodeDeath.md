# Proxy Horizontal Scaling and Writer Completion

**Status: implemented foundations, membership protocol under revision.** Exact full manifests,
ordered publishing, strict control-record timestamps, interleaved chunk reconstruction,
`NoMoreWrites`, writer-completion settlement, peer fencing, group membership, and the local
capture write gate exist. The current membership implementation does not yet implement the
`JOINING`/`READY`/`ACTIVE` admission protocol, pairwise witnesses, or versioned write footprints
specified below. Those are required before the scaling implementation is considered complete.

Terminology in this document is deliberately specific:

- An **open-connection manifest** is the chunked record of one writer's currently open
  connections on one partition.
- A **writer-completion record** is `NoMoreWrites`: one writer has no more records covered by
  that record on one partition.
- The node that writes a `NoMoreWrites` record is its **emitter**. It may be the writer itself or
  a peer that witnessed the writer leave the group.
- Bare terms such as "declaration" and "snapshot" are avoided because they do not identify which
  record or state view is meant. Existing code names that use those words are migration targets,
  not preferred terminology.

The earlier "node death" framing was also misleading: the protocol does not determine whether a
process died. Every writer-completion record is scoped to one `(nodeId, partition)` pair and says
only "no further covered records from this writer here" — see §3.3 for why partition scope is
forced rather than incidental. The filename remains stable to avoid churning cross-references.

It supersedes `replayer-expiration-hardening.md` §5.4.5 and §7's "A stable per-host nodeId"
rejection, and it closes the dead-proxy residual that both that document's §5.4 and
`replayerHardenedArchitectureDesign.md` §10.8 leave open. It revisits
`replayerHardenedArchitectureDesign.md` §19.7 (how a proxy chooses its partitions).

It does not change the expiry *policy* or §10.8's decision to list all open connections rather
than idle-only. It updates the omission predicate to match the exact-registry proof: one complete
manifest copied after the connection's last record is sufficient. What the remaining scaling work
changes is the set of admissible positive signals.

## 1. What problem this solves

Two problems that turned out to be the same problem.

**Scaling capture.** Today a proxy hashes its own `nodeId` to pick a starting partition and
claims `shardWidth` consecutive partitions from there (`PartitionRoutingPlan.forTopic`).
Ranges collide at random, growing the topic reshuffles every proxy's range, and there is no
way to add capture capacity deliberately.

**Proving a connection is gone.** The absence-proof rule
(`replayerHardenedArchitectureDesign.md` §10.8) can prove an open connection is gone using one
complete, exact manifest from its own node after the connection's last record. It
cannot prove anything about a node that stopped emitting manifests, because absence of
manifests is indistinguishable from a slow node.

Be precise about what that residual currently is, because it is *not* a timeout. A wall-clock
commit is already rejected (`replayer-expiration-hardening.md` §7) and already banned
(`replayerHardenedArchitectureDesign.md` §10.5's "Wall-clock age → **Never**" row and §21's
non-goal "any path by which wall-clock timeout code can commit Kafka"). What the shipped design
does instead is:

- **Retain and halt loudly** (§10.5, "No proxy snapshots arriving → **Never** → Retain; halt
  loudly if it blocks progress"). Correct, but it converts a dead proxy into an operator page.
- **Emit unresolved-head metrics and diagnostics.** Operators may let the run wait indefinitely or
  terminate it in this premature terminal state. Neither choice advances the commit.

The connection: if the fleet has a membership protocol, then a departure is an *event* a
surviving member observes, not a silence it has to interpret. A surviving member can record that
event as `NoMoreWrites`. If no member survives, the protocol deliberately has no automatic
settlement path; a fresh replacement process cannot retroactively speak for an old `nodeId`.

## 2. Core reframe

Two ideas carry the whole design.

**Departure is observed per node and materialized per partition.** The fact a survivor observes
is "node X left this group view." Earlier drafts tied writer completion to partition ownership —
a new owner's arrival finished the old owner's connections — which is unsound because ownership
legitimately transfers while the previous owner is still alive and writing to those partitions
(§4.4). The fact is node-scoped, but the ordered Kafka evidence remains partition-scoped.

**Partition assignment is load balancing, not a lock.** The assignor decides where *newly
opened* connections go. It says nothing about existing ones and grants no exclusivity.
Multiple nodes writing one partition concurrently is normal and safe, because every
liveness conclusion is already keyed per `(nodeId, partition)`.

A consequence worth stating up front: **partition balance is not safety-critical.** Uneven
distribution costs throughput headroom, never correctness. Admission-state handling, membership
metadata validation, footprint confirmation, and withholding assignments from non-ACTIVE members
*are* recovery-critical. This distinction matters because unevenness itself is not only a failure
case — the design produces it deliberately and routinely:

- A drain after a scale-up has A still writing to partitions B now owns, by design (§3.5).
- The first proxy in a fleet holds all M partitions until others join.
- Connection-to-partition hashing is uneven at small connection counts, and long-lived
  connections keep it that way.

Since none of those are distinguishable from a misassignment by their effect on the log, the
protocol cannot depend on evenness and does not. A partition assigned to nobody is simply
unused; a partition assigned to two nodes is the drain case, which is already legal.

### 2.1 What changes and what doesn't

The layers are separated here because omission and writer completion authorize the same downstream
disposition through different structural facts.

| Layer | Status |
|---|---|
| **Expiry policy** — what a confirmation authorizes (commit vs. retain, per the disposition matrix) | **Unchanged.** A writer-completion confirmation produces the same `ConfirmedAbsent` verdict and takes the same downstream path. |
| **Omission predicate** — the latest complete exact manifest from one node on one partition after the connection's last record omits it | **Implemented.** One manifest is sufficient because registry copy and final-record acknowledgement are linearized. |
| **Admissible signals** | **Strengthened.** One added, one deleted outright. |
| **Post-completion violation rule** | **New.** Traffic after peer completion is fatal and retained, never silently discarded. |

The evidence set gets strictly better in three ways:

- A positive **writer-completion record** (`NoMoreWrites`, §3.3) is added as an independent
  evidence type.
  It covers writer completion rather than one connection: one record can settle every prior
  connection for that writer/partition, because it is an assertion by or about the writer
  rather than an inference from what is missing.
- **Omission's scope of necessity shrinks.** A node holds a partition until every connection it
  placed there has closed, and its `NoMoreWrites` is ordered after the last of those records — so every
  *terminal* and *handover* case is now settled by a statement. Omission is left covering only
  the connections of a node that is alive and still assigned, which is the case where the node is
  demonstrably able to speak for itself.
- The **dead-proxy residual narrows but does not disappear.** A surviving member can write
  `NoMoreWrites` after observing departure. If every member is gone, no process can produce that
  ordered fact, so unresolved work remains retained. §6.2 makes that accepted terminal behavior
  explicit instead of adding a second, out-of-band proof mechanism.

The **post-completion violation rule** is separate policy. A peer emits `NoMoreWrites` only after
the hard producer-quiescence bound in §5, so later traffic from that writer/partition is forbidden.
If the replayer nevertheless observes it, the source protocol has been violated: retain, halt
loudly, and do not acknowledge that later record. Never turn it into an inert record and commit
past it. A self-emitted record does not install this terminal fence because ordinary scale-down may
return the partition to the same process (§4.3).

So: the policy is constant, the omission predicate is strengthened around the exact registry, the
evidence set gains writer completion, and post-completion traffic becomes a fatal protocol
violation rather than a lossy cleanup path.

## 3. Protocol

### 3.1 Static setup

- One topic, `M` partitions, provisioned for the **peak** fleet size. Kafka can add
  partitions but never remove them, and vacant capacity is free here, so oversizing `M` up
  front means scaling proxies never requires a Kafka change.
- One consumer group, members = proxies. Each proxy `subscribe()`s to the traffic topic,
  `pause()`es every assigned partition immediately, and `poll()`s about every 100ms. It
  consumes nothing; membership is the only thing being used, and pausing means `poll()`
  issues no fetches.
- Membership eligibility requires writer-completion capability. Before subscribing, a process
  must initialize its producer, resolve the traffic topic partitions, and prove locally that it can
  emit `NoMoreWrites` to every partition. `JOINING` means "not admitted for client capture," not
  "partially initialized."
- That `poll()` loop runs on **its own thread**, never shared with capture work. Otherwise producer
  backpressure on the capture path could stall heartbeats and get the proxy evicted, so capture
  slowness would masquerade as proxy death — and eviction trips the §3.5 latch, which is not
  recoverable in-process.
- A **custom cooperative assignor** implements admission states and preserves existing active
  assignments while new members are on probation. Cooperative transfer is required, not an
  optimization: an eager protocol revokes every partition from every member on every rebalance.
- There is no coordination topic. All membership state travels in Kafka group subscription and
  assignment `userData`.
- Two independent startup controls are configured:
  - `minimumActiveProxyCount` counts the candidate itself. It is a clean fleet-start barrier: a
    cold group can promote a cohort only when that cohort plus existing active members reaches
    this count.
  - `requiredPeerWitnesses` excludes the candidate. It states how many other live members must
    mature as witnesses before the candidate may become active. With `N` mature witnesses, the
    writer can fail together with up to `N-1` of those witnesses while still leaving one survivor
    that observed it. Therefore configure
    `requiredPeerWitnesses = toleratedPeerWitnessFailures + 1`.
    Zero is allowed only as an explicit singleton/availability-only mode that accepts
    total-fleet-loss retention; it is not hardened multi-proxy scaling.
- `peerWitnessMaturityPeriod` applies independently to each candidate/witness pair. It delays
  admission only; it never authorizes replay settlement or interprets peer silence as completion.
- Both settings are admission controls. A later fall below the configured witness coverage emits
  a persistent degraded-redundancy metric but does not demote an already active process or
  interrupt existing connections. During that degraded interval, recovery from a subsequent writer
  failure is no longer guaranteed; unresolved replay work may retain as in total-fleet loss. This
  is an explicit operational capacity shortfall, not a silent continuation of the configured
  failure-tolerance guarantee.
- A cold group of `N` processes can make progress only when
  `N >= max(minimumActiveProxyCount, requiredPeerWitnesses + 1)`. Deployment validation and
  metrics expose configurations or replica counts that cannot satisfy that condition.
- A process that trips its one-way write latch or loses the ability to emit writer-completion
  records stops advertising acknowledgements and leaves the group. It cannot remain ACTIVE or
  count as a witness. Re-entry requires a fresh process and `nodeId`.

### 3.2 Per-proxy state

| State | Source | Used for |
|---|---|---|
| `nodeId` | fresh per process | attribution; never reused |
| admission phase | local state plus leader assignment | `JOINING`, `READY`, or `ACTIVE` |
| raw Kafka assignments | assignor callbacks | partitions proposed by group assignment |
| eligible traffic assignments | footprint owner | partitions visible to connection routing and source forwarding |
| connection → partition | chosen at open, **immutable** | routing every subsequent record |
| partitions with live connections | `ProxyLivenessRegistry` key set | manifest targets, `NoMoreWrites` timing |
| footprint tuple | serialized footprint owner, echoed by group leader | `(nodeId, revision, canonicalPartitionDigest)` plus its partition set |
| peer witness acknowledgements | subscription/assignment `userData` | proves which peers observed this member and exact footprint tuple |
| pending peer-completion work | ordered publisher lane | departed writer/partition records retained until Kafka acknowledgement |

The connection→partition entry is chosen once and stored. Nothing recomputes it. This is
what makes adoption unable to remap anything: there is no hash whose inputs could shift.
`CaptureKafkaPublisher`'s existing consistency check becomes a map lookup rather than a
recomputation, keeping the assertion without the hazard.

#### 3.2.1 Membership metadata

Every subscription advertises a versioned structure containing:

- `nodeId`;
- advertised admission phase: `JOINING`, `READY`, or `ACTIVE`;
- the currently advertised footprint tuple and canonical partition set, or `UNKNOWN`;
- for each member observed in the previous assignment table, the exact
  `(nodeId, footprintRevision, canonicalFootprintDigest)` tuple this member observed;
- the exact peer node ids and footprint tuples whose pairwise relationships it currently considers
  mature. ACTIVE writers continue tracking this state so replacement peers can restore witness
  coverage and authorize later footprint expansions.

The group leader places the complete member table into every assignment's `userData`. Each row
contains the member's advertised phase, leader-effective phase, exact footprint tuple and set,
mature-witness claim, and peer-observation acknowledgements. The leader serializes one immutable
canonical table encoding for the group and distributes that same encoding to every member. Reusing
one revision for different partition sets, a digest mismatch, malformed or duplicate node ids,
oversized metadata, or any internally inconsistent acknowledgement fails capture closed.

Group metadata propagates only during rebalances. Receiving an assignment table is not itself
proof that every peer received it. A peer becomes a witness only after that peer's later
subscription echoes the candidate's `nodeId`; the same echo mechanism acknowledges a candidate's
exact footprint tuple.

The leader never trusts an advertised READY phase or mature-witness claim by itself. For each
writer, it intersects the writer's claimed mature witnesses with the current rebalance's
subscriptions. A witness is currently valid only when it is a distinct completion-capable current
member and its current subscription acknowledges the writer and required footprint tuple.
Candidates use that validated set for promotion; ACTIVE writers use it for coverage reporting and
footprint expansion.

A survivor uses a bounded footprint for departure cleanup only when it observed the writer's exact
write-authorizing tuple through a continuous installed assignment-table sequence. An assignment
gap, membership loss/rejoin, missing writer row, invalid tuple, or uncertain continuity changes that
writer's footprint to UNKNOWN and therefore every partition.

Quiet groups must still advance the protocol. Maturity expiry, transition to READY, activation
debounce expiry, and an unacknowledged footprint expansion each schedule a coalesced
`enforceRebalance()` with jitter. Only the membership thread invokes it. Redundant requests are
coalesced, with at most one in-flight enforcement for one proposed metadata tuple.

#### 3.2.2 Admission phases and pairwise witness maturity

Pre-activation readiness is revocable; ACTIVE is monotonic for one process:

```
JOINING <-> READY -> ACTIVE
```

- **`JOINING`** participates fully in group membership and peer observation but receives no
  traffic assignments and admits no capture connections. Therefore a scaling probation never
  removes capacity from existing active members.
- A candidate starts one independent maturity interval for each peer whose subscription echoes
  the candidate's presence. The relationship remains maturing while both node ids remain in the
  group and the peer continues acknowledging the candidate. An unrelated member joining or
  leaving does not reset it. Losing that peer resets only that pair.
- **`READY`** means at least `requiredPeerWitnesses` peer relationships have matured and those
  peers acknowledge the candidate's current initial footprint tuple. READY members still receive
  no traffic assignments. Losing a required current witness, an acknowledgement, or the initial
  footprint confirmation returns the effective phase to JOINING.
- Advertised READY is a claim, not promotion authority. The leader revalidates the exact current
  witnesses as described in §3.2.1.
- When the first currently eligible READY candidate appears, the leader starts one short activation
  debounce deadline. Additional eligible candidates join that cohort without extending the
  deadline; candidates that lose eligibility are removed. At the deadline, the leader promotes all
  currently eligible cohort members if existing ACTIVE members plus that cohort reaches
  `minimumActiveProxyCount`; otherwise it promotes none. Leader replacement may restart the
  debounce but cannot bypass witness validation.
- **`ACTIVE`** receives traffic assignments and may admit new capture connections. ACTIVE never
  returns to READY or JOINING in-process. Eviction or local staleness trips the one-way write
  latch in §5 and requires process replacement.

There is deliberately no global "membership has been stable for N seconds" timer. Pairwise
maturity is a filter on protocol evidence, not the evidence itself. The peer's subscription echo
proves observation; a local monotonic timer rejects transient relationships. Timer expiry is
provisional and schedules a rebalance. Promotion occurs only when that subsequent rebalance
revalidates the pair against current subscriptions. Pair timers are neither persisted nor
transferred between processes. A later join does not erase evidence already accumulated between an
existing candidate and witness.

`minimumActiveProxyCount` is evaluated at the leader's promotion decision. It is a startup barrier,
not a maintained quorum and not proof that every promoted member has already installed its
assignment. For each ACTIVE writer, automatic peer completion remains conditional on at least one
completion-capable peer retaining an acknowledgement of that writer and applicable footprint. Zero
remaining coverage raises a distinct alarm and suspends that availability guarantee while
preserving retain/no-wrong-commit safety.

#### 3.2.3 Conservative write footprint

One serialized **footprint owner** processes raw assignment changes, connection registration and
removal, accepted/enqueued/in-flight publisher work, Kafka acknowledgements, and self
`NoMoreWrites` acknowledgements. Other threads post typed commands; membership metadata consumes an
immutable value produced by this owner. No membership thread samples independently changing maps.

The conservative footprint is the set of partitions on which the process can still produce an
ordered record:

```
eligible traffic assignments
∪ partitions with live connections
∪ partitions with pending final writes or self NoMoreWrites records
```

The initial footprint is `UNKNOWN`, which survivors interpret as every topic partition. The first
implementation must advertise the full topic before READY and obtain the required peer
acknowledgements. This lets the activation rebalance assign any partition without creating a second
capacity gap. Later implementations may advertise a smaller prospective set only if the leader
reserves assignments without revoking them from existing ACTIVE members until confirmation.

The owner tracks two exact sets:

- `latestAdvertisedFootprint`: the set in the most recent subscription metadata;
- `writeAuthorizedFootprint`: the exact tuple echoed by the leader and acknowledged by enough
  current mature witnesses.

Each partition moves through explicit owner states:

```
NOT_COVERED
  -> EXPANSION_PENDING
  -> WRITE_AUTHORIZED
  -> SHRINK_ELIGIBLE
  -> SHRINK_ADVERTISED
  -> NOT_COVERED
```

New-connection routing uses
`rawKafkaAssignments ∩ latestAdvertisedFootprint ∩ writeAuthorizedFootprint` and only while ACTIVE.
Existing connections use their stored partition, which must remain in both footprint sets until
their final work settles. Consequently:

- **Expansion precedes admission and source forwarding.** A newly assigned partition is not visible
  to connection routing, registration, capture, or request forwarding until a larger exact
  footprint tuple is leader-echoed and acknowledged by `requiredPeerWitnesses`. Blocking only
  `KafkaProducer.send` is too late because the source mutation may already have occurred.
- **An advertised shrink is immediately binding.** Once subscription metadata excludes a
  partition, that partition is removed from new routing and write admission even if the prior
  authorized set still included it. If it is reassigned, a new expansion revision must complete
  before use.
- **Shrink follows Kafka acknowledgement.** A partition may be removed only after it is no longer
  eligible for new traffic, has zero live connections, every accepted/enqueued/in-flight traffic
  write succeeded, and its self `NoMoreWrites`, when required, succeeded. A failed write retains
  coverage and trips the capture write gate.
- An unadvertised pending shrink may be cancelled if assignment, connection, or pending work
  returns. Immediately before advertising, the owner revalidates every removal. After advertising,
  re-adding a partition is an expansion with a new revision.
- Safe shrinkage is intentionally lazy. The five-minute deadline starts with the oldest pending
  eligible removal; later removals do not reset it. Natural rebalances may carry the reduction.
  Otherwise the membership thread calls `enforceRebalance()` with jitter around the attempt, not
  around the five-minute boundary. If the call overlaps another rebalance and the exact tuple is
  still unconfirmed afterward, re-arm it.
- Pending shrink state clears only when assignment metadata echoes the complete
  `(nodeId, revision, digest)` tuple. Peers that observed the new smaller set may use it
  immediately because shrink preconditions were already satisfied; peers with the old superset
  merely emit extra writer-completion records.

A stale superset causes extra `NoMoreWrites` records and is safe. A stale subset can miss a
partition the writer used and is unsafe. That asymmetry is why expansions block admission,
advertised reductions immediately disable writing, and reductions wait for a single serialized
proof of quiescence before advertisement.

### 3.3 Record types

Traffic records and manifest chunks as today, plus one writer-completion record:

```
NoMoreWrites { writerNodeId, partition, emitterNodeId }
    // "writerNodeId has no more covered records on partition, as of this offset"
    // emitterNodeId == writerNodeId -> self completion; future reassignment remains legal (§4.3)
    // emitterNodeId != writerNodeId -> peer observed writer departure; terminal for that nodeId
```

The current protobuf fields are named `nodeId` and `declaredBy`; implementation should rename the
source-level accessors to the precise terms above while retaining wire-field numbers.
`emitterNodeId` is load-bearing, not diagnostic: it selects whether §4.1's terminal writer fence
applies.
A self-emitted record settles the past without condemning future reassignment. A peer-emitted
record is terminal for that `writerNodeId`, because its purpose is to bound a process that may
still be running (§4.2).

Earlier drafts had two types, `Release` and a fleet-wide `NodeDeath`. That was a confusing
split, because a node-scoped judgment cannot be consumed as one:

**A replayer reads each partition independently, so a fleet-wide fact has to be materialized on
every partition where it is needed.** There is no cross-partition inference — the reader of
partition P settles P's connections from P's records alone. So "X left the group" is not a single
fleet-wide record the replayer can use. Survivors materialize one `NoMoreWrites` on each partition
in X's last advertised write footprint (§3.5). UNKNOWN footprint means every partition.

Every `NoMoreWrites` therefore has fixed scope: one writer, one partition, and records preceding
its offset. Only the emitter varies. There is deliberately no out-of-band replayer query that can
settle all partitions at once: without a Kafka position it cannot prove which historical traffic
it covers.

The record carries no generation or epoch. `nodeId` is fresh per process, so a writer-completion
record is unambiguous forever. `writeFootprintRevision` exists only in group metadata to confirm
which conservative partition set peers observed; it is not traffic identity and is never used by
the replayer.

### 3.4 Manifest emission

Every quantum, for each partition in
`eligibleTrafficAssignments ∪ partitionsWithLiveConnections`,
emit a manifest listing **all** of that node's open connections on that partition — not just the idle
ones. This inherits `replayerHardenedArchitectureDesign.md` §10.8's reasoning unchanged: "it was
active before the first manifest" does not imply it will emit a record after that manifest.

The registry is exact and this is load-bearing, not defense in depth. Registration linearizes before
the first traffic submission, a manifest copies the registry at one linearization point, and removal
linearizes only after Kafka acknowledges the final traffic record. Therefore a connection is present
in every manifest copied during its writable lifetime and absent only after its complete record set
is durably ordered before that manifest. One complete omission is enough; a second omission would
not repair an early-removal bug because both copies could occur during the same invalid gap.

Listing all open connections remains the simpler protocol even with an exact registry. It avoids an
activity classifier, makes an empty manifest unambiguous, and allows the replayer to compare the
latest complete manifest directly rather than combine traffic recency with completion-record
filtering.
Replacing the synchronized registry with weakly consistent iteration would invalidate the proof and
is not permitted.

Each manifest declares `chunkCount`; chunks are numbered exactly `0..chunkCount-1`. Every chunk in
one manifest carries one shared `emittedAtMillis`, and that value strictly increases per
`(nodeId, partition)` even if the proxy clock stalls or moves backward. Timestamps are diagnostic;
manifest completeness and authority come from chunk indexes and Kafka offsets.

Listing the full open set also means the fan-out in §8's transient-spike note is over all open
connections, not a filtered subset.

- Assigned with zero connections → emit an **empty** manifest. That is a positive statement
  and distinguishes an idle-but-alive node from a departed one.
- Not assigned and zero connections → emit `NoMoreWrites{nodeId, partition, nodeId}` and stop
  touching that partition after Kafka acknowledges the record. The record covers connections that
  existed when it was written; it is not a promise never to write there again. The partition can
  legitimately return on a later rebalance (§4.3). Keep the partition in the write footprint until
  acknowledgement, then queue the five-minute footprint reduction.
- Never emit self `NoMoreWrites` for a partition that is still assigned.

A self-emitted `NoMoreWrites` must land after the node's last traffic record on that partition.
The idempotent producer (default) guarantees per-partition ordering for a single producer, so
"sent after" is "lands after" — but the final records must be handed to the producer before the
completion record is.

### 3.5 Lifecycles

**Startup.** Initialize writer-completion capability, then join as `JOINING` with UNKNOWN followed
by a full-topic initial footprint. Participate in group polls but receive no traffic assignments.
Accumulate pairwise witness maturity, advertise `READY`, and wait for leader cohort promotion.
Begin accepting connections only after becoming `ACTIVE` and after every eligible assignment is
covered by both advertised and write-authorized footprints. No traffic-log coordination record is
added.

**Scale-up.** B joins as `JOINING`; A keeps all traffic assignments during B's witness probation.
After B becomes READY, the leader promotes a debounced cohort and cooperatively transfers some
traffic assignments. A's existing connections on transferred partitions **drain in place** — A
keeps writing their traffic records and manifests while B owns those partitions for new
connections. A emits self `NoMoreWrites` as its last connection on each partition closes. No
client connection is disturbed by scale-up, and probation does not reduce existing capacity.

**Scale-down.** B, C, and D leave; the assignor gives their partitions back to A, including
partitions A previously emitted self `NoMoreWrites` for during scale-up. Before writing there, A
expands and confirms its footprint if necessary. It then resumes new admissions. No traffic-log
record announces the reacquisition; see §4.3, which is also why §4.1 ignores self-emitted records.

**Graceful shutdown.** Deregister from the load balancer → drain connections → emit self
`NoMoreWrites` on every partition in the acknowledged write footprint → wait for Kafka
acknowledgements → irreversibly close the process write gate → **leave the group last**. Once leave
begins, that `nodeId` may never rejoin or write again. Leaving early turns an orderly departure
into an eviction. Peer-emitted duplicates after departure remain harmless.

**Cooperative revoke.** `onPartitionsRevoked` only removes partitions from
traffic assignments. It does not drain existing connections because drain-in-place is legal.
The revoked partition remains in the footprint until all live connections and final writes settle.
The callback returns immediately and never risks blowing `max.poll.interval.ms`.

**Eviction or stall.** `onPartitionsLost`, or a local staleness gate (see §5), trips a
one-way latch: this process never writes again, and per an operator flag either exits or
degrades to pass-through. If the process can still execute, it stops witness acknowledgements and
leaves the group. Survivors begin peer-completion processing.

**Crash.** There is no self completion. Every continuous survivor that observes X disappear waits
the bounded producer-quiescence interval in §5, then emits `NoMoreWrites{X, P, self}` for each
partition P in X's last safely observed footprint. Missing row, invalid continuity, or UNKNOWN
footprint falls back to every topic partition. Each survivor retains every
`(departedNodeId, partition)` obligation until Kafka acknowledges it; asynchronous failure keeps the
obligation pending and trips the local write gate. Duplicate retries are allowed.

For a footprint tuple acknowledged by W witnesses, automatic peer completion remains guaranteed
while at least one of those W remains completion-capable. The writer may fail together with at most
W-1 witnesses. Loss of all W exhausts the configured failure boundary.

Every member computes departures from its own membership view, and any that notice may write.
Duplicates are idempotent: the rule in §4.1 is keyed on the earliest such offset per (X, P), and
a second copy at a later offset concludes nothing new. There is deliberately no single designated
emitter. Making only the group leader write would require durable handoff state to survive leader
loss between observing departure and publishing all required records; group `userData` does not
provide that handoff. If every observer dies before any acknowledgement, that is the explicit
multi-node failure boundary and unresolved replay work remains retained.

## 4. Replayer rules

The replayer needs **no** group membership access, no admin API, and no write permission.
Everything is in-band and read-only.

### 4.1 Settling

A writer-completion record settles connections that came before it. A peer-emitted record also
establishes a terminal writer fence because §5's producer-quiescence bound has elapsed. That fence
does not authorize discarding later traffic; later traffic is a fatal contradiction of the
protocol.

| Observation | Conclusion |
|---|---|
| `NoMoreWrites{X, P, *}` at offset O | every connection of X on P **already known at O** is dead as of O |
| latest complete exact manifest from X on P omits C, after C's last record | C is dead (§3.4) |

"Already known at O" is the load-bearing qualifier. The completion record settles connections
whose last record precedes O; it is not a standing rule about the pair (X, P) that also condemns
connections X opens later. A connection first seen after O was never covered by the record, so it
accumulates normally.

Both signals are usable while the replayer is lagging because both are positioned in the same Kafka
partition as the traffic they describe. There is no out-of-band membership verdict: a present-time
membership answer has no offset and therefore cannot prove which historical records it covers.

**Post-completion violation rule.** A record arriving from X on P after
`NoMoreWrites{X, P, emitter}` where `emitter != X` is retained and halts replay for that partition.
Emit a fatal protocol-violation metric and diagnostic; never acknowledge it as ignored traffic.
Records after a self-emitted `NoMoreWrites` remain valid and are processed normally.

### 4.2 Why only peer completion is terminal

Because self-emitted and peer-emitted writer-completion records say different things.

A **self-emitted** record is a node's statement about its own past: "I have no connections here as
of now." It is not a promise about the future, and §4.3 shows it cannot be one. A node that later
writes to that partition again has not contradicted anything.

A **peer-emitted** record describes a process that departed membership. Its purpose is to finish a
writer that cannot speak for itself, so it must be terminal. It is emitted only after §5 proves that
no accepted old-writer submission can still land:

- If the node truly died, a replacement is a new process with a fresh `nodeId` (§3.3), so it is a
  different subject and the record does not touch it.
- If the node was merely evicted or stalled, §3.5's one-way latch has already stopped it from
  accepting new work, the final producer-submission gate rejects stale queued work, and the bounded
  producer-delivery interval has elapsed.

Note also that a peer-emitted record about X never impedes another node: records are keyed per
`(nodeId, partition)`, so `NoMoreWrites{B, P, A}` says nothing about A's own writes to P.

Making self completion terminal would be wrong. The records after it are real traffic from ordinary
reassignment; treating them as a violation would turn scale-down into a permanent outage.

### 4.3 Scale-down: a node reacquiring a partition it released

This is the case that forces the split above, and an earlier draft got it wrong.

Take one proxy A holding all `M` partitions. Scale up to four: the assignor gives A a quarter, and A
drains and emits `NoMoreWrites{A, P, A}` on the other three quarters (§3.4). Now scale back
down to A alone. **Kafka hands every partition back to A** — same process, same `nodeId`, and no
mechanism by which A could decline. A immediately begins writing new connections to partitions it
has already emitted completion records for.

Under a terminal fence that ignored the emitter, all of that new traffic would halt replay:
ordinary scale-down would become a permanent failure on the majority of partitions.

Three properties make reacquisition safe, and none requires a new record or an epoch:

- **The terminal fence does not apply**, because A emitted the record about itself (§4.2).
- **Nothing was settled wrongly.** A emits self completion only with zero live connections, so
  at offset O there was genuinely nothing left on P to settle. The connections A opens after
  reacquiring have fresh connection ids and first records after O, so §4.1's "already known at O"
  qualifier excludes them.
- **The completion record was never falsified.** It is scoped to a partition and to the
  connections existing when it was written; it claims nothing about future assignment. Reading it
  as "A will never write here again" is the misreading that caused the bug.

**Repeated cycles are fine and need no counter.** Scaling up and down repeatedly produces an
alternating sequence on P — traffic, self completion, traffic, self completion — and each
`NoMoreWrites` settles only what preceded it. There is no state to reconcile across cycles, which
is why no era, epoch, or generation field is needed on the record or on traffic records. This is
the concrete payoff of `nodeId` being fresh per process.

The one thing reacquisition must not do is resurrect a condemned node. If a peer emitted
`NoMoreWrites{A, P, C}`, A must never write again at all — enforced by the latch in §3.5, not by
anything in the reacquisition path.

### 4.4 Why adoption proves nothing

Stated explicitly because an earlier draft got it wrong. With 120 partitions and one proxy
A, A owns all 120 and has connections spread across them. B joins and takes 60. A still
has live connections on those 60 and must keep writing there. So "B is now the owner of P"
cannot imply "A's connections on P are finished." Only A can say that, or a survivor observing
A's departure — which is exactly the two provenances of `NoMoreWrites`.

Two nodes' manifests interleaving on one partition is already handled: the scanner keys
streams by `(nodeId, partition, routingPlanId)`, so A's connections can only be proven dead
by A's own manifests, and B's stream is a different key.

## 5. Local zombie containment

Broker-side fencing (`transactional.id` + producer epoch) would make a stale node's writes
fail at the broker. We are not requiring transactions in this revision because the same
no-later-append property can be established with bounded local submission and delivery contracts.
Broker fencing remains the upgrade if those bounds cannot be enforced.

Instead, locally:

- **One-way latch** on eviction or staleness. Never clearable; requires a restart.
- **Staleness gate at admission and final producer submission.** A monotonic
  `lastSuccessfulPollNanos` is checked before source forwarding and again on the ordered publisher
  lane immediately before `KafkaProducer.send`. Queued work cannot bypass a membership stall by
  reaching the producer later.
- **Bounded `delivery.timeout.ms`.** Every accepted producer send succeeds or fails within that
  configured upper bound. Failure trips capture closed.
- **Bounded peer-completion delay.** After observing departure, survivors wait at least
  `membershipWriteStalenessBudget + producerDeliveryTimeout + safetyMargin` before sending peer
  `NoMoreWrites`. The old process's last successful poll necessarily preceded coordinator
  departure; after the staleness budget it can submit nothing new, and after one additional
  delivery timeout nothing it previously submitted can still land.

This is not a global membership-stability timer and is not a replayer expiry clock. Membership
departure is the event; the delay is a hard producer-quiescence bound before materializing that
event in the traffic log. If any component cannot enforce its bound, survivors must not emit peer
completion and the fleet fails closed.

## 6. Failure audit

### 6.1 Traffic after peer completion — fatal protocol violation

Under §5's contracts, no old-writer record can land after peer `NoMoreWrites`. Observing one means a
staleness gate, publisher queue bound, producer delivery bound, or peer delay was violated. The
replayer retains that record, stops advancing the partition, emits a fatal metric and diagnostic,
and terminates the run. It does not ignore or commit the record.

What determines the cost is the operator flag:

- **fail-closed** — the proxy fails the client request, so the source never applies it and
  there is nothing to diverge.
- **fail-open** — the proxy passes traffic through uncaptured, deliberately accepting
  divergence during proxy failures.

So the flag is availability versus **migration fidelity**, not availability versus
strictness. That framing belongs in customer-facing docs.

Fail-open can still create uncaptured source mutations after a local capture failure; that is an
explicit operator-selected fidelity loss. It never authorizes the replayer to discard a captured
Kafka record.

### 6.2 The last proxy crashing — retained, never inferred

If every member disappears, no survivor can write `NoMoreWrites`. The protocol deliberately has no
second settlement mechanism for that case. A group-membership query describes the present but has no
Kafka offset, so it cannot prove which historical records it covers. A clock has the same defect and
is also inadmissible.

The replayer retains the unresolved commit head and either waits indefinitely or terminates the
current run in a premature terminal state. It emits:

- unresolved connection and commit-head counts;
- retained record and byte counts;
- the blocked partition, `nodeId`, and head offset;
- the last authoritative manifest offset, plus source and manifest timestamps for diagnostics only.

If the original process was stalled rather than dead and resumes under the same `nodeId`, later
traffic and manifests resolve the blocker normally. If a survivor observed the departure, its
ordered `NoMoreWrites` record resolves the blocker. A fresh replacement process has a new `nodeId`
and cannot retroactively prove what happened to its predecessor. Permanent retention after a real
total fleet crash is therefore an accepted availability outcome.

When capture resumes behind a retained head, hard record and byte budgets prevent unbounded local
growth. Reaching a budget pauses or terminates replay without advancing the commit. Every failure
direction remains "stall or stop," never "commit wrongly."

### 6.3 Not separate holes

- **Evicted-but-healthy node**: bounded by §5. A post-completion write is the same fatal protocol
  violation as §6.1, not a separate lossy path.
- **Uneven or duplicated assignment**, however caused: benign, per §2.

## 7. Code deltas

| Change | Where |
|---|---|
| Remove the chunk-contiguity requirement | **Implemented.** `KafkaLivenessScanner` groups chunks by node, partition, plan, and sequence; unrelated records may appear between chunk offsets. |
| Use one complete exact omission | **Implemented.** `AbsenceProof.LivenessOmission` carries one complete manifest span after the connection's last record; scanner tests cover presence/omission replacement and malformed manifests. |
| Allocate one strict timestamp per manifest | **Implemented.** `CaptureKafkaPublisher` copies one strictly increasing timestamp to every chunk. It remains diagnostic only. |
| Record the partition per connection instead of hashing | replaces `PartitionRoutingPlan.partitionFor`; the registry already stores it |
| Delete level-1 routing (nodeId hash → shard start) | `PartitionRoutingPlan.forTopic`; `selectedPartitions` becomes the assignment |
| Drop `topicPartitionCount` from the plan digest, or drop `routingPlanId` outright | `PartitionRoutingPlan.makePlanId`. The mapping is fully determined by the stored per-connection partition, so the guard collapses to "a connection's partition stamp never changes", which `KafkaTrafficCaptureSource.java:648` already checks. |
| Group membership client | **Foundation implemented, protocol revision required.** Subscribe, pause, and poll stay on the dedicated membership thread. |
| Admission-aware cooperative assignor | Add `JOINING`/`READY`/`ACTIVE`, zero traffic assignments for non-ACTIVE members, full member-table assignment metadata, and debounced cohort promotion (§3.2.1–§3.2.2). |
| Pairwise witness tracking | Add peer observation echoes and per-pair maturity. Do not reset mature pairs for unrelated joins. |
| Versioned write footprint | Add conservative footprint/revision metadata, expansion-before-write gating, Kafka-acknowledged shrink eligibility, five-minute shrink coalescing, jittered `enforceRebalance()`, and assignment echo confirmation (§3.2.3). |
| Peer departure emission | Every survivor emits only over its last observed footprint for the departed writer; UNKNOWN falls back to all partitions. Do not select one emitter. |
| Relieve the proxy duration cap of its correctness role | no code deleted — `replayer-expiration-hardening.md` §5.3/§5.4 and `replayerHardenedArchitectureDesign.md` §10.8 stop citing the cap as the finite window a dead-proxy proof needs. The cap stays as operational policy. **No wall-clock setting is removed, because none exists**: force-expiry was rejected (§7 there) and banned (§10.5 row, §21 non-goal). Verified by grep — nothing clock-driven is reachable from the Kafka commit path. |
| `NoMoreWrites` record | **Implemented.** `ProxyNoMoreWrites` is emitted through `CaptureKafkaPublisher`'s ordered lane with an offset-authoritative boundary and a strictly increasing diagnostic timestamp. |
| Settle-on-writer-completion evidence | **Implemented.** `AbsenceProof.NoMoreWrites` carries emitter identity and the record offset; `KafkaLivenessScanner` settles only candidates whose last record precedes it. |
| Rename vague provenance fields | Rename source-level `declaredBy` terminology to `emitterNodeId` while preserving protobuf field numbers. |
| Traffic after peer completion | **Current behavior is unsound and must change.** Delete `KafkaSupersededTrafficRecord`; do not replace it with another synthetic record such as `KafkaPostDeclarationTrafficRecord`. Raise a `TrafficAfterPeerCompletionException`, retain the Kafka record, terminate replay, and emit `traffic_after_peer_completion`. |
| Producer-quiescence bound | Add final-send staleness validation and delay peer completion by staleness budget + producer delivery timeout + margin (§5). |
| Latch, staleness gate, failure-mode flag | `CaptureProxy`, `KafkaCaptureFactory` |

## 8. Out of scope for this round

- Broker-side fencing via `transactional.id` (§5).
- Block-shape optimization beyond the admission-aware custom cooperative assignor. Vacancy is not
  needed for correctness.
- Static membership (`group.instance.id`). Would avoid rebalances on planned restarts, but
  needs a stable identity from outside the process, which is the deployment coupling we are
  avoiding.
- Deliberate expiry of over-old connections. This is the only sound way to shrink post-scale-up
  manifest fan-out: capping *manifests* is unsafe, since skipping one for a partition that has live
  connections manufactures a false omission (§3.4). So the lever is connection lifetime, not
  manifest count. Deferred — the spike is
  transient and bounded by the request-duration cap.

## 9. Decisions and remaining follow-ups

### 9.1 Decided

**Peer-emitted `NoMoreWrites` records are mandatory.** They are point-in-time observations whose
ordering against the traffic they describe is critical. Putting them in the traffic log makes that
ordering readable. Group metadata identifies who should emit; it cannot replace the ordered record.
See §4.1 and §6.2.

**`emitterNodeId` stores the full `nodeId`,** not a self/peer bit. The size is irrelevant for these
rare records, and storing the identity makes self completion checkable by comparison
(`emitterNodeId == writerNodeId`) rather than an unverifiable flag. That comparison gates whether
later traffic is interpreted as an ordinary post-reassignment write or as a fatal protocol
contradiction that must be retained.

**Every survivor emits peer completion records.** A designated emitter would need durable handoff
state to survive leader failure after observing departure but before publishing every required
partition record. Assignment `userData` is intentionally not such a handoff log.

**Admission evidence is pairwise.** Global membership stability and elapsed time do not prove that
a survivor observed a short-lived writer. Only a peer's later subscription echo starts that
candidate/peer maturity relationship.

**Footprints are conservative.** Expansion is confirmed before writing; shrink happens only after
Kafka acknowledgement and may wait. UNKNOWN always means every partition.

**Peer completion waits for producer quiescence.** Group departure alone does not order one
producer against another. Survivors wait the hard §5 bound before publishing, retain each
partition obligation until Kafka acknowledgement, and treat any later writer traffic as fatal.

**Kafka membership timeouts are left at the client's defaults and are not set by us.** No values are
pinned here deliberately — the client version documents them, and restating them in a design doc only
creates something to drift. The reasoning behind not tuning them is that the tradeoff is asymmetric:
slow death detection costs retained memory and a delayed commit, both recoverable, while a *false*
eviction trips the §3.5 latch and takes a healthy proxy out of service over a GC pause. Bias toward
patience, which is what the defaults already do.

**Manifest fan-out gets no cap;** connection-lifetime expiry is the only sound lever and it is
deferred to §8.

### 9.2 Follow-up

Implement the remaining §7 rows and prove the following boundaries:

- deterministic assignor tests: cold-start cohorts, probation without assignment movement,
  unrelated joins not resetting mature pairs, witness loss resetting only that pair, and ACTIVE
  monotonicity; exact `N`/`N-1` witness-loss boundaries; quiet-group progress driven by
  `enforceRebalance()`; and degraded-redundancy reporting after ACTIVE witness loss;
- deterministic footprint tests: expansion blocks writes until echo plus witness acknowledgement,
  final-write acknowledgement precedes shrink eligibility, natural-rebalance coalescing, jittered
  five-minute enforcement, UNKNOWN fallback, stale/out-of-order revisions, crash before shrink
  echo, and activation assignments contained by the full-topic initial footprint;
- Testcontainers Kafka: real cooperative rebalances, short-lived joiner loss, leader loss during
  every admission/promotion stage, every-survivor peer completion, a survivor dying midway through
  its Kafka writes, footprint-version propagation with different peers on different revisions,
  metadata size limits, and broker restart during expansion/shrink;
- live Kubernetes: pod kill before and after activation, membership-network isolation, insufficient
  witness capacity, and total-fleet loss retention;
- OTel assertions for admission phase, mature-witness count, active fleet count, footprint
  revision/size, blocked expansion, forced rebalance, zero witness coverage, pending peer-completion
  obligations, write-gate failure, and fatal traffic-after-peer-completion violations.
