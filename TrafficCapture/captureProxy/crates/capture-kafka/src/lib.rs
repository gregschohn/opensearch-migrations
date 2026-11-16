//! Kafka capability probing, group membership, routing generations, publisher lanes, heartbeats,
//! and retirement.
//!
//! See `docs/captureAndReplay/proxyCaptureProtocol.md` sections 3, 4, 5 and 7.
//!
//! # What this crate guarantees
//!
//! * A connection's partition and writer identity are chosen once and never change, bit-for-bit
//!   compatibly with the Java proxy ([`routing_hash`]).
//! * A routing generation is usable only after an initial heartbeat has been acknowledged **and
//!   accepted** on **every** assigned partition ([`routing_state`], [`publisher_actor`]).
//! * There is one process-wide order between admitted submissions and the first terminal capture
//!   failure, and it is irreversible ([`write_gate`]).
//! * A heartbeat acknowledgement that arrived before `E` wins even if the publisher is backlogged
//!   past `E`, and the component that notices a *missing* acknowledgement cannot be starved by the
//!   publisher ([`ack_deadline`]).
//! * No capture record ever carries a Kafka header, and no payload carries a partition.
//! * Internal state corruption terminates the process under **both** capture-failure policies
//!   ([`error`]).
//!
//! # Layout
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`error`] | KAFKA-07 | the two terminal classes and the bounded failure reasons |
//! | [`metrics`] | — | the frozen metric name and attribute contract |
//! | [`write_gate`] | KAFKA-04 | the one-way process-wide capture gate |
//! | [`ack_deadline`] | KAFKA-03 | the four-state deadline arbiter and its independent clock |
//! | [`routing_hash`] | KAFKA-05 | Java-compatible Murmur2 partition selection |
//! | [`routing_state`] | KAFKA-06 | generations, writer identities, connection routes, baselines |
//! | [`budget`] | — | bounded publication memory with a reserved control reservation |
//! | [`publisher_lane`] | KAFKA-03 | per-`(writer, partition)` lane state |
//! | [`records`] | KAFKA-01, KAFKA-03 | heartbeat and probe record construction |
//! | [`client_config`] | KAFKA-09 | allowlisted client configuration and auth |
//! | [`msk_iam`] | KAFKA-09 | SASL/OAUTHBEARER `SigV4` tokens for MSK IAM |
//! | [`topic_metadata`] | KAFKA-10 | layout discovery and validation, never mutation |
//! | [`capability_probe`] | KAFKA-01 | timestamp-type qualification of the traffic topic |
//! | [`membership`] | KAFKA-02 | the paused consumer and the last usable assignment |
//! | [`publisher_actor`] | KAFKA-03 | single-owner publication, heartbeats, retirement |
//! | [`bootstrap`] | KAFKA-08 | safe startup ordering and readiness |
//! | [`connection_factory`] | WIRE-04, WIRE-05 | per-connection routing and the record sink |
//! | [`shutdown`] | WIRE-06 | orderly retirement and incomplete-retirement reporting |
//!
//! # Binding owner decisions
//!
//! * **OD-4** — the capability probe validates the timestamp **type**: an acknowledgement is
//!   accepted only when the delivery report reports `Timestamp::LogAppendTime(t)` with `t > 0`. The
//!   documented "produce with timestamp zero and require a positive timestamp" check is impossible
//!   through librdkafka, which substitutes the client wall clock for a zero timestamp, so
//!   "positive" would falsely pass on a `CreateTime` topic. See [`capability_probe`].
//! * **OD-5** — a non-`LogAppendTime` traffic topic refuses startup, before the listener opens.
//! * **OD-6** — backward broker-time movement beyond the clock-skew allowance `S` is rejected. See
//!   [`routing_state::RoutingState::accept_heartbeat_log_append_time`].
//! * **OD-7** — the proxy **never** changes the partition count and performs **no** admin
//!   operation. Layout is validated, never mutated. See [`topic_metadata`].
//!
//! Every deliberate divergence from the Java implementation is marked with a `DEVIATION:` comment
//! at its site.

pub mod ack_deadline;
pub mod bootstrap;
pub mod budget;
pub mod capability_probe;
pub mod client_config;
pub mod connection_factory;
pub mod error;
pub mod membership;
pub mod metrics;
pub mod msk_iam;
pub mod publisher_actor;
pub mod publisher_lane;
pub mod records;
pub mod routing_hash;
pub mod routing_state;
pub mod shutdown;
pub mod topic_metadata;
pub mod write_gate;

pub use error::{
    CaptureFailure, ClientConfigError, CorruptedCaptureState, CorruptionCheck, FailureClass,
    FailureReason, StartupRefusal, TopicLayoutError,
};
pub use metrics::{CaptureMetrics, NoopMetrics, PublishOutcome, RecordKind, RoutingTransition};
pub use routing_hash::select_partition;
pub use write_gate::WriteGate;
