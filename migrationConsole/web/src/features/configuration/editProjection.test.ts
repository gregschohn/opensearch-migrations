import { expect, test } from "vitest";

import type { ManageSnapshot } from "../../api/client";
import { projectEditSnapshot } from "./editProjection";
import type { PendingResourceAddition } from "./resourceAdds";


test("projects a named pending resource into an otherwise empty group", () => {
  const snapshot: ManageSnapshot = {
    formatVersion: 1,
    revision: "draft-1",
    observedAt: "2026-09-10T12:00:00Z",
    namespace: "ma",
    workflowName: "migration-workflow",
    workflow: null,
    rootIds: ["section:Snapshot Migration"],
    nodes: {
      "section:Snapshot Migration": {
        id: "section:Snapshot Migration",
        revision: "section-1",
        parentId: null,
        childIds: [],
        kind: "section",
        label: "Snapshot Migration",
        description: null,
        status: "ok",
        phase: null,
        valueSummary: null,
        diagnostics: [],
        capabilities: [],
        details: [],
        relationships: [],
        comparisons: [],
        resourcePlural: null,
        resourceName: null,
        resourceType: null,
        configPresence: {},
      },
    },
  };
  const addition: PendingResourceAddition = {
    id: "optimistic-add:edit:snapshotMigrationConfigs.0",
    editTargetId: "edit:snapshotMigrationConfigs.0",
    groupId: "section:Snapshot Migration",
    groupLabel: "Snapshot Migration",
    label: "visual-check",
    nodeKind: "resource",
    resourceName: "visual-check",
    resourcePlural: "snapshotmigrations",
    resourceType: "Snapshot migration",
    sectionId: "section:Snapshot Migration",
    sectionLabel: "Snapshot Migration",
    status: "awaiting-draft",
  };

  const projected = projectEditSnapshot(snapshot, [addition]);

  expect(projected.nodes["section:Snapshot Migration"].childIds).toEqual([
    addition.id,
  ]);
  expect(projected.nodes[addition.id]).toMatchObject({
    parentId: "section:Snapshot Migration",
    label: "visual-check",
    status: "changed",
    valueSummary: "Addition pending submission",
    capabilities: [{
      kind: "edit",
      editTargetId: "edit:snapshotMigrationConfigs.0",
    }],
  });
});
