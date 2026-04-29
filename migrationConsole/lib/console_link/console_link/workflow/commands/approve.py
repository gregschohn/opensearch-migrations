"""Approve command for workflow CLI - approves pending gates via CRD status patching."""

import logging
import os

import click
from click.shell_completion import CompletionItem
from kubernetes import client
from kubernetes.client.rest import ApiException

from ..models.utils import ExitCode, load_k8s_config
from .autocomplete_workflows import DEFAULT_WORKFLOW_NAME, get_workflow_completions
from .argo_utils import DEFAULT_ARGO_SERVER_URL, get_workflow
from .crd_utils import (
    CRD_GROUP,
    CRD_VERSION,
    list_migration_resources,
    match_names,
)

logger = logging.getLogger(__name__)


def _pending_gate_names(namespace):
    return [
        name
        for _, name, phase, _ in list_migration_resources(namespace, ['approvalgates'])
        if phase in ('Initialized', 'Pending')
    ]


def _waiting_gate_names(namespace, workflow_name):
    """Find approval gates the workflow is actively waiting on via Argo node data.

    Returns list of (gate_name, denial_reason) tuples.
    """
    try:
        load_k8s_config()
        wf = get_workflow(namespace, workflow_name)
        if not wf:
            return []
        nodes = wf.get('status', {}).get('nodes', {})
        results = []
        for node in nodes.values():
            if node.get('phase') != 'Running':
                continue
            if node.get('templateName', '') != 'waitforapproval':
                continue
            gate_name = None
            for p in node.get('inputs', {}).get('parameters', []):
                if p.get('name') == 'resourceName':
                    gate_name = p['value']
            if not gate_name:
                continue
            # Find sibling tryApply's denial message via shared boundaryID
            reason = None
            boundary = node.get('boundaryID')
            if boundary:
                for sibling in nodes.values():
                    if (sibling.get('boundaryID') == boundary and
                            sibling.get('phase') == 'Failed' and
                            sibling.get('message')):
                        msg = sibling['message']
                        for marker in ('denied request: ', 'message: '):
                            idx = msg.find(marker)
                            if idx >= 0:
                                reason = msg[idx + len(marker):].strip().rstrip('.')
                                break
                        if reason:
                            break
            results.append((gate_name, reason))
        return results
    except Exception:
        return []


def _get_gate_names(namespace, workflow_name, pre_approve):
    """Get the appropriate gate names based on mode.

    Returns list of (gate_name, denial_reason_or_None) tuples.
    """
    if pre_approve:
        return [(name, None) for name in _pending_gate_names(namespace)]
    return _waiting_gate_names(namespace, workflow_name)


def approve_gate(namespace, name):
    """Patch an ApprovalGate's status.phase to Approved. Returns True if patched."""
    custom = client.CustomObjectsApi()
    try:
        custom.patch_namespaced_custom_object_status(
            group=CRD_GROUP, version=CRD_VERSION,
            namespace=namespace, plural='approvalgates', name=name,
            body={'status': {'phase': 'Approved'}}
        )
        return True
    except ApiException as e:
        logger.error(f"Failed to approve {name}: {e}")
        return False


def get_approval_task_name_completions(ctx, _, incomplete):
    """Shell completion matching the current mode (waiting gates or all pending)."""
    namespace = ctx.params.get('namespace', 'ma')
    workflow_name = ctx.params.get('workflow_name') or DEFAULT_WORKFLOW_NAME
    pre_approve = ctx.params.get('pre_approve', False)
    gates = _get_gate_names(namespace, workflow_name, pre_approve)
    return [CompletionItem(name) for name, _ in gates if name.startswith(incomplete)]


@click.command(name="approve")
@click.argument('task-names', nargs=-1, required=False, shell_complete=get_approval_task_name_completions)
@click.option('--list', 'list_gates', is_flag=True, default=False,
              help='List available gates and exit')
@click.option('--workflow-name', default=DEFAULT_WORKFLOW_NAME, shell_complete=get_workflow_completions)
@click.option('--pre-approve', is_flag=True, default=False,
              help='Approve gates the workflow has not reached yet (matches all pending gates)')
@click.option(
    '--argo-server',
    default=lambda: os.environ.get(
        'ARGO_SERVER',
        DEFAULT_ARGO_SERVER_URL
    ),
    help='Argo Server URL'
)
@click.option('--namespace', default='ma', help='Kubernetes namespace (default: ma)')
@click.option('--insecure', is_flag=True, default=True, help='Skip TLS certificate verification (default: True)')
@click.option('--token', help='Bearer token for authentication')
@click.pass_context
def approve_command(ctx, task_names, list_gates, workflow_name, pre_approve, argo_server, namespace, insecure, token):
    """Approve workflow gates matching TASK_NAMES.

    By default, only matches gates the workflow is currently waiting on.
    Use --pre-approve to also match gates the workflow has not reached yet.
    Use --list to show available gates without approving.

    Each TASK_NAME can be an exact name or glob pattern (e.g., *.evaluateMetadata).

    \b
    Examples:
        workflow approve --list
        workflow approve source.target.snap1.migration-0.evaluateMetadata
        workflow approve "*.migrateMetadata"
        workflow approve --pre-approve "*"
    """
    try:
        load_k8s_config()
        available = _get_gate_names(namespace, workflow_name, pre_approve)

        if list_gates:
            if not available:
                click.echo("No gates available.")
            else:
                for name, reason in available:
                    if reason:
                        click.echo(f"  {name}  ({reason})")
                    else:
                        click.echo(f"  {name}")
            return

        if not task_names:
            click.echo("Error: missing TASK_NAMES argument.\n", err=True)
            click.echo(ctx.get_help())
            ctx.exit(ExitCode.FAILURE.value)
            return
        if not available:
            if pre_approve:
                click.echo("No pending approval gates found.")
            else:
                click.echo("No gates are currently being waited on by the workflow.")
            ctx.exit(ExitCode.FAILURE.value)
            return

        available_names = [name for name, _ in available]
        matches = []
        for pattern in task_names:
            for name in match_names(available_names, pattern):
                if name not in matches:
                    matches.append(name)

        if not matches:
            click.echo(f"No gates match {task_names}.")
            click.echo("Available gates:")
            for name, reason in available:
                if reason:
                    click.echo(f"  - {name}  ({reason})")
                else:
                    click.echo(f"  - {name}")
            if not pre_approve:
                click.echo("\nUse --pre-approve to match gates the workflow has not reached yet.")
            ctx.exit(ExitCode.FAILURE.value)
            return

        for name in matches:
            if approve_gate(namespace, name):
                click.echo(f"  ✓ Approved {name}")
            else:
                click.echo(f"  ✗ Failed to approve {name}", err=True)
                ctx.exit(ExitCode.FAILURE.value)
                return

        click.echo(f"\nApproved {len(matches)} gate(s).")

    except Exception as e:
        click.echo(f"Error: {str(e)}", err=True)
        ctx.exit(ExitCode.FAILURE.value)
