/** Stable sections used by portable Snow App configuration packages. */
export const DATA_SECTIONS = [
  "api-config",
  "model-settings",
  "system-settings",
  "mcp",
  "prompts",
  "hooks",
  "sub-agents",
  "keyboard-shortcuts",
  "theme",
  "skills",
  "plugins",
] as const;

export type DataSection = (typeof DATA_SECTIONS)[number];

/** Version of the portable `.snow-config` manifest contract. */
export const DATA_MANAGEMENT_FORMAT_VERSION = 1 as const;

export type DataManifestFile = {
  path: string;
  sha256: string;
  sizeBytes: number;
};

export type DataManifest = {
  formatVersion: typeof DATA_MANAGEMENT_FORMAT_VERSION;
  appVersion: string;
  schemaVersion: number;
  createdAt: string;
  deviceId: string;
  sections: DataSection[];
  containsSecrets: boolean;
  encrypted: boolean;
  files: DataManifestFile[];
};

export type DataManagementTaskOperation =
  | "config-export"
  | "config-import"
  | "backup-create"
  | "backup-restore"
  | "sync";

export type DataManagementTaskStatus =
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

export type DataManagementProgress = {
  taskId: string;
  operation: DataManagementTaskOperation;
  status: DataManagementTaskStatus;
  phase: string;
  completed: number;
  total: number;
  currentItem: string;
  cancellable: boolean;
  error?: string;
};

export type DataManagementSyncIntervalMinutes = 0 | 15 | 30 | 60;

export type DataManagementSyncMode = "config" | "mirror";

export type DataManagementBackupFrequency = "6h" | "12h" | "daily" | "weekly";

export type DataManagementBackupSettings = {
  enabled: boolean;
  frequency: DataManagementBackupFrequency;
  retentionCount: number;
  directory: string;
  includeArchive: boolean;
  includeAttachments: boolean;
  beforeImport: boolean;
  beforeRestore: boolean;
};

/** Non-secret WebDAV settings. Passwords and encryption keys are never here. */
export type DataManagementWebDavSettings = {
  endpoint: string;
  remoteRoot: string;
  username: string;
  syncEnabled: boolean;
  syncIntervalMinutes: DataManagementSyncIntervalMinutes;
  syncMode: DataManagementSyncMode;
  allowInsecureHttp: boolean;
};

export type DataManagementSettings = {
  deviceName: string;
  webdav: DataManagementWebDavSettings;
  backup: DataManagementBackupSettings;
};

export type DataManagementSettingsPatch = {
  deviceName?: string;
  webdav?: Partial<DataManagementWebDavSettings>;
  backup?: Partial<DataManagementBackupSettings>;
};

export type DataManagementCredentialKind =
  | "webdav-password"
  | "sync-master-key";

/** This value is write-only from the Renderer perspective. */
export type DataManagementCredentialUpdate = {
  kind: DataManagementCredentialKind;
  value: string;
};

export type DataManagementCredentialStatus = {
  webdavPasswordConfigured: boolean;
  syncMasterKeyConfigured: boolean;
};

export type DataManagementBackupRecord = {
  id: string;
  path: string;
  createdAt: string;
  reason: string;
  appVersion: string;
  schemaVersion: number;
  sizeBytes: number;
  includesArchive: boolean;
  includesAttachments: boolean;
  encrypted: boolean;
  integrity: "unchecked" | "valid" | "invalid";
};

export type DataManagementImportPreview = {
  path: string;
  encrypted: boolean;
  containsSecrets: boolean;
  formatVersion: number;
  schemaVersion: number;
  sections: DataSection[];
  rows: number;
  estimatedBytes: number;
  deviceSpecificItems: number;
};

export type DataManagementExportRequest = {
  sections: DataSection[];
  includeSecrets: boolean;
  password?: string;
};

export type DataManagementImportRequest = {
  sections: DataSection[];
  password?: string;
  replaceSelected: boolean;
};

export type DataManagementConflictChoice = "local" | "remote" | "keep-both";

export type DataManagementSyncState = {
  status: "idle" | "running" | "offline" | "auth-error" | "conflict" | "quota-error" | "error";
  mode: DataManagementSyncMode;
  lastSuccessAt: string | null;
  baseRevision: number;
  pendingUploadBytes: number;
  weakConflictProtection: boolean;
  conflict: {
    localRevision: number;
    remoteRevision: number;
    remoteDeviceName: string;
  } | null;
  lastError: string | null;
};

export type DataManagementState = {
  deviceId: string;
  deviceName: string;
  appVersion: string;
  manifestFormatVersion: typeof DATA_MANAGEMENT_FORMAT_VERSION;
  safeStorageAvailable: boolean;
  credentialStatus: DataManagementCredentialStatus;
  activeTask: DataManagementProgress | null;
  backups: DataManagementBackupRecord[];
  sync: DataManagementSyncState;
};
