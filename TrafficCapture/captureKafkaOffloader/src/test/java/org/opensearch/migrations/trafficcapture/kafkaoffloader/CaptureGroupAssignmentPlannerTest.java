package org.opensearch.migrations.trafficcapture.kafkaoffloader;

import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AdmissionPhase;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Footprint;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Subscription;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;

class CaptureGroupAssignmentPlannerTest {
    private static final Duration DEBOUNCE = Duration.ofSeconds(2);

    @Test
    void readyRequiresAnExactCurrentPairwiseWitnessEcho() {
        var planner = new CaptureGroupAssignmentPlanner(1, 1, DEBOUNCE);
        var candidateFootprint = Footprint.known(2, List.of(0, 1));
        var staleFootprint = Footprint.known(1, List.of(0));
        var witnessFootprint = Footprint.known(3, List.of(0, 1));

        var plan = planner.plan(
            Map.of(
                "member-a",
                subscription(
                    "node-a",
                    AdmissionPhase.READY,
                    candidateFootprint,
                    Map.of(),
                    Map.of("node-b", candidateFootprint.identity("node-a"))
                ),
                "member-b",
                subscription(
                    "node-b",
                    AdmissionPhase.ACTIVE,
                    witnessFootprint,
                    Map.of("node-a", staleFootprint.identity("node-a")),
                    Map.of()
                )
            ),
            0
        );

        assertEquals(AdmissionPhase.JOINING, plan.table().members().get("node-a").effectivePhase());
        assertEquals(Map.of("node-a", java.util.Set.of(), "node-b", java.util.Set.of()), plan.validWitnesses());
        assertNull(plan.activationDeadlineNanos());
    }

    @Test
    void laterReadyMembersJoinTheCohortWithoutExtendingItsDeadline() {
        var planner = new CaptureGroupAssignmentPlanner(2, 1, DEBOUNCE);
        var nodeA = Footprint.known(1, List.of(0, 1));
        var nodeB = Footprint.known(1, List.of(0, 1));
        var first = subscriptions(
            ready("node-a", nodeA, "node-b"),
            joiningWitness("node-b", nodeB, "node-a", nodeA)
        );

        var initial = planner.plan(first, 0);
        assertEquals(DEBOUNCE.toNanos(), initial.activationDeadlineNanos());

        var bothReady = subscriptions(
            readyWithObservations("node-a", nodeA, "node-b", nodeB, Map.of()),
            readyWithObservations(
                "node-b",
                nodeB,
                "node-a",
                nodeA,
                Map.of("node-a", nodeA.identity("node-a"))
            )
        );
        var joined = planner.plan(bothReady, Duration.ofSeconds(1).toNanos());
        assertEquals(DEBOUNCE.toNanos(), joined.activationDeadlineNanos());

        var promoted = planner.plan(bothReady, DEBOUNCE.toNanos());
        assertEquals(
            java.util.Set.of("node-a", "node-b"),
            promoted.promotedNodeIds()
        );
        assertEquals(AdmissionPhase.ACTIVE, promoted.table().members().get("node-a").effectivePhase());
        assertEquals(AdmissionPhase.ACTIVE, promoted.table().members().get("node-b").effectivePhase());
        assertNull(promoted.activationDeadlineNanos());
    }

    @Test
    void losingARequiredWitnessRevokesReadinessBeforePromotion() {
        var planner = new CaptureGroupAssignmentPlanner(1, 1, DEBOUNCE);
        var nodeA = Footprint.known(1, List.of(0));
        var nodeB = Footprint.known(1, List.of(0));

        var ready = subscriptions(
            ready("node-a", nodeA, "node-b"),
            joiningWitness("node-b", nodeB, "node-a", nodeA)
        );
        planner.plan(ready, 0);

        var lostWitness = planner.plan(Map.of("member-a", ready.get("member-node-a")), DEBOUNCE.toNanos());

        assertEquals(AdmissionPhase.JOINING, lostWitness.table().members().get("node-a").effectivePhase());
        assertEquals(java.util.Set.of(), lostWitness.promotedNodeIds());
        assertNull(lostWitness.activationDeadlineNanos());
    }

    @Test
    void activeIsMonotonicEvenAfterWitnessCoverageFalls() {
        var planner = new CaptureGroupAssignmentPlanner(2, 2, DEBOUNCE);
        var active = subscription(
            "node-a",
            AdmissionPhase.ACTIVE,
            Footprint.known(1, List.of(0)),
            Map.of(),
            Map.of()
        );

        var plan = planner.plan(Map.of("member-a", active), 0);

        assertEquals(AdmissionPhase.ACTIVE, plan.table().members().get("node-a").effectivePhase());
        assertEquals(java.util.Set.of(), plan.validWitnesses().get("node-a"));
        assertNull(plan.activationDeadlineNanos());
    }

    @Test
    void minimumActiveProxyCountIncludesThePromotedCandidate() {
        var planner = new CaptureGroupAssignmentPlanner(1, 0, Duration.ZERO);
        var candidate = subscription(
            "node-a",
            AdmissionPhase.READY,
            Footprint.known(1, List.of(0)),
            Map.of(),
            Map.of()
        );

        var plan = planner.plan(Map.of("member-a", candidate), 0);

        assertEquals(java.util.Set.of("node-a"), plan.promotedNodeIds());
        assertEquals(AdmissionPhase.ACTIVE, plan.table().members().get("node-a").effectivePhase());
    }

    private static Map<String, Subscription> subscriptions(Subscription... subscriptions) {
        var result = new LinkedHashMap<String, Subscription>();
        for (var subscription : subscriptions) {
            result.put("member-" + subscription.nodeId(), subscription);
        }
        return Map.copyOf(result);
    }

    private static Subscription ready(String nodeId, Footprint footprint, String witnessNodeId) {
        return subscription(
            nodeId,
            AdmissionPhase.READY,
            footprint,
            Map.of(),
            Map.of(witnessNodeId, footprint.identity(nodeId))
        );
    }

    private static Subscription joiningWitness(
        String nodeId,
        Footprint footprint,
        String observedNodeId,
        Footprint observedFootprint
    ) {
        return subscription(
            nodeId,
            AdmissionPhase.JOINING,
            footprint,
            Map.of(observedNodeId, observedFootprint.identity(observedNodeId)),
            Map.of()
        );
    }

    private static Subscription readyWithObservations(
        String nodeId,
        Footprint footprint,
        String witnessNodeId,
        Footprint witnessFootprint,
        Map<String, CaptureGroupProtocol.FootprintIdentity> additionalObservations
    ) {
        var observations = new LinkedHashMap<>(additionalObservations);
        observations.put(witnessNodeId, witnessFootprint.identity(witnessNodeId));
        return subscription(
            nodeId,
            AdmissionPhase.READY,
            footprint,
            observations,
            Map.of(witnessNodeId, footprint.identity(nodeId))
        );
    }

    private static Subscription subscription(
        String nodeId,
        AdmissionPhase phase,
        Footprint footprint,
        Map<String, CaptureGroupProtocol.FootprintIdentity> observations,
        Map<String, CaptureGroupProtocol.FootprintIdentity> matureWitnesses
    ) {
        return new Subscription(nodeId, phase, footprint, observations, matureWitnesses);
    }
}
