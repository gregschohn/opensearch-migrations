import argparse
import ast
from dataclasses import dataclass, field
import datetime
from k8s_service import K8sService, HelmCommandFailed
import logging
import random
import string
import sys
from tabulate import tabulate
from typing import List, Optional

logging.basicConfig(format='%(asctime)s [%(levelname)s] %(message)s', level=logging.INFO)
logger = logging.getLogger(__name__)

SOURCE_RELEASE_NAME = "source"
TARGET_RELEASE_NAME = "target"
MA_RELEASE_NAME = "ma"


# Data classes to represent test output generated by python e2e tests
@dataclass
class TestEntry:
    name: str
    description: str
    result: str
    duration: float
    error: Optional[str] = None


@dataclass
class TestSummary:
    passed: int
    failed: int
    source_version: str
    target_version: str


@dataclass
class TestReport:
    summary: TestSummary
    tests: List[TestEntry] = field(default_factory=list)


class TestsFailed(Exception):
    pass


class TestClusterEnvironment:
    def __init__(self, source_version: str,
                 source_helm_values_path: str,
                 source_chart_path: str,
                 target_version: str,
                 target_helm_values_path: str,
                 target_chart_path: str) -> None:

        self.source_version = source_version
        self.source_helm_values_path = source_helm_values_path
        self.source_chart_path = source_chart_path
        self.target_version = target_version
        self.target_helm_values_path = target_helm_values_path
        self.target_chart_path = target_chart_path


class TestRunner:

    def __init__(self, k8s_service: K8sService, unique_id: str, test_ids: List[str], ma_chart_path: str,
                 ma_chart_values_path: str, helm_dependency_script_path: str,
                 test_cluster_environments: [TestClusterEnvironment]) -> None:
        self.k8s_service = k8s_service
        self.unique_id = unique_id
        self.test_ids = test_ids
        self.ma_chart_path = ma_chart_path
        self.ma_chart_values_path = ma_chart_values_path
        self.helm_dependency_script_path = helm_dependency_script_path
        self.test_cluster_environments = test_cluster_environments

    def _print_test_stats(self, report: TestReport) -> None:
        for test in report.tests:
            print(f"{test.name}:")
            print(f"  - result: {test.result}")
            print(f"  - duration: {test.duration:.5f} seconds")
            if test.error:
                print(f"  - error: {test.error}")
            print()

    def _print_summary_table(self, reports: List[TestReport]) -> None:
        all_test_names = sorted({test.name for report in reports for test in report.tests})

        # Build the test matrix rows
        matrix_rows = []
        for report in reports:
            version_label = f"{report.summary.source_version} -> {report.summary.target_version}"
            row = [version_label]
            test_results = {test.name: "✓" if test.result == "passed" else "X" for test in report.tests}
            for name in all_test_names:
                row.append(test_results.get(name, ""))
            matrix_rows.append(row)

        # Build test description rows
        test_descriptions = {}
        for report in reports:
            for test in report.tests:
                test_descriptions.setdefault(test.name, test.description)

        # Print Test Matrix
        headers = ["Version"] + all_test_names
        print("\nTest Matrix:")
        print(tabulate(matrix_rows, headers=headers, tablefmt="fancy_grid"))

        # Print Test Case Information
        description_table = [[name, test_descriptions[name]] for name in all_test_names]
        print("\nTest Case Information:")
        print(tabulate(description_table, headers=["Test Name", "Description"], tablefmt="fancy_grid"))

        # Print Test Stats
        print("\nTest Stats:")
        for report in reports:
            print(f"===== {report.summary.source_version} -> {report.summary.target_version} =====")
            self._print_test_stats(report)

    def _parse_test_report(self, data: dict) -> TestReport:
        tests = [TestEntry(**test) for test in data.get("tests", [])]
        summary = TestSummary(**data.get("summary"))
        return TestReport(tests=tests, summary=summary)

    def run_tests(self) -> bool:
        """Runs pytest tests."""
        logger.info(f"Executing migration test cases with pytest and test ID filters: {self.test_ids}")
        self.k8s_service.exec_migration_console_cmd(["pipenv",
                                                     "run",
                                                     "pytest",
                                                     "/root/lib/integ_test/integ_test/ma_workflow_test.py",
                                                     f"--unique_id={self.unique_id}",
                                                     f"--test_ids={','.join(self.test_ids)}"])
        output_file_path = f"/root/lib/integ_test/results/{self.unique_id}/test_report.json"
        logger.info(f"Retrieving test report at {output_file_path}")
        cmd_response = self.k8s_service.exec_migration_console_cmd(command_list=["cat", output_file_path],
                                                                   unbuffered=False)
        test_data = ast.literal_eval(cmd_response)
        logger.debug(f"Received the following test data: {test_data}")
        test_report = self._parse_test_report(test_data)
        print(f"Test cases passed: {test_report.summary.passed}")
        print(f"Test cases failed: {test_report.summary.failed}")
        self._print_summary_table(reports=[test_report])
        if test_report.summary.passed == 0 or test_report.summary.failed > 0:
            return False
        return True

    def cleanup_deployment(self) -> None:
        self.k8s_service.helm_uninstall(release_name=SOURCE_RELEASE_NAME)
        self.k8s_service.helm_uninstall(release_name=TARGET_RELEASE_NAME)
        self.k8s_service.helm_uninstall(release_name=MA_RELEASE_NAME)
        self.k8s_service.wait_for_all_healthy_pods()
        self.k8s_service.delete_all_pvcs()

    def run(self, skip_delete: bool = False) -> None:
        self.k8s_service.helm_dependency_update(script_path=self.helm_dependency_script_path)
        for clusters in self.test_cluster_environments:
            try:
                logger.info(f"Performing helm deployment for migration testing environment "
                            f"from {clusters.source_version} to {clusters.target_version}")

                if not self.k8s_service.helm_install(chart_path=self.ma_chart_path, release_name=MA_RELEASE_NAME,
                                                     values_file=self.ma_chart_values_path):
                    raise HelmCommandFailed("Helm install of Migrations Assistant chart failed")

                if not self.k8s_service.helm_install(chart_path=clusters.source_chart_path,
                                                     release_name=SOURCE_RELEASE_NAME,
                                                     values_file=clusters.source_helm_values_path):
                    raise HelmCommandFailed("Helm install of source cluster chart failed")

                if not self.k8s_service.helm_install(chart_path=clusters.target_chart_path,
                                                     release_name=TARGET_RELEASE_NAME,
                                                     values_file=clusters.target_helm_values_path):
                    raise HelmCommandFailed("Helm install of target cluster chart failed")

                self.k8s_service.wait_for_all_healthy_pods()

                tests_passed = self.run_tests()

                if not tests_passed:
                    raise TestsFailed(f"Tests failed (or no tests executed) for upgrade "
                                      f"from {clusters.source_version} to {clusters.target_version}.")
                else:
                    logger.info(f"Tests passed successfully for upgrade "
                                f"from {clusters.source_version} to {clusters.target_version}.")
            except HelmCommandFailed as helmError:
                logger.error(f"Helm command failed with error: {helmError}. Testing may be incomplete")
            except TimeoutError as timeoutError:
                logger.error(f"Timeout error encountered: {timeoutError}. Testing may be incomplete")

            if not skip_delete:
                self.cleanup_deployment()

        logger.info("Test execution completed.")


def _parse_test_ids(test_ids_str: str) -> List[str]:
    # Split the string by commas and remove extra whitespace
    return [tid.strip() for tid in test_ids_str.split(",") if tid.strip()]


def _generate_unique_id() -> str:
    """Generate a human-readable unique ID with a timestamp and a 4-character random string."""
    timestamp = datetime.datetime.now().strftime("%Y%m%d%H%M%S")
    random_part = ''.join(random.choices(string.ascii_lowercase + string.digits, k=4))
    return f"{random_part}-{timestamp}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Process inputs for test automation runner"
    )
    parser.add_argument(
        "--skip-delete",
        action="store_true",
        help="If set, skip deletion operations."
    )
    parser.add_argument(
        "--delete-only",
        action="store_true",
        help="If set, only perform deletion operations."
    )
    parser.add_argument(
        '--unique-id',
        type=str,
        default=_generate_unique_id(),
        help="Provide a unique ID for labeling test resources, or generate one by default"
    )
    parser.add_argument(
        "--test-ids",
        type=_parse_test_ids,
        default=[],
        help="Comma-separated list of test IDs to run (e.g. 0001,0003)"
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    k8s_service = K8sService()
    helm_k8s_base_path = "../../deployment/k8s"
    helm_dependency_script_path = f"{helm_k8s_base_path}/update_deps.sh"
    helm_charts_base_path = f"{helm_k8s_base_path}/charts"
    ma_chart_path = f"{helm_charts_base_path}/aggregates/migrationAssistant"
    elasticsearch_cluster_chart_path = f"{helm_charts_base_path}/components/elasticsearchCluster"
    opensearch_cluster_chart_path = f"{helm_charts_base_path}/components/opensearchCluster"

    # Currently utilizes a single test cluster environment, but should be expanded to allow a matrix of cases based
    # on provided source and target version
    ma_chart_values_path = "es-5-values.yaml"
    es_5_6_values = (f"{helm_charts_base_path}/components/elasticsearchCluster/"
                     f"environments/es-5-6-single-node-cluster.yaml")
    os_2_19_values = (f"{helm_charts_base_path}/components/opensearchCluster/"
                      f"environments/os-2-latest-single-node-cluster.yaml")
    test_cluster_env = TestClusterEnvironment(source_version="ES_5.6",
                                              source_helm_values_path=es_5_6_values,
                                              source_chart_path=elasticsearch_cluster_chart_path,
                                              target_version="OS_2.19",
                                              target_helm_values_path=os_2_19_values,
                                              target_chart_path=opensearch_cluster_chart_path)

    test_runner = TestRunner(k8s_service=k8s_service,
                             unique_id=args.unique_id,
                             test_ids=args.test_ids,
                             ma_chart_path=ma_chart_path,
                             ma_chart_values_path=ma_chart_values_path,
                             helm_dependency_script_path=helm_dependency_script_path,
                             test_cluster_environments=[test_cluster_env])

    if args.delete_only:
        return test_runner.cleanup_deployment()
    test_runner.run(skip_delete=args.skip_delete)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        # Handle Ctrl+C cleanly too
        sys.exit(0)
    except Exception as e:
        logger.error(f"Fatal error: {e}")
        sys.exit(1)
