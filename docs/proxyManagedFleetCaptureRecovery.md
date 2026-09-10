# Automated Fleet Capture Awareness and Recovery

**Status: deferred design addendum; non-normative pending rewrite (2026-09-10).** The standalone
contract is
[Proxy Horizontal Scaling, Capture Liveness, and Writer Completion](proxyHorizontalScalingAndNodeDeath.md).
That contract now uses exact positive manifests, terminal self-only `NoMoreWrites` for permanent
writer-partition retirement, configured expiration of incomplete state, and an irreversible local
capture gate. It has no designated witnesses, peer completion, or peer terminal cutoff.

The remainder of this addendum was drafted against the superseded witness model. Its session and
fleet-recovery ideas remain useful input, but its witness, footprint, suppression, completion, and
replayer-settlement rules are not implementation requirements.

A future rewrite must preserve these base rules:

- a writer node id is never reused after capture is abandoned;
- temporary partition drain uses an acknowledged empty manifest;
- `NoMoreWrites` is partition-scoped, self-emitted after full connection/publisher teardown, and
  terminal for that writer-partition identity;
- configured expiration settles only incomplete reconstruction state;
- `lastPositiveLivenessBrokerTime`, `scannedThroughBrokerTime`, and every replayer liveness or
  expiration timestamp use Kafka `LogAppendTime` exclusively;
- strict mode captures a complete request before source execution and checks acknowledged-manifest
  freshness before forwarding;
- pass-through permanently abandons capture and raises a persistent gap alarm; and
- an uncaptured source interval starts a new capture and replay run with a fresh source snapshot.

Until that rewrite is complete, `suppressCaptureByDefault=true` has no normative managed protocol in
this document.

## 1. Goals

The managed extension adds five capabilities:

1. Durably mark capture incomplete before intentionally forwarding uncaptured source traffic.
2. Determine whether every process still capable of affecting the source is capture-only.
3. Recover a fleet automatically after an uncaptured interval without confusing delayed records
   from the abandoned capture interval with new capture.
4. Bind a source snapshot and replayer run to one authoritative capture session.
5. Make controller failure conservative: stall, remain incomplete, or exit, but never silently
   assert complete capture.

The extension does not promise global ordering across connections or partitions. It also does not
make missing traffic replayable. Recovery establishes a new complete interval and requires a new
source snapshot.

## 2. Why the base protocol is insufficient for automation

The base protocol deliberately exposes local facts rather than one fleet verdict. That leaves
several automation gaps:

- A proxy can enter autonomous pass-through before any external system records the compromise.
- A process removed from the load balancer may retain an existing connection and continue mutating
  the source for an hour or more.
- Killing a producer prevents new sends but does not retract a Kafka request already transmitted
  to a broker. Client delivery and blocking timeouts are not broker fencing.
- A delayed record from an old writer can first appear on a partition after a new fleet boundary.
  An opaque, previously unseen writer node id is insufficient to classify it.
- A replayer starting after Kafka retention removed an earlier boundary record cannot safely infer
  the current capture generation from the first traffic record it sees.
- Total witness loss has no ordinary settlement path. Automated recovery must explicitly abandon
  the incomplete interval rather than pretend that a replacement witness observed the old writer.

These gaps require an external authority plus an explicit data-plane session identity. A process
timeout alone is not that authority.

## 3. Terminology and identities

The managed protocol uses distinct identities for distinct purposes.

### 3.1 `processId`

Identifies one running proxy process to the controller and deployment platform. It is fresh on
every process start.

### 3.2 `writerNodeId`

Identifies one capture activation for membership, manifests, traffic attribution, and ordinary
`NoMoreWrites` handling. It remains opaque to the replayer and is never reused after terminal
self-completion or local capture-authority loss.

### 3.3 `recoveryId`

Identifies one idempotent controller recovery workflow. It is composed from the Capture Replay
resource identity and a monotonically increasing controller sequence. It prevents stale control
commands from reactivating an older workflow.

### 3.4 `captureSessionId`

Identifies the complete-capture interval to which data-plane records belong. In a controller-managed
deployment it is derived from, or equal to, the recovery id that established that interval.

`captureSessionId` is the load-bearing wire identity that prevents delayed old traffic from being
accepted after fleet recovery. It is not Kafka's producer epoch, is not an assignment revision,
and is not inferred from `writerNodeId`.

### 3.5 Traffic-affecting process

A process is traffic-affecting if it:

- can receive a new source connection;
- owns an existing client or source-side connection;
- may still forward a previously accepted source operation; or
- is unreachable and has not been proven terminated.

Fleet completeness is evaluated over this set, not just load-balancer endpoints or desired
replicas.

## 4. Required proxy changes

### 4.1 Configuration

Controller-managed processes use:

```
suppressCaptureByDefault = true
captureFailureRetryTimeout
captureFailureDisposition = CONTROLLER_AUTHORIZED_PASS_THROUGH
controllerAuthorizationTimeoutAction = CONTINUE_BLOCKING | EXIT
controllerPassThroughHandoff = SAME_PROCESS | REPLACEMENT_PROCESS
nonCapturingConnectionDrainTimeout
```

`suppressCaptureByDefault=true` is stable deployment configuration. The controller does not toggle
it during each incident. Every replacement therefore boots suppressed even if it missed prior
commands.

The proxy may retry Kafka while in `CAPTURE_RETRYING`. All affected source forwarding remains
capture-before-forward during that interval. Once retry is exhausted, the writer activation is
permanently closed and the process enters `CAPTURE_FAILURE_PENDING`.

### 4.2 Local states

The managed outer lifecycle is:

```
SUPPRESSED
  -> PROBING
  -> PROBATIONARY
  -> ACTIVE
  -> CAPTURE_RETRYING
       -> ACTIVE                         // Kafka recovered before failure decision
       -> CAPTURE_FAILURE_PENDING        // writer activation permanently closed
            -> PASS_THROUGH_COMPROMISED  // only after controller authorization
            -> process exit
```

A healthy controller-requested suppression has a separate path:

```
ACTIVE -> SUPPRESSING -> CAPTURE_SUPPRESSED_AND_QUIESCENT
```

Only the healthy suppression path may later create a fresh writer activation in the same process.
A process that entered `CAPTURE_FAILURE_PENDING` or `PASS_THROUGH_COMPROMISED` because of a send
failure or ambiguous producer outcome never captures again. It must terminate before the recovery
can become complete.

### 4.3 Status and control interface

Every proxy exposes a port that is not used for source forwarding. Read-only status includes:

```
processId
writerNodeId, if one is active
recoveryId
captureSessionId
captureOperatingState
captureCapability and latestProbeResult
captureFailureCause and captureFailureRetryAge
controllerAuthorizationWaitAge
membershipPhase
forwardingReady
captureReady
captureAdmissionSuppressed
publisherQuiescent
capturingConnectionCount
nonCapturingConnectionCount
oldestNonCapturingConnectionAge
pendingPeerCompletionCount
protocolVersion
binaryVersion
```

Authenticated, authorized, idempotent mutation endpoints include:

```
suppressCapture(recoveryId)
authorizeCompromisedPassThrough(recoveryId)
grantCapture(recoveryId, captureSessionId, captureActivationNumber)
forceCloseNonCapturingConnections(recoveryId)
emitFleetCaptureReset(recoveryId, captureSessionId)
emitCaptureCoverageEstablished(recoveryId, captureSessionId)
```

Readiness probes cannot call mutation endpoints. Network policy, transport authentication, and
request authorization restrict mutations to the workflow controller.

### 4.4 Capture capability

`captureCapability=true` must mean more than metadata connectivity. Before implementation, the
probe contract must specify:

- whether it performs an acknowledged Kafka write;
- which traffic partitions and broker leaders it covers;
- the producer settings and timeouts it validates;
- how probe records are distinguished from traffic; and
- how the controller prevents a partial probe from authorizing capture.

Until that contract exists, capability is advisory and cannot authorize a capture grant. A
metadata lookup or successful TCP connection alone is insufficient.

## 5. Data-plane record changes

Every controller-managed writer-scoped record carries the capture session of the subject writer:

```
TrafficRecord {
    writerNodeId
    captureSessionId
    ...
}

OpenConnectionManifest {
    writerNodeId
    captureSessionId
    ...
}

NoMoreWrites {
    writerNodeId
    writerCaptureSessionId
    partition
    emitterNodeId
}
```

The session field is optional only for the unmanaged legacy mode. A controller-managed process
fails startup if the configured wire version cannot carry it.

The managed protocol adds two partition-scoped control records:

```
FleetCaptureReset {
    recoveryId
    captureSessionId
}

CaptureCoverageEstablished {
    recoveryId
    captureSessionId
}
```

Each control record is written independently to every traffic partition and acknowledged on every
partition before the controller advances. Timestamps may be included for diagnostics, but they do
not establish ordering or authority.

`FleetCaptureReset` and `CaptureCoverageEstablished` are record names. “Reset marker” and “coverage
marker” are informal shorthand and should not appear in protocol APIs.

Adding the session field and control records is a wire-level change. Per the base document's
rollout rule, incompatible binaries never coexist as capture-group members. Deployment first enters
an explicit no-capture interval, empties the old group, rolls all binaries, and then starts a new
cold cohort.

## 6. Controller source of truth

### 6.1 Capture Replay status

The Capture Replay custom resource keeps current truth:

```
status:
  conditions:
    - type: CaptureCoverageComplete
      status: "True" | "False" | "Unknown"
      reason: ...
      lastTransitionTime: ...
  recoveryId: ...
  captureSessionId: ...
  nonCapturingSince: ...
  captureRecoveryTimestamp: ...
```

The status is not an unbounded event history. Logs and metrics retain diagnostic history; the
resource answers whether capture is complete now and identifies the current recovery.

### 6.2 Grant ledger

The controller durably records every capture grant:

```
processId
writerNodeId
recoveryId
captureSessionId
grantTime
lastObservedState
retirementState
```

The ledger is the inventory of writer activations that must be retired before recovery. Replica
count, current group membership, Pod objects, and load-balancer endpoints cannot reconstruct this
history after failures.

### 6.3 Completeness population

`CaptureCoverageComplete=True` requires:

- every traffic-affecting process is reachable or proven terminated;
- every traffic-affecting live process reports the current recovery and session;
- every process able to forward source traffic is capture-only;
- every non-capturing connection count is zero;
- no previous-session process can still forward a source operation;
- every required protocol and binary version is compatible; and
- `CaptureCoverageEstablished` has been acknowledged on every partition.

A process removed from the load balancer but retaining one existing connection remains in the
population. An unreachable process is never assumed healthy.

Capacity thresholds such as `minimumStrictReadyPerAz` decide whether availability requires
pass-through capacity. No threshold below 100% means capture is complete.

## 7. Capture-compromise transition

### 7.1 Planned admission of pass-through capacity

Before intentionally routing any pass-through process, the controller:

1. allocates the next recovery id;
2. durably sets `CaptureCoverageComplete=False`, `recoveryId`, and `nonCapturingSince`;
3. verifies that the status update succeeded; and
4. only then admits the minimum pass-through capacity required by availability policy.

If the status update fails or is ambiguous, pass-through is not admitted.

### 7.2 Active proxy capture failure

After `captureFailureRetryTimeout`, an otherwise-healthy proxy reports
`CAPTURE_FAILURE_PENDING` and keeps uncaptured forwarding blocked. The controller chooses:

**Same-process handoff**

1. Durably set the incomplete condition and recovery id.
2. Send `authorizeCompromisedPassThrough(recoveryId)`.
3. The proxy validates the id, closes the writer activation permanently, leaves membership,
   converts existing connections to non-capturing, and opens the pass-through gate.
4. The process remains forwarding-only and must later terminate.

**Replacement-process handoff**

1. Durably set the incomplete condition and recovery id.
2. Start or select a process that booted suppressed.
3. Admit that process as pass-through.
4. Remove and terminate the failed writer.

Updating the resource to incomplete and then failing to admit pass-through is conservative. Opening
pass-through before the durable update is forbidden. If controller authorization times out, the
proxy remains blocked or exits according to configuration.

### 7.3 Failure without a gap

Loss of one strict proxy does not automatically compromise capture if:

- it never forwarded uncaptured traffic;
- it is removed from routing or proven terminated;
- every remaining traffic-affecting process is strict; and
- sufficient strict capacity exists without admitting pass-through.

This is ordinary node replacement and witness repair. It does not require a new capture session or
source snapshot.

## 8. Suppression and old-writer retirement

### 8.1 Healthy suppression

A proxy may acknowledge `CAPTURE_SUPPRESSED_AND_QUIESCENT` only when:

1. capture admission for the old writer is irreversibly closed;
2. no new traffic or manifest submission can enter its publisher;
3. every previously accepted producer send completed successfully;
4. the ordered publisher lane and producer accumulator are empty;
5. the producer is closed;
6. every connection is permanently non-capturing or closed; and
7. the writer left membership and will never rejoin under that writer node id.

This is a strong local proof and may allow a healthy process to receive a later capture grant with
a fresh writer node id and capture session.

### 8.2 Failed or ambiguous producer

A process with a failed or ambiguous send cannot acknowledge healthy suppression. Stopping new
submissions is not proof that all old Kafka requests disappeared.

For fleet reset, the controller may treat the old writer activation as capture-retired only after:

- the one-way writer latch is irreversibly closed;
- no new capture submission can enter locally;
- the writer left membership;
- the grant ledger forbids another grant to that process activation; and
- every later old-session Kafka record will be rejected by the session gate.

An otherwise-healthy process may remain alive as authorized pass-through while coverage is
incomplete. If the process cannot reliably report or enforce the closed latch, the controller
terminates the actual process, container, or host and confirms termination rather than merely
observing API-object deletion.

Process death proves that no new application sends originate afterward. It does not retract an old
Kafka request already outside the process. `producerMaxBlockTime`, `producerDeliveryTimeout`, and a
safety margin may be useful operational waits, but they are not the correctness proof for rejecting
late Kafka records.

The capture-session rule supplies that proof: after reset to session S, every record carrying an
older session is discarded and alarmed, even when its writer node id was never previously observed
on that partition.

### 8.3 Remaining source-side quiescence requirement

Kafka session fencing does not prove that a source request transmitted before process death cannot
finish later. Before capture coverage can become complete or a source snapshot can begin, the
workflow must define a conservative source-side retirement condition for:

- a request fully transmitted to the source but not yet completed;
- a source response in flight when the proxy is killed; and
- force-closed long-lived connections.

If the source protocol or proxy cannot prove completion, cancellation, or a bounded maximum
operation lifetime, recovery remains incomplete. A convenient Kubernetes timeout is not evidence.

This is a blocking design item, not an implementation detail.

## 9. Fleet reset and coverage establishment

### 9.1 Reset

After every prior writer activation is capture-retired under §8 and the controller has frozen the
old grant ledger:

1. allocate the new `captureSessionId`;
2. select a healthy Kafka-capable control-record producer;
3. write `FleetCaptureReset{recoveryId, captureSessionId}` once to every traffic partition;
4. retain and retry every incomplete partition obligation; and
5. issue no new capture grant until every reset copy is acknowledged.

The reset establishes the accepted session for each partition. It does not reconstruct source
traffic from the abandoned interval.

A delayed record from an old session that lands after the reset is discarded and alarmed. This
applies even if the old `writerNodeId` had never appeared on that partition.

Authorized pass-through processes and their existing connections may still be forwarding while the
condition is false. They do not block reset once their old capture activation is permanently
closed, because every one of their source mutations remains part of the incomplete interval. They
do block coverage establishment and the next source snapshot.

### 9.2 New capture grants

After reset fan-out completes, the controller grants the new session to eligible processes. Each
process creates a fresh writer node id and follows the base PROBATIONARY-to-ACTIVE admission
protocol, including witness maturity and conservative footprint confirmation.

New connections use strict capture only after activation. Existing pass-through connections never
upgrade in place; they drain or are force-closed.

### 9.3 Coverage establishment

Only after every traffic-affecting process is strict or terminated and all non-capturing
connections are gone may the controller write
`CaptureCoverageEstablished{recoveryId, captureSessionId}` to every partition.

This record settles no connection and repairs no missing traffic. It states that the controller
had already established complete capture for the named session before that partition offset.

The custom-resource condition becomes true only after:

- every partition copy is acknowledged;
- the recovery and session remain current;
- the complete population is revalidated; and
- no new compromise occurred during fan-out.

Partial fan-out remains incomplete and retries idempotently.

## 10. Replayer rules and bootstrap

### 10.1 Session gate

A managed replayer is created with one expected `captureSessionId`. For every writer-scoped record:

- matching session: process under the base replayer rules;
- older or otherwise different session: discard, advance, and emit a high-severity alarm;
- missing session in a managed run: fail closed as protocol-incompatible.

The replayer does not parse writer node ids or compare numeric epochs.

### 10.2 Reset encountered by an existing replay run

A replay run based on a snapshot from session S1 must not silently cross
`FleetCaptureReset{captureSessionId=S2}`. The uncaptured interval between sessions requires a new
source snapshot.

Therefore an existing S1 replay run that encounters the S2 reset terminates as superseded or
invalidated. It does not settle old state and continue into S2.

### 10.3 Trusted replay plan

The snapshot workflow durably creates:

```
snapshotId
captureSessionId
trafficTopic
partitionStartOffsets
coverageEstablishedOffsetByPartition
recoveryId
```

The replayer receives this plan as trusted bootstrap. It need not have personally consumed the
earlier reset or coverage-establishment records when its configured start offsets are later.

The replayer must not adopt the first capture session it sees. A delayed old record could be first.
If neither a trusted replay plan nor an observed authoritative reset establishes the session, the
replayer fails closed.

Replayer checkpoints persist the expected capture session. Restart never re-infers it from retained
traffic.

### 10.4 Kafka retention

Traffic-topic retention may delete old reset and coverage-establishment records before a new
replayer starts. The durable replay plan is therefore required for long-lived snapshots and
restarts. Periodically re-emitting a boundary record is diagnostic redundancy, not a substitute for
trusted snapshot metadata.

## 11. Snapshot workflow

A source snapshot is usable only when:

1. `CaptureCoverageComplete=True`;
2. the workflow records the current recovery and capture session;
3. the complete-population check succeeds immediately before snapshot start;
4. the condition and session remain unchanged throughout the snapshot; and
5. the complete-population check succeeds again after snapshot completion.

If capture becomes false or unknown, the recovery/session changes, any granted process becomes
unreachable, or the traffic-affecting inventory changes ambiguously, reject the snapshot.

The accepted snapshot manifest stores the replay plan from §10.3. That manifest, rather than the
current custom-resource condition at some later time, identifies the session the replayer may
consume.

## 12. End-to-end recovery workflow

For an incident that actually forwarded uncaptured traffic:

1. A proxy enters `CAPTURE_RETRYING`; forwarding remains strict and blocked as needed.
2. Retry is exhausted; the proxy enters `CAPTURE_FAILURE_PENDING`.
3. The controller allocates recovery R and durably sets coverage false.
4. The controller authorizes same-process or replacement-process pass-through if availability
   policy requires it.
5. Strict replacement capacity is started and probed.
6. Every previous writer activation is capture-retired by healthy suppression, a trusted permanent
   writer latch, or termination of an untrusted process.
7. The controller creates session S and fans `FleetCaptureReset{R,S}` to every partition.
8. After all reset acknowledgements, the controller grants S to eligible proxies.
9. Proxies become ACTIVE under the base witness and footprint protocol.
10. Remaining pass-through processes and off-load-balancer connections drain or are removed.
11. Source-side in-flight retirement is established.
12. The controller fans `CaptureCoverageEstablished{R,S}` to every partition.
13. After all acknowledgements and revalidation, the resource becomes complete.
14. A new source snapshot is taken and bound to S.
15. A new replayer run starts from that snapshot's trusted replay plan.

At every stage, failure leaves the condition false or unknown. No timeout skips a proof.

## 13. Implementation deltas

| Change | Required behavior |
|---|---|
| Managed configuration | Add `suppressCaptureByDefault`, controller-authorized fallback, retry, authorization, drain, and handoff settings. |
| Separate status/control interface | Expose local state and authenticated idempotent commands without sharing the source listener. |
| Capture Replay status | Add complete/false/unknown condition, recovery id, capture session id, and transition timestamps. |
| Durable grant ledger | Persist every process, writer, session, grant, and retirement state across controller failover. |
| Traffic-affecting inventory | Track load-balanced processes, draining processes, off-LB live connections, and unreachable grants. |
| Record schema | Carry `captureSessionId` on traffic, manifests, and writer completion; add reset and coverage-establishment records. |
| Proxy state machine | Implement retry, pending authorization, same-process pass-through, healthy suppression, and permanent failed-activation rules. |
| Session-aware replayer | Enforce the expected session, reject late old-session records, persist bootstrap, and invalidate a run on a later reset. |
| Snapshot replay plan | Persist the accepted session and per-partition offsets with the source snapshot. |
| Capability probe | Define and implement a real acknowledged capability proof rather than metadata connectivity. |
| Source retirement proof | Define how forced termination excludes source operations completing after the recovery boundary. |
| Security | Authenticate and authorize every mutation and control-record emission. |
| Observability | Export recovery, session, inventory, compromise, suppression, drain, fan-out, snapshot, and rejection metrics. |

## 14. Failure boundaries

- Controller unavailability never authorizes pass-through or capture grants.
- Existing strict proxies may continue their already granted session while the controller is down.
- New processes remain suppressed without a grant.
- A stale command cannot reactivate an older recovery or session.
- Load-balancer removal and Pod deletion are not process termination.
- A percentage below 100% can authorize capacity decisions but never complete capture.
- Total witness loss is not retroactively repaired. Fleet recovery explicitly abandons that
  incomplete interval and requires a new session and source snapshot.
- A reset acknowledged on only some partitions establishes nothing globally.
- Coverage establishment acknowledged on only some partitions keeps the resource incomplete.
- Late records from an old session are discarded and alarmed, including the first record from a
  previously unseen writer.
- A replay run never crosses from one capture session to another without a new snapshot.

## 15. Validation plan

### 15.1 Deterministic tests

- stale, duplicate, and out-of-order controller commands;
- retry success returning to ACTIVE before the failure decision;
- permanent writer closure after retry exhaustion;
- same-process and replacement-process pass-through only after durable incomplete status;
- controller timeout causing block or exit, never autonomous degradation;
- healthy suppression acknowledgement only after every producer future succeeds;
- failed or ambiguous sends preventing healthy suppression;
- off-load-balancer live connections blocking completeness;
- unreachable processes producing false or unknown, never true;
- reset and coverage fan-out idempotence and partial failure;
- session mismatch rejection for known and previously unseen writers;
- replayer bootstrap before and after retained boundary records;
- checkpoint restart preserving expected session; and
- later reset invalidating an older snapshot/replay run.

### 15.2 Testcontainers Kafka

- an old producer request appended after `FleetCaptureReset` and rejected by session;
- reset from a different producer overtaking an ambiguous old send;
- controller failover during every fan-out stage;
- broker restart during suppression, reset, activation, and coverage establishment;
- duplicate control records and independent partition barriers;
- writer and witnesses failing together through the configured boundary; and
- protocol-version rejection during the wire-format rollout.

### 15.3 Live deployment tests

- one strict proxy failure with sufficient remaining capacity and no capture compromise;
- AZ capacity falling below policy and authoritative pass-through admission;
- same-process pass-through preserving existing connections;
- replacement-process handoff requiring client reconnect;
- an off-load-balancer hour-long connection blocking recovery until forced close;
- Pod deletion without actual process death;
- node or host fencing when process termination cannot be confirmed;
- controller restart with grant-ledger reconstruction;
- snapshot rejection when recovery changes mid-snapshot; and
- full recovery followed by a replayer launched after reset records have aged out, using only the
  durable replay plan.

## 16. Blocking design decisions before implementation

The following must be resolved before production code treats this addendum as sound:

1. Define the source-side in-flight retirement proof after forced process or host termination.
2. Define the exact Kafka capability probe and its partition coverage.
3. Finalize the `captureSessionId` wire fields and compatibility rollout.
4. Define durable grant-ledger storage and controller failover semantics.
5. Define replay-plan storage, retention, and checkpoint integration.
6. Decide whether unmanaged operators may manually provide a `captureSessionId`; if so, document
   that this is a manual new-run boundary rather than automatic recovery.

Until those decisions are complete, the base document's manual new-topic/new-run procedure remains
the sound recovery path after autonomous pass-through.
