import * as fs from "node:fs";
import * as path from "node:path";

import { z } from "zod";

import {
    collectTransitionFields,
    generateTransitionTree,
    resolveTransitionField,
    validateMutatorAgainstTransitionTree,
} from "../src/transitionTreeGenerator";
import {
    dataSnapshotMaxSnapshotRateMutator,
    proxyClientAuthMutator,
    proxyNumThreadsMutator,
    snapshotMigrationMaxConnectionsMutator,
} from "../src/fixtures/mutators";

declare module "zod" {
    interface ZodType {
        changeRestrictionForTest?: (restriction: "impossible" | "gated") => this;
    }
}

function restricted<T extends z.ZodTypeAny>(
    schema: T,
    changeRestriction: "gated" | "impossible",
): T {
    return schema.meta({ ...(schema.meta() ?? {}), changeRestriction }) as T;
}

describe("transition tree generation", () => {
    it("generates deterministic fields with array and record path tokens", () => {
        const schema = z.object({
            records: z.record(
                z.string(),
                z.object({
                    items: z.array(z.object({ value: z.number() })),
                }),
            ),
            safeDefault: z.string(),
        });

        const first = collectTransitionFields(schema);
        const second = collectTransitionFields(schema);

        expect(first).toEqual(second);
        expect(first.map((f) => f.path)).toEqual([...first.map((f) => f.path)].sort());
        expect(first).toEqual(
            expect.arrayContaining([
                expect.objectContaining({ path: "records.{key}.items.[]", changeClass: "safe" }),
                expect.objectContaining({ path: "records.{key}.items.[].value", changeClass: "safe" }),
                expect.objectContaining({ path: "safeDefault", changeClass: "safe" }),
            ]),
        );
    });

    it("inherits parent restrictions down nested subtrees", () => {
        const schema = z.object({
            proxyConfig: z.object({
                tls: restricted(
                    z.object({
                        clientAuth: z.object({
                            required: z.boolean(),
                        }),
                    }),
                    "gated",
                ),
            }),
        });

        const tree = generateTransitionTree(schema, {
            sourceSchema: "TEST_SCHEMA",
            generatedAt: "2026-01-01T00:00:00.000Z",
        });

        expect(resolveTransitionField(tree, "proxyConfig.tls")?.changeClass).toBe("gated");
        expect(resolveTransitionField(tree, "proxyConfig.tls.clientAuth")?.changeClass).toBe("gated");
        expect(resolveTransitionField(tree, "proxyConfig.tls.clientAuth.required")?.changeClass).toBe("gated");
        expect(resolveTransitionField(tree, "proxyConfig.tls.clientAuth")?.inheritedFrom).toBe("proxyConfig.tls");
    });

    it("resolves concrete user paths against generated record and array patterns", () => {
        const schema = z.object({
            snapshotMigrationConfigs: z.array(
                z.object({
                    perSnapshotConfig: z.record(
                        z.string(),
                        z.array(
                            z.object({
                                documentBackfillConfig: z.object({
                                    maxConnections: restricted(z.number(), "gated"),
                                }),
                            }),
                        ),
                    ),
                }),
            ),
        });
        const tree = generateTransitionTree(schema);

        const resolved = resolveTransitionField(
            tree,
            "snapshotMigrationConfigs.0.perSnapshotConfig.snap1.0.documentBackfillConfig.maxConnections",
        );

        expect(resolved).toMatchObject({
            path: "snapshotMigrationConfigs.[].perSnapshotConfig.{key}.[].documentBackfillConfig.maxConnections",
            changeClass: "gated",
        });
    });

    it("validates current built-in mutator field classes against OVERALL_MIGRATION_CONFIG", () => {
        const tree = generateTransitionTree(undefined, {
            generatedAt: "2026-01-01T00:00:00.000Z",
        });

        expect(validateMutatorAgainstTransitionTree(proxyNumThreadsMutator(), tree)).toEqual([]);
        expect(validateMutatorAgainstTransitionTree(proxyClientAuthMutator(), tree)).toEqual([]);
        expect(validateMutatorAgainstTransitionTree(dataSnapshotMaxSnapshotRateMutator(), tree)).toEqual([]);
        expect(validateMutatorAgainstTransitionTree(snapshotMigrationMaxConnectionsMutator(), tree)).toEqual([]);
    });

    it("keeps the committed user-config transition tree in sync with the generator", () => {
        const committedPath = path.resolve(
            __dirname,
            "..",
            "transitionTrees",
            "userConfigFields.json",
        );
        const committed = JSON.parse(fs.readFileSync(committedPath, "utf8"));

        expect(committed).toEqual(generateTransitionTree());
    });

    it("reports unknown paths and class mismatches", () => {
        const tree = generateTransitionTree(
            z.object({
                gatedField: restricted(z.string(), "gated"),
            }),
        );

        expect(
            validateMutatorAgainstTransitionTree(
                {
                    name: "bad",
                    changeClass: "safe",
                    changedPaths: ["missing.path", "gatedField"],
                },
                tree,
            ).map((e) => e.message),
        ).toEqual([
            "changed path 'missing.path' does not resolve to a user-config transition-tree field",
            "changed path 'gatedField' is 'gated' in the transition tree (matched 'gatedField') but mutator declares field class 'safe'",
            "mutator effective class 'safe' differs from transition-tree field class 'gated' for 'gatedField' but effectiveChangeReason is missing",
            "mutator effective class differs for 'gatedField', but fieldChangeClass is '(missing)' instead of 'gated'",
        ]);
    });
});
