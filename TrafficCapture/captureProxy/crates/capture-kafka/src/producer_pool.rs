//! Which librdkafka producer publishes each partition (owner decision OD-12).
//!
//! # Why several producers
//!
//! librdkafka sends one partition per `ProduceRequest`, and idempotence caps every producer at five
//! requests in flight per broker connection. Partitions led by the **same** broker and published by
//! the **same** producer therefore compete for those five slots, while partitions on different
//! brokers never do (each has its own connection). Java's client packs every partition led by a
//! broker into one request and never hits this (Phase 1 review §5.5 and §8).
//!
//! # Sizing
//!
//! [`ProducerSizing::Auto`] uses `min(cap, the most generation partitions led by one broker)`
//! producers. With production's layout (four partitions per proxy over twelve brokers) that is one;
//! with four partitions on one broker it is four. The cap bounds the cost: each producer owns
//! librdkafka threads (one per broker plus a few) and about 8 MiB of memory (measured).
//! [`ProducerSizing::Fixed`] is the operator override.
//!
//! # Ownership: write-once, never moved
//!
//! `captureAndReplayArchitecture.md` §5.1 permits several producers only when each
//! `(writerNodeId, partition)` has exactly one producer, ownership never overlaps, and any handoff
//! drains first. This module satisfies that by construction: a partition's producer is chosen once,
//! the first time the partition is seen (by a generation's [`ProducerAssignment::assign`] or, failing
//! that, by its first record), and **never changes for the life of the process**. There is no
//! handoff to drain. A later leader change or rebalance can make the spread less even; it can never
//! make two producers publish one partition.
//!
//! # Spreading
//!
//! New partitions are spread **per leader broker**: each one goes to the producer that has the
//! fewest of this generation's partitions on the same leader. A mapping by `partition mod N` is
//! deliberately avoided: Kafka typically places partition `p`'s leader on broker `(start + p) mod B`,
//! so when `N` divides `B` every partition of one broker would share one producer and the extra
//! producers would buy nothing.

use std::collections::{BTreeMap, HashMap};

/// The automatic sizing cap (owner direction: capped at four).
pub const AUTO_PRODUCER_CAP: usize = 4;

/// The largest fixed producer count an operator may request.
pub const MAX_FIXED_PRODUCERS: usize = 64;

/// How many producers to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerSizing {
    /// `min(cap, most generation partitions led by one broker)`, at least one.
    Auto {
        /// The most producers automatic sizing will create.
        cap: usize,
    },
    /// Exactly this many producers, whatever the layout.
    Fixed(usize),
}

impl Default for ProducerSizing {
    fn default() -> Self {
        Self::Auto {
            cap: AUTO_PRODUCER_CAP,
        }
    }
}

impl ProducerSizing {
    /// The producers to start with, before any partition is known: one for automatic sizing (it
    /// serves metadata and the capability probe), all of them for a fixed count.
    #[must_use]
    pub fn initial(self) -> usize {
        match self {
            Self::Auto { .. } => 1,
            Self::Fixed(count) => count.clamp(1, MAX_FIXED_PRODUCERS),
        }
    }

    /// The producers a generation with this leader layout should be spread over.
    fn target(self, most_on_one_leader: usize) -> usize {
        match self {
            Self::Auto { cap } => most_on_one_leader.clamp(1, cap.max(1)),
            Self::Fixed(count) => count.clamp(1, MAX_FIXED_PRODUCERS),
        }
    }
}

/// What [`ProducerAssignment::assign`] decided for one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentPlan {
    /// Producers the generation was spread over (`0..spread`).
    pub spread: usize,
    /// Producers the sink must own afterwards: at least `spread`, and never fewer than before, since
    /// an earlier generation's partitions keep their producers.
    pub required: usize,
    /// The partitions this call pinned, with their producer, in ascending partition order.
    pub pinned: Vec<(i32, usize)>,
}

/// The write-once partition → producer table.
#[derive(Debug, Default, Clone)]
pub struct ProducerAssignment {
    owner: HashMap<i32, usize>,
}

impl ProducerAssignment {
    /// The producer that owns `partition`, if one has been chosen.
    #[must_use]
    pub fn owner_of(&self, partition: i32) -> Option<usize> {
        self.owner.get(&partition).copied()
    }

    /// The owner of `partition`, pinning it to producer 0 if it has none yet.
    ///
    /// The fallback for a record whose partition no generation announced first (the capability
    /// probe at startup). Pinning on first use keeps the never-moves rule unconditional.
    pub fn owner_or_pin_first(&mut self, partition: i32) -> usize {
        *self.owner.entry(partition).or_insert(0)
    }

    /// The highest producer index any partition is pinned to, plus one.
    #[must_use]
    pub fn producers_in_use(&self) -> usize {
        self.owner
            .values()
            .map(|index| index + 1)
            .max()
            .unwrap_or(0)
    }

    /// Chooses a producer for every partition of a generation that has none yet.
    ///
    /// `generation` lists each partition with its current leader broker, or `None` when the leader
    /// is unknown (metadata unavailable): unknown leaders are treated as one broker, which is the
    /// conservative spread. Partitions already pinned keep their producer and count toward the load
    /// the new ones are balanced against. `producers_owned` is how many the sink owns now.
    pub fn assign(
        &mut self,
        generation: &[(i32, Option<i32>)],
        sizing: ProducerSizing,
        producers_owned: usize,
    ) -> AssignmentPlan {
        // Partitions by leader, each list ascending, so the plan is deterministic.
        let mut by_leader: BTreeMap<Option<i32>, Vec<i32>> = BTreeMap::new();
        for &(partition, leader) in generation {
            by_leader.entry(leader).or_default().push(partition);
        }
        for partitions in by_leader.values_mut() {
            partitions.sort_unstable();
            partitions.dedup();
        }
        let most_on_one_leader = by_leader.values().map(Vec::len).max().unwrap_or(0);
        let spread = sizing.target(most_on_one_leader);

        // This generation's load per producer, overall, from partitions already pinned.
        let mut total = vec![0_usize; spread];
        for partitions in by_leader.values() {
            for partition in partitions {
                if let Some(index) = self.owner_of(*partition)
                    && index < spread
                {
                    total[index] += 1;
                }
            }
        }

        let mut pinned = Vec::new();
        for partitions in by_leader.values() {
            let mut on_leader = vec![0_usize; spread];
            for partition in partitions {
                if let Some(index) = self.owner_of(*partition)
                    && index < spread
                {
                    on_leader[index] += 1;
                }
            }
            for &partition in partitions {
                if self.owner.contains_key(&partition) {
                    continue;
                }
                let choice = (0..spread)
                    .min_by_key(|&index| (on_leader[index], total[index], index))
                    .unwrap_or(0);
                self.owner.insert(partition, choice);
                on_leader[choice] += 1;
                total[choice] += 1;
                pinned.push((partition, choice));
            }
        }
        pinned.sort_unstable();

        AssignmentPlan {
            spread,
            required: spread.max(producers_owned).max(self.producers_in_use()),
            pinned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> ProducerSizing {
        ProducerSizing::Auto {
            cap: AUTO_PRODUCER_CAP,
        }
    }

    /// `partitions` with leaders placed the way Kafka's default assignment does: `p mod brokers`.
    fn round_robin(partitions: std::ops::Range<i32>, brokers: i32) -> Vec<(i32, Option<i32>)> {
        partitions.map(|p| (p, Some(p % brokers))).collect()
    }

    /// For each leader, how many of its partitions each producer got.
    fn spread_per_leader(
        assignment: &ProducerAssignment,
        generation: &[(i32, Option<i32>)],
        producers: usize,
    ) -> BTreeMap<Option<i32>, Vec<usize>> {
        let mut out: BTreeMap<Option<i32>, Vec<usize>> = BTreeMap::new();
        for &(partition, leader) in generation {
            let counts = out.entry(leader).or_insert_with(|| vec![0; producers]);
            counts[assignment.owner_of(partition).expect("assigned")] += 1;
        }
        out
    }

    #[test]
    fn production_layout_needs_one_producer() {
        // Four partitions per proxy, each led by a different one of twelve brokers.
        let generation = vec![(8, Some(8)), (9, Some(9)), (10, Some(10)), (11, Some(11))];
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, auto(), 1);
        assert_eq!(plan.spread, 1);
        assert_eq!(plan.required, 1);
        assert!(
            generation
                .iter()
                .all(|(p, _)| assignment.owner_of(*p) == Some(0))
        );
    }

    #[test]
    fn four_partitions_on_one_broker_get_one_producer_each() {
        let generation = round_robin(0..4, 1);
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, auto(), 1);
        assert_eq!(plan.spread, 4);
        assert_eq!(plan.required, 4);
        let mut owners: Vec<usize> = (0..4)
            .map(|p| assignment.owner_of(p).expect("assigned"))
            .collect();
        owners.sort_unstable();
        assert_eq!(owners, vec![0, 1, 2, 3]);
    }

    #[test]
    fn automatic_sizing_is_capped_at_four() {
        // 64 partitions on three brokers: about 21 per broker, capped to four producers.
        let generation = round_robin(0..64, 3);
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, auto(), 1);
        assert_eq!(plan.spread, 4);
        assert_eq!(plan.required, 4);
        assert_eq!(assignment.producers_in_use(), 4);
    }

    #[test]
    fn each_brokers_partitions_are_spread_even_when_the_count_divides_the_broker_count() {
        // 48 partitions over 12 brokers, leaders at p mod 12, four producers. `p mod 4` would put
        // all four of a broker's partitions on ONE producer; the per-leader spread must not.
        let generation = round_robin(0..48, 12);
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, auto(), 1);
        assert_eq!(plan.spread, 4);
        for (leader, counts) in spread_per_leader(&assignment, &generation, 4) {
            assert_eq!(counts, vec![1, 1, 1, 1], "leader {leader:?}");
        }
    }

    #[test]
    fn uneven_groups_differ_by_at_most_one_per_producer() {
        let generation = round_robin(0..64, 12);
        let mut assignment = ProducerAssignment::default();
        assignment.assign(&generation, auto(), 1);
        for (leader, counts) in spread_per_leader(&assignment, &generation, 4) {
            let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
            assert!(max - min <= 1, "leader {leader:?}: {counts:?}");
        }
    }

    #[test]
    fn a_pinned_partition_never_moves_across_generations() {
        let mut assignment = ProducerAssignment::default();
        let first = round_robin(0..4, 1);
        assignment.assign(&first, auto(), 1);
        let before: Vec<_> = (0..4).map(|p| assignment.owner_of(p)).collect();

        // A rebalance: two old partitions kept, two new ones, and the leaders all moved.
        let second = vec![(2, Some(7)), (3, Some(7)), (4, Some(7)), (5, Some(7))];
        let plan = assignment.assign(&second, auto(), 4);
        let after: Vec<_> = (0..4).map(|p| assignment.owner_of(p)).collect();
        assert_eq!(before, after, "existing owners are write-once");
        assert_eq!(plan.pinned.len(), 2, "only the new partitions are pinned");
        // The two new partitions avoid the producers already carrying 2 and 3 on that leader.
        let taken = [assignment.owner_of(2), assignment.owner_of(3)];
        for (partition, index) in plan.pinned {
            assert!(
                !taken.contains(&Some(index)),
                "partition {partition} doubled up on producer {index}"
            );
        }
    }

    #[test]
    fn a_smaller_later_generation_never_shrinks_the_producer_set() {
        let mut assignment = ProducerAssignment::default();
        assignment.assign(&round_robin(0..4, 1), auto(), 1);
        let plan = assignment.assign(&[(9, Some(3))], auto(), 4);
        assert_eq!(plan.spread, 1);
        assert_eq!(plan.required, 4, "partitions 0-3 still use producers 1-3");
    }

    #[test]
    fn unknown_leaders_are_treated_as_one_broker() {
        let generation: Vec<_> = (0..6).map(|p| (p, None)).collect();
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, auto(), 1);
        assert_eq!(plan.spread, 4);
        for (_, counts) in spread_per_leader(&assignment, &generation, 4) {
            assert_eq!(counts.iter().sum::<usize>(), 6);
            assert!(counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1);
        }
    }

    #[test]
    fn a_fixed_count_ignores_the_layout() {
        let generation = vec![(0, Some(0)), (1, Some(1))];
        let mut assignment = ProducerAssignment::default();
        let plan = assignment.assign(&generation, ProducerSizing::Fixed(3), 3);
        assert_eq!(plan.spread, 3);
        assert_eq!(plan.required, 3);
        assert_eq!(ProducerSizing::Fixed(3).initial(), 3);
        assert_eq!(auto().initial(), 1);
    }

    #[test]
    fn first_use_pins_to_producer_zero_and_generations_respect_it() {
        let mut assignment = ProducerAssignment::default();
        // The capability probe touches partition 0 before any generation.
        assert_eq!(assignment.owner_or_pin_first(0), 0);
        let plan = assignment.assign(&round_robin(0..4, 1), auto(), 1);
        assert_eq!(assignment.owner_of(0), Some(0));
        assert!(plan.pinned.iter().all(|(p, i)| *p != 0 && *i != 0));
    }

    #[test]
    fn assignment_is_deterministic() {
        let generation = round_robin(0..37, 5);
        let mut a = ProducerAssignment::default();
        let mut b = ProducerAssignment::default();
        assert_eq!(
            a.assign(&generation, auto(), 1),
            b.assign(&generation, auto(), 1)
        );
    }
}
