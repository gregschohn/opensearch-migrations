package org.opensearch.migrations.bulkload.worker;

import org.opensearch.migrations.MigrationMode;
import org.opensearch.migrations.bulkload.models.GlobalMetadata;
import org.opensearch.migrations.bulkload.transformers.Transformer;
import org.opensearch.migrations.metadata.GlobalMetadataCreator;
import org.opensearch.migrations.metadata.GlobalMetadataCreatorResults;
import org.opensearch.migrations.metadata.tracing.IMetadataMigrationContexts.IClusterMetadataContext;

import lombok.AllArgsConstructor;
import lombok.extern.slf4j.Slf4j;

@Slf4j
@AllArgsConstructor
public class MetadataRunner {

    private final String snapshotName;
    private final GlobalMetadata.Factory metadataFactory;
    private final GlobalMetadataCreator metadataCreator;
    private final Transformer transformer;

    public GlobalMetadataCreatorResults migrateMetadata(MigrationMode mode, IClusterMetadataContext context) {
        log.info("Migrating the Templates...");
        var globalMetadata = metadataFactory.fromRepo();
        var transformedRoot = transformer.transformGlobalMetadata(globalMetadata);
        var results = metadataCreator.create(transformedRoot, mode, context);
        log.info("Templates migration complete");
        return results;
    }
}
