package org.opensearch.migrations.commands;

import java.util.ArrayList;

import org.opensearch.migrations.MigrateOrEvaluateArgs;
import org.opensearch.migrations.MigrationMode;
import org.opensearch.migrations.bulkload.transformers.TransformFunctions;
import org.opensearch.migrations.bulkload.transformers.Transformer;
import org.opensearch.migrations.bulkload.worker.IndexMetadataResults;
import org.opensearch.migrations.bulkload.worker.IndexRunner;
import org.opensearch.migrations.bulkload.worker.MetadataRunner;
import org.opensearch.migrations.cli.ClusterReaderExtractor;
import org.opensearch.migrations.cli.Clusters;
import org.opensearch.migrations.cli.Items;
import org.opensearch.migrations.cluster.ClusterProviderRegistry;
import org.opensearch.migrations.metadata.GlobalMetadataCreatorResults;
import org.opensearch.migrations.metadata.tracing.RootMetadataMigrationContext;

import lombok.extern.slf4j.Slf4j;

/** Shared functionality between migration and evaluation commands */
@Slf4j
public abstract class MigratorEvaluatorBase {

    static final int INVALID_PARAMETER_CODE = 999;
    static final int UNEXPECTED_FAILURE_CODE = 888;

    protected final MigrateOrEvaluateArgs arguments;
    protected final ClusterReaderExtractor clusterReaderCliExtractor;

    protected MigratorEvaluatorBase(MigrateOrEvaluateArgs arguments) {
        this.arguments = arguments;
        this.clusterReaderCliExtractor = new ClusterReaderExtractor(arguments);
    }

    protected Clusters createClusters() {
        var clusters = Clusters.builder();
        var sourceCluster = clusterReaderCliExtractor.extractClusterReader();
        clusters.source(sourceCluster);

        var targetCluster = ClusterProviderRegistry.getRemoteWriter(arguments.targetArgs.toConnectionContext(), arguments.dataFilterArgs);
        clusters.target(targetCluster);
        return clusters.build();
    }

    protected Transformer selectTransformer(Clusters clusters) {
        var transformer = TransformFunctions.getTransformer(
            clusters.getSource().getVersion(),
            clusters.getTarget().getVersion(),
            arguments.minNumberOfReplicas
        );
        log.info("Selected transformer " + transformer.toString());
        return transformer;
    }

    protected Items migrateAllItems(MigrationMode migrationMode, Clusters clusters, Transformer transformer, RootMetadataMigrationContext context) {
        var items = Items.builder();
        items.dryRun(migrationMode.equals(MigrationMode.SIMULATE));
        var metadataResults = migrateGlobalMetadata(migrationMode, clusters, transformer, context);

        var indexTemplates = new ArrayList<String>();
        indexTemplates.addAll(metadataResults.getLegacyTemplates());
        indexTemplates.addAll(metadataResults.getIndexTemplates());
        items.indexTemplates(indexTemplates);
        items.componentTemplates(metadataResults.getComponentTemplates());

        var indexResults = migrateIndices(migrationMode, clusters, transformer, context);
        items.indexes(indexResults.getIndexNames());
        items.aliases(indexResults.getAliases());

        return items.build();
    }

    private GlobalMetadataCreatorResults migrateGlobalMetadata(MigrationMode mode, Clusters clusters, Transformer transformer, RootMetadataMigrationContext context) {
        var metadataRunner = new MetadataRunner(
            arguments.snapshotName,
            clusters.getSource().getGlobalMetadata(),
            clusters.getTarget().getGlobalMetadataCreator(),
            transformer
        );
        var metadataResults = metadataRunner.migrateMetadata(mode, context.createMetadataMigrationContext());
        log.info("Metadata copy complete.");
        return metadataResults;
    }

    private IndexMetadataResults migrateIndices(MigrationMode mode, Clusters clusters, Transformer transformer, RootMetadataMigrationContext context) {
        var indexRunner = new IndexRunner(
            arguments.snapshotName,
            clusters.getSource().getIndexMetadata(),
            clusters.getTarget().getIndexCreator(),
            transformer,
            arguments.dataFilterArgs.indexAllowlist
        );
        var indexResults = indexRunner.migrateIndices(mode, context.createIndexContext());
        log.info("Index copy complete.");
        return indexResults;
    } 
}
