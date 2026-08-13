import { safeStorage } from "electron";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import type {
  DataManagementCredentialKind,
  DataManagementCredentialStatus,
} from "../../preload/types/dataManagement";
import { getDataManagementDirectory, getCredentialsPath } from "./paths";

type StoredCredentials = Partial<Record<DataManagementCredentialKind, string>>;

const ensureDirectory = (): void => {
  const directory = getDataManagementDirectory();
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  try {
    chmodSync(directory, 0o700);
  } catch {
    // Ignore unsupported permission operations.
  }
};

const readStoredCredentials = (): StoredCredentials => {
  ensureDirectory();
  const path = getCredentialsPath();
  if (!existsSync(path)) {
    return {};
  }
  try {
    const value: unknown = JSON.parse(readFileSync(path, "utf8"));
    if (!value || typeof value !== "object") {
      return {};
    }
    const record = value as Record<string, unknown>;
    const result: StoredCredentials = {};
    for (const kind of ["webdav-password", "sync-master-key"] as const) {
      if (typeof record[kind] === "string" && record[kind].length > 0) {
        result[kind] = record[kind];
      }
    }
    return result;
  } catch {
    // A corrupt credential file must not make the app crash. The next save
    // replaces it after safeStorage has been checked.
    return {};
  }
};

const writeStoredCredentials = (credentials: StoredCredentials): void => {
  ensureDirectory();
  const path = getCredentialsPath();
  const stagingPath = `${path}.${process.pid}.tmp`;
  writeFileSync(stagingPath, JSON.stringify(credentials, null, 2), {
    encoding: "utf8",
    mode: 0o600,
  });
  try {
    chmodSync(stagingPath, 0o600);
  } catch {
    // Ignore unsupported permission operations.
  }
  renameSync(stagingPath, path);
};

export const isDataManagementSafeStorageAvailable = (): boolean => {
  try {
    return safeStorage.isEncryptionAvailable();
  } catch {
    return false;
  }
};

export const getDataManagementCredentialStatus = (): DataManagementCredentialStatus => {
  const stored = readStoredCredentials();
  return {
    webdavPasswordConfigured: Boolean(stored["webdav-password"]),
    syncMasterKeyConfigured: Boolean(stored["sync-master-key"]),
  };
};

export const saveDataManagementCredential = (
  kind: DataManagementCredentialKind,
  value: string
): DataManagementCredentialStatus => {
  if (!isDataManagementSafeStorageAvailable()) {
    throw new Error(
      "OS-level encryption (safeStorage) is not available. Sensitive data cannot be stored securely on this system."
    );
  }
  const normalized = value.trim();
  if (!normalized) {
    throw new Error("Credential value is required");
  }

  const stored = readStoredCredentials();
  stored[kind] = safeStorage.encryptString(normalized).toString("base64");
  writeStoredCredentials(stored);
  return getDataManagementCredentialStatus();
};

export const deleteDataManagementCredential = (
  kind: DataManagementCredentialKind
): DataManagementCredentialStatus => {
  const stored = readStoredCredentials();
  delete stored[kind];
  writeStoredCredentials(stored);
  return getDataManagementCredentialStatus();
};

/** Main-process-only helper for future WebDAV requests; never expose via IPC. */
export const decryptDataManagementCredential = (
  kind: DataManagementCredentialKind
): string | null => {
  const encrypted = readStoredCredentials()[kind];
  if (!encrypted) {
    return null;
  }
  if (!isDataManagementSafeStorageAvailable()) {
    throw new Error("OS-level encryption (safeStorage) is not available");
  }
  return safeStorage.decryptString(Buffer.from(encrypted, "base64"));
};
