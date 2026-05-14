import {z} from "zod";
import {
    CLUSTER_VERSION_STRING,
    COMPLETE_SNAPSHOT_CONFIG,
    CONSOLE_SERVICES_CONFIG_FILE,
    DEFAULT_RESOURCES,
    NAMED_TARGET_CLUSTER_CONFIG,
    ResourceRequirementsType,
    ARGO_RFS_OPTIONS,
    ARGO_RFS_WORKFLOW_OPTION_KEYS,
} from "@opensearch-migrations/schemas";
import {MigrationConsole} from "./migrationConsole";
import {CONTAINER_NAMES} from "../containerNames";

import {
    AllowLiteralOrExpression,
    BaseExpression,
    defineParam,
    defineRequiredParam,
    expr,
    IMAGE_PULL_POLICY,
    INTERNAL, makeDirectTypeProxy, makeStringTypeProxy,
    selectInputsForRegister,
    Serialized,
    typeToken,
    WorkflowBuilder
} from "@opensearch-migrations/argo-workflow-builders";
import {Deployment} from "@opensearch-migrations/argo-workflow-builders";
import {OwnerReference} from "@opensearch-migrations/k8s-types";
import {makeRepoParamDict} from "./metadataMigration";
import {
    setupLog4jConfigForContainer,
    setupTestCredsForContainer,
    setupTransformsForContainerForMode,
    TransformVolumeMode
} from "./commonUtils/containerFragments";
import {CommonWorkflowParameters} from "./commonUtils/workflowParameters";
import {makeRequiredImageParametersForKeys} from "./commonUtils/imageDefinitions";
import {makeTargetParamDict, makeRfsCoordinatorParamDict} from "./commonUtils/clusterSettingManipulators";
import {getHttpAuthSecretName} from "./commonUtils/clusterSettingManipulators";
import {getTargetHttpAuthCredsEnvVars, getCoordinatorHttpAuthCredsEnvVars} from "./commonUtils/basicCredsGetters";
import {K8S_RESOURCE_RETRY_STRATEGY} from "./commonUtils/resourceRetryStrategy";
import {RfsCoordinatorCluster, getRfsCoordinatorClusterName, makeRfsCoordinatorConfig} from "./rfsCoordinatorCluster";
import {ResourceManagement} from "./resourceManagement";

function shouldCreateRfsWorkCoordinationCluster(
    documentBackfillConfig: BaseExpression<Serialized<z.infer<typeof ARGO_RFS_OPTIONS>>>
): BaseExpression<boolean, "complicatedExpression"> {
    return expr.not(
        expr.get(
            expr.deserializeRecord(documentBackfillConfig),
            "useTargetClusterForWorkCoordination"
        )
    );
}

function makeParamsDict(
    sourceVersion: BaseExpression<z.infer<typeof CLUSTER_VERSION_STRING>>,
    targetConfig: BaseExpression<Serialized<z.infer<typeof NAMED_TARGET_CLUSTER_CONFIG>>>,
    rfsCoordinatorConfig: BaseExpression<Serialized<z.infer<typeof NAMED_TARGET_CLUSTER_CONFIG>>>,
    snapshotConfig: BaseExpression<Serialized<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>>,
    options: BaseExpression<Serialized<z.infer<typeof ARGO_RFS_OPTIONS>>>,
    sessionName: BaseExpression<string>
) {
    return expr.mergeDicts(
        expr.mergeDicts(
            expr.mergeDicts(
                makeTargetParamDict(targetConfig),
                makeRfsCoordinatorParamDict(rfsCoordinatorConfig)
            ),
            expr.omit(expr.deserializeRecord(options), ...ARGO_RFS_WORKFLOW_OPTION_KEYS)
        ),
        expr.mergeDicts(
            expr.makeDict({
                snapshotName: expr.get(expr.deserializeRecord(snapshotConfig), "snapshotName"),
                sourceVersion: sourceVersion,
                sessionName: sessionName,
                luceneDir: "/tmp",
                cleanLocalDirs: true
            }),
            makeRepoParamDict(
                expr.omit(expr.get(expr.deserializeRecord(snapshotConfig), "repoConfig"), "s3RoleArn"),
                true)
        )
    );
}

function getRfsDeploymentName(sessionName: BaseExpression<string>) {
    return expr.concat(sessionName, expr.literal("-rfs"));
}

function getRfsDoneCronJobName(sessionName: BaseExpression<string>) {
    return expr.concat(sessionName, expr.literal("-rfs-done"));
}

function getRfsStatusConfigMapName(sessionName: BaseExpression<string>) {
    return expr.concat(sessionName, expr.literal("-rfs-status"));
}

// Label keys for the RFS completion CronJob.
//   workflow-uid  — per-claim, rewritten on every workflow apply, used for INV-4 supersession.
//   session       — stable for the lifetime of the SnapshotMigration; used as the drain
//                   selector during INV-8's apply step.
//   cadence-step  — mirrors the */N value in spec.schedule. Owned solely by the CronJob's
//                   own script; the workflow apply does not touch it.
const RFS_MONITOR_WORKFLOW_UID_LABEL = "migrations.opensearch.org/rfs-monitor-workflow-uid";
const RFS_MONITOR_SESSION_LABEL = "migrations.opensearch.org/rfs-monitor-session";
const RFS_MONITOR_CADENCE_LABEL = "migrations.opensearch.org/rfs-monitor-cadence-step";

// Constant bash script run by every CronJob Job pod. All inputs come from
// env vars wired up on the CronJob's jobTemplate by the apply step:
//   WORKFLOW_UID, PARENT_WORKFLOW_NAME, PARENT_WORKFLOW_UID, CLAIMED_AT,
//   SNAPSHOT_MIGRATION_NAME, CONFIG_CHECKSUM, CHECKSUM_FOR_REPLAYER,
//   RFS_DEPLOYMENT_NAME, RFS_COORDINATOR_NAME, USES_DEDICATED_COORDINATOR,
//   RFS_CRONJOB_NAME, RFS_STATUS_CONFIGMAP_NAME, STARTUP_GRACE_SECONDS,
//   CONSOLE_CONFIG_BASE64.
//
// Phases (mutually exclusive per the design's decision tree):
//   0. Supersession (INV-4): GET cronjob; exit 0 if labels.workflow-uid !=
//      env.WORKFLOW_UID, or if cronjob is NotFound.
//   1. Recovery cleanup (INV-6 b): if SM.status reflects env.CONFIG_CHECKSUM,
//      redrive the idempotent runtime delete and the preconditioned cronjob delete.
//   2. Orphan / startup window (INV-6 c): if SM is missing OR runtime is missing,
//      gate on parent_workflow_running and CLAIMED_AT, then either wait or
//      self-delete (INV-5 preconditions).
//   3. Status poll + commit + cleanup (INV-6 a+b): run the deep-check; if
//      Completed, PATCH SM.status, then idempotent-delete runtime, then
//      preconditioned-delete the cronjob in the same execution.
//
// At every non-terminal exit, maybe_bump_cadence advances spec.schedule along
// the */1 -> */5 ladder via a resourceVersion-preconditioned PATCH.
const RFS_DONE_CRONJOB_SCRIPT = `set -eu

RFS_MONITOR_CADENCE_LABEL="${RFS_MONITOR_CADENCE_LABEL}"
RFS_MONITOR_WORKFLOW_UID_LABEL="${RFS_MONITOR_WORKFLOW_UID_LABEL}"
K8S_API_SERVER="https://\${KUBERNETES_SERVICE_HOST}:\${KUBERNETES_SERVICE_PORT_HTTPS:-443}"
K8S_SA_TOKEN_FILE="/var/run/secrets/kubernetes.io/serviceaccount/token"
K8S_SA_CA_FILE="/var/run/secrets/kubernetes.io/serviceaccount/ca.crt"
K8S_SA_TOKEN="$(cat "$K8S_SA_TOKEN_FILE")"

# Materialize the console services config produced by the workflow's
# getConsoleConfig step so phase 3 can run "console backfill status".
mkdir -p /config
echo "$CONSOLE_CONFIG_BASE64" | base64 -d > /config/migration_services.yaml_
jq -f workflowConfigToServicesConfig.jq < /config/migration_services.yaml_ > /config/migration_services.yaml
export MIGRATION_USE_SERVICES_YAML_CONFIG=true

k8s_api() {
    method="$1"
    path="$2"
    content_type="$3"
    body="\${4:-}"
    if [ -n "$body" ]; then
        curl -fsS \
            --cacert "$K8S_SA_CA_FILE" \
            -H "Authorization: Bearer $K8S_SA_TOKEN" \
            -H "Content-Type: $content_type" \
            -X "$method" \
            "$K8S_API_SERVER$path" \
            --data "$body"
    else
        curl -fsS \
            --cacert "$K8S_SA_CA_FILE" \
            -H "Authorization: Bearer $K8S_SA_TOKEN" \
            -H "Content-Type: $content_type" \
            -X "$method" \
            "$K8S_API_SERVER$path"
    fi
}

# 0. SUPERSESSION CHECK (INV-4) ----------------------------------------------
cronjob_json="$(kubectl get cronjob "$RFS_CRONJOB_NAME" -o json 2>/dev/null || true)"
if [ -z "$cronjob_json" ]; then
    echo "cronjob $RFS_CRONJOB_NAME not found; nothing to do"
    exit 0
fi
cj_workflow_uid="$(echo "$cronjob_json" | jq -r --arg k "$RFS_MONITOR_WORKFLOW_UID_LABEL" '.metadata.labels[$k] // ""')"
if [ "$cj_workflow_uid" != "$WORKFLOW_UID" ]; then
    echo "superseded: cronjob workflow-uid=$cj_workflow_uid env.WORKFLOW_UID=$WORKFLOW_UID"
    exit 0
fi
cj_uid="$(echo "$cronjob_json" | jq -r '.metadata.uid')"
cj_rv="$(echo "$cronjob_json" | jq -r '.metadata.resourceVersion')"
cj_namespace="$(echo "$cronjob_json" | jq -r '.metadata.namespace')"

sm_json="$(kubectl get snapshotmigration "$SNAPSHOT_MIGRATION_NAME" -o json 2>/dev/null || true)"

# delete_cronjob_with_preconditions: INV-5 atomic self-delete. kubectl
# delete does not expose --uid / --resource-version; use the raw API.
delete_cronjob_with_preconditions() {
    body="$(jq -nc --arg uid "$cj_uid" --arg rv "$cj_rv" \\
        '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid,resourceVersion:$rv}}')"
    if k8s_api DELETE "/apis/batch/v1/namespaces/$cj_namespace/cronjobs/$RFS_CRONJOB_NAME" \
        "application/json" "$body" >/dev/null 2>&1; then
        return 0
    fi
    echo "preconditioned cronjob delete returned non-zero (likely 409: superseded or RV advanced)"
    return 1
}

delete_runtime_resources() {
    kubectl delete deployment "$RFS_DEPLOYMENT_NAME" --ignore-not-found
    if [ "$USES_DEDICATED_COORDINATOR" = "true" ]; then
        kubectl delete statefulset "$RFS_COORDINATOR_NAME" --ignore-not-found
        kubectl delete service     "$RFS_COORDINATOR_NAME" --ignore-not-found
        kubectl delete secret      "$RFS_COORDINATOR_NAME-creds" --ignore-not-found
    fi
}

write_status_configmap() {
    summary="$1"
    status_json_text="$2"
    now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    configmap_yaml="$(kubectl create configmap "$RFS_STATUS_CONFIGMAP_NAME" \\
        --from-literal=summary="$summary" \\
        --from-literal=status.json="$status_json_text" \\
        --from-literal=updatedAt="$now" \\
        --dry-run=client -o yaml)"
    printf '%s\n' "$configmap_yaml" | kubectl apply -f -
}

# Cadence ramp 1 -> 5. Owned solely by this script; atomic PATCH with
# resourceVersion precondition.
maybe_bump_cadence() {
    raw_step="$(echo "$cronjob_json" | jq -r --arg k "$RFS_MONITOR_CADENCE_LABEL" '.metadata.labels[$k] // "1"')"
    case "$raw_step" in
        1|2|3|4|5) current="$raw_step" ;;
        *)         current=1 ;;
    esac
    if [ "$current" -ge 5 ]; then return 0; fi
    desired=$((current + 1))
    body="$(jq -nc \\
        --arg label   "$RFS_MONITOR_CADENCE_LABEL" \\
        --arg desired "$desired" \\
        --arg sched   "*/$desired * * * *" \\
        --arg rv      "$cj_rv" \\
        '{metadata:{resourceVersion:$rv,labels:{($label):$desired}},spec:{schedule:$sched}}')"
    if k8s_api PATCH "/apis/batch/v1/namespaces/$cj_namespace/cronjobs/$RFS_CRONJOB_NAME" \
        "application/merge-patch+json" "$body" >/dev/null 2>&1; then
        echo "cadence: $current -> $desired"
    else
        echo "cadence patch failed (resourceVersion mismatch: concurrent reclaim); ignoring"
    fi
}

parent_workflow_running() {
    phase="$(kubectl get workflow "$PARENT_WORKFLOW_NAME" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    [ "$phase" = "Running" ] || [ "$phase" = "Pending" ]
}

runtime_resource_exists() {
    kubectl get deployment "$RFS_DEPLOYMENT_NAME" >/dev/null 2>&1
}

# 1. RECOVERY CLEANUP (INV-6 b) ----------------------------------------------
sm_phase="$(echo "$sm_json" | jq -r '.status.phase // ""' 2>/dev/null || echo "")"
sm_status_cc="$(echo "$sm_json" | jq -r '.status.configChecksum // ""' 2>/dev/null || echo "")"
if [ -n "$sm_json" ] && [ "$sm_phase" = "Completed" ] && [ "$sm_status_cc" = "$CONFIG_CHECKSUM" ]; then
    echo "phase 1: recovery cleanup (status already reflects env.CONFIG_CHECKSUM)"
    delete_runtime_resources
    delete_cronjob_with_preconditions || true
    exit 0
fi

# 2. ORPHAN / STARTUP-WINDOW (INV-6 c) ---------------------------------------
if [ -z "$sm_json" ] || ! runtime_resource_exists; then
    if parent_workflow_running; then
        echo "phase 2: parent workflow still running; waiting"
        maybe_bump_cadence
        exit 0
    fi
    elapsed=$(( $(date -u +%s) - CLAIMED_AT ))
    if [ "$elapsed" -le "$STARTUP_GRACE_SECONDS" ]; then
        echo "phase 2: within startup grace ($elapsed of $STARTUP_GRACE_SECONDS s)"
        maybe_bump_cadence
        exit 0
    fi
    echo "phase 2: orphaned (sm or runtime missing, parent gone, grace expired); self-deleting"
    delete_cronjob_with_preconditions || true
    exit 0
fi

# 3. STATUS POLL + COMMIT (+ CLEANUP) (INV-6 a+b) ----------------------------
status_json="$(console --config-file=/config/migration_services.yaml --json backfill status --deep-check 2>/dev/null || true)"
status="$(echo "$status_json" | jq -r '.status // ""' 2>/dev/null || echo "")"

if [ -z "$status_json" ] || [ -z "$status" ]; then
    write_status_configmap "Status check is not available yet" "\${status_json:-}"
    maybe_bump_cadence
    exit 0
fi

summary="$(echo "$status_json" | jq -r '
    if .status == "Pending" then
        "Shards are initializing"
    else
        "complete: \\(.percentage_completed // 0)%, shards: \\(.shard_complete // 0)/\\(.shard_total // 0)"
    end
')"
write_status_configmap "$summary" "$status_json"

if [ "$status" != "Completed" ]; then
    echo "phase 3: still working ($summary)"
    maybe_bump_cadence
    exit 0
fi

# Backfill is done. Lock-on-Complete VAP freezes spec the instant this
# PATCH returns, so there is no spec-change window between commit and the
# cleanup below. If the status PATCH fails, this execution exits non-zero and
# leaves the runtime + CronJob in place so a future run can retry safely.
echo "phase 3: backfill complete; committing and cleaning up"
patch_body="$(jq -nc \\
    --arg cc "$CONFIG_CHECKSUM" \\
    --arg cr "$CHECKSUM_FOR_REPLAYER" \\
    '{status:{phase:"Completed",configChecksum:$cc,checksumForReplayer:$cr}}')"
kubectl patch snapshotmigration "$SNAPSHOT_MIGRATION_NAME" \\
    --subresource=status --type=merge -p "$patch_body"

delete_runtime_resources
delete_cronjob_with_preconditions || true
exit 0
`;

// The base64 of the CronJob's Job-pod script, computed at TS build time
// because expr.literal rejects strings containing "}}" (which the script
// uses inside jq filters), and shipped as a TS-time string literal so the
// rendered YAML simply contains the encoded bytes without an Argo template
// expression in the middle of a shell variable assignment.
const RFS_DONE_CRONJOB_SCRIPT_B64 = Buffer.from(RFS_DONE_CRONJOB_SCRIPT, "utf8").toString("base64");

// Apply step (INV-8). The whole bash runs inside an Argo container template,
// renders the CronJob YAML inline, and:
//   - try-create with `kubectl create -f -` (201 ⇒ no drain).
//   - on "AlreadyExists", PATCH only the workflow-uid label and jobTemplate
//     (never spec.schedule or cadence-step), then force-delete prior-claim
//     Jobs/Pods using the drain selector.
//   - apply the status ConfigMap idempotently.
//   - read-back loop until labels and env converge.
//
// Template placeholders are filled at workflow build time by expr.fillTemplate.
// Runtime values like resourceVersion are looked up inline by the script.
const APPLY_RFS_DONE_CRONJOB_SCRIPT = `set -eu

CRONJOB_NAME="{{CRONJOB_NAME}}"
STATUS_CONFIGMAP_NAME="{{STATUS_CONFIGMAP_NAME}}"
SESSION_NAME="{{SESSION_NAME}}"
WORKFLOW_NAME="{{WORKFLOW_NAME}}"
WORKFLOW_UID="{{WORKFLOW_UID}}"
PARENT_WORKFLOW_NAME="{{PARENT_WORKFLOW_NAME}}"
PARENT_WORKFLOW_UID="{{PARENT_WORKFLOW_UID}}"
SNAPSHOT_MIGRATION_NAME="{{SNAPSHOT_MIGRATION_NAME}}"
SM_UID="{{SM_UID}}"
CONFIG_CHECKSUM="{{CONFIG_CHECKSUM}}"
CHECKSUM_FOR_REPLAYER="{{CHECKSUM_FOR_REPLAYER}}"
RFS_DEPLOYMENT_NAME="{{RFS_DEPLOYMENT_NAME}}"
RFS_COORDINATOR_NAME="{{RFS_COORDINATOR_NAME}}"
USES_DEDICATED_COORDINATOR="{{USES_DEDICATED_COORDINATOR}}"
CONSOLE_IMAGE="{{CONSOLE_IMAGE}}"
CONSOLE_IMAGE_PULL_POLICY="{{CONSOLE_IMAGE_PULL_POLICY}}"
FROM_SNAPSHOT_MIGRATION_LABEL="{{FROM_SNAPSHOT_MIGRATION_LABEL}}"
CONSOLE_CONFIG_BASE64="{{CONSOLE_CONFIG_BASE64}}"
STARTUP_GRACE_SECONDS="600"
CLAIMED_AT="$(date -u +%s)"

WORKFLOW_UID_LABEL="${RFS_MONITOR_WORKFLOW_UID_LABEL}"
SESSION_LABEL="${RFS_MONITOR_SESSION_LABEL}"

# The CronJob script (constant) is shipped here base64-encoded so it can
# be inlined into the YAML heredoc below without escape hazards.
RFS_DONE_SCRIPT_B64="${RFS_DONE_CRONJOB_SCRIPT_B64}"

render_cronjob_yaml() {
    cat <<YAML
apiVersion: batch/v1
kind: CronJob
metadata:
  name: \${CRONJOB_NAME}
  ownerReferences:
    - apiVersion: migrations.opensearch.org/v1alpha1
      kind: SnapshotMigration
      name: \${SNAPSHOT_MIGRATION_NAME}
      uid: \${SM_UID}
      controller: false
      blockOwnerDeletion: true
  labels:
    \${WORKFLOW_UID_LABEL}: "\${WORKFLOW_UID}"
    \${SESSION_LABEL}: "\${SESSION_NAME}"
    workflows.argoproj.io/workflow: "\${WORKFLOW_NAME}"
    migrations.opensearch.org/from-snapshot-migration: "\${FROM_SNAPSHOT_MIGRATION_LABEL}"
spec:
  schedule: "*/1 * * * *"
  concurrencyPolicy: Forbid
  successfulJobsHistoryLimit: 1
  failedJobsHistoryLimit: 3
  jobTemplate:
    metadata:
      labels:
        \${WORKFLOW_UID_LABEL}: "\${WORKFLOW_UID}"
        \${SESSION_LABEL}: "\${SESSION_NAME}"
        workflows.argoproj.io/workflow: "\${WORKFLOW_NAME}"
        migrations.opensearch.org/from-snapshot-migration: "\${FROM_SNAPSHOT_MIGRATION_LABEL}"
    spec:
      template:
        metadata:
          labels:
            \${WORKFLOW_UID_LABEL}: "\${WORKFLOW_UID}"
            \${SESSION_LABEL}: "\${SESSION_NAME}"
            workflows.argoproj.io/workflow: "\${WORKFLOW_NAME}"
            migrations.opensearch.org/from-snapshot-migration: "\${FROM_SNAPSHOT_MIGRATION_LABEL}"
        spec:
          serviceAccountName: argo-workflow-executor
          restartPolicy: Never
          containers:
            - name: rfs-monitor
              image: "\${CONSOLE_IMAGE}"
              imagePullPolicy: "\${CONSOLE_IMAGE_PULL_POLICY}"
              command: ["/bin/sh", "-c"]
              args:
                - |
$(echo "$RFS_DONE_SCRIPT_B64" | base64 -d | sed 's/^/                  /')
              env:
                - {name: WORKFLOW_UID,               value: "\${WORKFLOW_UID}"}
                - {name: PARENT_WORKFLOW_NAME,       value: "\${PARENT_WORKFLOW_NAME}"}
                - {name: PARENT_WORKFLOW_UID,        value: "\${PARENT_WORKFLOW_UID}"}
                - {name: CLAIMED_AT,                 value: "\${CLAIMED_AT}"}
                - {name: SNAPSHOT_MIGRATION_NAME,    value: "\${SNAPSHOT_MIGRATION_NAME}"}
                - {name: CONFIG_CHECKSUM,            value: "\${CONFIG_CHECKSUM}"}
                - {name: CHECKSUM_FOR_REPLAYER,      value: "\${CHECKSUM_FOR_REPLAYER}"}
                - {name: RFS_DEPLOYMENT_NAME,        value: "\${RFS_DEPLOYMENT_NAME}"}
                - {name: RFS_COORDINATOR_NAME,       value: "\${RFS_COORDINATOR_NAME}"}
                - {name: USES_DEDICATED_COORDINATOR, value: "\${USES_DEDICATED_COORDINATOR}"}
                - {name: RFS_CRONJOB_NAME,           value: "\${CRONJOB_NAME}"}
                - {name: RFS_STATUS_CONFIGMAP_NAME,  value: "\${STATUS_CONFIGMAP_NAME}"}
                - {name: STARTUP_GRACE_SECONDS,      value: "\${STARTUP_GRACE_SECONDS}"}
                - {name: CONSOLE_CONFIG_BASE64,      value: "\${CONSOLE_CONFIG_BASE64}"}
YAML
}

render_status_configmap_yaml() {
    cat <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: \${STATUS_CONFIGMAP_NAME}
  ownerReferences:
    - apiVersion: migrations.opensearch.org/v1alpha1
      kind: SnapshotMigration
      name: \${SNAPSHOT_MIGRATION_NAME}
      uid: \${SM_UID}
      controller: false
      blockOwnerDeletion: true
  labels:
    migrations.opensearch.org/from-snapshot-migration: "\${FROM_SNAPSHOT_MIGRATION_LABEL}"
data:
  summary: ""
  status.json: ""
  updatedAt: ""
YAML
}

# Try-create. On 201, skip drain. On 409 (AlreadyExists), patch only the
# workflow-uid label and jobTemplate (never spec.schedule or cadence-step,
# both owned by the script), then drain prior-claim Jobs/Pods.
echo "applying CronJob $CRONJOB_NAME"
create_out="$(render_cronjob_yaml | kubectl create -f - 2>&1 || true)"
if echo "$create_out" | grep -q "created"; then
    echo "$create_out"
    echo "fresh create — no drain needed"
else
    if ! echo "$create_out" | grep -qi "AlreadyExists"; then
        echo "create failed unexpectedly: $create_out" >&2
        exit 1
    fi
    echo "exists — patching jobTemplate and draining prior claim"

    patch_payload="$(render_cronjob_yaml \\
        | kubectl create --dry-run=client -f - -o json \\
        | jq -c --arg uidkey "$WORKFLOW_UID_LABEL" \\
            '{
                metadata:{labels:{($uidkey):.metadata.labels[$uidkey]}},
                spec:{jobTemplate:.spec.jobTemplate}
            }')"
    kubectl patch cronjob "$CRONJOB_NAME" --type=merge -p "$patch_payload"

    OLD_FILTER="$SESSION_LABEL=$SESSION_NAME,$WORKFLOW_UID_LABEL!=$WORKFLOW_UID"
    kubectl delete jobs -l "$OLD_FILTER" --force --grace-period=0 --ignore-not-found || true
    kubectl delete pods -l "$OLD_FILTER" --force --grace-period=0 --ignore-not-found || true
fi

# Apply the status ConfigMap idempotently.
render_status_configmap_yaml | kubectl apply -f -

# Read-back loop until labels and env converge.
deadline=$(( $(date +%s) + 60 ))
while :; do
    cj="$(kubectl get cronjob "$CRONJOB_NAME" -o json 2>/dev/null || true)"
    if [ -n "$cj" ]; then
        ok=true
        echo "$cj" | jq -e '.metadata.deletionTimestamp == null' >/dev/null || ok=false
        echo "$cj" | jq -e --arg k "$WORKFLOW_UID_LABEL" --arg v "$WORKFLOW_UID" \\
            '.metadata.labels[$k] == $v' >/dev/null || ok=false
        echo "$cj" | jq -e --arg k "$SESSION_LABEL" --arg v "$SESSION_NAME" \\
            '.metadata.labels[$k] == $v' >/dev/null || ok=false
        echo "$cj" | jq -e --arg k "$WORKFLOW_UID_LABEL" --arg v "$WORKFLOW_UID" \\
            '.spec.jobTemplate.metadata.labels[$k] == $v' >/dev/null || ok=false
        echo "$cj" | jq -e --arg k "$SESSION_LABEL" --arg v "$SESSION_NAME" \\
            '.spec.jobTemplate.metadata.labels[$k] == $v' >/dev/null || ok=false
        echo "$cj" | jq -e --arg cc "$CONFIG_CHECKSUM" \\
            '[.spec.jobTemplate.spec.template.spec.containers[0].env[] | select(.name=="CONFIG_CHECKSUM")][0].value == $cc' \\
            >/dev/null || ok=false
        echo "$cj" | jq -e --arg claimed "$CLAIMED_AT" \\
            '[.spec.jobTemplate.spec.template.spec.containers[0].env[] | select(.name=="CLAIMED_AT")][0].value == $claimed' \\
            >/dev/null || ok=false
        if [ "$ok" = "true" ]; then
            echo "read-back consistent"
            exit 0
        fi
    fi
    if [ "$(date +%s)" -gt "$deadline" ]; then
        echo "read-back never converged within 60s" >&2
        exit 1
    fi
    sleep 2
done
`;

const startHistoricalBackfillInputs = {
    sessionName: defineRequiredParam<string>(),
    rfsJsonConfig: defineRequiredParam<string>(),
    targetBasicCredsSecretNameOrEmpty: defineRequiredParam<string>(),
    coordinatorBasicCredsSecretNameOrEmpty: defineRequiredParam<string>(),
    podReplicas: defineRequiredParam<number>(),
    jvmArgs: defineRequiredParam<string>(),
    loggingConfigurationOverrideConfigMap: defineRequiredParam<string>(),
    transformsImage: defineRequiredParam<string>(),
    transformsConfigMap: defineRequiredParam<string>(),
    useLocalStack: defineRequiredParam<boolean>({description: "Only used for local testing"}),
    resources: defineRequiredParam<ResourceRequirementsType>(),
    crdName: defineRequiredParam<string>(),
    crdUid: defineRequiredParam<string>(),
    sourceK8sLabel: defineRequiredParam<string>(),
    targetK8sLabel: defineRequiredParam<string>(),
    snapshotK8sLabel: defineRequiredParam<string>(),
    fromSnapshotMigrationK8sLabel: defineRequiredParam<string>(),
    taskK8sLabel: defineParam<string>({expression: expr.literal("reindexFromSnapshot")}),
    ...makeRequiredImageParametersForKeys(["ReindexFromSnapshot"])
};

function getRfsDeploymentManifest
(args: {
    workflowName: BaseExpression<string>,
    jsonConfig: BaseExpression<string>
    sessionName: BaseExpression<string>,
    podReplicas: BaseExpression<number>,
    targetBasicCredsSecretNameOrEmpty: AllowLiteralOrExpression<string>,
    coordinatorBasicCredsSecretNameOrEmpty: AllowLiteralOrExpression<string>,

    useLocalstackAwsCreds: BaseExpression<boolean>,
    loggingConfigMap: BaseExpression<string>,
    jvmArgs: BaseExpression<string>,
    transformsImage: BaseExpression<string>,
    transformsConfigMap: BaseExpression<string>,
    transformsVolumeMode: TransformVolumeMode,

    rfsImageName: BaseExpression<string>,
    rfsImagePullPolicy: BaseExpression<IMAGE_PULL_POLICY>,
    resources: BaseExpression<ResourceRequirementsType>,
    crdName: BaseExpression<string>,
    crdUid: BaseExpression<string>,

    sourceK8sLabel: BaseExpression<string>,
    targetK8sLabel: BaseExpression<string>,
    snapshotK8sLabel: BaseExpression<string>,
    fromSnapshotMigrationK8sLabel: BaseExpression<string>,
    taskK8sLabel: BaseExpression<string>
}): Deployment {
    const ownerReferences: OwnerReference[] = [{
        apiVersion: "migrations.opensearch.org/v1alpha1",
        kind: "SnapshotMigration",
        name: makeDirectTypeProxy(args.crdName),
        uid: makeDirectTypeProxy(args.crdUid),
        controller: false,
        blockOwnerDeletion: true
    }];
    const useCustomLogging = expr.not(expr.isEmpty(args.loggingConfigMap));
    const baseContainerDefinition = {
        name: CONTAINER_NAMES.BULK_LOADER,
        image: makeStringTypeProxy(args.rfsImageName),
        imagePullPolicy: makeStringTypeProxy(args.rfsImagePullPolicy),
        command: ["/rfs-app/runJavaWithClasspathWithRepeat.sh"],
        env: [
            ...getTargetHttpAuthCredsEnvVars(args.targetBasicCredsSecretNameOrEmpty),
            ...getCoordinatorHttpAuthCredsEnvVars(args.coordinatorBasicCredsSecretNameOrEmpty),
            // We don't have a mechanism to scrape these off disk so need to disable this to avoid filling up the disk
            {
                name: "FAILED_REQUESTS_LOGGER_LEVEL",
                value: "OFF"
            },
            {
                name: "CONSOLE_LOG_FORMAT",
                value: "json"
            }
        ],
        args: [
            "org.opensearch.migrations.RfsMigrateDocuments",
            "---INLINE-JSON",
            makeStringTypeProxy(args.jsonConfig)
        ],
        resources: makeDirectTypeProxy(args.resources)
    };

    const finalContainerDefinition = setupTransformsForContainerForMode(
        args.transformsVolumeMode,
        args.transformsImage,
        args.transformsConfigMap,
        setupTestCredsForContainer(
            args.useLocalstackAwsCreds,
            setupLog4jConfigForContainer(
                useCustomLogging,
                args.loggingConfigMap,
                {container: baseContainerDefinition, volumes: []},
                args.jvmArgs
            )
        )
    );
    const deploymentName = getRfsDeploymentName(args.sessionName);
    return {
        apiVersion: "apps/v1",
        kind: "Deployment",
        metadata: {
            name: makeStringTypeProxy(deploymentName),
            ownerReferences,
            labels: {
                "workflows.argoproj.io/workflow": makeStringTypeProxy(args.workflowName),
                "migrations.opensearch.org/source": makeStringTypeProxy(args.sourceK8sLabel),
                "migrations.opensearch.org/target": makeStringTypeProxy(args.targetK8sLabel),
                "migrations.opensearch.org/snapshot": makeStringTypeProxy(args.snapshotK8sLabel),
                "migrations.opensearch.org/from-snapshot-migration": makeStringTypeProxy(args.fromSnapshotMigrationK8sLabel),
                "migrations.opensearch.org/task": makeStringTypeProxy(args.taskK8sLabel)
            },
        },
        spec: {
            replicas: makeDirectTypeProxy(args.podReplicas),
            strategy: {
                type: "RollingUpdate",
                rollingUpdate: {
                    maxUnavailable: 1,
                    maxSurge: 1
                }
            },
            selector: {
                matchLabels: {
                    app: "bulk-loader",
                    "deployment-name": makeStringTypeProxy(deploymentName)
                },
            },
            template: {
                metadata: {
                    labels: {
                        app: "bulk-loader",
                        "deployment-name": makeStringTypeProxy(deploymentName),
                        "workflows.argoproj.io/workflow": makeStringTypeProxy(args.workflowName),
                        "migrations.opensearch.org/source": makeStringTypeProxy(args.sourceK8sLabel),
                        "migrations.opensearch.org/target": makeStringTypeProxy(args.targetK8sLabel),
                        "migrations.opensearch.org/snapshot": makeStringTypeProxy(args.snapshotK8sLabel),
                        "migrations.opensearch.org/from-snapshot-migration": makeStringTypeProxy(args.fromSnapshotMigrationK8sLabel),
                        "migrations.opensearch.org/task": makeStringTypeProxy(args.taskK8sLabel)
                    },
                },
                spec: {
                    serviceAccountName: "argo-workflow-executor",
                    containers: [finalContainerDefinition.container],
                    volumes: [...finalContainerDefinition.volumes]
                }
            }
        }
    } as Deployment;
}


export const DocumentBulkLoad = WorkflowBuilder.create({
    k8sResourceName: "document-bulk-load",
    serviceAccountName: "argo-workflow-executor"
})

    .addParams(CommonWorkflowParameters)

    // Apply (or update) the RFS-completion CronJob and its status ConfigMap.
    // Janitor-first ordering (INV-8): apply the CronJob before SM reconcile
    // and runtime apply, so the safety net is in place before any work that
    // might need to be cleaned up.
    .addTemplate("applyRfsDoneCronJob", t => t
        .addRequiredInput("sessionName", typeToken<string>())
        .addRequiredInput("crdName", typeToken<string>())
        .addRequiredInput("crdUid", typeToken<string>())
        .addRequiredInput("configChecksum", typeToken<string>())
        .addRequiredInput("checksumForReplayer", typeToken<string>())
        .addRequiredInput("usesDedicatedCoordinator", typeToken<string>())
        .addRequiredInput("fromSnapshotMigrationK8sLabel", typeToken<string>())
        .addRequiredInput("consoleConfigContents", typeToken<z.infer<typeof CONSOLE_SERVICES_CONFIG_FILE>>())
        .addInputsFromRecord(makeRequiredImageParametersForKeys(["MigrationConsole"]))

        .addContainer(c => c
            .addImageInfo(c.inputs.imageMigrationConsoleLocation, c.inputs.imageMigrationConsolePullPolicy)
            .addResources(DEFAULT_RESOURCES.SHELL_MIGRATION_CONSOLE_CLI)
            .addCommand(["/bin/sh", "-c"])
            .addArgs([
                expr.fillTemplate(APPLY_RFS_DONE_CRONJOB_SCRIPT, {
                    CRONJOB_NAME: getRfsDoneCronJobName(c.inputs.sessionName),
                    STATUS_CONFIGMAP_NAME: getRfsStatusConfigMapName(c.inputs.sessionName),
                    SESSION_NAME: c.inputs.sessionName,
                    WORKFLOW_NAME: expr.getWorkflowValue("name"),
                    WORKFLOW_UID: expr.getWorkflowValue("uid"),
                    PARENT_WORKFLOW_NAME: expr.getWorkflowValue("name"),
                    PARENT_WORKFLOW_UID: expr.getWorkflowValue("uid"),
                    SNAPSHOT_MIGRATION_NAME: c.inputs.crdName,
                    SM_UID: c.inputs.crdUid,
                    CONFIG_CHECKSUM: c.inputs.configChecksum,
                    CHECKSUM_FOR_REPLAYER: c.inputs.checksumForReplayer,
                    RFS_DEPLOYMENT_NAME: getRfsDeploymentName(c.inputs.sessionName),
                    RFS_COORDINATOR_NAME: getRfsCoordinatorClusterName(c.inputs.sessionName),
                    USES_DEDICATED_COORDINATOR: c.inputs.usesDedicatedCoordinator,
                    CONSOLE_IMAGE: c.inputs.imageMigrationConsoleLocation,
                    CONSOLE_IMAGE_PULL_POLICY: c.inputs.imageMigrationConsolePullPolicy,
                    FROM_SNAPSHOT_MIGRATION_LABEL: c.inputs.fromSnapshotMigrationK8sLabel,
                    CONSOLE_CONFIG_BASE64: expr.toBase64(expr.asString(c.inputs.consoleConfigContents)),
                })
            ])
        )
        .addRetryParameters({
            limit: "5",
            retryPolicy: "Always",
            backoff: {duration: "2", factor: "2", cap: "30"}
        })
    )


    .addTemplate("startHistoricalBackfillWithImageTransforms", t => t
        .addInputsFromRecord(startHistoricalBackfillInputs)
        .addResourceTask(b => b
            .setDefinition({
                action: "apply",
                setOwnerReference: false,
                manifest: getRfsDeploymentManifest({
                    podReplicas: expr.deserializeRecord(b.inputs.podReplicas),
                    loggingConfigMap: b.inputs.loggingConfigurationOverrideConfigMap,
                    jvmArgs: b.inputs.jvmArgs,
                    transformsImage: b.inputs.transformsImage,
                    transformsConfigMap: b.inputs.transformsConfigMap,
                    transformsVolumeMode: "image",
                    useLocalstackAwsCreds: expr.deserializeRecord(b.inputs.useLocalStack),
                    sessionName: b.inputs.sessionName,
                    targetBasicCredsSecretNameOrEmpty: b.inputs.targetBasicCredsSecretNameOrEmpty,
                    coordinatorBasicCredsSecretNameOrEmpty: b.inputs.coordinatorBasicCredsSecretNameOrEmpty,
                    rfsImageName: b.inputs.imageReindexFromSnapshotLocation,
                    rfsImagePullPolicy: b.inputs.imageReindexFromSnapshotPullPolicy,
                    workflowName: expr.getWorkflowValue("name"),
                    jsonConfig: expr.toBase64(b.inputs.rfsJsonConfig),
                    resources: expr.deserializeRecord(b.inputs.resources),
                    crdName: b.inputs.crdName,
                    crdUid: b.inputs.crdUid,
                    sourceK8sLabel: b.inputs.sourceK8sLabel,
                    targetK8sLabel: b.inputs.targetK8sLabel,
                    snapshotK8sLabel: b.inputs.snapshotK8sLabel,
                    fromSnapshotMigrationK8sLabel: b.inputs.fromSnapshotMigrationK8sLabel,
                    taskK8sLabel: b.inputs.taskK8sLabel,
                })
            }))
        .addRetryParameters(K8S_RESOURCE_RETRY_STRATEGY)
    )
    .addTemplate("startHistoricalBackfillWithConfigMapTransforms", t => t
        .addInputsFromRecord(startHistoricalBackfillInputs)
        .addResourceTask(b => b
            .setDefinition({
                action: "apply",
                setOwnerReference: false,
                manifest: getRfsDeploymentManifest({
                    podReplicas: expr.deserializeRecord(b.inputs.podReplicas),
                    loggingConfigMap: b.inputs.loggingConfigurationOverrideConfigMap,
                    jvmArgs: b.inputs.jvmArgs,
                    transformsImage: b.inputs.transformsImage,
                    transformsConfigMap: b.inputs.transformsConfigMap,
                    transformsVolumeMode: "configMap",
                    useLocalstackAwsCreds: expr.deserializeRecord(b.inputs.useLocalStack),
                    sessionName: b.inputs.sessionName,
                    targetBasicCredsSecretNameOrEmpty: b.inputs.targetBasicCredsSecretNameOrEmpty,
                    coordinatorBasicCredsSecretNameOrEmpty: b.inputs.coordinatorBasicCredsSecretNameOrEmpty,
                    rfsImageName: b.inputs.imageReindexFromSnapshotLocation,
                    rfsImagePullPolicy: b.inputs.imageReindexFromSnapshotPullPolicy,
                    workflowName: expr.getWorkflowValue("name"),
                    jsonConfig: expr.toBase64(b.inputs.rfsJsonConfig),
                    resources: expr.deserializeRecord(b.inputs.resources),
                    crdName: b.inputs.crdName,
                    crdUid: b.inputs.crdUid,
                    sourceK8sLabel: b.inputs.sourceK8sLabel,
                    targetK8sLabel: b.inputs.targetK8sLabel,
                    snapshotK8sLabel: b.inputs.snapshotK8sLabel,
                    fromSnapshotMigrationK8sLabel: b.inputs.fromSnapshotMigrationK8sLabel,
                    taskK8sLabel: b.inputs.taskK8sLabel,
                })
            }))
        .addRetryParameters(K8S_RESOURCE_RETRY_STRATEGY)
    )
    .addTemplate("startHistoricalBackfillNoTransforms", t => t
        .addInputsFromRecord(startHistoricalBackfillInputs)
        .addResourceTask(b => b
            .setDefinition({
                action: "apply",
                setOwnerReference: false,
                manifest: getRfsDeploymentManifest({
                    podReplicas: expr.deserializeRecord(b.inputs.podReplicas),
                    loggingConfigMap: b.inputs.loggingConfigurationOverrideConfigMap,
                    jvmArgs: b.inputs.jvmArgs,
                    transformsImage: b.inputs.transformsImage,
                    transformsConfigMap: b.inputs.transformsConfigMap,
                    transformsVolumeMode: "emptyDir",
                    useLocalstackAwsCreds: expr.deserializeRecord(b.inputs.useLocalStack),
                    sessionName: b.inputs.sessionName,
                    targetBasicCredsSecretNameOrEmpty: b.inputs.targetBasicCredsSecretNameOrEmpty,
                    coordinatorBasicCredsSecretNameOrEmpty: b.inputs.coordinatorBasicCredsSecretNameOrEmpty,
                    rfsImageName: b.inputs.imageReindexFromSnapshotLocation,
                    rfsImagePullPolicy: b.inputs.imageReindexFromSnapshotPullPolicy,
                    workflowName: expr.getWorkflowValue("name"),
                    jsonConfig: expr.toBase64(b.inputs.rfsJsonConfig),
                    resources: expr.deserializeRecord(b.inputs.resources),
                    crdName: b.inputs.crdName,
                    crdUid: b.inputs.crdUid,
                    sourceK8sLabel: b.inputs.sourceK8sLabel,
                    targetK8sLabel: b.inputs.targetK8sLabel,
                    snapshotK8sLabel: b.inputs.snapshotK8sLabel,
                    fromSnapshotMigrationK8sLabel: b.inputs.fromSnapshotMigrationK8sLabel,
                    taskK8sLabel: b.inputs.taskK8sLabel,
                })
            }))
        .addRetryParameters(K8S_RESOURCE_RETRY_STRATEGY)
    )
    .addTemplate("startHistoricalBackfill", t => t
        .addInputsFromRecord(startHistoricalBackfillInputs)
        .addSteps(b => {
            const hasImage = expr.not(expr.isEmpty(b.inputs.transformsImage));
            const hasConfigMap = expr.not(expr.isEmpty(b.inputs.transformsConfigMap));

            return b
                .addStep("withImageTransforms", INTERNAL, "startHistoricalBackfillWithImageTransforms", c =>
                    c.register({...selectInputsForRegister(b, c), taskK8sLabel: b.inputs.taskK8sLabel}),
                    {when: {templateExp: hasImage}}
                )
                .addStep("withConfigMapTransforms", INTERNAL, "startHistoricalBackfillWithConfigMapTransforms", c =>
                    c.register({...selectInputsForRegister(b, c), taskK8sLabel: b.inputs.taskK8sLabel}),
                    {when: {templateExp: expr.and(expr.not(hasImage), hasConfigMap)}}
                )
                .addStep("withoutTransforms", INTERNAL, "startHistoricalBackfillNoTransforms", c =>
                    c.register({...selectInputsForRegister(b, c), taskK8sLabel: b.inputs.taskK8sLabel}),
                    {when: {templateExp: expr.and(expr.not(hasImage), expr.not(hasConfigMap))}}
                );
        })
    )


    .addTemplate("startHistoricalBackfillFromConfig", t => t
        .addRequiredInput("sessionName", typeToken<string>())
        .addRequiredInput("sourceVersion", typeToken<z.infer<typeof CLUSTER_VERSION_STRING>>())
        .addRequiredInput("sourceLabel", typeToken<string>())
        .addRequiredInput("snapshotConfig", typeToken<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>())
        .addRequiredInput("targetConfig", typeToken<z.infer<typeof NAMED_TARGET_CLUSTER_CONFIG>>())
        .addRequiredInput("rfsCoordinatorConfig", typeToken<z.infer<typeof NAMED_TARGET_CLUSTER_CONFIG>>())
        .addRequiredInput("documentBackfillConfig", typeToken<z.infer<typeof ARGO_RFS_OPTIONS>>())
        .addRequiredInput("migrationLabel", typeToken<string>())
        .addRequiredInput("crdName", typeToken<string>())
        .addRequiredInput("crdUid", typeToken<string>())
        .addOptionalInput("sourceEndpoint", c => expr.literal(""))
        .addInputsFromRecord(makeRequiredImageParametersForKeys(["ReindexFromSnapshot"]))

        .addSteps(b => b
            .addStep("startHistoricalBackfill", INTERNAL, "startHistoricalBackfill", c =>
                c.register({
                    ...selectInputsForRegister(b, c),
                    podReplicas: expr.dig(expr.deserializeRecord(b.inputs.documentBackfillConfig), ["podReplicas"], 1),
                    targetBasicCredsSecretNameOrEmpty: getHttpAuthSecretName(b.inputs.targetConfig),
                    coordinatorBasicCredsSecretNameOrEmpty: getHttpAuthSecretName(b.inputs.rfsCoordinatorConfig),
                    loggingConfigurationOverrideConfigMap: expr.dig(expr.deserializeRecord(b.inputs.documentBackfillConfig), ["loggingConfigurationOverrideConfigMap"], ""),
                    jvmArgs: expr.dig(expr.deserializeRecord(b.inputs.documentBackfillConfig), ["jvmArgs"], ""),
                    transformsImage: expr.dig(expr.deserializeRecord(b.inputs.documentBackfillConfig), ["transformsImage"], ""),
                    transformsConfigMap: expr.dig(expr.deserializeRecord(b.inputs.documentBackfillConfig), ["transformsConfigMap"], ""),
                    useLocalStack: expr.dig(expr.deserializeRecord(b.inputs.snapshotConfig), ["repoConfig", "useLocalStack"], false),
                    rfsJsonConfig: expr.asString(expr.serialize(
                        makeParamsDict(b.inputs.sourceVersion,
                            b.inputs.targetConfig,
                            b.inputs.rfsCoordinatorConfig,
                            b.inputs.snapshotConfig,
                            b.inputs.documentBackfillConfig,
                            b.inputs.sessionName)
                    )),
                    resources: expr.serialize(expr.jsonPathStrict(b.inputs.documentBackfillConfig, "resources")),
                    crdName: b.inputs.crdName,
                    crdUid: b.inputs.crdUid,
                    sourceK8sLabel: b.inputs.sourceLabel,
                    targetK8sLabel: expr.jsonPathStrict(b.inputs.targetConfig, "label"),
                    snapshotK8sLabel: expr.jsonPathStrict(b.inputs.snapshotConfig, "label"),
                    fromSnapshotMigrationK8sLabel: b.inputs.migrationLabel
                })
            )
        )
    )


    .addTemplate("doNothing", t => t
        .addSteps(b => b.addStepGroup(c => c)))


    .addTemplate("setupAndRunBulkLoad", t => t
        .addRequiredInput("sourceVersion", typeToken<z.infer<typeof CLUSTER_VERSION_STRING>>())
        .addRequiredInput("sourceLabel", typeToken<string>())
        .addRequiredInput("targetConfig", typeToken<z.infer<typeof NAMED_TARGET_CLUSTER_CONFIG>>())
        .addRequiredInput("snapshotConfig", typeToken<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>())
        .addRequiredInput("sessionName", typeToken<string>())
        .addRequiredInput("documentBackfillConfig", typeToken<z.infer<typeof ARGO_RFS_OPTIONS>>())
        .addRequiredInput("migrationLabel", typeToken<string>())
        .addRequiredInput("crdName", typeToken<string>())
        .addRequiredInput("crdUid", typeToken<string>())
        .addRequiredInput("configChecksum", typeToken<string>())
        .addRequiredInput("checksumForReplayer", typeToken<string>())
        .addOptionalInput("sourceEndpoint", c => expr.literal(""))
        .addInputsFromRecord(makeRequiredImageParametersForKeys(["ReindexFromSnapshot", "MigrationConsole", "CoordinatorCluster"]))

        .addSteps(b => {
            const createRfsCluster = shouldCreateRfsWorkCoordinationCluster(b.inputs.documentBackfillConfig);
            const rfsCoordinatorConfig = expr.ternary(
                createRfsCluster,
                expr.serialize(makeRfsCoordinatorConfig(getRfsCoordinatorClusterName(b.inputs.sessionName))),
                b.inputs.targetConfig
            );
            return b
                .addStep("setupRfsConsoleConfig", MigrationConsole, "getConsoleConfig", c =>
                    c.register({
                        ...selectInputsForRegister(b, c),
                        targetConfig: rfsCoordinatorConfig,
                        backfillSession: expr.serialize(expr.makeDict({
                            sessionName: b.inputs.sessionName,
                            deploymentName: getRfsDeploymentName(b.inputs.sessionName)
                        }))
                    })
                )

                .addStep("applyRfsDoneCronJob", INTERNAL, "applyRfsDoneCronJob", c =>
                    c.register({
                        ...selectInputsForRegister(b, c),
                        sessionName: b.inputs.sessionName,
                        crdName: b.inputs.crdName,
                        crdUid: b.inputs.crdUid,
                        configChecksum: b.inputs.configChecksum,
                        checksumForReplayer: b.inputs.checksumForReplayer,
                        usesDedicatedCoordinator: expr.ternary(createRfsCluster, expr.literal("true"), expr.literal("false")),
                        fromSnapshotMigrationK8sLabel: b.inputs.migrationLabel,
                        consoleConfigContents: c.steps.setupRfsConsoleConfig.outputs.configContents
                    })
                )

                .addStep("createRfsCoordinator", RfsCoordinatorCluster, "createRfsCoordinator", c =>
                        c.register({
                            clusterName: getRfsCoordinatorClusterName(b.inputs.sessionName),
                            coordinatorImage: b.inputs.imageCoordinatorClusterLocation,
                            ownerName: b.inputs.crdName,
                            ownerUid: b.inputs.crdUid
                        }),
                    {when: {templateExp: createRfsCluster}}
                )

                .addStep("startHistoricalBackfillFromConfig", INTERNAL, "startHistoricalBackfillFromConfig", c =>
                    c.register({
                        ...selectInputsForRegister(b, c),
                        rfsCoordinatorConfig: rfsCoordinatorConfig
                    })
                )
                
                .addStep("waitForSnapshotMigration", ResourceManagement, "waitForSnapshotMigration", c =>
                    c.register({
                        ...selectInputsForRegister(b, c),
                        resourceName: b.inputs.crdName,
                        configChecksum: b.inputs.configChecksum,
                        checksumField: expr.literal("configChecksum")
                    })
                );
        })
    )

    .getFullScope();
