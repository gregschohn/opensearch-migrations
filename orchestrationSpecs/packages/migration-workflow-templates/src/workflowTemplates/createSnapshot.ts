import {z} from "zod";
import {
    ARGO_CREATE_SNAPSHOT_WORKFLOW_OPTION_KEYS,
    ARGO_CREATE_SNAPSHOT_OPTIONS,
    CLUSTER_CONFIG,
    COMPLETE_SNAPSHOT_CONFIG,
    CONSOLE_SERVICES_CONFIG_FILE,
    DEFAULT_RESOURCES,
    NAMED_SOURCE_CLUSTER_CONFIG_WITHOUT_SNAPSHOT_INFO,
} from "@opensearch-migrations/schemas";
import {MigrationConsole} from "./migrationConsole";
import {ResourceManagement} from "./resourceManagement";
import {
    BaseExpression,
    expr,
    INTERNAL,
    selectInputsForRegister, Serialized,
    typeToken,
    WorkflowBuilder
} from "@opensearch-migrations/argo-workflow-builders";
import {makeRepoParamDict} from "./metadataMigration";

import {CommonWorkflowParameters} from "./commonUtils/workflowParameters";
import {makeRequiredImageParametersForKeys} from "./commonUtils/imageDefinitions";
import {makeClusterParamDict} from "./commonUtils/clusterSettingManipulators";
import {getHttpAuthSecretName} from "./commonUtils/clusterSettingManipulators";
import {getSourceHttpAuthCreds} from "./commonUtils/basicCredsGetters";
import {CONTAINER_TEMPLATE_RETRY_STRATEGY} from "./commonUtils/resourceRetryStrategy";

function getSnapshotDoneCronJobName(dataSnapshotName: BaseExpression<string>) {
    return expr.concat(dataSnapshotName, expr.literal("-snapshot-done"));
}

const SNAPSHOT_MONITOR_WORKFLOW_UID_LABEL = "migrations.opensearch.org/snapshot-monitor-workflow-uid";
const SNAPSHOT_MONITOR_SESSION_LABEL = "migrations.opensearch.org/snapshot-monitor-session";
const SNAPSHOT_MONITOR_CADENCE_LABEL = "migrations.opensearch.org/snapshot-monitor-cadence-step";

const SNAPSHOT_DONE_CRONJOB_SCRIPT = `set -eu

SNAPSHOT_MONITOR_CADENCE_LABEL="${SNAPSHOT_MONITOR_CADENCE_LABEL}"
SNAPSHOT_MONITOR_WORKFLOW_UID_LABEL="${SNAPSHOT_MONITOR_WORKFLOW_UID_LABEL}"
K8S_API_SERVER="https://\${KUBERNETES_SERVICE_HOST}:\${KUBERNETES_SERVICE_PORT_HTTPS:-443}"
K8S_SA_TOKEN_FILE="/var/run/secrets/kubernetes.io/serviceaccount/token"
K8S_SA_CA_FILE="/var/run/secrets/kubernetes.io/serviceaccount/ca.crt"
K8S_SA_TOKEN="$(cat "$K8S_SA_TOKEN_FILE")"

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
        curl -fsS \\
            --cacert "$K8S_SA_CA_FILE" \\
            -H "Authorization: Bearer $K8S_SA_TOKEN" \\
            -H "Content-Type: $content_type" \\
            -X "$method" \\
            "$K8S_API_SERVER$path" \\
            --data "$body"
    else
        curl -fsS \\
            --cacert "$K8S_SA_CA_FILE" \\
            -H "Authorization: Bearer $K8S_SA_TOKEN" \\
            -H "Content-Type: $content_type" \\
            -X "$method" \\
            "$K8S_API_SERVER$path"
    fi
}

cronjob_json="$(kubectl get cronjob "$SNAPSHOT_CRONJOB_NAME" -o json 2>/dev/null || true)"
if [ -z "$cronjob_json" ]; then
    echo "cronjob $SNAPSHOT_CRONJOB_NAME not found; nothing to do"
    exit 0
fi
cj_workflow_uid="$(echo "$cronjob_json" | jq -r --arg k "$SNAPSHOT_MONITOR_WORKFLOW_UID_LABEL" '.metadata.labels[$k] // ""')"
if [ "$cj_workflow_uid" != "$WORKFLOW_UID" ]; then
    echo "superseded: cronjob workflow-uid=$cj_workflow_uid env.WORKFLOW_UID=$WORKFLOW_UID"
    exit 0
fi
cj_uid="$(echo "$cronjob_json" | jq -r '.metadata.uid')"
cj_rv="$(echo "$cronjob_json" | jq -r '.metadata.resourceVersion')"
cj_namespace="$(echo "$cronjob_json" | jq -r '.metadata.namespace')"

delete_cronjob_with_preconditions() {
    body="$(jq -nc --arg uid "$cj_uid" --arg rv "$cj_rv" \\
        '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid,resourceVersion:$rv}}')"
    if k8s_api DELETE "/apis/batch/v1/namespaces/$cj_namespace/cronjobs/$SNAPSHOT_CRONJOB_NAME" \\
        "application/json" "$body" >/dev/null 2>&1; then
        return 0
    fi
    echo "preconditioned cronjob delete returned non-zero (likely 409: superseded or RV advanced)"
    return 1
}

maybe_bump_cadence() {
    raw_step="$(echo "$cronjob_json" | jq -r --arg k "$SNAPSHOT_MONITOR_CADENCE_LABEL" '.metadata.labels[$k] // "1"')"
    case "$raw_step" in
        1|2|3|4|5) current="$raw_step" ;;
        *)         current=1 ;;
    esac
    if [ "$current" -ge 5 ]; then return 0; fi
    desired=$((current + 1))
    body="$(jq -nc \\
        --arg label "$SNAPSHOT_MONITOR_CADENCE_LABEL" \\
        --arg desired "$desired" \\
        --arg sched "*/$desired * * * *" \\
        --arg rv "$cj_rv" \\
        '{metadata:{resourceVersion:$rv,labels:{($label):$desired}},spec:{schedule:$sched}}')"
    if k8s_api PATCH "/apis/batch/v1/namespaces/$cj_namespace/cronjobs/$SNAPSHOT_CRONJOB_NAME" \\
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

number_or_zero() {
    case "\${1:-}" in
        ''|*[!0-9]*) echo 0 ;;
        *) echo "$1" ;;
    esac
}

extract_line_value() {
    pattern="$1"
    printf '%s\\n' "$SNAPSHOT_DEEP_OUTPUT" | awk -v p="$pattern" '$0 ~ p { sub(/^[^:]*:[[:space:]]*/, ""); print; exit }'
}

make_snapshot_status_patch() {
    data_snapshot_phase="$1"
    snapshot_phase="$2"
    message="$3"
    now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    total="$(number_or_zero "$(extract_line_value "Total shards")")"
    successful="$(number_or_zero "$(extract_line_value "Successful shards")")"
    failed="$(number_or_zero "$(extract_line_value "Failed shards")")"
    data_line="$(extract_line_value "Data processed")"
    data_processed="$(printf '%s' "$data_line" | awk '{print $1}')"
    data_processed_unit="$(printf '%s' "$data_line" | awk '{print $2}')"
    eta="$(extract_line_value "Estimated time to completion")"

    jq -nc \\
        --arg dataSnapshotPhase "$data_snapshot_phase" \\
        --arg snapshotPhase "$snapshot_phase" \\
        --arg updatedAt "$now" \\
        --arg message "$message" \\
        --arg snapshotName "$SNAPSHOT_NAME" \\
        --arg configChecksum "$CONFIG_CHECKSUM" \\
        --arg checksumForSnapshotMigration "$CONFIG_CHECKSUM" \\
        --argjson shardsTotal "$total" \\
        --argjson shardsSuccessful "$successful" \\
        --argjson shardsFailed "$failed" \\
        --arg dataProcessed "$data_processed" \\
        --arg dataProcessedUnit "$data_processed_unit" \\
        --arg eta "$eta" \\
        '{status:{
            phase:$dataSnapshotPhase,
            snapshotName:$snapshotName,
            configChecksum:$configChecksum,
            checksumForSnapshotMigration:$checksumForSnapshotMigration,
            snapshotCreation:{
                phase:$snapshotPhase,
                updatedAt:$updatedAt,
                message:$message,
                summary:{
                    shardsTotal:$shardsTotal,
                    shardsSuccessful:$shardsSuccessful,
                    shardsFailed:$shardsFailed,
                    dataProcessed:($dataProcessed | if . == "" then null else . end),
                    dataProcessedUnit:($dataProcessedUnit | if . == "" then null else . end),
                    eta:($eta | if . == "" then null else . end)
                }
            }
        }}'
}

patch_snapshot_status() {
    data_snapshot_phase="$1"
    snapshot_phase="$2"
    message="$3"
    patch_body="$(make_snapshot_status_patch "$data_snapshot_phase" "$snapshot_phase" "$message")"
    kubectl patch datasnapshot "$DATASNAPSHOT_NAME" \\
        --subresource=status --type=merge -p "$patch_body"
}

ds_json="$(kubectl get datasnapshot "$DATASNAPSHOT_NAME" -o json 2>/dev/null || true)"
if [ -z "$ds_json" ]; then
    if parent_workflow_running; then
        echo "datasnapshot $DATASNAPSHOT_NAME not found yet; parent workflow still running"
        maybe_bump_cadence
        exit 0
    fi
    elapsed=$(( $(date -u +%s) - CLAIMED_AT ))
    if [ "$elapsed" -le "$STARTUP_GRACE_SECONDS" ]; then
        echo "datasnapshot $DATASNAPSHOT_NAME not found; within startup grace"
        maybe_bump_cadence
        exit 0
    fi
    echo "datasnapshot $DATASNAPSHOT_NAME missing and parent workflow is gone; self-deleting"
    delete_cronjob_with_preconditions || true
    exit 0
fi

ds_phase="$(echo "$ds_json" | jq -r '.status.phase // ""')"
ds_config_checksum="$(echo "$ds_json" | jq -r '.status.configChecksum // ""')"
if { [ "$ds_phase" = "Completed" ] || [ "$ds_phase" = "Error" ]; } && [ "$ds_config_checksum" = "$CONFIG_CHECKSUM" ]; then
    echo "datasnapshot already terminal for this config; self-deleting"
    delete_cronjob_with_preconditions || true
    exit 0
fi

status_error_file="/tmp/snapshot-status-error.txt"
status="$(console --config-file=/config/migration_services.yaml snapshot status 2>"$status_error_file" || true)"
status_error="$(cat "$status_error_file" 2>/dev/null || true)"
SNAPSHOT_DEEP_OUTPUT="$(console --config-file=/config/migration_services.yaml snapshot status --deep-check 2>>"$status_error_file" || true)"
if [ -z "$SNAPSHOT_DEEP_OUTPUT" ]; then
    SNAPSHOT_DEEP_OUTPUT="$status_error"
fi

case "$status" in
    SUCCESS)
        patch_snapshot_status "Completed" "Completed" "Snapshot completed successfully"
        delete_cronjob_with_preconditions || true
        exit 0
        ;;
    FAILED)
        patch_snapshot_status "Error" "Failed" "Snapshot failed"
        delete_cronjob_with_preconditions || true
        exit 0
        ;;
esac

case "$SNAPSHOT_DEEP_OUTPUT" in
    SUCCESS)
        patch_snapshot_status "Completed" "Completed" "Snapshot completed successfully"
        delete_cronjob_with_preconditions || true
        exit 0
        ;;
    FAILED)
        patch_snapshot_status "Error" "Failed" "Snapshot failed"
        delete_cronjob_with_preconditions || true
        exit 0
        ;;
esac

if [ -z "$status" ] && [ -z "$SNAPSHOT_DEEP_OUTPUT" ]; then
    patch_snapshot_status "Running" "Unknown" "Snapshot status check is not available yet"
    maybe_bump_cadence
    exit 0
fi

message="$(printf '%s\\n' "$SNAPSHOT_DEEP_OUTPUT" | awk '
    /Total shards:/ { total = $3 }
    /Successful shards:/ { successful = $3 }
    /Data processed:/ { data = $3; unit = $4 }
    /Estimated time to completion:/ { sub(/.*: /, ""); eta = $0 }
    END {
        if (total) {
            output = "Shards: " successful "/" total
            if (data != "") output = output " | Data: " data " " unit
            if (eta != "" && eta != "0h 0m 0s") output = output " | ETA: " eta
            print output
        }
    }
')"
if [ -z "$message" ]; then
    message="$(printf '%s' "\${status_error:-\${SNAPSHOT_DEEP_OUTPUT:-Snapshot is running}}" | head -c 1024)"
fi

patch_snapshot_status "Running" "Running" "$message"
maybe_bump_cadence
exit 0
`;

const SNAPSHOT_DONE_CRONJOB_SCRIPT_B64 = Buffer.from(SNAPSHOT_DONE_CRONJOB_SCRIPT, "utf8").toString("base64");

const APPLY_SNAPSHOT_DONE_CRONJOB_SCRIPT = `set -eu

CRONJOB_NAME="{{CRONJOB_NAME}}"
SESSION_NAME="{{SESSION_NAME}}"
WORKFLOW_NAME="{{WORKFLOW_NAME}}"
WORKFLOW_UID="{{WORKFLOW_UID}}"
PARENT_WORKFLOW_NAME="{{PARENT_WORKFLOW_NAME}}"
PARENT_WORKFLOW_UID="{{PARENT_WORKFLOW_UID}}"
DATASNAPSHOT_NAME="{{DATASNAPSHOT_NAME}}"
DATASNAPSHOT_UID="{{DATASNAPSHOT_UID}}"
SNAPSHOT_NAME="{{SNAPSHOT_NAME}}"
CONFIG_CHECKSUM="{{CONFIG_CHECKSUM}}"
CONSOLE_IMAGE="{{CONSOLE_IMAGE}}"
CONSOLE_IMAGE_PULL_POLICY="{{CONSOLE_IMAGE_PULL_POLICY}}"
SOURCE_LABEL="{{SOURCE_LABEL}}"
SNAPSHOT_LABEL="{{SNAPSHOT_LABEL}}"
CONSOLE_CONFIG_BASE64="{{CONSOLE_CONFIG_BASE64}}"
STARTUP_GRACE_SECONDS="600"
CLAIMED_AT="$(date -u +%s)"

WORKFLOW_UID_LABEL="${SNAPSHOT_MONITOR_WORKFLOW_UID_LABEL}"
SESSION_LABEL="${SNAPSHOT_MONITOR_SESSION_LABEL}"
SNAPSHOT_DONE_SCRIPT_B64="${SNAPSHOT_DONE_CRONJOB_SCRIPT_B64}"

render_cronjob_yaml() {
    cat <<YAML
apiVersion: batch/v1
kind: CronJob
metadata:
  name: \${CRONJOB_NAME}
  ownerReferences:
    - apiVersion: migrations.opensearch.org/v1alpha1
      kind: DataSnapshot
      name: \${DATASNAPSHOT_NAME}
      uid: \${DATASNAPSHOT_UID}
      controller: false
      blockOwnerDeletion: true
  labels:
    \${WORKFLOW_UID_LABEL}: "\${WORKFLOW_UID}"
    \${SESSION_LABEL}: "\${SESSION_NAME}"
    workflows.argoproj.io/workflow: "\${WORKFLOW_NAME}"
    migrations.opensearch.org/source: "\${SOURCE_LABEL}"
    migrations.opensearch.org/snapshot: "\${SNAPSHOT_LABEL}"
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
        migrations.opensearch.org/source: "\${SOURCE_LABEL}"
        migrations.opensearch.org/snapshot: "\${SNAPSHOT_LABEL}"
    spec:
      template:
        metadata:
          labels:
            \${WORKFLOW_UID_LABEL}: "\${WORKFLOW_UID}"
            \${SESSION_LABEL}: "\${SESSION_NAME}"
            workflows.argoproj.io/workflow: "\${WORKFLOW_NAME}"
            migrations.opensearch.org/source: "\${SOURCE_LABEL}"
            migrations.opensearch.org/snapshot: "\${SNAPSHOT_LABEL}"
        spec:
          serviceAccountName: argo-workflow-executor
          restartPolicy: Never
          containers:
            - name: snapshot-monitor
              image: "\${CONSOLE_IMAGE}"
              imagePullPolicy: "\${CONSOLE_IMAGE_PULL_POLICY}"
              command: ["/bin/sh", "-c"]
              args:
                - |
$(echo "$SNAPSHOT_DONE_SCRIPT_B64" | base64 -d | sed 's/^/                  /')
              env:
                - {name: WORKFLOW_UID,             value: "\${WORKFLOW_UID}"}
                - {name: PARENT_WORKFLOW_NAME,     value: "\${PARENT_WORKFLOW_NAME}"}
                - {name: PARENT_WORKFLOW_UID,      value: "\${PARENT_WORKFLOW_UID}"}
                - {name: CLAIMED_AT,               value: "\${CLAIMED_AT}"}
                - {name: DATASNAPSHOT_NAME,        value: "\${DATASNAPSHOT_NAME}"}
                - {name: SNAPSHOT_NAME,            value: "\${SNAPSHOT_NAME}"}
                - {name: CONFIG_CHECKSUM,          value: "\${CONFIG_CHECKSUM}"}
                - {name: SNAPSHOT_CRONJOB_NAME,    value: "\${CRONJOB_NAME}"}
                - {name: STARTUP_GRACE_SECONDS,    value: "\${STARTUP_GRACE_SECONDS}"}
                - {name: CONSOLE_CONFIG_BASE64,    value: "\${CONSOLE_CONFIG_BASE64}"}
YAML
}

echo "applying CronJob $CRONJOB_NAME"
create_out="$(render_cronjob_yaml | kubectl create -f - 2>&1 || true)"
if echo "$create_out" | grep -q "created"; then
    echo "$create_out"
    echo "fresh create - no drain needed"
else
    if ! echo "$create_out" | grep -qi "AlreadyExists"; then
        echo "create failed unexpectedly: $create_out" >&2
        exit 1
    fi
    echo "exists - patching jobTemplate and draining prior claim"

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

now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
seed_status_patch="$(jq -nc \\
    --arg updatedAt "$now" \\
    --arg snapshotName "$SNAPSHOT_NAME" \\
    --arg configChecksum "$CONFIG_CHECKSUM" \\
    '{status:{
        phase:"Running",
        snapshotName:$snapshotName,
        configChecksum:$configChecksum,
        checksumForSnapshotMigration:$configChecksum,
        snapshotCreation:{
            phase:"Pending",
            updatedAt:$updatedAt,
            message:"Snapshot monitor is installed",
            summary:{
                shardsTotal:0,
                shardsSuccessful:0,
                shardsFailed:0,
                dataProcessed:null,
                dataProcessedUnit:null,
                eta:null
            }
        }
    }}')"
kubectl patch datasnapshot "$DATASNAPSHOT_NAME" \\
    --subresource=status --type=merge -p "$seed_status_patch"

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

export function makeSourceParamDict(sourceConfig: BaseExpression<Serialized<z.infer<typeof CLUSTER_CONFIG>>>) {
    return makeClusterParamDict("source", sourceConfig);
}

function makeParamsDict(
    sourceConfig: BaseExpression<Serialized<z.infer<typeof NAMED_SOURCE_CLUSTER_CONFIG_WITHOUT_SNAPSHOT_INFO>>>,
    snapshotConfig: BaseExpression<Serialized<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>>,
    options: BaseExpression<Serialized<z.infer<typeof ARGO_CREATE_SNAPSHOT_OPTIONS>>>
) {
    return expr.mergeDicts(
        expr.mergeDicts(
            makeSourceParamDict(sourceConfig),
            expr.mergeDicts(
                expr.omit(expr.deserializeRecord(options), ...ARGO_CREATE_SNAPSHOT_WORKFLOW_OPTION_KEYS),
                // noWait is essential for workflow logic - the workflow handles polling for snapshot
                // completion separately via checkSnapshotStatus, so the CreateSnapshot command must
                // return immediately to allow the workflow to manage the wait/retry behavior
                expr.makeDict({
                    "noWait": expr.literal(true)
                })
            )
        ),
        expr.mergeDicts(
            expr.makeDict({
                "snapshotName": expr.get(expr.deserializeRecord(snapshotConfig), "snapshotName"),
                "snapshotRepoName": expr.jsonPathStrict(snapshotConfig, "repoConfig", "repoName")
            }),
            makeRepoParamDict(expr.get(expr.deserializeRecord(snapshotConfig), "repoConfig"), false)
        )
    );
}


export const CreateSnapshot = WorkflowBuilder.create({
    k8sResourceName: "create-snapshot",
    parallelism: 100,
    serviceAccountName: "argo-workflow-executor"
})

    .addParams(CommonWorkflowParameters)


    .addTemplate("runCreateSnapshot", t => t
        .addRequiredInput("sourceConfig", typeToken<z.infer<typeof NAMED_SOURCE_CLUSTER_CONFIG_WITHOUT_SNAPSHOT_INFO>>())
        .addRequiredInput("snapshotConfig", typeToken<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>())
        .addRequiredInput("createSnapshotConfig", typeToken<z.infer<typeof ARGO_CREATE_SNAPSHOT_OPTIONS>>())
        .addRequiredInput("sourceK8sLabel", typeToken<string>())
        .addRequiredInput("snapshotK8sLabel", typeToken<string>())
        .addOptionalInput("taskK8sLabel", c => "snapshot")

        .addInputsFromRecord(makeRequiredImageParametersForKeys(["MigrationConsole"]))

        .addContainer(b => b
            .addImageInfo(b.inputs.imageMigrationConsoleLocation, b.inputs.imageMigrationConsolePullPolicy)
            .addCommand(["/root/createSnapshot/bin/CreateSnapshot"])
            .addVolumesFromRecord({
                'test-creds': {
                    configMap: {
                        name: expr.literal("localstack-test-creds"),
                        optional: true
                    },
                    mountPath: "/config/credentials",
                    readOnly: true
                }
            })
            .addEnvVar("AWS_SHARED_CREDENTIALS_FILE",
                expr.ternary(
                    expr.dig(expr.deserializeRecord(b.inputs.snapshotConfig), ["repoConfig", "useLocalStack"], false),
                    expr.literal("/config/credentials/configuration"),
                    expr.literal(""))
            )
            .addEnvVarsFromRecord(getSourceHttpAuthCreds(getHttpAuthSecretName(b.inputs.sourceConfig)))
            .addEnvVar("JDK_JAVA_OPTIONS",
                expr.dig(expr.deserializeRecord(b.inputs.createSnapshotConfig), ["jvmArgs"], "")
            )
            .addResources(DEFAULT_RESOURCES.JAVA_MIGRATION_CONSOLE_CLI)
            .addArgs([
                expr.literal("---INLINE-JSON"),
                expr.asString(expr.serialize(
                    makeParamsDict(b.inputs.sourceConfig, b.inputs.snapshotConfig, b.inputs.createSnapshotConfig)
                ))
            ])
            .addPodMetadata(({inputs}) => ({
                labels: {
                    'migrations.opensearch.org/source': inputs.sourceK8sLabel,
                    'migrations.opensearch.org/snapshot': inputs.snapshotK8sLabel,
                    'migrations.opensearch.org/task': inputs.taskK8sLabel
                }
            }))
        )
        .addRetryParameters(CONTAINER_TEMPLATE_RETRY_STRATEGY)
    )

    .addTemplate("applySnapshotDoneCronJob", t => t
        .addRequiredInput("configContents", typeToken<z.infer<typeof CONSOLE_SERVICES_CONFIG_FILE>>())
        .addRequiredInput("dataSnapshotName", typeToken<string>())
        .addRequiredInput("dataSnapshotUid", typeToken<string>())
        .addRequiredInput("snapshotName", typeToken<string>())
        .addRequiredInput("configChecksum", typeToken<string>())
        .addRequiredInput("sourceK8sLabel", typeToken<string>())
        .addRequiredInput("snapshotK8sLabel", typeToken<string>())
        .addInputsFromRecord(makeRequiredImageParametersForKeys(["MigrationConsole"]))
        .addContainer(b => b
            .addImageInfo(b.inputs.imageMigrationConsoleLocation, b.inputs.imageMigrationConsolePullPolicy)
            .addCommand(["/bin/bash", "-c"])
            .addResources(DEFAULT_RESOURCES.SHELL_MIGRATION_CONSOLE_CLI)
            .addArgs([
                expr.fillTemplate(APPLY_SNAPSHOT_DONE_CRONJOB_SCRIPT, {
                    CRONJOB_NAME: getSnapshotDoneCronJobName(b.inputs.dataSnapshotName),
                    SESSION_NAME: b.inputs.dataSnapshotName,
                    WORKFLOW_NAME: expr.getWorkflowValue("name"),
                    WORKFLOW_UID: expr.getWorkflowValue("uid"),
                    PARENT_WORKFLOW_NAME: expr.getWorkflowValue("name"),
                    PARENT_WORKFLOW_UID: expr.getWorkflowValue("uid"),
                    DATASNAPSHOT_NAME: b.inputs.dataSnapshotName,
                    DATASNAPSHOT_UID: b.inputs.dataSnapshotUid,
                    SNAPSHOT_NAME: b.inputs.snapshotName,
                    CONFIG_CHECKSUM: b.inputs.configChecksum,
                    CONSOLE_IMAGE: b.inputs.imageMigrationConsoleLocation,
                    CONSOLE_IMAGE_PULL_POLICY: b.inputs.imageMigrationConsolePullPolicy,
                    SOURCE_LABEL: b.inputs.sourceK8sLabel,
                    SNAPSHOT_LABEL: b.inputs.snapshotK8sLabel,
                    CONSOLE_CONFIG_BASE64: expr.toBase64(expr.asString(b.inputs.configContents)),
                })
            ])
        )
        .addRetryParameters({
            limit: "5", retryPolicy: "Always",
            backoff: {duration: "2", factor: "2", cap: "30"}
        })
    )


    .addTemplate("snapshotWorkflow", t => t
        .addRequiredInput("sourceConfig", typeToken<z.infer<typeof NAMED_SOURCE_CLUSTER_CONFIG_WITHOUT_SNAPSHOT_INFO>>())
        .addRequiredInput("snapshotConfig", typeToken<z.infer<typeof COMPLETE_SNAPSHOT_CONFIG>>())
        .addRequiredInput("createSnapshotConfig", typeToken<z.infer<typeof ARGO_CREATE_SNAPSHOT_OPTIONS>>())
        .addRequiredInput("semaphoreConfigMapName", typeToken<string>())
        .addRequiredInput("semaphoreKey", typeToken<string>())
        .addRequiredInput("configChecksum", typeToken<string>())
        .addRequiredInput("dataSnapshotName", typeToken<string>())
        .addRequiredInput("dataSnapshotUid", typeToken<string>())
        .addInputsFromRecord(makeRequiredImageParametersForKeys(["MigrationConsole"]))

        .addSteps(b => b
            .addStep("createSnapshot", INTERNAL, "runCreateSnapshot", c =>
                c.register({
                    ...selectInputsForRegister(b, c),
                    sourceK8sLabel: expr.jsonPathStrict(b.inputs.sourceConfig, "label"),
                    snapshotK8sLabel: expr.jsonPathStrict(b.inputs.snapshotConfig, "label")
                }))

            .addStep("getConsoleConfig", MigrationConsole, "getConsoleConfig", c =>
                c.register({
                    ...selectInputsForRegister(b, c)
                }))

            .addStep("applySnapshotDoneCronJob", INTERNAL, "applySnapshotDoneCronJob", c =>
                c.register({
                    ...selectInputsForRegister(b, c),
                    configContents: c.steps.getConsoleConfig.outputs.configContents,
                    dataSnapshotName: b.inputs.dataSnapshotName,
                    dataSnapshotUid: b.inputs.dataSnapshotUid,
                    snapshotName: expr.jsonPathStrict(b.inputs.snapshotConfig, "snapshotName"),
                    configChecksum: b.inputs.configChecksum,
                    sourceK8sLabel: expr.jsonPathStrict(b.inputs.sourceConfig, "label"),
                    snapshotK8sLabel: expr.jsonPathStrict(b.inputs.snapshotConfig, "label")
                }))
            .addStep("waitForCompletion", ResourceManagement, "waitForDataSnapshot", c =>
                c.register({
                    ...selectInputsForRegister(b, c),
                    resourceName: b.inputs.dataSnapshotName,
                    configChecksum: b.inputs.configChecksum,
                    checksumField: expr.literal("checksumForSnapshotMigration"),
                }))
        )
        .addSynchronization(c => ({
            semaphores: [{
                configMapKeyRef: {
                    name: c.inputs.semaphoreConfigMapName,
                    key: c.inputs.semaphoreKey
                }
            }]
        }))
        .addExpressionOutput("snapshotConfig", b => b.inputs.snapshotConfig)
    )


    .getFullScope();
