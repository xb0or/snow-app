import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import type {
  DataManagementSyncMode,
  DataManagementSyncState,
} from "../../preload/types/dataManagement";
import { getDataManagementDirectory, getSyncStatePath } from "./paths";

export type PersistedSyncState = {
  baseRevision: number;
  baseLocalHash: string;
  baseRemoteHash: string;
  lastSuccessAt: string | null;
  mode: DataManagementSyncMode;
  weakConflictProtection: boolean;
  conflict: DataManagementSyncState["conflict"];
  lastError: string | null;
  lastStatus: DataManagementSyncState["status"];
};

const DEFAULT_STATE: PersistedSyncState = {
  baseRevision: 0,
  baseLocalHash: "",
  baseRemoteHash: "",
  lastSuccessAt: null,
  mode: "config",
  weakConflictProtection: false,
  conflict: null,
  lastError: null,
  lastStatus: "idle",
};

const ensureDirectory = (): void => {
  mkdirSync(getDataManagementDirectory(), { recursive: true, mode: 0o700 });
  try {
    chmodSync(getDataManagementDirectory(), 0o700);
  } catch {
    // Unsupported on some filesystems.
  }
};

export const readPersistedSyncState = (): PersistedSyncState => {
  ensureDirectory();
  if (!existsSync(getSyncStatePath())) return { ...DEFAULT_STATE };
  try {
    const value = JSON.parse(readFileSync(getSyncStatePath(), "utf8")) as Partial<PersistedSyncState>;
    const lastStatus = ["idle", "running", "offline", "auth-error", "conflict", "quota-error", "error"].includes(
      value.lastStatus as string
    )
      ? (value.lastStatus as PersistedSyncState["lastStatus"])
      : "idle";
    return {
      ...DEFAULT_STATE,
      ...value,
      conflict: value.conflict ?? null,
      lastSuccessAt: value.lastSuccessAt ?? null,
      lastStatus,
    };
  } catch {
    return { ...DEFAULT_STATE };
  }
};

export const writePersistedSyncState = (value: PersistedSyncState): void => {
  ensureDirectory();
  const temporaryPath = `${getSyncStatePath()}.${process.pid}.tmp`;
  writeFileSync(temporaryPath, JSON.stringify(value, null, 2), { mode: 0o600 });
  renameSync(temporaryPath, getSyncStatePath());
};

export const clearPersistedSyncState = (): void => {
  writePersistedSyncState({ ...DEFAULT_STATE });
};
