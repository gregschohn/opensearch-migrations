export type NoBareTemplateString<T> =
    T extends string
        ? T extends `${string}{{${string}}}${string}` ? never : T
        : T;

const BARE_TEMPLATE_PATTERN = /{{[^}]*}}/;

export function assertNoBareTemplateString(value: unknown, context: string) {
    if (typeof value === "string" && BARE_TEMPLATE_PATTERN.test(value)) {
        throw new Error(
            `${context} received raw Argo template syntax '${value}'. ` +
            "Use expression helpers instead of embedding bare {{...}} markers."
        );
    }
}
