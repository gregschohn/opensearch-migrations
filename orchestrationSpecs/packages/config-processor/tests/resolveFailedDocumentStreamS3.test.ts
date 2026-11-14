import {resolveFailedDocumentStreamS3} from "../src/migrationConfigTransformer";

describe("resolveFailedDocumentStreamS3", () => {
    it("resolves the URI and passes through explicit region/endpoint", () => {
        const out = resolveFailedDocumentStreamS3(
            {
                repoPathUri: "s3://user-bucket/failures/",
                awsRegion: "us-west-2",
                endpoint: "https://s3.example",
                maxBufferBytes: 1024,
            },
            {awsRegion: "eu-west-1", endpoint: "https://repo.example"},
            {defaultS3Bucket: "dep-bucket", defaultS3Region: "us-east-1"}
        );
        expect(out).toEqual({
            failedDocumentStreamS3Prefix: "failures/",
            failedDocumentStreamS3Bucket: "user-bucket",
            disableFailedDocumentStream: false,
            failedDocumentStreamS3Region: "us-west-2",
            failedDocumentStreamS3Endpoint: "https://s3.example",
            failedDocumentStreamMaxBufferBytes: 1024,
        });
    });

    it("inherits the snapshot repo's region/endpoint ahead of the deployment defaults", () => {
        const out = resolveFailedDocumentStreamS3(
            {repoPathUri: "s3://user-bucket"},
            {awsRegion: "eu-west-1", endpoint: "https://repo.example"},
            {defaultS3Region: "us-east-1", defaultS3Endpoint: "https://dep.example"}
        );
        expect(out.failedDocumentStreamS3Bucket).toBe("user-bucket");
        expect(out.failedDocumentStreamS3Region).toBe("eu-west-1");
        expect(out.failedDocumentStreamS3Endpoint).toBe("https://repo.example");
    });

    it("falls back to the deployment region/endpoint when neither the user nor the repo supplies one", () => {
        const out = resolveFailedDocumentStreamS3(
            {repoPathUri: "s3://user-bucket"},
            undefined,
            {defaultS3Region: "us-east-1", defaultS3Endpoint: "https://dep.example"}
        );
        expect(out.failedDocumentStreamS3Region).toBe("us-east-1");
        expect(out.failedDocumentStreamS3Endpoint).toBe("https://dep.example");
    });

    it("disables the stream only through the explicit disabled choice", () => {
        // A deployment-provisioned default must not quietly turn the stream on.
        const out = resolveFailedDocumentStreamS3(
            {type: "disabled"},
            {awsRegion: "eu-west-1", endpoint: "https://repo.example"},
            {defaultS3Bucket: "dep-bucket", defaultS3Region: "us-east-1", defaultS3Endpoint: "https://dep.example"}
        );
        expect(out).toEqual({
            failedDocumentStreamS3Bucket: undefined,
            disableFailedDocumentStream: true,
            failedDocumentStreamS3Region: undefined,
            failedDocumentStreamS3Endpoint: undefined,
        });
    });

    it("throws when a bucket is set but no region can be determined", () => {
        expect(() =>
            resolveFailedDocumentStreamS3({repoPathUri: "s3://user-bucket"}, undefined, {})
        ).toThrow(/no region could be determined/);
    });

    it.each([
        ["s3://user-bucket", undefined],
        ["s3://user-bucket/", ""],
        ["s3://user-bucket/a/../failures/", "a/../failures/"],
    ])("preserves the prefix choice in %s", (repoPathUri, prefix) => {
        const out = resolveFailedDocumentStreamS3({repoPathUri}, undefined, {defaultS3Region: "us-east-1"});
        expect(out.failedDocumentStreamS3Prefix).toBe(prefix);
    });
});
