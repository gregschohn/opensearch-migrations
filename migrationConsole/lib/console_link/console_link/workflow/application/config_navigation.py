"""Project configuration-edit navigation from runtime state and schema hints."""

from dataclasses import dataclass, replace
import re
from typing import Any, Dict, Iterable, Mapping, Optional, Tuple, cast

from ..manage_tree_schema import EDIT_ID_BY_TREE_ID
from .config_drafts import ConfigDraft
from .models import (
    ManageCapability,
    ManageConfigState,
    ManageDetail,
    ManageDiagnostic,
    ManageNode,
    ManageSnapshot,
)

EDIT_TARGET_PREFIX = "edit:"


@dataclass(frozen=True)
class _Identity:
    kind: str
    prefix: str = ""
    suffix: str = ""
    first_index: int = 0


@dataclass(frozen=True)
class _Placement:
    collection_path: Tuple[str, ...]
    section_id: str
    section_label: str
    section_order: int
    group_id: str
    group_label: str
    group_order: int
    parent_group_id: Optional[str]
    parent_group_label: Optional[str]
    parent_group_order: Optional[int]
    resource_plural: str
    resource_type: str
    identity: _Identity


@dataclass(frozen=True)
class _DefinitionPlacement:
    collection_path: Tuple[str, ...]
    owner_target_id: str
    group_id: str
    group_label: str
    group_order: int
    definition_type: str


def project_config_navigation(
    snapshot: ManageSnapshot,
    draft: ConfigDraft,
) -> ManageSnapshot:
    """Return stable config navigation; clients may add transient UI overlays."""
    configuration = _without_workflow_steps(snapshot)
    provenance = _mapping(draft.edit_state.get("provenance"))
    if provenance.get("mode") == "raw" or draft.repair_yaml is not None:
        return configuration

    edit_nodes = _edit_nodes(draft.edit_state.get("nodes") or ())
    placements = _resource_placements(draft.edit_state.get("nodes") or ())
    definition_placements = _definition_placements(
        draft.edit_state.get("nodes") or ()
    )
    snapshot_migration_targets = _snapshot_migration_edit_targets(edit_nodes)
    nodes = {
        node_id: _project_existing_node(
            node,
            draft,
            edit_nodes,
            placements,
            snapshot_migration_targets,
        )
        for node_id, node in configuration.nodes.items()
    }
    root_ids = list(configuration.root_ids)

    _ensure_navigation(nodes, root_ids, placements, draft.draft_revision)
    _add_draft_resources(
        nodes,
        edit_nodes,
        placements,
        draft.draft_revision,
        draft.dirty,
    )
    _add_draft_definitions(
        nodes,
        edit_nodes,
        definition_placements,
        draft.draft_revision,
        draft.dirty,
    )
    _prefer_resource_surfaces(nodes)
    projected = cast(ManageSnapshot, replace(
        configuration,
        revision=(
            f"{snapshot.revision}:{draft.draft_revision}:configuration"
        ),
        root_ids=tuple(root_ids),
        nodes=nodes,
    ))
    return group_snapshot_migration_navigation(projected)


_SNAPSHOT_SECTION_ID = "section:Snapshot Migration"
_SNAPSHOT_BASE_GROUP_ID = "group:Snapshot Migration:Backfill"
_SNAPSHOT_DYNAMIC_GROUP_PREFIX = "snapshot-navigation:"
_NAVIGATION_STATUS_RANK = {
    "ok": 0,
    "unknown": 1,
    "changed": 2,
    "removed": 3,
    "warning": 4,
    "required": 5,
    "blocked": 6,
    "error": 7,
}


def _natural_component(value: str) -> Tuple[Tuple[int, Any], ...]:
    return tuple(
        (1, int(part)) if part.isdigit() else (0, part.casefold())
        for part in re.split(r"(\d+)", value)
        if part
    )


def _snapshot_sort_key(node: ManageNode) -> Tuple[Any, ...]:
    return tuple(
        _natural_component(value)
        for value in node.navigation_key
    )


def _snapshot_group_id(prefix: Tuple[str, ...]) -> str:
    return (
        f"{_SNAPSHOT_DYNAMIC_GROUP_PREFIX}{len(prefix)}:"
        + ":".join(prefix)
    )


def _snapshot_group_label(
    prefix: Tuple[str, ...],
    parent_id: str,
) -> str:
    parent_depth = (
        int(parent_id.split(":", 2)[1])
        if parent_id.startswith(_SNAPSHOT_DYNAMIC_GROUP_PREFIX)
        else 0
    )
    visible = prefix[parent_depth:]
    return _snapshot_migration_name(*visible)


def _snapshot_group_status(
    nodes: Mapping[str, ManageNode],
    child_ids: Iterable[str],
) -> str:
    statuses = [
        nodes[child_id].status
        for child_id in child_ids
        if child_id in nodes
    ]
    if not statuses:
        return "ok"
    if all(status == "removed" for status in statuses):
        return "removed"
    # A group is not going away just because one of its members is, but it has
    # still changed, so removals rank as changes while any member survives.
    return max(
        ("changed" if status == "removed" else status for status in statuses),
        key=lambda status: _NAVIGATION_STATUS_RANK.get(status, 0),
    )


def group_snapshot_migration_navigation(
    snapshot: ManageSnapshot,
) -> ManageSnapshot:
    """Sort SnapshotMigration leaves and add only useful semantic groups.

    Edit and runtime projections call this same function with different leaf
    sets. A source/target/snapshot prefix becomes collapsible only when it
    contains at least three leaves and would have at least two direct children.
    """
    nodes = {
        node_id: node
        for node_id, node in snapshot.nodes.items()
        if not node_id.startswith(_SNAPSHOT_DYNAMIC_GROUP_PREFIX)
    }
    resources = sorted(
        (
            node for node in nodes.values()
            if (
                node.kind == "resource"
                and node.resource_plural == "snapshotmigrations"
                and len(node.navigation_key) == 4
                and all(node.navigation_key)
            )
        ),
        key=_snapshot_sort_key,
    )
    base_group = nodes.get(_SNAPSHOT_BASE_GROUP_ID)
    if base_group is None:
        return snapshot

    resource_ids = {node.id for node in resources}
    nodes[_SNAPSHOT_BASE_GROUP_ID] = replace(
        base_group,
        label="Snapshot migrations",
        child_ids=tuple(
            child_id for child_id in base_group.child_ids
            if child_id not in resource_ids
            and not child_id.startswith(_SNAPSHOT_DYNAMIC_GROUP_PREFIX)
        ),
    )
    for resource in resources:
        nodes[resource.id] = replace(
            resource,
            parent_id=_SNAPSHOT_BASE_GROUP_ID,
            label=_snapshot_migration_name(*resource.navigation_key),
        )

    def representations(
        items: Tuple[ManageNode, ...],
        level: int,
        parent_id: str,
    ) -> list[str]:
        if level >= 3:
            result = []
            for item in items:
                nodes[item.id] = replace(item, parent_id=parent_id)
                result.append(item.id)
            return result

        buckets: Dict[str, list[ManageNode]] = {}
        for item in items:
            buckets.setdefault(item.navigation_key[level], []).append(item)
        result: list[str] = []
        for value in sorted(buckets, key=_natural_component):
            bucket = tuple(sorted(buckets[value], key=_snapshot_sort_key))
            prefix = bucket[0].navigation_key[:level + 1]
            prospective_parent = _snapshot_group_id(prefix)
            children = representations(bucket, level + 1, prospective_parent)
            if len(bucket) >= 3 and len(children) >= 2:
                group_id = prospective_parent
                nodes[group_id] = ManageNode(
                    id=group_id,
                    revision=f"{snapshot.revision}:{group_id}",
                    parent_id=parent_id,
                    kind="group",
                    label=_snapshot_group_label(prefix, parent_id),
                    status=_snapshot_group_status(nodes, children),
                    child_ids=tuple(children),
                )
                result.append(group_id)
                continue
            for child_id in children:
                nodes[child_id] = replace(nodes[child_id], parent_id=parent_id)
            result.extend(children)
        return result

    grouped_ids = representations(
        tuple(resources),
        0,
        _SNAPSHOT_BASE_GROUP_ID,
    )
    base = nodes[_SNAPSHOT_BASE_GROUP_ID]
    nodes[_SNAPSHOT_BASE_GROUP_ID] = replace(
        base,
        child_ids=(*base.child_ids, *grouped_ids),
        status=_snapshot_group_status(nodes, grouped_ids) if grouped_ids else base.status,
    )
    # Groups are created against the parent they were offered, which is not the
    # parent they end up with when an intermediate level is not worth keeping.
    # Label them from where they actually landed so siblings stay distinguishable.
    for node_id, node in list(nodes.items()):
        if node.kind != "group" or not node_id.startswith(_SNAPSHOT_DYNAMIC_GROUP_PREFIX):
            continue
        nodes[node_id] = replace(
            node,
            label=_snapshot_group_label(
                tuple(node_id.split(":")[2:]),
                node.parent_id or "",
            ),
        )
    for resource in resources:
        grouped_resource = nodes[resource.id]
        parent_depth = (
            int(grouped_resource.parent_id.split(":", 2)[1])
            if (
                grouped_resource.parent_id
                and grouped_resource.parent_id.startswith(
                    _SNAPSHOT_DYNAMIC_GROUP_PREFIX
                )
            )
            else 0
        )
        nodes[resource.id] = replace(
            grouped_resource,
            label=_snapshot_migration_name(
                *resource.navigation_key[parent_depth:]
            ),
        )
    section = nodes.get(_SNAPSHOT_SECTION_ID)
    base_group = nodes.get(_SNAPSHOT_BASE_GROUP_ID)
    if (
        section is not None
        and base_group is not None
        and base_group.parent_id == section.id
    ):
        flattened_ids = []
        for child_id in section.child_ids:
            if child_id == base_group.id:
                flattened_ids.extend(base_group.child_ids)
            else:
                flattened_ids.append(child_id)
        for child_id in base_group.child_ids:
            child = nodes.get(child_id)
            if child is not None:
                nodes[child_id] = replace(child, parent_id=section.id)
        # This function runs over the runtime snapshot and again over the merged
        # draft projection. The earlier pass leaves flattened leaves attached to
        # the section, so drop any the current pass has re-homed into a group -
        # otherwise the tree walk emits them twice, once per parent.
        nodes[section.id] = replace(
            section,
            child_ids=tuple(
                child_id for child_id in dict.fromkeys(flattened_ids)
                if child_id not in nodes
                or nodes[child_id].parent_id == section.id
            ),
        )
        del nodes[base_group.id]
    return cast(ManageSnapshot, replace(snapshot, nodes=nodes))


def _node_edit_target(node: ManageNode) -> Optional[str]:
    for capability in node.capabilities:
        if capability.kind == "edit" and capability.target_id:
            return capability.target_id
    return None


def _prefer_resource_surfaces(nodes: Dict[str, ManageNode]) -> None:
    """Fold a definition entry into the resource that edits the same data.

    A data snapshot, for example, has one definition inside its source
    cluster; when the runtime resource for it exists, present a single
    navigation entry (the resource, widened to edit the full definition)
    instead of a resource entry plus a definition entry.
    """
    definition_ids = [
        node_id for node_id, node in nodes.items()
        if node.kind == "config-definition"
    ]
    for definition_id in definition_ids:
        definition = nodes.get(definition_id)
        if definition is None:
            continue
        target = _node_edit_target(definition)
        if not target:
            continue
        owner_id = next(
            (
                node_id for node_id, node in nodes.items()
                if node.kind == "resource"
                and (edit_target := _node_edit_target(node)) is not None
                and (
                    edit_target == target
                    or edit_target.startswith(f"{target}.")
                )
            ),
            None,
        )
        if owner_id is None:
            continue
        resource = nodes[owner_id]
        nodes[owner_id] = replace(resource, capabilities=tuple(
            replace(capability, target_id=target)
            if capability.kind == "edit" else capability
            for capability in resource.capabilities
        ))
        previous_parent_id = nodes[owner_id].parent_id
        definition_parent_id = definition.parent_id
        if definition_parent_id and definition_parent_id in nodes:
            parent = nodes[definition_parent_id]
            nodes[definition_parent_id] = replace(parent, child_ids=tuple(
                owner_id if child_id == definition_id else child_id
                for child_id in parent.child_ids
                if child_id != owner_id
            ))
        if (
            previous_parent_id
            and previous_parent_id != definition_parent_id
            and previous_parent_id in nodes
        ):
            previous_parent = nodes[previous_parent_id]
            nodes[previous_parent_id] = replace(
                previous_parent,
                child_ids=tuple(
                    child_id for child_id in previous_parent.child_ids
                    if child_id != owner_id
                ),
            )
        nodes[owner_id] = replace(
            nodes[owner_id],
            parent_id=definition_parent_id,
        )
        del nodes[definition_id]


def _without_workflow_steps(snapshot: ManageSnapshot) -> ManageSnapshot:
    step_ids = {
        node_id
        for node_id, node in snapshot.nodes.items()
        if node.kind == "workflow-step"
    }
    if not step_ids:
        return snapshot
    nodes = {
        node_id: replace(
            node,
            child_ids=tuple(
                child_id
                for child_id in node.child_ids
                if child_id not in step_ids
            ),
        )
        for node_id, node in snapshot.nodes.items()
        if node_id not in step_ids
    }
    return cast(ManageSnapshot, replace(
        snapshot,
        root_ids=tuple(
            node_id for node_id in snapshot.root_ids
            if node_id not in step_ids
        ),
        nodes=nodes,
    ))


def _project_existing_node(
    node: ManageNode,
    draft: ConfigDraft,
    edit_nodes: Mapping[str, Mapping[str, Any]],
    placements: Tuple[_Placement, ...],
    snapshot_migration_targets: Mapping[
        Tuple[str, ...],
        Tuple[str, ...],
    ],
) -> ManageNode:
    if node.kind != "resource":
        return node
    semantic_removal = False
    if (
        draft.dirty
        and node.resource_plural == "snapshotmigrations"
        and len(node.navigation_key) == 4
        and all(node.navigation_key)
        and _snapshot_migration_collection_changed(edit_nodes)
    ):
        matching_targets = snapshot_migration_targets.get(
            node.navigation_key,
            (),
        )
        if matching_targets:
            current_target = _edit_target(node)
            node = _with_edit_capability(
                node,
                (
                    current_target
                    if current_target in matching_targets
                    else matching_targets[0]
                ),
            )
        else:
            semantic_removal = True
            node = _without_edit_capability(node)
    target_id = _edit_target(node)
    edit_node = edit_nodes.get(target_id) if target_id is not None else None
    config_state = (
        _config_state(edit_node, draft.dirty)
        if edit_node is not None
        else None
    )
    explicit_removal = (
        node.config_presence.get("pending") is False
        and (
            node.config_presence.get("deployed") is True
            or node.config_presence.get("submitted") is True
        )
    )
    removed_from_draft = (
        semantic_removal
        or (
            draft.dirty
            and target_id is not None
            and _is_resource_target(target_id, placements)
            and target_id not in edit_nodes
            and _resource_collection_changed(
                target_id,
                edit_nodes,
                placements,
            )
        )
    )
    if not explicit_removal and not removed_from_draft:
        return cast(ManageNode, replace(node, config_state=config_state))
    label = "Removal pending submission"
    if node.config_presence.get("pending") is not False and draft.dirty:
        label = "Marked for removal"
    return cast(ManageNode, replace(
        node,
        revision=f"{node.revision}:{draft.draft_revision}:removed",
        status="removed",
        value_summary=label,
        config_state=config_state,
    ))


def _edit_target(node: ManageNode) -> Optional[str]:
    return next(
        (
            capability.target_id
            for capability in node.capabilities
            if capability.kind == "edit"
        ),
        None,
    )


def _without_edit_capability(node: ManageNode) -> ManageNode:
    return cast(ManageNode, replace(
        node,
        capabilities=tuple(
            capability
            for capability in node.capabilities
            if capability.kind != "edit"
        ),
    ))


def _snapshot_migration_key(
    node: Mapping[str, Any],
) -> Optional[Tuple[str, ...]]:
    path = node.get("path")
    if (
        not isinstance(path, list)
        or len(path) != 2
        or path[0] != "snapshotMigrationConfigs"
    ):
        return None
    value = _mapping(node.get("value"))
    key = tuple(
        str(value.get(field) or "")
        for field in (
            "fromSource",
            "toTarget",
            "fromSnapshot",
            "slice",
        )
    )
    return key if all(key) else None


def _snapshot_migration_edit_targets(
    edit_nodes: Mapping[str, Mapping[str, Any]],
) -> Dict[Tuple[str, ...], Tuple[str, ...]]:
    targets: Dict[Tuple[str, ...], list[str]] = {}
    for target_id, edit_node in edit_nodes.items():
        key = _snapshot_migration_key(edit_node)
        if key is not None:
            targets.setdefault(key, []).append(target_id)
    return {
        key: tuple(target_ids)
        for key, target_ids in targets.items()
    }


def _snapshot_migration_collection_changed(
    edit_nodes: Mapping[str, Mapping[str, Any]],
) -> bool:
    collection = edit_nodes.get("edit:snapshotMigrationConfigs")
    return (
        collection is not None
        and _draft_change_count(collection) > 0
    )


def _is_resource_target(
    target_id: str,
    placements: Tuple[_Placement, ...],
) -> bool:
    path = (
        target_id[len(EDIT_TARGET_PREFIX):]
        if target_id.startswith(EDIT_TARGET_PREFIX)
        else target_id
    )
    return any(
        path == ".".join(placement.collection_path)
        or path.startswith(f"{'.'.join(placement.collection_path)}.")
        for placement in placements
    )


def _resource_collection_changed(
    target_id: str,
    edit_nodes: Mapping[str, Mapping[str, Any]],
    placements: Tuple[_Placement, ...],
) -> bool:
    path = (
        target_id[len(EDIT_TARGET_PREFIX):]
        if target_id.startswith(EDIT_TARGET_PREFIX)
        else target_id
    )
    for placement in placements:
        collection_path = ".".join(placement.collection_path)
        if not path.startswith(f"{collection_path}."):
            continue
        collection = edit_nodes.get(f"{EDIT_TARGET_PREFIX}{collection_path}")
        return (
            collection is not None
            and _draft_change_count(collection) > 0
        )
    return False


def _edit_nodes(
    roots: Iterable[Mapping[str, Any]],
) -> Dict[str, Mapping[str, Any]]:
    result: Dict[str, Mapping[str, Any]] = {}

    def visit(nodes: Iterable[Mapping[str, Any]]) -> None:
        for node in nodes:
            node_id = node.get("id")
            if isinstance(node_id, str):
                result[node_id] = node
            visit(_mapping_children(node))

    visit(roots)
    return result


def _resource_placements(
    roots: Iterable[Mapping[str, Any]],
) -> Tuple[_Placement, ...]:
    result: Dict[Tuple[str, ...], _Placement] = {}

    def visit(nodes: Iterable[Mapping[str, Any]]) -> None:
        for node in nodes:
            placement = _placement(node)
            if placement is not None:
                result[placement.collection_path] = placement
            visit(_mapping_children(node))

    visit(roots)
    return tuple(result.values())


def _definition_placements(
    roots: Iterable[Mapping[str, Any]],
) -> Tuple[_DefinitionPlacement, ...]:
    result: Dict[Tuple[str, ...], _DefinitionPlacement] = {}

    def visit(nodes: Iterable[Mapping[str, Any]]) -> None:
        for node in nodes:
            placement = _definition_placement(node)
            if placement is not None:
                result[placement.collection_path] = placement
            visit(_mapping_children(node))

    visit(roots)
    return tuple(result.values())


def _definition_placement(
    node: Mapping[str, Any],
) -> Optional[_DefinitionPlacement]:
    collection = _mapping(
        _mapping(node.get("inputHint")).get("definitionCollection")
    )
    navigation = _mapping(collection.get("navigation"))
    definition = _mapping(collection.get("definition"))
    path = node.get("path")
    owner_ancestor_levels = collection.get("ownerAncestorLevels")
    group_id = navigation.get("groupId")
    node_id = node.get("id")
    if (
        not isinstance(path, list)
        or not path
        or not all(isinstance(part, str) for part in path)
        or not isinstance(owner_ancestor_levels, int)
        or owner_ancestor_levels <= 0
        or owner_ancestor_levels >= len(path)
        or not isinstance(navigation.get("groupLabel"), str)
        or not isinstance(navigation.get("groupOrder"), int)
        or not isinstance(definition.get("typeLabel"), str)
    ):
        return None
    if not isinstance(group_id, str):
        if not isinstance(node_id, str):
            return None
        group_id = f"definition-group:{node_id}"
    owner_path = path[:-owner_ancestor_levels]
    return _DefinitionPlacement(
        collection_path=tuple(path),
        owner_target_id=f"{EDIT_TARGET_PREFIX}{'.'.join(owner_path)}",
        group_id=group_id,
        group_label=navigation["groupLabel"],
        group_order=navigation["groupOrder"],
        definition_type=definition["typeLabel"],
    )


def _placement(node: Mapping[str, Any]) -> Optional[_Placement]:
    collection = _mapping(
        _mapping(node.get("inputHint")).get("resourceCollection")
    )
    navigation = _mapping(collection.get("navigation"))
    resource = _mapping(collection.get("resource"))
    identity = _mapping(resource.get("identity"))
    path = node.get("path")
    required_strings = (
        navigation.get("sectionId"),
        navigation.get("sectionLabel"),
        navigation.get("groupId"),
        navigation.get("groupLabel"),
        resource.get("plural"),
        resource.get("typeLabel"),
        identity.get("kind"),
    )
    if (
        not isinstance(path, list)
        or not all(isinstance(part, str) for part in path)
        or not all(isinstance(value, str) for value in required_strings)
        or not isinstance(navigation.get("sectionOrder"), int)
        or not isinstance(navigation.get("groupOrder"), int)
        or identity.get("kind") not in {"named", "indexed-config"}
    ):
        return None
    parent_values = (
        navigation.get("parentGroupId"),
        navigation.get("parentGroupLabel"),
        navigation.get("parentGroupOrder"),
    )
    has_parent = any(value is not None for value in parent_values)
    if has_parent and (
        not isinstance(parent_values[0], str)
        or not isinstance(parent_values[1], str)
        or not isinstance(parent_values[2], int)
    ):
        return None
    return _Placement(
        collection_path=tuple(path),
        section_id=navigation["sectionId"],
        section_label=navigation["sectionLabel"],
        section_order=navigation["sectionOrder"],
        group_id=navigation["groupId"],
        group_label=navigation["groupLabel"],
        group_order=navigation["groupOrder"],
        parent_group_id=parent_values[0] if has_parent else None,
        parent_group_label=parent_values[1] if has_parent else None,
        parent_group_order=parent_values[2] if has_parent else None,
        resource_plural=resource["plural"],
        resource_type=resource["typeLabel"],
        identity=_Identity(
            kind=identity["kind"],
            prefix=(
                identity["prefix"]
                if isinstance(identity.get("prefix"), str)
                else ""
            ),
            suffix=(
                identity["suffix"]
                if isinstance(identity.get("suffix"), str)
                else ""
            ),
            first_index=(
                identity["firstIndex"]
                if isinstance(identity.get("firstIndex"), int)
                else 0
            ),
        ),
    )


def _ensure_navigation(
    nodes: Dict[str, ManageNode],
    root_ids: list[str],
    placements: Tuple[_Placement, ...],
    revision: str,
) -> None:
    section_order = {
        placement.section_id: placement.section_order
        for placement in placements
    }
    group_order = {
        placement.group_id: placement.group_order
        for placement in placements
    }
    group_order.update({
        placement.parent_group_id: placement.parent_group_order
        for placement in placements
        if (
            placement.parent_group_id is not None
            and placement.parent_group_order is not None
        )
    })
    for placement in placements:
        section = _ensure_section(
            nodes,
            root_ids,
            placement,
            revision,
        )
        parent_id = _ensure_parent_group(
            nodes,
            section,
            placement,
            revision,
            group_order,
        )
        _ensure_group(
            nodes,
            parent_id,
            placement,
            revision,
            group_order,
        )

    root_ids[:] = _ordered_ids(root_ids, section_order)


def _ensure_section(
    nodes: Dict[str, ManageNode],
    root_ids: list[str],
    placement: _Placement,
    revision: str,
) -> ManageNode:
    section = nodes.get(placement.section_id) or ManageNode(
        id=placement.section_id,
        revision=f"{revision}:{placement.section_id}",
        kind="section",
        label=placement.section_label,
        status="ok",
    )
    section = _with_edit_capability(
        section,
        EDIT_ID_BY_TREE_ID.get(section.id),
    )
    nodes[section.id] = section
    if section.id not in root_ids:
        root_ids.append(section.id)
    return section


def _ensure_parent_group(
    nodes: Dict[str, ManageNode],
    section: ManageNode,
    placement: _Placement,
    revision: str,
    group_order: Mapping[str, int],
) -> str:
    if (
        placement.parent_group_id is None
        or placement.parent_group_label is None
    ):
        return section.id
    parent_group = nodes.get(placement.parent_group_id) or ManageNode(
        id=placement.parent_group_id,
        revision=f"{revision}:{placement.parent_group_id}",
        parent_id=section.id,
        kind="group",
        label=placement.parent_group_label,
        status="ok",
    )
    parent_group = _with_edit_capability(
        parent_group,
        EDIT_ID_BY_TREE_ID.get(parent_group.id),
    )
    nodes[parent_group.id] = parent_group
    _append_ordered_child(
        nodes,
        section.id,
        parent_group.id,
        revision,
        group_order,
    )
    return parent_group.id


def _ensure_group(
    nodes: Dict[str, ManageNode],
    parent_id: str,
    placement: _Placement,
    revision: str,
    group_order: Mapping[str, int],
) -> None:
    group = nodes.get(placement.group_id) or ManageNode(
        id=placement.group_id,
        revision=f"{revision}:{placement.group_id}",
        parent_id=parent_id,
        kind="group",
        label=placement.group_label,
        status="ok",
    )
    group = _with_edit_capability(
        group,
        EDIT_ID_BY_TREE_ID.get(group.id),
    )
    nodes[group.id] = group
    _append_ordered_child(
        nodes,
        parent_id,
        group.id,
        revision,
        group_order,
    )


def _append_ordered_child(
    nodes: Dict[str, ManageNode],
    parent_id: str,
    child_id: str,
    revision: str,
    order: Mapping[str, int],
) -> None:
    parent = nodes[parent_id]
    if child_id in parent.child_ids:
        return
    nodes[parent_id] = replace(
        parent,
        revision=f"{parent.revision}:{revision}",
        child_ids=tuple(_ordered_ids(
            (*parent.child_ids, child_id),
            order,
        )),
    )


def _with_edit_capability(
    node: ManageNode,
    target_id: Optional[str],
) -> ManageNode:
    if target_id is None:
        return node
    capabilities = tuple(
        capability
        for capability in node.capabilities
        if capability.kind != "edit"
    )
    return cast(ManageNode, replace(
        node,
        capabilities=(
            *capabilities,
            ManageCapability(
                kind="edit",
                target_id=target_id,
                label=f"Edit {node.label}",
            ),
        ),
    ))


def _ordered_ids(
    node_ids: Iterable[str],
    order: Mapping[str, int],
) -> list[str]:
    indexed = list(enumerate(node_ids))
    indexed.sort(
        key=lambda item: (
            order.get(item[1], 2**31 - 1),
            item[0],
        )
    )
    return [node_id for _, node_id in indexed]


def _diagnostics(node: Mapping[str, Any]) -> Tuple[ManageDiagnostic, ...]:
    return tuple(
        diagnostic
        for value in node.get("diagnostics") or ()
        if (diagnostic := _diagnostic(value)) is not None
    )


def _new_draft_resource(
    placement: _Placement,
    child: Mapping[str, Any],
    index: int,
    revision: str,
    dirty: bool,
    existing_targets: set[str],
    existing_node_ids: set[str],
) -> Optional[Tuple[ManageNode, str]]:
    if child.get("valueKind") == "command":
        return None
    target_id = child.get("id")
    if not isinstance(target_id, str) or target_id in existing_targets:
        return None
    identity = _resource_identity(placement, child, index)
    if identity is None or identity[0] in existing_node_ids:
        return None
    node_id, resource_name = identity
    implicit = child.get("implicit") is True
    status_value = child.get("status")
    status = (
        status_value
        if isinstance(status_value, str)
        and (implicit or status_value != "ok")
        else "changed"
    )
    phase = "Implicit default" if implicit else "Pending Config"
    value_summary = (
        "Available when referenced"
        if implicit
        else "Addition pending submission"
    )
    return (
        ManageNode(
            id=node_id,
            revision=(
                f"{revision}:{target_id}:implicit"
                if implicit
                else f"{revision}:{target_id}:added"
            ),
            parent_id=placement.group_id,
            kind="resource",
            label=resource_name,
            description=f"{placement.resource_plural}/{resource_name}",
            status=status,
            phase=phase,
            value_summary=value_summary,
            diagnostics=_diagnostics(child),
            capabilities=(
                ManageCapability(
                    kind="edit",
                    target_id=target_id,
                    label=f"Edit {resource_name}",
                ),
            ),
            details=(
                ManageDetail(
                    label="Phase",
                    value=phase,
                    kind="phase",
                ),
            ),
            resource_plural=placement.resource_plural,
            resource_name=resource_name,
            resource_type=placement.resource_type,
            config_presence={
                "deployed": False,
                "pending": not implicit,
            },
            config_state=_config_state(child, dirty),
        ),
        target_id,
    )


def _snapshot_migration_name(*parts: str) -> str:
    return re.sub(r"[^a-z0-9.]+", "-", "-".join(parts).lower()).strip("-.")


def _snapshot_migration_display_name(
    source: str,
    target: str,
    snapshot: str,
    label: str,
) -> str:
    return "-".join((
        source or "<SOURCE>",
        target or "<TARGET>",
        snapshot or "<SNAPSHOT>",
        label or "<NAME>",
    ))


def _new_draft_snapshot_resources(
    placement: _Placement,
    tuple_node: Mapping[str, Any],
    revision: str,
    dirty: bool,
    existing_targets: set[str],
) -> Tuple[Tuple[ManageNode, str], ...]:
    path = tuple_node.get("path")
    if (
        tuple_node.get("valueKind") == "command"
        or not isinstance(path, list)
        or len(path) != 2
        or path[0] != "snapshotMigrationConfigs"
        or not str(path[1]).isdigit()
    ):
        return ()
    value = _mapping(tuple_node.get("value"))
    source = str(value.get("fromSource") or "")
    target = str(value.get("toTarget") or "")
    snapshot = str(value.get("fromSnapshot") or "")
    label = str(value.get("slice") or "")
    target_id = tuple_node.get("id")
    if not isinstance(target_id, str) or target_id in existing_targets:
        return ()
    # An entry with a blank slice is mid-edit, not absent. It still needs a row,
    # or clearing the name leaves the user nothing to click to put it back.
    resource_name = _snapshot_migration_display_name(
        source,
        target,
        snapshot,
        label,
    )
    stable_path = target_id.removeprefix(f"{EDIT_TARGET_PREFIX}")
    node_id = f"config:{stable_path.replace('.', ':')}"
    status_value = tuple_node.get("status")
    status = (
        status_value
        if isinstance(status_value, str) and status_value != "ok"
        else "changed"
    )
    phase = "Pending Config"
    return ((
        ManageNode(
            id=node_id,
            revision=f"{revision}:{target_id}:added",
            parent_id=placement.group_id,
            kind="resource",
            label=resource_name,
            description=f"{placement.resource_plural}/{resource_name}",
            status=status,
            phase=phase,
            value_summary="Addition pending submission",
            diagnostics=_diagnostics(tuple_node),
            capabilities=(
                ManageCapability(
                    kind="edit",
                    target_id=target_id,
                    label=f"Edit {label}",
                ),
            ),
            details=(
                ManageDetail(
                    label="Phase",
                    value=phase,
                    kind="phase",
                ),
            ),
            resource_plural=placement.resource_plural,
            resource_name=resource_name,
            resource_type=placement.resource_type,
            config_presence={
                "deployed": False,
                "pending": True,
            },
            config_state=_config_state(tuple_node, dirty),
            navigation_key=(source, target, snapshot, label),
        ),
        target_id,
    ),)


def _append_child(
    nodes: Dict[str, ManageNode],
    parent_id: str,
    child_id: str,
    revision: str,
) -> None:
    parent = nodes.get(parent_id)
    if parent is None or child_id in parent.child_ids:
        return
    nodes[parent.id] = cast(ManageNode, replace(
        parent,
        revision=f"{parent.revision}:{revision}",
        child_ids=(*parent.child_ids, child_id),
    ))


def _add_draft_resources(
    nodes: Dict[str, ManageNode],
    edit_nodes: Mapping[str, Mapping[str, Any]],
    placements: Tuple[_Placement, ...],
    revision: str,
    dirty: bool,
) -> None:
    existing_targets = {
        target
        for node in nodes.values()
        if (target := _edit_target(node)) is not None
    }
    for placement in placements:
        collection_id = (
            f"{EDIT_TARGET_PREFIX}{'.'.join(placement.collection_path)}"
        )
        collection = edit_nodes.get(collection_id)
        if collection is None:
            continue
        if placement.collection_path == ("snapshotMigrationConfigs",):
            for tuple_node in _mapping_children(collection):
                for node, target_id in _new_draft_snapshot_resources(
                    placement,
                    tuple_node,
                    revision,
                    dirty,
                    existing_targets,
                ):
                    existing = nodes.get(node.id)
                    if existing is None:
                        nodes[node.id] = node
                    else:
                        retained_capabilities = tuple(
                            capability
                            for capability in existing.capabilities
                            if capability.kind != "edit"
                        )
                        nodes[node.id] = cast(ManageNode, replace(
                            existing,
                            capabilities=(
                                *retained_capabilities,
                                *node.capabilities,
                            ),
                            config_state=node.config_state,
                            navigation_key=node.navigation_key,
                        ))
                    existing_targets.add(target_id)
                    _append_child(nodes, placement.group_id, node.id, revision)
            continue
        for index, child in enumerate(_mapping_children(collection)):
            candidate = _new_draft_resource(
                placement,
                child,
                index,
                revision,
                dirty,
                existing_targets,
                set(nodes),
            )
            if candidate is None:
                continue
            node, target_id = candidate
            nodes[node.id] = node
            existing_targets.add(target_id)
            _append_child(nodes, placement.group_id, node.id, revision)


def _new_draft_definition(
    child: Mapping[str, Any],
    placement: _DefinitionPlacement,
    revision: str,
    dirty: bool,
) -> Optional[ManageNode]:
    if child.get("valueKind") == "command":
        return None
    target_id = child.get("id")
    path = child.get("path")
    if not isinstance(target_id, str) or not isinstance(path, list):
        return None
    if len(path) <= len(placement.collection_path):
        return None
    label = str(path[len(placement.collection_path)])
    status_value = child.get("status")
    status = status_value if isinstance(status_value, str) else "ok"
    return ManageNode(
        id=f"definition:{target_id}",
        revision=f"{revision}:{target_id}",
        parent_id=placement.group_id,
        kind="config-definition",
        label=label,
        description=placement.definition_type,
        status=status,
        diagnostics=_diagnostics(child),
        capabilities=(
            ManageCapability(
                kind="edit",
                target_id=target_id,
                label=f"Edit {label}",
            ),
        ),
        resource_type=placement.definition_type,
        config_state=_config_state(child, dirty),
    )


def _definition_group(
    placement: _DefinitionPlacement,
    collection: Mapping[str, Any],
    owner_id: str,
    revision: str,
    dirty: bool,
) -> Tuple[ManageNode, Tuple[ManageNode, ...]]:
    definitions = tuple(
        definition
        for child in _mapping_children(collection)
        if (
            definition := _new_draft_definition(
                child,
                placement,
                revision,
                dirty,
            )
        ) is not None
    )
    collection_id = (
        f"{EDIT_TARGET_PREFIX}{'.'.join(placement.collection_path)}"
    )
    group = ManageNode(
        id=placement.group_id,
        revision=f"{revision}:{collection_id}",
        parent_id=owner_id,
        child_ids=tuple(node.id for node in definitions),
        kind="group",
        label=placement.group_label,
        status="ok",
    )
    return group, definitions


def _add_draft_definitions(
    nodes: Dict[str, ManageNode],
    edit_nodes: Mapping[str, Mapping[str, Any]],
    placements: Tuple[_DefinitionPlacement, ...],
    revision: str,
    dirty: bool,
) -> None:
    owners_by_target = {
        target: node
        for node in nodes.values()
        if (target := _edit_target(node)) is not None
    }
    placements_by_owner: Dict[str, list[_DefinitionPlacement]] = {}
    for placement in placements:
        placements_by_owner.setdefault(
            placement.owner_target_id,
            [],
        ).append(placement)

    for owner_target_id, owner_placements in placements_by_owner.items():
        owner = owners_by_target.get(owner_target_id)
        if owner is None:
            continue
        group_order: Dict[str, int] = {}
        child_ids = list(owner.child_ids)
        for placement in owner_placements:
            collection_id = (
                f"{EDIT_TARGET_PREFIX}{'.'.join(placement.collection_path)}"
            )
            collection = edit_nodes.get(collection_id)
            if collection is None:
                continue
            group, definitions = _definition_group(
                placement,
                collection,
                owner.id,
                revision,
                dirty,
            )
            group_order[group.id] = placement.group_order
            nodes[group.id] = group
            nodes.update((definition.id, definition) for definition in definitions)
            child_ids.append(group.id)
        nodes[owner.id] = cast(ManageNode, replace(
            owner,
            revision=f"{owner.revision}:{revision}:definitions",
            child_ids=tuple(_ordered_ids(child_ids, group_order)),
        ))


def _resource_identity(
    placement: _Placement,
    child: Mapping[str, Any],
    index: int,
) -> Optional[Tuple[str, str]]:
    path = child.get("path")
    if (
        not isinstance(path, list)
        or len(path) <= len(placement.collection_path)
    ):
        return None
    authored_name = str(path[len(placement.collection_path)])
    if placement.identity.kind == "indexed-config":
        resource_name = (
            f"{placement.identity.prefix}"
            f"{index + placement.identity.first_index}"
        )
        return (
            f"config:{'.'.join(placement.collection_path)}:{index}",
            resource_name,
        )
    resource_name = (
        f"{placement.identity.prefix}{authored_name}"
        f"{placement.identity.suffix}"
    )
    return (
        f"resource:{placement.resource_plural}:{resource_name}",
        resource_name,
    )


def _diagnostic(value: Any) -> Optional[ManageDiagnostic]:
    if not isinstance(value, Mapping):
        return None
    severity = value.get("severity")
    message = value.get("message")
    if not isinstance(severity, str) or not isinstance(message, str):
        return None
    path = value.get("path")
    return ManageDiagnostic(
        severity=severity,
        message=message,
        path=tuple(
            str(part) for part in path
        ) if isinstance(path, list) else (),
        source=_optional_string(value.get("source")),
        code=_optional_string(value.get("code")),
        title=_optional_string(value.get("title")),
        remedy=_optional_string(value.get("remedy")),
        technical_detail=_optional_string(value.get("technicalDetail")),
    )


def _config_state(
    node: Mapping[str, Any],
    dirty: bool,
) -> ManageConfigState:
    errors, warnings = _validation_issue_counts(node)
    return ManageConfigState(
        validation_errors=errors,
        validation_warnings=warnings,
        draft_change_count=(
            _draft_change_count(node)
            if dirty
            else 0
        ),
    )


def _validation_issue_counts(
    node: Mapping[str, Any],
) -> Tuple[int, int]:
    counts = _mapping(node.get("statusCounts"))
    errors = sum(
        _nonnegative_int(counts.get(key))
        for key in ("errors", "required", "gated", "blocked")
    )
    warnings = _nonnegative_int(counts.get("warnings"))
    if errors == 0 and warnings == 0:
        status = node.get("status")
        if status in {"required", "error", "gated", "blocked"}:
            errors = 1
        elif status == "warning":
            warnings = 1

    child_counts = tuple(
        _validation_issue_counts(child)
        for child in _mapping_children(node)
        if child.get("valueKind") != "command"
    )
    errors = max(
        errors,
        sum(
            child_errors + child_warnings
            for child_errors, child_warnings in child_counts
            if child_errors > 0
        ),
    )
    warnings = max(
        warnings,
        sum(
            child_warnings
            for child_errors, child_warnings in child_counts
            if child_errors == 0
        ),
    )
    return errors, warnings


def _draft_change_count(node: Mapping[str, Any]) -> int:
    count = _nonnegative_int(node.get("draftChangeCount"))
    if count > 0:
        return count
    return 1 if node.get("draftChange") else 0


def _nonnegative_int(value: Any) -> int:
    return value if isinstance(value, int) and value > 0 else 0


def _mapping(value: Any) -> Mapping[str, Any]:
    return value if isinstance(value, Mapping) else {}


def _mapping_children(
    node: Mapping[str, Any],
) -> Tuple[Mapping[str, Any], ...]:
    return tuple(
        child
        for child in node.get("children") or ()
        if isinstance(child, Mapping)
    )


def _optional_string(value: Any) -> Optional[str]:
    return value if isinstance(value, str) else None
