import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import type {
  DataManagementSettings,
  DataManagementSettingsPatch,
  DataManagementBackupFrequency,
  DataManagementBackupSettings,
  DataManagementSyncIntervalMinutes,
  DataManagementSyncMode,
  DataManagementWebDavSettings,
} from "../../preload/types/dataManagement";
import { getOrCreateDeviceIdentity, setDeviceName } from "./deviceIdentity";
import { getDataManagementDirectory, getSettingsPath } from "./paths";

type StoredSettings = {
  webdav?: Partial<DataManagementWebDavSettings>;
  backup?: Partial<DataManagementBackupSettings>;
};

const DEFAULT_WEBDAV_SETTINGS: DataManagementWebDavSettings = {
  endpoint: "",
  remoteRoot: "snow-app",
  username: "",
  syncEnabled: false,
  syncIntervalMinutes: 0,
  syncMode: "config",
  allowInsecureHttp: false,
};

const DEFAULT_BACKUP_SETTINGS: DataManagementBackupSettings = {
  enabled: false,
  frequency: "daily",
  retentionCount: 7,
  directory: "",
  includeArchive: true,
  includeAttachments: false,
  beforeImport: true,
  beforeRestore: true,
};

const VALID_INTERVALS = new Set<DataManagementSyncIntervalMinutes>([
  0,
  15,
  30,
  60,
]);
const VALID_FREQUENCIES = new Set<DataManagementBackupFrequency>([
  "6h",
  "12h",
  "daily",
  "weekly",
]);
const VALID_SYNC_MODES = new Set<DataManagementSyncMode>(["config", "mirror"]);

const ensureDirectory = (): void => {
  const directory = getDataManagementDirectory();
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  try {
    chmodSync(directory, 0o700);
  } catch {
    // Ignore unsupported permission operations.
  }
};

const readStoredSettings = (): StoredSettings => {
  ensureDirectory();
  const path = getSettingsPath();
  if (!existsSync(path)) {
    return {};
  }
  try {
    const value: unknown = JSON.parse(readFileSync(path, "utf8"));
    if (!value || typeof value !== "object") {
      return {};
    }
    const webdav = (value as Record<string, unknown>).webdav;
    const backup = (value as Record<string, unknown>).backup;
    return {
      ...(webdav && typeof webdav === "object"
        ? { webdav: webdav as Partial<DataManagementWebDavSettings> }
        : {}),
      ...(backup && typeof backup === "object"
        ? { backup: backup as Partial<DataManagementBackupSettings> }
        : {}),
    };
  } catch {
    return {};
  }
};

const writeStoredSettings = (settings: StoredSettings): void => {
  ensureDirectory();
  const path = getSettingsPath();
  const stagingPath = `${path}.${process.pid}.tmp`;
  writeFileSync(stagingPath, JSON.stringify(settings, null, 2), {
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

const normalizeWebDavSettings = (
  value: Partial<DataManagementWebDavSettings> | undefined
): DataManagementWebDavSettings => {
  const endpoint = typeof value?.endpoint === "string" ? value.endpoint : "";
  const remoteRoot =
    typeof value?.remoteRoot === "string" && value.remoteRoot.trim()
      ? value.remoteRoot.trim()
      : DEFAULT_WEBDAV_SETTINGS.remoteRoot;
  const username = typeof value?.username === "string" ? value.username : "";
  const syncEnabled = value?.syncEnabled === true;
  const interval = value?.syncIntervalMinutes;
  const syncIntervalMinutes = VALID_INTERVALS.has(
    interval as DataManagementSyncIntervalMinutes
  )
    ? (interval as DataManagementSyncIntervalMinutes)
    : DEFAULT_WEBDAV_SETTINGS.syncIntervalMinutes;
  const syncMode = VALID_SYNC_MODES.has(value?.syncMode as DataManagementSyncMode)
    ? (value?.syncMode as DataManagementSyncMode)
    : DEFAULT_WEBDAV_SETTINGS.syncMode;

  return {
    endpoint,
    remoteRoot,
    username,
    syncEnabled,
    syncIntervalMinutes,
    syncMode,
    allowInsecureHttp: value?.allowInsecureHttp === true,
  };
};

const normalizeBackupSettings = (
  value: Partial<DataManagementBackupSettings> | undefined
): DataManagementBackupSettings => {
  const frequency = VALID_FREQUENCIES.has(value?.frequency as DataManagementBackupFrequency)
    ? (value?.frequency as DataManagementBackupFrequency)
    : DEFAULT_BACKUP_SETTINGS.frequency;
  const retentionCount =
    typeof value?.retentionCount === "number" && Number.isInteger(value.retentionCount)
      ? Math.min(100, Math.max(3, value.retentionCount))
      : DEFAULT_BACKUP_SETTINGS.retentionCount;
  return {
    enabled: value?.enabled === true,
    frequency,
    retentionCount,
    directory: typeof value?.directory === "string" ? value.directory.trim() : "",
    includeArchive: value?.includeArchive !== false,
    includeAttachments: value?.includeAttachments === true,
    beforeImport: value?.beforeImport !== false,
    beforeRestore: value?.beforeRestore !== false,
  };
};

export const getDataManagementSettings = (): DataManagementSettings => {
  const identity = getOrCreateDeviceIdentity();
  const stored = readStoredSettings();
  return {
    deviceName: identity.deviceName,
    webdav: normalizeWebDavSettings(stored.webdav),
    backup: normalizeBackupSettings(stored.backup),
  };
};

export const updateDataManagementSettings = (
  patch: DataManagementSettingsPatch
): DataManagementSettings => {
  if (patch.deviceName !== undefined) {
    setDeviceName(patch.deviceName);
  }

  const current = getDataManagementSettings();
  const nextWebdav = normalizeWebDavSettings({
    ...current.webdav,
    ...(patch.webdav ?? {}),
  });
  const nextBackup = normalizeBackupSettings({
    ...current.backup,
    ...(patch.backup ?? {}),
  });

  writeStoredSettings({ webdav: nextWebdav, backup: nextBackup });
  return {
    deviceName: getOrCreateDeviceIdentity().deviceName,
    webdav: nextWebdav,
    backup: nextBackup,
  };
};
