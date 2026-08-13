import { app } from "electron";
import { join } from "node:path";

export const getDataManagementDirectory = (): string =>
  join(app.getPath("userData"), "data-management");

export const getDeviceIdentityPath = (): string =>
  join(getDataManagementDirectory(), "device.json");

export const getSettingsPath = (): string =>
  join(getDataManagementDirectory(), "settings.json");

export const getCredentialsPath = (): string =>
  join(getDataManagementDirectory(), "credentials.json");

export const getBackupDirectory = (): string =>
  join(getDataManagementDirectory(), "backups");

export const getPendingRestorePath = (): string =>
  join(getDataManagementDirectory(), "pending-restore.json");

export const getSyncStatePath = (): string =>
  join(getDataManagementDirectory(), "sync-state.json");
