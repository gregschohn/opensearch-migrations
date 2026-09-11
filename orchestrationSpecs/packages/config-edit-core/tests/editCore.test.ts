import {
    annotateDraftChanges,
    applyEditOperation,
    buildEditStateFromObject,
    externalResourceSelectionOperations,
    projectConfigYaml,
    validationForConfig,
} from "../src";
import {describe, expect, it} from "@jest/globals";

describe("browser-safe configuration editing", () => {
    const config = {
        sourceClusters: {
            source: {
                endpoint: "https://source.example.com:9200",
                version: "ES 7.10",
            },
        },
        targetClusters: {
            target: {
                endpoint: "https://target.example.com:9200",
            },
        },
        snapshotMigrationConfigs: [],
    };

    it("applies operations without mutating the caller's document", () => {
        const updated = applyEditOperation(config, {
            op: "set",
            path: ["sourceClusters", "source", "allowInsecure"],
            value: true,
        }) as typeof config & {
            sourceClusters: {source: {allowInsecure?: boolean}};
        };

        expect(updated.sourceClusters.source.allowInsecure).toBe(true);
        expect("allowInsecure" in config.sourceClusters.source).toBe(false);
    });

    it("projects and validates without Node runtime services", () => {
        expect(validationForConfig(config).valid).toBe(true);
        expect(buildEditStateFromObject(config).provenance).toMatchObject({
            source: "pending-yaml",
            mode: "structured",
            lossy: false,
        });
    });

    it("projects raw YAML and preserves malformed documents for repair", () => {
        const projected = projectConfigYaml(`
sourceClusters:
  source:
    endpoint: https://source.example.com:9200
    version: ES 7.10
`);
        expect(projected.config).toMatchObject({
            sourceClusters: {
                source: {
                    endpoint: "https://source.example.com:9200",
                },
            },
        });
        expect(projected.editState.provenance.mode).toBe("structured");

        const malformed = projectConfigYaml("sourceClusters: [");
        expect(malformed.config).toBeNull();
        expect(malformed.editState.provenance.mode).toBe("raw");
        expect(malformed.editState.validation.valid).toBe(false);
    });

    it("annotates local changes against the saved projection", () => {
        const base = buildEditStateFromObject(config);
        const updated = buildEditStateFromObject(applyEditOperation(config, {
            op: "set",
            path: ["sourceClusters", "source", "allowInsecure"],
            value: true,
        }));
        const annotated = annotateDraftChanges(updated, base);
        const visit = (nodes: typeof annotated.nodes): typeof annotated.nodes =>
            nodes.flatMap(node => [node, ...visit(node.children ?? [])]);
        const allNodes = visit(annotated.nodes);
        const source = allNodes.find(
            node => node.id === "edit:sourceClusters.source",
        );
        const allowInsecure = allNodes.find(
            node => node.id === "edit:sourceClusters.source.allowInsecure",
        );

        expect(allowInsecure?.draftChange).toEqual({
            kind: "modified",
            previousValue: false,
            previousValuePresent: true,
        });
        expect(source?.draftChangeCount).toBeGreaterThan(0);
    });

    it("converts descriptor-driven external selections into edit operations", () => {
        expect(externalResourceSelectionOperations({
            id: "edit:file",
            path: ["traffic", "transform", "configMap"],
            label: "ConfigMap",
            valueKind: "scalar",
            externalRef: {
                kind: "kubernetesResource",
                purpose: "file-ref-config-map",
                displayName: "ConfigMap",
                selection: {
                    target: "fileRefConfigMap",
                    nameField: "configMap",
                    pathField: "path",
                },
            },
        }, {
            group: "",
            key: "transform.js",
            kind: "ConfigMap",
            name: "transform-code",
        })).toEqual([
            {
                op: "set",
                path: ["traffic", "transform", "configMap"],
                value: "transform-code",
            },
            {
                op: "set",
                path: ["traffic", "transform", "path"],
                value: "transform.js",
            },
        ]);
    });
});
