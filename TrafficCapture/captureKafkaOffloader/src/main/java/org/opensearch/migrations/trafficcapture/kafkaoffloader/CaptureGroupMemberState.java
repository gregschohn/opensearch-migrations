package org.opensearch.migrations.trafficcapture.kafkaoffloader;

import java.time.Duration;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.OptionalLong;
import java.util.TreeMap;

import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AdmissionPhase;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.AssignmentTable;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Footprint;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.FootprintIdentity;
import org.opensearch.migrations.trafficcapture.kafkaoffloader.CaptureGroupProtocol.Subscription;

/**
 * Member-side subscription echo, pairwise maturity, and advertised admission phase.
 */
final class CaptureGroupMemberState {
    record Update(
        Subscription subscription,
        AdmissionPhase effectivePhase,
        boolean metadataChanged,
        OptionalLong nextRebalanceAtNanos
    ) {}

    private final String nodeId;
    private final int requiredPeerWitnesses;
    private final long witnessMaturityNanos;
    private final long activationDebounceNanos;
    private final Map<String, Long> witnessObservedSinceNanos = new LinkedHashMap<>();
    private Footprint footprint;
    private AdmissionPhase advertisedPhase = AdmissionPhase.JOINING;
    private AdmissionPhase effectivePhase = AdmissionPhase.JOINING;
    private Map<String, FootprintIdentity> observedFootprints = Map.of();
    private Map<String, FootprintIdentity> matureWitnesses = Map.of();

    CaptureGroupMemberState(
        String nodeId,
        Footprint initialFootprint,
        int requiredPeerWitnesses,
        Duration witnessMaturity,
        Duration activationDebounce
    ) {
        if (nodeId == null || nodeId.isBlank()) {
            throw new IllegalArgumentException("nodeId must not be blank");
        }
        if (requiredPeerWitnesses < 0) {
            throw new IllegalArgumentException("requiredPeerWitnesses must not be negative");
        }
        Objects.requireNonNull(witnessMaturity);
        Objects.requireNonNull(activationDebounce);
        if (witnessMaturity.isNegative() || activationDebounce.isNegative()) {
            throw new IllegalArgumentException("membership durations must not be negative");
        }
        this.nodeId = nodeId;
        this.footprint = Objects.requireNonNull(initialFootprint);
        this.requiredPeerWitnesses = requiredPeerWitnesses;
        this.witnessMaturityNanos = witnessMaturity.toNanos();
        this.activationDebounceNanos = activationDebounce.toNanos();
    }

    Subscription subscription() {
        return new Subscription(
            nodeId,
            advertisedPhase,
            footprint,
            observedFootprints,
            matureWitnesses
        );
    }

    Update installAssignment(AssignmentTable table, long nowNanos) {
        Objects.requireNonNull(table);
        var ownRow = table.members().get(nodeId);
        if (ownRow == null) {
            throw new IllegalStateException("capture assignment table does not contain this node");
        }
        if (!footprint.equals(ownRow.footprint())) {
            throw new IllegalStateException("capture assignment table changed this node's footprint");
        }

        var previousSubscription = subscription();
        effectivePhase = ownRow.effectivePhase();
        observedFootprints = observedFootprints(table);
        updateWitnessTimers(table, nowNanos);
        matureWitnesses = matureWitnessClaims(nowNanos);
        advertisedPhase = nextAdvertisedPhase();
        var nextRebalanceAt = nextRebalanceAt(nowNanos);
        var currentSubscription = subscription();
        return new Update(
            currentSubscription,
            effectivePhase,
            !previousSubscription.equals(currentSubscription),
            nextRebalanceAt
        );
    }

    private Map<String, FootprintIdentity> observedFootprints(AssignmentTable table) {
        var observations = new LinkedHashMap<String, FootprintIdentity>();
        table.members().forEach((memberNodeId, row) -> {
            if (!memberNodeId.equals(nodeId)) {
                observations.put(memberNodeId, row.footprint().identity(memberNodeId));
            }
        });
        return Collections.unmodifiableMap(observations);
    }

    private void updateWitnessTimers(AssignmentTable table, long nowNanos) {
        var ownIdentity = footprint.identity(nodeId);
        witnessObservedSinceNanos.keySet().removeIf(witnessNodeId -> {
            var witness = table.members().get(witnessNodeId);
            return witness == null || !ownIdentity.equals(witness.observedFootprints().get(nodeId));
        });
        table.members().forEach((witnessNodeId, witness) -> {
            if (!witnessNodeId.equals(nodeId)
                && ownIdentity.equals(witness.observedFootprints().get(nodeId))) {
                witnessObservedSinceNanos.putIfAbsent(witnessNodeId, nowNanos);
            }
        });
    }

    private Map<String, FootprintIdentity> matureWitnessClaims(long nowNanos) {
        var ownIdentity = footprint.identity(nodeId);
        var claims = new TreeMap<String, FootprintIdentity>();
        witnessObservedSinceNanos.forEach((witnessNodeId, observedSinceNanos) -> {
            if (elapsedAtLeast(nowNanos, observedSinceNanos, witnessMaturityNanos)) {
                claims.put(witnessNodeId, ownIdentity);
            }
        });
        return Collections.unmodifiableMap(claims);
    }

    private AdmissionPhase nextAdvertisedPhase() {
        if (effectivePhase == AdmissionPhase.ACTIVE || advertisedPhase == AdmissionPhase.ACTIVE) {
            return AdmissionPhase.ACTIVE;
        }
        return matureWitnesses.size() >= requiredPeerWitnesses
            ? AdmissionPhase.READY
            : AdmissionPhase.JOINING;
    }

    private OptionalLong nextRebalanceAt(long nowNanos) {
        if (effectivePhase == AdmissionPhase.READY) {
            return OptionalLong.of(Math.addExact(nowNanos, activationDebounceNanos));
        }
        if (advertisedPhase == AdmissionPhase.READY && effectivePhase == AdmissionPhase.JOINING) {
            return OptionalLong.of(nowNanos);
        }
        return witnessObservedSinceNanos.values()
            .stream()
            .mapToLong(start -> Math.addExact(start, witnessMaturityNanos))
            .filter(deadline -> deadline > nowNanos)
            .min();
    }

    private static boolean elapsedAtLeast(long nowNanos, long startNanos, long requiredNanos) {
        return nowNanos - startNanos >= requiredNanos;
    }
}
