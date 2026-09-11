import {
    applyEditOperation,
    buildEditStateFromObject,
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
});
