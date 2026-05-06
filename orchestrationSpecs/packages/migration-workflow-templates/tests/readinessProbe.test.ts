import * as yaml from "js-yaml";
import {renderWorkflowTemplate} from "@opensearch-migrations/argo-workflow-builders";
import {SetupCapture} from "../src/workflowTemplates/setupCapture";
import {Replayer} from "../src/workflowTemplates/replayer";

/** Ensures proxy and replayer Deployments have a readinessProbe that gates on real init. */
describe("Proxy and replayer Deployments declare a readinessProbe", () => {
    const setupCapture = renderWorkflowTemplate(SetupCapture) as any;
    const replayer = renderWorkflowTemplate(Replayer) as any;

    function getResourceManifest(rendered: any, templateName: string): any {
        const templates: any[] = rendered.spec?.templates ?? [];
        const t = templates.find(x => x.name === templateName);
        expect(t).toBeDefined();
        expect(t.resource?.manifest).toBeDefined();
        return yaml.load(t.resource.manifest as string);
    }

    function getFirstContainer(deployment: any): any {
        expect(deployment.kind).toBe("Deployment");
        const containers = deployment.spec?.template?.spec?.containers;
        expect(Array.isArray(containers)).toBe(true);
        expect(containers.length).toBeGreaterThanOrEqual(1);
        return containers[0];
    }

    function assertMinReadySecondsIsSet(deployment: any) {
        const mrs = deployment.spec?.minReadySeconds;
        expect(typeof mrs).toBe("number");
        expect(mrs).toBeGreaterThanOrEqual(1);
    }

    it("deployProxyDeployment container has a tcpSocket readinessProbe on listenPort with minReadySeconds", () => {
        const deployment = getResourceManifest(setupCapture, "deployproxydeployment");
        const container = getFirstContainer(deployment);
        const probe = container.readinessProbe;
        expect(probe).toBeDefined();
        expect(probe.tcpSocket).toBeDefined();
        expect(probe.tcpSocket.port).toBeDefined();
        const ports = container.ports;
        expect(Array.isArray(ports)).toBe(true);
        expect(ports.length).toBeGreaterThanOrEqual(1);
        expect(probe.tcpSocket.port).toEqual(ports[0].containerPort);
        assertMinReadySecondsIsSet(deployment);
    });

    it("deployProxyDeploymentWithTls container has a tcpSocket readinessProbe on listenPort with minReadySeconds", () => {
        const deployment = getResourceManifest(setupCapture, "deployproxydeploymentwithtls");
        const container = getFirstContainer(deployment);
        const probe = container.readinessProbe;
        expect(probe).toBeDefined();
        expect(probe.tcpSocket).toBeDefined();
        const ports = container.ports;
        expect(probe.tcpSocket.port).toEqual(ports[0].containerPort);
        assertMinReadySecondsIsSet(deployment);
    });

    it("replayer Deployment has an exec readinessProbe on /tmp/replayer-ready with minReadySeconds", () => {
        const templates: any[] = replayer.spec?.templates ?? [];
        const deploymentTemplates = templates.filter(t => {
            const m: string | undefined = t.resource?.manifest;
            return typeof m === "string" && m.includes("kind: Deployment");
        });
        expect(deploymentTemplates.length).toBeGreaterThanOrEqual(1);
        for (const t of deploymentTemplates) {
            const deployment: any = yaml.load(t.resource.manifest);
            const container = getFirstContainer(deployment);
            const probe = container.readinessProbe;
            expect(probe).toBeDefined();
            expect(probe.exec).toBeDefined();
            expect(Array.isArray(probe.exec.command)).toBe(true);
            expect(probe.exec.command).toEqual(expect.arrayContaining(["/tmp/replayer-ready"]));
            assertMinReadySecondsIsSet(deployment);
        }
    });
});
