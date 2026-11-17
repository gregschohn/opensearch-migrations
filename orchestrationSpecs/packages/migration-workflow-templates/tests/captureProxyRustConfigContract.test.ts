/**
 * Contract between the orchestration and the Rust capture proxy.
 *
 * For each representative migration spec this test renders exactly what the Kubernetes path hands
 * the proxy container, using the real code on both sides of the Argo boundary:
 *
 *   1. the config processor (`MigrationConfigTransformer`) lowers the user spec, as
 *      `workflow configure` does;
 *   2. the `setupProxy` steps of the real `SetupCapture` template compute the arguments of the
 *      Deployment template they select, including the `---INLINE-JSON` document;
 *   3. that template's Deployment manifest and the Kafka client-properties ConfigMap are evaluated
 *      into the pod the cluster would run.
 *
 * Step 2 and 3 run the templates' own expression trees through `support/argoTemplateEvaluator`.
 *
 * The result is committed as JSON fixtures under the Rust workspace, where
 * `crates/capture-config/tests/orchestration_contract.rs` feeds each container's arguments and
 * environment to the proxy's configuration loader and checks the interpretation in `expect`, and
 * `crates/capture-proxy/tests/orchestration_contract.rs` builds the Kafka clients from them.
 *
 * Fixtures are compared byte for byte; regenerate them with `npx jest <this file> -u`.
 */
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import {
    renderWorkflowTemplate,
    toArgoExpressionString,
    transformExpressionsDeep,
    unwrapPlaceholdersAndStringify,
} from "@opensearch-migrations/argo-workflow-builders";
import {buildUnifiedSchema, UNIFIED_SCHEMA_PATH_ENV} from "@opensearch-migrations/schemas";
import {MigrationConfigTransformer} from "@opensearch-migrations/config-processor";
import {SetupCapture} from "../src/workflowTemplates/setupCapture";
import {evaluate, evaluateArgument, evaluateManifest, EvaluationScope} from "./support/argoTemplateEvaluator";

const FIXTURE_DIR = path.resolve(
    __dirname, "..", "..", "..", "..",
    "TrafficCapture", "captureProxy", "crates", "capture-config", "tests", "fixtures", "orchestration"
);

// Validation reads the unified schema. Point it at one built from the checked-in Strimzi fixture,
// exactly as the config-processor tests do, so nothing tries to read a live cluster.
const unifiedSchemaPath = path.join(os.tmpdir(), `capture-proxy-contract-unified-schema-${process.pid}.json`);
fs.writeFileSync(unifiedSchemaPath, JSON.stringify(buildUnifiedSchema({
    strimziSchemaPath: path.resolve(__dirname, "..", "..", "schemas", "tests", "fixtures", "strimzi", "minimal-openapi.json"),
}).schema));
process.env[UNIFIED_SCHEMA_PATH_ENV] = unifiedSchemaPath;

const PROXY_NAME = "capture-proxy";
const RESOURCE_UID = "00000000-0000-4000-8000-000000000001";
const WORKFLOW_VALUES = {name: "migration-workflow", namespace: "ma"};
const IMAGE_INPUTS = {
    imageCaptureProxyLocation: "migrations/capture_proxy:latest",
    imageCaptureProxyPullPolicy: "IfNotPresent",
    imageMigrationConsoleLocation: "migrations/migration_console:latest",
    imageMigrationConsolePullPolicy: "IfNotPresent",
};
/** What the chart's provider-config ConfigMap supplies to template inputs that read it. */
const PROVIDER_CONFIG: Record<string, string> = {
    cloudProvider: "aws",
    awsResourceTags: "",
    proxyNodePoolName: "",
    workloadZoneSpread: "none",
    workloadZoneSpreadConstraintExtras: "{}",
};
const TEST_CLIENT_CA_PEM = [
    "-----BEGIN CERTIFICATE-----",
    "MIIBszCCAVmgAwIBAgIUY2xpZW50LWNhLWZvci1jb250cmFjdC10ZXN0MAoGCCqGSM49BAMCMBwx",
    "GjAYBgNVBAMMEWNvbnRyYWN0LWNsaWVudC1jYTAeFw0yNjAxMDEwMDAwMDBaFw0zNjAxMDEwMDAw",
    "-----END CERTIFICATE-----",
].join("\n");

type KafkaCluster = Record<string, unknown>;

function migrationSpec(args: {
    kafka?: KafkaCluster,
    proxyConfig: Record<string, unknown>,
    sourceAllowInsecure?: boolean,
}) {
    return {
        kafkaClusterConfiguration: {default: args.kafka ?? {autoCreate: {}}},
        sourceClusters: {
            source: {
                endpoint: "https://source-cluster:9200",
                allowInsecure: args.sourceAllowInsecure ?? true,
                version: "ES 7.10",
                authConfig: {basic: {secretName: "source-creds"}},
            },
        },
        targetClusters: {
            target: {
                endpoint: "https://target-cluster:9200",
                allowInsecure: true,
                authConfig: {basic: {secretName: "target-creds"}},
            },
        },
        snapshotMigrationConfigs: [],
        traffic: {
            proxies: {
                [PROXY_NAME]: {source: "source", proxyConfig: args.proxyConfig},
            },
            replayers: {},
        },
    };
}

const DEFAULT_TLS = {
    certChainFile: "/etc/proxy-tls/tls.crt",
    keyFile: "/etc/proxy-tls/tls.key",
    clientAuth: "none",
    trust: null,
};
const MANAGED_SCRAM_CAPTURE = {
    brokers: "default-kafka-bootstrap:9093",
    topic: PROXY_NAME,
    clientId: "HttpCaptureProxyProducer",
    auth: "scram-sha-512",
    username: "default-migration-app",
    passwordSet: true,
    listenerName: "tls",
    propertyFile: "/config/kafka-auth/client.properties",
};
const NO_SUPPRESSION = {method: null, uriPath: null, methodAndPath: null, headers: []};
const DEFAULT_TUNING = {
    workerThreads: 0,
    maxTrafficBufferSize: 1048576,
    connectionPool: {size: 0, recycleAfterSeconds: 30},
};

type Scenario = {
    name: string,
    description: string,
    spec: ReturnType<typeof migrationSpec>,
    /** Secrets the pod's secretKeyRefs read, as they would exist in the cluster. */
    secrets?: Record<string, Record<string, string>>,
    /** What the proxy must make of the configuration. Checked by the Rust tests. */
    expect: Record<string, unknown>,
};

const SCENARIOS: Scenario[] = [
    {
        name: "default-proxy",
        description: "Only listenPort set: TLS is provisioned by cert-manager by default, the " +
            "workflow-managed Kafka cluster uses SCRAM-SHA-512, the source is HTTPS without verification.",
        spec: migrationSpec({proxyConfig: {listenPort: 9201}}),
        secrets: {"default-migration-app": {password: "managed-scram-password"}},
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: DEFAULT_TLS,
            capture: MANAGED_SCRAM_CAPTURE,
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "plaintext-listener-managed-kafka-without-auth",
        description: "tls.mode plaintext opts out of TLS; the workflow-managed Kafka cluster has no auth.",
        spec: migrationSpec({
            kafka: {autoCreate: {auth: {type: "none"}}},
            proxyConfig: {listenPort: 9200, tls: {mode: "plaintext"}},
        }),
        expect: {
            accepted: true,
            listenPort: 9200,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: null,
            capture: {
                brokers: "default-kafka-bootstrap:9092",
                topic: PROXY_NAME,
                clientId: "HttpCaptureProxyProducer",
                auth: "none",
                username: null,
                passwordSet: false,
                listenerName: "plain",
                propertyFile: null,
            },
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "existing-tls-secret-verified-source-existing-kafka",
        description: "TLS from an existing secret, a source whose certificate is verified, and an " +
            "existing Kafka cluster without auth.",
        spec: migrationSpec({
            kafka: {existing: {kafkaConnection: "kafka-1.example.internal:9092,kafka-2.example.internal:9092"}},
            proxyConfig: {listenPort: 9201, tls: {mode: "existingSecret", secretName: "proxy-tls"}},
            sourceAllowInsecure: false,
        }),
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: false},
            tls: DEFAULT_TLS,
            capture: {
                brokers: "kafka-1.example.internal:9092,kafka-2.example.internal:9092",
                topic: PROXY_NAME,
                clientId: "HttpCaptureProxyProducer",
                auth: "none",
                username: null,
                passwordSet: false,
                listenerName: null,
                propertyFile: null,
            },
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "mtls-inline-client-ca",
        description: "cert-manager TLS with required client certificates, trusted through an inline " +
            "PEM delivered in CAPTURE_PROXY_SSL_TRUST_CERT_PEM.",
        spec: migrationSpec({
            proxyConfig: {
                listenPort: 9201,
                tls: {
                    mode: "certManager",
                    issuerRef: {name: "migrations-ca"},
                    dnsNames: [PROXY_NAME],
                    clientAuth: {trustedClientCaPem: TEST_CLIENT_CA_PEM},
                },
            },
        }),
        secrets: {"default-migration-app": {password: "managed-scram-password"}},
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: {...DEFAULT_TLS, clientAuth: "require", trust: {envVar: "CAPTURE_PROXY_SSL_TRUST_CERT_PEM"}},
            capture: MANAGED_SCRAM_CAPTURE,
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "mtls-client-ca-file-not-required",
        description: "Existing-secret TLS with a client CA mounted from a ConfigMap and client " +
            "certificates not required.",
        spec: migrationSpec({
            kafka: {existing: {kafkaConnection: "kafka.example.internal:9092"}},
            proxyConfig: {
                listenPort: 9201,
                tls: {
                    mode: "existingSecret",
                    secretName: "proxy-tls",
                    clientAuth: {trustedClientCaFile: {configMap: "client-ca", path: "ca.pem"}, required: false},
                },
            },
        }),
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: {...DEFAULT_TLS, clientAuth: "none", trust: {fileFromConfigMap: "client-ca", key: "ca.pem"}},
            capture: {
                brokers: "kafka.example.internal:9092",
                topic: PROXY_NAME,
                clientId: "HttpCaptureProxyProducer",
                auth: "none",
                username: null,
                passwordSet: false,
                listenerName: null,
                propertyFile: null,
            },
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "existing-kafka-scram-sha-512",
        description: "An existing Kafka cluster with SCRAM-SHA-512, its CA and password from user secrets.",
        spec: migrationSpec({
            kafka: {
                existing: {
                    kafkaConnection: "kafka.example.internal:9096",
                    auth: {
                        type: "scram-sha-512",
                        secretName: "existing-kafka-user",
                        caSecretName: "existing-kafka-ca",
                        kafkaUserName: "capture-user",
                    },
                },
            },
            proxyConfig: {listenPort: 9201, tls: {mode: "plaintext"}},
        }),
        secrets: {"existing-kafka-user": {password: "existing-scram-password"}},
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: null,
            capture: {
                brokers: "kafka.example.internal:9096",
                topic: PROXY_NAME,
                clientId: "HttpCaptureProxyProducer",
                auth: "scram-sha-512",
                username: "capture-user",
                passwordSet: true,
                listenerName: null,
                propertyFile: "/config/kafka-auth/client.properties",
            },
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "existing-kafka-msk-iam",
        description: "Amazon MSK with IAM auth, enabled through proxyConfig.enableMSKAuth. The region " +
            "comes from the aws-metadata ConfigMap.",
        spec: migrationSpec({
            kafka: {
                existing: {
                    kafkaConnection: "b-1.capture.abc123.c2.kafka.us-east-2.amazonaws.com:9098",
                    enableMSKAuth: true,
                },
            },
            proxyConfig: {listenPort: 9201, enableMSKAuth: true, tls: {mode: "plaintext"}},
        }),
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: null,
            capture: {
                brokers: "b-1.capture.abc123.c2.kafka.us-east-2.amazonaws.com:9098",
                topic: PROXY_NAME,
                clientId: "HttpCaptureProxyProducer",
                auth: "msk-iam",
                username: null,
                passwordSet: false,
                listenerName: null,
                propertyFile: null,
            },
            awsRegion: "us-east-2",
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
    {
        name: "suppression-set-header-and-tuning",
        description: "Every capture-suppression pattern, two forced headers in the name/value pair " +
            "form the proxy reads, trace export on, metric export off, and non-default tuning.",
        spec: migrationSpec({
            kafka: {existing: {kafkaConnection: "kafka.example.internal:9092"}},
            proxyConfig: {
                listenPort: 9201,
                tls: {mode: "plaintext"},
                setHeader: ["X-Capture-Proxy", "rust", "X-Forwarded-Proto", "https"],
                suppressCaptureForHeaderMatch: ["User-Agent", ".*healthcheck.*"],
                suppressCaptureForMethod: "HEAD",
                suppressCaptureForUriPath: "/_cat/.*",
                suppressMethodAndPath: "GET /_cluster/health.*",
                numThreads: 4,
                destinationConnectionPoolSize: 2,
                destinationConnectionPoolTimeout: "PT5M",
                maxTrafficBufferSize: 65536,
                kafkaClientId: "capture-proxy-contract",
                otelMetricsExportEnabled: false,
                otelTraceCollectorEndpoint: "http://otel-trace-collector:4317",
            },
        }),
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: null,
            capture: {
                brokers: "kafka.example.internal:9092",
                topic: PROXY_NAME,
                clientId: "capture-proxy-contract",
                auth: "none",
                username: null,
                passwordSet: false,
                listenerName: null,
                propertyFile: null,
            },
            suppression: {
                method: "HEAD",
                uriPath: "/_cat/.*",
                methodAndPath: "GET /_cluster/health.*",
                headers: [["User-Agent", ".*healthcheck.*"]],
            },
            setHeader: [["X-Capture-Proxy", "rust"], ["X-Forwarded-Proto", "https"]],
            telemetry: {metrics: null, traces: "http://otel-trace-collector:4317"},
            workerThreads: 4,
            maxTrafficBufferSize: 65536,
            connectionPool: {size: 2, recycleAfterSeconds: 300},
        },
    },
    {
        name: "no-capture",
        description: "noCapture forwards without capturing: no broker list is passed and no Kafka " +
            "client is configured, though the Kafka fields still are.",
        spec: migrationSpec({proxyConfig: {listenPort: 9201, noCapture: true}}),
        // The pod still references the managed cluster's SCRAM secret, which the workflow creates.
        secrets: {"default-migration-app": {password: "managed-scram-password"}},
        expect: {
            accepted: true,
            listenPort: 9201,
            destination: {uri: "https://source-cluster:9200", insecure: true},
            tls: DEFAULT_TLS,
            capture: "disabled",
            suppression: NO_SUPPRESSION,
            setHeader: [],
            telemetry: {metrics: "http://otel-collector:4317", traces: null},
            ...DEFAULT_TUNING,
        },
    },
];

const templates = (SetupCapture as any).templates as Record<string, any>;

function templateInputs(templateName: string, args: Record<string, string>): Record<string, string> {
    const template = templates[templateName];
    expect(template).toBeDefined();
    const values: Record<string, string> = {};
    for (const [name, def] of Object.entries(template.inputs as Record<string, any>)) {
        if (name in args) {
            values[name] = args[name];
        } else if (def._hasDefault && def.defaultValue.from !== undefined) {
            const key = def.defaultValue.from.key as string;
            expect(def.defaultValue.from.name).toBe("provider-config");
            expect(key in PROVIDER_CONFIG).toBe(true);
            values[name] = PROVIDER_CONFIG[key];
        } else if (def._hasDefault) {
            values[name] = evaluateArgument(def.defaultValue.expression, {inputs: {}});
        } else {
            throw new Error(`${templateName} requires input ${name}, which nothing supplies`);
        }
    }
    return values;
}

function stepsOf(templateName: string): any[] {
    return (templates[templateName].body.steps as any[]).flatMap(group => group.steps);
}

function stepRuns(step: any, scope: EvaluationScope): boolean {
    if (step.when === undefined) return true;
    const condition = "templateExp" in step.when ? step.when.templateExp : step.when;
    return evaluate(condition, scope) === true;
}

function stepArguments(step: any, scope: EvaluationScope): Record<string, string> {
    return Object.fromEntries(
        Object.entries(step.args as Record<string, unknown>).map(([k, v]) => [k, evaluateArgument(v, scope)])
    );
}

/**
 * What Strimzi reports for a workflow-managed cluster, which `readKafkaConnectionProfile` reads:
 * the first listener's bootstrap address and name, and its authentication type, which is absent
 * (so empty) on the plain listener.
 */
function strimziConnectionProfile(kafkaConfig: any) {
    return {
        resolvedKafkaConnection: kafkaConfig.kafkaConnection,
        resolvedKafkaListenerName: kafkaConfig.listenerName,
        resolvedKafkaAuthType: kafkaConfig.authType === "scram-sha-512" ? "scram-sha-512" : "",
    };
}

async function renderScenario(scenario: Scenario) {
    const lowered: any = await new MigrationConfigTransformer().processFromObject(scenario.spec);
    const proxy = lowered.proxies.find((p: any) => p.name === PROXY_NAME);
    expect(proxy).toBeDefined();

    // fullMigration's createProxy step: the per-proxy record handed to reconcileCaptureTopicAndProxy.
    const proxySetupConfig = {
        name: proxy.name,
        sourceConfig: proxy.sourceConfig,
        sourceConnectionIdentity: proxy.sourceConnectionIdentity,
        kafkaConfig: proxy.kafkaConfig,
        proxyConfig: proxy.proxyConfig,
        configChecksum: proxy.configChecksum ?? "",
        topicConfigChecksum: proxy.topicConfigChecksum ?? "",
        checksumForSnapshot: proxy.checksumForSnapshot ?? "",
        checksumForReplayer: proxy.checksumForReplayer ?? "",
        resourceUid: RESOURCE_UID,
    };

    // reconcileCaptureTopicAndProxy runs setupProxy with the Strimzi-resolved connection for a
    // workflow-managed cluster (`setupProxy`), and without it otherwise
    // (`setupProxyWithConfiguredKafka`).
    const managed = proxy.kafkaConfig.managedByWorkflow === true;
    const resolved = managed ? strimziConnectionProfile(proxy.kafkaConfig) : {};
    const setupProxyScope: EvaluationScope = {
        inputs: templateInputs("setupProxy", {
            ...IMAGE_INPUTS,
            ...resolved,
            proxyConfig: JSON.stringify(proxySetupConfig),
            kafkaClusterName: proxy.kafkaConfig.label,
            proxyName: PROXY_NAME,
            ownerUid: RESOURCE_UID,
            listenPort: String(proxy.proxyConfig.listenPort),
            skipApproval: "true",
            podReplicas: String(proxy.proxyConfig.podReplicas),
            minPodReplicas: String(proxy.proxyConfig.minPodReplicas),
            sourceK8sLabel: proxy.sourceConfig.label,
            configChecksum: proxySetupConfig.configChecksum,
            checksumForSnapshot: proxySetupConfig.checksumForSnapshot,
            checksumForReplayer: proxySetupConfig.checksumForReplayer,
        }),
        workflowValues: WORKFLOW_VALUES,
    };

    const deploySteps = stepsOf("setupProxy")
        .filter(s => s.name === "deployProxyNoTls" || s.name === "deployProxyWithTls")
        .filter(s => stepRuns(s, setupProxyScope));
    expect(deploySteps).toHaveLength(1);
    const deployStep = deploySteps[0];
    const deployScope: EvaluationScope = {
        inputs: templateInputs(deployStep.template, stepArguments(deployStep, setupProxyScope)),
        workflowValues: WORKFLOW_VALUES,
    };
    const deployment: any = evaluateManifest(templates[deployStep.template].body.resource.manifest, deployScope);

    const configMapStep = stepsOf("setupProxy").find(s => s.name === "createKafkaClientConfig");
    const configMapScope: EvaluationScope = {
        inputs: templateInputs(configMapStep.template, stepArguments(configMapStep, setupProxyScope)),
    };
    const configMap: any = evaluateManifest(
        templates[configMapStep.template].body.resource.manifest, configMapScope);

    const podSpec = deployment.spec.template.spec;
    expect(podSpec.containers).toHaveLength(1);
    const container = podSpec.containers[0];
    return {
        comment: "Generated by orchestrationSpecs/packages/migration-workflow-templates/tests/" +
            "captureProxyRustConfigContract.test.ts from the real config processor and setupCapture " +
            "templates. Do not edit; regenerate with `npx jest tests/captureProxyRustConfigContract.test.ts -u`.",
        scenario: scenario.name,
        description: scenario.description,
        migrationSpecProxy: scenario.spec.traffic.proxies[PROXY_NAME],
        migrationSpecKafka: scenario.spec.kafkaClusterConfiguration.default,
        kafkaConnectionSource: managed ? "strimzi-status" : "migration-spec",
        deployTemplate: deployStep.template,
        decodedInlineJson: JSON.parse(deployScope.inputs.jsonConfig),
        pod: {
            terminationGracePeriodSeconds: podSpec.terminationGracePeriodSeconds,
            container: {
                name: container.name,
                args: container.args,
                env: container.env,
                volumeMounts: container.volumeMounts,
            },
            volumes: podSpec.volumes,
        },
        cluster: {
            configMaps: {
                [configMap.metadata.name]: configMap.data,
                "aws-metadata": {AWS_REGION: "us-east-2"},
            },
            secrets: scenario.secrets ?? {},
        },
        expect: scenario.expect,
    };
}

function isUpdatingSnapshots(): boolean {
    return (expect.getState() as any).snapshotState?._updateSnapshot === "all";
}

describe("capture-proxy configuration rendered for the Rust proxy", () => {
    it.each(SCENARIOS.map(s => [s.name, s] as const))("%s matches its committed fixture", async (name, scenario) => {
        const rendered = JSON.stringify(await renderScenario(scenario), null, 2) + "\n";
        const file = path.join(FIXTURE_DIR, `${name}.json`);
        if (isUpdatingSnapshots()) {
            fs.mkdirSync(FIXTURE_DIR, {recursive: true});
            fs.writeFileSync(file, rendered);
            return;
        }
        expect(fs.existsSync(file)).toBe(true);
        expect(fs.readFileSync(file, "utf-8")).toBe(rendered);
    });

    it("rejects setHeader written as 'Name: value' strings at spec validation, before any proxy starts", async () => {
        // Both proxies read setHeader as alternating names and values. The schema used to document
        // one "Name: value" string per header, which the proxy then refused at startup (exit 2); the
        // schema now rejects that form itself, with a message that says how to write it.
        const spec = migrationSpec({
            kafka: {existing: {kafkaConnection: "kafka.example.internal:9092"}},
            proxyConfig: {listenPort: 9201, tls: {mode: "plaintext"}, setHeader: ["X-Capture-Proxy: rust"]},
        });
        await expect(new MigrationConfigTransformer().processFromObject(spec)).rejects.toThrow(/not a header name/);
    });

    it("has no committed fixture without a scenario", () => {
        const expected = new Set(SCENARIOS.map(s => `${s.name}.json`));
        const committed = fs.existsSync(FIXTURE_DIR) ? fs.readdirSync(FIXTURE_DIR) : [];
        if (isUpdatingSnapshots()) {
            committed.filter(f => !expected.has(f)).forEach(f => fs.unlinkSync(path.join(FIXTURE_DIR, f)));
            return;
        }
        expect(committed.filter(f => !expected.has(f))).toEqual([]);
        expect([...expected].filter(f => !committed.includes(f))).toEqual([]);
    });

    it("evaluates the same steps and manifests the rendered WorkflowTemplate deploys", () => {
        // The evaluator walks the template model; this ties that model to the rendered template, so
        // a fixture cannot be produced from steps or manifests that are not the ones deployed.
        const rendered = renderWorkflowTemplate(SetupCapture) as any;
        const byName = (name: string) => rendered.spec.templates.find((t: any) => t.name === name.toLowerCase());
        const renderedSteps = byName("setupProxy").steps.flat();
        const deploySteps = stepsOf("setupProxy").filter(s => s.name.startsWith("deployProxy") && s.args.jsonConfig);
        expect(deploySteps.map(s => s.name).sort()).toEqual(["deployProxyNoTls", "deployProxyWithTls"]);
        for (const step of deploySteps) {
            const renderedStep = renderedSteps.find((s: any) => s.name === step.name);
            expect(renderedStep.template).toBe(step.template.toLowerCase());
            const renderedJsonConfig = renderedStep.arguments.parameters.find((p: any) => p.name === "jsonConfig");
            expect(renderedJsonConfig.value).toBe(toArgoExpressionString(step.args.jsonConfig));
            expect(byName(step.template).resource.manifest).toBe(unwrapPlaceholdersAndStringify(
                transformExpressionsDeep(templates[step.template].body.resource.manifest, true)));
        }
    });
});
