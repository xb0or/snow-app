import { Cloud, LockKeyhole, ShieldAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useI18n } from "../../../i18n";
import { CustomSelect } from "../../common/CustomSelect";
import type {
  DataManagementSettings,
  DataManagementSettingsPatch,
  DataManagementState,
  DataManagementWebDavSettings,
} from "../../../../preload";

type CloudSyncTabProps = {
  state: DataManagementState | null;
  settings: DataManagementSettings | null;
  onUpdateSettings: (patch: DataManagementSettingsPatch) => Promise<void>;
  onTestConnection: () => Promise<{ weakConflictProtection: boolean }>;
  onSync: () => Promise<unknown | null>;
  onResolveConflict: (choice: "local" | "remote" | "keep-both") => Promise<unknown | null>;
};

export function CloudSyncTab({
  state,
  settings,
  onUpdateSettings,
  onTestConnection,
  onSync,
  onResolveConflict,
}: CloudSyncTabProps): React.JSX.Element {
  const { t } = useI18n();
  const [deviceName, setDeviceName] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [remoteRoot, setRemoteRoot] = useState("");
  const [username, setUsername] = useState("");
  const [webdavPassword, setWebdavPassword] = useState("");
  const [syncMasterKey, setSyncMasterKey] = useState("");
  const [syncEnabled, setSyncEnabled] = useState(false);
  const [syncIntervalMinutes, setSyncIntervalMinutes] = useState<0 | 15 | 30 | 60>(0);
  const [syncMode, setSyncMode] = useState<"config" | "mirror">("config");
  const [allowInsecureHttp, setAllowInsecureHttp] = useState(false);
  const [busy, setBusy] = useState(false);
  const [statusMessage, setStatusMessage] = useState("");

  const initializedRef = useRef(false);

  useEffect(() => {
    if (!settings || initializedRef.current) {
      return;
    }
    initializedRef.current = true;
    setDeviceName(settings.deviceName);
    setEndpoint(settings.webdav.endpoint);
    setRemoteRoot(settings.webdav.remoteRoot);
    setUsername(settings.webdav.username);
    setSyncEnabled(settings.webdav.syncEnabled);
    setSyncIntervalMinutes(settings.webdav.syncIntervalMinutes);
    setSyncMode(settings.webdav.syncMode);
    setAllowInsecureHttp(settings.webdav.allowInsecureHttp);
  }, [settings]);

  const persistWebdav = (patch: Partial<DataManagementWebDavSettings>): void => {
    void onUpdateSettings({ webdav: patch }).catch(() => {
      // The shared panel displays the error from useDataManagement.
    });
  };

  const persistDeviceName = (): void => {
    void onUpdateSettings({ deviceName }).catch(() => {
      // The shared panel displays the error from useDataManagement.
    });
  };

  const commitCredential = async (
    kind: "webdav-password" | "sync-master-key",
    value: string,
    clear: () => void
  ): Promise<void> => {
    if (!value.trim()) return;
    clear();
    try {
      await window.snow.setDataManagementCredential({ kind, value });
    } catch {
      // The shared panel displays the error from useDataManagement.
    }
  };

  const changeSyncMode = (value: string): void => {
    const next = value as "config" | "mirror";
    setSyncMode(next);
    persistWebdav({ syncMode: next });
  };

  const changeSyncInterval = (value: string): void => {
    const next = Number(value) as 0 | 15 | 30 | 60;
    setSyncIntervalMinutes(next);
    persistWebdav({ syncIntervalMinutes: next });
  };

  const toggleSyncEnabled = (checked: boolean): void => {
    setSyncEnabled(checked);
    persistWebdav({ syncEnabled: checked });
  };

  const toggleAllowInsecureHttp = (checked: boolean): void => {
    setAllowInsecureHttp(checked);
    persistWebdav({ allowInsecureHttp: checked });
  };

  const testConnection = async (): Promise<void> => {
    setBusy(true);
    try {
      const result = await onTestConnection();
      setStatusMessage(
        result.weakConflictProtection
          ? t("settings.dataManagementConnectionWeakEtag", {
              defaultValue: "Connected; server has weak ETag protection",
            })
          : t("settings.dataManagementConnectionReady", {
              defaultValue: "WebDAV connection is ready",
            })
      );
    } finally { setBusy(false); }
  };

  const runSync = async (): Promise<void> => {
    setBusy(true);
    try {
      await onSync();
      setStatusMessage(
        t("settings.dataManagementSyncCompleted", {
          defaultValue: "Sync completed",
        })
      );
    } finally { setBusy(false); }
  };

  const safeStorageReady = state?.safeStorageAvailable === true;

  return (
    <div className="data-management-tab-content">
      <div className="data-management-hero">
        <div className="data-management-hero-icon">
          <Cloud size={20} strokeWidth={1.7} />
        </div>
        <div>
          <strong>
            {t("settings.dataManagementCloudTitle", {
              defaultValue: "WebDAV cloud sync",
            })}
          </strong>
          <p>
            {t("settings.dataManagementCloudInfo", {
              defaultValue:
                "Configuration sync will keep encrypted objects on your WebDAV server and coordinate changes with revisions and ETags.",
            })}
          </p>
        </div>
        <span className="data-management-phase-badge">
          {t("settings.dataManagementEncryptedEtagSync", {
            defaultValue: "Encrypted ETag sync",
          })}
        </span>
      </div>

      <section className="data-management-card">
        <div className="data-management-card-heading">
          <Cloud size={16} aria-hidden="true" />
          <strong>
            {t("settings.dataManagementDeviceIdentity", {
              defaultValue: "This device",
            })}
          </strong>
          {state && (
            <span className="data-management-card-meta">
              {state.deviceId.slice(0, 8)}…
            </span>
          )}
        </div>
        <div className="data-management-form-row">
          <label htmlFor="data-management-device-name">
            {t("settings.dataManagementDeviceName", {
              defaultValue: "Device name",
            })}
          </label>
          <input
            id="data-management-device-name"
            value={deviceName}
            maxLength={64}
            onChange={(event) => setDeviceName(event.target.value)}
            onBlur={persistDeviceName}
            placeholder={t("settings.dataManagementDeviceNamePlaceholder", {
              defaultValue: "Snow App",
            })}
          />
        </div>
      </section>

      <section className="data-management-card">
        <div className="data-management-card-heading">
          <LockKeyhole size={16} aria-hidden="true" />
          <strong>
            {t("settings.dataManagementWebDavConnection", {
              defaultValue: "WebDAV connection",
            })}
          </strong>
          <span className="data-management-inline-status">
            {t("settings.dataManagementConfigurationOnly", {
              defaultValue: "Configuration only",
            })}
          </span>
        </div>
        <div className="data-management-form-grid">
          <label>
            <span>
              {t("settings.dataManagementEndpoint", {
                defaultValue: "Endpoint",
              })}
            </span>
            <input
              value={endpoint}
              onChange={(event) => setEndpoint(event.target.value)}
              onBlur={() => persistWebdav({ endpoint })}
              placeholder="https://dav.example.com/remote.php/dav/files/user"
              inputMode="url"
            />
          </label>
          <label>
            <span>
              {t("settings.dataManagementRemoteRoot", {
                defaultValue: "Remote root",
              })}
            </span>
            <input
              value={remoteRoot}
              onChange={(event) => setRemoteRoot(event.target.value)}
              onBlur={() => persistWebdav({ remoteRoot })}
              placeholder="snow-app"
            />
          </label>
          <label>
            <span>
              {t("settings.dataManagementUsername", {
                defaultValue: "Username",
              })}
            </span>
            <input
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              onBlur={() => persistWebdav({ username })}
              autoComplete="username"
            />
          </label>
          <label>
            <span>{t("settings.dataManagementWebdavPassword", { defaultValue: "WebDAV password" })}</span>
            <input type="password" value={webdavPassword} onChange={(event) => setWebdavPassword(event.target.value)} onBlur={() => void commitCredential("webdav-password", webdavPassword, () => setWebdavPassword(""))} autoComplete="new-password" placeholder={state?.credentialStatus.webdavPasswordConfigured ? t("settings.dataManagementConfigured", { defaultValue: "Configured" }) : t("settings.dataManagementRequired", { defaultValue: "Required" })} />
          </label>
          <label>
            <span>{t("settings.dataManagementSyncEncryptionPassword", { defaultValue: "Sync encryption password" })}</span>
            <input type="password" value={syncMasterKey} onChange={(event) => setSyncMasterKey(event.target.value)} onBlur={() => void commitCredential("sync-master-key", syncMasterKey, () => setSyncMasterKey(""))} autoComplete="new-password" placeholder={state?.credentialStatus.syncMasterKeyConfigured ? t("settings.dataManagementConfigured", { defaultValue: "Configured" }) : t("settings.dataManagementRequired", { defaultValue: "Required" })} />
          </label>
          <label>
            <span>{t("settings.dataManagementSyncMode", { defaultValue: "Sync mode" })}</span>
            <CustomSelect
              value={syncMode}
              options={[
                { value: "config", label: t("settings.dataManagementConfigurationSync", { defaultValue: "Configuration sync" }) },
                { value: "mirror", label: t("settings.dataManagementFullMirror", { defaultValue: "Full database mirror (restart)" }) },
              ]}
              onChange={changeSyncMode}
            />
          </label>
          <label>
            <span>{t("settings.dataManagementAutomaticSync", { defaultValue: "Automatic sync" })}</span>
            <CustomSelect
              value={String(syncIntervalMinutes)}
              options={[
                { value: "0", label: t("settings.dataManagementManualOnly", { defaultValue: "Manual only" }) },
                { value: "15", label: t("settings.dataManagementEvery15Minutes", { defaultValue: "Every 15 minutes" }) },
                { value: "30", label: t("settings.dataManagementEvery30Minutes", { defaultValue: "Every 30 minutes" }) },
                { value: "60", label: t("settings.dataManagementEveryHour", { defaultValue: "Every hour" }) },
              ]}
              onChange={changeSyncInterval}
            />
          </label>
        </div>
        <div className="data-management-checkbox-row data-management-risk-option">
          <label className="toggle-switch">
            <input type="checkbox" hidden checked={allowInsecureHttp} onChange={(event) => toggleAllowInsecureHttp(event.target.checked)} />
            <span className="toggle-slider" />
          </label>
          <span>{t("settings.dataManagementAllowInsecureHttp", { defaultValue: "Allow insecure HTTP (high risk; HTTPS remains the default)" })}</span>
        </div>
        <div className="data-management-form-footer">
          <span className="data-management-muted-note">
            {statusMessage ||
              t("settings.dataManagementRemoteEncryptedNote", {
                defaultValue: "Remote objects are encrypted before upload; passwords never leave the main process.",
              })}
          </span>
          <div className="data-management-checkbox-row">
            <label className="toggle-switch">
              <input type="checkbox" hidden checked={syncEnabled} onChange={(event) => toggleSyncEnabled(event.target.checked)} />
              <span className="toggle-slider" />
            </label>
            <span>{t("settings.dataManagementEnableAutomaticSync", { defaultValue: "Enable automatic sync" })}</span>
          </div>
        </div>
        <div className="data-management-action-row">
          <button className="data-management-secondary-button" disabled={busy || !endpoint.trim()} type="button" onClick={() => void testConnection()}>{t("settings.dataManagementTestConnection", { defaultValue: "Test connection" })}</button>
          <button className="data-management-secondary-button" disabled={busy || !endpoint.trim()} type="button" onClick={() => void runSync()}>{t("settings.dataManagementSyncNow", { defaultValue: "Sync now" })}</button>
        </div>
      </section>

      <section
        className={`data-management-card data-management-status-card ${
          safeStorageReady ? "is-ready" : "is-warning"
        }`}
      >
        {safeStorageReady ? (
          <LockKeyhole size={16} aria-hidden="true" />
        ) : (
          <ShieldAlert size={16} aria-hidden="true" />
        )}
        <div>
          <strong>
            {safeStorageReady
              ? t("settings.dataManagementSecureStorageReady", {
                  defaultValue: "Secure credential storage is available",
                })
              : t("settings.dataManagementSecureStorageUnavailable", {
                  defaultValue: "Secure credential storage is unavailable",
                })}
          </strong>
          <span>
            {state?.credentialStatus.webdavPasswordConfigured
              ? t("settings.dataManagementPasswordConfigured", {
                  defaultValue: "A WebDAV password is configured locally.",
                })
              : t("settings.dataManagementPasswordNotConfigured", {
                  defaultValue: "No WebDAV password is stored.",
                })}
          </span>
        </div>
      </section>

      {state?.sync.status === "conflict" && state.sync.conflict && (
        <section className="data-management-card data-management-status-card is-warning">
          <ShieldAlert size={16} aria-hidden="true" />
          <div>
            <strong>{t("settings.dataManagementConflictTitle", { defaultValue: "Sync conflict needs a choice" })}</strong>
            <span>{t("settings.dataManagementConflictInfo", {
              values: {
                revision: state.sync.conflict.remoteRevision,
                device: state.sync.conflict.remoteDeviceName,
              },
              defaultValue: "Remote revision {{revision}} from {{device}}; local data was kept.",
            })}</span>
            <div className="data-management-form-footer">
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("local")} type="button">{t("settings.dataManagementKeepLocal", { defaultValue: "Keep local and upload" })}</button>
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("remote")} type="button">{t("settings.dataManagementUseRemote", { defaultValue: "Use remote" })}</button>
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("keep-both")} type="button">{t("settings.dataManagementKeepBoth", { defaultValue: "Keep both" })}</button>
            </div>
          </div>
        </section>
      )}

      <section className="data-management-card data-management-status-card">
        <Cloud size={16} aria-hidden="true" />
        <div>
          <strong>{t("settings.dataManagementSyncStatusLabel", { values: { status: state?.sync.status ?? "idle" }, defaultValue: "Sync status: {{status}}" })}</strong>
          <span>{state?.sync.lastSuccessAt ? t("settings.dataManagementLastSyncAt", { values: { time: new Date(state.sync.lastSuccessAt).toLocaleString() }, defaultValue: "Last successful sync: {{time}}" }) : t("settings.dataManagementNoSyncYet", { defaultValue: "No successful sync recorded yet." })}</span>
          {state?.sync.lastError && <span className="data-management-error-inline">{t("settings.dataManagementLatestError", { values: { error: state.sync.lastError }, defaultValue: "Latest error: {{error}}" })}</span>}
        </div>
      </section>
    </div>
  );
}
