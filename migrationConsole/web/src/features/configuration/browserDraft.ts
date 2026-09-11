import {
  annotateDraftChanges,
  applyEditOperationToObject,
  projectConfigYaml,
  type EditStateV1,
} from "@opensearch-migrations/config-edit-core";
import { parse } from "yaml";

import type {
  ConfigDraft,
  ConfigurationDocument,
  EditOperation,
  ManageSnapshot,
} from "../../api/client";


declare global {
  var __WORKFLOW_BROWSER_LOCAL_EDITING__: boolean | undefined;
}


export interface BrowserConfigDraft extends ConfigDraft {
  baseEditState: ConfigDraft["editState"];
  config: unknown;
  persistedRevision: string;
  rawDocument: string;
  savedRawDocument: string;
}


export const BROWSER_CONFIG_DRAFT_QUERY_KEY = ["browser-config-draft"] as const;


let localRevision = 0;


export function browserLocalEditingEnabled(): boolean {
  if (globalThis.__WORKFLOW_BROWSER_LOCAL_EDITING__ !== undefined) {
    return globalThis.__WORKFLOW_BROWSER_LOCAL_EDITING__;
  }
  return import.meta.env.VITE_BROWSER_LOCAL_CONFIG_EDITING !== "false";
}


function nextRevision(persistedRevision: string): string {
  localRevision += 1;
  return `browser:${persistedRevision}:${localRevision}`;
}


function apiEditState(editState: EditStateV1): ConfigDraft["editState"] {
  return editState as ConfigDraft["editState"];
}


function parseYaml(rawYaml: string): unknown {
  return parse(rawYaml) as unknown;
}


function projectedDraft(
  document: ConfigurationDocument,
  navigation?: ManageSnapshot | null,
): BrowserConfigDraft {
  const projection = projectConfigYaml(document.rawYaml);
  const editState = apiEditState(projection.editState);
  return {
    baseRevision: document.persistedRevision,
    draftRevision: nextRevision(document.persistedRevision),
    dirty: false,
    editState,
    navigation: navigation ?? null,
    rawYaml: projection.editState.provenance.mode === "raw"
      ? document.rawYaml
      : undefined,
    notices: [],
    baseEditState: structuredClone(editState),
    config: projection.config,
    persistedRevision: document.persistedRevision,
    rawDocument: document.rawYaml,
    savedRawDocument: document.rawYaml,
  };
}


export function createBrowserConfigDraft(
  document: ConfigurationDocument,
  navigation?: ManageSnapshot | null,
): BrowserConfigDraft {
  return projectedDraft(document, navigation);
}


export function applyBrowserEditOperation(
  draft: BrowserConfigDraft,
  operation: EditOperation,
): BrowserConfigDraft {
  if (draft.config === null) {
    throw new Error(
      "Repair the YAML before applying structured configuration changes.",
    );
  }
  const result = applyEditOperationToObject(
    draft.config,
    operation,
  );
  const config = result.yaml.trim() === "" ? {} : parseYaml(result.yaml);
  const editState = apiEditState(annotateDraftChanges(
    result.editState,
    draft.baseEditState as EditStateV1,
  ));
  return {
    ...draft,
    draftRevision: nextRevision(draft.persistedRevision),
    dirty: result.yaml !== draft.savedRawDocument,
    editState,
    rawYaml: undefined,
    notices: [],
    config,
    rawDocument: result.yaml,
  };
}


export function replaceBrowserConfigYaml(
  draft: BrowserConfigDraft,
  rawYaml: string,
): BrowserConfigDraft {
  const projection = projectConfigYaml(rawYaml);
  const projectedEditState = projection.editState.provenance.mode === "raw"
    ? projection.editState
    : annotateDraftChanges(
      projection.editState,
      draft.baseEditState as EditStateV1,
    );
  return {
    ...draft,
    draftRevision: nextRevision(draft.persistedRevision),
    dirty: rawYaml !== draft.savedRawDocument,
    editState: apiEditState(projectedEditState),
    rawYaml: projection.editState.provenance.mode === "raw"
      ? rawYaml
      : undefined,
    notices: [],
    config: projection.config,
    rawDocument: rawYaml,
  };
}


export function savedBrowserConfigDraft(
  document: ConfigurationDocument,
  previous: BrowserConfigDraft,
  navigation?: ManageSnapshot | null,
): BrowserConfigDraft {
  return projectedDraft(document, navigation ?? previous.navigation);
}


export function revertedBrowserConfigDraft(
  draft: BrowserConfigDraft,
): BrowserConfigDraft {
  return projectedDraft({
    modelVersion: "1",
    persistedRevision: draft.persistedRevision,
    rawYaml: draft.savedRawDocument,
  }, draft.navigation);
}


export function withBrowserDraftNavigation(
  draft: BrowserConfigDraft,
  navigation: ManageSnapshot | null | undefined,
): BrowserConfigDraft {
  return navigation === undefined || navigation === draft.navigation
    ? draft
    : { ...draft, navigation };
}
