# Hardened Traffic Replayer Architecture

**Status:** Implemented foundations under convergence hardening

**Date:** 2026-09-10

**Revision note.** This revision resolves the decisions formerly collected as open questions in §19
and hardens §10's capture-side open-connection manifest design. Exact manifests remain complete and
chunked, but omission correctness now comes from an explicit per-proxy, per-partition
`manifestCycle`, not an assumption that all concurrent proxy activity reaches Kafka in proxy-side
execution order. A complete manifest omission resets only an incomplete per-connection HTTP
accumulation that began in that cycle or earlier. Later observations for the same captured client
connection begin a new accumulation only when their `manifestCycle` is greater than the omitting
manifest. Elapsed-time expiration is not commit authority until a
separate design resolves delayed suffixes and broker-clock jumps. `NoMoreWrites` is self-emitted
only, follows full connection teardown and publisher drain, and terminally retires one
proxy-partition identity. The cancellation
review additionally made two contracts explicit: aborting an active target
exchange must actively settle and clean up every owned sub-operation rather than wait for the normal
response path, and reassignment/shutdown revokes a transaction's generation-scoped runway
independently of source and target outcomes. A later editorial pass reworked the §7 and §13.2
diagrams for legibility (per-component nodes, one message per arrow, ownership moved to companion
tables) and tightened prose; it changed no contracts, invariants, or decisions.

This document, [proxyHorizontalScalingAndNodeDeath.md](proxyHorizontalScalingAndNodeDeath.md), and
[proxyManagedFleetCaptureRecovery.md](proxyManagedFleetCaptureRecovery.md) are the normative design
set for this work. Other branch design notes are historical inputs or implementation crosswalks;
they must not override these three documents.

**Scaling and capture-liveness contract (partially implemented):**
[proxyHorizontalScalingAndNodeDeath.md](proxyHorizontalScalingAndNodeDeath.md)
— horizontal proxy scaling uses consumer-group membership only for new-connection routing.
Exact manifests define cycle-scoped reset boundaries. An acknowledged empty manifest closes a
temporary assignment drain, and terminal self `NoMoreWrites` closes permanent proxy-partition
retirement. A hard crash that emits neither may leave incomplete state retained; group departure
only triggers group rebalance and never authorizes replay settlement.
For this redesign, capture-before-forward assumes that mutating source handlers cannot apply effects
before receiving the complete HTTP request; true streaming source execution remains out of scope.
Hardening the proxy's existing HTTP-method-based mutating-request classification is also deferred
and remains a documented coverage limitation.
In the current Kubernetes round, a terminal capture failure terminally fails the complete capture
workflow. The controller may preserve pass-through availability, but the failed resource never
returns to capture or authorizes a replacement snapshot. A fresh workflow may proceed only after
the managed addendum's Kafka-input-isolation, old-workload retirement, and source-side quiescence
preconditions are satisfied. Every managed fresh run uses session fencing, a trusted replay plan,
and session-aware checkpoints. A same-topic fresh run also requires reset records now; automatic
same-resource coverage restoration remains future work.

---

## Implementation Status (2026-09-07, branch `integrating3231`)

Foundational classes exist on this branch, but implementation status is not evidence that the
architecture has converged. The table below distinguishes mechanisms that can be retained from
mechanisms that require correction before this design can be called implemented.

| Mechanism | Status | Where |
| --- | --- | --- |
| §6.4 Completion gates | **Done** | `lifecycle/CompletionGate.java`; gate discipline through actor/session/shutdown paths |
| §9 Typed identity | **Done** | `lifecycle/ReplayIdentity.java` — all five key records plus work/record ids |
| §10.2 One consumer, two cursors | **Done** | `TrackingKafkaConsumer.scanAhead` |
| §10.3 Settlement verdicts | **Revision required** | Replace offset-ordered `AbsenceProof` with complete-manifest-cycle reset; remove elapsed-time commit authority |
| §10.4 Verdicts via control loop | **Done** | scan-blocker listener in `CapturedTrafficToHttpTransactionAccumulator`; `runLivenessScanIfDue` |
| §10.5 Expiration policy matrix | **Revision required** | Keep `--packet-timeout-seconds` diagnostic-only; manifest-cycle reset and terminal self `NoMoreWrites` are the only capture-liveness terminal facts in this round |
| §10.6 Epsilon lookahead | **Done as an optimization** | `ReplayReadGate`, `ReplayProgressController`, `ReplayEngine`; it is not commit authority and scanner-disabled parity remains a verification target |
| §10.7 Capture-side duration cap | **Not implemented** — and explicitly optional here | No proxy flag exists. The scaling sketch removes its residual correctness role entirely, so implement it (if ever) as operational policy only |
| §10.8 Proxy manifests and proxy completion | **Foundations done; protocol revision required** | Keep exact registry, chunked manifests, and self `NoMoreWrites`; add observation-level `manifestCycle`; narrow local linearization; remove peer completion and global publisher-order assumptions. |
| §11 Connection actor | **Done** | `lifecycle/ConnectionActor.java`, `ActorMailbox`, `NettyEventLoopActorMailbox`; sorter and schedule map **deleted** (acceptance criterion 15) |
| §12 Async permit pool | **Done** | `lifecycle/AsyncPermitPool.java`; `TrafficStreamLimiter` **deleted** |
| §13 Replay transaction | **Done** | `lifecycle/ReplayTransaction.java`, `ReplayOutcomes`, `TargetExchangeState`, `ReplayTransactionRegistry` |
| §14 Record disposition | **Correction required** | Retained records must continue blocking the Kafka commit watermark; transaction proposal, ledger settlement, source acceptance, and broker acknowledgement need distinct authority |
| §15 Resource ownership | **Done** | `lifecycle/ResourceOwnership.java` (tracker + metrics) |
| §16.1–16.2 Rebalance / shutdown | **Correction required** | Rebalance callback must revoke source runway, enqueue typed interruption, and return without waiting for replay work |
| §16.3 Event-loop death | **Policy corrected; simplification pending** | Unexpected live-session loop death signals process-fatal shutdown. Cross-thread mailbox-loss recovery machinery is a migration residue to delete, not architecture to preserve. |
| §17 Evidence API | **Done to first-impl scope** | whole-tuple sink retained; disposition depends on explicit evidence; part-level receipts remain internal per §19.5 |
| §19.1 Poison classifier | **Done** | `TargetResponseClassifier` + shared `ExceptionTypeAllowlist`, default empty |
| §19.7 Group-assigned routing + manifest interval | **Migration pending** | Current `PartitionRoutingPlan` and shard-width flag are replaced by group assignments and per-connection stored partitions; manifests become cycle-scoped reset input. |

Naming drift between this document and the code: current `ProxyOmissionProof`/`AbsenceProof`
terminology must become a manifest-cycle reset value; `ProxyOpenConnectionRegistry` became
`ProxyLivenessRegistry`; current
`ProxyLivenessSnapshotChunk`/snapshot-interval names should migrate to precise manifest
terminology. Of the §3.3 working names, `KafkaSourceActor`,
`SourceAssembler`, `ReplayCoordinator`, `ConnectionRuntime`, `TargetExchange`,
`RequestPreparationService`, and `EvidenceWriter` did not become classes — their responsibilities
live in the retained current classes (`TrackingKafkaConsumer`/`KafkaTrafficCaptureSource`,
`CapturedTrafficToHttpTransactionAccumulator`, `RequestSenderOrchestrator` and its `ActorRuntime`,
`NettyPacketToHttpConsumer`, the transformation pipeline, and the tuple sink), adapted to the
contracts here.

**Future work** is concentrated in the final scaling and capture-liveness protocol
([proxyHorizontalScalingAndNodeDeath.md](proxyHorizontalScalingAndNodeDeath.md)), separation of the
Kafka source-I/O and replay-intake owners, removal of mailbox-loss recovery, mandatory lifecycle
interfaces, and layered real-Kafka verification. Elapsed-time expiration remains non-committing
even after the proxy contract lands.

---

## 0. How to Read This Document

**The short version.** Today the replayer's lifecycle decisions are spread across a graph of
callbacks running on whichever thread happens to settle a future, so answering "does this Kafka
record reach a deliberate commit-or-retain decision on every path?" requires inspecting every path.
This design replaces the graph with two single-owner state machines — a **connection actor** owning
ordered target-side execution and connection termination, and a **replay transaction** owning one
request's resources, evidence, and final record disposition. Everything else becomes a producer of
typed messages to one of those two, so each lifecycle question is answerable from one state machine.

**What is *not* being claimed.** Kafka delivery stays at-least-once: a record may be redelivered
after a crash, a rebalance, or any staged offset never durably acknowledged — and deliberate
`Retain` decisions exist precisely to cause that. The guarantee is one **explicit disposition
decision per accepted record inside a process** — never zero (an orphaned offset), never two (a
double-commit crash). Nor does correctness become purely local: actors and transactions localize the
lifecycle state that today leaks across callbacks, but source assembly, offset low-watermarks,
permit accounting, and the disposition ledger remain genuinely cross-component, held together by
§6's invariants.

**Reading order.** §1–§4 are conceptual and worth reading in sequence: the problem, the core
idea, the vocabulary, and a worked example that traces one request end to end. §5–§14 are the
mechanisms; each opens with the problem it solves, so they can be read in any order once you have
the example in mind. §15–§18 are rules, metrics, tests, and gates. §19 records the design decisions
that constrain the first implementation.

**Deliberate omissions.** Current class names and the migration sequence live in the companion
crosswalk, not here. And the current replayer is not claimed to be broken in general: Kafka
consumption, HTTP reconstruction, transformation, Netty I/O, tuple sinks, and offset-commit
mechanics are all retained. Only their orchestration contracts change.

---

## 1. The Problem

The trouble is concentrated in one place: **the connective tissue that decides when work is
finished.** Four distinct failure mechanisms found there share one shape, and that shape is what the
design makes impossible. They are four *mechanisms*, not four outages — F3 and F4 surfaced while
diagnosing the same rebalance incident, which is itself telling: one incident hid two independent
instances of the same structural defect.

### 1.1 The recurring failure: a required signal silently disappears

| Mechanism | What silently disappeared |
| --- | --- |
| F1 | A request spanning two Kafka records hit a `connectionException`. The handler reset state and discarded the held record keys *without committing them*. Those offsets pinned their partition forever. |
| F2 | An expiring keep-alive connection committed its held keys, then a `finally` block committed the same keys again and threw. Because commits are only staged, the crash meant Kafka never learned — so restart re-delivered the records and re-crashed. |
| F3 | `BlockingTrafficSource` implements the traffic-source interface but did not override two lifecycle methods, which are `default {}` no-ops. Production wires the close callback *through* that wrapper, so every close notification was swallowed and the drain gate never reopened. |
| F4 | Closing a connection called `schedule.clear()`, dropping pending futures without completing them. Everything waiting on them — limiter permits, tracker entries, the ordering sorter — waited forever. |

None of these is an exotic race. Each is a **required notification or decision that had no
owner**, so when one path forgot it, nothing noticed. F3 is the purest form: an empty default
method on an interface is a legal, invisible way to lose a mandatory signal.

### 1.2 Why the current structure invites this

Four structural properties, each of which this design targets directly.

**Ordering is reconstructed after the fact.** Requests are admitted in source order, then pass
through a concurrency limiter, asynchronous transformation, and an event-loop submission — after
which an `OnlineRadixSorter` puts them *back* in order using a request index. Ordering is a repair
operation, and the repair needs its own cancellation and drain semantics, which are themselves
state that can leak (that is F4).

**Terminal decisions are inferred rather than stated.** Whether to commit an offset is derived
from a `ReconstructionStatus` plus a boolean, at a call site that may or may not be reached.
`EXPIRED_PREMATURELY` commits; `CLOSED_PREMATURELY` does not. One status therefore cannot express
"reset because complete manifest M omitted the predecessor interval" (limited commit authority)
versus "timed out or ran out of runway" (must not commit) — and the code has no precise value to
reach for.

**Cancellation can masquerade as success, or as nothing at all.** A cancelled send produces an
exception that is rethrown during tuple packaging, *before* the commit decision. So the request's
local bookkeeping drains — making dashboards look healthy — while its offsets stay pinned and its
tracing contexts stay open. Cancellation is neither success nor failure in the current vocabulary.
It is a gap.

**"Done" is not represented by anything.** `cancelConnection` returns an already-completed future
while its drain, channel close, and acknowledgement are still in flight. A synthetic-close gate is
an `AtomicInteger` that a missing callback can leave nonzero forever. Shutdown relies on the
process exiting. No object anywhere means "this operation's entire effect has settled."

---

## 2. The Core Idea

Give every mutable thing exactly one owner, and make every "done" a real completion.

Two new owners absorb the *per-connection and per-request* lifecycle responsibilities that are
currently spread across callbacks. They are not the only owners in the system — §3.3 lists the rest,
and source intake, progress accounting, and record disposition stay separate on purpose — but they are
where the failures in §1 live:

**A connection actor** owns everything about one target connection: the queue of things to do on
it (send this request, then close), the timer for when the next thing is due, the Netty channel,
the single exchange in flight, and the connection's terminal state. It processes one command at a
time, in admission order.

**A replay transaction** owns everything about one request: source request and response state, the
Kafka records carrying it, the concurrency permit, the transformed request buffers, the target
outcome, the evidence outcome, its generation-scoped runway state, the tracing contexts, and —
crucially — the single typed disposition proposal for that request's Kafka offsets.

Everything else becomes a *producer of typed messages* to one of those two. The assembler produces
source outcomes. Preparation produces a `Prepared` message. Netty produces a target outcome. The
evidence writer produces a durability outcome. The Kafka scanner produces proof-bearing control
events. None of them decides anything terminal.

Three consequences make this worth doing:

- **Ordering stops being a repair.** A request is admitted to its actor's queue *while source
  order is still known* — before the permit, before transformation. Preparation may then finish in
  any order, because the actor only ever looks at the head of its queue. Ordering holds by
  construction, so the sorter and schedule map are deleted rather than hardened.

- **Every policy decision has one home.** The transaction invokes one pure exhaustive policy over
  what the source did, what the target did, whether evidence is durable, and its runway
  observation. It produces one typed proposal. The ledger validates and applies that proposal to
  its obligations; it does not independently rerun policy. Cancellation becomes a first-class
  outcome that can never select a commit.

- **"Done" becomes checkable.** Aborting an actor returns a gate that completes only after its
  queue is settled, its in-flight exchange has been actively cancelled and its owned cleanup joined,
  its channel is closed, every transaction has dispositioned, and its source acknowledgement is
  delivered. Gates await real completions instead of counting or passively waiting for a normal
  callback that cancellation made impossible.

The design also carries the expiration-hardening policy: optional read-ahead bounded by a small
epsilon and coupled to replay progress; exact manifest-cycle reset for incomplete per-connection
HTTP accumulation; optional metadata scanning on the same Kafka consumer and assignment as replay;
reassignment and shutdown retaining for redelivery; a capture-side maximum connection duration
that produces ordinary close observations and bounds resource use; and an evidence API that can
evolve toward independent tuple parts.

---

## 3. Vocabulary

Several of these words are overloaded in the existing code and docs. This section is the authority
for what they mean here.

Within the replayer only, **admit** means that the replay-intake owner has accepted an immutable
work item, registered its obligations, and become responsible for driving it to a terminal
disposition. It does not mean that a proxy may accept a new client connection, that the target has
accepted a request, or that Kafka has accepted a commit.

### 3.1 The five different things called "close"

This ambiguity is a real source of bugs, so the design keeps the five lexically distinct:

| Term used here | Means |
| --- | --- |
| **Captured close** | A `close` observation *in the recorded data* — the original client's connection ended. This is input to be reconstructed. |
| **Source-side settlement** | The assembler's conclusion that a captured request or one incomplete accumulation segment is finished, for example complete request, captured close, manifest-cycle reset, terminal self completion, interruption, or shutdown. |
| **Ordered close command** | A target-side close placed in a connection actor's queue at its time-shifted position, so it happens *after* the requests preceding it. |
| **Channel close** | The Netty target socket actually closing. |
| **Source acknowledgement** | Telling the Kafka layer "that session is gone," which is what releases its drain gate. **Not** an offset commit. |

### 3.2 Terminal-decision vocabulary

- **Settled / terminal** — reached a final state that will never change. Said of outcomes and
  gates, never of "the callback ran."
- **Disposition** — the terminal decision for a Kafka record: close its contexts, and either
  `Commit` or `Retain`. Every accepted record gets exactly one.
- **Commit** — advance the Kafka offset past this record, meaning *on restart we will never see it
  again*. This is the irreversible act, which is why it requires commit authority.
- **Retain** — deliberately do *not* advance the offset. The record stays eligible for redelivery
  to this or another consumer. Contexts still close; only the offset is held.
- **Commit authority** — the justification required before advancing a Kafka offset. This design
  recognizes these alternatives:
  - **Replay evidence** — durable output written to a store (today, the tuple) recording *what replay
    did*. Normal replay requires this evidence.
  - **Manifest-cycle reset** — a complete exact manifest M omits captured client connection C, and
    C's current incomplete per-connection HTTP accumulation began in cycle M or earlier. This
    authorizes disposing only the Kafka obligations held by that accumulation. It is not a claim
    that C can never appear again.
  - **Terminal self completion** — valid self `NoMoreWrites` permanently retires one
    `(writerNodeId, partition)` after the proxy's terminal barrier and authorizes settling its
    already-known incomplete accumulations.
  Elapsed time alone is not commit authority. `--packet-timeout-seconds` may support diagnostics,
  but it cannot advance the Kafka commit watermark in this design.
- **Evidence** — reserved for the normal replay output managed by `EvidenceWriter`. A manifest-cycle
  reset or terminal self completion is commit authority, but is not an `EvidenceWriter` artifact.
- **Completion gate** — see §6.4. A future for a whole lifecycle operation, with an owner and
  documented postconditions that are already true when it completes successfully.
- **Record obligation and claim** — the ledger owns one obligation per Kafka record. Each
  accumulation or transaction that depends on observations in that record owns one attributable
  claim against it. Every claim settles exactly once as `Commit` or `Retain`; the record can commit
  only when all claims are `Commit`, and any `Retain` keeps the record eligible for redelivery. This
  replaces anonymous counters because an unfulfilled claim names the connection, session, record,
  and work still owed, and double fulfillment becomes structurally impossible. Claim registration
  has an explicit seal: an obligation cannot reduce or close until the replay-intake owner has
  declared that every observation in the record has been assigned to a leaf claim.
- **Out of runway** — we lost the right or the time to finish this work (partition reassigned,
  process shutting down). Never commit-eligible: someone else must be able to pick it up.
- **Runway state** — generation-scoped authority to enter a commit disposition. `KafkaSourceActor`
  owns the authoritative source-generation state. `RecordDispositionLedger`, on the replay-intake
  owner, keeps a monotonic observed runway so it can reject known-stale commits early, but source
  acceptance is final only when the source actor processes the commit command in order with poll
  and rebalance callbacks. It starts `Available` and may transition once to
  `Lost(REASSIGNMENT)` or `Lost(SHUTDOWN)`. Transactions hold only a monotonic local observation
  delivered as `RunwayLost`. Runway is orthogonal to source and target outcomes: reassignment can
  occur after both have already settled but before evidence or disposition has finished. Losing
  runway never rewrites an existing outcome; it vetoes any commit that the source actor has not
  already accepted.
- **Commit proposed / accepted / acknowledged** — three deliberately distinct stages. A transaction
  proposes a commit disposition to `RecordDispositionLedger`. The ledger sends a typed commit
  command to `KafkaSourceActor`; the source accepts it only after checking the current generation
  and registering the offset on its owner thread. Kafka acknowledges it only when the broker commit
  succeeds. Revocation ordered before source acceptance selects `Retain`; after source acceptance,
  `KafkaSourceActor` owns the pending broker-commit operation until acknowledgement or explicit
  failure. The ledger continues to own the record obligation and waits on that source-owned result.
- **Per-connection HTTP accumulation** — the source-side state currently assembling HTTP
  observations for one captured client connection. A manifest-cycle reset discards only the
  incomplete accumulation that the manifest covers. An observation with a greater manifest cycle
  may initialize a new accumulation for the same `connectionId`; delayed observations from the
  omitted cycle or earlier remain covered predecessor data.
- **Manifest cycle** — a monotonically increasing logical value scoped to one
  `(writerNodeId, partition)`. The proxy stamps each reconstruction-relevant observation with the
  current value and closes one cycle when it atomically copies its exact active-connection set and
  increments the value. It is not a Kafka producer epoch, group generation, timestamp, or offset.
- **Epsilon** — an optional small read-ahead margin (~30s) that smooths source admission. It is not
  an expiry trigger, proof source, or hard memory bound.
- **Settled watermark** — the contiguous point in source time up to which all admitted work has
  settled. The read gate is `settledWatermark + epsilon`.

### 3.3 Component names

All working names; the crosswalk maps them to current classes. `Side` identifies where a term
originates so a proxy manifest is not mistaken for a direct replayer observation.

| Name | Side | One-line role |
| --- | --- | --- |
| `KafkaSourceActor` | Replayer | Owns the Kafka consumer, both cursors, offset tracking, and rebalance |
| `ProxyManifestIndex` | Replayer | Reconstructs complete open-connection manifests and tracks their `manifestCycle` per `(writerNodeId, partition)` |
| `CaptureKafkaPublisher` | Proxy | Publishes proxy traffic, manifests, and terminal self-completion records to Kafka while preserving producer acknowledgement and terminal ordering |
| `ProxyOpenConnectionRegistry` | Proxy | Exact capture-side registry from which open-connection manifests are copied |
| `ProxyOpenConnectionManifest` | Proxy record | Complete, immutable set of the proxy's open connections for one partition |
| `ConnectionPartitionAssignment` | Shared protocol | Group-assigned partition chosen once and stored for one connection |
| `ReplayProgressController` | Replayer | Owns work tokens and the contiguous settled watermark |
| `ReplayReadGate` | Replayer | Decides whether another source record may be admitted |
| `SourceAssembler` | Replayer | Reconstructs requests, responses, and closes; single-threaded |
| `ReplayCoordinator` | Replayer | Registries; creates transactions; admits commands to actors |
| `ConnectionRuntime` | Replayer | A session's event-loop assignment, holding its actor and transactions |
| `AsyncPermitPool` | Replayer | Cancellable, future-based replacement for `TrafficStreamLimiter` |
| `RequestPreparationService` | Replayer | Transformation and signing; yields an owned prepared request |
| `ConnectionActor` | Replayer | FIFO command queue, one head timer, channel, one live exchange |
| `TargetExchange` | Replayer | Owner-controlled target attempt, retry, response/finalizer, abort, and cleanup lifecycle |
| `ReplayTransaction` | Replayer | One request's resources, outcomes, and disposition |
| `EvidenceWriter` | Replayer | Durable whole-tuple output; internal adapters may model future parts |
| `RecordDispositionLedger` | Replayer | Owns record obligations, context closure, disposition tracking, and commit proposals; source-generation runway authority remains with `KafkaSourceActor` |

---

## 4. Worked Example: One Request, End to End

This is the section to return to when a later mechanism seems abstract. Nothing here is new
machinery — it is §2 traced concretely.

### 4.1 The normal path

1. **Read.** `KafkaSourceActor` polls and decodes records on its source-I/O owner thread. It sends
   one immutable source batch, including generation identity, to the replay-intake owner.
   `RecordDispositionLedger` registers each `RecordObligation`: this record now *must* receive a
   disposition. `ReplayReadGate` admits the batch only if its source time is within
   `settledWatermark + epsilon`.

2. **Reconstruct.** `SourceAssembler`, on the replay-intake owner, feeds observations into the
   per-connection state machine and recognizes the end of a request.

3. **Admit — the pivotal step.** `ReplayCoordinator`, still on the replay-intake owner and therefore
   still in source order, does three things at once:
   - finds or creates the session's `ConnectionRuntime`, pinning it to one existing Netty event
     loop;
   - registers the new transaction's claims against the ledger-owned record obligations and
     registers a work token with `ReplayProgressController`;
   - posts one immutable `AdmitRequest` envelope to the assigned event loop. That event loop creates
     the mutable `ReplayTransaction` and appends its `ReplayRequest` command to the actor's FIFO queue.

   Nothing has been transformed and no permit has been acquired yet. Because admission precedes
   all asynchrony, **the actor's queue is source order** — which is why no sorter is needed later.
   Admission itself is bounded O(1) control work (registry lookup, occasional runtime creation,
   token allocation, one event-loop enqueue); it never waits for a permit, transformation, signing,
   a timer, target I/O, retry, or evidence output. Benchmarks should confirm the intake owner
   stays cheap, but no expensive request processing moves onto it.

4. **Prepare, concurrently.** The transaction asynchronously acquires a permit from
   `AsyncPermitPool`, then asks `RequestPreparationService` to transform and sign. That yields an
   `OwnedPreparedRequest`: one explicit handle the transaction now owns. Preparation for many
   requests runs in parallel and may finish in *any* order. That is fine.

5. **Execute in order.** The actor examines only its head command. It sends when two conditions
   hold: the head's preparation has completed, and its time-shifted send time has arrived. If a
   later command became ready first, it waits. The actor never blocks its event loop — it reacts to
   a `Prepared` message or a timer firing.

6. **Target exchange.** The actor performs one request/response against the target (with retries
   per policy) and settles the command with a `TargetOutcome`. Because the transaction lives on the
   same event loop, that outcome is delivered without another executor hop.

7. **Source settles independently.** Meanwhile the assembler has been accumulating the captured
   *response*. When it finishes, it posts `SourceOutcome.Complete` to the transaction. This may
   arrive before, during, or after step 6 — the transaction does not care about order, only that
   both slots become terminal.

8. **Join and write evidence.** With every required outcome terminal, the transaction asks
   `EvidenceWriter` to persist the tuple and waits for an `EvidenceOutcome`.

9. **Dispose exactly once.** On its event-loop owner, the transaction invokes the exhaustive
   `ReplayDispositionPolicy` over its three outcomes and runway observation and produces one typed
   proposal for each claim. It sends those proposals—not the raw outcomes for a second policy
   decision—to `RecordDispositionLedger` on replay intake. The ledger validates and reduces the
   sealed record's claims and, when every claim is `Commit`, sends a `CommitProposed` command to
   `KafkaSourceActor`. The source actor serializes it with rebalance and poll state, accepts or
   rejects it after validating the generation, and reports the result; the ledger then joins the
   broker acknowledgement.

10. **Release.** The transaction closes its owned resources exactly once: prepared request, permit,
    tracing contexts. Its completion gate completes only after disposition has settled, and the
    coordinator removes it from the registry then — so registry drain genuinely implies "offset
    decided, contexts closed."

11. **Progress.** The transaction gate settles its work token, `ReplayProgressController` advances
    the settled watermark, `ReplayReadGate` raises, and step 1 can happen again.

### 4.2 The same request, cancelled by a partition reassignment

This is the failure path that motivated the design, and it shows where each mechanism earns its keep.

1. `KafkaSourceActor`'s rebalance callback fires: this partition is revoked. On its owner thread it
   marks the old generation's authoritative runway `Lost(REASSIGNMENT)`, stops admitting records
   from that generation, and sends one ordered **interruption control event** to replay intake.

2. The replay-intake owner applies that event in order with delivered source batches. The
   disposition ledger marks its observed runway lost; the assembler settles an unfinished source
   side as `SourceOutcome.Interrupted` without rewriting an already terminal outcome. Independently,
   the coordinator posts `RunwayLost(REASSIGNMENT)` to every still-active transaction in the
   generation. This covers a request whose source and target already completed but whose evidence or
   record disposition has not. Source acceptance remains race-free because commit commands and
   authoritative revocation are serialized by `KafkaSourceActor`; the intake-side runway is an
   early rejection and drain signal.

3. The coordinator aborts the matching connection actors **by typed `ConnectionSessionKey`** — not
   by a concatenated string, not via a placeholder session number. `abort()` returns a **session
   termination gate**.

4. Each actor, on its own event loop: marks itself terminal; settles every queued command as
   `TargetOutcome.Cancelled(REASSIGNMENT)` **without invoking their send callbacks**; actively aborts
   the in-flight exchange; joins its owned cleanup; closes the channel and awaits it; removes itself
   from the cache. "Abort" means settling retry and pacing timers, channel acquisition, packet
   sending, response decoding/finalization, attempt resources, and the owner-controlled exchange
   result. It does not mean closing the channel and then waiting for the ordinary response future.

5. Each active transaction drains its owned children and reaches `DISPOSING`. Transactions that were
   still reconstructing usually have source `Interrupted` and target `Cancelled`; transactions that
   had progressed further may retain earlier terminal source or target outcomes. In both cases lost
   runway selects `Retain` unless the source had already accepted the commit by validating the
   generation and registering the offset. The ledger closes every record context exactly once. Any
   source-accepted broker commit remains source-actor-owned and must reach a broker acknowledgement
   or an explicit failure before the session can terminate; the ledger owns the waiting record
   obligation, and a transaction-level proposal alone has no such status.

6. Only now — after step 5 has settled for *every* transaction of the session — does the actor deliver
   its source acknowledgement and its session termination gate complete. Per §6.4 rule 5, transaction
   settlement is one of that gate's postconditions, so step 4's channel-level teardown is a *child* of
   the gate, not the whole of it. An actor whose channel is closed but whose transactions have not
   yet disposed is not terminated.

7. The coordinator awaits every session termination gate for the revoked generation, including an
   explicit acknowledgement for connections that never opened a session at all. Real records for
   the new generation resume only after all of them complete — which, by step 6, means after every
   affected record has been dispositioned and no actor, transaction, target exchange, timer, permit,
   target context, or in-memory source obligation from the old generation remains.

**What joining an exchange means.** The exchange adapter owns a terminal result and a cleanup gate
that it can settle without cooperation from the normal response path. A library future may be
uncancellable, but it is not allowed to own the session lifecycle: abort fences its late completion,
settles the adapter's result as `Cancelled`, closes or detaches every owner-held context, and joins the
adapter's cleanup. A late callback may release a self-owned library resource; it may not restart work,
complete a transaction a second time, or find mutable state belonging to a newer generation.

---

## 5. Goals

1. Make correctness locally provable from state-machine transitions rather than global callback
   inspection.
2. Preserve source ordering for requests on the same connection.
3. Preserve replay timing where possible without weakening connection ordering.
4. Guarantee that every admitted request and connection command terminates exactly once.
5. Guarantee that every accepted Kafka record receives exactly one explicit disposition decision
   within a process — never zero, never two. This is not exactly-once delivery; Kafka may still
   redeliver a retained or unacknowledged record (§0).
6. Guarantee that cancellation cannot be interpreted as successful replay.
7. Bound read-ahead and make every incomplete-state reset fact-based.
8. Make rebalance and shutdown completion observable through real completion gates.
9. Give every reference-counted resource one documented owner.
10. Support deterministic tests over all terminal transitions and event interleavings.

---

## 6. Governing Invariants

The rules, each with the failure it prevents. Where a rule maps to an audit row or an incident
from §1.1, that is named.

### 6.1 Ownership

| Rule | Without it |
| --- | --- |
| Every mutable state object has one executor or thread owner | Concurrent mutation of accumulator or session state — the class of bug that a `volatile` on one flag does not fix |
| Cross-thread completions post typed messages to the owner; they never mutate foreign state | A second thread calling into single-threaded machinery (the wall-clock heartbeat-expiry hazard) |
| Generation runway authority is owned by `KafkaSourceActor`; the ledger and transactions hold only monotonic observations | A transaction, ledger, and Kafka consumer disagree about whether an old generation may still commit |
| Every resource is released by whoever accepted ownership of it | The refcount leaks of §13 (R16–R18) |
| No required lifecycle notification is an optional no-op callback | **F3** — a `default {}` method silently swallowing every close notification |

Mandatory lifecycle interfaces use **no Java default methods and no built-in `NO_OP` instance**.
Every required collaborator is an explicit constructor argument, so omitting lifecycle forwarding
is a compile failure. If a capability is genuinely optional, split it into a separate interface and
make the composition root choose an explicit adapter. Do not make a required interface look
optional for test convenience.

### 6.2 Ordering

| Rule | Without it |
| --- | --- |
| Connection commands are admitted while source order is still known | You need a sorter, and the sorter needs its own cancellation and drain semantics — more state to leak (**F4**) |
| An actor executes one command at a time in FIFO admission order | Out-of-order sends on one connection |
| Asynchronous preparation may complete out of order but cannot reorder execution | A fast-transforming request overtaking a slow one ahead of it |
| Sequence numbers are validation and diagnostic data, not the ordering mechanism | Ordering silently depends on an index staying consistent across rebuilds |

### 6.3 Disposition

| Rule | Without it |
| --- | --- |
| Closing a traffic-stream context and committing its offset are separate actions | Retained records leak open contexts — F1/F2 territory |
| Every accepted record is deliberately committed or deliberately retained | Records with no decision at all: offsets pinned, dashboards clean (**F1**) |
| Normal replay commits only after required evidence is durable | Committing data whose evidence was never written |
| Work whose runway is lost before source acceptance, and any unclassified failure, does not commit | Teardown masquerading as successful replay — silent data loss |
| A deterministic poison record commits only under an explicit classifier, with durable, loud skip evidence | Either an unskippable crash loop or a silent skip — and no way for an operator to choose which |
| A complete manifest-cycle reset may commit only the incomplete accumulation covered by that cycle; elapsed time may not | An old omission or an impatient timeout committing newer or still-live data |

### 6.4 Completion gates

A **completion gate** is the future for a lifecycle operation *together with* an explicit owner and
documented successful postconditions. It is deliberately none of the following: a Java Memory Model
barrier, a separately cancellable aggregate waiter, or a notification that cleanup has merely
started. Getting this distinction wrong is exactly what makes `cancelConnection` return "done"
while work is still in flight.

1. Every public lifecycle operation that starts, stops, cancels, or waits for work returns **one**
   gate representing the complete operation.
2. The operation owner registers every child operation before it can run, and composes every
   child's terminal stage into the gate. No owned child may be launched as an unreturned
   fire-and-forget branch.
3. Successful gate completion means all documented postconditions are **already** true. A child
   failure propagates through the gate unless the operation's typed result explicitly accounts for
   it.
4. Requesting cancellation does not complete the gate. The gate completes only after queued and
   active children have reached terminal outcomes *and* their owned resources have been released.
   The owner must actively drive cancellable children to those outcomes; waiting on the normal
   success path after making success impossible is not a cancellation implementation.
5. Session termination completes only after queued commands, active work, channel closure, cache
   removal, transaction settlement, and source acknowledgement have all settled. No mutable or
   resource-owning object from the terminated generation may remain reachable by the next generation.
6. Active target-exchange abort is total over every phase: before channel acquisition, during
   acquisition, pacing, packet send, response wait, retry delay, response finalization, and late
   completion. Its gate describes owned cleanup, not merely the exchange result.
7. A timeout or watchdog may report or fail the operation. It may never reset state or complete the
   gate successfully while postconditions remain false.
8. Reusable shutdown does not rely on process exit for cleanup.
9. End-of-input drain uses a replay-quiescence gate owned by `ReplayProgressController`. Admission
   opens a new interval when outstanding work changes from zero to one; settlement completes that
   interval only when the count returns to zero. The top-level drain joins this gate with request
   tracking while servicing the replay-intake mailbox. It does not poll `isWorkOutstanding()`, and
   cancellation of a caller's aggregate waiter cannot cancel the controller's authoritative gate.

Callers chain subsequent lifecycle work from the gate. They do not infer completion from a callback
firing, a counter reaching zero, a cache entry disappearing, or the cancellation of a separate
aggregate waiter.

**Enforce this mechanically, not by convention** — every one of the four failure mechanisms in §1.1
was a convention that one code path did not follow:

- The owner keeps the mutable `CompletableFuture` private and exposes a non-mutable view, such as
  `minimalCompletionStage()` or a dedicated wrapper.
- Cancellation is requested through an owner method such as `abort(reason)`. Callers cannot cancel
  or complete the gate directly.
- Each child enters the owner's pending set *before* its asynchronous work is launched, and leaves
  that set exactly once when its terminal message is processed.
- Pending-child diagnostics include the child identity and phase, so a stuck response finalizer,
  retry delay, context close, disposition acknowledgement, or source acknowledgement is distinguishable
  without a thread dump.
- One owner-thread method — `tryCompleteTermination()` — is the only code allowed to complete the
  gate successfully.
- That method asserts the operation is terminal, the pending set is empty, all required resources
  are released, and all required acknowledgements and dispositions have settled.

Tests must attempt early completion, caller-side cancellation, child failure, late child
completion, timeout paths, and multiple consecutive generation terminations, and prove that none can
produce a false successful result or leave state that the next generation can observe.

---

## 7. Proposed System

Three views answer three different questions: §7.1 which thread owns which state, §7.2 what one
request waits on and what releases each wait, §7.3 what must drain before a lifecycle operation may
complete. In every diagram an arrow is a message, a future completion, or a network exchange — never
direct cross-thread mutation, and never a thread blocking.

### 7.1 Thread ownership and cross-thread messages

Containers are execution domains: everything inside one mutates state only on that container's
thread. **Solid arrows cross a thread boundary** and are always queued messages or future
completions. **Dotted arrows stay on one thread** and show phase order or direct delivery.

```mermaid
flowchart LR
    subgraph SOURCE["Kafka source-I/O owner — exactly one"]
        direction TB
        KSA["KafkaSourceActor<br/>consumer + scanner + commit adapter"]
    end

    subgraph INTAKE["Replay-intake owner — exactly one"]
        direction TB
        READ["ReplayReadGate"]
        ASM["SourceAssembler"]
        COORD["ReplayCoordinator"]
        POOL["AsyncPermitPool"]
        LEDGER["RecordDispositionLedger<br/>+ ReplayProgressController"]
    end

    subgraph LOOP["One session's ConnectionRuntime — its assigned Netty event loop"]
        direction TB
        ACTOR["ConnectionActor"]
        EXCH["TargetExchange"]
        TXN["ReplayTransaction"]
    end

    PREP["RequestPreparationService<br/>(transformation workers)"]
    EVID["EvidenceWriter<br/>(evidence sink executor)"]
    TARGET["Target cluster"]

    READ -->|"read demand / pause"| KSA
    KSA -->|"immutable source batch /<br/>lifecycle event"| ASM
    LEDGER -->|"CommitProposed / release /<br/>source acknowledgement"| KSA
    KSA -->|"CommitAccepted /<br/>CommitAcknowledged"| LEDGER
    ASM -.->|"completed request / close"| COORD

    COORD -->|"AdmitRequest"| ACTOR
    COORD -->|"abort(sessionKey)"| ACTOR
    ASM -->|"SourceOutcome"| TXN
    COORD -->|"RunwayLost"| TXN

    POOL <-->|"PermitRequested /<br/>PermitGranted"| TXN
    TXN <-->|"PrepareRequest /<br/>Prepared"| PREP
    TXN <-->|"write evidence /<br/>EvidenceOutcome"| EVID
    EXCH <-->|"target request /<br/>response or failure"| TARGET

    TXN -.->|"head ready"| ACTOR
    ACTOR -.->|"execute head"| EXCH
    EXCH -.->|"TargetOutcome"| TXN

    TXN -->|"DispositionDecision ·<br/>settle work token ·<br/>PermitReleased"| LEDGER

    style SOURCE fill:#dcecf8,stroke:#2f6687
    style INTAKE fill:#e9f3fb,stroke:#2f6687
    style LOOP fill:#fbe9dc,stroke:#9a5a2e
    style PREP fill:#eaf6e8,stroke:#4f7a46
    style EVID fill:#f5efdc,stroke:#7d6c32
    style TARGET fill:#eeeeee,stroke:#666666
```

`AdmitRequest` lands on the session's assigned event loop, which creates the `ReplayTransaction` and
appends its command to the actor's FIFO — so the actor, exchange, and transaction for one session
share one event loop and exchange no cross-thread messages among themselves. Other sessions may use
other loops from the existing group.

Kafka polling, scanning, source-generation state, and commit acceptance belong to the source-I/O
actor. Reconstruction, admission, permits, progress, and disposition bookkeeping belong to replay
intake. The two owners exchange immutable batches, lifecycle events, source commands, and
acknowledgements. When source admission is closed, the source actor keeps servicing Kafka
heartbeats, commits, rebalances, and bounded scans while replay intake continues draining mailbox
work. `ReplayTransaction` receives `RunwayLost` so it can begin draining promptly; the source actor
still makes the final source-acceptance decision.

### 7.2 One request: wait states and their releasing events

The numbered states are the only places a request can be waiting. No wait blocks an OS thread: the
owner records which conditions remain unsatisfied, returns to its event loop, and reevaluates only
when one of the labeled events arrives. The table under the diagram gives each wait's owner and
thread.

```mermaid
flowchart TD
    ADMITTED["ADMITTED — transaction created,<br/>command queued in the actor's FIFO"]
    W1["1 · awaiting permit AND prepared request"]
    W2["2 · awaiting FIFO head, scheduled time,<br/>and no active exchange"]
    W3["3 · awaiting terminal target outcome"]
    W4["4 · awaiting join: every required<br/>source and target outcome terminal"]
    W5["5 · awaiting evidence durability<br/>(when required)"]
    W6["6 · awaiting disposition, commit ack<br/>when accepted, and resource release"]
    DONE(["Transaction completion gate succeeds"])

    SRC["Source slot — settles independently:<br/>before, during, or after target work"]
    CANCEL["Cancellation or runway loss — any phase"]
    DRAIN["DRAINING — actively settle timers, permit<br/>acquisition, preparation, exchange, resources"]

    ADMITTED -->|"permit acquisition and preparation<br/>start concurrently"| W1
    W1 -->|"PermitGranted AND Prepared"| W2
    W2 -->|"all three conditions hold"| W3
    W3 -->|"TargetOutcome, including abort"| W4
    SRC -->|"SourceOutcome"| W4
    W4 -->|"last required outcome terminal"| W5
    W5 -->|"EvidenceOutcome, or not required"| W6
    W6 -->|"DispositionSettled AND resources closed"| DONE
    CANCEL -->|"never skips disposition"| DRAIN
    DRAIN -->|"owned children terminal"| W6

    classDef wait fill:#fff2cc,stroke:#9a6700,stroke-width:2px
    classDef action fill:#e9f3fb,stroke:#2f6687
    classDef terminal fill:#eaf6e8,stroke:#4f7a46,stroke-width:2px
    class W1,W2,W3,W4,W5,W6 wait
    class ADMITTED,SRC,CANCEL,DRAIN action
    class DONE terminal
```

| # | Waiting for | Wait owner (thread) | Released by |
| --- | --- | --- | --- |
| 1 | Permit and prepared request | `ReplayTransaction` (event loop) | `AsyncPermitPool`: `PermitGranted` **and** `RequestPreparationService`: `Prepared` |
| 2 | FIFO head, scheduled send time, previous exchange terminal | `ConnectionActor` (event loop) | All three conditions holding at once |
| 3 | Terminal target outcome | `ConnectionActor` (event loop) | `TargetExchange`: `TargetOutcome` or abort outcome |
| 4 | Every policy-required source and target outcome | `ReplayTransaction` (event loop) | The last required outcome turning terminal |
| 5 | Evidence outcome, when required | `ReplayTransaction` (event loop) | `EvidenceWriter`: `EvidenceOutcome`, or policy: not required |
| 6 | Disposition proposal applied, source acceptance and commit acknowledgement when accepted, owned-resource release | `ReplayTransaction` computes one proposal (event loop); `RecordDispositionLedger` validates and applies it (replay intake); `KafkaSourceActor` accepts and acknowledges commit (source I/O) | Source result settles the ledger; transaction closes its resources |

The two side entries are orthogonal to the main path on purpose. The source slot may settle at any
time relative to target work — state 4 simply requires both. Cancellation from any phase actively
settles every owned child, then rejoins the same disposition path; it never jumps to successful
completion. And a later request may already hold `Prepared` yet sit in state 2 until it is the FIFO
head — that is the ordering rule.

### 7.3 Drain dependencies and completion gates

An arrow means the downstream gate cannot complete until the upstream gate has completed. Each gate
additionally has local postconditions (table below) that must already be true when it completes.
Requesting cancellation, closing a channel, removing a cache entry, or observing a counter change
releases nothing by itself.

```mermaid
flowchart TD
    TXN(["Transaction gate<br/>ReplayTransaction · event loop"])
    COMMIT(["Accepted-commit gate<br/>KafkaSourceActor · source I/O"])
    SESSION(["Session termination gate<br/>ConnectionActor · event loop"])
    DRAIN(["Replay quiescence gate<br/>ReplayProgressController · replay intake"])
    LIFE(["Rebalance / shutdown gate<br/>source I/O + replay intake"])
    GO(["Resume the next generation<br/>or finish shutdown"])

    TXN -->|"every session transaction joined"| SESSION
    COMMIT -->|"every session commit joined"| SESSION
    TXN -->|"settles its request work token"| DRAIN
    SESSION -->|"settles its session work token"| DRAIN
    SESSION -->|"every in-scope session joined"| LIFE
    DRAIN -->|"replay quiescent"| LIFE
    LIFE -->|"all joined gates succeeded"| GO

    classDef gate fill:#f4f4f4,stroke:#555555,stroke-width:2px
    classDef terminal fill:#eaf6e8,stroke:#4f7a46,stroke-width:2px
    class TXN,COMMIT,SESSION,DRAIN,LIFE gate
    class GO terminal
```

| Gate | Local postconditions, beyond joined child gates |
| --- | --- |
| Transaction | Required outcomes terminal; disposition accepted; contexts and resources released |
| Accepted commit | Generation-valid broker acknowledgement received, or an explicit failure |
| Session termination | Queue empty; `TargetExchange` cleanup joined; channel closed; cache entry removed; source acknowledgement delivered |
| Replay quiescence | Every admitted request and session work token settled — outstanding work reaches zero |
| Rebalance / shutdown | Every in-scope transaction, accepted-commit, session, and quiescence gate succeeded. Transactions and commits join through their session gates in the picture, but the lifecycle owner verifies all four kinds. |

Two further waits gate record flow rather than lifecycle completion: `ReplayReadGate` admits another
source record only when the settled watermark plus epsilon allows it and lifecycle intake is open,
and the Kafka commit watermark advances only across a contiguous prefix of commit-eligible
obligations whose broker commits have been acknowledged. A `Retain` decision closes local contexts
and settles the process-local lifecycle, but it continues to block the Kafka commit watermark so
the record remains eligible for redelivery.

`ReplayProgressController` computes the contiguous settled watermark and owns the replay-quiescence
gate. Completion gates and `ScanEvidence` are threadless values: each owner completes its private
mutable future on its own thread, while other components hold only a non-mutable stage view.

---

## 8. Thread and Executor Model

**The design uses two explicit serialized source-side owners: one Kafka source-I/O actor and one
replay-intake mailbox.** Kafka's consumer API is blocking and thread-confined, while replay intake
must continue servicing actor completions, permits, progress, and disposition callbacks. Treating
those as one thread forced the implementation toward locks and concurrently mutable maps. The
sound boundary is two owners with typed messages, not three threads sharing lifecycle state.

The application-owned execution domains on the normal request path are therefore:

1. Exactly one Kafka source-I/O owner thread.
2. Exactly one replay-intake owner thread.
3. The existing transformation worker pool, with its configured worker count.
4. The existing Netty event-loop group, with its configured thread count; each session is pinned to
   one of those threads, not given a new thread.
5. The sink-specific evidence executor, when the configured sink uses one.

Actors, transactions, completion gates, permit waiters, and source accumulations do not create
threads. Kafka or Netty libraries may own internal housekeeping threads, but those threads own none of
the replay lifecycle state described here. A blocking read adapter may not own or mutate source
lifecycle state and may not invoke the Kafka source from a third thread. The current
`BlockingTrafficSource` executor is therefore migration residue: replace its blocking handoff with
an asynchronous read-gate signal or fold it into replay intake.

When the first command for a `ConnectionSessionKey` is admitted, `ReplayCoordinator` assigns a
`ConnectionRuntime` to one existing Netty event loop and records that assignment in the session
registry. The runtime is created *before* permit acquisition, transformation, or target-channel
creation. Its `ConnectionActor` and every `ReplayTransaction` for that session execute all state
transitions on that same event loop. An actor or transaction is a **mailbox-bound state machine,
not a dedicated thread.**

| Owner | Thread/executor | Mutable state |
| --- | --- | --- |
| `KafkaSourceActor` | One source-I/O owner thread | Consumer assignment, source-generation runway, replay/scan positions, offset trackers, commit acceptance and acknowledgement, source-side active-connection index |
| `SourceAssembler` and `ReplayCoordinator` | One replay-intake owner thread | Reconstruction state, session admission, affinity registry |
| `AsyncPermitPool` | Replay-intake owner | Permit queue and available capacity; releases are posted back to this owner |
| `ConnectionRuntime` | One assigned existing Netty event loop | `ConnectionActor`, session transactions, command mailbox, timers, target channel, terminal state |
| `RequestPreparationService` | Transformation/event-loop workers as appropriate | No shared connection lifecycle state |
| `EvidenceWriter` | Sink-specific executor | Sink-local buffering and durability |
| `ReplayProgressController` | Replay-intake owner | Admitted-work tokens, replay-quiescence gate, and contiguous settled watermark |
| `ReplayReadGate` | Replay-intake owner | Source admission using settled watermark, epsilon, and lifecycle state |
| `RecordDispositionLedger` | Replay-intake owner | Record obligations, observed runway, context closure, disposition state, retained-record release |

Cross-thread completions are converted into messages: `Prepared`, `SourceSettled`,
`EvidenceSettled`, `PermitReleased`, `RunwayLost`, `CommitProposed`, `CommitAccepted`,
`CommitAcknowledged`, and `AbortRequested`. Target exchange callbacks already run on the assigned
Netty event loop, so **the actor and transaction communicate with no extra executor hop** — this is
why co-locating the transaction with its actor matters rather than giving transactions their own
executor.

Every mutable owner has an always-enabled `OwnerThreadGuard`. Every state-mutating method checks the
guard and invokes the fatal replay handler on violation; this is not a Java `assert`, which
production commonly disables. Interfaces exposed across owners accept immutable command values and
return `CompletionStage` acknowledgements where ordering matters. They never expose live maps,
queues, lifecycle registries, or callbacks that mutate foreign-owned state directly. A lock or
`ConcurrentHashMap` is not a substitute for assigning an owner.

The sole cross-owner mutable primitive is a process-wide, one-way `ReplayFatalFence`. It carries no
per-request state, registry, cleanup operation, or recovery authority. Its
`tryAcceptCommit(registration)` and `close()` operations share one bounded synchronization point:

- `KafkaSourceActor` calls `tryAcceptCommit` on its owner thread. If open, the gate executes only the
  bounded generation validation and pending-commit registration before releasing the gate.
- A fatal observer calls `close`. Closure waits only for any acceptance already inside that bounded
  critical section, then permanently rejects later acceptance.

Kafka I/O and broker acknowledgement never run while holding the gate. This creates one exact order:
a commit is accepted before fatal closure or rejected after it; a check-then-register race is
impossible.

The normal logical handoffs are bounded and explicit:

1. Kafka source-I/O owner → replay-intake owner, with an immutable decoded batch or source-lifecycle
   event.
2. Replay-intake owner → Kafka source-I/O owner, with commit, release, scan-blocker, or session-
   termination commands; the source replies with immutable acceptance/acknowledgement results.
3. Replay-intake owner → the assigned Netty event loop, with an immutable admission envelope.
4. Netty event loop → replay-intake owner for permit acquisition, and back when granted.
5. Netty event loop → a transformation worker, and back with `Prepared`.
6. Netty event loop → the evidence sink, and back with `EvidenceSettled`.
7. Netty event loop → replay-intake owner, with immutable disposition, progress, and permit-release
   messages.

Removed relative to today: direct intake-thread mutation of `TrackingKafkaConsumer`,
`commitDataLock`, source-state `ConcurrentHashMap` sharing, the blocking-source third-thread call
into Kafka, the limiter-feeder thread, the post-transformation sorter handoff, the independent
schedule executor, any actor-to-transaction hop, and any per-connection thread. Actual OS context
switches remain scheduler-dependent.

The Kafka source actor must remain responsive to Kafka's poll contract. It processes source commands
under a bounded record-count or elapsed-time budget and returns to Kafka before that budget can
threaten `max.poll.interval.ms`. When `ReplayReadGate` closes, replay intake stops requesting
ordinary data; the source actor continues short polls or equivalent touches for heartbeats, commits,
rebalance callbacks, source-control messages, and bounded scanner cycles. The replay-intake mailbox
continues servicing transaction completions independently, so neither owner blocks the other's
progress.

---

## 9. Identity Model

**The problem.** Identity is currently assembled ad hoc — `connectionId + ":" + sessionNumber +
":" + generation` strings, with a `PENDING_CLOSE_SESSION_NUMBER_PLACEHOLDER` constant that three
separate call sites must keep in lockstep or the drain gate leaks forever. Elsewhere identity is
recovered from "whichever traffic-stream key is still in this list," which is why a normal close
with an empty key list can skip a required notification.

**The mechanism.** Identity is typed and travels with every message:

```java
record SourceConnectionKey(String nodeId, String connectionId) {}

record ConnectionSessionKey(
    SourceConnectionKey connection,
    int sessionNumber,
    int sourceGeneration
) {}

record ReplayRequestId(
    ConnectionSessionKey session,
    int requestIndex
) {}

record KafkaRecordId(String topic, int partition, long offset, int generation) {}

record ManagedCaptureRunKey(
    String captureDomainId,
    long captureDomainRecoverySequence,
    String captureSessionId,
    String trafficKafkaClusterId,
    String trafficTopicId
) {}
```

All source acknowledgement, actor lookup, scan evidence, tracing, and record disposition use these.
No callback reconstructs identity from whichever traffic-stream key happens to remain in a list.

Managed replay batches and checkpoints also carry `ManagedCaptureRunKey`. A managed run receives it
from the trusted replay plan; it never infers the key from the first traffic or reset record. This
is mandatory for every managed run. A distinct topic changes how the trusted starting offsets are
established; it does not make the session, plan, or checkpoint identity optional.

**Why this shape.** Two sites cannot disagree about a key when the key is one shared type — the
lockstep problem is *deleted* rather than documented. `sessionNumber` distinguishes logical sessions
on one captured connection (keep-alive reuse, restarts). `sourceGeneration` is what makes a stale
commit from a previous assignment structurally impossible rather than defensively filtered.

**`nodeId`/`writerNodeId` identifies one capture activation, and that is load-bearing.** The capture
proxy generates a fresh UUID whenever it begins capture, including when the same operating-system
process could otherwise attempt to resume after capture abandonment. It identifies neither a host
nor a permanently reusable process. A stable identity would let a replacement activation speak
about a stalled predecessor's still-open connections. The fresh identity scopes manifests to the
activation that produced them. It does not fence that activation's Kafka producer, which is why
silence and group departure cannot prove completion.

---

## 10. Source Intake and Structural Scanning

### 10.1 Why metadata lookahead is useful

Lookahead answers two questions: *does a follow-up observation for this connection exist, or has its
proxy produced a complete manifest-cycle boundary that lists or omits it?*

The replay cursor will eventually encounter the same traffic and manifests in normal offset order.
The scan cursor is therefore not required for correctness. It is a metadata-only optimization that
can reach a distant follow-up or manifest without buffering all intervening payloads. Its job is to
decouple **decision distance** from **buffered bytes**.

Scanner enabled and scanner disabled must produce the same eventual disposition for the same
durable log once normal replay reaches the same records. They may differ in memory use and latency.
Scanner budget exhaustion is `Inconclusive`; it does not authorize expiration or commit.

Traffic silence, group departure, process-health observations, and elapsed time do not prove that a
proxy producer is fenced. If no complete applicable manifest or terminal self `NoMoreWrites`
exists, incomplete Kafka obligations remain retained.

### 10.2 One consumer, two logical cursors

`KafkaSourceActor` owns the Kafka consumer and provides:

* **Replay cursor:** polls full records for normal reconstruction.
* **Scan cursor:** temporarily seeks ahead and reads the metadata needed to determine whether a
  blocked connection has follow-up observations.

A scan cycle:

1. Copy assignment, generation, and the exact replay position for every partition.
2. Select commit-head blockers and the required follow-up kind for each.
3. Seek ahead within a bounded operational scan budget.
4. Poll and decode **only** connection identity, recognized record type, observation kind,
   observation `manifestCycle`, and proxy open-connection manifest chunks.
5. Discard payloads.
6. Stop early per blocker when a required follow-up is found or one complete exact manifest
   resolves an applicable cycle boundary (§10.8). A discovered self `NoMoreWrites` at cutoff K
   settles that blocker early only if the scan covered every offset from the blocker's replay
   frontier through K without finding its required follow-up.
7. Restore every replay position before returning control.
8. Discard all scan results if assignment or generation changed during the cycle.

The scanner never advances replay positions, record lifecycles, replay time, or commit offsets.
Exhausting the operational scan budget produces `Inconclusive`.

**Why the same consumer rather than a second consumer?** A separately grouped consumer does not
automatically share assignment or generation with replay; manual assignment could reproduce that
relationship, but would create a second ownership and rebalance protocol to keep correct. One consumer
with two logical cursors gives exact assignment and generation coupling by construction.

### 10.3 Verdicts are proof-bearing

```java
sealed interface ScanEvidence {
    record FollowUpPresent(...) implements ScanEvidence {}
    record ManifestCycleResolved(ManifestCycleDecision decision, ...) implements ScanEvidence {}
    record TerminalSelfCompletion(ProxyPartitionCompletion completion, ...) implements ScanEvidence {}
    record Inconclusive(...) implements ScanEvidence {}
}

sealed interface ManifestCycleDecision {
    record Listed(
        String writerNodeId,
        int partition,
        String connectionId,
        CompleteManifest manifest
    ) implements ManifestCycleDecision {}

    record Omitted(
        String writerNodeId,
        int partition,
        String connectionId,
        CompleteManifest manifest
    ) implements ManifestCycleDecision {}
}

record CompleteManifest(
    long manifestCycle,
    long firstOffset,
    long lastOffset,
    Set<String> openConnectionIds
) {}

record ProxyPartitionCompletion(
    String writerNodeId,
    int partition,
    long completionOffset
) {}

enum FollowUpRequirement {
    REQUEST_COMPLETION,
    RESPONSE_COMPLETION,
    CONNECTION_TERMINATION
}
```

`ManifestCycleResolved` is usable only when:

- every index in `0..chunkCount-1` was consumed exactly once and all
  chunks carry a consistent header.
- the observations and manifest use the same `writerNodeId` and partition;
- the partition stamped inside every traffic record and manifest chunk equals the Kafka partition
  from which it was consumed.
- the manifest's `manifestCycle` is valid for that proxy activation and partition; and
- malformed records, contradictory chunks, duplicate indexes, partition mismatches, or cycle
  regression have made neither the manifest nor its decision ambiguous.

A complete manifest M that lists C resolves continuity across boundary M. A complete manifest M
that omits C resets only the predecessor accumulation segment containing observations whose cycle
is at most M. The omission is not a permanent tombstone for C.

Because each `TrafficRecord` is cycle-homogeneous, the record-level rule is exact and idempotent:

- a record for C with `manifestCycle <= M` belongs to the predecessor covered by an omitting M,
  even if Kafka appended or delivered that record after the manifest chunks;
- a record for C with `manifestCycle > M` belongs to the successor and is not committed by M; and
- a record that mixes cycles is malformed and halts rather than creating sub-offset obligations.

The Kafka-order inversion requires one additional source-assembler rule. The concrete race is:

1. The replayer holds an incomplete per-connection HTTP request prefix for C whose observations
   carry cycle M.
2. The proxy closes C's source-forward gate, drains every previously source-authorized operation,
   and removes C from the exact active-connection registry.
3. The next manifest snapshots that registry as manifest M, omits C, and advances the partition
   counter to M+1.
4. A late, non-source-authoritative callback for C reads M+1. Kafka may append or deliver that
   observation before the chunks of manifest M.

The cycle-M prefix and the cycle-M+1 observation can therefore both be present at the replayer
before the omitting manifest is resolved. They are not two simultaneously valid versions of one
request. Until M is resolved, the newer observation is held separately as a **successor
accumulation segment** and is not irreversibly merged with the predecessor prefix. When M resolves,
the replayer performs this exact per-connection transition:

- if M lists C, merge the predecessor and successor according to the ordinary TrafficStream
  observation-ordering rules;
- if M omits C, terminally dispose only the incomplete cycle-M-or-earlier request-assembly state,
  create an empty per-connection HTTP request accumulator, and feed the already-held
  cycle-M+1-or-later observations into that new accumulator;
- Kafka offsets belonging to the successor remain obligations of the successor and are not
  committed by M's omission; and
- a request already recognized as complete is detached from connection liveness and is never
  discarded merely because a later manifest omits its connection.

This “reset” is only a reset of the replayer's per-connection HTTP request-assembly state. It does
not reset, reopen, or otherwise act on the proxy's connection. A later observation for C is treated
as the first observation of a new accumulation segment and still requires subsequent manifests or
ordinary protocol completion to settle.

A byte sequence would not be considered one complete request if recognizing it requires joining
predecessor observations through M with successor observations after an omitting M. The omission
intentionally breaks that continuity. Under the proxy contract, such a cross-boundary sequence
cannot represent a mutating request that the proxy allowed to execute at the source while still
owing replay to the target.

This segmenting is internal replayer state. It is not a proxy epoch and must not appear as another
wire protocol identity.

Terminal self completion is offset-scoped. Scan-ahead may install cutoff K immediately, but it may
settle only the particular blocker for which the scanner covered every intervening offset through K
and found no required follow-up. Other known accumulations wait. Traffic and manifests from that
proxy activation and partition at offsets below K remain valid when normal replay later reaches
them. When the replay cursor reaches K, every lower offset has been processed, so remaining
incomplete state for that proxy-partition may settle and the identity becomes fully retired. Any
traffic or manifest record above K is the protocol violation; only a duplicate self
`NoMoreWrites` is idempotent. Discovering K early does not turn earlier records into
post-completion traffic.

### 10.4 Verdicts enter through the normal control loop

The scanner does not call into mutable assembler state reentrantly. On the Kafka source actor it
creates a typed `SourceControlEvent.ManifestCycleResolved` or
`SourceControlEvent.TerminalSelfCompletion` and delivers it through the same ordered source-batch
channel used for traffic records. The explicit message boundary preserves ordering and keeps
source-I/O callbacks from mutating source-assembly state directly. `SourceAssembler`, on replay
intake, applies the event to the matching generation and emits source outcomes or accumulator-reset
results to the owning transaction or connection coordinator.

This preserves the accumulator's single-threaded contract and — more valuably — provides **one
ordering point** for five things that would otherwise race:

* real source observations,
* captured closes,
* scanner-delivered manifest-cycle decisions or terminal self completion,
* partition-reassignment interruption,
* shutdown.

### 10.5 Expiration policy

| Cause | Commit authority or observation | Commit eligible? | Required action |
| --- | --- | --- | --- |
| Complete request/response | Captured observations | Yes | Finish transaction and evidence requirements; later liveness facts cannot discard it |
| Captured close with incomplete request | Captured close plus explicit close policy | Policy must be explicit | Settle only the incomplete accumulator; do not claim replay success |
| Manifest M lists C | Complete exact manifest-cycle decision | No reset | Resolve continuity across M and merge predecessor/successor segments |
| Manifest M omits C | Complete exact manifest-cycle decision | Yes, for predecessor only | Dispose the incomplete predecessor segment through M; initialize empty state and preserve/re-feed observations after M |
| Terminal self `NoMoreWrites` | Valid self completion after proxy barrier | Yes, for already-known incomplete state | Settle incomplete state for that proxy-partition and permanently retire the identity |
| Follow-up found before a cycle decision | Scan metadata | No terminal decision | Preserve it in the applicable segment |
| Scan inconclusive | Incomplete or ambiguous manifest information | No | Continue or halt according to resource policy |
| Elapsed-time expiration | Silence or a configured duration | **No** | Diagnostic and alarm only; retain Kafka obligations |
| Partition reassignment | Ownership lost | No | Abort old generation and redeliver |
| Shutdown | Process runway ended | No | Abort and retain |
| Replayer wall-clock age | Elapsed time since the replayer noticed a blocker | **Never** | Diagnostic only |

The elapsed-time rows are hard rules. A watchdog can fire while a delayed suffix still exists, and
Kafka broker time can jump forward. Neither wall-clock waiting nor `LogAppendTime` progression
fences the old producer. If a proxy dies with connections open and emits neither an applicable
manifest nor self `NoMoreWrites`, incomplete state remains retained.

Finite legacy sources may continue to use their configured inactivity timeout. They retain their
distinct `LegacyExpired` outcome. That source-specific policy must not be reused for Kafka capture.

### 10.6 Epsilon lookahead

Lookahead is an optional smoothing margin rather than the expiry mechanism. The intended default is
approximately 30 seconds, subject to measurement; disabling it must not change any disposition.

`ReplayProgressController` tracks admitted replay work and advances a contiguous settled source-time
watermark. Reads are allowed up to:

```text
settledReplayWatermark + epsilon
```

When no replay work is outstanding, the watermark may advance toward the replay clock. An unsettled
target request prevents idle advancement past the relevant work frontier. This coupling reduces
read-ahead, but it is not by itself an exact memory bound: hard byte, record, and owned-resource
budgets provide that bound. A partial source request that has never become replay work can be
resolved when normal replay reaches its follow-up or manifest, or sooner through the optional
metadata scanner.

Removing the existing `isWorkOutstanding()` coupling without an equivalent low-watermark rule is not
permitted. Read-ahead bounded only by the replay clock is unbounded in exactly the scenario where
bounding matters most: a stalled target. Scanner budget exhaustion pauses or retains; it never
changes the eventual disposition.

### 10.7 Capture-side duration cap

The capture proxy should optionally enforce a maximum connection duration. When the cap fires, it
requests the same idempotent capture-close path used by ordinary channel teardown: write exactly one
real close observation, submit the final traffic record, then close the channel. It must not call
`addCloseEvent` independently and then trigger a second close from `channelUnregistered`.

The cap has two useful jobs. It bounds ordinary long-lived resource ownership, and in the common case
it turns a connection that would otherwise linger into a normal captured close that needs no scanner
verdict. It is **not** an absence proof and is not combined with `connectionTimeout` to manufacture
one. A paused event loop may run its timer late, and a stalled producer may append previously captured
records later; neither fact is changed by configuration arithmetic.

The cap is therefore recommended and operator-configurable, but it is not mandatory for safety. Its
absence affects ordinary proxy resource bounds and how often a silent-proxy blocker appears, not
whether a manifest-cycle decision is valid.

### 10.8 Proxy open-connection manifests and proxy completion

**The problem.** Kafka append order can differ from proxy-side lifecycle order because connection
events, packet capture, and periodic manifest preparation run concurrently. The replayer therefore
needs an explicit logical boundary that says which connection lifecycle interval a complete
manifest covers. Kafka offsets alone cannot supply that boundary.

The authoritative proxy contract is
[`proxyHorizontalScalingAndNodeDeath.md`](proxyHorizontalScalingAndNodeDeath.md). Membership assigns
partitions for new captured client connections. Group departure triggers rebalance and has no replay
meaning. Exact manifests resolve `manifestCycle` boundaries. A listing preserves continuity across
the boundary; an omission resets only the covered incomplete predecessor accumulation.

`NoMoreWrites{writerNodeId, partition, emitterNodeId}` is valid only when
`emitterNodeId == writerNodeId`. It is emitted only for permanent proxy-partition retirement,
after the partition publisher moves one-way from `OPEN` to `RETIRING`, new traffic and
ordinary manifests are rejected, the periodic manifest publisher is asynchronously quiescent,
every related Netty connection and teardown future has settled, every previously accepted send
succeeds, the exact registry is empty, and a final empty manifest is acknowledged. Only the
retirement barrier may submit that final manifest and `NoMoreWrites` while `RETIRING`. Acknowledged
completion moves the publisher to `RETIRED`, which rejects every later submission.
`NoMoreWrites` promptly settles prior incomplete state and terminally retires that
proxy-partition identity.

Ordinary assignment drain uses the acknowledged empty manifest and does not emit
`NoMoreWrites`, so the same live proxy activation may later reacquire the partition after acknowledging a new
initial manifest. The `NoMoreWrites` Kafka offset is its terminal cutoff. Traffic and manifests
below that offset remain valid even when scan-ahead discovered completion first. Every traffic or
manifest record above the cutoff halts and alarms as a protocol violation; it is neither silently
discarded nor treated as a new interval. Only duplicate self completion is idempotent.
Peer-emitted completion is ignored for settlement and alarmed as invalid provenance.

A hard crash may emit neither a final manifest nor self `NoMoreWrites`. In that case §10.5's
retention rule applies: no other proxy fabricates completion, and elapsed time does not make the
old producer fenced. The affected incomplete obligations remain retained unless an applicable
complete manifest had already been published.

The proxy's stronger end-to-end ordering is still essential:

```text
SourceApplied(Q)
    implies
Kafka acknowledged the complete replay representation of Q
```

After complete Kafka acknowledgement and immediately before source execution, strict-mode proxies
also require a recent acknowledged manifest. If a suspended process resumes after the stale
threshold, it may finish Kafka capture but cannot newly complete the request at the source. A
request whose source-forwarding operation had already passed the gate was completely captured
first.

This guarantee is scoped to source handlers that do not apply effects before receiving the complete
HTTP request. HTTP chunked transfer encoding is compatible when it is only wire framing and the
source waits for end-of-message. Handlers that mutate while consuming an incomplete body remain
unsupported in this redesign; supporting them requires complete-request buffering or spooling
before any effect-causing source bytes are released.

The current round also retains the proxy's existing HTTP-method predicate for deciding which
requests require the complete-request barrier. Source-specific classification, an explicit
read-only allowlist, and default-mutating treatment of unknown requests are future hardening work.

**What is emitted.** Every `manifestInterval` (default 30s), each proxy records **all** open captured
client connections whose traffic routes to each relevant partition. Every
reconstruction-relevant observation also carries the current `manifestCycle` for that proxy
activation and partition.

The manifest is chunked before serialization:

```proto
message TrafficRecord {
  string writerNodeId = 1;
  bytes connectionId = 2;
  int32 partition = 3;
  int64 manifestCycle = 4;
  repeated TrafficObservation observations = 5;
}

message TrafficObservation {
  int64 connectionObservationSequence = 1;
  // existing observation fields follow
}

message ProxyOpenConnectionManifestChunk {
  string writerNodeId = 1;
  int32 partition = 2;
  int64 manifestCycle = 3;
  int32 chunkIndex = 4;
  int32 chunkCount = 5;
  int64 emittedAtMillis = 6;       // diagnostic only
  repeated bytes openConnections = 7;
}
```

Every `TrafficRecord` is homogeneous in
`(writerNodeId, partition, connectionId, manifestCycle)`. A batching layer flushes before any of
those values changes. This is load-bearing because Kafka offsets are indivisible: an omitting
manifest may authorize committing predecessor records while successor records must remain retained.
The protocol does not introduce sub-offset obligations.

`connectionObservationSequence` is monotonically increasing for one captured client connection and
validates the order of its reconstruction-relevant observations. The proxy submits those records
through one connection-local chain, so the replayer consumes them in contiguous sequence order even
though unrelated connection records and manifests interleave. `SourceAssembler` tracks
`nextExpectedConnectionObservationSequence`; a gap, regression, or conflicting duplicate halts and
retains rather than buffering indefinitely or guessing. Incidental diagnostics that cannot affect
HTTP reconstruction are not part of this sequence.

The replayer validates and reconstructs complete manifests into its `ProxyManifestIndex`. The
index is derived replayer state; it is not the proxy registry and does not independently observe
whether connections are alive.

An empty set still emits one chunk. The scanner may use a manifest only after receiving every chunk
exactly once and validating a consistent header. A missing, duplicate, oversized, or contradictory
chunk makes that manifest unusable; it can never be interpreted as an empty manifest. Chunking is
required because inbound frontside connections are not bounded by one host's ephemeral-port range,
so no fixed connection-count estimate proves that one record fits under Kafka's pre-compression size
limit. `manifestCycle` increases monotonically per `(writerNodeId, partition)`, and `chunkIndex`
covers exactly `0..chunkCount-1`. `emittedAtMillis` and captured observation timestamps remain
diagnostic or pacing values; they do not participate in manifest-cycle decisions.

**The registry is exact and the lifecycle boundary is narrow.** For each partition, the proxy
linearizes only:

1. adding a newly created captured client connection to the exact active set;
2. removing a captured client connection from that set; and
3. copying the active set for manifest M and incrementing the partition's atomic counter from M to
   M+1.

Addition precedes acceptance of the connection's first reconstruction-relevant observation.
Removal first atomically closes the connection's one-way source-forward gate, then waits until every
previously authorized source operation has a complete acknowledged Kafka representation, and only
then removes the connection under the partition lifecycle boundary. A manifest prepared while that
wait is in progress still lists the connection.

Ordinary packet capture does not take this boundary. Each reconstruction-relevant observation reads
the current counter and carries it to Kafka. The manifest is published after the copy-and-increment
operation and may be appended before or after concurrent observations.

Before removing C, the proxy guarantees that every request on C that was allowed to execute at the
source already has a complete acknowledged Kafka representation. This does not require every packet
to have been published before removal. A late observation may still appear, but it cannot represent
a source-executed request still owed to the target.

Complete manifests are prepared and acknowledged through a non-overlapping per-partition chain. The
proxy does not prepare or submit cycle M+1 until every chunk of M is acknowledged. Cycle regression
or overlapping publication is a protocol failure.

**Kafka submission order is not the manifest-cycle proof.** Producer ordering still protects
records submitted in a known order and is mandatory for terminal self `NoMoreWrites`, but the
replayer must tolerate a manifest and concurrent observations appearing in either Kafka order. The
producer enforces `enable.idempotence=true`, `acks=all`, and
   `max.in.flight.requests.per.connection<=5`. Configuration cannot weaken those correctness
   settings, and startup fails closed if the effective settings cannot preserve same-partition
   append order across retries.

A synchronous or asynchronous send failure closes the proxy capture gate and fails capture closed.
It does not continue emitting authoritative manifests or terminal self completion after publication
outcomes become ambiguous.

The resulting rule is logical rather than offset-based:

> Complete manifest M omitting C resets the incomplete predecessor accumulation through cycle M.
> Observations with cycle greater than M belong to a successor accumulation and remain obligations.

The required inversion cases are:

- an open observation accepted before M but appended after M is continued if M lists C;
- if C was also removed before M, the same delayed observation is predecessor data because its
  cycle is at most M; and
- an open observation accepted after M's lifecycle boundary but appended before M is safe because
  its cycle is greater than M.

**Structural requirements this places on the rest of the design.**

| Requirement | Why |
| --- | --- |
| One immutable routing decision is shared by traffic, registry, and manifest publishing for each connection | Computing routing independently can split one connection across partitions while every individual record still looks valid |
| A proxy writes traffic and manifests for one connection to the same explicit partition | The replayer must compare an observation's cycle with the complete manifests for the same proxy activation and partition |
| The explicit partition is stamped in every traffic record and manifest chunk and asserted on read | A mismatch invalidates the record and halts loudly; validation detects routing bugs instead of trusting that publishers used the same helper |
| Every reconstruction-relevant observation is stamped with `manifestCycle` | Kafka order alone cannot distinguish an observation accepted before the manifest boundary from one accepted after it |
| Add, remove, and manifest copy-and-increment share one partition-local lifecycle boundary | A manifest must be an exact statement about the active set at one logical point |
| Observations after an unresolved boundary remain separable from the predecessor accumulator | An omission must not commit a newer suffix that Kafka delivered first |
| Manifest batches are complete and size-bounded before submission | A truncated manifest must never look like an empty one |
| Manifest chunks do not create replay accumulations or long-lived record obligations | When encountered by the replay cursor they are immediately marked settled, subject to the partition's ordinary contiguous commit low-watermark; scan-cursor decoding remains read-only |

Kafka metadata discovery and producer qualification happen in `PROBING` before the process joins
the group. The proxy writes semantically inert capability probes to one representative traffic
partition per current leader broker, waits for acknowledgement, refreshes metadata, and then joins
as `PROBATIONARY`. It accepts no captured connections until an assignment promotes it to `ACTIVE`.
A probe creates no replay work and does not affect manifest-cycle interpretation.

For each newly accepted connection, the current group assignment chooses one traffic partition.
The proxy stores that choice and uses it for every traffic record and manifest entry for the life of
the connection. Later rebalances move only eligibility to accept new captured client connections;
existing connections drain on their stored partitions. Topic recreation or loss of a stored
partition fails capture rather than silently selecting a different partition.

**Why `writerNodeId` is per capture activation.** A stable per-host identity would let a replacement
proxy's manifest affect its predecessor's state. That is unsafe: the predecessor may only be
stalled and may later flush records. Every capture activation, including one started by the same
operating-system process after capture abandonment, therefore receives a fresh UUID. The identity
does **not** fence its Kafka producer; that is why group departure and silence have no replay
meaning, and why elapsed-time expiration remains non-committing.

---

## 11. Connection Actor

**The problem.** Per-connection ordering is currently reconstructed *after* transformation by a
sorter, alongside a separate due-time schedule map, a separate transformation-timer collection, a
volatile cancellation flag, and a close-callback graph. Each is state with its own drain and
cancellation semantics, and F4 is what happens when one of them is cleared without settling.

**The mechanism.** One actor per connection session owns all of it.

### 11.1 Command model

```java
sealed interface ConnectionCommand permits ReplayRequest, CloseConnection {}
```

Conceptually:

```java
record ReplayRequest(
    ReplayRequestId id,
    Instant scheduledStart,
    CompletionStage<PreparationOutcome> preparation,
    CompletableFuture<TargetOutcome> completion
) implements ConnectionCommand {}

record CloseConnection(
    ConnectionSessionKey session,
    Instant scheduledStart,
    CloseReason reason,
    CompletableFuture<SessionOutcome> completion
) implements ConnectionCommand {}
```

Commands are admitted from the serialized source path **before** limiter acquisition or
transformation. Admission creates or finds the session's `ConnectionRuntime` and enqueues the command
onto its assigned Netty event loop. **This is the key change that makes a plain FIFO sufficient.**

### 11.2 Actor state

```text
OPEN
  -> command queued
  -> head waiting for preparation and scheduled time
  -> ACTIVE
  -> head settled
  -> next command
  -> ORDERED_CLOSE
  -> TERMINATED

OPEN / ACTIVE
  -> ABORTING
  -> queued commands cancelled
  -> active exchange abort requested
  -> active exchange cleanup joined
  -> channel closed
  -> source acknowledged
  -> TERMINATED
```

The actor owns:

* the FIFO command deque,
* **one** head timer,
* **one** active target exchange,
* channel creation and reconnection policy,
* the cancellation token,
* the target-exchange abort and cleanup gate,
* the session close acknowledgement,
* the termination completion gate.

Every method that reads or mutates this state must run on the runtime's assigned Netty event loop.

There is no independent sorter, schedule map, cancellation marker, or close-callback graph. Those are
not hardened — they are **deleted**.

### 11.3 Ordering and asynchronous preparation

Preparation may run concurrently across requests and connections, so a later command may become ready
first. The actor examines only the head command. That single rule preserves ordering without
re-sorting completed preparation callbacks, and it is why the sorter can go.

The actor never blocks its Netty thread. It reacts when the head's preparation future posts a
`Prepared` message, or when its scheduled timer fires.

### 11.4 Failure behavior

* Ordinary target failure settles the request with `TargetOutcome.Failed`. Policy is decided by the
  **transaction**; the actor does not advance as though it succeeded.
* Session abort settles queued commands as cancelled **without invoking their send callbacks** — a
  cancelled command must never look like it ran.
* An active exchange is explicitly aborted and its owned cleanup joined before session termination
  completes. Channel close is one child of that cleanup, not a substitute for it.
* Late preparation or target callbacks observe terminal session state, release their own resources,
  and do not restart the session.

### 11.5 Active target-exchange abort contract

`TargetExchange` is the adapter between actor lifecycle and the existing Netty/request machinery. It
owns the wrapper result returned to the actor and a separate cleanup gate:

```java
interface TargetExchange<P, R> {
    CompletionStage<TargetOutcome<R>> execute(P request);
    CompletionStage<Void> close();
    CompletionStage<Void> abort(CancellationException cause);
}
```

Successful `abort` completion proves all of the following:

1. The owner-controlled `execute` result is terminal exactly once, normally as
   `TargetOutcome.Cancelled` carrying the original cause.
2. No new pacing timer, retry delay, channel acquisition, packet send, response decode, or response
   finalization for that exchange can start.
3. Every scheduled timer is cancelled and the future or mailbox obligation depending on it is
   settled. Cancelling only the scheduler handle is insufficient.
4. An active packet receiver/decoder/finalizer has received the cancellation signal and released its
   target request/response contexts. Its cancellation path must not depend on receiving another byte
   or a normal end-of-response callback.
5. Channel acquisition and close are accounted for. A channel that arrives after abort is immediately
   closed and cannot be installed into the runtime.
6. Attempt payloads, response buffers, tracing scopes, and other exchange-owned resources are closed
   exactly once.
7. Any uncancellable foreign callback is fenced by the exchange identity and generation. It may
   perform only self-cleanup when it eventually runs; the session gate does not wait for a semantic
   result that the adapter has already replaced with cancellation.

The exchange also owns the target-response timeout. That timeout begins only after the complete
request has been handed to the target channel and the exchange starts waiting for a response. Channel
acquisition, replay pacing, transformation, and time between request fragments do not consume the
target's response budget. Once response waiting begins, decoded response activity may refresh the
inactivity timer, while cancellation settles the timer as part of the exchange's owned cleanup.

`abort` and `close` are idempotent. Repeated calls join the same cleanup rather than starting another
teardown. This contract is what makes a never-completing response finalizer a test case instead of a
permanent session drain.

---

## 12. Asynchronous Permit Pool

**The problem.** The concurrency limiter hands work to a dedicated `requestFeederThread`, which is what
blocks on the semaphore — intake itself does not block; `queueWork` only `offer`s onto an unbounded
queue. So the cost is not a stalled intake thread but three other things: a queued acquisition is not
addressable, so "cancel this request's pending acquisition" cannot be expressed at all; `close()`
interrupts the feeder and leaves every queued `WorkItem` stranded, its task never invoked and its
waiters never settled (audit row R2, and one of the ways a shutdown fails to be a shutdown); and the
unbounded queue means backpressure shows up as memory rather than as refusal.

`AsyncPermitPool` is the replacement for `TrafficStreamLimiter`, not a wrapper, subclass, or second
limiter. It preserves the weighted-capacity policy while replacing the feeder thread, blocking
semaphore acquisition, anonymous `WorkItem`, and callback-based release with an addressable waiter and
an explicitly owned permit. The first implementation currently charges one unit per replay request;
the `cost` field preserves the existing weighted-policy option.

**The mechanism.** A lease expressed as a future:

```java
interface AsyncPermitPool {
    CompletionStage<Permit> acquire(ReplayRequestId requestId, int cost);
}

interface Permit extends AutoCloseable {
    @Override
    void close();
}
```

Requirements:

* Fair FIFO acquisition unless an explicit policy says otherwise.
* Queued acquisition can be cancelled by request, session, partition, or shutdown.
* Pool shutdown settles **every** queued acquisition exceptionally.
* Permit release is idempotence-guarded and owned by `ReplayTransaction`.
* Queue mutation occurs on the replay-intake owner; cross-thread release posts a `PermitReleased`
  event to that owner.
* No dedicated feeder thread, and no bare callback accepting a `WorkItem`.

**Why.** A cancellable future makes "cancel this request's queued acquisition" expressible at all,
which an anonymous queue entry behind a blocking `acquire` does not. Making the permit `AutoCloseable`
and transaction-owned turns "the permit gets released somewhere in a `whenComplete`" into a single
named owner. Removing the feeder thread is a secondary benefit — its real defect is that its queue has
no settlement contract, not that it exists.

---

## 13. Replay Transaction

**The problem.** A request's concerns are spread across several owners: the limiter releases the
permit, the orchestrator releases a temporary buffer retain, a tracker holds the join future, tuple
packaging closes some contexts, and a commit helper may or may not be reached. Audit rows R13–R15 are
all the same story — the last link breaks and nothing owns the decision.

### 13.1 Responsibilities

A `ReplayTransaction` owns:

* source request and response state,
* references to its ledger-owned traffic-stream record claims,
* the permit lease,
* transformed-request ownership,
* the target outcome,
* the tuple/evidence outcome,
* the latest monotonic observation of generation-scoped runway,
* tracing contexts,
* and its single terminal disposition proposal.

The transaction does not close Kafka record contexts or advance offsets. `RecordDispositionLedger`
owns those obligations and applies the accepted terminal proposal exactly once. All transaction
transitions execute on the same assigned Netty event loop as the owning connection actor. Source,
preparation, evidence, runway-loss, and abort inputs arriving from other threads are mailbox
messages; target outcomes are delivered directly on that event loop.

### 13.2 State model

```mermaid
stateDiagram-v2
    direction TB
    state "Normal progression" as RUNNING {
        [*] --> ADMITTED
        ADMITTED --> PREPARING
        PREPARING --> READY
        READY --> TARGET_ACTIVE
        TARGET_ACTIVE --> WAITING_FOR_JOIN
        ADMITTED --> WAITING_FOR_JOIN: target not required
        WAITING_FOR_JOIN --> WRITING_EVIDENCE: all required outcomes settled
    }
    RUNNING --> DRAINING: runway lost in any normal state
    WRITING_EVIDENCE --> DISPOSING
    DRAINING --> DISPOSING: outcomes and owned child cleanup settled
    DISPOSING --> TERMINATED
    note right of DISPOSING
        Runway lost while DISPOSING stays DISPOSING.
        KafkaSourceActor's ordering of authoritative runway
        revocation vs. source acceptance decides the commit.
    end note
```

Two facts are deliberately *not* linear states:

* **Source settlement is orthogonal.** It may occur before, during, or after target work. The guard on
  `WAITING_FOR_JOIN -> WRITING_EVIDENCE` requires every outcome needed by the request's policy to be
  terminal, including source completion, target completion or explicit target omission, and any
  required preparation result.
* **Runway is orthogonal.** Reassignment or shutdown may arrive in any phase, including while evidence
  or disposition is in flight. It does not overwrite a terminal source or target outcome. It moves
  unfinished work through `DRAINING`, where cancellable children are actively settled and
  uncancellable children are joined or failed loudly before disposition. If disposition has already
  been submitted, the transaction remains `DISPOSING`; `KafkaSourceActor`'s ordering of
  authoritative runway revocation versus source acceptance decides whether the commit was accepted.

The invariant that matters: **`DISPOSING` is reached once and only once, from every path.**
Cancellation does not bypass it — it drains into it.

### 13.3 Outcomes

```java
sealed interface TargetOutcome {
    record Succeeded(...) implements TargetOutcome {}
    record Failed(...) implements TargetOutcome {}
    record Cancelled(CancellationReason reason) implements TargetOutcome {}
    record Filtered(...) implements TargetOutcome {}
}

sealed interface SourceOutcome {
    record Complete(...) implements SourceOutcome {}
    record CapturedClose(...) implements SourceOutcome {}
    record ManifestCycleReset(ScanEvidence evidence) implements SourceOutcome {}
    record TerminalProxyCompletion(ScanEvidence evidence) implements SourceOutcome {}
    record LegacyExpired(...) implements SourceOutcome {}
    record Interrupted(...) implements SourceOutcome {}
    record Shutdown(...) implements SourceOutcome {}
}

sealed interface EvidenceOutcome {
    record Durable(...) implements EvidenceOutcome {}
    record Failed(...) implements EvidenceOutcome {}
    record NotRequired(...) implements EvidenceOutcome {}
}

sealed interface RunwayObservation {
    record Available(int sourceGeneration) implements RunwayObservation {}
    record Lost(int sourceGeneration, RunwayLossReason reason) implements RunwayObservation {}
}
```

Visitors or exhaustive switches must handle every subtype. This is where `Cancelled` stops being a
gap: it is a value the disposition matrix must have a row for, and the compiler forces every new
outcome to be considered everywhere. `RunwayObservation` is different: it is monotonic local state,
not a replacement outcome or the authoritative Kafka generation fence. Its only transition is
`Available -> Lost`, and the transaction mailbox serializes that transition with entry into
disposition. The ledger validates its monotonic observed runway before forwarding a commit proposal,
and `KafkaSourceActor` makes the authoritative generation check when it accepts or rejects the
commit command.

`SourceOutcome` is also where the overloaded-status problem is fixed — but the problem is narrower than
"today everything collapses into one status," so it is worth stating exactly.
`ReconstructionStatus` already distinguishes `CLOSED_PREMATURELY` from
`TRAFFIC_SOURCE_READER_INTERRUPTED`, and the commit path already suppresses both. The three real gaps:

* **Manifest-cycle reset is not represented precisely.** `EXPIRED_PREMATURELY` does not say which
  manifest cycle authorizes disposing which predecessor obligations, and it cannot preserve a
  newer successor segment. `ManifestCycleReset` carries that exact decision.
* **Elapsed-time expiration must not reach Kafka commit policy.** Existing `ConfiguredExpired`
  production paths are migration residue to disable or remove for Kafka input.
* **Legacy finite sources still need an honest timeout value.** They have no durable offset to retain,
  but calling a timed-out reconstruction `Complete` would make the model lie. `LegacyExpired` preserves
  their existing local release behavior while making the compatibility boundary exhaustive and
  preventing that outcome from authorizing a Kafka commit.
* **Shutdown has no value of its own.** It currently arrives as reader-interruption, which happens to
  suppress commits and therefore happens to be safe — a correct outcome reached by coincidence of
  another cause's policy rather than by stating it.

Those source values remain useful because they describe how reconstruction ended. They do not replace
runway state: a source-complete request may still lose commit authority while evidence is being
written, and a target-success outcome must not be rewritten as cancellation merely to make the
disposition policy retain it.

---

## 14. Record Disposition

**The problem.** Committing is currently inferred from a status plus a boolean at a site that can be
skipped, and closing a record's context is entangled with committing its offset. F1 and F2 are both
failures of that arrangement.

### 14.1 Record obligations

Each accepted Kafka record creates one ledger-owned `RecordObligation`. An observation consumer—a
source accumulation, transaction, or explicit discard path—registers a `RecordClaim` before it can
depend on that record. Claims are explicit and attributable. A record containing observations for
more than one request may therefore have more than one claim without giving the Kafka offset more
than one owner.

The claim-registration lifecycle is:

```text
OPEN_FOR_CLAIMS
  -> root assembler claim registered
  -> root claim may transfer or split into leaf claims
  -> CLAIMS_SEALED
  -> every leaf claim has one proposal
  -> record disposition reduced exactly once
```

- At record delivery, `SourceAssembler` receives one root claim covering every observation in that
  record.
- Transfer moves that same claim from an accumulation to one transaction; it neither settles the
  claim nor creates a gap.
- If observations in the record feed multiple work owners, replay intake atomically replaces the
  root with a known set of child claims before any child can settle.
- After every observation has been assigned, replay intake seals the obligation. No later claim may
  be registered.
- The ledger may reduce proposals, close record contexts, or send a Kafka commit only after sealing
  and after every leaf claim is terminal.

All split, transfer, and seal operations execute on replay intake. Event-loop transactions hold
only immutable claim IDs and return proposals.

```java
sealed interface RecordDisposition {
    record Commit(CommitReason reason) implements RecordDisposition {}
    record Retain(RetainReason reason) implements RecordDisposition {}
}
```

There is no nullable or boolean disposition. Both variants carry a *reason*, which is what makes "why
did this commit?" answerable from metrics.

The ledger reduces claim proposals mechanically:

- every claim `Commit` permits the record obligation to propose commit;
- any claim `Retain` makes the record obligation retained; and
- a missing claim decision leaves the record unresolved.

This reduction is not a second replay policy engine. The transaction or accumulation computes each
claim proposal from its own outcomes; the ledger only joins claims for the indivisible Kafka offset.

### 14.2 Decision matrix

Runway is evaluated first. The transaction and ledger observations control early rejection and
draining, but `KafkaSourceActor` owns the authoritative generation state at commit acceptance. A
lost-runway row supersedes the source/target/evidence rows below it.

`ReplayDispositionPolicy` is a pure exhaustive function invoked exactly once by the transaction on
its owner event loop. Its result is the transaction's typed `Commit` or `Retain` proposal. The
ledger validates that the proposal references obligations it owns, has not already been applied,
and is not already vetoed by observed runway loss. It never recomputes the matrix from outcomes.

A complete captured request leaves the liveness accumulator as soon as it is recognized. It may be
sent to the target without waiting for a complete captured source response or connection close.
Evidence must represent a complete, incomplete, or absent source response explicitly; missing
response data is not permission to discard the request. Later manifest-cycle decisions affect only
the remaining incomplete connection accumulator.

| Runway state | Source outcome | Target outcome | Evidence outcome | Disposition |
| --- | --- | --- | --- | --- |
| Lost by reassignment before source acceptance | Any | Any | Any | Retain |
| Lost by shutdown before source acceptance | Any | Any | Any | Retain |
| Available | Complete request; source response complete, incomplete, or absent | Succeeded | Durable evidence encodes the source-response state | Commit |
| Available | Captured close with incomplete request | Not sent | Not required; captured close is durable | Commit as incomplete-capture discard under the non-streaming source contract |
| Available | Manifest-cycle reset of incomplete predecessor | Not sent | Not required; complete applicable manifest present | Commit only predecessor obligations through that cycle |
| Available | Terminal self completion with incomplete state | Not sent | Not required; valid self `NoMoreWrites` present | Commit the already-known incomplete obligations for that proxy-partition |
| Available | Elapsed-time expiration or silence | Not sent | None | Retain and alarm |
| Available | Captured explicit drop/ignore | Not sent | Durable discard evidence | Commit as deliberate discard |
| Available | Deterministic poison | Failed | Durable classified-skip evidence | Commit only when configured |
| Available | Transient failure | Failed | Any | Retain and halt after retry exhaustion |
| Available | Tuple/evidence failure | Any | Failed | Retain and halt |
| Any | Unknown combination | Any | Any | **Retain and halt** |

Properties to internalize:

- **Context closure happens for both `Commit` and `Retain`.** Kafka commit happens only for `Commit`.
  Separating the two actions is the point; conflating them is how retained records leaked open
  contexts.
- **The default is fail-closed.** An unrecognized combination retains and halts loudly. A failure
  that forces a human to look is strictly better than a silent skip.
- **Runway loss is not represented by rewriting outcomes.** A request may legitimately retain
  `SourceOutcome.Complete`, `TargetOutcome.Succeeded`, and even durable evidence while still being
  retained because reassignment arrived before the source accepted its commit.
- **Source acceptance is the linearization point.** A transaction's `Commit` disposition is only a
  proposal. On the source-I/O owner, `KafkaSourceActor` checks the generation and registers the
  offset as pending. Runway revocation and this source acceptance are therefore serialized by that
  owner:
  - if revocation runs first, the proposal becomes `Retain` and no commit is registered;
  - if source acceptance runs first, `KafkaSourceActor` owns the pending broker commit until
    acknowledgement or explicit failure; the ledger waits on that result and does not later relabel
    the obligation as `Retain`.
- **Broker acknowledgement is a later stage.** It may fail, including after a rebalance. Such a failure
  completes the lifecycle exceptionally; it does not retroactively claim that an already accepted
  commit was deliberately retained.

Failure classification cannot be judged at catch time, so it comes from retries plus an
operator-declared poison classifier — see §19.1.

A manifest-cycle reset has no replay result to preserve for the incomplete predecessor. The
complete manifest and cycle are the fact authorizing that limited commit. Emit a reason-coded metric
and a trace/debug diagnostic containing the proxy activation, partition, connection, manifest
cycle, and affected obligation range. Do not expand `EvidenceWriter` merely to persist an empty
result.

The captured-close row relies on the same supported-source boundary as capture-before-forward: a
source handler cannot apply a mutating request before receiving the complete request. If a
deployment has true streaming mutation semantics, this discard is not safe and that deployment is
outside the protocol.

### 14.3 Disposition ledger

`RecordDispositionLedger`:

1. Accepts record obligations.
2. Registers, splits, transfers, and seals every dependent `RecordClaim` under the lifecycle above.
3. **Rejects duplicate claim disposition and duplicate record disposition** — this is F2,
   structurally prevented.
4. Closes record and traffic-stream contexts exactly once.
5. Validates and applies each owner's one typed claim proposal without recomputing policy, then
   mechanically reduces all claims for the record.
6. Sends accepted commit proposals to `KafkaSourceActor` and joins its acceptance result.
7. Tracks source-accepted records and joins the broker
   acknowledgement.
8. Rejects a commit when its observed generation is already lost or stale before submission;
   `KafkaSourceActor` repeats the authoritative check.
9. Closes retained records' process-local contexts without advancing the Kafka commit watermark.
   The retained offset continues to block every later offset in that partition from being committed
   past it.
10. Exposes unresolved obligations for shutdown and diagnostics.

The existing `OffsetLifecycleTracker` may remain behind the commit adapter initially.

---

## 15. Resource Ownership

**The problem.** Three reference-counted lifetimes are currently conflated: the transformer returns a
producer with refcount 1 that **nobody releases**; the scheduler retains and releases only its own
extra share; and each `get()` may return a shared list or a fresh one depending on the producer.
Signing plus retries therefore leaks per attempt. The diagnostic copy is released only if tuple
packaging is reached.

**The mechanism.** Explicit handles instead of shared refcounts:

```java
interface OwnedPreparedRequest extends AutoCloseable {
    AttemptPayload newAttempt();
    DiagnosticPayload retainDiagnosticCopy();
}

interface AttemptPayload extends AutoCloseable {}
interface DiagnosticPayload extends AutoCloseable {}
```

Contract:

* Preparation transfers **one** `OwnedPreparedRequest` to the transaction.
* Every send attempt owns and closes **one** `AttemptPayload`.
* Tuple evidence owns and closes one diagnostic payload.
* Closing the transaction closes the prepared request exactly once.
* No component releases a child buffer owned by another live wrapper.

**Why handles rather than just adding the missing release.** With a shared refcount this cannot be
repaired incrementally: the trivial producer's list is simultaneously the producer's, the attempt's,
and the summary's, so any release you add is correct for one caller and wrong for another. Handles
make each transition a distinct object with a distinct owner, so "who releases this?" has exactly one
answer per handle.

The same ownership rule applies to tracing contexts, permits, timers, sink handles, and record
obligations. `TargetExchange` owns target request/response contexts, attempt payloads, response
finalization, and any adapter future that fences a foreign callback. `ReplayTransaction` owns the
prepared request, permit, references to ledger-owned record claims, evidence handle, and
transaction tracing scopes. A
resource must not be owned by both merely because both have a completion callback that can see it.

---

## 16. Rebalance and Shutdown

**The problem.** "Done" is not represented. `cancelConnection` returns a completed future while work
is in flight (F4 territory); the synthetic-close gate is a counter that a missing callback can leave
nonzero (F3); shutdown relies on process exit.

### 16.1 Rebalance

For each revoked partition:

1. Stop admitting new records from the old generation.
2. On the Kafka source-I/O owner, mark the old generation's authoritative runway lost. This is the
   linearization point after which any newly processed old-generation commit command is rejected.
   Enqueue one ordered `GenerationRevoked` event to replay intake.
3. Return from the Kafka rebalance callback after that bounded owner-thread work. The callback must
   not await replay intake, actor mailboxes, target cancellation, evidence, disposition, or channel
   closure. Waiting there can violate Kafka's poll contract and deadlock the very completions needed
   to drain.
4. Replay intake applies that one event idempotently and in order with source batches. It first
   marks the ledger's observed runway lost, then settles unfinished assembler state as
   `SourceOutcome.Interrupted`, then delivers `RunwayLost` to active transactions, and finally
   requests actor abort. It does not receive a second independently ordered interruption event.
   Correctness does not depend on mailbox delivery winning a race with commit submission because
   step 2 is authoritative.
5. Abort matching connection actors **by typed `ConnectionSessionKey`**.
6. Settle queued and active target work as reassignment cancellation, join exchange cleanup, and let
   every transaction drain to its disposition. Lost runway selects `Retain` unless the source had
   already accepted the commit.
7. Close target channels and remove actors from the registry.
8. Acknowledge **every** registered old-generation session — including an explicit acknowledgement
   for sessions that never existed, so absence is an *answer* rather than a missing callback. A
   session's acknowledgement comes after its transactions have dispositioned, per §6.4 rule 5; steps
   6 and 7 are children of the termination gate, not substitutes for it.
9. Continue short Kafka polls while the asynchronous drain runs. Records from a newly assigned
   generation may be polled or buffered under a hard bound, but replay intake does not deliver them
   until all old-generation termination gates complete successfully — which
   therefore means after every affected record has been dispositioned.
10. Before delivering the next generation, assert that old-generation actor, transaction, exchange,
   timer, permit, target-context, and in-memory source-obligation registries are empty. Deliberately
   retained Kafka records are not live in-memory obligations.
11. Do not commit unfinished old-generation obligations.

**No timeout is allowed to reset the drain gate and continue lossily.** A timeout may halt loudly. A
watchdog that discards records on a timer is impatience wearing a safety vest; when it eventually
fires it will be for an unrelated reason and it will cause a fresh incident.

### 16.2 Shutdown

Shutdown is a structured operation:

1. Stop source admission and scanner cycles.
2. Copy transaction and connection registries into immutable shutdown work lists.
3. On the Kafka source-I/O owner, revoke authoritative runway for every unfinished generation and
   deliver the matching shutdown events to replay intake.
4. Deliver `RunwayLost(SHUTDOWN)` to unfinished transactions.
5. Abort all actors.
6. Await their termination completion gates.
7. Finalize every transaction with retain/no-commit unless the source already accepted its commit.
8. Flush and acknowledge eligible Kafka commits.
9. Close Kafka, evidence sinks, transformation resources, and event loops.

Normal shutdown remains a reusable, testable completion-gate protocol. Unexpected event-loop death
is different: it invalidates the owner itself and is process-fatal.

### 16.3 Unexpected event-loop death terminates the replay process

A session's Netty event loop is simultaneously the channel's I/O thread and the actor mailbox
(§7.1, §8). That identity is load-bearing: it is what makes connection and transaction state
single-owner and lock-free. If the loop terminates while its session is live, no legal owner remains
that can advance the channel, actor, transaction, timer, or target exchange.

The required response is therefore:

1. detect termination through `EventLoop.terminationFuture()`;
2. atomically close the process-wide `ReplayFatalFence`. Its shared commit-acceptance critical
   section establishes whether a concurrent source commit was accepted before closure or rejected
   after it; a command merely queued before closure receives no authority;
3. post an immutable `InvalidateSession` command to replay intake, which remains the sole owner of
   the affinity registry. The termination callback does not mutate that registry directly;
4. emit an ERROR log and `replayFatalFailures{reason=event_loop_terminated}`;
5. invoke the required process-level fatal handler immediately without waiting for owner-thread
   cleanup;
6. stop source intake and issue no new commit commands;
7. begin bounded best-effort resource closure without making process termination depend on a dead
   mailbox; and
8. force non-successful process termination if bounded shutdown cannot finish.

A commit accepted by `KafkaSourceActor` before the fatal fence closed remains source-actor-owned and
may have an indeterminate broker outcome at process death. A commit processed after the fence closes
is rejected even if its command was queued earlier. This is the fatal-path linearization point.

This path does **not** transfer actor or transaction ownership to a cleanup thread and does not
synthesize successful lifecycle completion. Cross-thread structures such as
`terminateAfterMailboxLoss`, handoff/claim records, or alternate mutation paths are migration
residue from an attempted local-recovery policy and should be deleted. They expand the production
state machine precisely when its owner is already gone.

The fatal handler is a mandatory constructor dependency for production composition; there is no
log-only default. Tests inject a recording fatal handler and prove the observable contract:

- one fatal signal even if several sessions notice loop termination;
- the process fatal fence closes before the signal;
- a queued-but-unaccepted commit is rejected after the fence closes;
- registry invalidation occurs only on replay intake and is best-effort cleanup, not a precondition
  for signaling fatal;
- no new target or source work is admitted;
- no source offset becomes commit-eligible because of loop death;
- the top-level run terminates exceptionally; and
- a shutdown watchdog prevents the JVM from hanging indefinitely.

Expected event-loop termination after the owning session and normal shutdown gates have completed
is not fatal. The distinction is whether live owner state still exists when the loop terminates.
The durable outcome matches an OOM or hard process kill. Work whose commit was never accepted by the
source actor remains eligible for redelivery. For a commit already submitted to Kafka, process
death may leave the broker result indeterminate: the broker may have committed it even if the
process never observed the acknowledgement. The fatal path must not claim otherwise or attempt
cross-thread recovery; restart relies on Kafka's actual committed offset and the existing
at-least-once/idempotency behavior.

---

## 17. Evidence API and Phase 2 Compatibility

The first implementation keeps the public sink contract whole-tuple:

```java
interface EvidenceWriter {
    CompletionStage<EvidenceReceipt> writeTuple(ReplayRequestId id, ...);
}
```

The transaction may organize source request, source response, target exchange, and comparison as
internal parts, but the adapter produces one receipt and external sink implementors see no premature
four-receipt API. The obligation model must not assume that one receipt is permanent: a future
granular store may make one record depend on several independent receipts without moving disposition
policy back into sink callbacks.

**Why this matters now.** Today the commit waits for the source *response*, which can arrive minutes
after the request, holding the commit head far longer than necessary. Decoupling shrinks that window —
but it has a hard prerequisite: once request offsets commit before response offsets, a crash means
response records are re-delivered while request records are not. So restart must be able to:

* look up durable request evidence by `ReplayRequestId`,
* skip resending when request evidence already exists,
* reconstruct and write only the redelivered source response and required comparison.

That is why `FollowUpRequirement` distinguishes request completion, response completion, and
connection termination. It is also why this API belongs in the transaction rather than back in sink
callbacks: the commit policy must stay with the disposition owner.

---

## 18. Observability, Verification, Acceptance

### 18.1 Required state-machine metrics

| Area | Metrics |
| --- | --- |
| Source | replay position, settled watermark, epsilon utilization, records buffered |
| Scanner | scan distance, latency, bytes discarded, follow-up found, manifest listed, manifest omitted, terminal self completion, inconclusive |
| Actor | queued commands, head wait reason, active duration, abort duration, active-exchange phase, pending abort child, channel state |
| Transaction | count by phase, runway state/loss reason, terminal outcome, retry class, disposition reason |
| Permits | available, queued, held duration, cancellation count |
| Evidence | tuple-write latency, failures, retries, durable receipts |
| Kafka | unresolved obligations, commit head identity/age, staged commits, pending commit acknowledgements by generation, commit latency |
| Capture proxy | membership phase, active member count, capture-gate state, open connections, acknowledged-manifest age by partition, manifest cycle, manifest chunks/bytes, incomplete manifests, publisher failures, capture-abandoned transitions, pass-through gap alarms |
| Resources | owned buffer counts/bytes, duplicate-close attempts, leaked-owner assertions |

Here, **monitoring** means code that reports health without owning lifecycle decisions: OTel metric
exporters, heartbeat loggers, active-context diagnostics, and shutdown progress reporters. Such code
receives an **immutable diagnostic view** — a value object copied by the state owner containing
counts, identifiers, phases, and wait reasons. It does not receive live mutable maps, registries, or
queues and cannot call mutation methods. This diagnostic view is unrelated to a proxy
open-connection manifest; the overloaded word "snapshot" is intentionally avoided.

Two cautions. Diagnostic heartbeat output must not mutate or expire state. Exact proxy manifests are
different: they are source records that resolve manifest-cycle boundaries. Commit-head *age*
measured from insertion wall-clock remains a stall signal only; it never authorizes a commit.

### 18.2 Deterministic model tests

Use fake clocks, fake event loops, and manually controlled futures to enumerate:

Test collaborators should be small plain Java implementations of the design interfaces. Prefer
state-recording fakes and the OTel in-memory exporter for count/metric assertions. Mocking
frameworks are reserved for a narrow third-party interface that cannot reasonably be wrapped; they
must not stand in for lifecycle owners, completion gates, Kafka authority, actor mailboxes, or
resource ownership. If a required behavior is difficult to test without a deep mock graph, improve
the production interface rather than adding callback configuration to the test.

* every permutation of preparation, source completion, target completion, close, and abort;
* request/close admission order with out-of-order preparation;
* cancellation before permit, transformation, channel acquisition, pacing, send, response,
  retry delay, response finalization, and evidence durability;
* a response finalizer and channel acquisition that never complete normally, proving abort settles
  the owner-controlled exchange and cleanup gates;
* late callbacks after actor termination and after a new generation has reused the same source
  connection identity;
* runway loss after source completion, target completion, evidence durability, immediately before
  source acceptance, and immediately after source acceptance but before broker acknowledgement;
* at least two consecutive generation terminations in one process, with the second beginning only
  after every first-generation registry and ownership counter has returned to baseline;
* duplicate and missing lifecycle events;
* scanner follow-up, manifest-listed, manifest-omitted, terminal-self-completion, inconclusive, and
  generation-change results;
* manifest-cycle cases: an incomplete manifest has no effect; M listing C joins continuity across
  M; M omitting C resets only the predecessor through M; observations after M remain successor
  obligations;
* the Kafka-order inversions: an open accepted before M and appended after M, and an open accepted
  after M but appended before it;
* delayed predecessor observations with cycle M or earlier consumed after omitting M are settled by
  M, while only greater-cycle observations enter the successor;
* a malformed Kafka record mixing manifest cycles halts; a cycle-homogeneous record can be committed
  or retained without sub-offset accounting;
* a race where the replayer holds an incomplete cycle-M prefix and receives a cycle-M+1 late
  observation before manifest M: listing M continues the existing state, while omitting M disposes
  only the old prefix and feeds the held M+1 observation into empty per-connection HTTP state;
* exact-registry races: connection add during manifest preparation, remove during preparation, and
  manifest copy-and-increment on each side of those lifecycle operations while ordinary packets
  continue concurrently;
* same-connection callbacks become ready out of order, but the connection-local submission chain
  preserves sequence order; an injected Kafka sequence gap, regression, or conflicting duplicate
  halts rather than being reordered;
* overlapping manifest timer callbacks coalesce, and M+1 is never prepared or submitted before M
  is completely acknowledged;
* chunk handling: missing, duplicate, reordered, oversized, and contradictory chunks all make the
  manifest unusable rather than empty;
* timestamp separation: `manifestCycle`, captured observation time, proxy-local monotonic time,
  Kafka timestamp, and Kafka offset never substitute for one another;
* publisher failure: asynchronous or ambiguous send failure closes the capture gate and prevents
  later authoritative manifests or terminal self completion;
* proxy membership: startup capability probing, `PROBATIONARY` receiving no new traffic, active
  capacity boundaries, cooperative scale-up, leader replacement, and cold-start configurations
  that cannot satisfy their new-connection eligibility predicate;
* immutable routing: assignment changes move eligibility for new captured client connections while existing connections and
  manifests remain on their stored partitions;
* proxy completion: permanent new-connection and publication revocation, complete Netty teardown, an empty manifest,
  manifest-publisher quiescence, and every accepted send precede terminal self `NoMoreWrites`; a
  periodic callback racing `RETIRING` is either drained as earlier accepted work or exits without
  submission; peer completion is rejected, duplicate self completion is idempotent, and later
  traffic or manifests halt as a protocol violation;
* scan-ahead self completion: offset K may settle known blockers below K, replay later accepts
  previously unseen records below K, every traffic or manifest record above K violates the
  protocol, and only duplicate self completion is idempotent;
* pre-forward capture gating: complete Kafka acknowledgement precedes source execution, stale
  acknowledged-manifest age blocks strict execution, and a request admitted just before gate
  closure is already completely captured, under the documented non-streaming source-execution
  scope;
* elapsed-time expiration: neither replayer wall time nor Kafka `LogAppendTime` progression creates
  a commit-eligible disposition;
* source-owner ordering: commit proposal before versus after revocation, broker acknowledgement
  before versus after lifecycle notification, scan-blocker and connection-completion commands
  ordered with reads, and shutdown while source commands remain queued;
* fatal-fence ordering: commit registration paused inside the bounded acceptance section versus
  event-loop fatal closure, proving exactly one order and no check-then-register gap;
* owner-affinity enforcement: every source-I/O and replay-intake mutator succeeds on its owner,
  fails fatally off-owner, and exposes only immutable diagnostic views across the boundary;
* routing mismatches: partition stamp mismatch and attempted connection-partition mutation both
  halt rather than expire;
* a `writerNodeId` that stops emitting remains commit-blocking unless an applicable complete
  manifest or terminal self completion already exists.

Assertions:

* one terminal outcome per command and transaction,
* one disposition per record,
* every actor and transaction transition occurs on its assigned Netty event loop,
* every Kafka consumer, scan, source-generation, active-source-index, and source-commit mutation
  occurs on the Kafka source-I/O owner,
* every reconstruction, permit, progress, and disposition mutation occurs on replay intake,
* no send, retry, decode, or finalization work starts after the actor accepts abort; already queued
  foreign callbacks may perform only fenced self-cleanup,
* active-exchange abort does not complete before all owner-held contexts and resources are released,
* runway loss before source acceptance prevents commit submission,
* no commit on teardown,
* no owned resource remains,
* completion gates do not complete successfully before their postconditions hold,
* a new generation cannot observe, settle, or be blocked by state from a terminated generation.

### 18.3 Property tests

Generate captured observation sequences containing requests, responses, closes, connection exceptions,
dropped requests, partition changes, and scanner evidence. Check invariants rather than only expected
examples — F1 and F2 both hid behind generators that never produced a triggering input (no
close/exception directives, and every observation at the same timestamp so the expiry sweep never
fired).

### 18.4 Integration tests

* Kafka rebalance with active requests, and with no replay session at all.
* Two or more consecutive rebalances in one long-lived replayer, proving that each generation
  drains independently and later Kafka reads resume.
* Deleting or recreating the traffic topic while a replay is running is unsupported. An observed
  offset rewind fails the replay closed; operators must restart against a deliberately selected
  source rather than merge two unrelated offset namespaces in one process.
* Same-consumer partition round trip.
* Dead and slow targets under epsilon lookahead.
* Long legitimate connection with scanner follow-up present.
* Manifest-cycle omission at the commit head, including a newer successor segment already observed.
* Proxy duration cap producing exactly one real close through ordinary channel teardown.
* Open keep-alive connection retained across many manifest intervals, then closed — its incomplete
  remainder resets on the applicable omission, not before.
* Temporary partition drain ends with an acknowledged empty manifest; later reacquisition under the
  same capture-activation identity begins with a new initial manifest.
* Permanent proxy-partition retirement emits terminal self `NoMoreWrites` only after all related Netty
  connections and publisher work settle; injected later traffic halts and alarms.
* Proxy killed with connections open (`SIGKILL`, no close observations): complete requests replay;
  incomplete blockers without an applicable manifest remain retained and alarmed.
* Stalled strict-mode proxy that resumes after the stale threshold: delayed records are consumed
  normally, but no newly completed source request lacks a complete earlier Kafka representation.
* Manifest spanning the 1 MiB boundary: the proxy emits complete chunks, and dropping one chunk
  makes the manifest unusable.
* Group-assigned routing keeps every connection's traffic and manifests on its stored immutable
  partition while eligibility for new captured client connections follows the current cooperative
  assignment.
* Scanner enabled and scanner disabled produce identical eventual dispositions for the same durable
  log; they differ only in latency and resource use.
* A complete manifest whose chunks have unrelated traffic or another proxy's records between them is
  reconstructed and used normally.
* Total proxy-fleet loss does not fabricate completion; a replacement process never emits
  completion or manifests for an old capture-activation identity.

### 18.5 Leak tests

Enable Netty leak detection and instrument permits, contexts, actor entries, record obligations, and
evidence handles. Every test finishes with all registries empty. Repeated-generation tests assert
that the same baseline is reached after each cycle, not only when the process exits.

### 18.6 Acceptance criteria

The redesigned path is ready to replace the current path when:

1. The responsibility audit maps every concern to one proposed owner.
2. All deterministic terminal-transition tests pass.
3. Active target-exchange abort passes at every phase, including retry delay, channel acquisition,
   response wait, and a finalizer that never completes normally.
4. Rebalance and shutdown completion gates prove their documented drain postconditions.
5. Consecutive generation turnovers in one long-lived process return all ownership counters and
   registries to baseline before the next generation is admitted.
6. No teardown test commits work whose runway was lost before source acceptance.
7. Hard byte, record, and owned-resource budgets remain bounded during a stalled target; scanner and
   epsilon settings affect latency and resource use, not disposition.
8. Scanner settlement validates a complete manifest and applies its cycle only to the covered
   predecessor segment; no elapsed-time path is commit eligible.
9. Long live connections listed by manifests preserve continuity.
10. A silent proxy activation without an applicable manifest or terminal self completion remains
    retained and visible in alarms.
11. Incomplete manifests, publisher failure, and partition mismatch halt instead of creating a
    manifest-cycle reset.
12. Add, remove, and manifest copy-and-increment share the narrow proxy lifecycle boundary;
    ordinary packet capture remains concurrent and carries `manifestCycle`.
13. Netty leak detection and ownership counters remain clean.
14. Existing replay timing and ordering integration tests pass, or have an explicitly approved policy
    change.
15. The old sorter/schedule/callback orchestration can be **deleted** rather than retained as a
    fallback inside the new path.
16. Executor inventory shows exactly one Kafka source-I/O owner and one replay-intake owner, with no
    third blocking-source caller; affinity tests show each actor and its transactions remain on one
    existing Netty event loop.
17. Owner checks prove that `TrackingKafkaConsumer`, source scan state, reconstruction state,
    disposition state, permits, and progress are each mutated only by their documented owner.
18. Reflection/architecture tests reject Java default methods and built-in `NO_OP` instances on
    every required lifecycle interface. Lifecycle tests use hand-written state-recording
    implementations and in-memory OTel exporters; no mocking framework stands in for an owner,
    completion gate, mailbox, Kafka authority, or resource lifecycle.

Criterion 15 is the real gate. A migration that leaves the old orchestration reachable has added a
second way to be wrong rather than removing the first.

---

## 19. Resolved Design Decisions

These choices constrain the first implementation. They can be revisited only with an explicit change
to the corresponding invariant, matrix row, and tests.

### 19.1 Poison-record classification follows the RFS allowlist pattern

Use an explicit exception-type allowlist, default empty. The existing shared
`BulkDocErrorTypes.NON_RETRYABLE` vocabulary answers whether retrying is useful; it does **not**
authorize committing a failed replay. Those are separate policy decisions:

1. Retry classification decides whether another target attempt can help.
2. After retries are exhausted, the operator allowlist decides whether this deterministic failure may
   be recorded as a deliberate skip and committed.

Refactor the RFS `DocumentExceptionAllowlist` shape into a common helper beside
`BulkDocErrorTypes`, and use the same normalization and matching code from both products. Do not use
the replayer's non-empty default non-retryable set as an implicit commit allowlist. An unlisted failure
retains and halts; an allowlisted failure produces loud, durable classified-skip evidence before
commit.

### 19.2 Manifest-cycle resets do not require durable replay evidence initially

A manifest-cycle reset has no replay result to preserve for the incomplete predecessor. The
complete manifest and cycle-scoped decision are the commit authority. The first implementation
emits:

* a reason-coded metric without high-cardinality connection labels;
* a high-severity diagnostic containing the proxy activation, connection, partition, manifest
  cycle, affected obligation range, and disposition reason.

It does not write a durable discard receipt and does not expand `EvidenceWriter` for this case.
Accordingly, the matrix row is
`(ManifestCycleReset, NotSent, NotRequired) -> CommitPredecessorOnly`.

### 19.3 Duration configuration is diagnostic, not commit authority

There is no claim that a timeout, Kafka broker-time horizon, group departure, or health check fences
a stalled producer. None may construct a commit-eligible Kafka source outcome.

`maxConnectionDuration` remains an optional proxy resource cap. The scanner's operational budget
limits work per scan; exhaustion yields `Inconclusive`. `--packet-timeout-seconds`, if retained for
Kafka input, may emit a stall diagnostic but does not settle obligations. Replayer wall time and
Kafka `LogAppendTime` are equally non-authoritative for this purpose.

Enabling time-based settlement requires a separately reviewed design that resolves delayed suffix
records, arbitrary forward broker-clock jumps, and the explicit run-abandonment policy. It is not a
hidden implementation option.

### 19.4 Source-time progress uses the minimum partition watermark

`ReplayReadGate` uses the minimum settled watermark across the currently assigned partition
generation. This gives the simplest global memory-bound statement, accepting that one slow partition
can throttle the others.

This source-time watermark controls replay pacing only. It is derived from captured observation
time and is never used for manifest-cycle decisions or expiration.

An assigned partition with no outstanding admitted work advances toward the replay clock rather than
contributing negative infinity. Revocation removes its watermark; assignment creates a new
generation-scoped watermark so stale progress cannot leak across ownership changes.

### 19.5 Part-level evidence receipts remain internal

The first implementation exposes the existing whole-tuple behavior publicly. The transaction may use
an internal part-shaped adapter so ownership and future sequencing are not blocked, but external sink
implementors do not receive the four-receipt contract until a store actually persists the parts
independently.

### 19.6 Target retries remain inside one exchange

The target exchange performs retries while its actor command remains at the head of the FIFO. The
transaction supplies the immutable retry and classification policy, receives one terminal
`TargetOutcome`, and does not re-admit retries as new commands. This preserves per-connection ordering
without another actor transition.

### 19.7 Proxy traffic assignments and manifests follow group membership

The accepted routing design is
[`proxyHorizontalScalingAndNodeDeath.md`](proxyHorizontalScalingAndNodeDeath.md). A custom
cooperative assignor moves members through `PROBATIONARY` and `ACTIVE`; PROBATIONARY members
receive no traffic assignments. New connections choose once from the ACTIVE member's traffic
assignments and store that partition immutably. Existing connections drain in place through later
assignment changes.

No witness, peer-visibility, or writer-footprint contract is required. Therefore the old `nodeId`
hash range and startup-only shard-width setting are migration residue, not the target architecture.
Remove them after the group-aware assignor is active.

The manifest interval defaults to 30 seconds and must be positive. A complete listing manifest
resolves continuity across its cycle; a complete omission resets only the covered incomplete
predecessor. Manifest chunking remains mandatory regardless of assignment width.

---

## 20. PR Convergence Plan

The existing PR remains the integration vehicle. Soundness is established by explicit proof gates,
not by making reviewers reason about one undifferentiated diff:

**Review gate:** if implementation requires a design change, or a test exposes behavior whose
contract is not documented, stop that implementation slice. Update the design and review the new
contract before changing production code or complexifying it to satisfy the test.

1. **Freeze the protocol.** Land the group-routing, manifest-cycle, capture-before-forward,
   proxy-completion, retention, ownership, and fatal-event-loop contracts in the three normative
   design documents before implementing them. Once this review is accepted, delete the superseded
   branch-only notes `replayer-3231-review-notes.md`, `replayer-expiration-hardening.md`,
   `replayerCurrentToProposedArchitectureMap.md`, `replayerSimplifiedLifecycleDelta.md`,
   `replayerSimplifiedLifecycleDesign.md`, and `replayerWorkLifecycleResponsibilityAudit.md`.
   Their retained conclusions are incorporated here; leaving parallel design narratives in the
   final PR would recreate ambiguity about which contract governs.
2. **Remove superseded alternatives.** Never change the Kafka input namespace of a running replay;
   remove peer completion, witness coverage, and post-peer cutoffs; require process-fatal event-loop handling; remove
   cross-thread mailbox-loss mutation; remove lifecycle default methods and built-in no-op
   collaborators.
3. **Restore explicit owner authority.** Use exactly one Kafka source-I/O owner and one
   replay-intake owner, with typed immutable messages between them. Each mutable object belongs to
   exactly one of those owners. Remove the third-thread `BlockingTrafficSource` authority,
   `commitDataLock`, and shared source-state maps. Add always-on owner checks and structural tests
   that reject required default methods, `NO_OP` instances, and production constructors missing
   required fatal/lifecycle collaborators.
4. **Implement proxy membership and capture gating behind deterministic models.** Each state
   transition lands with its boundary tests: capability probing, probationary new-connection
   eligibility,
   cooperative transfer, immutable routing, acknowledged manifests, capture-gate closure,
   self-completion ordering, and irreversible pass-through.
5. **Prove real Kafka behavior.** Testcontainers exercises cooperative assignment `userData`,
   `enforceRebalance()`, leader replacement, metadata limits, complete manifest reconstruction,
   observation/manifest Kafka-order inversion, self `NoMoreWrites`, retained blockers, and
   unacknowledged-record redelivery. Live
   Kubernetes tests cover pod death, suspension and resume, network isolation, insufficient
   capacity, pass-through abandonment, and total-fleet loss.
6. **Run the complete acceptance matrix.** Existing replay, capture, leak, race, and integration
   suites must pass together. No feature is considered complete because its local tests pass while
   a known unsound path remains reachable.

Keep these as ordered, reviewable commits in the PR. A stacked-PR extraction is optional only after
a slice has its contract and targeted proof tests and does not expose an unsound reachable
intermediate state. Do not first spend time mechanically disentangling the historical diff; use the
new invariant-oriented commits to create the review seams. The final integration PR remains the
place where the complete architecture and full test matrix are proven together.

---

## 21. Non-Goals

Deliberately out of scope:

* Replacing Kafka or changing its at-least-once delivery model.
* Rewriting HTTP reconstruction or request transformations.
* Introducing a new durable tuple store in the first implementation.
* Providing exactly-once target-side effects across process crashes.
* Reproducing HTTP/2 multiplexing semantics.
* Any path by which replayer wall-clock age alone can commit Kafka records.
