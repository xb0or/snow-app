import { randomUUID } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { getDataManagementDirectory, getDeviceIdentityPath } from "./paths";

export type DeviceIdentity = {
  deviceId: string;
  deviceName: string;
};

type StoredDeviceIdentity = DeviceIdentity;

const DEFAULT_DEVICE_NAME = "Snow App";

const ensureDirectory = (): void => {
  const directory = getDataManagementDirectory();
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  try {
    chmodSync(directory, 0o700);
  } catch {
    // Windows and some mounted filesystems do not support chmod.
  }
};

const isValidIdentity = (value: unknown): value is StoredDeviceIdentity => {
  if (!value || typeof value !== "object") {
    return false;
  }
  const record = value as Record<string, unknown>;
  return (
    typeof record.deviceId === "string" &&
    record.deviceId.trim().length >= 16 &&
    typeof record.deviceName === "string" &&
    record.deviceName.trim().length > 0
  );
};

const writeIdentity = (identity: StoredDeviceIdentity): void => {
  ensureDirectory();
  const path = getDeviceIdentityPath();
  const stagingPath = `${path}.${process.pid}.tmp`;
  writeFileSync(stagingPath, JSON.stringify(identity, null, 2), {
    encoding: "utf8",
    mode: 0o600,
  });
  try {
    chmodSync(stagingPath, 0o600);
  } catch {
    // Windows and some mounted filesystems do not support chmod.
  }
  renameSync(stagingPath, path);
};

const readIdentity = (): StoredDeviceIdentity | null => {
  ensureDirectory();
  const path = getDeviceIdentityPath();
  if (!existsSync(path)) {
    return null;
  }
  try {
    const value: unknown = JSON.parse(readFileSync(path, "utf8"));
    return isValidIdentity(value) ? value : null;
  } catch {
    return null;
  }
};

export const getOrCreateDeviceIdentity = (): DeviceIdentity => {
  const existing = readIdentity();
  if (existing) {
    return existing;
  }

  // Keep the default name privacy-preserving. The hostname is intentionally
  // not written to disk or exposed to the Renderer without user action.
  const identity: DeviceIdentity = {
    deviceId: randomUUID(),
    deviceName: DEFAULT_DEVICE_NAME,
  };
  writeIdentity(identity);
  return identity;
};

export const setDeviceName = (deviceName: string): DeviceIdentity => {
  const normalized = deviceName.trim();
  if (!normalized || normalized.length > 64) {
    throw new Error("Device name must contain 1–64 characters");
  }

  const current = getOrCreateDeviceIdentity();
  const next = { ...current, deviceName: normalized };
  writeIdentity(next);
  return next;
};
