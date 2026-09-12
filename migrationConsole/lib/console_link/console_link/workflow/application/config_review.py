"""Shared configuration review projection for draft and saved documents."""

from dataclasses import dataclass
from typing import Any, Iterable, Mapping, Optional


@dataclass(frozen=True)
class ConfigReviewChange:
    resource_id: Optional[str]
    resource_label: Optional[str]
    path: str
    label: str
    kind: str


def validation_messages(
    validation: Mapping[str, Any],
) -> tuple[str, ...]:
    messages = [
        str(item.get("message"))
        for item in validation.get("diagnostics") or []
        if isinstance(item, Mapping) and item.get("message")
    ]
    messages.extend(
        str(message)
        for message in validation.get("errors") or []
        if message
    )
    return tuple(dict.fromkeys(messages))


def review_changes(
    edit_state: Mapping[str, Any],
    snapshot: Optional[Any],
) -> tuple[ConfigReviewChange, ...]:
    changes: list[ConfigReviewChange] = []
    seen: set[tuple[Optional[str], str]] = set()

    for node in getattr(snapshot, "nodes", {}).values() if snapshot else ():
        if getattr(node, "kind", None) != "resource":
            continue
        resource_id = str(getattr(node, "id", ""))
        resource_label = str(getattr(node, "label", ""))
        for comparison in getattr(node, "comparisons", ()):
            if not getattr(comparison, "pending_changed", False):
                continue
            path = str(getattr(comparison, "path", ""))
            key = (resource_id, path)
            if key in seen:
                continue
            seen.add(key)
            changes.append(ConfigReviewChange(
                resource_id=resource_id,
                resource_label=resource_label,
                path=path,
                label=str(getattr(comparison, "label", path)),
                kind="field",
            ))
        summary = str(getattr(node, "value_summary", "") or "")
        if "pending submission" in summary.lower():
            key = (resource_id, "$presence")
            if key not in seen:
                seen.add(key)
                changes.append(ConfigReviewChange(
                    resource_id=resource_id,
                    resource_label=resource_label,
                    path="$presence",
                    label=summary,
                    kind="resource",
                ))

    def visit(nodes: Iterable[Mapping[str, Any]]) -> None:
        for node in nodes:
            children = node.get("children") or []
            if (
                node.get("status") == "changed"
                and (
                    not children
                    or node.get("valueKind")
                    in {"scalar", "boolean", "union"}
                )
            ):
                path = ".".join(str(part) for part in node.get("path") or [])
                key = (None, path)
                if path and key not in seen:
                    seen.add(key)
                    changes.append(ConfigReviewChange(
                        resource_id=None,
                        resource_label=None,
                        path=path,
                        label=str(node.get("label") or path),
                        kind="field",
                    ))
            visit(children)

    visit(edit_state.get("nodes") or [])
    return tuple(changes)
