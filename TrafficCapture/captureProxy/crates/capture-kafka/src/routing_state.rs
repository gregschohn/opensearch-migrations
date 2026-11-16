//! Routing generations, writer identities, connection routes, and heartbeat baselines
//! (traceability row KAFKA-06).
//!
//! # Two orthogonal state machines
//!
//! This module owns the **routing** machine, keyed per `(writerNodeId, partition)`:
//!
//! ```text
//! INITIALIZING --activate--> CURRENT --(superseded | orderly retirement)--> DRAINING
//!              DRAINING --(no connections remain)--> RETIRING --> RETIRED
//! ```
//!
//! [`crate::publisher_lane`] owns the **publication** machine, `OPEN -> RETIRING -> RETIRED`.
//! They must not be merged. A lane stays `OPEN` while its writer status is `DRAINING` — that is
//! exactly how a superseded generation keeps heartbeating and keeps serving the connections it
//! already owns (kafka spec §13.1, discrepancy D-20 records that merging the two enums is the most
//! likely way to break it).
//!
//! # Writer identity
//!
//! `writerNodeId = captureActivationId + ":" + assignmentSequence`, with the sequence
//! pre-incremented so the **first** generation is `1`. The sequence advances once per
//! [`RoutingState::prepare_routing_generation`] call *including calls that later fail*, so
//! identities may skip a number but can never repeat. A repeated identity would make two different
//! partition assignments indistinguishable to the replayer.
//!
//! # Immutability
//!
//! A [`ConnectionRoute`] is created once and its writer identity and partition are never
//! recomputed, so every record for that connection — including records published long after the
//! generation was superseded — carries the original identity and goes to the original partition.
//! The registry key is `(writerNodeId, connectionId)`, so the same local connection id may legally
//! appear under two writer identities.
//!
//! # Owner decision OD-6
//!
//! A heartbeat acknowledgement is accepted only when `-S <= elapsed < E`. Java has **no** lower
//! bound (`CaptureRoutingState.java:465-479`, deliberately asserted at
//! `CaptureRoutingStateTest.java:73`) and overwrites the baseline on backward movement, which
//! widens the pre-forward window without bound: a regressed baseline makes
//! `observation - baseline < E` easier to satisfy, so a stale observation can release a source
//! write. The same lower bound applies to the pre-forward critical-mutation check.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{CaptureFailure, CorruptedCaptureState, CorruptionCheck, FailureReason};
use crate::routing_hash::select_partition;

/// One writer identity's claim on one Kafka partition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WriterPartition {
    writer_node_id: Arc<str>,
    partition: i32,
}

impl WriterPartition {
    /// Pairs `writer_node_id` with `partition`.
    #[must_use]
    pub fn new(writer_node_id: Arc<str>, partition: i32) -> Self {
        Self {
            writer_node_id,
            partition,
        }
    }

    /// The writer identity, `captureActivationId:assignmentSequence`.
    #[must_use]
    pub fn writer_node_id(&self) -> &str {
        &self.writer_node_id
    }

    /// A cheap clone of the writer identity handle.
    #[must_use]
    pub fn writer_node_id_handle(&self) -> Arc<str> {
        Arc::clone(&self.writer_node_id)
    }

    /// The Kafka partition.
    #[must_use]
    pub fn partition(&self) -> i32 {
        self.partition
    }
}

impl fmt::Display for WriterPartition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.writer_node_id, self.partition)
    }
}

/// The routing lifecycle of one writer partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterStatus {
    /// Registered, awaiting its initial heartbeat acknowledgement. Not routable.
    Initializing,
    /// The active generation. New connections route here.
    Current,
    /// Superseded or shutting down. Serves existing connections and keeps heartbeating.
    Draining,
    /// Drained of connections and selected for retirement. Heartbeats have stopped.
    Retiring,
    /// Finished. There is no transition out of this state.
    Retired,
}

/// An immutable connection-to-writer-partition binding.
///
/// Cloning yields an equal route with the same identity token, so a clone handed to the record
/// serializer is still accepted by [`RoutingState::accept_traffic_submission`]. A route for a
/// *different* registration, even with the same connection id and writer, carries a different
/// token and is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRoute {
    writer_node_id: Arc<str>,
    connection_id: Arc<str>,
    partition: i32,
    token: u64,
}

impl ConnectionRoute {
    /// The writer identity stamped into every record for this connection.
    #[must_use]
    pub fn writer_node_id(&self) -> &str {
        &self.writer_node_id
    }

    /// A cheap clone of the writer identity handle, for `TrafficStream.nodeId`.
    #[must_use]
    pub fn writer_node_id_handle(&self) -> Arc<str> {
        Arc::clone(&self.writer_node_id)
    }

    /// The connection id, which is also the Kafka record key.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// A cheap clone of the connection id handle.
    #[must_use]
    pub fn connection_id_handle(&self) -> Arc<str> {
        Arc::clone(&self.connection_id)
    }

    /// The Kafka partition every record for this connection goes to.
    #[must_use]
    pub fn partition(&self) -> i32 {
        self.partition
    }

    /// The `(writerNodeId, partition)` lane this route publishes through.
    #[must_use]
    pub fn writer_partition(&self) -> WriterPartition {
        WriterPartition::new(Arc::clone(&self.writer_node_id), self.partition)
    }

    /// This registration's identity token, the Rust stand-in for Java's reference identity.
    #[must_use]
    pub fn token(&self) -> u64 {
        self.token
    }
}

impl fmt::Display for ConnectionRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}#{}",
            self.writer_node_id, self.partition, self.connection_id
        )
    }
}

/// A generation that has been registered but is not yet usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRoutingGeneration {
    writer_node_id: Arc<str>,
    sequence: u64,
    partitions: Vec<i32>,
}

impl PendingRoutingGeneration {
    /// The writer identity this generation will publish under.
    #[must_use]
    pub fn writer_node_id(&self) -> &str {
        &self.writer_node_id
    }

    /// A cheap clone of the writer identity handle.
    #[must_use]
    pub fn writer_node_id_handle(&self) -> Arc<str> {
        Arc::clone(&self.writer_node_id)
    }

    /// The assignment sequence number, starting at 1.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The ascending, deduplicated partition list.
    #[must_use]
    pub fn partitions(&self) -> &[i32] {
        &self.partitions
    }

    /// The writer partitions of this generation, in ascending partition order.
    #[must_use]
    pub fn writer_partitions(&self) -> Vec<WriterPartition> {
        self.partitions
            .iter()
            .map(|p| WriterPartition::new(Arc::clone(&self.writer_node_id), *p))
            .collect()
    }
}

/// What changed when a generation was activated, for logging and metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// The identity that is now current.
    pub writer_node_id: Arc<str>,
    /// Its assignment sequence.
    pub sequence: u64,
    /// Its partitions, ascending.
    pub partitions: Vec<i32>,
    /// The superseded identity, if there was one.
    pub previous_writer_node_id: Option<Arc<str>>,
    /// The superseded partitions, ascending.
    pub previous_partitions: Vec<i32>,
}

/// What began draining when orderly retirement started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderlyRetirement {
    /// The identity that stopped being current, if any generation was active.
    pub writer_node_id: Option<Arc<str>>,
    /// Its partitions, ascending.
    pub partitions: Vec<i32>,
}

/// A routing-state operation was refused.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RoutingError {
    /// An enumerated internal invariant was violated. Always terminates the process.
    #[error("{0}")]
    Corrupted(#[from] CorruptedCaptureState),

    /// The caller supplied an unusable argument. An ordinary capture failure.
    #[error("{0}")]
    InvalidArgument(String),

    /// Broker time was missing, non-positive, or outside the accepted window.
    #[error("{0}")]
    BrokerTime(String),

    /// Routing is shutting down and no new connection may be admitted.
    #[error("capture Kafka routing is shutting down")]
    ShuttingDown,

    /// No routing generation is usable yet.
    ///
    /// # Deviation
    ///
    /// Spec discrepancy D-5: Java builds this condition *and* calls the capture-failure policy
    /// (`KafkaCaptureFactory.java:310-314`, `:324-327`), so under `fail-closed` an early
    /// connection terminates the process. `proxyCaptureProtocol.md` §3.2 says the opposite: "a
    /// client connecting earlier is refused, and no capture-failure policy is invoked by that
    /// refusal." The refusal here invokes no policy.
    #[error(
        "capture Kafka is not accepting new connections before its first routing generation is ready"
    )]
    NotReady,

    /// A partition was assigned that is above the highest partition index known to this process.
    ///
    /// # Deviation
    ///
    /// Spec discrepancy D-15 combined with owner decision OD-7. Java freezes
    /// `topicPartitionCount` at construction and throws
    /// `IllegalArgumentException("partition is outside the traffic topic")` for any higher
    /// partition, which fails the generation and terminates capture. Under OD-7 an out-of-band
    /// partition increase is the **intended** operational path, so that behaviour would mean every
    /// intentional partition increase causes a capture outage on every running proxy. The count is
    /// therefore treated as a lower bound that a metadata refresh may raise, never lower. A
    /// partition still out of range after a refresh is a hard failure.
    #[error(
        "partition {partition} is above the highest known partition index for the traffic topic \
         ({known_count} partitions known); the topic may have been grown out of band"
    )]
    PartitionAboveKnownCount {
        /// The assigned partition.
        partition: i32,
        /// How many partitions this process currently knows about.
        known_count: usize,
    },
}

impl RoutingError {
    /// The terminal failure this error becomes.
    ///
    /// The routing table of `CaptureKafkaPublisher.failForThrowable`
    /// (`CaptureKafkaPublisher.java:1204-1210`): corruption is an unstable-process failure,
    /// everything else is an ordinary capture failure.
    #[must_use]
    pub fn into_capture_failure(self) -> CaptureFailure {
        match self {
            Self::Corrupted(corruption) => CaptureFailure::corrupted(corruption),
            Self::BrokerTime(message) => {
                CaptureFailure::capture(FailureReason::BrokerTime, message)
            }
            other => CaptureFailure::capture(FailureReason::Other, other.to_string()),
        }
    }

    /// Whether this error is a catalogued corruption.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::Corrupted(_))
    }
}

fn corrupted(check: CorruptionCheck, detail: impl Into<String>) -> RoutingError {
    RoutingError::Corrupted(CorruptedCaptureState::new(check, detail))
}

#[derive(Debug)]
struct WriterPartitionState {
    status: WriterStatus,
    last_accepted_heartbeat_log_append_time: Option<i64>,
    connection_ids: HashSet<Arc<str>>,
}

impl WriterPartitionState {
    fn new() -> Self {
        Self {
            status: WriterStatus::Initializing,
            last_accepted_heartbeat_log_append_time: None,
            connection_ids: HashSet::new(),
        }
    }
}

#[derive(Debug)]
struct RegisteredConnection {
    route: ConnectionRoute,
    traffic_submission_accepted: bool,
    terminal_submission_accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveRoutingGeneration {
    writer_node_id: Arc<str>,
    sequence: u64,
    partitions: Vec<i32>,
}

/// A point-in-time view for the `capture_health` log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticSnapshot {
    /// Writer partitions registered but not yet activated. Computed but deliberately not logged,
    /// matching Java.
    pub initializing_writers: usize,
    /// Writer partitions in the active generation.
    pub active_writers: usize,
    /// Superseded writer partitions still serving existing connections.
    pub draining_writers: usize,
    /// Drained writer partitions whose retirement is in progress.
    pub retiring_writers: usize,
    /// Fully retired writer partitions.
    pub retired_writers: usize,
    /// Registered connections across all writer identities.
    pub active_connections: usize,
    /// Partitions in the active generation.
    pub partitions: usize,
    /// The **maximum** accepted `LogAppendTime` over all writer partitions, for diagnostics only.
    pub last_heartbeat_broker_time: Option<i64>,
}

/// The authoritative capture routing state.
///
/// Every mutator is `&mut self`, so the type itself is a pure state machine and is tested as one.
/// Production shares it as a `Mutex<RoutingState>`, because new connections are routed from
/// connection-accepting threads while everything else runs on the publisher actor. Java reaches the
/// same place with `synchronized` on every mutator.
#[derive(Debug)]
pub struct RoutingState {
    capture_activation_id: Arc<str>,
    topic_partition_count: usize,
    routing_generation_sequence: u64,
    active: Option<ActiveRoutingGeneration>,
    writer_partitions: BTreeMap<WriterPartition, WriterPartitionState>,
    connections: HashMap<(Arc<str>, Arc<str>), RegisteredConnection>,
    next_route_token: u64,
    shutting_down: bool,
}

impl RoutingState {
    /// Creates the routing state for one process activation.
    ///
    /// # Errors
    ///
    /// Rejects a blank `capture_activation_id` — every writer identity and every probe identity is
    /// derived from it — and a zero `topic_partition_count`.
    pub fn new(
        capture_activation_id: impl Into<Arc<str>>,
        topic_partition_count: usize,
    ) -> Result<Self, RoutingError> {
        let capture_activation_id: Arc<str> = capture_activation_id.into();
        if capture_activation_id.trim().is_empty() {
            return Err(RoutingError::InvalidArgument(
                "captureActivationId must not be blank".to_owned(),
            ));
        }
        if topic_partition_count == 0 {
            return Err(RoutingError::InvalidArgument(
                "the traffic topic must have at least one partition".to_owned(),
            ));
        }
        Ok(Self {
            capture_activation_id,
            topic_partition_count,
            routing_generation_sequence: 0,
            active: None,
            writer_partitions: BTreeMap::new(),
            connections: HashMap::new(),
            next_route_token: 1,
            shutting_down: false,
        })
    }

    /// The process activation id every writer identity is derived from.
    #[must_use]
    pub fn capture_activation_id(&self) -> &str {
        &self.capture_activation_id
    }

    /// The highest number of partitions this process knows the topic to have.
    #[must_use]
    pub fn topic_partition_count(&self) -> usize {
        self.topic_partition_count
    }

    /// Raises the known partition count after an out-of-band topic growth.
    ///
    /// Monotonic: a lower count is ignored. Returns whether the count changed.
    ///
    /// # Owner decision OD-7
    ///
    /// The proxy never performs this growth itself. This records a growth an operator performed, so
    /// an assignment containing a new partition can be served instead of terminating capture.
    pub fn raise_topic_partition_count(&mut self, count: usize) -> bool {
        if count > self.topic_partition_count {
            self.topic_partition_count = count;
            true
        } else {
            false
        }
    }

    /// Registers a new generation and advances the writer identity sequence.
    ///
    /// The returned generation is **not** usable: every one of its writer partitions must first
    /// have an initial heartbeat acknowledged and accepted, then
    /// [`RoutingState::activate_routing_generation`] must succeed.
    ///
    /// # Errors
    ///
    /// * [`RoutingError::InvalidArgument`] for an empty or duplicated partition list, a negative
    ///   partition, or sequence overflow.
    /// * [`RoutingError::PartitionAboveKnownCount`] when a partition exceeds the known topic size.
    /// * [`RoutingError::Corrupted`] when a `(writerNodeId, partition)` is already registered,
    ///   which can only happen if the identity sequence repeated.
    pub fn prepare_routing_generation(
        &mut self,
        partitions: &[i32],
    ) -> Result<PendingRoutingGeneration, RoutingError> {
        let validated = self.validate_routing_partitions(partitions)?;
        self.routing_generation_sequence = self
            .routing_generation_sequence
            .checked_add(1)
            .ok_or_else(|| {
                RoutingError::InvalidArgument(
                    "the routing generation sequence overflowed".to_owned(),
                )
            })?;
        let sequence = self.routing_generation_sequence;
        let writer_node_id: Arc<str> =
            Arc::from(format!("{}:{sequence}", self.capture_activation_id));

        for partition in &validated {
            let key = WriterPartition::new(Arc::clone(&writer_node_id), *partition);
            if self.writer_partitions.contains_key(&key) {
                return Err(corrupted(
                    CorruptionCheck::WriterPartitionAlreadyPrepared,
                    key.to_string(),
                ));
            }
            self.writer_partitions
                .insert(key, WriterPartitionState::new());
        }

        Ok(PendingRoutingGeneration {
            writer_node_id,
            sequence,
            partitions: validated,
        })
    }

    fn validate_routing_partitions(&self, partitions: &[i32]) -> Result<Vec<i32>, RoutingError> {
        if partitions.is_empty() {
            return Err(RoutingError::InvalidArgument(
                "a routing generation requires at least one partition".to_owned(),
            ));
        }
        // A BTreeSet reproduces Java's `TreeSet<Integer>`: ascending numeric order, which is the
        // order the routing hash indexes into. Duplicates are a hard error rather than silently
        // collapsed, because a duplicate means the assignment accumulator is wrong.
        let mut unique = BTreeSet::new();
        for partition in partitions {
            self.validate_partition(*partition)?;
            if !unique.insert(*partition) {
                return Err(RoutingError::InvalidArgument(format!(
                    "routing partitions must be unique, but {partition} appears twice"
                )));
            }
        }
        Ok(unique.into_iter().collect())
    }

    fn validate_partition(&self, partition: i32) -> Result<(), RoutingError> {
        if partition < 0 {
            return Err(RoutingError::InvalidArgument(format!(
                "partition {partition} is negative"
            )));
        }
        let known = i32::try_from(self.topic_partition_count).unwrap_or(i32::MAX);
        if partition >= known {
            return Err(RoutingError::PartitionAboveKnownCount {
                partition,
                known_count: self.topic_partition_count,
            });
        }
        Ok(())
    }

    /// Makes `pending` the active generation, superseding any previous one.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] when any writer partition of `pending` is not `Initializing` or
    /// has no accepted heartbeat baseline — the usability rule — or when the outgoing generation
    /// holds a partition that is not `Current`.
    pub fn activate_routing_generation(
        &mut self,
        pending: &PendingRoutingGeneration,
    ) -> Result<Activation, RoutingError> {
        // Re-check the usability rule independently of the publisher's `allOf`. Two independent
        // checks is deliberate: one is a scheduling fact, the other is a state fact.
        for writer_partition in pending.writer_partitions() {
            let state = self.require_writer_partition(&writer_partition)?;
            if state.status != WriterStatus::Initializing
                || state.last_accepted_heartbeat_log_append_time.is_none()
            {
                return Err(corrupted(
                    CorruptionCheck::WriterPartitionNotReadyForActivation,
                    format!(
                        "{writer_partition} status={:?} baseline={:?}",
                        state.status, state.last_accepted_heartbeat_log_append_time
                    ),
                ));
            }
        }

        let previous = self.active.take();
        let (previous_writer_node_id, previous_partitions) = match &previous {
            Some(generation) => {
                for partition in &generation.partitions {
                    let key =
                        WriterPartition::new(Arc::clone(&generation.writer_node_id), *partition);
                    let state = self.require_writer_partition(&key)?;
                    if state.status != WriterStatus::Current {
                        return Err(corrupted(
                            CorruptionCheck::ActiveGenerationHasNonCurrentWriterPartition,
                            format!("{key} status={:?}", state.status),
                        ));
                    }
                }
                for partition in &generation.partitions {
                    let key =
                        WriterPartition::new(Arc::clone(&generation.writer_node_id), *partition);
                    if let Some(state) = self.writer_partitions.get_mut(&key) {
                        state.status = WriterStatus::Draining;
                    }
                }
                (
                    Some(Arc::clone(&generation.writer_node_id)),
                    generation.partitions.clone(),
                )
            }
            None => (None, Vec::new()),
        };

        for writer_partition in pending.writer_partitions() {
            if let Some(state) = self.writer_partitions.get_mut(&writer_partition) {
                state.status = WriterStatus::Current;
            }
        }
        self.active = Some(ActiveRoutingGeneration {
            writer_node_id: pending.writer_node_id_handle(),
            sequence: pending.sequence,
            partitions: pending.partitions.clone(),
        });

        Ok(Activation {
            writer_node_id: pending.writer_node_id_handle(),
            sequence: pending.sequence,
            partitions: pending.partitions.clone(),
            previous_writer_node_id,
            previous_partitions,
        })
    }

    /// Routes a new connection, binding it immutably to a writer identity and partition.
    ///
    /// # Errors
    ///
    /// * [`RoutingError::ShuttingDown`] once retirement or shutdown has begun.
    /// * [`RoutingError::NotReady`] before the first generation is usable. Invokes no policy.
    /// * [`RoutingError::Corrupted`] when the selected partition's writer is not `Current`, or the
    ///   `(writerNodeId, connectionId)` pair is already registered.
    pub fn route_new_connection(
        &mut self,
        connection_id: impl Into<Arc<str>>,
    ) -> Result<ConnectionRoute, RoutingError> {
        if self.shutting_down {
            return Err(RoutingError::ShuttingDown);
        }
        let connection_id: Arc<str> = connection_id.into();
        if connection_id.is_empty() {
            // Java: "connectionId must not be null - partition locality requires a stable key".
            return Err(RoutingError::InvalidArgument(
                "connectionId must not be empty - partition locality requires a stable key"
                    .to_owned(),
            ));
        }
        let active = self.active.clone().ok_or(RoutingError::NotReady)?;

        let partition = select_partition(&connection_id, &active.partitions);
        let writer_partition = WriterPartition::new(Arc::clone(&active.writer_node_id), partition);
        let state = self.require_writer_partition(&writer_partition)?;
        if state.status != WriterStatus::Current {
            return Err(corrupted(
                CorruptionCheck::NewConnectionRoutedToNonCurrentWriterPartition,
                format!("{writer_partition} status={:?}", state.status),
            ));
        }

        let key = (
            Arc::clone(&active.writer_node_id),
            Arc::clone(&connection_id),
        );
        if self.connections.contains_key(&key) {
            return Err(corrupted(
                CorruptionCheck::ConnectionAlreadyRegistered,
                format!(
                    "connection {connection_id} writer {}",
                    active.writer_node_id
                ),
            ));
        }

        let token = self.next_route_token;
        self.next_route_token = self.next_route_token.wrapping_add(1);
        let route = ConnectionRoute {
            writer_node_id: Arc::clone(&active.writer_node_id),
            connection_id: Arc::clone(&connection_id),
            partition,
            token,
        };
        if let Some(state) = self.writer_partitions.get_mut(&writer_partition) {
            state.connection_ids.insert(Arc::clone(&connection_id));
        }
        self.connections.insert(
            key,
            RegisteredConnection {
                route: route.clone(),
                traffic_submission_accepted: false,
                terminal_submission_accepted: false,
            },
        );
        Ok(route)
    }

    /// Accepts one traffic submission against `route`.
    ///
    /// # Errors
    ///
    /// * [`RoutingError::Corrupted`] when the route is not the registered one, when its writer
    ///   partition is already retiring or retired, or when a submission follows the accepted
    ///   terminal record.
    pub fn accept_traffic_submission(
        &mut self,
        route: &ConnectionRoute,
        is_terminal: bool,
    ) -> Result<(), RoutingError> {
        let writer_partition = route.writer_partition();
        let status = self.require_writer_partition(&writer_partition)?.status;
        let key = (route.writer_node_id_handle(), route.connection_id_handle());
        let registered = self
            .connections
            .get_mut(&key)
            .filter(|registered| registered.route.token == route.token)
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::ConnectionRouteNotActive,
                    route.to_string(),
                )
            })?;

        if matches!(status, WriterStatus::Retiring | WriterStatus::Retired) {
            return Err(corrupted(
                CorruptionCheck::TrafficSubmissionReachedRetiredWriterPartition,
                format!("{writer_partition} status={status:?}"),
            ));
        }
        if registered.terminal_submission_accepted {
            return Err(corrupted(
                CorruptionCheck::TrafficSubmissionFollowedTerminalRecord,
                route.to_string(),
            ));
        }
        registered.traffic_submission_accepted = true;
        if is_terminal {
            registered.terminal_submission_accepted = true;
        }
        Ok(())
    }

    /// Removes a connection after Kafka acknowledged its terminal record.
    ///
    /// This is the **only** exit for a connection that published anything, and it runs after the
    /// acknowledgement, not after the submission: the per-connection chain guarantees that is after
    /// every earlier record for the connection.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] when no terminal submission was accepted, when the route is not
    /// registered, or when the per-writer connection set has lost the id.
    pub fn remove_after_terminal_acknowledgement(
        &mut self,
        route: &ConnectionRoute,
    ) -> Result<(), RoutingError> {
        let key = (route.writer_node_id_handle(), route.connection_id_handle());
        let registered = self
            .connections
            .get(&key)
            .filter(|registered| registered.route.token == route.token)
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::ConnectionRouteNotRegistered,
                    route.to_string(),
                )
            })?;
        if !registered.terminal_submission_accepted {
            return Err(corrupted(
                CorruptionCheck::ConnectionRouteHasNoAcceptedTerminalRecord,
                route.to_string(),
            ));
        }
        self.remove_connection(route)
    }

    /// Removes a connection that never published anything.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] when any traffic publication was already accepted: that
    /// connection's records are in Kafka and the replayer will expect its terminal record.
    pub fn abandon_unpublished_connection(
        &mut self,
        route: &ConnectionRoute,
    ) -> Result<(), RoutingError> {
        let key = (route.writer_node_id_handle(), route.connection_id_handle());
        let registered = self
            .connections
            .get(&key)
            .filter(|registered| registered.route.token == route.token)
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::ConnectionRouteNotRegistered,
                    route.to_string(),
                )
            })?;
        if registered.traffic_submission_accepted {
            return Err(corrupted(
                CorruptionCheck::CannotAbandonAfterAcceptedPublication,
                route.to_string(),
            ));
        }
        self.remove_connection(route)
    }

    fn remove_connection(&mut self, route: &ConnectionRoute) -> Result<(), RoutingError> {
        let key = (route.writer_node_id_handle(), route.connection_id_handle());
        if self.connections.remove(&key).is_none() {
            return Err(corrupted(
                CorruptionCheck::ConnectionRouteNotRegistered,
                route.to_string(),
            ));
        }
        let writer_partition = route.writer_partition();
        let state = self
            .writer_partitions
            .get_mut(&writer_partition)
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::UnknownWriterPartition,
                    writer_partition.to_string(),
                )
            })?;
        if !state.connection_ids.remove(route.connection_id()) {
            return Err(corrupted(
                CorruptionCheck::ConnectionRegistryInconsistent,
                route.to_string(),
            ));
        }
        Ok(())
    }

    /// Accepts a heartbeat's broker `LogAppendTime` as the new baseline for `writer_partition`.
    ///
    /// # Owner decision OD-6
    ///
    /// Accepts only when `-S <= candidate - previous < E`. Both bounds reject without changing the
    /// baseline, so a rejected heartbeat cannot move the window it failed.
    ///
    /// # Errors
    ///
    /// * [`RoutingError::BrokerTime`] for a non-positive timestamp, for movement at or beyond `E`,
    ///   and for backward movement worse than `S`.
    /// * [`RoutingError::Corrupted`] for an unknown writer partition or one already `Retired`.
    pub fn accept_heartbeat_log_append_time(
        &mut self,
        writer_partition: &WriterPartition,
        heartbeat_log_append_time: i64,
        expiration: Duration,
        clock_skew_allowance: Duration,
    ) -> Result<(), RoutingError> {
        let expiration_millis = duration_millis(expiration, "E")?;
        if expiration_millis <= 0 {
            return Err(RoutingError::InvalidArgument(
                "the heartbeat expiration interval E must be positive".to_owned(),
            ));
        }
        let skew_millis = duration_millis(clock_skew_allowance, "S")?;
        if heartbeat_log_append_time <= 0 {
            return Err(RoutingError::BrokerTime(format!(
                "Kafka did not assign a positive LogAppendTime to heartbeat {writer_partition}: \
                 {heartbeat_log_append_time}"
            )));
        }
        let state = self.require_writer_partition(writer_partition)?;
        if state.status == WriterStatus::Retired {
            return Err(corrupted(
                CorruptionCheck::HeartbeatAcknowledgementAfterRetirement,
                writer_partition.to_string(),
            ));
        }

        if let Some(previous) = state.last_accepted_heartbeat_log_append_time {
            let elapsed = heartbeat_log_append_time.checked_sub(previous).ok_or_else(|| {
                RoutingError::BrokerTime(format!(
                    "heartbeat broker time for {writer_partition} overflowed: previous={previous}, \
                     candidate={heartbeat_log_append_time}"
                ))
            })?;
            if elapsed >= expiration_millis {
                return Err(RoutingError::BrokerTime(format!(
                    "Heartbeat broker time exceeded the configured expiration interval for \
                     {writer_partition}: previous={previous}, \
                     candidate={heartbeat_log_append_time}, \
                     expirationMillis={expiration_millis}"
                )));
            }
            // DEVIATION: owner decision OD-6. Java has no lower bound here and overwrites the
            // baseline with the smaller value (`CaptureRoutingState.java:465-479`, asserted at
            // `CaptureRoutingStateTest.java:73`). A regressed baseline widens the pre-forward
            // window, because `observation - baseline < E` becomes easier to satisfy, so a stale
            // observation can release a source write. Backward movement is tolerated only within
            // the configured clock-skew allowance `S`, which is the bound the replayer already
            // enforces fatally.
            if elapsed < -skew_millis {
                return Err(RoutingError::BrokerTime(format!(
                    "Heartbeat broker time moved backward beyond the configured clock-skew \
                     allowance for {writer_partition}: previous={previous}, \
                     candidate={heartbeat_log_append_time}, skewAllowanceMillis={skew_millis}"
                )));
            }
        }

        let state = self
            .writer_partitions
            .get_mut(writer_partition)
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::UnknownWriterPartition,
                    writer_partition.to_string(),
                )
            })?;
        state.last_accepted_heartbeat_log_append_time = Some(heartbeat_log_append_time);
        Ok(())
    }

    /// The last accepted heartbeat `LogAppendTime` for `writer_partition`.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] when the writer partition is not registered.
    pub fn last_accepted_heartbeat_log_append_time(
        &self,
        writer_partition: &WriterPartition,
    ) -> Result<Option<i64>, RoutingError> {
        Ok(self
            .require_writer_partition(writer_partition)?
            .last_accepted_heartbeat_log_append_time)
    }

    /// Validates a Critical Mutation Traffic acknowledgement against its writer partition's
    /// heartbeat baseline — the pre-forward freshness gate.
    ///
    /// Both operands are broker `LogAppendTime` milliseconds; no local clock is involved.
    ///
    /// # Owner decision OD-6
    ///
    /// Accepts only when `-S <= observation - baseline < E`, applying the same lower bound as the
    /// heartbeat check.
    ///
    /// # Errors
    ///
    /// * [`RoutingError::BrokerTime`] for a non-positive observation, for an observation at or
    ///   beyond `E` past the baseline, and for one more than `S` before it.
    /// * [`RoutingError::Corrupted`] when the writer partition has no baseline at all: forwarding a
    ///   mutation with no broker-time proof is exactly what this gate exists to prevent.
    pub fn validate_critical_mutation_traffic_acknowledgement(
        &self,
        route: &ConnectionRoute,
        observation_log_append_time: i64,
        expiration: Duration,
        clock_skew_allowance: Duration,
    ) -> Result<(), RoutingError> {
        let expiration_millis = duration_millis(expiration, "E")?;
        let skew_millis = duration_millis(clock_skew_allowance, "S")?;
        if observation_log_append_time <= 0 {
            return Err(RoutingError::BrokerTime(format!(
                "Kafka did not assign a positive LogAppendTime to Critical Mutation Traffic for \
                 {route}: {observation_log_append_time}"
            )));
        }
        let writer_partition = route.writer_partition();
        let state = self.require_writer_partition(&writer_partition)?;
        let baseline = state
            .last_accepted_heartbeat_log_append_time
            .ok_or_else(|| {
                CorruptedCaptureState::new(
                    CorruptionCheck::CriticalMutationBeforeHeartbeatBaseline,
                    route.to_string(),
                )
            })?;
        let elapsed = observation_log_append_time
            .checked_sub(baseline)
            .ok_or_else(|| {
                RoutingError::BrokerTime(format!(
                    "Critical Mutation Traffic broker time for {route} overflowed: \
                     heartbeat={baseline}, observation={observation_log_append_time}"
                ))
            })?;
        if elapsed >= expiration_millis {
            return Err(RoutingError::BrokerTime(format!(
                "Critical Mutation Traffic acknowledgement exceeded the configured heartbeat \
                 expiration interval for {route}: heartbeat={baseline}, \
                 observation={observation_log_append_time}, expirationMillis={expiration_millis}"
            )));
        }
        // DEVIATION: owner decision OD-6 applies the same lower bound here. Java has none
        // (`CaptureRoutingState.java:486-518`), so an observation arbitrarily far before the
        // baseline passes.
        if elapsed < -skew_millis {
            return Err(RoutingError::BrokerTime(format!(
                "Critical Mutation Traffic acknowledgement preceded the heartbeat baseline by more \
                 than the configured clock-skew allowance for {route}: heartbeat={baseline}, \
                 observation={observation_log_append_time}, skewAllowanceMillis={skew_millis}"
            )));
        }
        Ok(())
    }

    /// Selects every `Draining` writer partition with no remaining connections, flips each to
    /// `Retiring`, and returns them sorted by `(writerNodeId, partition)`.
    ///
    /// The deterministic order makes retirement logs and metrics comparable between runs and
    /// between the Java and Rust implementations.
    pub fn prepare_drained_writer_retirements(&mut self) -> Vec<WriterPartition> {
        let drained: Vec<WriterPartition> = self
            .writer_partitions
            .iter()
            .filter(|(_, state)| {
                state.status == WriterStatus::Draining && state.connection_ids.is_empty()
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &drained {
            if let Some(state) = self.writer_partitions.get_mut(key) {
                state.status = WriterStatus::Retiring;
            }
        }
        drained
    }

    /// Completes retirement of `writer_partition`.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] unless the writer partition is `Retiring` with no connections.
    pub fn complete_writer_retirement(
        &mut self,
        writer_partition: &WriterPartition,
    ) -> Result<(), RoutingError> {
        let state = self.require_writer_partition(writer_partition)?;
        if state.status != WriterStatus::Retiring || !state.connection_ids.is_empty() {
            return Err(corrupted(
                CorruptionCheck::WriterPartitionCannotCompleteRetirement,
                format!(
                    "{writer_partition} status={:?} connections={}",
                    state.status,
                    state.connection_ids.len()
                ),
            ));
        }
        if let Some(state) = self.writer_partitions.get_mut(writer_partition) {
            state.status = WriterStatus::Retired;
        }
        Ok(())
    }

    /// Begins orderly retirement of the whole process: the active generation stops being current
    /// and no new connection is admitted.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Corrupted`] when any connection is still registered. Retirement is the
    /// Kafka component's **refusal** to retire early; disconnecting connections and the timing of
    /// that disconnection belong to the proxy layer (spec discrepancy D-19).
    pub fn begin_orderly_retirement(&mut self) -> Result<OrderlyRetirement, RoutingError> {
        if !self.connections.is_empty() {
            return Err(corrupted(
                CorruptionCheck::CannotRetireWhileConnectionsRemain,
                format!("{} connections remain", self.connections.len()),
            ));
        }
        self.shutting_down = true;
        let Some(active) = self.active.take() else {
            return Ok(OrderlyRetirement {
                writer_node_id: None,
                partitions: Vec::new(),
            });
        };
        for partition in &active.partitions {
            let key = WriterPartition::new(Arc::clone(&active.writer_node_id), *partition);
            let state = self.require_writer_partition(&key)?;
            if state.status != WriterStatus::Current {
                return Err(corrupted(
                    CorruptionCheck::CurrentWriterPartitionIsNotCurrent,
                    format!("{key} status={:?}", state.status),
                ));
            }
        }
        for partition in &active.partitions {
            let key = WriterPartition::new(Arc::clone(&active.writer_node_id), *partition);
            if let Some(state) = self.writer_partitions.get_mut(&key) {
                state.status = WriterStatus::Draining;
            }
        }
        Ok(OrderlyRetirement {
            writer_node_id: Some(active.writer_node_id),
            partitions: active.partitions,
        })
    }

    /// Immediately removes routing eligibility, without any retirement bookkeeping.
    ///
    /// Used by close paths, including after a failure, where orderly retirement is not attempted.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.active = None;
    }

    /// Whether retirement or shutdown has begun.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Whether every registered writer partition is `Retired`.
    #[must_use]
    pub fn all_writer_partitions_retired(&self) -> bool {
        self.writer_partitions
            .values()
            .all(|state| state.status == WriterStatus::Retired)
    }

    /// Writer partitions that are not yet `Retired`, sorted, for incomplete-retirement reporting.
    #[must_use]
    pub fn writer_partitions_not_retired(&self) -> Vec<WriterPartition> {
        self.writer_partitions
            .iter()
            .filter(|(_, state)| state.status != WriterStatus::Retired)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Whether `writer_partition` still has registered connections.
    #[must_use]
    pub fn has_connections(&self, writer_partition: &WriterPartition) -> bool {
        self.writer_partitions
            .get(writer_partition)
            .is_some_and(|state| !state.connection_ids.is_empty())
    }

    /// The status of `writer_partition`, if registered.
    #[must_use]
    pub fn writer_status(&self, writer_partition: &WriterPartition) -> Option<WriterStatus> {
        self.writer_partitions
            .get(writer_partition)
            .map(|state| state.status)
    }

    /// The active generation's partitions, ascending, or empty when none is usable.
    #[must_use]
    pub fn active_routing_partitions(&self) -> Vec<i32> {
        self.active
            .as_ref()
            .map(|generation| generation.partitions.clone())
            .unwrap_or_default()
    }

    /// The active writer identity, if a generation is usable.
    #[must_use]
    pub fn active_writer_node_id(&self) -> Option<Arc<str>> {
        self.active
            .as_ref()
            .map(|generation| Arc::clone(&generation.writer_node_id))
    }

    /// Every writer identity ever registered, ascending.
    #[must_use]
    pub fn writer_node_ids(&self) -> BTreeSet<Arc<str>> {
        self.writer_partitions
            .keys()
            .map(WriterPartition::writer_node_id_handle)
            .collect()
    }

    /// Every registered writer partition, sorted.
    #[must_use]
    pub fn writer_partitions(&self) -> Vec<WriterPartition> {
        self.writer_partitions.keys().cloned().collect()
    }

    /// How many connections are registered across all writer identities.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// The current generation sequence. Zero before the first generation is prepared.
    #[must_use]
    pub fn routing_generation_sequence(&self) -> u64 {
        self.routing_generation_sequence
    }

    /// The `capture_health` view.
    #[must_use]
    pub fn diagnostic_snapshot(&self) -> DiagnosticSnapshot {
        let mut snapshot = DiagnosticSnapshot {
            active_connections: self.connections.len(),
            partitions: self.active_routing_partitions().len(),
            ..DiagnosticSnapshot::default()
        };
        for state in self.writer_partitions.values() {
            match state.status {
                WriterStatus::Initializing => snapshot.initializing_writers += 1,
                WriterStatus::Current => snapshot.active_writers += 1,
                WriterStatus::Draining => snapshot.draining_writers += 1,
                WriterStatus::Retiring => snapshot.retiring_writers += 1,
                WriterStatus::Retired => snapshot.retired_writers += 1,
            }
            if let Some(time) = state.last_accepted_heartbeat_log_append_time {
                snapshot.last_heartbeat_broker_time = Some(
                    snapshot
                        .last_heartbeat_broker_time
                        .map_or(time, |m: i64| m.max(time)),
                );
            }
        }
        snapshot
    }

    fn require_writer_partition(
        &self,
        writer_partition: &WriterPartition,
    ) -> Result<&WriterPartitionState, RoutingError> {
        self.writer_partitions.get(writer_partition).ok_or_else(|| {
            corrupted(
                CorruptionCheck::UnknownWriterPartition,
                writer_partition.to_string(),
            )
        })
    }
}

fn duration_millis(duration: Duration, name: &'static str) -> Result<i64, RoutingError> {
    i64::try_from(duration.as_millis()).map_err(|_| {
        RoutingError::InvalidArgument(format!("capture interval {name} is implausibly large"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: Duration = Duration::from_secs(30);
    const S: Duration = Duration::from_secs(5);

    fn state() -> RoutingState {
        RoutingState::new("activation", 4).expect("valid activation")
    }

    /// Prepares a generation and accepts an initial heartbeat on each of its partitions.
    fn activate(
        routing: &mut RoutingState,
        partitions: &[i32],
        baseline: i64,
    ) -> PendingRoutingGeneration {
        let pending = routing
            .prepare_routing_generation(partitions)
            .expect("valid partitions");
        for writer_partition in pending.writer_partitions() {
            routing
                .accept_heartbeat_log_append_time(&writer_partition, baseline, E, S)
                .expect("initial heartbeat");
        }
        routing
            .activate_routing_generation(&pending)
            .expect("activation");
        pending
    }

    #[test]
    fn the_first_generation_is_sequence_one_and_identities_never_repeat() {
        // CaptureRoutingStateTest.java:25, :44-57.
        let mut routing = state();
        let first = routing
            .prepare_routing_generation(&[0, 2])
            .expect("valid partitions");
        assert_eq!(first.sequence(), 1);
        assert_eq!(first.writer_node_id(), "activation:1");
        assert_eq!(
            first.partitions(),
            [0, 2],
            "the list is ascending and deduplicated"
        );

        let second = routing
            .prepare_routing_generation(&[1])
            .expect("valid partitions");
        assert_eq!(second.writer_node_id(), "activation:2");

        // A failed preparation still consumes a sequence number, so identities skip but never
        // repeat.
        assert!(routing.prepare_routing_generation(&[]).is_err());
        let third = routing
            .prepare_routing_generation(&[3])
            .expect("valid partitions");
        assert_eq!(
            third.writer_node_id(),
            "activation:3",
            "an empty list is rejected before the sequence advances"
        );
    }

    #[test]
    fn the_partition_list_is_sorted_ascending_whatever_order_kafka_supplies() {
        let mut routing = RoutingState::new("activation", 8).expect("valid");
        let pending = routing
            .prepare_routing_generation(&[5, 0, 3, 1])
            .expect("valid partitions");
        assert_eq!(pending.partitions(), [0, 1, 3, 5]);
    }

    #[test]
    fn a_duplicated_partition_is_a_hard_error() {
        // CaptureRoutingState.java:664. A duplicate means the assignment accumulator is wrong, and
        // silently collapsing it would change the routing hash's modulus.
        let mut routing = state();
        let error = routing
            .prepare_routing_generation(&[1, 1])
            .expect_err("duplicates are rejected");
        assert!(matches!(error, RoutingError::InvalidArgument(_)));
        assert!(error.to_string().contains("unique"));
    }

    #[test]
    fn a_routing_generation_is_not_usable_until_every_initial_heartbeat_is_accepted() {
        // CaptureRoutingStateTest.routingGenerationIsNotUsableUntilEveryInitialHeartbeatIsAcknowledged
        // (:20-41).
        let mut routing = state();
        let pending = routing
            .prepare_routing_generation(&[0, 2])
            .expect("valid partitions");
        assert!(
            routing.active_routing_partitions().is_empty(),
            "nothing is routable yet"
        );

        let partitions = pending.writer_partitions();
        routing
            .accept_heartbeat_log_append_time(&partitions[0], 1_000, E, S)
            .expect("partition 0's heartbeat");

        let error = routing
            .activate_routing_generation(&pending)
            .expect_err("partition 2 has no baseline");
        assert!(error.is_corruption(), "this is a state corruption");
        assert!(routing.active_routing_partitions().is_empty());

        routing
            .accept_heartbeat_log_append_time(&partitions[1], 1_000, E, S)
            .expect("partition 2's heartbeat");
        let activation = routing
            .activate_routing_generation(&pending)
            .expect("both baselines established");
        assert_eq!(activation.partitions, vec![0, 2]);
        assert_eq!(routing.active_routing_partitions(), vec![0, 2]);
    }

    #[test]
    fn writer_partitions_maintain_independent_continuous_baselines() {
        // CaptureRoutingStateTest.java:62-77.
        let mut routing = state();
        let pending = activate(&mut routing, &[0, 1], 1_000);
        let partitions = pending.writer_partitions();

        routing
            .accept_heartbeat_log_append_time(&partitions[0], 30_999, E, S)
            .expect("30_999 - 1_000 = 29_999 < E");
        assert_eq!(
            routing
                .last_accepted_heartbeat_log_append_time(&partitions[0])
                .expect("registered"),
            Some(30_999)
        );
        assert_eq!(
            routing
                .last_accepted_heartbeat_log_append_time(&partitions[1])
                .expect("registered"),
            Some(1_000),
            "the other partition's baseline is untouched"
        );
    }

    #[test]
    fn a_heartbeat_at_the_expiration_boundary_is_rejected_without_changing_the_baseline() {
        // CaptureRoutingStateTest.heartbeatAtExpirationBoundaryIsRejectedWithoutChangingBaseline
        // (:80-89). The comparison is `>= E`, so exactly E is a rejection.
        let mut routing = state();
        let pending = activate(&mut routing, &[0], 1_000);
        let writer_partition = &pending.writer_partitions()[0];

        let error = routing
            .accept_heartbeat_log_append_time(writer_partition, 31_000, E, S)
            .expect_err("exactly E is a rejection");
        assert!(matches!(error, RoutingError::BrokerTime(_)));
        assert_eq!(
            routing
                .last_accepted_heartbeat_log_append_time(writer_partition)
                .expect("registered"),
            Some(1_000),
            "a rejected heartbeat must not move the window it failed"
        );
    }

    #[test]
    fn a_non_positive_heartbeat_timestamp_is_rejected() {
        // CaptureRoutingStateTest.java:92.
        let mut routing = state();
        let pending = routing.prepare_routing_generation(&[0]).expect("valid");
        let writer_partition = &pending.writer_partitions()[0];
        for candidate in [0_i64, -1, i64::MIN] {
            let error = routing
                .accept_heartbeat_log_append_time(writer_partition, candidate, E, S)
                .expect_err("non-positive broker time is not a proof of anything");
            assert!(matches!(error, RoutingError::BrokerTime(_)));
        }
    }

    #[test]
    fn backward_broker_time_within_the_skew_allowance_is_accepted() {
        // Owner decision OD-6. Java accepts any backward movement
        // (CaptureRoutingStateTest.java:73 asserts 1_500 after 2_000); the Rust port accepts it
        // only within S.
        let mut routing = state();
        let pending = activate(&mut routing, &[0], 2_000);
        let writer_partition = &pending.writer_partitions()[0];

        routing
            .accept_heartbeat_log_append_time(writer_partition, 1_500, E, S)
            .expect("500 ms backward is inside the 5 s allowance");
        assert_eq!(
            routing
                .last_accepted_heartbeat_log_append_time(writer_partition)
                .expect("registered"),
            Some(1_500),
            "the baseline tracks broker time, bounded below by the skew allowance"
        );
    }

    #[test]
    fn backward_broker_time_beyond_the_skew_allowance_is_rejected() {
        // Owner decision OD-6, the case Java accepts and which widens the pre-forward window.
        let mut routing = state();
        let pending = activate(&mut routing, &[0], 20_000);
        let writer_partition = &pending.writer_partitions()[0];

        let error = routing
            .accept_heartbeat_log_append_time(writer_partition, 14_999, E, S)
            .expect_err("5_001 ms backward exceeds the 5 s allowance");
        assert!(matches!(error, RoutingError::BrokerTime(_)));
        assert!(error.to_string().contains("backward"));
        assert_eq!(
            routing
                .last_accepted_heartbeat_log_append_time(writer_partition)
                .expect("registered"),
            Some(20_000),
            "the baseline does not regress on a rejected heartbeat"
        );

        // Exactly S backward is accepted: the bound is `-S <= elapsed`.
        routing
            .accept_heartbeat_log_append_time(writer_partition, 15_000, E, S)
            .expect("exactly -S is inside the allowance");
    }

    #[test]
    fn a_zero_skew_allowance_rejects_any_backward_movement() {
        let mut routing = state();
        let pending = activate(&mut routing, &[0], 5_000);
        let writer_partition = &pending.writer_partitions()[0];
        assert!(
            routing
                .accept_heartbeat_log_append_time(writer_partition, 4_999, E, Duration::ZERO)
                .is_err(),
            "S = 0 means tolerate no backward movement at all"
        );
        routing
            .accept_heartbeat_log_append_time(writer_partition, 5_000, E, Duration::ZERO)
            .expect("no movement is not backward movement");
    }

    #[test]
    fn critical_mutation_freshness_uses_broker_time_on_both_sides() {
        // CaptureRoutingStateTest.java:104-120: 30_999 accepted, 900 accepted, 31_000 rejected,
        // 0 rejected — all against a 1_000 baseline with E = 30 s.
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing
            .route_new_connection("conn-1")
            .expect("a generation is active");

        routing
            .validate_critical_mutation_traffic_acknowledgement(&route, 30_999, E, S)
            .expect("29_999 < E");
        routing
            .validate_critical_mutation_traffic_acknowledgement(&route, 900, E, S)
            .expect("100 ms before the baseline is inside S");

        assert!(
            routing
                .validate_critical_mutation_traffic_acknowledgement(&route, 31_000, E, S)
                .is_err(),
            "exactly E past the baseline is a rejection"
        );
        assert!(
            routing
                .validate_critical_mutation_traffic_acknowledgement(&route, 0, E, S)
                .is_err(),
            "a non-positive observation proves nothing"
        );
    }

    #[test]
    fn critical_mutation_validation_applies_the_od6_lower_bound() {
        // Owner decision OD-6 extends the lower bound to the pre-forward check. Java accepts an
        // observation arbitrarily far before the baseline.
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 100_000);
        let route = routing.route_new_connection("conn-1").expect("active");

        routing
            .validate_critical_mutation_traffic_acknowledgement(&route, 95_000, E, S)
            .expect("exactly -S is accepted");
        let error = routing
            .validate_critical_mutation_traffic_acknowledgement(&route, 94_999, E, S)
            .expect_err("more than S before the baseline is rejected");
        assert!(matches!(error, RoutingError::BrokerTime(_)));
    }

    #[test]
    fn critical_mutation_validation_without_a_baseline_is_corruption() {
        // CaptureRoutingState.java:500-503. Forwarding a mutation with no broker-time proof is
        // precisely what this gate prevents, so reaching it without a baseline is not an ordinary
        // failure.
        let mut routing = state();
        let pending = activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing.route_new_connection("conn-1").expect("active");

        // Build a second generation whose baseline was never established, and a route against it.
        let orphan = routing.prepare_routing_generation(&[0]).expect("valid");
        assert_ne!(orphan.writer_node_id(), pending.writer_node_id());
        let orphan_route = ConnectionRoute {
            writer_node_id: orphan.writer_node_id_handle(),
            connection_id: route.connection_id_handle(),
            partition: 0,
            token: route.token(),
        };
        let error = routing
            .validate_critical_mutation_traffic_acknowledgement(&orphan_route, 1_500, E, S)
            .expect_err("no baseline");
        assert!(error.is_corruption());
    }

    #[test]
    fn a_replacement_generation_changes_only_new_connection_routing() {
        // CaptureRoutingStateTest.java:122-137.
        let mut routing = state();
        let first = activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let existing = routing.route_new_connection("existing").expect("active");
        let existing_partition = existing.partition();
        let existing_writer = existing.writer_node_id().to_owned();

        let second = activate(&mut routing, &[0, 1, 2, 3], 2_000);
        assert_ne!(second.writer_node_id(), first.writer_node_id());

        // The existing route is unchanged, and still accepts submissions through its draining
        // writer partition.
        assert_eq!(existing.partition(), existing_partition);
        assert_eq!(existing.writer_node_id(), existing_writer);
        routing
            .accept_traffic_submission(&existing, false)
            .expect("a draining writer still serves its existing connections");
        assert_eq!(
            routing.writer_status(&existing.writer_partition()),
            Some(WriterStatus::Draining)
        );

        let fresh = routing.route_new_connection("fresh").expect("active");
        assert_eq!(fresh.writer_node_id(), second.writer_node_id());
        assert_eq!(
            routing.writer_status(&fresh.writer_partition()),
            Some(WriterStatus::Current)
        );
    }

    #[test]
    fn connection_identity_is_local_to_writer_identity() {
        // CaptureRoutingStateTest.connectionIdentityIsLocalToWriterIdentity (:138-158): the same
        // connection id registered under two generations is two registrations.
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let first = routing.route_new_connection("same-id").expect("active");

        activate(&mut routing, &[0, 1, 2, 3], 2_000);
        let second = routing.route_new_connection("same-id").expect("active");

        assert_ne!(first.writer_node_id(), second.writer_node_id());
        assert_eq!(
            routing.connection_count(),
            2,
            "the registry key is (writerNodeId, connectionId)"
        );
    }

    #[test]
    fn a_duplicate_registration_under_one_writer_is_corruption() {
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        routing.route_new_connection("dup").expect("active");
        let error = routing
            .route_new_connection("dup")
            .expect_err("the same id twice under one writer");
        assert!(error.is_corruption());
    }

    #[test]
    fn a_connection_remains_registered_until_its_terminal_record_is_acknowledged() {
        // CaptureRoutingStateTest.connectionRemainsRegisteredUntilTerminalRecordAcknowledgement
        // (:161-183).
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing.route_new_connection("conn").expect("active");

        routing
            .accept_traffic_submission(&route, false)
            .expect("ordinary record");
        assert!(
            routing
                .remove_after_terminal_acknowledgement(&route)
                .is_err(),
            "no terminal submission has been accepted"
        );

        routing
            .accept_traffic_submission(&route, true)
            .expect("terminal record");
        assert!(
            routing.accept_traffic_submission(&route, false).is_err(),
            "nothing may follow the accepted terminal record"
        );
        assert_eq!(routing.connection_count(), 1);

        routing
            .remove_after_terminal_acknowledgement(&route)
            .expect("terminal acknowledgement");
        assert_eq!(routing.connection_count(), 0);
        assert!(
            routing
                .remove_after_terminal_acknowledgement(&route)
                .is_err(),
            "double removal is corruption"
        );
    }

    #[test]
    fn only_a_connection_with_no_accepted_publication_may_be_abandoned() {
        // CaptureRoutingStateTest.onlyAConnectionWithNoAcceptedPublicationMayBeAbandoned (:186-198).
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);

        let unpublished = routing.route_new_connection("quiet").expect("active");
        routing
            .abandon_unpublished_connection(&unpublished)
            .expect("nothing was ever published for it");
        assert_eq!(routing.connection_count(), 0);

        let published = routing.route_new_connection("noisy").expect("active");
        routing
            .accept_traffic_submission(&published, false)
            .expect("one record");
        let error = routing
            .abandon_unpublished_connection(&published)
            .expect_err("its records are already in Kafka");
        assert!(error.is_corruption());
    }

    #[test]
    fn a_stale_route_object_is_rejected_by_identity_not_by_value() {
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing.route_new_connection("conn").expect("active");
        let forged = ConnectionRoute {
            token: route.token().wrapping_add(1),
            ..route.clone()
        };
        let error = routing
            .accept_traffic_submission(&forged, false)
            .expect_err("a different registration");
        assert!(error.is_corruption());
        routing
            .accept_traffic_submission(&route, false)
            .expect("the registered route still works");
    }

    #[test]
    fn a_draining_writer_retires_only_after_its_connections_are_gone() {
        // CaptureRoutingStateTest.java:201-232.
        let mut routing = state();
        let first = activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing.route_new_connection("conn").expect("active");
        routing
            .accept_traffic_submission(&route, true)
            .expect("terminal record");

        activate(&mut routing, &[0, 1, 2, 3], 2_000);
        let drained = routing.prepare_drained_writer_retirements();
        assert!(
            !drained.contains(&route.writer_partition()),
            "the partition still holding a connection is not drained"
        );
        // Its three sibling partitions have no connections, so they drain immediately.
        assert_eq!(drained.len(), 3);
        assert!(
            drained
                .iter()
                .all(|wp| wp.writer_node_id() == first.writer_node_id())
        );
        assert!(
            drained.windows(2).all(|w| w[0] <= w[1]),
            "retirements are returned in a deterministic sorted order"
        );

        routing
            .remove_after_terminal_acknowledgement(&route)
            .expect("terminal acknowledgement");
        let drained = routing.prepare_drained_writer_retirements();
        assert_eq!(drained, vec![route.writer_partition()]);
        routing
            .complete_writer_retirement(&route.writer_partition())
            .expect("retiring and empty");
        assert_eq!(
            routing.writer_status(&route.writer_partition()),
            Some(WriterStatus::Retired)
        );
    }

    #[test]
    fn a_retired_writer_partition_refuses_further_traffic_and_heartbeats() {
        let mut routing = state();
        let first = activate(&mut routing, &[0], 1_000);
        let writer_partition = first.writer_partitions()[0].clone();
        activate(&mut routing, &[0], 2_000);
        assert_eq!(
            routing.prepare_drained_writer_retirements(),
            vec![writer_partition.clone()]
        );
        routing
            .complete_writer_retirement(&writer_partition)
            .expect("retires");

        let error = routing
            .accept_heartbeat_log_append_time(&writer_partition, 3_000, E, S)
            .expect_err("retired");
        assert!(error.is_corruption());
    }

    #[test]
    fn orderly_retirement_requires_an_empty_connection_registry() {
        // CaptureRoutingStateTest.orderlyRetirementRequiresAnEmptyConnectionRegistry (:233-253).
        // Spec discrepancy D-19: refusing is this layer's job; disconnecting is the proxy's.
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        let route = routing.route_new_connection("conn").expect("active");

        let error = routing
            .begin_orderly_retirement()
            .expect_err("a connection remains");
        assert!(error.is_corruption());
        assert!(
            !routing.is_shutting_down(),
            "a refused retirement must not half-close routing"
        );

        routing
            .abandon_unpublished_connection(&route)
            .expect("nothing published");
        let retirement = routing
            .begin_orderly_retirement()
            .expect("the registry is empty");
        assert_eq!(retirement.partitions, vec![0, 1, 2, 3]);
        assert!(routing.is_shutting_down());
        assert!(matches!(
            routing.route_new_connection("late"),
            Err(RoutingError::ShuttingDown)
        ));
    }

    #[test]
    fn shutdown_removes_routing_eligibility_immediately() {
        // CaptureRoutingStateTest.java:256-264.
        let mut routing = state();
        activate(&mut routing, &[0, 1, 2, 3], 1_000);
        routing.begin_shutdown();
        assert!(routing.active_routing_partitions().is_empty());
        assert!(matches!(
            routing.route_new_connection("late"),
            Err(RoutingError::ShuttingDown)
        ));
    }

    #[test]
    fn routing_before_the_first_generation_is_refused_without_invoking_any_policy() {
        // Spec discrepancy D-5: the refusal carries no capture failure.
        let mut routing = state();
        let error = routing
            .route_new_connection("early")
            .expect_err("no generation yet");
        assert!(matches!(error, RoutingError::NotReady));
        assert!(
            !error.is_corruption(),
            "an early connection is not evidence of corruption"
        );
        assert_eq!(
            error.into_capture_failure().reason(),
            FailureReason::Other,
            "the caller decides to refuse the connection; it does not have to fail capture"
        );
    }

    #[test]
    fn invalid_construction_and_durations_fail_loudly() {
        // CaptureRoutingStateTest.java:265.
        assert!(RoutingState::new("   ", 4).is_err(), "blank activation id");
        assert!(RoutingState::new("activation", 0).is_err(), "no partitions");

        let mut routing = state();
        let pending = routing.prepare_routing_generation(&[0]).expect("valid");
        let writer_partition = &pending.writer_partitions()[0];
        assert!(
            routing
                .accept_heartbeat_log_append_time(writer_partition, 1_000, Duration::ZERO, S)
                .is_err(),
            "a zero expiration interval is not a configuration"
        );
    }

    #[test]
    fn an_out_of_range_partition_reports_a_growth_rather_than_corruption() {
        // DEVIATION, spec discrepancy D-15 under owner decision OD-7: an out-of-band partition
        // increase is the intended operational path, so an assignment containing a new partition
        // must not terminate capture.
        let mut routing = RoutingState::new("activation", 2).expect("valid");
        let error = routing
            .prepare_routing_generation(&[0, 1, 2])
            .expect_err("partition 2 is above the known count");
        match error {
            RoutingError::PartitionAboveKnownCount {
                partition,
                known_count,
            } => {
                assert_eq!(partition, 2);
                assert_eq!(known_count, 2);
            }
            other => panic!("expected a growth report, got {other:?}"),
        }

        assert!(routing.raise_topic_partition_count(3), "count grows");
        assert!(
            !routing.raise_topic_partition_count(2),
            "the known count never shrinks"
        );
        let pending = routing
            .prepare_routing_generation(&[0, 1, 2])
            .expect("the refreshed count admits partition 2");
        assert_eq!(pending.partitions(), [0, 1, 2]);
        assert!(
            routing.prepare_routing_generation(&[-1]).is_err(),
            "a negative partition is always invalid"
        );
    }

    #[test]
    fn the_diagnostic_snapshot_reports_the_maximum_baseline_and_every_status() {
        let mut routing = state();
        let first = activate(&mut routing, &[0, 1], 1_000);
        routing
            .accept_heartbeat_log_append_time(&first.writer_partitions()[1], 5_000, E, S)
            .expect("advances only that partition");
        let route = routing.route_new_connection("conn").expect("active");
        activate(&mut routing, &[2, 3], 6_000);
        for writer_partition in routing.prepare_drained_writer_retirements() {
            routing
                .complete_writer_retirement(&writer_partition)
                .expect("drained");
        }

        let snapshot = routing.diagnostic_snapshot();
        assert_eq!(snapshot.active_writers, 2, "the new generation");
        assert_eq!(snapshot.active_connections, 1);
        assert_eq!(snapshot.partitions, 2);
        assert_eq!(
            snapshot.last_heartbeat_broker_time,
            Some(6_000),
            "the health line reports the maximum over all writer partitions"
        );
        assert_eq!(
            snapshot.draining_writers + snapshot.retired_writers,
            2,
            "the superseded generation's two partitions"
        );
        assert!(
            routing.has_connections(&route.writer_partition()),
            "the connection keeps its draining partition alive"
        );
    }
}
