package org.opensearch.migrations.bulkload.solr;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.Test;

import static org.hamcrest.MatcherAssert.assertThat;
import static org.hamcrest.Matchers.equalTo;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

class SolrSchemaConverterTest {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    @Test
    void convertsBasicSolrFieldTypes() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));
        fields.add(field("count", "pint"));
        fields.add(field("price", "pfloat"));
        fields.add(field("id", "string"));
        fields.add(field("created", "pdate"));
        fields.add(field("active", "boolean"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var properties = mappings.get("properties");

        assertThat(properties.get("title").get("type").asText(), equalTo("text"));
        assertThat(properties.get("count").get("type").asText(), equalTo("integer"));
        assertThat(properties.get("price").get("type").asText(), equalTo("float"));
        assertThat(properties.get("id").get("type").asText(), equalTo("keyword"));
        assertThat(properties.get("created").get("type").asText(), equalTo("date"));
        assertThat(properties.get("active").get("type").asText(), equalTo("boolean"));
    }

    @Test
    void skipsInternalFields() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));
        fields.add(field("_version_", "plong"));
        fields.add(field("_root_", "string"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var properties = mappings.get("properties");

        assertNotNull(properties.get("title"));
        assertTrue(properties.get("_version_") == null || properties.get("_version_").isMissingNode());
        assertTrue(properties.get("_root_") == null || properties.get("_root_").isMissingNode());
    }

    @Test
    void handlesUnknownTypeAsText() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("custom", "my_custom_type"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        assertThat(mappings.get("properties").get("custom").get("type").asText(), equalTo("text"));
    }

    @Test
    void handlesEmptyFields() {
        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(MAPPER.createArrayNode());
        assertNotNull(mappings.get("properties"));
        assertThat(mappings.get("properties").size(), equalTo(0));
    }

    @Test
    void handlesNullFields() {
        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(null);
        assertNotNull(mappings.get("properties"));
        assertThat(mappings.get("properties").size(), equalTo(0));
    }

    // --- Dynamic field tests ---

    @Test
    void convertsDynamicFieldsToTemplates() {
        var dynamicFields = MAPPER.createArrayNode();
        dynamicFields.add(dynField("*_s", "string"));
        dynamicFields.add(dynField("*_i", "pint"));
        dynamicFields.add(dynField("attr_*", "text_general"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(
            MAPPER.createArrayNode(), dynamicFields, null, null
        );

        var templates = mappings.get("dynamic_templates");
        assertNotNull(templates, "Should have dynamic_templates");
        assertThat("3 dynamic templates", templates.size(), equalTo(3));
    }

    @Test
    void dynamicFieldSuffixPatternMapsCorrectly() {
        var template = SolrSchemaConverter.buildDynamicTemplate("*_s", "keyword");
        assertNotNull(template);
        // Template should have a path_match pattern (matches full dotted path) gated
        // by match_mapping_type so it fires only on leaves, never object containers.
        var inner = template.fields().next().getValue();
        assertThat(inner.get("path_match").asText(), equalTo("*_s"));
        assertThat(inner.get("match_mapping_type").asText(), equalTo("string"));
        assertThat(inner.get("mapping").get("type").asText(), equalTo("keyword"));
    }

    @Test
    void dynamicFieldPrefixPatternMapsCorrectly() {
        var template = SolrSchemaConverter.buildDynamicTemplate("attr_*", "text");
        assertNotNull(template);
        var inner = template.fields().next().getValue();
        assertThat(inner.get("path_match").asText(), equalTo("attr_*"));
        assertThat(inner.get("match_mapping_type").asText(), equalTo("string"));
        assertThat(inner.get("mapping").get("type").asText(), equalTo("text"));
    }

    @Test
    void dynamicFieldTemplateGatesByJsonShapeForEachOpenSearchType() {
        // path_match alone is not enough: an int dynamic-field pattern like attr_*
        // would otherwise also match an OBJECT container "attr_field" produced by a
        // dotted-name leaf like attr_field.withdot, causing OpenSearch to try to
        // make attr_field an integer and then explode when the .withdot child
        // arrives. match_mapping_type pins each template to the JSON value shape
        // that OpenSearch reports for that type, so containers (no value shape)
        // can never trigger it.
        assertThat(SolrSchemaConverter.buildDynamicTemplate("attr_*", "integer")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("long"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("attr_*", "long")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("long"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("price_*", "float")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("double"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("price_*", "double")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("double"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("flag_*", "boolean")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("boolean"));
        // Date: Solr stores dates as epoch millis (long) in stored fields, so the
        // converted documents present date leaves as JSON longs — gate accordingly.
        assertThat(SolrSchemaConverter.buildDynamicTemplate("when_*", "date")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("long"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("name_*", "keyword")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("string"));
        assertThat(SolrSchemaConverter.buildDynamicTemplate("body_*", "text")
            .fields().next().getValue().get("match_mapping_type").asText(), equalTo("string"));
    }

    @Test
    void dynamicFieldTemplateOmitsMatchMappingTypeForBinaryAndUnknownOsTypes() {
        // BINARY (and any future / unrecognized OpenSearch type) has no JSON value
        // shape we can confidently gate on, so the template falls through to the
        // default branch and emits no match_mapping_type. path_match alone scopes it.
        var binaryTemplate = SolrSchemaConverter.buildDynamicTemplate("blob_*", "binary");
        var binaryInner = binaryTemplate.fields().next().getValue();
        assertThat(binaryInner.get("path_match").asText(), equalTo("blob_*"));
        assertThat("BINARY: no match_mapping_type emitted",
            binaryInner.has("match_mapping_type"), equalTo(false));
        assertThat(binaryInner.get("mapping").get("type").asText(), equalTo("binary"));

        var unknownTemplate = SolrSchemaConverter.buildDynamicTemplate("x_*", "some_future_type");
        var unknownInner = unknownTemplate.fields().next().getValue();
        assertThat("Unknown OS type: no match_mapping_type emitted",
            unknownInner.has("match_mapping_type"), equalTo(false));
    }

    @Test
    void dynamicFieldTemplateOmitsMatchMappingTypeWhenOsTypeIsNull() {
        // resolveOsType can return null when the Solr fieldType doesn't map to any
        // known OS type and isn't class-resolvable. The template still emits a
        // path_match-scoped entry, but with no match_mapping_type gate.
        var template = SolrSchemaConverter.buildDynamicTemplate("ignore_*", null);
        var inner = template.fields().next().getValue();
        assertThat(inner.get("path_match").asText(), equalTo("ignore_*"));
        assertThat("Null osType: no match_mapping_type emitted",
            inner.has("match_mapping_type"), equalTo(false));
    }

    // --- CopyField tests ---

    @Test
    void copyFieldAddsDestinationAsTextField() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "title");
        cf.put("dest", "text_all");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, null, copyFields, null);
        var properties = mappings.get("properties");

        assertThat("Original field present", properties.get("title").get("type").asText(), equalTo("text"));
        assertThat("CopyField dest added as text", properties.get("text_all").get("type").asText(), equalTo("text"));
    }

    @Test
    void copyFieldDoesNotOverrideExistingField() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));
        fields.add(field("text_all", "string")); // Explicit field

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "title");
        cf.put("dest", "text_all");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, null, copyFields, null);
        // Explicit field type should win
        assertThat(mappings.get("properties").get("text_all").get("type").asText(), equalTo("keyword"));
    }

    @Test
    void copyFieldDestResolvesTypeFromDynamicFieldPattern() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));

        var dynamicFields = MAPPER.createArrayNode();
        dynamicFields.add(dynField("*_str", "strings"));

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "title");
        cf.put("dest", "title_str");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, dynamicFields, copyFields, null);
        var properties = mappings.get("properties");

        assertThat("title → text", properties.get("title").get("type").asText(), equalTo("text"));
        assertThat("title_str → keyword (via *_str dynamic pattern)",
            properties.get("title_str").get("type").asText(), equalTo("keyword"));
    }

    @Test
    void copyFieldDestResolvesDateTypeFromDynamicFieldWithFormat() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("name", "text_general"));

        var dynamicFields = MAPPER.createArrayNode();
        dynamicFields.add(dynField("*_dt", "pdate"));

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "name");
        cf.put("dest", "name_dt");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, dynamicFields, copyFields, null);
        var dest = mappings.get("properties").get("name_dt");

        assertThat("name_dt → date", dest.get("type").asText(), equalTo("date"));
        assertThat("date format included", dest.get("format").asText(), equalTo(SolrSchemaConverter.OS_DATE_FORMAT));
    }

    @Test
    void copyFieldDestFallsBackToTextWhenNoDynamicFieldMatches() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("title", "text_general"));

        var dynamicFields = MAPPER.createArrayNode();
        dynamicFields.add(dynField("*_s", "string")); // does not match "text_all"

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "title");
        cf.put("dest", "text_all");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, dynamicFields, copyFields, null);
        assertThat("text_all → text (no dynamic match)",
            mappings.get("properties").get("text_all").get("type").asText(), equalTo("text"));
    }

    @Test
    void copyFieldDestResolvesViaFieldTypeClass() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("brand", "text_general"));

        var dynamicFields = MAPPER.createArrayNode();
        dynamicFields.add(dynField("*_str", "my_str_type"));

        var fieldTypes = MAPPER.createArrayNode();
        fieldTypes.add(fieldType("my_str_type", "solr.StrField"));

        var copyFields = MAPPER.createArrayNode();
        var cf = MAPPER.createObjectNode();
        cf.put("source", "brand");
        cf.put("dest", "brand_str");
        copyFields.add(cf);

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, dynamicFields, copyFields, fieldTypes);
        assertThat("brand_str → keyword (via fieldType class resolution)",
            mappings.get("properties").get("brand_str").get("type").asText(), equalTo("keyword"));
    }

    // --- FieldType class resolution tests ---

    @Test
    void resolvesTypeViaFieldTypeClass() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("custom_date", "my_date_type"));
        fields.add(field("custom_str", "my_str_type"));

        var fieldTypes = MAPPER.createArrayNode();
        fieldTypes.add(fieldType("my_date_type", "solr.TrieDateField"));
        fieldTypes.add(fieldType("my_str_type", "solr.StrField"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, null, null, fieldTypes);
        var properties = mappings.get("properties");

        assertThat("TrieDateField → date", properties.get("custom_date").get("type").asText(), equalTo("date"));
        assertThat("StrField → keyword", properties.get("custom_str").get("type").asText(), equalTo("keyword"));
    }

    @Test
    void resolvesTrieFieldTypes() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("old_int", "tint"));
        fields.add(field("old_long", "tlong"));
        fields.add(field("old_float", "tfloat"));

        var fieldTypes = MAPPER.createArrayNode();
        fieldTypes.add(fieldType("tint", "solr.TrieIntField"));
        fieldTypes.add(fieldType("tlong", "solr.TrieLongField"));
        fieldTypes.add(fieldType("tfloat", "solr.TrieFloatField"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, null, null, fieldTypes);
        var properties = mappings.get("properties");

        assertThat(properties.get("old_int").get("type").asText(), equalTo("integer"));
        assertThat(properties.get("old_long").get("type").asText(), equalTo("long"));
        assertThat(properties.get("old_float").get("type").asText(), equalTo("float"));
    }

    // --- Date format tests ---

    @Test
    void dateFieldsIncludeFormat() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("created", "pdate"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var created = mappings.get("properties").get("created");

        assertThat(created.get("type").asText(), equalTo("date"));
        assertThat(created.get("format").asText(), equalTo(SolrSchemaConverter.OS_DATE_FORMAT));
    }

    @Test
    void dateFieldFromTrieDateFieldIncludesFormat() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("legacy_date", "my_trie_date"));

        var fieldTypes = MAPPER.createArrayNode();
        fieldTypes.add(fieldType("my_trie_date", "solr.TrieDateField"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields, null, null, fieldTypes);
        var legacyDate = mappings.get("properties").get("legacy_date");

        assertThat(legacyDate.get("type").asText(), equalTo("date"));
        assertThat(legacyDate.get("format").asText(), equalTo(SolrSchemaConverter.OS_DATE_FORMAT));
    }

    @Test
    void dynamicDateFieldIncludesFormat() {
        var template = SolrSchemaConverter.buildDynamicTemplate("*_dt", "date");
        assertNotNull(template);
        var inner = template.fields().next().getValue();
        assertThat(inner.get("mapping").get("format").asText(), equalTo(SolrSchemaConverter.OS_DATE_FORMAT));
    }

    @Test
    void nonDateFieldsDoNotIncludeFormat() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("name", "string"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var name = mappings.get("properties").get("name");

        assertThat(name.get("type").asText(), equalTo("keyword"));
        assertTrue(name.get("format") == null || name.get("format").isMissingNode(),
            "Non-date fields should not have format");
    }

    // --- Dotted field name tests ---
    //
    // Solr permits flat field names that contain '.' (e.g. "category.name"). When
    // the converter emits these as keys under "properties", OpenSearch silently
    // expands them into nested-object form (properties.category.properties.name)
    // — a documented OpenSearch behavior, not a bug. Queries written against the
    // original "category.name" path continue to resolve and the _source preserves
    // the original key shape, so the migration is intuitive from the customer's
    // point of view. These tests pin that contract down so it does not regress.

    @Test
    void preservesDottedFieldNamesAsLiteralKeys() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("id", "string"));
        fields.add(field("category.name", "string"));
        fields.add(field("metric.cpu.percent", "pfloat"));
        fields.add(field("event.created", "pdate"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var properties = mappings.get("properties");

        // Keys stay literal — the converter does NOT pre-split them.
        assertNotNull(properties.get("category.name"),
            "dotted field name should be emitted as a literal key");
        assertThat(properties.get("category.name").get("type").asText(), equalTo("keyword"));

        assertNotNull(properties.get("metric.cpu.percent"),
            "multi-segment dotted field name should be emitted as a literal key");
        assertThat(properties.get("metric.cpu.percent").get("type").asText(), equalTo("float"));

        // Date format is propagated even when the field name has dots.
        var eventCreated = properties.get("event.created");
        assertNotNull(eventCreated);
        assertThat(eventCreated.get("type").asText(), equalTo("date"));
        assertThat(eventCreated.get("format").asText(), equalTo(SolrSchemaConverter.OS_DATE_FORMAT));
    }

    @Test
    void dottedFieldNamesCoexistWithFlatFields() {
        var fields = MAPPER.createArrayNode();
        fields.add(field("id", "string"));
        fields.add(field("title", "text_general"));
        fields.add(field("user.id", "string"));
        fields.add(field("user.email", "string"));

        var mappings = SolrSchemaConverter.convertToOpenSearchMappings(fields);
        var properties = mappings.get("properties");

        assertThat(properties.get("title").get("type").asText(), equalTo("text"));
        assertThat(properties.get("user.id").get("type").asText(), equalTo("keyword"));
        assertThat(properties.get("user.email").get("type").asText(), equalTo("keyword"));
    }

    // --- Helpers ---

    private static ObjectNode field(String name, String type) {
        var node = MAPPER.createObjectNode();
        node.put("name", name);
        node.put("type", type);
        return node;
    }

    private static ObjectNode dynField(String name, String type) {
        var node = MAPPER.createObjectNode();
        node.put("name", name);
        node.put("type", type);
        return node;
    }

    private static ObjectNode fieldType(String name, String className) {
        var node = MAPPER.createObjectNode();
        node.put("name", name);
        node.put("class", className);
        return node;
    }
}
