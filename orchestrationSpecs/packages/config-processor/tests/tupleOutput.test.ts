import {MigrationConfigTransformer, buildResolvedMigrationResources} from "../src";

function configWithTupleOutput(tupleOutput?: unknown) {
    return {
        sourceClusters: {
            source: {version: "ES 7.10.2", endpoint: "http://source:9200"},
        },
        targetClusters: {
            target: {endpoint: "http://target:9200"},
        },
        snapshotMigrationConfigs: [],
        traffic: {
            proxies: {
                capture: {source: "source", proxyConfig: {listenPort: 9201}},
            },
            replayers: {
                replay: {
                    fromCapturedTraffic: "capture",
                    toTarget: "target",
                    replayerConfig: {
                        ...(tupleOutput === undefined ? {} : {tupleOutput}),
                        tupleMaxBufferSeconds: 5,
                    },
                },
            },
        },
    };
}

describe("tuple output destination", () => {
    const transformer = new MigrationConfigTransformer();

    it("lowers a repository URI into the existing replayer and TrafficReplay fields", async () => {
        const config = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri: "s3://tuple-results/custom/tuples/",
            awsRegion: "us-west-2",
            endpoint: "http://localhost:4566",
        }));
        const expected = {
            tupleS3Bucket: "tuple-results",
            tupleS3Prefix: "custom/tuples/",
            tupleS3Region: "us-west-2",
            tupleS3Endpoint: "http://localhost:4566",
            tupleMaxBufferSeconds: 5,
        };
        expect(config.trafficReplays[0].replayerConfig).toMatchObject(expected);
        expect(config.trafficReplays[0].replayerConfig).not.toHaveProperty("tupleOutput");

        const resources = buildResolvedMigrationResources(config, "workflow");
        const replay = resources.resources.find(resource => resource.kind === "TrafficReplay");
        expect(replay?.parameters).toMatchObject(expected);
        expect(replay?.parameters).not.toHaveProperty("tupleOutput");
    });

    it("retains tuple defaults when the URI has no path", async () => {
        const config = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri: "s3://tuple-results", awsRegion: "us-west-2",
        }));
        expect(config.trafficReplays[0].replayerConfig).toMatchObject({
            tupleS3Prefix: "tuples/",
            tupleS3Endpoint: "",
            tupleMaxFileSizeMb: 256,
            tupleMaxPerFile: 0,
        });
    });

    it.each([
        ["s3://tuple-results/", ""],
        ["s3://tuple-results/a/../tuples/", "a/../tuples/"],
        ["s3://tuple-results/tuples", "tuples"],
    ])("preserves the literal prefix in %s", async (repoPathUri, prefix) => {
        const config = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri, awsRegion: "us-west-2",
        }));
        expect(config.trafficReplays[0].replayerConfig.tupleS3Prefix).toBe(prefix);
    });

    it.each([
        ["localstack://127.0.0.1:4566", "http://127.0.0.1:4566"],
        ["localstacks://127.0.0.1:4566", "https://127.0.0.1:4566"],
        ["http://[::1]:4566", "http://[::1]:4566"],
    ])("resolves the storage endpoint %s before recording it", async (endpoint, expected) => {
        const config = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri: "s3://tuple-results", awsRegion: "us-west-2", endpoint,
        }));
        expect(config.trafficReplays[0].replayerConfig.tupleS3Endpoint).toBe(expected);
    });

    it.each([undefined, {type: "disabled"}])("retains logger output for %j", async tupleOutput => {
        const config = await transformer.processFromObject(configWithTupleOutput(tupleOutput));
        expect(config.trafficReplays[0].replayerConfig.tupleS3Bucket).toBeUndefined();
        expect(config.trafficReplays[0].replayerConfig.tupleS3Region).toBeUndefined();
    });

    it("keeps the same replay identity when S3 persistence is omitted or explicitly disabled", async () => {
        const omitted = await transformer.processFromObject(configWithTupleOutput());
        const disabled = await transformer.processFromObject(configWithTupleOutput({type: "disabled"}));
        expect(disabled.trafficReplays[0]).toEqual(omitted.trafficReplays[0]);
    });

    it("includes the destination in the replay config checksum", async () => {
        const first = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri: "s3://tuple-results", awsRegion: "us-west-2",
        }));
        const second = await transformer.processFromObject(configWithTupleOutput({
            repoPathUri: "s3://other-results", awsRegion: "us-west-2",
        }));
        expect(second.trafficReplays[0].configChecksum).not.toBe(first.trafficReplays[0].configChecksum);
    });

    it.each([
        {repoPathUri: "s3://tuple-results"},
        {repoPathUri: "s3://tuple-results", awsRegion: "  "},
        {repoPathUri: "s3://  ", awsRegion: "us-west-2"},
        {repoPathUri: "s3://tuple-results", awsRegion: "us-west-2", endpoint: "not-an-endpoint"},
        {type: "disabled", repoPathUri: "s3://tuple-results"},
        {repoPathUri: "gs://tuple-results", awsRegion: "us-west-2"},
        {repoPathUri: "azure://account/tuple-results", awsRegion: "us-west-2"},
        {type: "s3", bucket: "tuple-results", region: "us-west-2"},
    ])("rejects invalid or unsupported destinations: %j", async tupleOutput => {
        await expect(transformer.processFromObject(configWithTupleOutput(tupleOutput))).rejects.toThrow();
    });

    it("rejects the former flat user fields", async () => {
        const config = configWithTupleOutput();
        Object.assign(config.traffic.replayers.replay.replayerConfig, {
            tupleS3Bucket: "tuple-results",
            tupleS3Region: "us-west-2",
        });
        await expect(transformer.processFromObject(config)).rejects.toThrow(/tupleS3/);
    });
});
