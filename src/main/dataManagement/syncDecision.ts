import type { DataManagementSyncMode } from "../../preload/types/dataManagement";

export type SyncDecision = "upload" | "download" | "conflict" | "none";

export type SyncDecisionInput = {
  localContentHash: string;
  localMode: DataManagementSyncMode;
  baseRevision: number;
  baseLocalHash: string;
  baseRemoteHash: string;
  remoteRevision: number;
  remoteContentHash: string;
  remoteMode: DataManagementSyncMode;
};

/**
 * Decide a two-way sync action without touching local or remote state.
 * A mode transition always requires an explicit choice. A missing common
 * base does as well unless both sides are already content-equivalent.
 */
export const decideSyncAction = (input: SyncDecisionInput): SyncDecision => {
  if (input.localMode !== input.remoteMode) return "conflict";

  if (input.baseRevision === 0) {
    return input.localContentHash === input.remoteContentHash ? "none" : "conflict";
  }

  if (input.localContentHash === input.remoteContentHash) return "none";

  const localChanged = input.localContentHash !== input.baseLocalHash;
  const remoteChanged =
    input.remoteRevision !== input.baseRevision ||
    input.remoteContentHash !== input.baseRemoteHash;

  if (!localChanged && remoteChanged) return "download";
  if (localChanged && !remoteChanged) return "upload";
  return "conflict";
};
