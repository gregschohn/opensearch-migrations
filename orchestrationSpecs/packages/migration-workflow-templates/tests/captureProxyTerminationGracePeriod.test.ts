import * as yaml from "js-yaml";
import {renderWorkflowTemplate} from "@opensearch-migrations/argo-workflow-builders";
import {
    CAPTURE_PROXY_TERMINATION_GRACE_PERIOD_SECONDS,
    SetupCapture
} from "../src/workflowTemplates/setupCapture";

/**
 * Owner decision OD-15: the capture proxy's orderly shutdown stops accepting at 60 s, drains to
 * 240/270 s and hard-stops at 300 s. Without an explicit grace period Kubernetes SIGKILLs the pod
 * 30 s after SIGTERM, which pre-empts all of it. The pod must grant the 300 s budget plus a margin.
 */
describe("Capture proxy pods grant the full orderly-shutdown budget (OD-15)", () => {
    const PROXY_HARD_STOP_SECONDS = 300;
    const setupCapture = renderWorkflowTemplate(SetupCapture) as any;

    function getDeployment(templateName: string): any {
        const templates: any[] = setupCapture.spec?.templates ?? [];
        const t = templates.find(x => x.name === templateName);
        expect(t?.resource?.manifest).toBeDefined();
        // The dynamic list/map fields are Argo expressions that are not YAML; blank them so the
        // static structure parses.
        const manifest = (t.resource.manifest as string).replace(
            /^(\s*)(nodeSelector|tolerations|topologySpreadConstraints|volumeMounts|volumes): \{\{=.*$/gm,
            "$1$2: []",
        );
        const deployment = yaml.load(manifest) as any;
        expect(deployment.kind).toBe("Deployment");
        return deployment;
    }

    it("grants 330 s, the 300 s hard stop plus a 30 s margin", () => {
        expect(CAPTURE_PROXY_TERMINATION_GRACE_PERIOD_SECONDS).toBe(330);
        expect(CAPTURE_PROXY_TERMINATION_GRACE_PERIOD_SECONDS).toBeGreaterThan(PROXY_HARD_STOP_SECONDS);
    });

    it.each(["deployproxydeployment", "deployproxydeploymentwithtls"])(
        "%s sets terminationGracePeriodSeconds on the pod template",
        templateName => {
            const podSpec = getDeployment(templateName).spec?.template?.spec;
            expect(podSpec?.terminationGracePeriodSeconds).toBe(330);
        }
    );
});
