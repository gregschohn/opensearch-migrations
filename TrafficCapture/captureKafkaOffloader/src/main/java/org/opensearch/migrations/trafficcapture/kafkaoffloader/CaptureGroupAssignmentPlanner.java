package org.opensearch.migrations.trafficcapture.kafkaoffloader;

import java.time.Duration;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AdmissionPhase;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AssignmentTable;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.MemberRow;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Subscription;

/**
 * Leader-side admission validation and fixed-deadline cohort promotion.
 */
final class CaptureGroupAssignmentPlanner {
    record Plan(
        AssignmentTable table,
        Map<String, Set<String>> validWitnesses,
        Set<String> promotedNodeIds,
        Long activationDeadlineNanos
    ) {}

    private final int minimumActiveProxyCount;
    private final int requiredPeerWitnesses;
    private final long activationDebounceNanos;
    private final Set<String> promotionCohort = new TreeSet<>();
    private Long activationDeadlineNanos;

    CaptureGroupAssignmentPlanner(
        int minimumActiveProxyCount,
        int requiredPeerWitnesses,
        Duration activationDebounce
    ) {
        if (minimumActiveProxyCount <= 0) {
            throw new IllegalArgumentException("minimumActiveProxyCount must be positive");
        }
        if (requiredPeerWitnesses < 0) {
            throw new IllegalArgumentException("requiredPeerWitnesses must not be negative");
        }
        Objects.requireNonNull(activationDebounce);
        if (activationDebounce.isNegative()) {
            throw new IllegalArgumentException("activationDebounce must not be negative");
        }
        this.minimumActiveProxyCount = minimumActiveProxyCount;
        this.requiredPeerWitnesses = requiredPeerWitnesses;
        this.activationDebounceNanos = activationDebounce.toNanos();
    }

    Plan plan(Map<String, Subscription> subscriptionsByMemberId, long nowNanos) {
        var subscriptionsByNodeId = byNodeId(subscriptionsByMemberId);
        var validWitnesses = validatedWitnesses(subscriptionsByNodeId);
        var active = new TreeSet<String>();
        var eligibleReady = new TreeSet<String>();
        subscriptionsByNodeId.forEach((nodeId, subscription) -> {
            if (subscription.advertisedPhase() == AdmissionPhase.ACTIVE) {
                active.add(nodeId);
            } else if (subscription.advertisedPhase() == AdmissionPhase.READY
                && validWitnesses.get(nodeId).size() >= requiredPeerWitnesses) {
                eligibleReady.add(nodeId);
            }
        });

        updateCohort(eligibleReady, nowNanos);
        var promoted = promoteIfDue(active, nowNanos);

        var rows = new LinkedHashMap<String, MemberRow>();
        subscriptionsByNodeId.forEach((nodeId, subscription) -> {
            final AdmissionPhase effectivePhase;
            if (active.contains(nodeId) || promoted.contains(nodeId)) {
                effectivePhase = AdmissionPhase.ACTIVE;
            } else if (eligibleReady.contains(nodeId)) {
                effectivePhase = AdmissionPhase.READY;
            } else {
                effectivePhase = AdmissionPhase.JOINING;
            }
            rows.put(
                nodeId,
                new MemberRow(
                    nodeId,
                    subscription.advertisedPhase(),
                    effectivePhase,
                    subscription.footprint(),
                    subscription.observedFootprints(),
                    subscription.matureWitnesses()
                )
            );
        });

        return new Plan(
            new AssignmentTable(rows),
            immutableSetMap(validWitnesses),
            Set.copyOf(promoted),
            activationDeadlineNanos
        );
    }

    private void updateCohort(Set<String> eligibleReady, long nowNanos) {
        promotionCohort.retainAll(eligibleReady);
        promotionCohort.addAll(eligibleReady);
        if (promotionCohort.isEmpty()) {
            activationDeadlineNanos = null;
        } else if (activationDeadlineNanos == null) {
            activationDeadlineNanos = Math.addExact(nowNanos, activationDebounceNanos);
        }
    }

    private Set<String> promoteIfDue(Set<String> active, long nowNanos) {
        if (activationDeadlineNanos == null || nowNanos < activationDeadlineNanos) {
            return Set.of();
        }
        var promoted = active.size() + promotionCohort.size() >= minimumActiveProxyCount
            ? Set.copyOf(promotionCohort)
            : Set.<String>of();
        promotionCohort.clear();
        activationDeadlineNanos = null;
        return promoted;
    }

    private Map<String, Subscription> byNodeId(Map<String, Subscription> subscriptionsByMemberId) {
        Objects.requireNonNull(subscriptionsByMemberId);
        if (subscriptionsByMemberId.isEmpty()) {
            throw new IllegalArgumentException("capture group must contain at least one member");
        }
        var subscriptionsByNodeId = new TreeMap<String, Subscription>();
        subscriptionsByMemberId.forEach((memberId, subscription) -> {
            if (memberId == null || memberId.isBlank()) {
                throw new IllegalArgumentException("Kafka member id must not be blank");
            }
            if (subscriptionsByNodeId.putIfAbsent(subscription.nodeId(), subscription) != null) {
                throw new IllegalStateException(
                    "duplicate capture proxy nodeId in Kafka membership: " + subscription.nodeId()
                );
            }
        });
        return Collections.unmodifiableMap(subscriptionsByNodeId);
    }

    private Map<String, Set<String>> validatedWitnesses(Map<String, Subscription> subscriptionsByNodeId) {
        var result = new LinkedHashMap<String, Set<String>>();
        subscriptionsByNodeId.forEach((writerNodeId, writer) -> {
            var writerIdentity = writer.footprint().identity(writerNodeId);
            var valid = new LinkedHashSet<String>();
            writer.matureWitnesses().forEach((witnessNodeId, claimedWriterIdentity) -> {
                var witness = subscriptionsByNodeId.get(witnessNodeId);
                if (witness != null
                    && writerIdentity.equals(claimedWriterIdentity)
                    && writerIdentity.equals(witness.observedFootprints().get(writerNodeId))) {
                    valid.add(witnessNodeId);
                }
            });
            result.put(writerNodeId, Collections.unmodifiableSet(valid));
        });
        return Collections.unmodifiableMap(result);
    }

    private static Map<String, Set<String>> immutableSetMap(Map<String, Set<String>> source) {
        var result = new LinkedHashMap<String, Set<String>>();
        source.forEach((key, value) -> result.put(key, Set.copyOf(value)));
        return Collections.unmodifiableMap(result);
    }
}
