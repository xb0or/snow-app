import { Check, Cloud, LockKeyhole, Save, ShieldAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useI18n } from "../../../i18n";
import type {
  DataManagementSettings,
  DataManagementSettingsPatch,
  DataManagementState,
} from "../../../../preload";

type CloudSyncTabProps = {
  state: DataManagementState | null;
  settings: DataManagementSettings | null;
  isSaving: boolean;
  onUpdateSettings: (patch: DataManagementSettingsPatch) => Promise<void>;
  onTestConnection: () => Promise<{ weakConflictProtection: boolean }>;
  onSync: () => Promise<unknown | null>;
  onResolveConflict: (choice: "local" | "remote" | "keep-both") => Promise<unknown | null>;
};

export function CloudSyncTab({
  state,
  settings,
  isSaving,
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
  const [saved, setSaved] = useState(false);
  const [webdavPassword, setWebdavPassword] = useState("");
  const [syncMasterKey, setSyncMasterKey] = useState("");
  const [syncEnabled, setSyncEnabled] = useState(false);
  const [syncIntervalMinutes, setSyncIntervalMinutes] = useState<0 | 15 | 30 | 60>(0);
  const [syncMode, setSyncMode] = useState<"config" | "mirror">("config");
  const [allowInsecureHttp, setAllowInsecureHttp] = useState(false);
  const [busy, setBusy] = useState(false);
  const [statusMessage, setStatusMessage] = useState("");

  useEffect(() => {
    if (!settings) {
      return;
    }
    setDeviceName(settings.deviceName);
    setEndpoint(settings.webdav.endpoint);
    setRemoteRoot(settings.webdav.remoteRoot);
    setUsername(settings.webdav.username);
    setSyncEnabled(settings.webdav.syncEnabled);
    setSyncIntervalMinutes(settings.webdav.syncIntervalMinutes);
    setSyncMode(settings.webdav.syncMode);
    setAllowInsecureHttp(settings.webdav.allowInsecureHttp);
  }, [settings]);

  const handleSave = async (): Promise<void> => {
    setBusy(true);
    try {
      if (webdavPassword.trim()) {
        await window.snow.setDataManagementCredential({ kind: "webdav-password", value: webdavPassword });
        setWebdavPassword("");
      }
      if (syncMasterKey.trim()) {
        await window.snow.setDataManagementCredential({ kind: "sync-master-key", value: syncMasterKey });
        setSyncMasterKey("");
      }
      await onUpdateSettings({
        deviceName,
        webdav: { endpoint, remoteRoot, username, syncEnabled, syncIntervalMinutes, syncMode, allowInsecureHttp },
      });
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2200);
    } finally {
      setBusy(false);
    }
  };

  const testConnection = async (): Promise<void> => {
    setBusy(true);
    try {
      const result = await onTestConnection();
      setStatusMessage(result.weakConflictProtection ? "Connected; server has weak ETag protection" : "WebDAV connection is ready");
    } finally { setBusy(false); }
  };

  const runSync = async (): Promise<void> => {
    setBusy(true);
    try { await onSync(); setStatusMessage("Sync completed"); } finally { setBusy(false); }
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
        <span className="data-management-phase-badge">Encrypted ETag sync</span>
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
              autoComplete="username"
            />
          </label>
          <label>
            <span>WebDAV password</span>
            <input type="password" value={webdavPassword} onChange={(event) => setWebdavPassword(event.target.value)} autoComplete="new-password" placeholder={state?.credentialStatus.webdavPasswordConfigured ? "Configured" : "Required"} />
          </label>
          <label>
            <span>Sync encryption password</span>
            <input type="password" value={syncMasterKey} onChange={(event) => setSyncMasterKey(event.target.value)} autoComplete="new-password" placeholder={state?.credentialStatus.syncMasterKeyConfigured ? "Configured" : "Required"} />
          </label>
          <label>
            <span>Sync mode</span>
            <select value={syncMode} onChange={(event) => setSyncMode(event.target.value as "config" | "mirror")}>
              <option value="config">Configuration sync</option>
              <option value="mirror">Full database mirror (restart)</option>
            </select>
          </label>
          <label>
            <span>Automatic sync</span>
            <select value={syncIntervalMinutes} onChange={(event) => setSyncIntervalMinutes(Number(event.target.value) as 0 | 15 | 30 | 60)}>
              <option value={0}>Manual only</option>
              <option value={15}>Every 15 minutes</option>
              <option value={30}>Every 30 minutes</option>
              <option value={60}>Every hour</option>
            </select>
          </label>
        </div>
        <label className="data-management-checkbox-row data-management-risk-option">
          <input type="checkbox" checked={allowInsecureHttp} onChange={(event) => setAllowInsecureHttp(event.target.checked)} />
          <span>Allow insecure HTTP (high risk; HTTPS remains the default)</span>
        </label>
        <div className="data-management-form-footer">
          <span className="data-management-muted-note">
            {statusMessage || "Remote objects are encrypted before upload; passwords never leave the main process."}
          </span>
          <label className="data-management-checkbox-row"><input type="checkbox" checked={syncEnabled} onChange={(event) => setSyncEnabled(event.target.checked)} /><span>Enable automatic sync</span></label>
          <button className="data-management-secondary-button" disabled={busy || !endpoint.trim()} type="button" onClick={() => void testConnection()}>Test connection</button>
          <button className="data-management-secondary-button" disabled={busy || !endpoint.trim()} type="button" onClick={() => void runSync()}>Sync now</button>
          <button
            className="data-management-primary-button"
            type="button"
            onClick={() => void handleSave()}
            disabled={isSaving || busy || !deviceName.trim()}
          >
            {saved ? <Check size={14} aria-hidden="true" /> : <Save size={14} aria-hidden="true" />}
            {saved
              ? t("settings.dataManagementSaved", { defaultValue: "Saved" })
              : t("settings.dataManagementSave", { defaultValue: "Save" })}
          </button>
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
            <strong>Sync conflict needs a choice</strong>
            <span>Remote revision {state.sync.conflict.remoteRevision} from {state.sync.conflict.remoteDeviceName}; local data was kept.</span>
            <div className="data-management-form-footer">
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("local")} type="button">Keep local and upload</button>
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("remote")} type="button">Use remote</button>
              <button className="data-management-secondary-button" disabled={busy} onClick={() => void onResolveConflict("keep-both")} type="button">Keep both</button>
            </div>
          </div>
        </section>
      )}

      <section className="data-management-card data-management-status-card">
        <Cloud size={16} aria-hidden="true" />
        <div>
          <strong>Sync status: {state?.sync.status ?? "idle"}</strong>
          <span>{state?.sync.lastSuccessAt ? `Last successful sync: ${new Date(state.sync.lastSuccessAt).toLocaleString()}` : "No successful sync recorded yet."}</span>
          {state?.sync.lastError && <span className="data-management-error-inline">Latest error: {state.sync.lastError}</span>}
        </div>
      </section>
    </div>
  );
}
