package org.opensearch.migrations.bulkload.transformers;

import java.util.List;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import org.opensearch.migrations.bulkload.models.GlobalMetadata;
import org.opensearch.migrations.bulkload.models.IndexMetadata;
import org.opensearch.migrations.bulkload.version_os_2_11.GlobalMetadataData_OS_2_11;
import org.opensearch.migrations.bulkload.version_os_2_11.IndexMetadataData_OS_2_11;
import org.opensearch.migrations.transformation.TransformationRule;
import org.opensearch.migrations.transformation.entity.Index;
import org.opensearch.migrations.transformation.rules.IndexMappingTypeRemoval;

import lombok.extern.slf4j.Slf4j;

@Slf4j
public class Transformer_ES_6_8_to_OS_2_11 implements Transformer {
    private static final ObjectMapper mapper = new ObjectMapper();

    private final List<TransformationRule<Index>> indexTransformations = List.of(new IndexMappingTypeRemoval());
    private final List<TransformationRule<Index>> indexTemplateTransformations = List.of(new IndexMappingTypeRemoval());

    private final int awarenessAttributeDimensionality;

    public Transformer_ES_6_8_to_OS_2_11(int awarenessAttributeDimensionality) {
        this.awarenessAttributeDimensionality = awarenessAttributeDimensionality;
    }

    @Override
    public GlobalMetadata transformGlobalMetadata(GlobalMetadata globalData) {
        ObjectNode newRoot = mapper.createObjectNode();

        // Transform the original "templates", but put them into the legacy "templates" bucket on the target
        var templatesRoot = globalData.getTemplates();
        if (templatesRoot != null) {
            var templates = mapper.createObjectNode();
            templatesRoot.fields().forEachRemaining(template -> {
                var templateCopy = (ObjectNode) template.getValue().deepCopy();
                var indexTemplate = (Index) () -> templateCopy;
                transformIndex(indexTemplate, IndexType.TEMPLATE);
                templates.set(template.getKey(), indexTemplate.getRawJson());
            });
            newRoot.set("templates", templates);
        }

        // Make empty index_templates
        ObjectNode indexTemplatesRoot = mapper.createObjectNode();
        ObjectNode indexTemplatesSubRoot = mapper.createObjectNode();
        indexTemplatesRoot.set("index_template", indexTemplatesSubRoot);
        newRoot.set("index_template", indexTemplatesRoot);

        // Make empty component_templates
        ObjectNode componentTemplatesRoot = mapper.createObjectNode();
        ObjectNode componentTemplatesSubRoot = mapper.createObjectNode();
        componentTemplatesRoot.set("component_template", componentTemplatesSubRoot);
        newRoot.set("component_template", componentTemplatesRoot);

        return new GlobalMetadataData_OS_2_11(newRoot);
    }

    @Override
    public IndexMetadata transformIndexMetadata(IndexMetadata index) {
        var copy = index.deepCopy();
        transformIndex(copy, IndexType.CONCRETE);
        return new IndexMetadataData_OS_2_11(copy.getRawJson(), copy.getId(), copy.getName());
    }

    private void transformIndex(Index index, IndexType type) {
        log.atDebug().setMessage(()->"Original Object: {}").addArgument(index.getRawJson().toString()).log();
        var newRoot = index.getRawJson();

        switch (type) {
            case CONCRETE:
                indexTransformations.forEach(transformer -> transformer.applyTransformation(index));
                break;
            case TEMPLATE:
                indexTemplateTransformations.forEach(transformer -> transformer.applyTransformation(index));
                break;
            default:
                throw new IllegalArgumentException("Unknown type: " + type);
        }

        newRoot.set("settings", TransformFunctions.convertFlatSettingsToTree((ObjectNode) newRoot.get("settings")));
        TransformFunctions.removeIntermediateIndexSettingsLevel(newRoot); // run before fixNumberOfReplicas
        TransformFunctions.fixReplicasForDimensionality(newRoot, awarenessAttributeDimensionality);

        log.atDebug().setMessage(()->"Transformed Object: {}").addArgument(newRoot.toString()).log();
    }

    private enum IndexType {
        CONCRETE,
        TEMPLATE;
    }
}