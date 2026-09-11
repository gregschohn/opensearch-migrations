package org.opensearch.migrations.replay.lifecycle;

import org.opensearch.migrations.replay.lifecycle.ReplayIdentity.SourcePartitionKey;

import lombok.NonNull;

/**
 * The process-local result of the source-side portion of a whole-record disposition.
 *
 * <p>An unknown result after revocation does not assert that Kafka committed or rejected the
 * offset. It only records that this process must stop waiting and let the next assignment's
 * starting offset determine whether Kafka redelivers the record.
 */
public sealed interface BrokerCommitResult permits
    BrokerCommitResult.NotAttempted,
    BrokerCommitResult.Acknowledged,
    BrokerCommitResult.UnknownAfterRevocation {

    record NotAttempted() implements BrokerCommitResult {}

    record Acknowledged() implements BrokerCommitResult {}

    record UnknownAfterRevocation(@NonNull SourcePartitionKey partition) implements BrokerCommitResult {}
}
