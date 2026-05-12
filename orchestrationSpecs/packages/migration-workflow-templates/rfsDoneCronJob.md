# Design Doc: RFS Completion CronJob for `documentBulkLoad`

## Objective

Move RFS document backfill monitoring and RFS runtime cleanup out of the Argo workflow and into a Kubernetes `CronJob` owned by the `SnapshotMigration` resource.

The RFS runtime may outlive the workflow that created it, but it must also dispose of itself when the RFS work is complete. After applying the monitor and runtime resources, the workflow should only wait for a completion signal on `SnapshotMigration`; it should no longer be responsible for stopping the RFS worker deployment or dedicated coordinator.

## Current Ground Truth

### Current Workflow Shape

The relevant flow today is:

1. `FullMigration.runSingleSnapshotMigration`
   - Reconciles the `SnapshotMigration` custom resource.
   - Skips the migration when `SnapshotMigration.status.configChecksum` already matches the desired `configChecksum`.
   - Runs `migrateFromSnapshot` when the checksum is not done.
   - Patches `SnapshotMigration.status.phase = Completed` after `migrateFromSnapshot` returns.

2. `FullMigration.migrateFromSnapshot`
   - Runs metadata migration first, if `metadataMigrationConfig` is present.
   - Runs `DocumentBulkLoad.setupAndRunBulkLoad`, if `documentBackfillConfig` is present.

3. `DocumentBulkLoad.setupAndRunBulkLoad`
   - Creates a dedicated RFS coordinator OpenSearch cluster unless `documentBackfillConfig.useTargetClusterForWorkCoordination` is true.
   - Runs `runBulkLoad`.
   - Deletes the dedicated coordinator after `runBulkLoad` returns.

4. `DocumentBulkLoad.runBulkLoad`
   - Applies the RFS worker `Deployment`.
   - Builds a migration console config containing:
     - `backfill.reindex_from_snapshot.k8s.namespace`
     - `backfill.reindex_from_snapshot.k8s.deployment_name`
     - `backfill.reindex_from_snapshot.backfill_session_name`
   - Runs `waitForCompletion`, which repeatedly runs `checkBackfillStatus`.
   - Deletes the RFS worker `Deployment`.

This design changes steps 3 and 4 so cleanup is owned by the `CronJob`, not by workflow steps.

### Names and Ownership

In the full migration workflow:

- `sessionName` is `rfs-${workloadIdentityChecksum}`.
- The RFS worker deployment name is `${sessionName}-rfs`.
- The dedicated coordinator name is `${sessionName}-rfs-coordinator`.
- The dedicated coordinator is currently a `StatefulSet`, `Service`, and `Secret`, not a `Deployment`.
- The RFS worker container runs:

```text
/rfs-app/runJavaWithClasspathWithRepeat.sh
org.opensearch.migrations.RfsMigrateDocuments
---INLINE-JSON
<base64 RFS config>
```

The RFS worker `Deployment` already renders an owner reference to `SnapshotMigration`:

```yaml
ownerReferences:
  - apiVersion: migrations.opensearch.org/v1alpha1
    kind: SnapshotMigration
    name: <crdName>
    uid: <crdUid>
    controller: false
    blockOwnerDeletion: true
```

That is the right owner model. `controller: false` is expected because `SnapshotMigration` owns multiple child resources. If deployments are observed without this owner reference, that is likely an implementation/render/apply bug.

The new `CronJob`, RFS deployment, RFS coordinator `StatefulSet`, coordinator `Service`, coordinator `Secret`, and optional status `ConfigMap` should all use the same `SnapshotMigration` owner reference.

### Current Completion Check

`checkBackfillStatus` currently runs in a `MigrationConsole` pod:

```bash
status_json=$(console --config-file=/config/migration_services.yaml --json backfill status --deep-check)
status=$(echo "$status_json" | jq -r '.status')
```

The check exits `0` only when `status == "Completed"`. Otherwise it writes progress to `/tmp/status-output.txt`, writes `Checked` to `/tmp/phase-output.txt`, and exits `1`, causing Argo retry.

The retry policy is:

```yaml
limit: "200"
retryPolicy: Always
backoff:
  duration: "5"
  factor: "2"
  cap: "300"
```

So the current steady-state check frequency is once every 5 minutes. The check itself is not a long-poll; it performs Kubernetes API calls for pod/deployment state and OpenSearch queries against the RFS working state index.

The deep status comes from the RFS working state index:

```text
.migrations_working_state_<sessionName>
```

The migration console reports `Completed` when no shards remain, specifically when shard total is zero or completed shards are greater than or equal to total shards.

## Proposed Model

### Completion Label

Use a label on `SnapshotMigration` as the handoff from the lifecycle cronjob to Argo:

```text
migrations.opensearch.org/document-backfill-completed=<configChecksum>
```

The label value should be the desired `SnapshotMigration` `configChecksum`, not `true`.

This label is the workflow synchronization signal. It is not a separate pre-flight skip gate in `DocumentBulkLoad`; the existing skip gate remains the `SnapshotMigration.status.configChecksum` check in `FullMigration.runSingleSnapshotMigration`.

### Workflow Execution Flow

For document backfill, the workflow should become:

1. Reconcile `SnapshotMigration` as today.
2. If `SnapshotMigration.status.configChecksum` already matches the desired `configChecksum`, skip `migrateFromSnapshot` as today.
3. Run metadata migration as today, if configured.
4. Build the RFS migration console config and deterministic resource names.
5. Apply the RFS completion `CronJob`, owned by `SnapshotMigration`.
6. If `useTargetClusterForWorkCoordination` is false, apply the dedicated RFS coordinator resources.
7. Apply the RFS worker `Deployment`.
8. Wait for the `SnapshotMigration` completion label set by the `CronJob`.
9. Patch `SnapshotMigration.status.phase = Completed` and `status.configChecksum` as today.

The workflow should not delete the RFS worker deployment or the dedicated RFS coordinator after the wait. The cronjob owns those lifecycle transitions so cleanup still happens when the workflow is gone.

### Per-Run Labels

The cronjob is applied before the coordinator and worker deployment. That creates a startup window where the cronjob exists but the resources it monitors do not yet exist. To avoid confusing a later workflow's resources with this cronjob's resources, every resource in this RFS runtime should include the workflow UID that created it.

Add a label like this to the cronjob, the RFS deployment, and all dedicated coordinator resources:

```text
migrations.opensearch.org/rfs-monitor-workflow-uid={{workflow.uid}}
```

The cronjob should only consider a resource to be its resource when both the deterministic resource name and this UID label match.

This label is runtime metadata only. It should not become a CRD spec field or participate in config checksum material.

### CronJob Behavior

The cronjob should run one status check per schedule and then exit. It should not contain its own status polling loop.

Use the `MigrationConsole` image, because the monitor needs:

- `console`
- `jq`
- `kubectl`
- the same config rendering shape used by `MigrationConsole.getConsoleConfig`
- the same target/coordinator auth environment behavior

Preserve the current steady-state check frequency:

```yaml
schedule: "*/5 * * * *"
concurrencyPolicy: Forbid
successfulJobsHistoryLimit: 1
failedJobsHistoryLimit: 3
```

Recommended script shape:

```bash
set -euo pipefail

owned_label_value() {
  local kind="$1"
  local name="$2"
  local json
  json="$(kubectl get "$kind" "$name" -o json 2>/dev/null || true)"
  if [[ -z "$json" ]]; then
    echo ""
    return 0
  fi
  echo "$json" | jq -r --arg key "$WORKFLOW_UID_LABEL" '.metadata.labels[$key] // ""'
}

owned_resource_exists() {
  local kind="$1"
  local name="$2"
  [[ "$(owned_label_value "$kind" "$name")" == "$WORKFLOW_UID" ]]
}

delete_if_owned() {
  local kind="$1"
  local name="$2"
  if owned_resource_exists "$kind" "$name"; then
    kubectl delete "$kind" "$name" --ignore-not-found
  else
    echo "Skipping $kind/$name cleanup because it is missing or not owned by workflow UID $WORKFLOW_UID"
  fi
}

cleanup_runtime_and_self() {
  delete_if_owned deployment "$RFS_DEPLOYMENT_NAME"

  if [[ "$USES_DEDICATED_COORDINATOR" == "true" ]]; then
    delete_if_owned statefulset "$RFS_COORDINATOR_NAME"
    delete_if_owned service "$RFS_COORDINATOR_NAME"
    delete_if_owned secret "${RFS_COORDINATOR_NAME}-creds"
  fi

  delete_if_owned cronjob "$RFS_CRONJOB_NAME"
}

write_status_configmap() {
  local summary="$1"
  local raw_json="${2:-}"
  # The implementation should render or patch ownerReferences and labels for this
  # ConfigMap. This shorthand shows the data update only.
  kubectl create configmap "$RFS_STATUS_CONFIGMAP_NAME" \
    --from-literal=summary="$summary" \
    --from-literal=status.json="$raw_json" \
    --from-literal=updatedAt="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --dry-run=client -o yaml \
    | kubectl apply -f -
}

parent_workflow_is_running() {
  local json
  json="$(kubectl get workflow "$PARENT_WORKFLOW_NAME" -o json 2>/dev/null || true)"
  if [[ -z "$json" ]]; then
    return 1
  fi

  local phase
  local uid
  phase="$(echo "$json" | jq -r '.status.phase // ""')"
  uid="$(echo "$json" | jq -r '.metadata.uid // ""')"
  [[ "$phase" == "Running" && "$uid" == "$PARENT_WORKFLOW_UID" ]]
}

cron_age_seconds() {
  local created
  created="$(kubectl get cronjob "$RFS_CRONJOB_NAME" -o jsonpath='{.metadata.creationTimestamp}')"
  echo $(( $(date -u +%s) - $(date -u -d "$created" +%s) ))
}

deployment_exists=false
if owned_resource_exists deployment "$RFS_DEPLOYMENT_NAME"; then
  deployment_exists=true
fi

snapshot_json="$(kubectl get snapshotmigration "$SNAPSHOT_MIGRATION_NAME" -o json 2>/dev/null || true)"
completion_label_value=""
if [[ -n "$snapshot_json" ]]; then
  completion_label_value="$(echo "$snapshot_json" | jq -r --arg key "$COMPLETION_LABEL_KEY" '.metadata.labels[$key] // ""')"
fi
if [[ "$completion_label_value" == "$COMPLETION_LABEL_VALUE" ]]; then
  echo "Completion label already present; ensuring runtime cleanup"
  cleanup_runtime_and_self
  exit 0
fi

if [[ "$deployment_exists" == "false" ]] \
  && ! parent_workflow_is_running \
  && [[ "$(cron_age_seconds)" -gt "$STARTUP_GRACE_SECONDS" ]]; then
  echo "RFS deployment for workflow UID $WORKFLOW_UID never appeared and workflow is not running; cleaning up monitor/runtime remnants"
  cleanup_runtime_and_self
  exit 0
fi

if ! status_json="$(console --config-file=/config/migration_services.yaml --json backfill status --deep-check 2>&1)"; then
  write_status_configmap "Status check is not available yet" "$status_json"
  echo "$status_json"
  exit 0
fi

status="$(echo "$status_json" | jq -r '.status')"
summary="$(echo "$status_json" | jq -r '
  if .status == "Pending" then
    "Shards are initializing"
  else
    "complete: \(.percentage_completed // 0)%, ETA: " +
    (if .eta_ms == null then "unknown" else "\((.eta_ms / 1000) | floor)s" end) +
    "; shards in-progress: \(.shard_in_progress // 0)" +
    "; remaining: \(.shard_waiting // 0)" +
    "; shards complete/total: \(.shard_complete // 0)/\(.shard_total // 0)"
  end
')"

write_status_configmap "$summary" "$status_json"
echo "$summary"

if [[ "$status" == "Completed" ]]; then
  kubectl label snapshotmigration "$SNAPSHOT_MIGRATION_NAME" \
    "$COMPLETION_LABEL_KEY=$COMPLETION_LABEL_VALUE" \
    --overwrite

  cleanup_runtime_and_self
  exit 0
fi

exit 0
```

Important behavior:

- Non-completion is an expected scheduled check result, so it exits `0`.
- The orphan startup check is retained from the original design: if the deployment for this workflow UID never appears, the parent workflow is no longer running, and the cronjob is older than the grace period, the cronjob gives up and deletes itself.
- The orphan startup check should also clean up any dedicated coordinator remnants for this same workflow UID.
- If the workflow dies after the RFS deployment exists, the cronjob must keep checking. The workflow phase is only used for the startup orphan case.
- If the completion label is already present but the cronjob still exists, the cronjob should retry cleanup before deleting itself.
- Cleanup should be centralized in `cleanup_runtime_and_self` because it is used by both the completion path and the already-labeled/idempotent cleanup path.

### Status ConfigMap

Today, `workflow status` and `workflow manage` display retry-check progress from Argo node outputs:

- `MigrationConsole.runMigrationCommandForStatus` writes `statusOutput` as an artifact from `/tmp/status-output.txt`.
- It writes `overriddenPhase` from `/tmp/phase-output.txt`.
- `tree_utils.get_step_status_output` reads `statusOutput` from node parameters or artifacts.
- The TUI also has a live status path that can rerun checks from `configContents` while a node is expanded.

Moving the recurring check into a cronjob means the workflow wait node will no longer naturally produce the same per-check `statusOutput` artifacts. A status `ConfigMap` is a reasonable replacement because the cronjob is the component doing the monitoring.

Recommended status `ConfigMap`:

```text
name: ${sessionName}-rfs-status
owner: SnapshotMigration
labels:
  migrations.opensearch.org/from-snapshot-migration: <migrationLabel>
  migrations.opensearch.org/rfs-monitor-workflow-uid: <workflow.uid>
data:
  summary: <human-readable one-line progress>
  status.json: <raw console JSON, or latest error text>
  updatedAt: <UTC timestamp>
```

Recommended console changes:

- Add a deterministic `rfsStatusConfigMapName` input to the workflow wait node or a related node that survives tree filtering.
- Update `workflow status` rendering to look for that input and read `data.summary` from the configmap as a fallback/addition to `statusOutput`.
- Update `workflow manage` live status for RFS wait nodes to read the configmap instead of rerunning `console backfill status`.

This avoids doubling the expensive OpenSearch status checks. The cronjob remains the only component polling the RFS status; status/manage only read the latest cached result.

This does add responsibility to the cronjob, but it is aligned with the cronjob's role: it already performs the status check, so persisting the latest display value is a small and useful side effect.

### Workflow Wait

The workflow should wait for the completion label on `SnapshotMigration`. The exact implementation can be handled separately; the important design constraint is that the workflow waits for the cronjob's signal and does not run its own RFS status loop.

### RBAC

The existing `argo-workflow-executor` role already has broad permissions for:

- deployments
- pods
- secrets/configmaps
- `snapshotmigrations`
- `snapshotmigrations/status`
- Argo workflows
- batch `jobs`

It does not currently include batch `cronjobs`.

For the simplest implementation, reuse `argo-workflow-executor` for the cronjob pod and add `cronjobs` to the existing batch RBAC rule:

```yaml
- apiGroups: ["batch"]
  resources: ["jobs", "cronjobs"]
  verbs: ["create", "get", "list", "watch", "update", "patch", "delete"]
```

The cronjob also needs permissions already present on `argo-workflow-executor`:

- `get` workflows, for the startup orphan check.
- `get`, `delete` deployments.
- `get`, `delete` statefulsets.
- `get`, `delete` services.
- `get`, `delete` secrets.
- `get`, `patch` snapshotmigrations, for the completion label.
- `create`, `get`, `update`, `patch` configmaps, if using the status configmap.

A narrower service account is possible later, but it must cover the same resource surface.

### Template Touch Points

Expected code changes:

- `documentBulkLoad.ts`
  - Add constants for the completion label and workflow UID label.
  - Add `getRfsDoneCronJobName(sessionName)`.
  - Add `getRfsStatusConfigMapName(sessionName)`.
  - Add a cronjob manifest builder owned by `SnapshotMigration`.
  - Inject the parent workflow name and UID into the cronjob environment for the startup orphan check.
  - Add the workflow UID label to the RFS deployment.
  - Apply the cronjob before the dedicated coordinator and RFS deployment.
  - Remove workflow-side RFS deployment cleanup after wait.
  - Replace `checkBackfillStatus` retry polling with a wait for the `SnapshotMigration` label.

- `rfsCoordinatorCluster.ts`
  - Add the workflow UID label to the coordinator `StatefulSet`, `Service`, and `Secret`.
  - Remove or stop using workflow-side coordinator cleanup from the document backfill path; the cronjob owns lifecycle cleanup.

- `migrationConsole` workflow status/manage code
  - Add a configmap read path for cached RFS status if the status configmap input is present.
  - Avoid rerunning live RFS status checks when the cronjob status configmap is available.

- `deployment/k8s/charts/aggregates/migrationAssistantWithArgo/templates/resources/workflowRbac.yaml`
  - Add `cronjobs` to the batch RBAC rule.

- Tests/snapshots
  - Update `documentBulkLoad` and full migration rendered snapshots.
  - Add assertions for:
    - cronjob owner reference points to `SnapshotMigration`.
    - cronjob is applied before coordinator and RFS deployment.
    - RFS deployment and coordinator resources include the workflow UID label.
    - cronjob uses `concurrencyPolicy: Forbid`.
    - cronjob uses the `MigrationConsole` image.
    - cronjob script has a shared cleanup function.
    - RBAC includes batch `cronjobs`.
    - status/manage can display cached configmap status.

## Failure Modes

| Failure Mode | Expected Behavior |
| --- | --- |
| Workflow dies before creating the cronjob | No independent monitor exists. This is the remaining unavoidable window before any runtime resource should exist. |
| Workflow dies after creating the cronjob but before creating coordinator/deployment | Cronjob waits through the startup grace period, sees no RFS deployment with its workflow UID, confirms the workflow is not running, and deletes itself. |
| Workflow dies after creating coordinator but before creating deployment | Cronjob waits through the startup grace period, sees no RFS deployment with its workflow UID, confirms the workflow is not running, deletes coordinator remnants for its workflow UID, and deletes itself. |
| Workflow dies after creating deployment and cronjob | RFS continues. The cronjob keeps checking every 5 minutes and cleans up deployment, coordinator, and itself when RFS is complete. |
| A later workflow creates same deterministic resource names | Old cronjob ignores resources whose workflow UID label does not match its own workflow UID. |
| Cronjob labels `SnapshotMigration`, then cleanup partially fails | The label remains. A later cronjob execution should see the label and retry cleanup through the shared cleanup method. |
| Workflow waits successfully, then dies before patching status | The completion label remains. The current top-level skip still keys on `status.configChecksum`, so this edge should be handled explicitly if it becomes a recovery requirement. The primary design keeps the label as a synchronization signal, not a replacement for the existing completed-status check. |
| User deletes `SnapshotMigration` | Kubernetes garbage collection should delete all owned runtime resources. |

## Fit and Open Issues

### Fits Well

- The owner model matches the current `SnapshotMigration` ownership pattern.
- The RFS status check is already available through `MigrationConsole`, so the cronjob can reuse the same behavior.
- The 5-minute cron schedule preserves the current steady-state retry cap.
- The workflow becomes a waiter for the cronjob's label signal rather than the component responsible for monitoring and cleanup.
- A status configmap avoids losing user-visible progress without introducing a second expensive status polling path.

### Needs Care

- The startup orphan check must compare deterministic resource names and workflow UID labels, not just names.
- The dedicated coordinator is a `StatefulSet`/`Service`/`Secret` in the current code, so cleanup should mirror the existing `deleteRfsCoordinator` behavior.
- The status configmap must be owned by `SnapshotMigration`; otherwise it can become the new orphan.
- If recovery after "label set but status not patched" must skip rerunning RFS, that requires an explicit top-level design choice. It should not be hidden inside `DocumentBulkLoad` as a second skip gate.

## Implementation Decisions

Default decisions unless changed before implementation:

- Completion label key: `migrations.opensearch.org/document-backfill-completed`
- Completion label value: desired `SnapshotMigration` `configChecksum`
- Runtime workflow UID label key: `migrations.opensearch.org/rfs-monitor-workflow-uid`
- Cron schedule: `*/5 * * * *`
- Startup orphan grace: 600 seconds
- Cron image: `MigrationConsole`
- Cron service account: `argo-workflow-executor`
- RFS deployment owner: `SnapshotMigration`
- RFS coordinator owner: `SnapshotMigration`
- Cronjob owner: `SnapshotMigration`
- Status configmap owner: `SnapshotMigration`
- Workflow role after startup: wait for completion label, then patch `SnapshotMigration.status`
