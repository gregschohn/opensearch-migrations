from dataclasses import replace

from console_link.workflow.application.config_drafts import ConfigDraft
from console_link.workflow.application.config_navigation import (
    group_snapshot_migration_navigation,
    project_config_navigation,
)
from console_link.workflow.application.manage_state import ManageStateService
from console_link.workflow.application.models import (
    ManageCapability,
    ManageNode,
    ManageSnapshot,
)
from console_link.workflow.resource_tree import _build_tree_from_raw


def _collection(
    path,
    *,
    section_id,
    section_label,
    section_order,
    group_id,
    group_label,
    group_order,
    plural,
    resource_type,
    children,
    identity=None,
    parent_group=None,
    draft_change_count=None,
):
    result = {
        "id": f"edit:{'.'.join(path)}",
        "path": path,
        "label": group_label,
        "valueKind": "record",
        "inputHint": {
            "kind": "record",
            "resourceCollection": {
                "navigation": {
                    "sectionId": section_id,
                    "sectionLabel": section_label,
                    "sectionOrder": section_order,
                    "groupId": group_id,
                    "groupLabel": group_label,
                    "groupOrder": group_order,
                    **(
                        {
                            "parentGroupId": parent_group["id"],
                            "parentGroupLabel": parent_group["label"],
                            "parentGroupOrder": parent_group["order"],
                        }
                        if parent_group else {}
                    ),
                },
                "resource": {
                    "kind": resource_type.replace(" ", ""),
                    "plural": plural,
                    "typeLabel": resource_type,
                    "identity": identity or {"kind": "named"},
                },
            },
        },
        "status": "ok",
        "diagnostics": [],
        "children": children,
    }
    if draft_change_count is not None:
        result["draftChangeCount"] = draft_change_count
    return result


def _resource_edit(
    path,
    *,
    status="ok",
    implicit=False,
    status_counts=None,
    draft_change_count=None,
    diagnostics=None,
    children=None,
):
    result = {
        "id": f"edit:{'.'.join(path)}",
        "path": path,
        "label": path[-1],
        "valueKind": "object",
        "status": status,
        "diagnostics": diagnostics or [],
        "children": children or [],
    }
    if status_counts is not None:
        result["statusCounts"] = status_counts
    if implicit:
        result["implicit"] = True
    if draft_change_count is not None:
        result["draftChangeCount"] = draft_change_count
    return result


def _definition_collection(
    path,
    *,
    owner_ancestor_levels,
    group_label,
    group_order,
    type_label,
    children,
):
    group_id = f"definition-group:edit:{'.'.join(path)}"
    return {
        "id": f"edit:{'.'.join(path)}",
        "path": path,
        "label": group_label,
        "valueKind": "record",
        "inputHint": {
            "kind": "record",
            "definitionCollection": {
                "ownerAncestorLevels": owner_ancestor_levels,
                "navigation": {
                    "groupLabel": group_label,
                    "groupOrder": group_order,
                    "groupId": group_id,
                },
                "definition": {
                    "typeLabel": type_label,
                },
            },
        },
        "status": "ok",
        "diagnostics": [],
        "children": children,
    }


def _draft(nodes, *, dirty=True, mode="structured"):
    return ConfigDraft(
        base_revision="base-1",
        draft_revision="draft-2",
        dirty=dirty,
        edit_state={
            "formatVersion": 1,
            "provenance": {
                "source": "pending-yaml",
                "lossy": mode == "raw",
                "mode": mode,
                "warnings": [],
            },
            "nodes": nodes,
            "validation": {"valid": True, "errors": [], "diagnostics": []},
        },
    )


def _runtime_snapshot():
    source_id = "resource:sourceconfigs:legacy"
    step_id = f"workflow-step:{source_id}:apply"
    source = ManageNode(
        id=source_id,
        revision="source-1",
        parent_id="group:Sources:Sources",
        child_ids=(step_id,),
        kind="resource",
        label="legacy",
        status="ok",
        capabilities=(
            ManageCapability(
                kind="edit",
                target_id="edit:sourceClusters.legacy",
                label="Edit legacy",
            ),
        ),
        resource_plural="sourceconfigs",
        resource_name="legacy",
        resource_type="Source cluster",
        config_presence={"deployed": True, "pending": True},
    )
    step = ManageNode(
        id=step_id,
        revision="step-1",
        parent_id=source_id,
        kind="workflow-step",
        label="Apply",
        status="ok",
    )
    group = ManageNode(
        id="group:Sources:Sources",
        revision="sources-group-1",
        parent_id="section:Sources",
        child_ids=(source_id,),
        kind="group",
        label="Sources",
        status="ok",
    )
    section = ManageNode(
        id="section:Sources",
        revision="sources-section-1",
        child_ids=(group.id,),
        kind="section",
        label="Sources",
        status="ok",
    )
    return ManageSnapshot(
        format_version=1,
        revision="runtime-1",
        observed_at="2026-08-23T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(section.id,),
        nodes={
            section.id: section,
            group.id: group,
            source.id: source,
            step.id: step,
        },
    )


def test_project_config_navigation_uses_collection_hints_for_draft_resources():
    sources = _collection(
        ["sourceClusters"],
        section_id="section:Sources",
        section_label="Sources",
        section_order=0,
        group_id="group:Sources:Sources",
        group_label="Sources",
        group_order=0,
        plural="sourceconfigs",
        resource_type="Source cluster",
        draft_change_count=1,
        children=[
            _resource_edit(
                ["sourceClusters", "modern"],
                status="error",
                status_counts={"errors": 2, "warnings": 1},
                draft_change_count=3,
                diagnostics=[{
                    "severity": "error",
                    "message": "Endpoint is required.",
                    "path": ["sourceClusters", "modern", "endpoint"],
                }],
            ),
        ],
    )
    targets = _collection(
        ["targetClusters"],
        section_id="section:Targets",
        section_label="Targets",
        section_order=1,
        group_id="group:Targets:Targets",
        group_label="Targets",
        group_order=0,
        plural="targetconfigs",
        resource_type="Target cluster",
        children=[_resource_edit(["targetClusters", "target"])],
    )

    projected = project_config_navigation(
        _runtime_snapshot(),
        _draft([sources, targets]),
    )

    assert projected.root_ids == ("section:Sources", "section:Targets")
    assert "workflow-step:resource:sourceconfigs:legacy:apply" not in projected.nodes
    assert projected.nodes["resource:sourceconfigs:legacy"].status == "removed"
    assert (
        projected.nodes["resource:sourceconfigs:legacy"].value_summary
        == "Marked for removal"
    )
    assert projected.nodes["resource:sourceconfigs:legacy"].child_ids == ()

    modern = projected.nodes["resource:sourceconfigs:modern"]
    assert modern.parent_id == "group:Sources:Sources"
    assert modern.resource_type == "Source cluster"
    assert modern.status == "error"
    assert modern.config_presence == {"deployed": False, "pending": True}
    assert modern.capabilities[0].target_id == "edit:sourceClusters.modern"
    assert modern.diagnostics[0].message == "Endpoint is required."
    assert modern.config_state is not None
    assert modern.config_state.validation_errors == 2
    assert modern.config_state.validation_warnings == 1
    assert modern.config_state.draft_change_count == 3

    target = projected.nodes["resource:targetconfigs:target"]
    assert target.parent_id == "group:Targets:Targets"
    assert projected.nodes["section:Targets"].child_ids == (
        "group:Targets:Targets",
    )
    assert projected.nodes["section:Targets"].capabilities[0].target_id == (
        "edit:targetClusters"
    )
    assert projected.nodes["group:Targets:Targets"].capabilities[0].target_id == (
        "edit:targetClusters"
    )


def test_project_config_navigation_keeps_generated_pending_resource():
    snapshot = _runtime_snapshot()
    generated = ManageNode(
        id="resource:kafkaclusters:default",
        revision="kafka-1",
        parent_id=(
            "group:Live Traffic Migration:Buffer:Kafka Clusters"
        ),
        kind="resource",
        label="default",
        status="ok",
        capabilities=(
            ManageCapability(
                kind="edit",
                target_id="edit:traffic.kafkaClusters.default",
                label="Edit default",
            ),
        ),
        resource_plural="kafkaclusters",
        resource_name="default",
        resource_type="Kafka cluster",
        config_presence={
            "deployed": True,
            "submitted": True,
            "pending": True,
        },
    )
    snapshot = replace(
        snapshot,
        root_ids=(
            *snapshot.root_ids,
            "section:Live Traffic Migration",
        ),
        nodes={
            **snapshot.nodes,
            "section:Live Traffic Migration": ManageNode(
                id="section:Live Traffic Migration",
                revision="traffic-section-1",
                child_ids=("group:Live Traffic Migration:Buffer",),
                kind="section",
                label="Live Traffic Migration",
                status="ok",
            ),
            "group:Live Traffic Migration:Buffer": ManageNode(
                id="group:Live Traffic Migration:Buffer",
                revision="buffer-group-1",
                parent_id="section:Live Traffic Migration",
                child_ids=(
                    "group:Live Traffic Migration:Buffer:Kafka Clusters",
                ),
                kind="group",
                label="Buffer",
                status="ok",
            ),
            (
                "group:Live Traffic Migration:Buffer:Kafka Clusters"
            ): ManageNode(
                id=(
                    "group:Live Traffic Migration:Buffer:Kafka Clusters"
                ),
                revision="kafka-group-1",
                parent_id="group:Live Traffic Migration:Buffer",
                child_ids=(generated.id,),
                kind="group",
                label="Kafka Clusters",
                status="ok",
            ),
            generated.id: generated,
        },
    )
    kafka = _collection(
        ["traffic", "kafkaClusters"],
        section_id="section:Live Traffic Migration",
        section_label="Live Traffic Migration",
        section_order=3,
        group_id=(
            "group:Live Traffic Migration:Buffer:Kafka Clusters"
        ),
        group_label="Kafka Clusters",
        group_order=0,
        parent_group={
            "id": "group:Live Traffic Migration:Buffer",
            "label": "Buffer",
            "order": 0,
        },
        plural="kafkaclusters",
        resource_type="Kafka cluster",
        children=[],
    )

    projected = project_config_navigation(
        snapshot,
        _draft([kafka], dirty=True),
    )

    node = projected.nodes[generated.id]
    assert node.status == "ok"
    assert node.value_summary is None
    assert node.config_presence == {
        "deployed": True,
        "submitted": True,
        "pending": True,
    }


def test_project_config_navigation_exposes_implicit_default_without_addition():
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime-empty",
        observed_at="2026-08-23T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(),
        nodes={},
    )
    kafka = _collection(
        ["traffic", "kafkaClusters"],
        section_id="section:Live Traffic Migration",
        section_label="Live Traffic Migration",
        section_order=3,
        group_id=(
            "group:Live Traffic Migration:Buffer:Kafka Clusters"
        ),
        group_label="Kafka Clusters",
        group_order=0,
        parent_group={
            "id": "group:Live Traffic Migration:Buffer",
            "label": "Buffer",
            "order": 0,
        },
        plural="kafkaclusters",
        resource_type="Kafka cluster",
        children=[
            _resource_edit(
                ["traffic", "kafkaClusters", "default"],
                implicit=True,
            ),
        ],
    )

    projected = project_config_navigation(
        snapshot,
        _draft([kafka], dirty=False),
    )

    node = projected.nodes["resource:kafkaclusters:default"]
    assert node.status == "ok"
    assert node.phase == "Implicit default"
    assert node.value_summary == "Available when referenced"
    assert node.config_presence == {
        "deployed": False,
        "pending": False,
    }
    assert node.capabilities[0].target_id == (
        "edit:traffic.kafkaClusters.default"
    )


def test_project_config_navigation_builds_nested_resource_groups():
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime-empty",
        observed_at="2026-08-23T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(),
        nodes={},
    )
    kafka = _collection(
        ["traffic", "kafkaClusters"],
        section_id="section:Live Traffic Migration",
        section_label="Live Traffic Migration",
        section_order=3,
        group_id=(
            "group:Live Traffic Migration:Buffer:Kafka Clusters"
        ),
        group_label="Kafka Clusters",
        group_order=0,
        parent_group={
            "id": "group:Live Traffic Migration:Buffer",
            "label": "Buffer",
            "order": 0,
        },
        plural="kafkaclusters",
        resource_type="Kafka cluster",
        children=[],
    )
    topics = _collection(
        ["traffic", "s3Sources"],
        section_id="section:Live Traffic Migration",
        section_label="Live Traffic Migration",
        section_order=3,
        group_id="group:Live Traffic Migration:Buffer:Kafka Topics",
        group_label="Kafka Topics",
        group_order=1,
        parent_group={
            "id": "group:Live Traffic Migration:Buffer",
            "label": "Buffer",
            "order": 0,
        },
        plural="capturedtraffics",
        resource_type="Kafka topic",
        children=[],
    )

    projected = project_config_navigation(
        snapshot,
        _draft([kafka, topics], dirty=False),
    )

    section = projected.nodes["section:Live Traffic Migration"]
    assert section.child_ids == ("group:Live Traffic Migration:Buffer",)
    buffer = projected.nodes["group:Live Traffic Migration:Buffer"]
    assert buffer.child_ids == (
        "group:Live Traffic Migration:Buffer:Kafka Clusters",
        "group:Live Traffic Migration:Buffer:Kafka Topics",
    )


def test_project_config_navigation_associates_existing_resource_edit_state():
    sources = _collection(
        ["sourceClusters"],
        section_id="section:Sources",
        section_label="Sources",
        section_order=0,
        group_id="group:Sources:Sources",
        group_label="Sources",
        group_order=0,
        plural="sourceconfigs",
        resource_type="Source cluster",
        children=[
            _resource_edit(
                ["sourceClusters", "legacy"],
                status_counts={"warnings": 1},
                draft_change_count=2,
            ),
        ],
    )

    projected = project_config_navigation(
        _runtime_snapshot(),
        _draft([sources]),
    )

    legacy = projected.nodes["resource:sourceconfigs:legacy"]
    assert legacy.config_state is not None
    assert legacy.config_state.validation_errors == 0
    assert legacy.config_state.validation_warnings == 1
    assert legacy.config_state.draft_change_count == 2


def test_project_config_navigation_places_nested_definitions_under_their_owner():
    repos = _definition_collection(
        ["sourceClusters", "legacy", "snapshotInfo", "repos"],
        owner_ancestor_levels=2,
        group_label="Repositories",
        group_order=0,
        type_label="Snapshot repository",
        children=[
            _resource_edit(
                [
                    "sourceClusters",
                    "legacy",
                    "snapshotInfo",
                    "repos",
                    "repo1",
                ],
                draft_change_count=1,
            ),
        ],
    )
    snapshots = _definition_collection(
        ["sourceClusters", "legacy", "snapshotInfo", "snapshots"],
        owner_ancestor_levels=2,
        group_label="Snapshots",
        group_order=1,
        type_label="Source snapshot",
        children=[
            _resource_edit(
                [
                    "sourceClusters",
                    "legacy",
                    "snapshotInfo",
                    "snapshots",
                    "snap1",
                ],
            ),
        ],
    )
    source = _resource_edit(
        ["sourceClusters", "legacy"],
        children=[
            _resource_edit(
                ["sourceClusters", "legacy", "snapshotInfo"],
                children=[repos, snapshots],
            ),
        ],
    )
    sources = _collection(
        ["sourceClusters"],
        section_id="section:Sources",
        section_label="Sources",
        section_order=0,
        group_id="group:Sources:Sources",
        group_label="Sources",
        group_order=0,
        plural="sourceconfigs",
        resource_type="Source cluster",
        children=[source],
    )

    projected = project_config_navigation(
        _runtime_snapshot(),
        _draft([sources]),
    )

    source_node = projected.nodes["resource:sourceconfigs:legacy"]
    assert source_node.child_ids == (
        "definition-group:edit:sourceClusters.legacy.snapshotInfo.repos",
        "definition-group:edit:sourceClusters.legacy.snapshotInfo.snapshots",
    )
    repo_group = projected.nodes[source_node.child_ids[0]]
    assert repo_group.id == (
        "definition-group:edit:"
        "sourceClusters.legacy.snapshotInfo.repos"
    )
    assert repo_group.label == "Repositories"
    repo = projected.nodes[repo_group.child_ids[0]]
    assert repo.kind == "config-definition"
    assert repo.label == "repo1"
    assert repo.resource_type == "Snapshot repository"
    assert repo.capabilities[0].target_id == (
        "edit:sourceClusters.legacy.snapshotInfo.repos.repo1"
    )
    assert repo.config_state is not None
    assert repo.config_state.draft_change_count == 1

    snapshot_group = projected.nodes[source_node.child_ids[1]]
    assert snapshot_group.label == "Snapshots"
    snapshot = projected.nodes[snapshot_group.child_ids[0]]
    assert snapshot.label == "snap1"
    assert snapshot.capabilities[0].target_id == (
        "edit:sourceClusters.legacy.snapshotInfo.snapshots.snap1"
    )


def test_project_config_navigation_aggregates_nested_validation_fallbacks():
    sources = _collection(
        ["sourceClusters"],
        section_id="section:Sources",
        section_label="Sources",
        section_order=0,
        group_id="group:Sources:Sources",
        group_label="Sources",
        group_order=0,
        plural="sourceconfigs",
        resource_type="Source cluster",
        children=[
            _resource_edit(
                ["sourceClusters", "legacy"],
                children=[
                    _resource_edit(
                        ["sourceClusters", "legacy", "endpoint"],
                        status="error",
                    ),
                    _resource_edit(
                        ["sourceClusters", "legacy", "auth"],
                        status="warning",
                    ),
                ],
            ),
        ],
    )

    projected = project_config_navigation(
        _runtime_snapshot(),
        _draft([sources], dirty=False),
    )

    config_state = projected.nodes[
        "resource:sourceconfigs:legacy"
    ].config_state
    assert config_state is not None
    assert config_state.validation_errors == 1
    assert config_state.validation_warnings == 1
    assert config_state.draft_change_count == 0


def test_project_config_navigation_preserves_explicit_pending_removal():
    snapshot = _runtime_snapshot()
    source = snapshot.nodes["resource:sourceconfigs:legacy"]
    snapshot = replace(
        snapshot,
        nodes={
            **snapshot.nodes,
            source.id: replace(
                source,
                config_presence={
                    "deployed": True,
                    "pending": False,
                },
            ),
        },
    )
    sources = _collection(
        ["sourceClusters"],
        section_id="section:Sources",
        section_label="Sources",
        section_order=0,
        group_id="group:Sources:Sources",
        group_label="Sources",
        group_order=0,
        plural="sourceconfigs",
        resource_type="Source cluster",
        children=[_resource_edit(["sourceClusters", "legacy"])],
    )

    projected = project_config_navigation(snapshot, _draft([sources], dirty=False))

    removed = projected.nodes["resource:sourceconfigs:legacy"]
    assert removed.status == "removed"
    assert removed.value_summary == "Removal pending submission"


def test_project_config_navigation_owns_flat_snapshot_migrations():
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime-empty",
        observed_at="2026-08-23T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(),
        nodes={},
    )
    migration_tuple = _resource_edit(
        ["snapshotMigrationConfigs", "0"],
    )
    migration_tuple["value"] = {
        "fromSource": "source",
        "toTarget": "target",
        "fromSnapshot": "snap",
        "slice": "slice-0",
        "metadataMigrationConfig": {},
    }
    migrations = _collection(
        ["snapshotMigrationConfigs"],
        section_id="section:Snapshot Migration",
        section_label="Snapshot Migration",
        section_order=2,
        group_id="group:Snapshot Migration:Backfill",
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        identity={
            "kind": "indexed-config",
            "prefix": "slice-",
            "firstIndex": 0,
        },
        children=[
            migration_tuple,
            {
                "id": "edit:snapshotMigrationConfigs:add",
                "path": ["snapshotMigrationConfigs"],
                "label": "+ Add snapshot migration",
                "valueKind": "command",
                "status": "ok",
                "diagnostics": [],
                "children": [],
            },
        ],
    )
    archives = _collection(
        ["traffic", "s3Sources"],
        section_id="section:Live Traffic Migration",
        section_label="Live Traffic Migration",
        section_order=3,
        group_id="group:Live Traffic Migration:Buffer",
        group_label="Buffer",
        group_order=0,
        plural="capturedtraffics",
        resource_type="S3 source",
        identity={"kind": "named", "suffix": "-topic"},
        children=[_resource_edit(["traffic", "s3Sources", "archive"])],
    )

    projected = project_config_navigation(
        snapshot,
        _draft([migrations, archives]),
    )

    migration = projected.nodes["config:snapshotMigrationConfigs:0"]
    assert migration.label == "source-target-snap-slice-0"
    assert migration.parent_id == "section:Snapshot Migration"
    assert migration.capabilities[0].target_id == (
        "edit:snapshotMigrationConfigs.0"
    )
    assert "group:Snapshot Migration:Backfill" not in projected.nodes
    assert projected.nodes["section:Snapshot Migration"].child_ids == (
        migration.id,
    )
    assert "config:snapshotMigrationConfigs:add" not in projected.nodes
    archive = projected.nodes["resource:capturedtraffics:archive-topic"]
    assert archive.label == "archive-topic"
    assert archive.resource_type == "S3 source"


def test_project_config_navigation_keeps_duplicate_snapshot_migrations_separate():
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime-empty",
        observed_at="2026-09-10T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(),
        nodes={},
    )
    migrations = []
    for index in range(2):
        migration = _resource_edit(
            ["snapshotMigrationConfigs", str(index)],
            status="error",
            diagnostics=[{
                "severity": "error",
                "message": "Snapshot migration is already configured.",
                "path": ["snapshotMigrationConfigs", str(index), "slice"],
            }],
        )
        migration["value"] = {
            "fromSource": "source",
            "toTarget": "target",
            "fromSnapshot": "snap",
            "slice": "slice-0",
        }
        migrations.append(migration)
    collection = _collection(
        ["snapshotMigrationConfigs"],
        section_id="section:Snapshot Migration",
        section_label="Snapshot Migration",
        section_order=2,
        group_id="group:Snapshot Migration:Backfill",
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        children=migrations,
    )

    projected = project_config_navigation(snapshot, _draft([collection]))

    section = projected.nodes["section:Snapshot Migration"]
    assert section.child_ids == (
        "config:snapshotMigrationConfigs:0",
        "config:snapshotMigrationConfigs:1",
    )
    assert projected.nodes["config:snapshotMigrationConfigs:0"].status == "error"
    assert projected.nodes["config:snapshotMigrationConfigs:1"].status == "error"
    assert all(
        projected.nodes[node_id].label == "source-target-snap-slice-0"
        for node_id in section.child_ids
    )


def test_project_config_navigation_keeps_named_incomplete_snapshot_migration():
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime-empty",
        observed_at="2026-09-10T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(),
        nodes={},
    )
    migration = _resource_edit(
        ["snapshotMigrationConfigs", "0"],
        status="required",
        diagnostics=[{
            "severity": "required",
            "message": "Choose a source, target, and snapshot.",
            "path": ["snapshotMigrationConfigs", "0"],
        }],
    )
    migration["value"] = {"slice": "slice-0"}
    collection = _collection(
        ["snapshotMigrationConfigs"],
        section_id="section:Snapshot Migration",
        section_label="Snapshot Migration",
        section_order=2,
        group_id="group:Snapshot Migration:Backfill",
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        children=[migration],
    )

    projected = project_config_navigation(snapshot, _draft([collection]))

    item = projected.nodes["config:snapshotMigrationConfigs:0"]
    assert item.parent_id == "section:Snapshot Migration"
    assert item.label == "<SOURCE>-<TARGET>-<SNAPSHOT>-slice-0"
    assert item.status == "required"
    assert item.capabilities[0].target_id == "edit:snapshotMigrationConfigs.0"


def test_snapshot_migration_navigation_groups_only_useful_prefixes():
    group_id = "group:Snapshot Migration:Backfill"
    resources = (
        ("a", ("source", "target-a", "snap-a", "slice-10"), "warning"),
        ("b", ("source", "target-a", "snap-a", "slice-2"), "ok"),
        ("c", ("source", "target-a", "snap-b", "slice-3"), "error"),
        ("d", ("source", "target-b", "snap-c", "slice-4"), "ok"),
    )
    nodes = {
        group_id: ManageNode(
            id=group_id,
            revision="r",
            parent_id="section:Snapshot Migration",
            kind="group",
            label="Backfill",
            status="ok",
        ),
    }
    for suffix, navigation_key, status in resources:
        node_id = f"resource:snapshotmigrations:{suffix}"
        nodes[node_id] = ManageNode(
            id=node_id,
            revision="r",
            parent_id=group_id,
            kind="resource",
            label=suffix,
            status=status,
            resource_plural="snapshotmigrations",
            resource_name=suffix,
            resource_type="Snapshot migration",
            navigation_key=navigation_key,
        )
    nodes[group_id] = replace(
        nodes[group_id],
        child_ids=tuple(
            f"resource:snapshotmigrations:{suffix}"
            for suffix, _, _ in resources
        ),
    )
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime",
        observed_at="2026-09-09T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(group_id,),
        nodes=nodes,
    )

    grouped = group_snapshot_migration_navigation(snapshot)

    source_group = grouped.nodes["snapshot-navigation:1:source"]
    target_group = grouped.nodes[
        "snapshot-navigation:2:source:target-a"
    ]
    assert source_group.label == "source"
    assert source_group.status == "error"
    assert target_group.label == "target-a"
    assert target_group.status == "error"
    assert target_group.child_ids == (
        "resource:snapshotmigrations:b",
        "resource:snapshotmigrations:a",
        "resource:snapshotmigrations:c",
    )
    assert "snapshot-navigation:3:source:target-a:snap-a" not in grouped.nodes
    assert grouped.nodes["resource:snapshotmigrations:d"].parent_id == source_group.id
    assert grouped.nodes["resource:snapshotmigrations:d"].label == (
        "target-b-snap-c-slice-4"
    )
    assert grouped.nodes["resource:snapshotmigrations:b"].label == (
        "snap-a-slice-2"
    )


def test_snapshot_migration_deletion_tracks_tuple_after_array_compaction():
    section_id = "section:Snapshot Migration"
    group_id = "group:Snapshot Migration:Backfill"
    slice_zero_id = (
        "resource:snapshotmigrations:source-target-snap-slice-0"
    )
    slice_one_id = (
        "resource:snapshotmigrations:source-target-snap-slice-1"
    )
    nodes = {
        section_id: ManageNode(
            id=section_id,
            revision="section",
            parent_id=None,
            kind="section",
            label="Snapshot Migration",
            status="ok",
            child_ids=(group_id,),
        ),
        group_id: ManageNode(
            id=group_id,
            revision="group",
            parent_id=section_id,
            kind="group",
            label="Backfill",
            status="ok",
            child_ids=(slice_zero_id, slice_one_id),
        ),
    }
    for index, node_id in enumerate((slice_zero_id, slice_one_id)):
        slice_name = f"slice-{index}"
        nodes[node_id] = ManageNode(
            id=node_id,
            revision=f"slice-{index}",
            parent_id=group_id,
            kind="resource",
            label=node_id.rsplit(":", 1)[-1],
            status="ok",
            capabilities=(
                ManageCapability(
                    kind="edit",
                    target_id=f"edit:snapshotMigrationConfigs.{index}",
                    label=f"Edit {slice_name}",
                ),
            ),
            resource_plural="snapshotmigrations",
            resource_name=node_id.rsplit(":", 1)[-1],
            resource_type="Snapshot migration",
            config_presence={"deployed": True, "pending": True},
            navigation_key=(
                "source",
                "target",
                "snap",
                slice_name,
            ),
        )
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime",
        observed_at="2026-09-10T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(section_id,),
        nodes=nodes,
    )
    retained = _resource_edit(["snapshotMigrationConfigs", "0"])
    retained["value"] = {
        "fromSource": "source",
        "toTarget": "target",
        "fromSnapshot": "snap",
        "slice": "slice-1",
        "metadataMigrationConfig": {},
    }
    migrations = _collection(
        ["snapshotMigrationConfigs"],
        section_id=section_id,
        section_label="Snapshot Migration",
        section_order=2,
        group_id=group_id,
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        children=[retained],
        draft_change_count=1,
    )

    projected = project_config_navigation(snapshot, _draft([migrations]))

    removed = projected.nodes[slice_zero_id]
    retained_node = projected.nodes[slice_one_id]
    removed_edit_capabilities = tuple(
        capability
        for capability in removed.capabilities
        if capability.kind == "edit"
    )
    retained_edit_capabilities = tuple(
        capability
        for capability in retained_node.capabilities
        if capability.kind == "edit"
    )
    assert removed.status == "removed"
    assert removed_edit_capabilities == ()
    assert retained_node.status == "ok"
    assert retained_edit_capabilities[0].target_id == (
        "edit:snapshotMigrationConfigs.0"
    )
    assert "config:snapshotMigrationConfigs:0" not in projected.nodes


def _observe_pending_only_migrations(tuples):
    """Observe a snapshot whose snapshot migrations exist only in pending config.

    Each entry is a (source, target, snapshot, slice) identity; a bare string is
    shorthand for that slice of the default source/target/snapshot.
    """
    identities = [
        ("source", "target", "snap", entry) if isinstance(entry, str) else entry
        for entry in tuples
    ]
    resources = [
        {
            "kind": "SnapshotMigration",
            "name": f"{source}-{target}-{snapshot}-{slice_name}",
            "parameters": {
                "sourceLabel": source,
                "targetLabel": target,
                "snapshotLabel": snapshot,
                "migrationLabel": slice_name,
            },
            "parameterProvenance": {
                "migrationLabel": {
                    "path": ["migrationLabel"],
                    "presence": "authored",
                    "sourcePath": ["snapshotMigrationConfigs", str(index)],
                },
            },
        }
        for index, (source, target, snapshot, slice_name) in enumerate(identities)
    ]

    class _Argo:
        def get_workflow(self, name, namespace):
            return {"success": False, "error": "workflow not found"}, {}

    class _Config:
        def load_resource_config_snapshots(self, workflow_name):
            return {"pending": {"resources": resources}, "pending_console": {}}

    return ManageStateService(
        namespace="ma",
        workflow_name="migration-workflow",
        argo_service=_Argo(),
        resource_loader=lambda namespace: _build_tree_from_raw({}),
        config_service_provider=_Config,
    ).observe()


def test_deleting_a_config_only_snapshot_migration_strikes_that_entry():
    # Nothing is deployed yet, so identity has to survive the array compaction
    # that deleting an earlier entry causes. Matching on the draft index alone
    # struck whichever row happened to land on the vanished index.
    snapshot = _observe_pending_only_migrations(("slice-0", "slice-1"))
    retained = _resource_edit(["snapshotMigrationConfigs", "0"])
    retained["value"] = {
        "fromSource": "source",
        "toTarget": "target",
        "fromSnapshot": "snap",
        "slice": "slice-1",
        "metadataMigrationConfig": {},
    }
    migrations = _collection(
        ["snapshotMigrationConfigs"],
        section_id="section:Snapshot Migration",
        section_label="Snapshot Migration",
        section_order=2,
        group_id="group:Snapshot Migration:Backfill",
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        children=[retained],
        draft_change_count=1,
    )

    projected = project_config_navigation(snapshot, _draft([migrations]))

    deleted = projected.nodes["resource:snapshotmigrations:source-target-snap-slice-0"]
    retained_node = projected.nodes["resource:snapshotmigrations:source-target-snap-slice-1"]
    assert deleted.status == "removed"
    assert retained_node.status != "removed"
    assert [
        capability.target_id
        for capability in retained_node.capabilities
        if capability.kind == "edit"
    ] == ["edit:snapshotMigrationConfigs.0"]


def _snapshot_migration_draft(entries, *, change_count=1):
    children = []
    for index, entry in enumerate(entries):
        source, target, snapshot, slice_name = (
            ("source", "target", "snap", entry) if isinstance(entry, str) else entry
        )
        child = _resource_edit(["snapshotMigrationConfigs", str(index)])
        child["value"] = {
            "fromSource": source,
            "toTarget": target,
            "fromSnapshot": snapshot,
            "slice": slice_name,
            "metadataMigrationConfig": {},
        }
        children.append(child)
    return _draft([_collection(
        ["snapshotMigrationConfigs"],
        section_id="section:Snapshot Migration",
        section_label="Snapshot Migration",
        section_order=2,
        group_id="group:Snapshot Migration:Backfill",
        group_label="Backfill",
        group_order=1,
        plural="snapshotmigrations",
        resource_type="Snapshot migration",
        children=children,
        draft_change_count=change_count,
    )])


def _parent_counts(projected):
    """Count how many parents claim each node, the way the tree walk sees it."""
    counts = {}
    for node in projected.nodes.values():
        for child_id in node.child_ids:
            counts[child_id] = counts.get(child_id, 0) + 1
    return counts


def test_grouping_a_second_time_does_not_leave_leaves_under_two_parents():
    # Runtime observation groups once and the draft projection groups again. The
    # first pass flattens leaves onto the section, so the second pass has to
    # release the ones it re-homes into a group or the tree renders them twice.
    snapshot = _observe_pending_only_migrations((
        ("source", "target-a", "snap", "slice-1"),
        ("source", "target-b", "snap", "slice-2"),
    ))
    drafted = _snapshot_migration_draft((
        ("source", "target-a", "snap", "slice-1"),
        ("source", "target-b", "snap", "slice-2"),
        ("source", "target-c", "snap", "slice-3"),
    ))

    projected = project_config_navigation(snapshot, drafted)

    assert projected.nodes["snapshot-navigation:1:source"].kind == "group"
    assert [
        node_id for node_id, count in _parent_counts(projected).items() if count > 1
    ] == []


def test_a_removed_slice_does_not_mark_its_whole_group_removed():
    snapshot = _observe_pending_only_migrations(("slice-0", "slice-1", "slice-2"))
    renamed = _snapshot_migration_draft(("slice-0", "slice-9", "slice-2"))
    emptied = _snapshot_migration_draft(())

    renamed_group = project_config_navigation(snapshot, renamed).nodes[
        "snapshot-navigation:3:source:target:snap"
    ]
    assert renamed_group.status == "changed"

    all_removed = project_config_navigation(snapshot, emptied).nodes[
        "snapshot-navigation:3:source:target:snap"
    ]
    assert all_removed.status == "removed"


def test_sibling_snapshot_groups_are_labeled_by_where_they_landed():
    # Groups are built against the parent they were offered, not the one they get
    # when an intermediate level is dropped, so labels have to be recomputed or
    # two sources collapse to the same visible name.
    snapshot = _observe_pending_only_migrations(tuple(
        (source, "tgt", "nightly", f"slice-{index}")
        for source in ("src1", "src2")
        for index in range(3)
    ))

    projected = project_config_navigation(
        snapshot, _snapshot_migration_draft((), change_count=0),
    )

    labels = [
        projected.nodes[f"snapshot-navigation:3:{source}:tgt:nightly"].label
        for source in ("src1", "src2")
    ]
    assert labels == [
        "src1-tgt-nightly",
        "src2-tgt-nightly",
    ]


def test_clearing_a_slice_name_keeps_the_entry_reachable():
    snapshot = _observe_pending_only_migrations(("slice-0",))
    drafted = _snapshot_migration_draft((("source", "target", "snap", ""),))

    projected = project_config_navigation(snapshot, drafted)

    mid_edit = projected.nodes["config:snapshotMigrationConfigs:0"]
    assert [
        capability.target_id
        for capability in mid_edit.capabilities
        if capability.kind == "edit"
    ] == ["edit:snapshotMigrationConfigs.0"]
    assert mid_edit.label == "source-target-snap-<NAME>"


def test_incomplete_snapshot_migration_name_updates_as_references_are_selected():
    snapshot = _observe_pending_only_migrations(())
    drafted = _snapshot_migration_draft((
        ("source", "", "", "slice-2"),
    ))

    projected = project_config_navigation(snapshot, drafted)

    migration = projected.nodes["config:snapshotMigrationConfigs:0"]
    assert migration.label == "source-<TARGET>-<SNAPSHOT>-slice-2"
    assert migration.navigation_key == ("source", "", "", "slice-2")


def test_snapshot_migration_navigation_deduplicates_flattened_resources():
    section_id = "section:Snapshot Migration"
    group_id = "group:Snapshot Migration:Backfill"
    resource_id = "resource:snapshotmigrations:source-target-snap-slice-0"
    snapshot = ManageSnapshot(
        format_version=1,
        revision="runtime",
        observed_at="2026-09-10T12:00:00+00:00",
        namespace="ma",
        workflow_name="migration-workflow",
        workflow=None,
        root_ids=(section_id,),
        nodes={
            section_id: ManageNode(
                id=section_id,
                revision="r",
                parent_id=None,
                kind="section",
                label="Snapshot Migration",
                status="ok",
                child_ids=(resource_id, group_id),
            ),
            group_id: ManageNode(
                id=group_id,
                revision="r",
                parent_id=section_id,
                kind="group",
                label="Backfill",
                status="ok",
                child_ids=(resource_id,),
            ),
            resource_id: ManageNode(
                id=resource_id,
                revision="r",
                parent_id=group_id,
                kind="resource",
                label="slice-0",
                status="ok",
                resource_plural="snapshotmigrations",
                resource_name="source-target-snap-slice-0",
                resource_type="Snapshot migration",
                navigation_key=("source", "target", "snap", "slice-0"),
            ),
        },
    )

    grouped = group_snapshot_migration_navigation(snapshot)

    assert grouped.nodes[section_id].child_ids == (resource_id,)


def test_project_config_navigation_does_not_infer_structure_from_raw_repair():
    snapshot = _runtime_snapshot()

    projected = project_config_navigation(snapshot, _draft([], mode="raw"))

    assert projected.root_ids == snapshot.root_ids
    assert "resource:sourceconfigs:legacy" in projected.nodes
    assert "workflow-step:resource:sourceconfigs:legacy:apply" not in projected.nodes
    assert projected.nodes["resource:sourceconfigs:legacy"].status == "ok"
