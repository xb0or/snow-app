import { app } from "electron";
import { createHash, randomUUID } from "node:crypto";
import { existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { getOrCreateDeviceIdentity } from "./deviceIdentity";
import { createDatabaseBackup } from "./backupService";
import { decryptBundlePayload, encryptBundlePayload, sha256Buffer } from "./cryptoBundle";
import { createZipArchive, readZipArchive } from "./zipArchive";
import type { NativeBridge } from "../native/types";
import {
  DATA_MANAGEMENT_FORMAT_VERSION,
  DATA_SECTIONS,
  type DataManifest,
  type DataManagementImportPreview,
  type DataSection,
} from "../../preload/types/dataManagement";

const CONFIG_EXTENSION = ".snow-config";
const MANIFEST_ENTRY = "manifest.json";
const CONFIG_ENTRY = "config.json";
const ENCRYPTED_ENTRY = "payload.bin";
const MAX_CONFIG_PACKAGE_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_MANIFEST_BYTES = 1024 * 1024;
const REDACTED_MARKER = "__SNOWAPP_REDACTED__";

type ConfigPackageManifest = DataManifest & {
  packageId: string;
  schemaVersion: number;
};

type ReadConfigPackage = {
  manifest: ConfigPackageManifest;
  configJson: string;
  preview: DataManagementImportPreview;
};

type ConfigPackageContainer = {
  manifest: ConfigPackageManifest;
  payload: Buffer;
};

const sha256 = (value: Buffer): string =>
  createHash("sha256").update(value).digest("hex");

const assertSections = (sections: DataSection[]): void => {
  if (sections.length === 0) {
    throw new Error("Select at least one configuration section");
  }
  const unique = new Set(sections);
  if (unique.size !== sections.length || sections.some((section) => !DATA_SECTIONS.includes(section))) {
    throw new Error("Configuration section selection is invalid");
  }
};

type ConfigBundleMetadata = {
  schemaVersion: number;
  sections: DataSection[];
  containsSecrets: boolean;
  rows: number;
  redactions: number;
};

const parseBundleMetadata = (configJson: string): ConfigBundleMetadata => {
  let value: unknown;
  try {
    value = JSON.parse(configJson);
  } catch {
    throw new Error("Configuration payload is not valid JSON");
  }
  if (!value || typeof value !== "object") {
    throw new Error("Configuration payload is invalid");
  }
  const record = value as Record<string, unknown>;
  if (record.formatVersion !== DATA_MANAGEMENT_FORMAT_VERSION) {
    throw new Error("Configuration payload format is unsupported");
  }
  if (
    typeof record.schemaVersion !== "number" ||
    !Number.isSafeInteger(record.schemaVersion) ||
    record.schemaVersion < 0
  ) {
    throw new Error("Configuration payload schema version is invalid");
  }
  if (!Array.isArray(record.sections)) {
    throw new Error("Configuration payload has no section list");
  }
  const sections = record.sections as DataSection[];
  assertSections(sections);
  if (typeof record.containsSecrets !== "boolean") {
    throw new Error("Configuration payload sensitivity marker is invalid");
  }
  const tables = record.tables;
  if (!tables || typeof tables !== "object") {
    throw new Error("Configuration payload has no tables");
  }
  let rows = 0;
  let redactions = 0;
  const count = (item: unknown): void => {
    if (item === REDACTED_MARKER) {
      redactions += 1;
    } else if (Array.isArray(item)) {
      item.forEach(count);
    } else if (item && typeof item === "object") {
      Object.values(item).forEach(count);
    }
  };
  for (const table of Object.values(tables as Record<string, unknown>)) {
    if (Array.isArray(table)) rows += table.length;
    count(table);
  }
  return {
    schemaVersion: record.schemaVersion,
    sections,
    containsSecrets: record.containsSecrets,
    rows,
    redactions,
  };
};

const readManifest = (entries: Map<string, Buffer>): ConfigPackageManifest => {
  const raw = entries.get(MANIFEST_ENTRY);
  if (!raw) throw new Error("Configuration manifest is missing");
  if (raw.length > MAX_MANIFEST_BYTES) throw new Error("Configuration manifest is too large");
  let value: unknown;
  try {
    value = JSON.parse(raw.toString("utf8"));
  } catch {
    throw new Error("Configuration manifest is not valid JSON");
  }
  if (!value || typeof value !== "object") throw new Error("Configuration manifest is invalid");
  const manifest = value as Partial<ConfigPackageManifest>;
  if (
    manifest.formatVersion !== DATA_MANAGEMENT_FORMAT_VERSION ||
    typeof manifest.packageId !== "string" || !manifest.packageId || manifest.packageId.length > 1_024 ||
    typeof manifest.appVersion !== "string" || !manifest.appVersion || manifest.appVersion.length > 1_024 ||
    typeof manifest.deviceId !== "string" || !manifest.deviceId || manifest.deviceId.length > 1_024 ||
    typeof manifest.createdAt !== "string" || !Number.isFinite(Date.parse(manifest.createdAt)) ||
    typeof manifest.schemaVersion !== "number" ||
    !Number.isSafeInteger(manifest.schemaVersion) ||
    manifest.schemaVersion < 0 ||
    !Array.isArray(manifest.sections) ||
    !Array.isArray(manifest.files) ||
    manifest.files.length !== 1 ||
    typeof manifest.containsSecrets !== "boolean" ||
    typeof manifest.encrypted !== "boolean" ||
    (manifest.containsSecrets && !manifest.encrypted)
  ) {
    throw new Error("Unsupported or incomplete configuration manifest");
  }
  assertSections(manifest.sections as DataSection[]);
  const file = manifest.files[0];
  if (
    !file ||
    typeof file.path !== "string" ||
    !/^[a-f0-9]{64}$/.test(file.sha256) ||
    !Number.isSafeInteger(file.sizeBytes) ||
    file.sizeBytes < 0 ||
    file.sizeBytes > MAX_CONFIG_PACKAGE_BYTES
  ) {
    throw new Error("Configuration manifest file metadata is invalid");
  }
  return manifest as ConfigPackageManifest;
};

const verifyManifestFiles = (
  manifest: ConfigPackageManifest,
  entries: Map<string, Buffer>
): void => {
  for (const file of manifest.files) {
    const data = entries.get(file.path);
    if (!data || data.length !== file.sizeBytes || sha256(data) !== file.sha256) {
      throw new Error(`Configuration package checksum mismatch: ${file.path}`);
    }
  }
};

const readConfigPackageContainer = (path: string): ConfigPackageContainer => {
  if (!existsSync(path) || !path.endsWith(CONFIG_EXTENSION)) {
    throw new Error("A .snow-config package is required");
  }
  const source = readFileSync(path);
  if (source.length > MAX_CONFIG_PACKAGE_BYTES) throw new Error("Configuration package is too large");
  const entries = readZipArchive(source, {
    maxEntries: 64,
    maxEntryBytes: 512 * 1024 * 1024,
    maxTotalBytes: MAX_CONFIG_PACKAGE_BYTES,
  });
  const manifest = readManifest(entries);
  const payloadEntry = manifest.encrypted ? ENCRYPTED_ENTRY : CONFIG_ENTRY;
  if (
    manifest.files[0]?.path !== payloadEntry ||
    entries.size !== 2 ||
    [...entries.keys()].some((entry) => entry !== MANIFEST_ENTRY && entry !== payloadEntry)
  ) {
    throw new Error("Configuration package entries do not match the manifest");
  }
  verifyManifestFiles(manifest, entries);
  const payload = entries.get(payloadEntry);
  if (!payload) throw new Error(`Configuration package is missing ${payloadEntry}`);
  return { manifest, payload };
};

const decodeConfigPackage = async (
  path: string,
  container: ConfigPackageContainer,
  password?: string
): Promise<ReadConfigPackage> => {
  const { manifest, payload } = container;
  const config = manifest.encrypted
    ? await decryptBundlePayload(payload, password ?? "")
    : payload;
  const configJson = config.toString("utf8");
  const metadata = parseBundleMetadata(configJson);
  if (
    metadata.schemaVersion !== manifest.schemaVersion ||
    metadata.containsSecrets !== manifest.containsSecrets ||
    metadata.sections.length !== manifest.sections.length ||
    metadata.sections.some((section) => !manifest.sections.includes(section))
  ) {
    throw new Error("Configuration payload metadata does not match the manifest");
  }
  return {
    manifest,
    configJson,
    preview: {
      path,
      encrypted: manifest.encrypted,
      containsSecrets: manifest.containsSecrets,
      formatVersion: manifest.formatVersion,
      schemaVersion: metadata.schemaVersion,
      sections: manifest.sections as DataSection[],
      rows: metadata.rows,
      estimatedBytes: config.length,
      deviceSpecificItems: metadata.redactions,
    },
  };
};

const readConfigPackage = async (path: string, password?: string): Promise<ReadConfigPackage> =>
  decodeConfigPackage(path, readConfigPackageContainer(path), password);

export const previewConfigPackage = (
  path: string,
  password?: string
): Promise<DataManagementImportPreview> => readConfigPackage(path, password).then((value) => value.preview);

export const exportConfigPackage = async (
  native: NativeBridge,
  path: string,
  sections: DataSection[],
  includeSecrets: boolean,
  password?: string
): Promise<DataManagementImportPreview> => {
  assertSections(sections);
  if (!path.endsWith(CONFIG_EXTENSION)) throw new Error("Export path must end with .snow-config");
  if (includeSecrets && !password) {
    throw new Error("An export password is required when including sensitive configuration");
  }
  const configJson = await native.exportDataManagementConfig(JSON.stringify(sections), includeSecrets);
  const metadata = parseBundleMetadata(configJson);
  const plainPayload = Buffer.from(configJson, "utf8");
  const payload = includeSecrets
    ? await encryptBundlePayload(plainPayload, password ?? "")
    : plainPayload;
  const payloadPath = includeSecrets ? ENCRYPTED_ENTRY : CONFIG_ENTRY;
  const manifest: ConfigPackageManifest = {
    formatVersion: DATA_MANAGEMENT_FORMAT_VERSION,
    packageId: randomUUID(),
    appVersion: app.getVersion(),
    schemaVersion: metadata.schemaVersion,
    createdAt: new Date().toISOString(),
    deviceId: getOrCreateDeviceIdentity().deviceId,
    sections,
    containsSecrets: includeSecrets,
    encrypted: includeSecrets,
    files: [{ path: payloadPath, sha256: sha256(payload), sizeBytes: payload.length }],
  };
  const archive = createZipArchive([
    { name: MANIFEST_ENTRY, data: Buffer.from(JSON.stringify(manifest, null, 2), "utf8") },
    { name: payloadPath, data: payload },
  ]);
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  writeFileSync(temporaryPath, archive, { mode: 0o600 });
  renameSync(temporaryPath, path);
  return {
    path,
    encrypted: includeSecrets,
    containsSecrets: includeSecrets,
    formatVersion: DATA_MANAGEMENT_FORMAT_VERSION,
    schemaVersion: metadata.schemaVersion,
    sections,
    rows: metadata.rows,
    estimatedBytes: archive.length,
    deviceSpecificItems: metadata.redactions,
  };
};

export const applyConfigPackage = async (
  native: NativeBridge,
  path: string,
  sections: DataSection[],
  password: string | undefined,
  replaceSelected: boolean,
  createSafetySnapshot: boolean
): Promise<DataManagementImportPreview> => {
  assertSections(sections);
  const packageData = await readConfigPackage(path, password);
  const selected = packageData.manifest.sections.filter((section): section is DataSection =>
    sections.includes(section)
  );
  assertSections(selected);
  if (createSafetySnapshot) {
    await createDatabaseBackup(native, "pre-import");
  }
  await native.applyDataManagementConfig(
    packageData.configJson,
    JSON.stringify(selected),
    replaceSelected
  );
  return packageData.preview;
};

export const inspectConfigPackage = async (
  path: string,
  password?: string
): Promise<DataManagementImportPreview> => {
  const container = readConfigPackageContainer(path);
  if (!container.manifest.encrypted || password) {
    return (await decodeConfigPackage(path, container, password)).preview;
  }
  return {
    path,
    encrypted: true,
    containsSecrets: container.manifest.containsSecrets,
    formatVersion: container.manifest.formatVersion,
    schemaVersion: container.manifest.schemaVersion,
    sections: container.manifest.sections as DataSection[],
    rows: 0,
    estimatedBytes: container.payload.length,
    deviceSpecificItems: 0,
  };
};

export const configPackageHash = async (path: string, password?: string): Promise<string> =>
  sha256Buffer((await readConfigPackage(path, password)).configJson);

export const configPackagePayload = (
  native: NativeBridge,
  sections: DataSection[],
  includeSecrets: boolean,
  password: string
): Promise<{ configJson: string; encryptedPayload: Buffer; plainHash: string }> =>
  native.exportDataManagementConfig(JSON.stringify(sections), includeSecrets).then(async (configJson) => ({
    configJson,
    encryptedPayload: await encryptBundlePayload(Buffer.from(configJson, "utf8"), password),
    plainHash: sha256Buffer(configJson),
  }));
