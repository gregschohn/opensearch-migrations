package org.opensearch.migrations.trafficcapture.kafkaoffloader;

import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AdmissionPhase;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AssignmentTable;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Footprint;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.MemberRow;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

class CaptureGroupMemberStateTest {
    private static final Duration MATURITY = Duration.ofSeconds(10);
    private static final Duration ACTIVATION_DEBOUNCE = Duration.ofSeconds(2);

    @Test
    void pairMaturityStartsOnlyAfterThePeerEchoesThisExactFootprint() {
        var nodeA = Footprint.known(1, List.of(0, 1));
        var nodeB = Footprint.known(1, List.of(0, 1));
        var state = new CaptureGroupMemberState(
            "node-a",
            nodeA,
            1,
            MATURITY,
            ACTIVATION_DEBOUNCE
        );

        var first = state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row("node-b", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeB, Map.of())
            ),
            0
        );
        assertEquals(AdmissionPhase.JOINING, first.subscription().advertisedPhase());
        assertEquals(Map.of("node-b", nodeB.identity("node-b")), first.subscription().observedFootprints());
        assertTrue(first.metadataChanged());

        var echoed = state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                )
            ),
            Duration.ofSeconds(1).toNanos()
        );
        assertEquals(Duration.ofSeconds(11).toNanos(), echoed.nextRebalanceAtNanos().orElseThrow());

        var mature = state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                )
            ),
            Duration.ofSeconds(11).toNanos()
        );
        assertEquals(AdmissionPhase.READY, mature.subscription().advertisedPhase());
        assertEquals(Map.of("node-b", nodeA.identity("node-a")), mature.subscription().matureWitnesses());
        assertEquals(Duration.ofSeconds(11).toNanos(), mature.nextRebalanceAtNanos().orElseThrow());
    }

    @Test
    void unrelatedJoinDoesNotResetAnExistingMaturingPair() {
        var nodeA = Footprint.known(1, List.of(0));
        var nodeB = Footprint.known(1, List.of(0));
        var nodeC = Footprint.known(1, List.of(0));
        var state = new CaptureGroupMemberState(
            "node-a",
            nodeA,
            1,
            MATURITY,
            ACTIVATION_DEBOUNCE
        );
        state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                )
            ),
            0
        );

        var withLaterJoin = state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                ),
                row("node-c", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeC, Map.of())
            ),
            Duration.ofSeconds(9).toNanos()
        );
        assertEquals(MATURITY.toNanos(), withLaterJoin.nextRebalanceAtNanos().orElseThrow());

        var mature = state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                ),
                row("node-c", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeC, Map.of())
            ),
            MATURITY.toNanos()
        );
        assertEquals(AdmissionPhase.READY, mature.subscription().advertisedPhase());
    }

    @Test
    void losingTheOnlyWitnessReturnsReadyToJoiningBeforeActivation() {
        var nodeA = Footprint.known(1, List.of(0));
        var nodeB = Footprint.known(1, List.of(0));
        var state = new CaptureGroupMemberState(
            "node-a",
            nodeA,
            1,
            Duration.ZERO,
            ACTIVATION_DEBOUNCE
        );
        state.installAssignment(
            table(
                row("node-a", AdmissionPhase.JOINING, AdmissionPhase.JOINING, nodeA, Map.of()),
                row(
                    "node-b",
                    AdmissionPhase.JOINING,
                    AdmissionPhase.JOINING,
                    nodeB,
                    Map.of("node-a", nodeA.identity("node-a"))
                )
            ),
            0
        );

        var lost = state.installAssignment(
            table(row("node-a", AdmissionPhase.READY, AdmissionPhase.JOINING, nodeA, Map.of())),
            1
        );

        assertEquals(AdmissionPhase.JOINING, lost.subscription().advertisedPhase());
        assertEquals(Map.of(), lost.subscription().matureWitnesses());
    }

    @Test
    void activeNeverDemotesWhenWitnessesDisappear() {
        var nodeA = Footprint.known(1, List.of(0));
        var state = new CaptureGroupMemberState(
            "node-a",
            nodeA,
            1,
            Duration.ZERO,
            ACTIVATION_DEBOUNCE
        );

        state.installAssignment(
            table(row("node-a", AdmissionPhase.READY, AdmissionPhase.ACTIVE, nodeA, Map.of())),
            0
        );
        var alone = state.installAssignment(
            table(row("node-a", AdmissionPhase.ACTIVE, AdmissionPhase.ACTIVE, nodeA, Map.of())),
            1
        );

        assertEquals(AdmissionPhase.ACTIVE, alone.subscription().advertisedPhase());
        assertEquals(Map.of(), alone.subscription().matureWitnesses());
    }

    @Test
    void assignmentCannotReplaceTheLocalFootprint() {
        var state = new CaptureGroupMemberState(
            "node-a",
            Footprint.known(1, List.of(0)),
            0,
            Duration.ZERO,
            ACTIVATION_DEBOUNCE
        );

        assertThrows(
            IllegalStateException.class,
            () -> state.installAssignment(
                table(
                    row(
                        "node-a",
                        AdmissionPhase.JOINING,
                        AdmissionPhase.JOINING,
                        Footprint.known(2, List.of(0, 1)),
                        Map.of()
                    )
                ),
                0
            )
        );
    }

    private static AssignmentTable table(MemberRow... rows) {
        var members = new LinkedHashMap<String, MemberRow>();
        for (var row : rows) {
            members.put(row.nodeId(), row);
        }
        return new AssignmentTable(members);
    }

    private static MemberRow row(
        String nodeId,
        AdmissionPhase advertised,
        AdmissionPhase effective,
        Footprint footprint,
        Map<String, CaptureGroupProtocol.FootprintIdentity> observations
    ) {
        return new MemberRow(
            nodeId,
            advertised,
            effective,
            footprint,
            observations,
            Map.of()
        );
    }
}
