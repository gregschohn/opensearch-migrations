import { describe, expect, it } from "vitest";

import type { ConfigurationDocument } from "../../api/client";
import {
  applyBrowserEditOperation,
  createBrowserConfigDraft,
  markBrowserConfigDraftStale,
  replaceBrowserConfigYaml,
  revertedBrowserConfigDraft,
} from "./browserDraft";


const document: ConfigurationDocument = {
  modelVersion: "1",
  persistedRevision: "saved-1",
  rawYaml: `
sourceClusters:
  source:
    endpoint: https://source.example.com:9200
    version: ES 7.10
    allowInsecure: false
targetClusters:
  target:
    endpoint: https://target.example.com:9200
snapshotMigrationConfigs: []
`,
};


describe("browser configuration drafts", () => {
  it("applies ordinary edits without a server-owned revision", () => {
    const draft = createBrowserConfigDraft(document);
    const updated = applyBrowserEditOperation(draft, {
      op: "set",
      path: ["sourceClusters", "source", "allowInsecure"],
      value: true,
    });

    expect(updated.dirty).toBe(true);
    expect(updated.draftRevision).toMatch(/^browser:saved-1:/);
    expect(updated.rawDocument).toContain("allowInsecure: true");
    expect(updated.editState.nodes).not.toBe(draft.editState.nodes);
  });

  it("keeps malformed YAML in repair mode and reverts it in memory", () => {
    const draft = createBrowserConfigDraft(document);
    const malformed = replaceBrowserConfigYaml(
      draft,
      "sourceClusters: [",
    );

    expect(malformed.rawYaml).toBe("sourceClusters: [");
    expect(malformed.editState.provenance.mode).toBe("raw");
    expect(malformed.editState.validation.valid).toBe(false);

    const reverted = revertedBrowserConfigDraft(malformed);
    expect(reverted.dirty).toBe(false);
    expect(reverted.rawYaml).toBeUndefined();
    expect(reverted.rawDocument).toBe(document.rawYaml);
  });

  it("retains dirty local work when its saved base changes remotely", () => {
    const draft = applyBrowserEditOperation(
      createBrowserConfigDraft(document),
      {
        op: "set",
        path: ["sourceClusters", "source", "allowInsecure"],
        value: true,
      },
    );

    const stale = markBrowserConfigDraftStale(draft, "saved-2");

    expect(stale.dirty).toBe(true);
    expect(stale.baseStale).toBe(true);
    expect(stale.persistedRevision).toBe("saved-1");
    expect(stale.remotePersistedRevision).toBe("saved-2");
    expect(stale.rawDocument).toContain("allowInsecure: true");
  });
});
