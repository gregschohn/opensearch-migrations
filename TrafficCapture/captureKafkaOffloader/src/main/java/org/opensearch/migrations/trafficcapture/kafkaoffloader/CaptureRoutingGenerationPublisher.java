package org.opensearch.migrations.trafficcapture.kafkaoffloader;

import java.util.Collection;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;

/**
 * Defines the asynchronous transition from a Kafka partition set to a usable capture routing
 * generation. Initialization creates fresh writer-partition lanes and persists an initial
 * heartbeat on each before exposing the generation's writer identity.
 *
 * <p>Only the newest requested partition set can become active, so requesting another one
 * supersedes any candidate still initializing: that candidate's lanes retire locally without ever
 * accepting a connection. Activating a generation changes routing for future connections only.
 * Superseded lanes remain independently publishable while their existing connections drain,
 * preserving the immutable route and heartbeat history associated with each connection.
 */
public interface CaptureRoutingGenerationPublisher {
    /**
     * Makes the complete current Kafka assignment the only candidate routing generation. The
     * result carries the activated writer identity, or is empty when the request was superseded
     * before activation — including when the assignment itself is empty.
     */
    CompletableFuture<Optional<String>> initializeRoutingGeneration(Collection<Integer> partitions);

    void stopAfterFailure(Throwable failure);
}
