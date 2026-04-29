import heapq
import sys
from contextlib import ExitStack

import click
import logging
import subprocess

from kubernetes import client

from .autocomplete_k8s_labels import get_label_completions
from .autocomplete_workflows import DEFAULT_WORKFLOW_NAME, get_workflow_completions
from .argo_utils import DEFAULT_ARGO_SERVER_URL
from ..models.utils import load_k8s_config


logger = logging.getLogger(__name__)

# Flags recognized on either side of `--`.  When they appear in the
# pass-through args they are "promoted" so the command behaves as if
# the user had placed them before `--`.
_PROMOTABLE_FLAGS = {
    '--timestamps': 'timestamps',
    '-f': 'follow',
    '--follow': 'follow',
}


def _get_label_selector(selector_str, prefix, workflow_name):
    """Parses and prefixes label selectors."""
    parts = selector_str.split(',') if selector_str else []
    prefixed_parts = []
    for part in parts:
        if '=' in part:
            k, v = part.split('=', 1)
            key = f"{prefix}{k}" if '/' not in k else k
            prefixed_parts.append(f"{key}={v}")
        else:
            prefixed_parts.append(part)
    if workflow_name:
        prefixed_parts.append(f"workflows.argoproj.io/workflow={workflow_name}")
    return ",".join(prefixed_parts)


def _promote_known_flags(extra_args):
    """Extract known flags from pass-through args so they take effect on both sides of `--`."""
    promoted = set()
    remaining = []
    for arg in extra_args:
        flag_name = _PROMOTABLE_FLAGS.get(arg)
        if flag_name:
            promoted.add(flag_name)
        else:
            remaining.append(arg)
    return promoted, remaining


@click.command(name="output")
@click.option('--list', 'list_labels', is_flag=True, default=False,
              help='List available label selectors and exit')
@click.option('--workflow-name', default=DEFAULT_WORKFLOW_NAME, shell_complete=get_workflow_completions,
              help='Workflow name to show output for (default: WORKFLOW_NAME env var)')
@click.option('--all-workflows', is_flag=True, default=False,
              help='Show output for all workflows instead of a single workflow')
@click.option(
    '--argo-server',
    default=DEFAULT_ARGO_SERVER_URL,
    help='Argo Server URL (default: ARGO_SERVER env var, or ARGO_SERVER_SERVICE_HOST:ARGO_SERVER_SERVICE_PORT)'
)
@click.option('--namespace', default='ma', help='Kubernetes namespace (default: ma)')
@click.option('--insecure', is_flag=True, default=True, help='Skip TLS certificate verification (default: True)')
@click.option('--token', help='Bearer token for authentication')
@click.option('--prefix', default='migrations.opensearch.org/',
              help='Label prefix for filters (default: migrations.opensearch.org/)')
@click.option(
    '-l', '--selector',
    help='Label selector (e.g. source=a,target=b)',
    shell_complete=get_label_completions
)
@click.option('-f', '--follow', is_flag=True, default=False,
              help='Stream live logs via stern instead of showing history')
@click.option('--timestamps', is_flag=True, default=False,
              help='Show timestamps in log output')
@click.argument('extra_args', nargs=-1, type=click.UNPROCESSED)
@click.pass_context
def output_command(ctx, list_labels, workflow_name, all_workflows, namespace, prefix, selector,
                   follow, timestamps, extra_args, **kwargs):
    """View or tail workflow logs.

    \b
    Use --list to show available label selectors.
    Arguments after -- are forwarded to the underlying tool
    (kubectl logs for history, stern for follow mode).
    Use -- --help to see what the underlying tool supports.

    \b
    Examples:
      workflow output --list
      workflow output
      workflow output --timestamps
      workflow output -f
      workflow output -l source=mycluster
      workflow output -- --since=1h --tail=100
      workflow output -f -- --container=proxy
      workflow output -- --help
    """
    _validate_inputs(ctx, all_workflows, extra_args)

    if list_labels:
        from .autocomplete_k8s_labels import _fetch_workflow_labels
        effective_name = None if all_workflows else workflow_name
        argo_server = kwargs.get('argo_server', DEFAULT_ARGO_SERVER_URL)
        token = kwargs.get('token')
        insecure = kwargs.get('insecure', True)
        label_map, _ = _fetch_workflow_labels(
            effective_name or DEFAULT_WORKFLOW_NAME, namespace,
            argo_server, token, insecure)
        if not label_map:
            click.echo("No matching pods found.")
        else:
            for key in sorted(label_map):
                for val in sorted(label_map[key]):
                    click.echo(f"{key}={val}")
        return

    promoted, passthrough_args = _promote_known_flags(list(extra_args))
    follow = follow or 'follow' in promoted
    timestamps = timestamps or 'timestamps' in promoted

    if '--help' in passthrough_args:
        _show_underlying_help(follow)
        return

    effective_name = None if all_workflows else workflow_name
    full_selector = _get_label_selector(selector, prefix, effective_name)

    if follow:
        _run_tailing_mode(namespace, full_selector, timestamps, passthrough_args)
    else:
        _run_history_mode(ctx, namespace, full_selector, timestamps, passthrough_args)


def _validate_inputs(ctx, all_workflows, extra_args):
    """Ensure mutually exclusive flags are respected and extra args are intentional."""
    is_wf_set = ctx.get_parameter_source('workflow_name') != click.core.ParameterSource.DEFAULT
    if all_workflows and is_wf_set:
        click.echo("Error: --workflow-name and --all-workflows are mutually exclusive", err=True)
        ctx.exit(1)
    # Find args that appear before -- in the command line (these are mistakes, not pass-through)
    if extra_args:
        try:
            dashdash_idx = sys.argv.index('--')
            before_dashdash = sys.argv[:dashdash_idx]
        except ValueError:
            before_dashdash = sys.argv
        for arg in extra_args:
            if not arg.startswith('-') and arg in before_dashdash:
                click.echo(f"Error: unexpected argument '{arg}'.\n", err=True)
                click.echo(ctx.get_help())
                ctx.exit(1)


def _show_underlying_help(follow):
    """Run a single --help invocation of the underlying tool."""
    if follow:
        subprocess.run(["stern", "--help"])
    else:
        subprocess.run(["kubectl", "logs", "--help"])


def _run_tailing_mode(namespace, selector, timestamps, passthrough_args):
    """Externalize stern execution."""
    cmd = ["stern", "-l", selector, "-n", namespace]
    if timestamps:
        cmd.append("--timestamps")
    cmd.extend(passthrough_args)
    subprocess.run(cmd)


def _run_history_mode(ctx, namespace, selector, show_timestamps, passthrough_args):
    """Handle the complex merging of historical logs from multiple pods."""
    pods = []
    try:
        load_k8s_config()
        v1 = client.CoreV1Api()
        pods = v1.list_namespaced_pod(namespace, label_selector=selector)
    except Exception as e:
        click.echo(f"Error listing pods: {e}", err=True)
        ctx.exit(1)

    if not pods.items:
        click.echo("No pods found matching the selector.")
        return

    with ExitStack() as stack:
        processes = []
        for pod in pods.items:
            cmd = ["kubectl", "logs", "--timestamps", pod.metadata.name, "-n", namespace] + passthrough_args
            p = stack.enter_context(subprocess.Popen(
                cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1
            ))
            processes.append((pod.metadata.name, p))

        # Merge and Stream
        streams = [p.stdout for _, p in processes]
        _stream_merged_logs(heapq.merge(*streams), show_timestamps)
        _check_process_errors(processes)


def _stream_merged_logs(merged_logs, show_ts):
    """Handle formatting and printing of the log lines."""
    try:
        for line in merged_logs:
            if not show_ts:
                # Strip RFC3339 timestamp
                parts = line.split(" ", 1)
                line = parts[1] if len(parts) > 1 else line
            click.echo(line.rstrip())
    except KeyboardInterrupt:
        click.echo("\nInterrupted by user.", err=True)


def _check_process_errors(processes):
    """Report errors from finished kubectl processes."""
    for name, p in processes:
        if p.poll() is not None and p.returncode != 0:
            err_out = p.stderr.read().strip()
            if err_out:
                click.echo(f"[{name}] Error: {err_out}", err=True)
