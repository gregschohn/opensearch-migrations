import {USER_RFS_OPTIONS} from "../src/userSchemas";

describe("failed document stream choice", () => {
    test.each([
        ["an S3 destination", {failedDocumentStream: {repoPathUri: "s3://bucket"}}],
        ["the explicit opt-out", {failedDocumentStream: {type: "disabled"}}],
    ])("accepts %s", (_name, data) => {
        expect(USER_RFS_OPTIONS.safeParse({maxShardSizeBytes: 1024, ...data}).success).toBe(true);
    });

    test.each([
        ["no choice", {}],
        ["a blank S3 bucket", {failedDocumentStream: {repoPathUri: "s3://  "}}],
        ["an unsupported destination", {failedDocumentStream: {type: "other"}}],
        ["S3 fields on the disabled choice", {failedDocumentStream: {type: "disabled", repoPathUri: "s3://bucket"}}],
        ["a GCS output URI", {failedDocumentStream: {repoPathUri: "gs://bucket"}}],
        ["an Azure output URI", {failedDocumentStream: {repoPathUri: "azure://account/container"}}],
        ["the former typed bucket shape", {failedDocumentStream: {type: "s3", bucket: "bucket"}}],
        ["an invalid endpoint", {failedDocumentStream: {repoPathUri: "s3://bucket", endpoint: "not-a-url"}}],
    ])("rejects %s", (_name, data) => {
        const result = USER_RFS_OPTIONS.safeParse({maxShardSizeBytes: 1024, ...data});
        expect(result.success).toBe(false);
    });

    it("applies S3 defaults inside the selected destination", () => {
        const parsed = USER_RFS_OPTIONS.parse({
            maxShardSizeBytes: 1024,
            failedDocumentStream: {repoPathUri: "s3://bucket"},
        });
        expect(parsed.failedDocumentStream).toEqual({
            repoPathUri: "s3://bucket",
            awsRegion: "",
            endpoint: "",
            maxBufferBytes: 67108864,
        });
    });
});

describe("maxDocumentSizeBytes", () => {
    const base = {maxShardSizeBytes: 1024, failedDocumentStream: {type: "disabled" as const}};

    test("is optional and has no default", () => {
        expect(USER_RFS_OPTIONS.parse(base).maxDocumentSizeBytes).toBeUndefined();
    });

    test.each([[0], [-1], [1.5]])("rejects %s", (value) => {
        expect(USER_RFS_OPTIONS.safeParse({...base, maxDocumentSizeBytes: value}).success).toBe(false);
    });

    test("accepts a positive integer", () => {
        expect(USER_RFS_OPTIONS.parse({...base, maxDocumentSizeBytes: 104857600}).maxDocumentSizeBytes).toBe(104857600);
    });
});
