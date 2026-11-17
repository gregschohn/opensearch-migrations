/**
 * A small, strict evaluator for the builder's expression trees, for tests that need the value Argo
 * would compute rather than the expression text.
 *
 * It interprets the exact `BaseExpression` objects a workflow template is built from, so a test that
 * uses it exercises the real template code. Only the operators and functions the evaluated templates
 * use are implemented, and anything else throws rather than guessing.
 *
 * The semantics follow Argo's expression environment (expr-lang plus Sprig):
 *
 * - Parameter values are strings. `fromJSON` parses them; numbers become floats, as in Go.
 * - `record['missing']` is nil, which `toJSON` renders as `null`. Indexing nil fails.
 * - `sprig.merge(dst, src...)` is mergo's non-overriding merge: a key already in `dst` keeps its
 *   value unless that value is empty (`""`, `false`, `0`, nil, or an empty map/list); nested maps
 *   merge recursively.
 * - `sprig.dig(k1, ..., default, dict)` returns the default when a key along the path is missing.
 * - `toJSON` is Go's `json.Marshal`: object keys sorted, `<`, `>` and `&` escaped.
 * - A `{{=...}}` result substituted into text is the string itself, `true`/`false`, a number in
 *   Go's `%v` form, or JSON for a map or list.
 *
 * The argo-workflow-builders integration suite (`tests/integ/contracts`) pins these behaviours
 * against a real Argo; this evaluator is only as faithful as that list.
 */
import {
    ArrayIndexExpression,
    ArrayMakeExpression,
    AsStringExpression,
    BaseExpression,
    ComparisonExpression,
    ConcatExpression,
    DictMakeExpression,
    FromParameterExpression,
    FunctionExpression,
    InfixExpression,
    LiteralExpression,
    RecordFieldSelectExpression,
    RecordGetExpression,
    TernaryExpression,
    UnquotedTypeWrapper,
    WorkflowValueExpression,
    YamlPlainSafeStringExpression,
} from "@opensearch-migrations/argo-workflow-builders";
import * as yaml from "yaml";

type AnyExpr = BaseExpression<any, any>;

export type EvaluationScope = {
    /** Template input parameter values, as Argo holds them: strings. */
    inputs: Record<string, string>;
    /** `workflow.<name>` values such as `name` and `namespace`. */
    workflowValues?: Record<string, string>;
};

export class UnsupportedExpressionError extends Error {}

function fail(message: string): never {
    throw new UnsupportedExpressionError(message);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isGoEmpty(value: unknown): boolean {
    if (value === null || value === undefined) return true;
    if (value === "" || value === false || value === 0) return true;
    if (Array.isArray(value)) return value.length === 0;
    if (isPlainObject(value)) return Object.keys(value).length === 0;
    return false;
}

function mergoMerge(dst: Record<string, unknown>, src: Record<string, unknown>): Record<string, unknown> {
    for (const [key, srcValue] of Object.entries(src)) {
        if (srcValue === null || srcValue === undefined) continue;
        const dstValue = dst[key];
        if (isPlainObject(dstValue) && isPlainObject(srcValue)) {
            mergoMerge(dstValue, srcValue);
            continue;
        }
        if (!(key in dst) || isGoEmpty(dstValue)) {
            dst[key] = srcValue;
        }
    }
    return dst;
}

function deepCopy<T>(value: T): T {
    return value === undefined ? value : JSON.parse(JSON.stringify(value));
}

/** Go's `json.Marshal` for the JSON values these templates produce. */
export function goJsonMarshal(value: unknown): string {
    if (value === null || value === undefined) return "null";
    if (typeof value === "string") {
        return JSON.stringify(value)
            .replace(/</g, "\\u003c")
            .replace(/>/g, "\\u003e")
            .replace(/&/g, "\\u0026")
            .replace(/\u2028/g, "\\u2028")
            .replace(/\u2029/g, "\\u2029");
    }
    if (typeof value === "number") {
        if (!Number.isFinite(value)) fail(`cannot marshal ${value}`);
        const abs = Math.abs(value);
        if (abs !== 0 && (abs < 1e-6 || abs >= 1e21)) fail(`exponent-form float ${value} is not modelled`);
        return JSON.stringify(value);
    }
    if (typeof value === "boolean") return value ? "true" : "false";
    if (Array.isArray(value)) return `[${value.map(goJsonMarshal).join(",")}]`;
    if (isPlainObject(value)) {
        const keys = Object.keys(value).sort();
        return `{${keys.map(k => `${goJsonMarshal(k)}:${goJsonMarshal(value[k])}`).join(",")}}`;
    }
    return fail(`cannot marshal a ${typeof value}`);
}

/** Go's `fmt.Sprintf("%v", f)` for a float64: shortest `%g`, exponent from 1e+06. */
function goFormatFloat(value: number): string {
    if (Number.isInteger(value) && Math.abs(value) < 1e6) return String(value);
    const exponent = value === 0 ? 0 : Math.floor(Math.log10(Math.abs(value)));
    if (exponent < -4 || exponent >= 6) {
        const [mantissa, exp] = value.toExponential().split("e");
        const sign = exp.startsWith("-") ? "-" : "+";
        const digits = exp.replace(/^[-+]/, "").padStart(2, "0");
        return `${mantissa}e${sign}${digits}`;
    }
    return String(value);
}

/** The text a `{{=...}}` result becomes when Argo substitutes it into a template. */
export function argoSubstitutionText(value: unknown): string {
    if (value === null || value === undefined) fail("an expression evaluated to nil, which Argo refuses to substitute");
    if (typeof value === "string") return value;
    if (typeof value === "boolean") return value ? "true" : "false";
    if (typeof value === "number") return goFormatFloat(value);
    return goJsonMarshal(value);
}

function asBool(value: unknown, what: string): boolean {
    if (typeof value !== "boolean") fail(`${what} must be a boolean, got ${JSON.stringify(value)}`);
    return value;
}

function asString(value: unknown, what: string): string {
    if (typeof value !== "string") fail(`${what} must be a string, got ${JSON.stringify(value)}`);
    return value;
}

function jsonPathGet(document: unknown, path: string): unknown {
    if (!path.startsWith("$.")) fail(`unsupported JSONPath ${path}`);
    let node: any = document;
    for (const segment of path.slice(2).split(".")) {
        const match = /^([^[\]]+)((?:\[\d+])*)$/.exec(segment);
        if (!match) fail(`unsupported JSONPath segment ${segment}`);
        if (!isPlainObject(node) || !(match[1] in node)) fail(`JSONPath ${path} does not resolve`);
        node = node[match[1]];
        for (const index of match[2].matchAll(/\[(\d+)]/g)) {
            if (!Array.isArray(node)) fail(`JSONPath ${path} indexes a non-list`);
            node = node[Number(index[1])];
        }
    }
    return node;
}

function callFunction(name: string, args: unknown[]): unknown {
    switch (name) {
        case "fromJSON":
            return JSON.parse(asString(args[0], "fromJSON's argument"));
        case "toJSON":
            return goJsonMarshal(args[0]);
        case "toBase64":
            return Buffer.from(asString(args[0], "toBase64's argument"), "utf8").toString("base64");
        case "!":
            return !asBool(args[0], "the operand of !");
        case "len": {
            const value = args[0];
            if (typeof value === "string" || Array.isArray(value)) return value.length;
            return fail(`len of ${JSON.stringify(value)}`);
        }
        case "sprig.merge": {
            if (!args.every(isPlainObject)) fail("sprig.merge takes maps");
            const [dst, ...srcs] = args.map(deepCopy) as Record<string, unknown>[];
            return srcs.reduce(mergoMerge, dst);
        }
        case "sprig.omit": {
            const [dict, ...keys] = args;
            if (!isPlainObject(dict)) fail("sprig.omit takes a map");
            const omitted = new Set(keys.map(k => asString(k, "an omitted key")));
            return Object.fromEntries(Object.entries(dict).filter(([k]) => !omitted.has(k)));
        }
        case "sprig.dig": {
            const dict = args[args.length - 1];
            const fallback = args[args.length - 2];
            const keys = args.slice(0, -2).map(k => asString(k, "a dig key"));
            let node: unknown = dict;
            for (const key of keys) {
                if (!isPlainObject(node)) fail("sprig.dig walked into a non-map");
                if (!(key in node)) return deepCopy(fallback);
                node = node[key];
            }
            return deepCopy(node);
        }
        case "sprig.concat": {
            if (!args.every(Array.isArray)) fail("sprig.concat takes lists");
            return (args as unknown[][]).flat();
        }
        default:
            return fail(`function ${name} is not modelled`);
    }
}

export function evaluate(e: AnyExpr, scope: EvaluationScope): unknown {
    if (e instanceof LiteralExpression) return deepCopy(e.value);
    if (e instanceof AsStringExpression || e instanceof YamlPlainSafeStringExpression) {
        return evaluate(e.source, scope);
    }
    if (e instanceof UnquotedTypeWrapper) return evaluate(e.value, scope);
    if (e instanceof FromParameterExpression) {
        const source = e.source;
        if (source.kind !== "input") fail(`${source.kind} parameters are not modelled`);
        if (!(source.parameterName in scope.inputs)) fail(`input ${source.parameterName} has no value`);
        return scope.inputs[source.parameterName];
    }
    if (e instanceof WorkflowValueExpression) {
        const value = scope.workflowValues?.[e.variable];
        if (value === undefined) fail(`workflow.${e.variable} has no value`);
        return value;
    }
    if (e instanceof ConcatExpression) {
        const parts = e.expressions.map((p: AnyExpr) => asString(evaluate(p, scope), "a concatenated value"));
        return parts.join(e.separator ?? "");
    }
    if (e instanceof TernaryExpression) {
        return asBool(evaluate(e.condition, scope), "a ternary condition")
            ? evaluate(e.whenTrue, scope)
            : evaluate(e.whenFalse, scope);
    }
    if (e instanceof ComparisonExpression) {
        const left = evaluate(e.left, scope) as any;
        const right = evaluate(e.right, scope) as any;
        switch (e.operator) {
            case "==": return left === right;
            case "!=": return left !== right;
            case "<": return left < right;
            case ">": return left > right;
            case "<=": return left <= right;
            case ">=": return left >= right;
        }
    }
    if (e instanceof InfixExpression) {
        switch (e.operator) {
            case "&&":
                return asBool(evaluate(e.left, scope), "&&") && asBool(evaluate(e.right, scope), "&&");
            case "||":
                return asBool(evaluate(e.left, scope), "||") || asBool(evaluate(e.right, scope), "||");
            default:
                return fail(`infix ${e.operator} is not modelled`);
        }
    }
    if (e instanceof RecordGetExpression) {
        const source = evaluate(e.source, scope);
        if (!isPlainObject(source)) fail(`cannot fetch ${e.key} from ${JSON.stringify(source)}`);
        return e.key in source ? source[e.key] : null;
    }
    if (e instanceof RecordFieldSelectExpression) {
        const source = asString(evaluate(e.source as AnyExpr, scope), "a jsonpath document");
        return jsonPathGet(JSON.parse(source), `$.${e.path as string}`);
    }
    if (e instanceof ArrayIndexExpression) {
        const array = evaluate(e.array, scope);
        if (!Array.isArray(array)) fail("indexing a non-list");
        return array[evaluate(e.index, scope) as number];
    }
    if (e instanceof ArrayMakeExpression) return e.elements.map((x: AnyExpr) => evaluate(x, scope));
    if (e instanceof DictMakeExpression) {
        return Object.fromEntries(
            Object.entries(e.entries as Record<string, AnyExpr>).map(([k, v]) => [k, evaluate(v, scope)])
        );
    }
    if (e instanceof FunctionExpression) {
        return callFunction(e.functionName, e.args.map((a: AnyExpr) => evaluate(a, scope)));
    }
    return fail(`expression kind ${e.kind} is not modelled`);
}

/** Whether the renderer emits `e` as a bare `{{identifier}}` rather than a `{{=...}}` expression. */
function rendersAsBareReference(e: AnyExpr): boolean {
    if (e instanceof UnquotedTypeWrapper) return rendersAsBareReference(e.value);
    if (e instanceof AsStringExpression || e instanceof YamlPlainSafeStringExpression) {
        return rendersAsBareReference(e.source);
    }
    return e instanceof FromParameterExpression || e instanceof WorkflowValueExpression;
}

/**
 * The string value a step argument passes to the called template.
 *
 * Argo parameters are strings. A top-level `fromJSON` is dropped by the renderer (the argument is
 * passed through as text), so it is dropped here too.
 */
export function evaluateArgument(value: unknown, scope: EvaluationScope): string {
    if (!(value instanceof BaseExpression)) {
        if (typeof value === "string") return value;
        if (typeof value === "number" || typeof value === "boolean") return String(value);
        return fail(`a literal ${typeof value} argument is not modelled`);
    }
    let e: AnyExpr = value;
    while (e instanceof AsStringExpression) e = e.source;
    if (e instanceof FunctionExpression && e.functionName === "fromJSON") e = e.args[0];
    return argoSubstitutionText(evaluate(e, scope));
}

/**
 * The value a resource-manifest node takes once Argo has substituted it and the manifest has been
 * parsed as YAML.
 *
 * - An unquoted (`makeDirectTypeProxy`) node is substituted raw: a bare `{{inputs.parameters.x}}`
 *   is the parameter's text, which YAML then types (`9201` becomes a number); a `{{=...}}` result
 *   is substituted as text and parsed the same way.
 * - Any other expression is rendered as `{{=toJSON(...)}}`, so it parses back to its JSON value.
 * - A quoted, YAML-plain-safe string stays a string.
 */
export function evaluateManifest(node: unknown, scope: EvaluationScope): unknown {
    if (node instanceof UnquotedTypeWrapper) {
        const value = evaluate(node, scope);
        const text = rendersAsBareReference(node) ? asString(value, "a bare reference") : argoSubstitutionText(value);
        return yaml.parse(text);
    }
    if (node instanceof YamlPlainSafeStringExpression) {
        return asString(evaluate(node, scope), "a YAML-plain-safe string");
    }
    if (node instanceof BaseExpression) return evaluate(node, scope);
    if (Array.isArray(node)) return node.map(n => evaluateManifest(n, scope));
    if (isPlainObject(node)) {
        return Object.fromEntries(Object.entries(node).map(([k, v]) => [k, evaluateManifest(v, scope)]));
    }
    return node;
}
