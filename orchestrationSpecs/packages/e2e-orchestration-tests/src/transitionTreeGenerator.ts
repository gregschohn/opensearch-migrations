import { OVERALL_MIGRATION_CONFIG } from "@opensearch-migrations/schemas";
import { z } from "zod";

import { ChangeClass } from "./types";
import { Mutator } from "./fixtures/mutators";

export interface TransitionTreeField {
    /** Dot-separated user-config path. Records use `{key}`; arrays use `[]`. */
    path: string;
    changeClass: ChangeClass;
    /** True when this node had its own schema metadata rather than inheriting. */
    explicit: boolean;
    /** Nearest ancestor path that supplied this inherited class, if any. */
    inheritedFrom?: string;
    checksumFor?: readonly string[];
}

export interface TransitionTree {
    sourceSchema: string;
    generatedAt: string;
    fields: TransitionTreeField[];
}

export interface TransitionTreeGenerationOptions {
    sourceSchema?: string;
    generatedAt?: string;
}

type FieldMeta = {
    checksumFor?: readonly string[];
    changeRestriction?: "gated" | "impossible";
};

const ARRAY_TOKEN = "[]";
const RECORD_KEY_TOKEN = "{key}";

const CHANGE_CLASS_RANK: Readonly<Record<ChangeClass, number>> = {
    safe: 0,
    gated: 1,
    impossible: 2,
};

/**
 * Generate the user-config transition tree from the public workflow
 * config schema. Missing changeRestriction metadata is safe by default.
 *
 * Restrictions inherit down object/array/record subtrees. This matters
 * for fields like `traffic.proxies.{key}.proxyConfig.tls`, where the
 * parent is gated and test mutators may target a child such as
 * `tls.clientAuth`.
 */
export function generateTransitionTree(
    schema: z.ZodTypeAny = OVERALL_MIGRATION_CONFIG,
    opts: TransitionTreeGenerationOptions = {},
): TransitionTree {
    const fields = collectTransitionFields(schema);
    return {
        sourceSchema: opts.sourceSchema ?? "OVERALL_MIGRATION_CONFIG",
        generatedAt: opts.generatedAt ?? "not-recorded",
        fields,
    };
}

export function collectTransitionFields(schema: z.ZodTypeAny): TransitionTreeField[] {
    const byPath = new Map<string, TransitionTreeField>();
    walkSchema(schema, [], undefined, undefined, byPath, new Set());
    return [...byPath.values()].sort((a, b) => a.path.localeCompare(b.path));
}

export function resolveTransitionField(
    tree: Pick<TransitionTree, "fields">,
    userConfigPath: string,
): TransitionTreeField | undefined {
    const actualTokens = splitPath(userConfigPath);
    const candidates = tree.fields.filter((field) =>
        patternMatchesOrContains(field.path, actualTokens),
    );
    candidates.sort((a, b) => {
        const lenDiff = splitPath(b.path).length - splitPath(a.path).length;
        if (lenDiff !== 0) return lenDiff;
        const rankDiff = CHANGE_CLASS_RANK[b.changeClass] - CHANGE_CLASS_RANK[a.changeClass];
        if (rankDiff !== 0) return rankDiff;
        return a.path.localeCompare(b.path);
    });
    return candidates[0];
}

export interface MutatorTransitionValidationError {
    mutatorName: string;
    changedPath: string;
    message: string;
}

export function validateMutatorAgainstTransitionTree(
    mutator: Pick<
        Mutator,
        "name" | "changedPaths" | "fieldChangeClass" | "changeClass" | "effectiveChangeReason"
    >,
    tree: Pick<TransitionTree, "fields">,
): MutatorTransitionValidationError[] {
    const errors: MutatorTransitionValidationError[] = [];
    for (const changedPath of mutator.changedPaths) {
        const field = resolveTransitionField(tree, changedPath);
        if (!field) {
            errors.push({
                mutatorName: mutator.name,
                changedPath,
                message: `changed path '${changedPath}' does not resolve to a user-config transition-tree field`,
            });
            continue;
        }

        const declaredFieldClass = mutator.fieldChangeClass ?? mutator.changeClass;
        if (declaredFieldClass !== field.changeClass) {
            errors.push({
                mutatorName: mutator.name,
                changedPath,
                message:
                    `changed path '${changedPath}' is '${field.changeClass}' in the transition tree ` +
                    `(matched '${field.path}') but mutator declares field class '${declaredFieldClass}'`,
            });
        }

        if (mutator.changeClass !== field.changeClass) {
            if (!mutator.effectiveChangeReason) {
                errors.push({
                    mutatorName: mutator.name,
                    changedPath,
                    message:
                        `mutator effective class '${mutator.changeClass}' differs from transition-tree ` +
                        `field class '${field.changeClass}' for '${changedPath}' but effectiveChangeReason is missing`,
                });
            }
            if (mutator.fieldChangeClass !== field.changeClass) {
                errors.push({
                    mutatorName: mutator.name,
                    changedPath,
                    message:
                        `mutator effective class differs for '${changedPath}', but fieldChangeClass ` +
                        `is '${mutator.fieldChangeClass ?? "(missing)"}' instead of '${field.changeClass}'`,
                });
            }
        }
    }
    return errors;
}

function walkSchema(
    schema: z.ZodTypeAny,
    pathTokens: string[],
    inheritedClass: ChangeClass | undefined,
    inheritedFrom: string | undefined,
    out: Map<string, TransitionTreeField>,
    seen: Set<z.ZodTypeAny>,
): void {
    const meta = fieldMeta(schema);
    const ownClass = meta?.changeRestriction;
    const path = pathTokens.join(".");
    const changeClass: ChangeClass = ownClass ?? inheritedClass ?? "safe";
    const explicit = ownClass !== undefined;
    const nextInheritedFrom = explicit && path ? path : inheritedFrom;

    if (path) {
        mergeField(out, {
            path,
            changeClass,
            explicit,
            inheritedFrom: explicit ? undefined : inheritedFrom,
            checksumFor: meta?.checksumFor,
        });
    }

    const childSchema = childBearingSchema(schema);
    if (!childSchema || seen.has(childSchema)) return;
    seen.add(childSchema);

    if (childSchema instanceof z.ZodObject) {
        const shape = childSchema.shape as Record<string, z.ZodTypeAny>;
        for (const key of Object.keys(shape).sort()) {
            walkSchema(
                shape[key],
                [...pathTokens, key],
                changeClass,
                nextInheritedFrom,
                out,
                seen,
            );
        }
        seen.delete(childSchema);
        return;
    }

    if (childSchema instanceof z.ZodArray) {
        walkSchema(
            arrayElement(childSchema as unknown as z.ZodTypeAny),
            [...pathTokens, ARRAY_TOKEN],
            changeClass,
            nextInheritedFrom,
            out,
            seen,
        );
        seen.delete(childSchema);
        return;
    }

    if (childSchema instanceof z.ZodRecord) {
        const value = recordValue(childSchema);
        if (value) {
            walkSchema(
                value,
                [...pathTokens, RECORD_KEY_TOKEN],
                changeClass,
                nextInheritedFrom,
                out,
                seen,
            );
        }
        seen.delete(childSchema);
        return;
    }

    if (childSchema instanceof z.ZodUnion || childSchema instanceof z.ZodDiscriminatedUnion) {
        for (const option of unionOptions(childSchema)) {
            walkSchema(option, pathTokens, changeClass, nextInheritedFrom, out, seen);
        }
        seen.delete(childSchema);
        return;
    }

    if (zodDef(childSchema).type === "pipe") {
        const def = zodDef(childSchema);
        if (def.out) {
            walkSchema(def.out as z.ZodTypeAny, pathTokens, changeClass, nextInheritedFrom, out, seen);
        }
        if (def.in) {
            walkSchema(def.in as z.ZodTypeAny, pathTokens, changeClass, nextInheritedFrom, out, seen);
        }
        seen.delete(childSchema);
        return;
    }

    if (childSchema instanceof z.ZodIntersection) {
        const def = zodDef(childSchema);
        if (def.left) {
            walkSchema(def.left as z.ZodTypeAny, pathTokens, changeClass, nextInheritedFrom, out, seen);
        }
        if (def.right) {
            walkSchema(def.right as z.ZodTypeAny, pathTokens, changeClass, nextInheritedFrom, out, seen);
        }
    }

    seen.delete(childSchema);
}

function mergeField(out: Map<string, TransitionTreeField>, next: TransitionTreeField): void {
    const current = out.get(next.path);
    if (!current) {
        out.set(next.path, next);
        return;
    }

    const changeClass = mostRestrictive(current.changeClass, next.changeClass);
    out.set(next.path, {
        path: next.path,
        changeClass,
        explicit: current.explicit || next.explicit,
        inheritedFrom: next.explicit ? undefined : current.inheritedFrom ?? next.inheritedFrom,
        checksumFor: uniqueStrings([...(current.checksumFor ?? []), ...(next.checksumFor ?? [])]),
    });
}

function mostRestrictive(a: ChangeClass, b: ChangeClass): ChangeClass {
    return CHANGE_CLASS_RANK[a] >= CHANGE_CLASS_RANK[b] ? a : b;
}

function uniqueStrings(values: readonly string[]): readonly string[] | undefined {
    const unique = [...new Set(values)].sort();
    return unique.length > 0 ? unique : undefined;
}

function fieldMeta(schema: z.ZodTypeAny): FieldMeta | undefined {
    return schema.meta?.() as FieldMeta | undefined;
}

function childBearingSchema(schema: z.ZodTypeAny): z.ZodTypeAny | undefined {
    return unwrap(schema);
}

function unwrap(schema: z.ZodTypeAny): z.ZodTypeAny {
    let current = schema;
    while (true) {
        const def = zodDef(current);
        if (
            def.type === "optional" ||
            def.type === "nullable" ||
            def.type === "nonoptional" ||
            def.type === "readonly" ||
            def.type === "catch" ||
            def.type === "prefault"
        ) {
            current = def.innerType as z.ZodTypeAny;
            continue;
        }
        if (def.type === "default") {
            current = def.innerType as z.ZodTypeAny;
            continue;
        }
        return current;
    }
}

function arrayElement(schema: z.ZodTypeAny): z.ZodTypeAny {
    return zodDef(schema).element as z.ZodTypeAny;
}

function recordValue(schema: z.ZodRecord): z.ZodTypeAny | undefined {
    return zodDef(schema).valueType as z.ZodTypeAny | undefined;
}

function unionOptions(schema: z.ZodUnion | z.ZodDiscriminatedUnion): z.ZodTypeAny[] {
    return [...(zodDef(schema).options as z.ZodTypeAny[] | undefined ?? [])];
}

function zodDef(schema: z.ZodTypeAny): Record<string, unknown> {
    return (schema as unknown as { _def: Record<string, unknown> })._def;
}

function patternMatchesOrContains(patternPath: string, actualTokens: readonly string[]): boolean {
    const patternTokens = splitPath(patternPath);
    if (patternTokens.length > actualTokens.length) return false;
    return patternTokens.every((token, i) => tokenMatches(token, actualTokens[i]));
}

function tokenMatches(patternToken: string, actualToken: string | undefined): boolean {
    if (actualToken === undefined) return false;
    if (patternToken === RECORD_KEY_TOKEN) return actualToken.length > 0;
    if (patternToken === ARRAY_TOKEN) return /^\d+$/.test(actualToken);
    return patternToken === actualToken;
}

function splitPath(path: string): string[] {
    return path.split(".").filter((token) => token.length > 0);
}
