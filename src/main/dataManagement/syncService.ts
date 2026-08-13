import { app } from "electron";
import { randomUUID } from "node:crypto";
import {
  readFileSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import type {
  DataManagementSyncMode,
  DataManagementSyncState,
} from "../../preload/types/dataManagement";
import type { NativeBridge } from "../native/types";
import {
  DATA_SECTIONS,
  type DataSection,
} from "../../preload/types/dataManagement";
import { decryptDataManagementCredential } from "./credentials";
import { getOrCreateDeviceIdentity } from "./deviceIdentity";
import { createDatabaseBackup, stageDatabaseRestore } from "./backupService";
import { encryptBundlePayload, sha256Buffer, decryptBundlePayload } from "./cryptoBundle";
import { getDataManagementDirectory } from "./paths";
import {
  getDataManagementSettings,
  updateDataManagementSettings,
} from "./settingsStore";
import { decideSyncAction } from "./syncDecision";
import {
  readPersistedSyncState,
  type PersistedSyncState,
  writePersistedSyncState,
} from "./syncStateStore";
import { WebDavClient, WebDavError } from "./webdavClient";

const REMOTE_FORMAT_VERSION = 1;
const REMOTE_STATE_NAME = "state.json";
const ALL_SECTIONS = [...DATA_SECTIONS] as DataSection[];

type RemoteState = {
  formatVersion: 1;
  revision: number;
  objectHash: string;
  contentHash: string;
  mode: DataManagementSyncMode;
  deviceId: string;
  deviceName: string;
  createdAt: string;
  updatedAt: string;
};

export type SyncRunResult = {
  status: DataManagementSyncState["status"];
  revision: number;
  changed: "none" | "uploaded" | "downloaded" | "conflict" | "staged-mirror";
  weakConflictProtection: boolean;
  restartRequired: boolean;
};

export class SyncConflictError extends Error {
  readonly remoteRevision: number;
  readonly remoteDeviceName: string;

  constructor(remote: RemoteState) {
    super("WebDAV has changes from another device; automatic sync is paused");
    this.name = "SyncConflictError";
    this.remoteRevision = remote.revision;
    this.remoteDeviceName = remote.deviceName;
  }
}

const rootFromSettings = (): string => {
  const root = getDataManagementSettings().webdav.remoteRoot.trim();
  if (!root || root.includes("..")) throw new Error("WebDAV remote root is invalid");
  return root;
};

const clientFromSettings = (): WebDavClient => {
  const settings = getDataManagementSettings();
  const password = decryptDataManagementCredential("webdav-password");
  if (!password) throw new Error("Configure a WebDAV password before syncing");
  return new WebDavClient(
    settings.webdav.endpoint,
    settings.webdav.username,
    password,
    settings.webdav.allowInsecureHttp
  );
};

const syncKey = (): string => {
  const key = decryptDataManagementCredential("sync-master-key");
  if (!key) throw new Error("Configure a sync encryption password before syncing");
  return key;
};

const stateUrl = (client: WebDavClient, root: string): string => client.url(root, "v1", REMOTE_STATE_NAME);
const objectUrl = (client: WebDavClient, root: string, hash: string): string =>
  client.url(root, "v1", "objects", `${hash}.bin`);

const parseRemoteState = (body: Buffer): RemoteState => {
  let value: unknown;
  try {
    value = JSON.parse(body.toString("utf8"));
  } catch {
    throw new Error("Remote WebDAV state is not valid JSON");
  }
  if (!value || typeof value !== "object") throw new Error("Remote WebDAV state is invalid");
  const state = value as Partial<RemoteState>;
  if (
    state.formatVersion !== REMOTE_FORMAT_VERSION ||
    typeof state.revision !== "number" ||
    !Number.isSafeInteger(state.revision) ||
    state.revision < 1 ||
    typeof state.objectHash !== "string" ||
    !/^[a-f0-9]{64}$/.test(state.objectHash) ||
    typeof state.contentHash !== "string" ||
    !/^[a-f0-9]{64}$/.test(state.contentHash) ||
    (state.mode !== "config" && state.mode !== "mirror") ||
    typeof state.deviceId !== "string" ||
    typeof state.deviceName !== "string"
  ) {
    throw new Error("Remote WebDAV state has an unsupported format");
  }
  return state as RemoteState;
};

const parseJsonState = (body: Buffer): RemoteState => parseRemoteState(body);

const fetchRemoteState = async (
  client: WebDavClient,
  root: string
): Promise<{ state: RemoteState | null; etag: string | null }> => {
  const response = await client.get(stateUrl(client, root));
  if (!response) return { state: null, etag: null };
  return { state: parseJsonState(response.body), etag: response.etag };
};

const writeDeviceState = async (client: WebDavClient, root: string): Promise<void> => {
  const identity = getOrCreateDeviceIdentity();
  const body = Buffer.from(JSON.stringify({
    formatVersion: 1,
    deviceId: identity.deviceId,
    deviceName: identity.deviceName,
    appVersion: app.getVersion(),
    updatedAt: new Date().toISOString(),
  }), "utf8");
  await client.put(client.url(root, "v1", "devices", `${identity.deviceId}.json`), body, null);
};

const saveSyncState = (patch: Partial<PersistedSyncState>): PersistedSyncState => {
  const current = readPersistedSyncState();
  const next = { ...current, ...patch };
  if (patch.lastError === null) {
    next.lastStatus = "idle";
  }
  writePersistedSyncState(next);
  return next;
};

const makeRemoteState = (
  previous: RemoteState | null,
  objectHash: string,
  contentHash: string,
  mode: DataManagementSyncMode
): RemoteState => {
  const identity = getOrCreateDeviceIdentity();
  const now = new Date().toISOString();
  return {
    formatVersion: 1,
    revision: (previous?.revision ?? 0) + 1,
    objectHash,
    contentHash,
    mode,
    deviceId: identity.deviceId,
    deviceName: identity.deviceName,
    createdAt: previous?.createdAt ?? now,
    updatedAt: now,
  };
};

const assertRemoteObjectHash = (payload: Buffer, expected: string): void => {
  if (sha256Buffer(payload) !== expected) throw new Error("Remote object checksum mismatch");
};

const uploadStateWithCas = async (
  client: WebDavClient,
  root: string,
  previous: RemoteState | null,
  etag: string | null,
  next: RemoteState
): Promise<void> => {
  let conditionalEtag = etag;
  if (!conditionalEtag && previous) {
    const secondRead = await fetchRemoteState(client, root);
    if (
      !secondRead.state ||
      secondRead.state.revision !== previous.revision ||
      secondRead.state.objectHash !== previous.objectHash
    ) {
      throw new SyncConflictError(secondRead.state ?? previous);
    }
    conditionalEtag = secondRead.etag;
  }
  try {
    await client.put(
      stateUrl(client, root),
      Buffer.from(JSON.stringify(next, null, 2), "utf8"),
      conditionalEtag,
      previous === null
    );
  } catch (error) {
    if (error instanceof WebDavError && error.status === 412) {
      const latest = await fetchRemoteState(client, root);
      if (
        latest.state?.revision === next.revision &&
        latest.state.objectHash === next.objectHash &&
        latest.state.contentHash === next.contentHash &&
        latest.state.mode === next.mode &&
        latest.state.deviceId === next.deviceId
      ) {
        return;
      }
      if (latest.state) throw new SyncConflictError(latest.state);
    }
    throw error;
  }
};

const uploadObject = async (
  client: WebDavClient,
  root: string,
  hash: string,
  payload: Buffer
): Promise<void> => {
  try {
    await client.put(objectUrl(client, root, hash), payload, null, true);
  } catch (error) {
    if (!(error instanceof WebDavError && error.status === 412)) throw error;
  }
};

const buildLocalPayload = async (
  native: NativeBridge,
  mode: DataManagementSyncMode,
  password: string
): Promise<{ payload: Buffer; contentHash: string; mirrorPath?: string }> => {
  if (mode === "config") {
    const configJson = await native.exportDataManagementConfig(JSON.stringify(ALL_SECTIONS), true);
    return {
      payload: await encryptBundlePayload(Buffer.from(configJson, "utf8"), password),
      contentHash: sha256Buffer(configJson),
    };
  }
  const backup = await createDatabaseBackup(native, "sync-mirror", true);
  const bytes = readFileSync(backup.path);
  return {
    payload: await encryptBundlePayload(bytes, password),
    contentHash: sha256Buffer(bytes),
    mirrorPath: backup.path,
  };
};

const applyRemoteConfig = async (
  native: NativeBridge,
  payload: Buffer,
  password: string,
  createSafetySnapshot: boolean
): Promise<void> => {
  const configJson = (await decryptBundlePayload(payload, password)).toString("utf8");
  const parsed = JSON.parse(configJson) as { sections?: unknown };
  const sections = Array.isArray(parsed.sections)
    ? parsed.sections.filter((section): section is DataSection =>
        typeof section === "string" && ALL_SECTIONS.includes(section as DataSection)
      )
    : ALL_SECTIONS;
  if (createSafetySnapshot) await createDatabaseBackup(native, "pre-sync");
  await native.applyDataManagementConfig(configJson, JSON.stringify(sections), false);
};

const applyRemoteMirror = async (
  payload: Buffer,
  password: string
): Promise<void> => {
  const bytes = await decryptBundlePayload(payload, password);
  const path = join(getDataManagementDirectory(), `remote-mirror-${randomUUID()}.snowbackup`);
  writeFileSync(path, bytes, { mode: 0o600 });
  stageDatabaseRestore(path);
};

const getSyncState = (
  statusOverride?: DataManagementSyncState["status"]
): DataManagementSyncState => {
  const persisted = readPersistedSyncState();
  const settings = getDataManagementSettings();
  return {
    status: statusOverride ?? (persisted.conflict ? "conflict" : persisted.lastStatus ?? (persisted.lastError ? "error" : "idle")),
    mode: settings.webdav.syncMode,
    lastSuccessAt: persisted.lastSuccessAt,
    baseRevision: persisted.baseRevision,
    pendingUploadBytes: 0,
    weakConflictProtection: persisted.weakConflictProtection,
    conflict: persisted.conflict,
    lastError: persisted.lastError,
  };
};

export const getDataManagementSyncState = (): DataManagementSyncState => getSyncState();

export const testWebDavConnection = async (): Promise<{ weakConflictProtection: boolean }> => {
  const client = clientFromSettings();
  const root = rootFromSettings();
  await client.ensureLayout(root);
  const result = await client.testConnection(root);
  return { weakConflictProtection: result.weakEtag };
};

export const runWebDavSync = async (
  native: NativeBridge,
  restartOnMirror: boolean
): Promise<SyncRunResult> => {
  const settings = getDataManagementSettings();
  const mode = settings.webdav.syncMode;
  const client = clientFromSettings();
  const password = syncKey();
  const root = rootFromSettings();
  await client.ensureLayout(root);
  await writeDeviceState(client, root);
  const local = await buildLocalPayload(native, mode, password);
  const localObjectHash = sha256Buffer(local.payload);
  const persisted = readPersistedSyncState();
  const remoteRead = await fetchRemoteState(client, root);
  const remote = remoteRead.state;
  const weakConflictProtection = remoteRead.etag === null;

  if (!remote) {
    await uploadObject(client, root, localObjectHash, local.payload);
    const created = makeRemoteState(null, localObjectHash, local.contentHash, mode);
    await uploadStateWithCas(client, root, null, null, created);
    saveSyncState({
      baseRevision: created.revision,
      baseLocalHash: local.contentHash,
      baseRemoteHash: created.contentHash,
      lastSuccessAt: new Date().toISOString(),
      mode,
      weakConflictProtection,
      conflict: null,
      lastError: null,
    });
    return { status: "idle", revision: created.revision, changed: "uploaded", weakConflictProtection, restartRequired: false };
  }

  const decision = decideSyncAction({
    localContentHash: local.contentHash,
    localMode: mode,
    baseRevision: persisted.baseRevision,
    baseLocalHash: persisted.baseLocalHash,
    baseRemoteHash: persisted.baseRemoteHash,
    remoteRevision: remote.revision,
    remoteContentHash: remote.contentHash,
    remoteMode: remote.mode,
  });

  if (decision === "conflict") {
    saveSyncState({
      mode,
      weakConflictProtection,
      conflict: {
        localRevision: persisted.baseRevision,
        remoteRevision: remote.revision,
        remoteDeviceName: remote.deviceName,
      },
      lastError: "Conflict requires an explicit choice",
      lastStatus: "conflict",
    });
    throw new SyncConflictError(remote);
  }

  if (decision === "download") {
    const object = await client.get(objectUrl(client, root, remote.objectHash));
    if (!object) throw new Error("Remote WebDAV object is missing");
    assertRemoteObjectHash(object.body, remote.objectHash);
    if (remote.mode === "mirror") {
      await applyRemoteMirror(object.body, password);
      saveSyncState({
        baseRevision: remote.revision,
        baseLocalHash: remote.contentHash,
        baseRemoteHash: remote.contentHash,
        lastSuccessAt: new Date().toISOString(),
        mode,
        weakConflictProtection,
        conflict: null,
        lastError: null,
      });
      if (restartOnMirror) {
        app.relaunch();
        app.exit(0);
      }
      return { status: "idle", revision: remote.revision, changed: "staged-mirror", weakConflictProtection, restartRequired: true };
    }
    await applyRemoteConfig(native, object.body, password, true);
    saveSyncState({
      baseRevision: remote.revision,
      baseLocalHash: remote.contentHash,
      baseRemoteHash: remote.contentHash,
      lastSuccessAt: new Date().toISOString(),
      mode,
      weakConflictProtection,
      conflict: null,
      lastError: null,
    });
    return { status: "idle", revision: remote.revision, changed: "downloaded", weakConflictProtection, restartRequired: false };
  }

  if (decision === "upload") {
    await uploadObject(client, root, localObjectHash, local.payload);
    const next = makeRemoteState(remote, localObjectHash, local.contentHash, mode);
    await uploadStateWithCas(client, root, remote, remoteRead.etag, next);
    saveSyncState({
      baseRevision: next.revision,
      baseLocalHash: local.contentHash,
      baseRemoteHash: next.contentHash,
      lastSuccessAt: new Date().toISOString(),
      mode,
      weakConflictProtection,
      conflict: null,
      lastError: null,
    });
    return { status: "idle", revision: next.revision, changed: "uploaded", weakConflictProtection, restartRequired: false };
  }

  saveSyncState({
    baseRevision: remote.revision,
    baseLocalHash: local.contentHash,
    baseRemoteHash: remote.contentHash,
    mode,
    weakConflictProtection,
    conflict: null,
    lastError: null,
  });
  return { status: "idle", revision: remote.revision, changed: "none", weakConflictProtection, restartRequired: false };
};

export const resolveWebDavConflict = async (
  native: NativeBridge,
  choice: "local" | "remote" | "keep-both",
  restartOnMirror: boolean
): Promise<SyncRunResult> => {
  if (!["local", "remote", "keep-both"].includes(choice)) throw new Error("Invalid sync conflict choice");
  const settings = getDataManagementSettings();
  const mode = settings.webdav.syncMode;
  const client = clientFromSettings();
  const password = syncKey();
  const root = rootFromSettings();
  const current = await fetchRemoteState(client, root);
  if (!current.state) throw new Error("The remote conflict state no longer exists");
  const remote = current.state;
  let restartRequired = false;
  if (choice === "remote") {
    const object = await client.get(objectUrl(client, root, remote.objectHash));
    if (!object) throw new Error("Remote conflict object is missing");
    assertRemoteObjectHash(object.body, remote.objectHash);
    if (remote.mode === "mirror") {
      await applyRemoteMirror(object.body, password);
      restartRequired = true;
    } else {
      await applyRemoteConfig(native, object.body, password, true);
    }
    updateDataManagementSettings({ webdav: { syncMode: remote.mode } });
  } else {
    if (choice === "keep-both") {
      const object = await client.get(objectUrl(client, root, remote.objectHash));
      if (!object) throw new Error("Remote conflict object is missing");
      assertRemoteObjectHash(object.body, remote.objectHash);
      writeFileSync(join(getDataManagementDirectory(), `conflict-${remote.revision}-${randomUUID()}.snow-sync-object`), object.body, { mode: 0o600 });
    }
    const local = await buildLocalPayload(native, mode, password);
    const localHash = sha256Buffer(local.payload);
    await uploadObject(client, root, localHash, local.payload);
    const next = makeRemoteState(remote, localHash, local.contentHash, mode);
    await uploadStateWithCas(client, root, remote, current.etag, next);
    saveSyncState({
      baseRevision: next.revision,
      baseLocalHash: local.contentHash,
      baseRemoteHash: next.contentHash,
      lastSuccessAt: new Date().toISOString(),
      mode,
      weakConflictProtection: current.etag === null,
      conflict: null,
      lastError: null,
    });
    return { status: "idle", revision: next.revision, changed: "uploaded", weakConflictProtection: current.etag === null, restartRequired: false };
  }
  saveSyncState({
    baseRevision: remote.revision,
    baseLocalHash: remote.contentHash,
    baseRemoteHash: remote.contentHash,
    lastSuccessAt: new Date().toISOString(),
    mode: remote.mode,
    weakConflictProtection: current.etag === null,
    conflict: null,
    lastError: null,
  });
  if (restartRequired && restartOnMirror) {
    app.relaunch();
    app.exit(0);
  }
  return {
    status: "idle",
    revision: remote.revision,
    changed: restartRequired ? "staged-mirror" : "downloaded",
    weakConflictProtection: current.etag === null,
    restartRequired,
  };
};

export const syncErrorState = (error: unknown): DataManagementSyncState["status"] => {
  if (error instanceof SyncConflictError) return "conflict";
  if (error instanceof WebDavError && (error.status === 401 || error.status === 403)) return "auth-error";
  if (error instanceof WebDavError && error.status === 507) return "quota-error";
  if (error instanceof TypeError || error instanceof DOMException) return "offline";
  return "error";
};

export const recordWebDavSyncError = (error: unknown): void => {
  const current = readPersistedSyncState();
  writePersistedSyncState({
    ...current,
    lastError: error instanceof Error ? error.message : String(error),
    lastStatus: syncErrorState(error),
  });
};
