"""Integration tests for workflow CLI commands."""

from click.testing import CliRunner
from unittest.mock import Mock, patch
from kubernetes.client.rest import ApiException

from console_link.workflow.cli import workflow_cli
from console_link.workflow.models.config import WorkflowConfig


class TestWorkflowCLICommands:
    """Test suite for workflow CLI command integration."""

    def test_short_help_alias_works_for_workflow_commands(self):
        runner = CliRunner()

        for args in (
            ['-h'],
            ['configure', '-h'],
            ['configure', 'secret', 'create', '-h'],
            ['approve', 'step', '-h'],
            ['log', 'filter', '-h'],
            ['status', '-h'],
            ['submit', '-h'],
            ['reset', '-h'],
        ):
            result = runner.invoke(workflow_cli, args)
            assert result.exit_code == 0
            assert 'Usage:' in result.output

    def test_workflow_selection_options_are_hidden_from_help(self):
        runner = CliRunner()

        for args in (
            ['status', '--help'],
            ['submit', '--help'],
            ['manage', '--help'],
            ['approve', 'step', '--help'],
            ['log', 'filter', '--help'],
        ):
            result = runner.invoke(workflow_cli, args)
            assert result.exit_code == 0
            assert '--workflow-name' not in result.output
            assert '--all-workflows' not in result.output

    @patch('console_link.workflow.commands.submit.verify_configured_secrets_exist')
    @patch('console_link.workflow.commands.submit.get_credentials_secret_store_for_namespace')
    @patch('console_link.workflow.commands.submit.delete_workflow')
    @patch('console_link.workflow.commands.submit.stop_workflow')
    @patch('console_link.workflow.commands.submit.workflow_exists')
    @patch('console_link.workflow.commands.submit.load_k8s_config')
    @patch('console_link.workflow.services.script_runner.subprocess.run')
    @patch('console_link.workflow.commands.submit.WorkflowConfigStore')
    def test_submit_command_basic(
        self,
        mock_store_class,
        mock_subprocess,
        _mock_k8s,
        mock_exists,
        mock_stop,
        mock_delete,
        _mock_get_secret_store,
        _mock_verify_secrets,
    ):
        """Test basic submit command execution."""
        # Mock subprocess to avoid actual Kubernetes submission
        mock_subprocess.return_value = Mock(
            returncode=0,
            stdout='{"workflow_name": "test-workflow-abc", "workflow_uid": "uid-123", "namespace": "ma"}'
        )
        mock_exists.return_value = False

        runner = CliRunner()

        # Mock the store with a valid config
        mock_store = Mock()
        mock_store_class.return_value = mock_store
        mock_config = WorkflowConfig({
            'parameters': {
                'message': 'test',
                'requiresApproval': False,
                'approver': ''
            }
        })
        mock_store.load_config.return_value = mock_config

        result = runner.invoke(workflow_cli, ['submit'])

        assert result.exit_code == 0
        assert 'submitted successfully' in result.output
        # The stale "NOT checking" warning should no longer appear.
        assert 'NOT checking' not in result.output
        # Check for workflow name pattern from test scripts (test-workflow-<timestamp>)
        assert 'test-workflow-' in result.output
        assert "--workflow-name" in mock_subprocess.call_args[0][0]
        assert "migration-workflow" in mock_subprocess.call_args[0][0]
        mock_stop.assert_not_called()
        mock_delete.assert_not_called()
        # The secret-existence check must be invoked before workflow submission.
        _mock_verify_secrets.assert_called_once()

    @patch('console_link.workflow.commands.submit.verify_configured_secrets_exist')
    @patch('console_link.workflow.commands.submit.get_credentials_secret_store_for_namespace')
    @patch('console_link.workflow.commands.submit.load_k8s_config')
    @patch('console_link.workflow.services.script_runner.subprocess.run')
    @patch('console_link.workflow.commands.submit.WorkflowConfigStore')
    def test_submit_command_fails_fast_when_secrets_missing(
        self,
        mock_store_class,
        mock_subprocess,
        _mock_k8s,
        _mock_get_secret_store,
        mock_verify_secrets,
    ):
        """Submit must abort before calling the workflow-submit script when a
        referenced HTTP-Basic secret is missing from the cluster."""
        import click

        # The secret check raises a click exception describing which secret is missing.
        mock_verify_secrets.side_effect = click.ClickException(
            "Found 1 missing secret that must be created to make well-formed HTTP-Basic "
            "requests to clusters:\n  source-cluster-secret\n\nRun `workflow configure edit` "
            "to create them."
        )

        runner = CliRunner()

        mock_store = Mock()
        mock_store_class.return_value = mock_store
        mock_config = WorkflowConfig({
            'parameters': {
                'message': 'test',
                'requiresApproval': False,
                'approver': ''
            }
        })
        mock_store.load_config.return_value = mock_config

        result = runner.invoke(workflow_cli, ['submit'])

        assert result.exit_code != 0
        assert 'source-cluster-secret' in result.output
        assert 'workflow configure edit' in result.output
        assert 'submitted successfully' not in result.output
        # The script_runner subprocess (which actually submits the workflow) must NOT
        # have been invoked — the check has to short-circuit before that.
        mock_subprocess.assert_not_called()

    @patch('console_link.workflow.commands.submit.verify_configured_secrets_exist')
    @patch('console_link.workflow.commands.submit.get_credentials_secret_store_for_namespace')
    @patch('console_link.workflow.commands.submit.delete_workflow')
    @patch('console_link.workflow.commands.submit.stop_workflow')
    @patch('console_link.workflow.commands.submit.workflow_exists')
    @patch('console_link.workflow.commands.submit.load_k8s_config')
    @patch('console_link.workflow.services.script_runner.subprocess.run')
    @patch('console_link.workflow.commands.submit.WorkflowService')
    @patch('console_link.workflow.commands.submit.WorkflowConfigStore')
    def test_submit_command_with_wait(
        self,
        mock_store_class,
        mock_service_class,
        mock_subprocess,
        _mock_k8s,
        mock_exists,
        mock_stop,
        mock_delete,
        _mock_get_secret_store,
        _mock_verify_secrets,
    ):
        """Test submit command with --wait flag."""
        # Mock subprocess to avoid actual Kubernetes submission
        mock_subprocess.return_value = Mock(
            returncode=0,
            stdout='{"workflow_name": "test-workflow-abc", "workflow_uid": "uid-123", "namespace": "ma"}'
        )
        mock_exists.return_value = False

        runner = CliRunner()

        # Mock the service
        mock_service = Mock()
        mock_service_class.return_value = mock_service

        mock_service.submit_workflow_to_argo.return_value = {
            'success': True,
            'workflow_name': 'test-workflow-abc',
            'workflow_uid': 'uid-123',
            'namespace': 'ma',
            'phase': None,
            'output_message': None,
            'error': None
        }

        mock_service.wait_for_workflow_completion.return_value = ('Succeeded', 'Hello World')

        # Mock the store with a valid config
        mock_store = Mock()
        mock_store_class.return_value = mock_store
        mock_config = WorkflowConfig({
            'parameters': {
                'message': 'test',
                'requiresApproval': False,
                'approver': ''
            }
        })
        mock_store.load_config.return_value = mock_config

        result = runner.invoke(workflow_cli, ['submit', '--wait', '--timeout', '60'])

        assert result.exit_code == 0
        assert 'submitted successfully' in result.output
        assert 'Waiting for workflow to complete' in result.output
        assert 'Succeeded' in result.output
        mock_stop.assert_not_called()
        mock_delete.assert_not_called()

    @patch('console_link.workflow.commands.submit.verify_configured_secrets_exist')
    @patch('console_link.workflow.commands.submit.get_credentials_secret_store_for_namespace')
    @patch('console_link.workflow.commands.submit.delete_workflow')
    @patch('console_link.workflow.commands.submit.wait_until_workflow_deleted')
    @patch('console_link.workflow.commands.submit.stop_workflow')
    @patch('console_link.workflow.commands.submit.workflow_exists')
    @patch('console_link.workflow.commands.submit.load_k8s_config')
    @patch('console_link.workflow.services.script_runner.subprocess.run')
    @patch('console_link.workflow.commands.submit.WorkflowConfigStore')
    def test_submit_command_replaces_existing_workflow(
        self,
        mock_store_class,
        mock_subprocess,
        _mock_k8s,
        mock_exists,
        mock_stop,
        mock_wait_until_deleted,
        mock_delete,
        _mock_get_secret_store,
        _mock_verify_secrets,
    ):
        """Test submit replaces an existing workflow before resubmitting."""
        mock_subprocess.return_value = Mock(
            returncode=0,
            stdout='{"workflow_name": "test-workflow-abc", "workflow_uid": "uid-123", "namespace": "ma"}'
        )
        mock_exists.return_value = True
        mock_stop.return_value = True
        mock_delete.return_value = True
        mock_wait_until_deleted.return_value = True

        runner = CliRunner()

        mock_store = Mock()
        mock_store_class.return_value = mock_store
        mock_config = WorkflowConfig({
            'parameters': {
                'message': 'test',
                'requiresApproval': False,
                'approver': ''
            }
        })
        mock_store.load_config.return_value = mock_config

        result = runner.invoke(workflow_cli, ['submit', '--workflow-name', 'migration-workflow'])

        assert result.exit_code == 0
        assert "Existing workflow 'migration-workflow' found; replacing..." in result.output
        assert 'Stopped' in result.output
        assert 'Deleted' in result.output
        mock_exists.assert_called_once_with('ma', 'migration-workflow')
        mock_stop.assert_called_once_with('ma', 'migration-workflow')
        mock_delete.assert_called_once_with('ma', 'migration-workflow')
        mock_wait_until_deleted.assert_called_once_with('ma', 'migration-workflow')

    @patch('console_link.workflow.commands.status.requests.get')
    @patch('console_link.workflow.commands.status.WorkflowService')
    def test_status_command_single_workflow(self, mock_service_class, mock_requests_get):
        """Test status command for a specific workflow."""
        runner = CliRunner()

        # Mock the service
        mock_service = Mock()
        mock_service_class.return_value = mock_service

        # Mock the Argo API response with full workflow data
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {
            'metadata': {
                'name': 'test-workflow',
                'namespace': 'ma'
            },
            'status': {
                'phase': 'Running',
                'startedAt': '2024-01-01T10:00:00Z',
                'finishedAt': None,
                'nodes': {
                    'test-workflow': {
                        'id': 'test-workflow',
                        'displayName': 'test-workflow',
                        'type': 'Steps',
                        'phase': 'Running'
                    },
                    'test-workflow-step1': {
                        'id': 'test-workflow-step1',
                        'displayName': 'step1',
                        'type': 'Pod',
                        'phase': 'Succeeded',
                        'boundaryID': 'test-workflow',
                        'startedAt': '2024-01-01T10:00:00Z'
                    },
                    'test-workflow-step2': {
                        'id': 'test-workflow-step2',
                        'displayName': 'step2',
                        'type': 'Pod',
                        'phase': 'Running',
                        'boundaryID': 'test-workflow',
                        'startedAt': '2024-01-01T10:01:00Z'
                    }
                }
            }
        }
        mock_requests_get.return_value = mock_response

        result = runner.invoke(workflow_cli, ['status', '--workflow-name', 'test-workflow'])

        assert result.exit_code == 0
        assert 'test-workflow' in result.output
        assert 'Running' in result.output
        assert 'step1' in result.output
        assert 'step2' in result.output
        assert 'workflow log all' in result.output
        assert '--workflow-name' not in result.output

    @patch('console_link.workflow.commands.status.requests.get')
    @patch('console_link.workflow.commands.status.WorkflowService')
    def test_status_command_list_all(self, mock_service_class, mock_requests_get):
        """Test status command listing all workflows."""
        runner = CliRunner()

        # Mock the service
        mock_service = Mock()
        mock_service_class.return_value = mock_service

        mock_service.list_workflows.return_value = {
            'success': True,
            'workflows': ['workflow-1', 'workflow-2'],
            'count': 2,
            'error': None
        }

        # Mock requests.get to return workflow data for each workflow
        def mock_get_response(*args, **kwargs):
            url = args[0]
            mock_response = Mock()
            mock_response.status_code = 200
            
            if 'workflow-1' in url:
                mock_response.json.return_value = {
                    'metadata': {'name': 'workflow-1', 'namespace': 'ma'},
                    'status': {
                        'phase': 'Running',
                        'startedAt': '2024-01-01T10:00:00Z',
                        'finishedAt': None,
                        'nodes': {
                            'workflow-1': {
                                'id': 'workflow-1',
                                'displayName': 'workflow-1',
                                'type': 'Steps',
                                'phase': 'Running'
                            }
                        }
                    }
                }
            elif 'workflow-2' in url:
                mock_response.json.return_value = {
                    'metadata': {'name': 'workflow-2', 'namespace': 'ma'},
                    'status': {
                        'phase': 'Succeeded',
                        'startedAt': '2024-01-01T09:00:00Z',
                        'finishedAt': '2024-01-01T09:05:00Z',
                        'nodes': {
                            'workflow-2': {
                                'id': 'workflow-2',
                                'displayName': 'workflow-2',
                                'type': 'Steps',
                                'phase': 'Succeeded'
                            }
                        }
                    }
                }
            return mock_response
        
        mock_requests_get.side_effect = mock_get_response

        result = runner.invoke(workflow_cli, ['status', '--all-workflows'])

        assert result.exit_code == 0
        assert 'Found 2 workflow(s)' in result.output
        assert 'workflow-1' in result.output
        assert 'workflow-2' in result.output

    # ─────────────────────────────────────────────
    # approve subcommand tests
    # ─────────────────────────────────────────────

    def _make_gate(self, name, category='step', status='waiting', **labels):
        from console_link.workflow.commands.approve import GateInfo
        return GateInfo(name=name, category=category, status=status, labels=labels)

    def test_approve_without_subcommand_errors(self):
        runner = CliRunner()
        result = runner.invoke(workflow_cli, ['approve'])
        assert result.exit_code != 0

    @patch('console_link.workflow.commands.approve._list_all_categories')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_list_shows_all_categories(self, mock_k8s, mock_list_all):
        runner = CliRunner()

        result = runner.invoke(workflow_cli, ['approve', '--list'])

        assert result.exit_code == 0
        mock_list_all.assert_called_once_with('ma', 'migration-workflow')

    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_with_exact_name(self, mock_k8s, mock_gather, mock_approve):
        runner = CliRunner()
        mock_gather.return_value = [
            self._make_gate('evaluatemetadata.source-target'),
            self._make_gate('migratemetadata.source-target'),
        ]
        mock_approve.return_value = True

        result = runner.invoke(
            workflow_cli,
            ['approve', 'step', 'evaluatemetadata.source-target']
        )

        assert result.exit_code == 0
        assert 'Approved 1 gate' in result.output
        mock_approve.assert_called_once_with('ma', 'evaluatemetadata.source-target')

    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_with_glob(self, mock_k8s, mock_gather, mock_approve):
        runner = CliRunner()
        mock_gather.return_value = [
            self._make_gate('metadatamigrate.a-b'),
            self._make_gate('metadatamigrate.x-y'),
            self._make_gate('backfill.a-b'),
        ]
        mock_approve.return_value = True

        result = runner.invoke(workflow_cli, ['approve', 'step', 'metadatamigrate.*'])

        assert result.exit_code == 0
        assert 'Approved 2 gate' in result.output
        assert mock_approve.call_count == 2

    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_all(self, mock_k8s, mock_gather, mock_approve):
        runner = CliRunner()
        mock_gather.return_value = [
            self._make_gate('a'), self._make_gate('b'), self._make_gate('c'),
        ]
        mock_approve.return_value = True

        result = runner.invoke(workflow_cli, ['approve', 'step', '--all'])

        assert result.exit_code == 0
        assert 'Approved 3 gate' in result.output
        assert mock_approve.call_count == 3

    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_list(self, mock_k8s, mock_gather):
        runner = CliRunner()
        mock_gather.return_value = [self._make_gate('gate-one')]

        result = runner.invoke(workflow_cli, ['approve', 'step', '--list'])

        assert result.exit_code == 0
        assert 'gate-one' in result.output
        args, kwargs = mock_gather.call_args
        assert args[:3] == ('ma', 'migration-workflow', 'step')
        assert kwargs == {'pre_approve': True, 'include_completed': True}

    @patch('console_link.workflow.commands.approve._waiting_gates_from_workflow')
    @patch('console_link.workflow.commands.approve._list_all_gates')
    def test_gather_gates_excludes_completed_by_default(
        self, mock_list_gates, mock_waiting_gates
    ):
        from console_link.workflow.commands.approve import _gather_gates

        mock_list_gates.return_value = [
            ('done-step', 'Approved', {}),
            ('future-step', 'Pending', {}),
        ]
        mock_waiting_gates.return_value = []

        gates = _gather_gates('ma', 'migration-workflow', 'step', pre_approve=True)

        assert [(gate.name, gate.status) for gate in gates] == [
            ('future-step', 'pending')
        ]

    @patch('console_link.workflow.commands.approve._waiting_gates_from_workflow')
    @patch('console_link.workflow.commands.approve._list_all_gates')
    def test_gather_gates_can_include_completed(
        self, mock_list_gates, mock_waiting_gates
    ):
        from console_link.workflow.commands.approve import _gather_gates

        mock_list_gates.return_value = [
            ('done-step', 'Approved', {}),
            ('future-step', 'Pending', {}),
        ]
        mock_waiting_gates.return_value = []

        gates = _gather_gates(
            'ma', 'migration-workflow', 'step',
            pre_approve=True, include_completed=True,
        )

        assert [(gate.name, gate.status) for gate in gates] == [
            ('done-step', 'approved'),
            ('future-step', 'pending'),
        ]

    @patch('console_link.workflow.commands.approve._waiting_gates_from_workflow')
    @patch('console_link.workflow.commands.approve._list_all_gates')
    def test_gather_gates_uses_approved_phase_for_stale_waiting_node(
        self, mock_list_gates, mock_waiting_gates
    ):
        from console_link.workflow.commands.approve import _gather_gates

        mock_list_gates.return_value = [
            ('done-step', 'Approved', {}),
        ]
        mock_waiting_gates.return_value = [('done-step', None)]

        list_gates = _gather_gates(
            'ma', 'migration-workflow', 'step',
            pre_approve=True, include_completed=True,
        )
        completion_gates = _gather_gates(
            'ma', 'migration-workflow', 'step',
            pre_approve=True, include_completed=False,
        )

        assert [(gate.name, gate.status) for gate in list_gates] == [
            ('done-step', 'approved')
        ]
        assert completion_gates == []

    @patch('console_link.workflow.commands.approve._list_all_gates')
    def test_find_already_approved_is_workflow_scoped(self, mock_list_gates):
        from console_link.workflow.commands.approve import _find_already_approved

        mock_list_gates.return_value = [
            ('done-step', 'Approved', {}),
        ]

        approved = _find_already_approved(
            'ma', 'migration-workflow',
            ['done-step'], 'step',
        )

        assert approved == ['done-step']
        mock_list_gates.assert_called_once_with('ma', 'migration-workflow')

    @patch('console_link.workflow.commands.approve._list_all_gates')
    def test_find_already_approved_accepts_runtime_categories(self, mock_list_gates):
        from console_link.workflow.commands.approve import _find_already_approved

        mock_list_gates.return_value = [
            ('captureproxy.capture-proxy.vapretry', 'Approved', {
                'migrations.opensearch.org/resource-kind': 'CaptureProxy',
                'migrations.opensearch.org/resource-name': 'capture-proxy',
            }),
        ]

        approved = _find_already_approved(
            'ma', 'migration-workflow',
            ['captureproxy.capture-proxy'], 'change',
        )

        assert approved == ['captureproxy.capture-proxy']

    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_completion_keeps_completed_filtered(self, mock_k8s, mock_gather):
        from console_link.workflow.commands.approve import _complete_names

        ctx = Mock()
        ctx.params = {
            'namespace': 'ma',
            'workflow_name': 'migration-workflow',
            'pre_approve': True,
        }
        mock_gather.return_value = [self._make_gate('future-step')]

        completions = _complete_names('step')(ctx, None, 'future')

        assert [item.value for item in completions] == ['future-step']
        mock_gather.assert_called_once_with('ma', 'migration-workflow', 'step', True)

    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_list_empty(self, mock_k8s, mock_gather):
        runner = CliRunner()
        mock_gather.return_value = []

        result = runner.invoke(workflow_cli, ['approve', 'step', '--list'])

        assert result.exit_code == 0
        assert 'No gates available' in result.output

    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_no_matches(self, mock_k8s, mock_gather):
        runner = CliRunner()
        mock_gather.return_value = [self._make_gate('a-b-c')]

        result = runner.invoke(workflow_cli, ['approve', 'step', 'nonexistent'])

        assert result.exit_code != 0
        assert 'No gates match' in result.output
        assert 'a-b-c' in result.output

    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_step_missing_action_errors(self, mock_k8s, mock_gather):
        runner = CliRunner()
        mock_gather.return_value = [self._make_gate('a')]
        result = runner.invoke(workflow_cli, ['approve', 'step'])

        assert result.exit_code != 0
        assert '--list' in result.output or 'specify one of' in result.output

    def test_approve_retry_rejects_pre_approve(self):
        runner = CliRunner()
        result = runner.invoke(workflow_cli, ['approve', 'retry', '--pre-approve', '--list'])
        # Click treats unknown options as a usage error (exit 2).
        assert result.exit_code != 0
        assert '--pre-approve' in result.output.lower() or 'no such option' in result.output.lower()

    @patch('console_link.workflow.commands.approve._resource_still_exists')
    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_retry_blocks_when_resource_exists(
        self, mock_k8s, mock_gather, mock_approve, mock_exists
    ):
        runner = CliRunner()
        mock_gather.return_value = [
            self._make_gate(
                'captureproxy.capture-proxy.vapretry',
                category='retry',
                **{
                    'migrations.opensearch.org/resource-kind': 'CaptureProxy',
                    'migrations.opensearch.org/resource-name': 'capture-proxy',
                }
            )
        ]
        mock_exists.return_value = True  # resource still present → block

        result = runner.invoke(
            workflow_cli,
            ['approve', 'retry', 'captureproxy.capture-proxy.vapretry']
        )

        assert result.exit_code != 0
        assert 'still exist' in result.output or 'still exist' in result.stderr_bytes.decode('utf-8', errors='replace')
        assert mock_approve.call_count == 0

    @patch('console_link.workflow.commands.approve._resource_still_exists')
    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_retry_approves_when_resource_gone(
        self, mock_k8s, mock_gather, mock_approve, mock_exists
    ):
        runner = CliRunner()
        mock_gather.return_value = [
            self._make_gate(
                'captureproxy.capture-proxy.vapretry',
                category='retry',
                **{
                    'migrations.opensearch.org/resource-kind': 'CaptureProxy',
                    'migrations.opensearch.org/resource-name': 'capture-proxy',
                }
            )
        ]
        mock_exists.return_value = False  # resource absent → allow
        mock_approve.return_value = True

        result = runner.invoke(
            workflow_cli,
            ['approve', 'retry', 'captureproxy.capture-proxy.vapretry']
        )

        assert result.exit_code == 0
        assert 'Approved 1 gate' in result.output
        mock_approve.assert_called_once_with('ma', 'captureproxy.capture-proxy.vapretry')

    @patch('console_link.workflow.commands.approve.approve_gate')
    @patch('console_link.workflow.commands.approve._gather_gates')
    @patch('console_link.workflow.commands.approve.load_k8s_config')
    def test_approve_change_uses_change_category(self, mock_k8s, mock_gather, mock_approve):
        runner = CliRunner()
        mock_gather.return_value = [self._make_gate('cp.captureproxy.vapretry', category='change')]
        mock_approve.return_value = True

        result = runner.invoke(workflow_cli, ['approve', 'change', '--all'])

        assert result.exit_code == 0
        args, kwargs = mock_gather.call_args
        assert args[2] == 'change'
        assert kwargs == {'pre_approve': False, 'include_completed': False}

    @patch('console_link.workflow.commands.log._run_history_mode')
    def test_output_all_uses_workflow_selector(self, mock_history):
        runner = CliRunner()

        result = runner.invoke(workflow_cli, ['log', 'all'])

        assert result.exit_code == 0
        args, _ = mock_history.call_args
        assert args[2] == 'workflows.argoproj.io/workflow=migration-workflow'
        assert args[4] == []

    @patch('console_link.workflow.commands.log._run_history_mode')
    def test_output_filter_combines_filter_options(self, mock_history):
        runner = CliRunner()

        result = runner.invoke(
            workflow_cli,
            ['log', 'filter', '--snapshot', 'snap1', '--target', 'target1', '--', '--since=1h']
        )

        assert result.exit_code == 0
        args, _ = mock_history.call_args
        assert args[2] == (
            'migrations.opensearch.org/target=target1,'
            'migrations.opensearch.org/snapshot=snap1,'
            'workflows.argoproj.io/workflow=migration-workflow'
        )
        assert args[4] == ['--since=1h']

    @patch('console_link.workflow.commands.log.load_k8s_config')
    @patch('console_link.workflow.commands.log.client')
    @patch('console_link.workflow.commands.log._run_history_mode')
    def test_output_resource_uses_resource_labels(self, mock_history, mock_client, _mock_k8s):
        runner = CliRunner()
        mock_custom = Mock()
        mock_client.CustomObjectsApi.return_value = mock_custom
        mock_custom.get_namespaced_custom_object.return_value = {
            'metadata': {
                'labels': {
                    'migrations.opensearch.org/source': 'source1',
                    'migrations.opensearch.org/target': 'target1',
                    'strimzi.io/cluster': 'default',
                    'app.kubernetes.io/name': 'ignored',
                }
            }
        }

        result = runner.invoke(workflow_cli, ['log', 'resource', 'captureproxy.my-proxy'])

        assert result.exit_code == 0
        mock_custom.get_namespaced_custom_object.assert_called_once_with(
            group='migrations.opensearch.org',
            version='v1alpha1',
            namespace='ma',
            plural='captureproxies',
            name='my-proxy',
        )
        args, _ = mock_history.call_args
        assert args[2] == (
            'migrations.opensearch.org/source=source1,'
            'migrations.opensearch.org/target=target1,'
            'strimzi.io/cluster=default'
        )

    @patch('console_link.workflow.commands.log.load_k8s_config')
    @patch('console_link.workflow.commands.log.client')
    @patch('console_link.workflow.commands.log._run_history_mode')
    def test_output_resource_keeps_workflow_selector_for_workflow_pods(
        self, mock_history, mock_client, _mock_k8s
    ):
        runner = CliRunner()
        mock_custom = Mock()
        mock_client.CustomObjectsApi.return_value = mock_custom
        mock_custom.get_namespaced_custom_object.return_value = {
            'metadata': {
                'labels': {
                    'migrations.opensearch.org/source': 'source1',
                    'migrations.opensearch.org/task': 'captureProxy',
                }
            }
        }

        result = runner.invoke(workflow_cli, ['log', 'resource', 'captureproxy.my-proxy'])

        assert result.exit_code == 0
        args, _ = mock_history.call_args
        assert args[2] == (
            'migrations.opensearch.org/source=source1,'
            'migrations.opensearch.org/task=captureProxy,'
            'workflows.argoproj.io/workflow=migration-workflow'
        )

    @patch('console_link.workflow.commands.log._run_history_mode')
    def test_output_filter_accepts_raw_label_option(self, mock_history):
        runner = CliRunner()

        result = runner.invoke(
            workflow_cli,
            ['log', 'filter', '--label', 'custom.example/key=value']
        )

        assert result.exit_code == 0
        args, _ = mock_history.call_args
        assert args[2] == (
            'custom.example/key=value,'
            'workflows.argoproj.io/workflow=migration-workflow'
        )

    def test_output_filter_rejects_unexpected_argument(self):
        runner = CliRunner()

        result = runner.invoke(workflow_cli, ['log', 'filter', 'oops'])

        assert result.exit_code != 0
        assert 'Use filter options such as --task or --label' in result.output

    def test_output_top_level_requires_subcommand(self):
        runner = CliRunner()

        result = runner.invoke(workflow_cli, ['log'])

        assert result.exit_code != 0
        assert 'Missing command' in result.output or 'Usage:' in result.output

    @patch('console_link.workflow.commands.log.load_k8s_config')
    @patch('console_link.workflow.commands.log.list_migration_resources')
    def test_output_resource_completion_uses_migration_resource_names(self, mock_list, _mock_k8s):
        from console_link.workflow.commands.log import _get_resource_completions

        ctx = Mock()
        ctx.params = {'namespace': 'ma'}
        mock_list.return_value = [
            ('captureproxies', 'my-proxy', 'Ready', []),
            ('snapshotmigrations', 'migration-0', 'Ready', []),
        ]

        completions = _get_resource_completions(ctx, None, 'capture')

        assert completions == ['captureproxy.my-proxy']

    @patch('console_link.workflow.commands.submit.verify_configured_secrets_exist')
    @patch('console_link.workflow.commands.submit.get_credentials_secret_store_for_namespace')
    @patch('console_link.workflow.commands.submit.delete_workflow')
    @patch('console_link.workflow.commands.submit.stop_workflow')
    @patch('console_link.workflow.commands.submit.workflow_exists')
    @patch('console_link.workflow.commands.submit.load_k8s_config')
    @patch('console_link.workflow.services.script_runner.subprocess.run')
    @patch('console_link.workflow.commands.submit.WorkflowConfigStore')
    def test_submit_command_with_config_injection(
        self,
        mock_store_class,
        mock_subprocess,
        _mock_k8s,
        mock_exists,
        mock_stop,
        mock_delete,
        _mock_get_secret_store,
        _mock_verify_secrets,
    ):
        """Test submit command with parameter injection from config."""
        # Mock subprocess to avoid actual Kubernetes submission
        mock_subprocess.return_value = Mock(
            returncode=0,
            stdout='{"workflow_name": "test-workflow-def", "workflow_uid": "uid-789", "namespace": "ma"}'
        )
        mock_exists.return_value = False

        runner = CliRunner()

        # Mock the store with config
        mock_store = Mock()
        mock_store_class.return_value = mock_store
        mock_config = WorkflowConfig({
            'parameters': {
                'message': 'test message',
                'requiresApproval': False,
                'approver': ''
            }
        })
        mock_store.load_config.return_value = mock_config

        result = runner.invoke(workflow_cli, ['submit'])

        assert result.exit_code == 0
        assert 'submitted successfully' in result.output
        # Check for workflow name pattern from test scripts
        assert 'test-workflow-' in result.output
        mock_stop.assert_not_called()
        mock_delete.assert_not_called()


class TestConfigureCommands:
    """Test suite for configure CLI commands."""

    @patch('console_link.workflow.commands.configure.get_workflow_config_store')
    def test_configure_sample_load(self, mock_get_workflow_config):
        """Test configure sample --load command."""
        runner = CliRunner()

        # Mock the store
        mock_store = Mock()
        mock_get_workflow_config.return_value = mock_store
        mock_store.save_config.return_value = "Configuration saved"

        result = runner.invoke(workflow_cli, ['configure', 'sample', '--load'])

        assert result.exit_code == 0
        assert 'Sample configuration loaded successfully' in result.output
        # Verify save_config was called
        assert mock_store.save_config.called

    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    def test_configure_secret_list_shows_managed_secrets(self, mock_get_secret_store):
        runner = CliRunner()
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.list_secrets.return_value = ['source-secret', 'target-secret']

        result = runner.invoke(workflow_cli, ['configure', 'secret', 'list'])

        assert result.exit_code == 0
        assert 'source-secret' in result.output
        assert 'target-secret' in result.output

    @patch('console_link.workflow.commands.configure.validate_and_find_secrets')
    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    @patch('console_link.workflow.commands.configure.get_workflow_config_store')
    def test_configure_secret_create_show_missing(self, mock_get_config_store, mock_get_secret_store, mock_validate):
        runner = CliRunner()
        mock_config_store = Mock()
        mock_get_config_store.return_value = mock_config_store
        mock_config_store.load_config.return_value = WorkflowConfig(raw_yaml='sourceClusters: {}')
        mock_validate.return_value = {
            'valid': True,
            'validSecrets': ['source-secret', 'target-secret'],
        }
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.secrets_exist.return_value = {
            'source-secret': False,
            'target-secret': True,
        }

        result = runner.invoke(workflow_cli, ['configure', 'secret', 'create', '--show-missing'])

        assert result.exit_code == 0
        assert 'source-secret' in result.output
        assert 'target-secret' not in result.output

    @patch('console_link.workflow.commands.configure.validate_and_find_secrets')
    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    @patch('console_link.workflow.commands.configure.get_workflow_config_store')
    def test_configure_secret_create_list_alias(self, mock_get_config_store, mock_get_secret_store, mock_validate):
        runner = CliRunner()
        mock_config_store = Mock()
        mock_get_config_store.return_value = mock_config_store
        mock_config_store.load_config.return_value = WorkflowConfig(raw_yaml='sourceClusters: {}')
        mock_validate.return_value = {
            'valid': True,
            'validSecrets': ['source-secret'],
        }
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.secrets_exist.return_value = {'source-secret': False}

        result = runner.invoke(workflow_cli, ['configure', 'secret', 'create', '--list'])

        assert result.exit_code == 0
        assert 'source-secret' in result.output

    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    def test_configure_secret_update_list_shows_managed_secrets(self, mock_get_secret_store):
        runner = CliRunner()
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.list_secrets.return_value = ['source-secret']

        result = runner.invoke(workflow_cli, ['configure', 'secret', 'update', '--list'])

        assert result.exit_code == 0
        assert 'source-secret' in result.output

    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    def test_configure_secret_delete_list_shows_managed_secrets(self, mock_get_secret_store):
        runner = CliRunner()
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.list_secrets.return_value = ['target-secret']

        result = runner.invoke(workflow_cli, ['configure', 'secret', 'delete', '--list'])

        assert result.exit_code == 0
        assert 'target-secret' in result.output

    @patch('console_link.workflow.commands.configure.validate_and_find_secrets')
    @patch('console_link.workflow.commands.configure.get_credentials_secret_store')
    def test_configure_secret_create_prompts_for_credentials(
        self,
        mock_get_secret_store,
        mock_validate,
    ):
        runner = CliRunner()
        mock_secret_store = Mock()
        mock_get_secret_store.return_value = mock_secret_store
        mock_secret_store.namespace = 'ma'
        mock_secret_store.default_labels = {'use-case': 'http-basic-credentials'}
        mock_secret_store.v1.read_namespaced_secret.side_effect = ApiException(status=404)
        mock_secret_store.save_secret.return_value = 'Secret created: source-secret'

        result = runner.invoke(
            workflow_cli,
            ['configure', 'secret', 'create', 'source-secret'],
            input='admin\nsecret-pass\nsecret-pass\n',
        )

        assert result.exit_code == 0
        mock_secret_store.save_secret.assert_called_once_with(
            'source-secret',
            {'username': 'admin', 'password': 'secret-pass'},
        )
        mock_validate.assert_not_called()
        assert 'Secret created: source-secret' in result.output
