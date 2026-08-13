import { app, BrowserWindow } from "electron";
import type {
  DataManagementCredentialStatus,
  DataManagementCredentialUpdate,
  DataManagementProgress,
  DataManagementSettings,
  DataManagementSettingsPatch,
  DataManagementState,
  DataManagementTaskOperation,
} from "../../preload/types/dataManagement";
import { createDatabaseBackup, listBackupRecords } from "./backupService";
import {
  deleteDataManagementCredential,
  getDataManagementCredentialStatus,
  isDataManagementSafeStorageAvailable,
  saveDataManagementCredential,
} from "./credentials";
import { getOrCreateDeviceIdentity } from "./deviceIdentity";
import {
  getDataManagementSettings,
  updateDataManagementSettings,
} from "./settingsStore";
import {
  DataManagementTaskBusyError,
  DataManagementTaskCoordinator,
} from "./taskCoordinator";
import {
  getDataManagementSyncState,
  recordWebDavSyncError,
  runWebDavSync,
} from "./syncService";
import type { NativeBridge } from "../native/types";

const PROGRESS_CHANNEL = "data-management:progress";
const BACKUP_FREQUENCY_MS: Record<DataManagementSettings["backup"]["frequency"], number> = {
  "6h": 6 * 60 * 60 * 1000,
  "12h": 12 * 60 * 60 * 1000,
  daily: 24 * 60 * 60 * 1000,
  weekly: 7 * 24 * 60 * 60 * 1000,
};

/**
 * Main-process coordinator for all data management work.
 *
 * Owns the single task boundary for configuration packages, database
 * snapshots and WebDAV synchronization. The coordinator also owns the
 * process-lifetime automatic work timers so database operations cannot overlap.
 */
export class DataManagementCoordinator {
  private readonly tasks = new DataManagementTaskCoordinator();
  private native: NativeBridge | null = null;
  private backupTimer: NodeJS.Timeout | null = null;
  private syncTimer: NodeJS.Timeout | null = null;
  private scheduleGeneration = 0;

  constructor() {
    this.tasks.subscribe((progress) => this.broadcastProgress(progress));
  }

  getState(): DataManagementState {
    const identity = getOrCreateDeviceIdentity();
    return {
      deviceId: identity.deviceId,
      deviceName: identity.deviceName,
      appVersion: app.getVersion(),
      manifestFormatVersion: 1,
      safeStorageAvailable: isDataManagementSafeStorageAvailable(),
      credentialStatus: getDataManagementCredentialStatus(),
      activeTask: this.tasks.getActiveTask(),
      backups: listBackupRecords(),
      sync: getDataManagementSyncState(),
    };
  }

  getSettings(): DataManagementSettings {
    return getDataManagementSettings();
  }

  setSettings(patch: DataManagementSettingsPatch): DataManagementSettings {
    const settings = updateDataManagementSettings(patch);
    this.scheduleAutomaticWork();
    return settings;
  }

  setCredential(update: DataManagementCredentialUpdate): DataManagementCredentialStatus {
    return saveDataManagementCredential(update.kind, update.value);
  }

  clearCredential(kind: DataManagementCredentialUpdate["kind"]): DataManagementCredentialStatus {
    return deleteDataManagementCredential(kind);
  }

  cancel(taskId?: string): boolean {
    return this.tasks.cancel(taskId);
  }

  async run<T>(
    operation: DataManagementTaskOperation,
    work: Parameters<DataManagementTaskCoordinator["run"]>[1]
  ): Promise<T> {
    return this.tasks.run(operation, work) as Promise<T>;
  }

  start(native: NativeBridge): void {
    this.native = native;
    this.scheduleAutomaticWork(true);
  }

  stop(): void {
    this.scheduleGeneration += 1;
    if (this.backupTimer) clearTimeout(this.backupTimer);
    if (this.syncTimer) clearTimeout(this.syncTimer);
    this.backupTimer = null;
    this.syncTimer = null;
    this.native = null;
  }

  private scheduleAutomaticWork(startup = false): void {
    const generation = ++this.scheduleGeneration;
    if (this.backupTimer) clearTimeout(this.backupTimer);
    if (this.syncTimer) clearTimeout(this.syncTimer);
    this.backupTimer = null;
    this.syncTimer = null;
    if (!this.native) return;

    const native = this.native;
    const settings = getDataManagementSettings();
    if (settings.backup.enabled) {
      const interval = BACKUP_FREQUENCY_MS[settings.backup.frequency];
      const latest = listBackupRecords().find((record) => record.integrity === "valid");
      const latestTimestamp = latest ? Date.parse(latest.createdAt) : Number.NaN;
      const elapsed = Number.isFinite(latestTimestamp)
        ? Math.max(0, Date.now() - latestTimestamp)
        : interval;
      const due = elapsed >= interval;
      const delay = due
        ? (startup ? 5_000 : 60_000)
        : Math.max(5_000, interval - elapsed);
      this.scheduleBackup(native, interval, delay, generation);
    }
    if (settings.webdav.syncEnabled && settings.webdav.syncIntervalMinutes > 0) {
      const interval = settings.webdav.syncIntervalMinutes * 60 * 1000;
      this.scheduleSync(
        native,
        interval,
        startup ? Math.min(interval, 5_000) : interval,
        generation
      );
    }
  }

  private scheduleBackup(
    native: NativeBridge,
    interval: number,
    delay: number,
    generation: number
  ): void {
    this.backupTimer = setTimeout(() => {
      this.backupTimer = null;
      void this.run("backup-create", async ({ report }) => {
        report({ phase: "automatic backup", total: 2, completed: 1 });
        const record = await createDatabaseBackup(native, "automatic");
        report({
          phase: "automatic backup complete",
          total: 2,
          completed: 2,
          currentItem: record.path,
        });
        return record;
      }).then(
        () => this.scheduleNextBackup(native, interval, interval, generation),
        (error) => this.scheduleNextBackup(
          native,
          interval,
          error instanceof DataManagementTaskBusyError ? 60_000 : Math.min(interval, 5 * 60_000),
          generation
        )
      );
    }, delay);
  }

  private scheduleNextBackup(
    native: NativeBridge,
    interval: number,
    delay: number,
    generation: number
  ): void {
    const settings = getDataManagementSettings();
    if (
      this.native !== native ||
      generation !== this.scheduleGeneration ||
      !settings.backup.enabled
    ) {
      return;
    }
    const nextInterval = BACKUP_FREQUENCY_MS[settings.backup.frequency];
    this.scheduleBackup(
      native,
      nextInterval,
      interval === nextInterval ? delay : nextInterval,
      generation
    );
  }

  private scheduleSync(
    native: NativeBridge,
    interval: number,
    delay: number,
    generation: number
  ): void {
    this.syncTimer = setTimeout(() => {
      this.syncTimer = null;
      if (getDataManagementSyncState().status === "conflict") {
        this.scheduleNextSync(native, interval, interval, generation);
        return;
      }
      let nextDelay = interval;
      void this.run("sync", async ({ report }) => {
        report({ phase: "automatic WebDAV sync", total: 2, completed: 1 });
        const result = await runWebDavSync(native, false);
        report({ phase: "automatic WebDAV sync complete", total: 2, completed: 2 });
        return result;
      }).catch((error) => {
        if (error instanceof DataManagementTaskBusyError) {
          nextDelay = Math.min(interval, 60_000);
        } else {
          recordWebDavSyncError(error);
          nextDelay = Math.min(interval, 5 * 60_000);
        }
      }).finally(() => {
        this.scheduleNextSync(native, interval, nextDelay, generation);
      });
    }, delay);
  }

  private scheduleNextSync(
    native: NativeBridge,
    interval: number,
    delay: number,
    generation: number
  ): void {
    const settings = getDataManagementSettings();
    if (
      this.native !== native ||
      generation !== this.scheduleGeneration ||
      !settings.webdav.syncEnabled ||
      settings.webdav.syncIntervalMinutes <= 0
    ) {
      return;
    }
    const nextInterval = settings.webdav.syncIntervalMinutes * 60 * 1000;
    this.scheduleSync(
      native,
      nextInterval,
      interval === nextInterval ? delay : nextInterval,
      generation
    );
  }

  private broadcastProgress(progress: DataManagementProgress): void {
    for (const window of BrowserWindow.getAllWindows()) {
      if (!window.isDestroyed()) {
        window.webContents.send(PROGRESS_CHANNEL, progress);
      }
    }
  }
}

export const dataManagementCoordinator = new DataManagementCoordinator();
