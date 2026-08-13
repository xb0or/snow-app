import { Archive, Database, DatabaseBackup, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import { useI18n } from "../../../i18n";
import type {
  DataManagementBackupRecord,
  DataManagementSettings,
  DataManagementSettingsPatch,
  DataManagementState,
} from "../../../../preload";

type BackupRestoreTabProps = {
  state: DataManagementState | null;
  settings: DataManagementSettings | null;
  isSaving: boolean;
  onUpdateSettings: (patch: DataManagementSettingsPatch) => Promise<void>;
  onCreate: (reason?: string) => Promise<unknown | null>;
  onRestore: (path: string) => Promise<boolean>;
  onDelete: (path: string) => Promise<boolean>;
};

export function BackupRestoreTab({
  state,
  settings,
  isSaving,
  onUpdateSettings,
  onCreate,
  onRestore,
  onDelete,
}: BackupRestoreTabProps): React.JSX.Element {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  const [backupEnabled, setBackupEnabled] = useState(false);
  const [frequency, setFrequency] = useState<"6h" | "12h" | "daily" | "weekly">("daily");
  const [retentionCount, setRetentionCount] = useState(7);
  const [directory, setDirectory] = useState("");
  const [includeArchive, setIncludeArchive] = useState(true);
  const [beforeImport, setBeforeImport] = useState(true);
  const [beforeRestore, setBeforeRestore] = useState(true);

  useEffect(() => {
    if (!settings || busy || isSaving) return;
    setBackupEnabled(settings.backup.enabled);
    setFrequency(settings.backup.frequency);
    setRetentionCount(settings.backup.retentionCount);
    setDirectory(settings.backup.directory);
    setIncludeArchive(settings.backup.includeArchive);
    setBeforeImport(settings.backup.beforeImport);
    setBeforeRestore(settings.backup.beforeRestore);
  }, [settings]);

  const saveBackupSettings = async (): Promise<void> => {
    await onUpdateSettings({
      backup: {
        enabled: backupEnabled,
        frequency,
        retentionCount,
        directory,
        includeArchive,
        beforeImport,
        beforeRestore,
      },
    });
  };

  const create = async (): Promise<void> => {
    setBusy(true);
    try {
      await onCreate("manual");
    } finally {
      setBusy(false);
    }
  };

  const restore = async (record: DataManagementBackupRecord): Promise<void> => {
    if (!window.confirm(`Restore snapshot from ${new Date(record.createdAt).toLocaleString()} and restart Snow App?`)) return;
    setBusy(true);
    try {
      await onRestore(record.path);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (record: DataManagementBackupRecord): Promise<void> => {
    if (!window.confirm(`Delete snapshot ${record.id}?`)) return;
    setBusy(true);
    try {
      await onDelete(record.path);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="data-management-tab-content">
      <div className="data-management-hero">
        <div className="data-management-hero-icon">
          <DatabaseBackup size={20} strokeWidth={1.7} />
        </div>
        <div>
          <strong>{t("settings.dataManagementBackupTitle", { defaultValue: "Database snapshots and recovery" })}</strong>
          <p>{t("settings.dataManagementBackupInfo", { defaultValue: "Snapshots use SQLite Online Backup so WAL writes remain consistent while the app is running." })}</p>
        </div>
        <span className="data-management-phase-badge">SQLite Online Backup</span>
      </div>

      <section className="data-management-card">
        <div className="data-management-card-heading">
          <ShieldCheck size={16} aria-hidden="true" />
          <strong>{t("settings.dataManagementSnapshotScope", { defaultValue: "Snapshot scope" })}</strong>
        </div>
        <div className="data-management-database-list">
          <div className="data-management-database-row">
            <Database size={15} aria-hidden="true" />
            <div><strong>snowapp.db</strong><span>Main application database</span></div>
            <span className="data-management-scope-badge">Included</span>
          </div>
          <div className="data-management-database-row">
            <Archive size={15} aria-hidden="true" />
            <div><strong>archive.db</strong><span>Archived conversations</span></div>
            <span className="data-management-scope-badge">Included</span>
          </div>
        </div>
      </section>

      <section className="data-management-card">
        <div className="data-management-card-heading"><DatabaseBackup size={16} aria-hidden="true" /><strong>Automatic backup</strong></div>
        <div className="data-management-form-grid">
          <label className="data-management-checkbox-row">
            <input type="checkbox" checked={backupEnabled} onChange={(event) => setBackupEnabled(event.target.checked)} />
            <span>Enable scheduled snapshots</span>
          </label>
          <label><span>Frequency</span><select value={frequency} onChange={(event) => setFrequency(event.target.value as typeof frequency)}>
            <option value="6h">Every 6 hours</option><option value="12h">Every 12 hours</option><option value="daily">Daily</option><option value="weekly">Weekly</option>
          </select></label>
          <label><span>Keep snapshots</span><input type="number" min={3} max={100} value={retentionCount} onChange={(event) => setRetentionCount(Math.min(100, Math.max(3, Number(event.target.value) || 3)))} /></label>
          <label><span>Backup directory</span><input value={directory} onChange={(event) => setDirectory(event.target.value)} placeholder="Default application backup directory" /></label>
        </div>
        <div className="data-management-form-footer">
          <label className="data-management-checkbox-row"><input type="checkbox" checked={includeArchive} onChange={(event) => setIncludeArchive(event.target.checked)} /><span>Include archive.db</span></label>
          <label className="data-management-checkbox-row"><input type="checkbox" checked={beforeImport} onChange={(event) => setBeforeImport(event.target.checked)} /><span>Safety snapshot before import</span></label>
          <label className="data-management-checkbox-row"><input type="checkbox" checked={beforeRestore} onChange={(event) => setBeforeRestore(event.target.checked)} /><span>Safety snapshot before restore</span></label>
          <button className="data-management-primary-button" disabled={isSaving || busy} onClick={() => void saveBackupSettings()} type="button">Save backup settings</button>
        </div>
        <span className="data-management-muted-note">Attachment files are intentionally not included in this release; database snapshots remain portable and bounded.</span>
      </section>

      <div className="data-management-card-grid">
        <section className="data-management-card">
          <div className="data-management-card-heading"><DatabaseBackup size={16} aria-hidden="true" /><strong>Create a snapshot</strong></div>
          <p>Copies both databases through SQLite Online Backup, checks them, and commits one atomic package.</p>
          <button className="data-management-secondary-button" disabled={busy} onClick={() => void create()} type="button">Create snapshot now</button>
        </section>
        <section className="data-management-card">
          <div className="data-management-card-heading"><Archive size={16} aria-hidden="true" /><strong>Restore safely</strong></div>
          <p>Restoration is staged and applied before storage initialization after an explicit app restart.</p>
          <span className="data-management-muted-note">{state?.activeTask?.phase ?? `${state?.backups.length ?? 0} snapshot(s) available`}</span>
        </section>
      </div>

      <section className="data-management-card">
        <div className="data-management-card-heading"><strong>Available snapshots</strong></div>
        <div className="data-management-backup-list">
          {state?.backups?.length ? state.backups.map((record) => (
            <div className="data-management-backup-row" key={record.path}>
              <div><strong>{new Date(record.createdAt).toLocaleString()}</strong><span>{record.reason} · {(record.sizeBytes / 1024 / 1024).toFixed(1)} MB · {record.integrity}</span></div>
              <div className="data-management-backup-actions">
                <button className="data-management-secondary-button" disabled={busy || record.integrity !== "valid"} onClick={() => void restore(record)} type="button">Restore</button>
                <button className="data-management-secondary-button" disabled={busy} onClick={() => void remove(record)} type="button">Delete</button>
              </div>
            </div>
          )) : <span className="data-management-muted-note">No snapshots yet.</span>}
        </div>
      </section>
    </div>
  );
}
