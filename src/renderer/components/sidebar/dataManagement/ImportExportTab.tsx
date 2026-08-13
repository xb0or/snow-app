import { FileDown, FileUp, LockKeyhole, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../../../i18n";
import {
  DATA_MANAGEMENT_FORMAT_VERSION,
  DATA_SECTIONS,
} from "../../../../preload/types/dataManagement";
import type { DataManagementState } from "../../../../preload/types/dataManagement";
import type {
  DataManagementExportRequest,
  DataManagementImportPreview,
  DataManagementImportRequest,
} from "../../../../preload/types/dataManagement";

type ImportExportTabProps = {
  state: DataManagementState | null;
  onPreviewImport: (password?: string) => Promise<DataManagementImportPreview | null>;
  onExport: (request: DataManagementExportRequest) => Promise<DataManagementImportPreview | null>;
  onImport: (request: DataManagementImportRequest) => Promise<DataManagementImportPreview | null>;
};

const SECTION_LABELS: Record<string, string> = {
  "api-config": "API and model configuration",
  "model-settings": "Model settings",
  "system-settings": "System settings",
  mcp: "MCP servers",
  prompts: "Prompts and commands",
  hooks: "Hooks",
  "sub-agents": "Sub-agents",
  "keyboard-shortcuts": "Keyboard shortcuts",
  theme: "Theme",
  skills: "Portable skills",
  plugins: "Managed plugins",
};

export function ImportExportTab({
  state,
  onPreviewImport,
  onExport,
  onImport,
}: ImportExportTabProps): React.JSX.Element {
  const { t } = useI18n();
  const [includeSecrets, setIncludeSecrets] = useState(false);
  const [replaceSelected, setReplaceSelected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [preview, setPreview] = useState<DataManagementImportPreview | null>(null);

  const handleExport = async (): Promise<void> => {
    const password = includeSecrets
      ? window.prompt("Set an encryption password for this export") ?? ""
      : undefined;
    if (includeSecrets && !password) return;
    setBusy(true);
    try {
      const result = await onExport({
        sections: [...DATA_SECTIONS],
        includeSecrets,
        password,
      });
      if (result) setMessage(`Exported ${result.rows} configuration rows`);
    } catch {
      // The shared panel displays the error from useDataManagement.
    } finally {
      setBusy(false);
    }
  };

  const handleImport = async (): Promise<void> => {
    setBusy(true);
    try {
      let nextPreview = await onPreviewImport();
      if (!nextPreview) return;
      const password = nextPreview.encrypted
        ? window.prompt("Enter the package encryption password") ?? ""
        : undefined;
      if (nextPreview.encrypted && !password) return;
      if (password) {
        const decryptedPreview = await onPreviewImport(password);
        if (!decryptedPreview) return;
        nextPreview = decryptedPreview;
      }
      setPreview(nextPreview);
      const description = `${nextPreview.rows} rows, ${nextPreview.sections.length} sections`;
      if (!window.confirm(`Import this configuration package (${description})?`)) return;
      const result = await onImport({
        sections: [...DATA_SECTIONS],
        password,
        replaceSelected,
      });
      if (result) setMessage(`Imported ${result.rows} configuration rows`);
    } catch {
      // The shared panel displays the error from useDataManagement.
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="data-management-tab-content">
      <div className="data-management-hero">
        <div className="data-management-hero-icon">
          <FileDown size={20} strokeWidth={1.7} />
        </div>
        <div>
          <strong>
            {t("settings.dataManagementImportExportTitle", {
              defaultValue: "Portable configuration packages",
            })}
          </strong>
          <p>
            {t("settings.dataManagementImportExportInfo", {
              defaultValue:
                "Move selected Snow App settings between installations without copying sessions or device-specific credentials.",
            })}
          </p>
        </div>
          <span className="data-management-phase-badge">v1</span>
      </div>

      <div className="data-management-card-grid">
        <section className="data-management-card">
          <div className="data-management-card-heading">
            <FileDown size={16} aria-hidden="true" />
            <strong>
              {t("settings.dataManagementExport", {
                defaultValue: "Export configuration",
              })}
            </strong>
          </div>
          <p>
            {t("settings.dataManagementExportInfo", {
              defaultValue:
                "The export flow will create a .snow-config package with a versioned manifest and per-file hashes.",
            })}
          </p>
          <button className="data-management-secondary-button" disabled={busy} onClick={() => void handleExport()} type="button">
            {includeSecrets ? "Export encrypted package" : "Export package"}
          </button>
          <label className="data-management-checkbox-row">
            <input type="checkbox" checked={includeSecrets} onChange={(event) => setIncludeSecrets(event.target.checked)} />
            <span>Include sensitive configuration (requires encryption)</span>
          </label>
        </section>

        <section className="data-management-card">
          <div className="data-management-card-heading">
            <FileUp size={16} aria-hidden="true" />
            <strong>
              {t("settings.dataManagementImport", {
                defaultValue: "Import configuration",
              })}
            </strong>
          </div>
          <p>
            {t("settings.dataManagementImportInfo", {
              defaultValue:
                "Before writing anything, the importer will validate hashes, reject unsafe paths and create a safety snapshot.",
            })}
          </p>
          <button className="data-management-secondary-button" disabled={busy} onClick={() => void handleImport()} type="button">
            Import and preview changes
          </button>
          <label className="data-management-checkbox-row">
            <input type="checkbox" checked={replaceSelected} onChange={(event) => setReplaceSelected(event.target.checked)} />
            <span>Replace selected sections</span>
          </label>
        </section>
      </div>

      {message && <div className="data-management-muted-note" role="status">{message}</div>}

      {preview && (
        <section className="data-management-card data-management-preview-card">
          <div className="data-management-card-heading">
            <ShieldCheck size={16} aria-hidden="true" />
            <strong>Last import preview</strong>
          </div>
          <p>
            {preview.rows} rows across {preview.sections.length} sections; estimated payload {preview.estimatedBytes.toLocaleString()} bytes.
            {preview.deviceSpecificItems > 0 ? ` ${preview.deviceSpecificItems} device-specific values are redacted.` : ""}
          </p>
        </section>
      )}

      <section className="data-management-card data-management-security-card">
        <div className="data-management-card-heading">
          <ShieldCheck size={16} aria-hidden="true" />
          <strong>
            {t("settings.dataManagementPackageRules", {
              defaultValue: "Package rules",
            })}
          </strong>
          <span className="data-management-inline-status">
            {t("settings.dataManagementManifestVersion", {
              values: { version: DATA_MANAGEMENT_FORMAT_VERSION },
              defaultValue: "Manifest v{{version}}",
            })}
          </span>
        </div>
        <div className="data-management-rule-list">
          <div>
            <LockKeyhole size={14} aria-hidden="true" />
            <span>
              {t("settings.dataManagementSecretRule", {
                defaultValue:
                  "Secrets are excluded by default and can only be included in an encrypted package.",
              })}
            </span>
          </div>
          <div>
            <ShieldCheck size={14} aria-hidden="true" />
            <span>
              {t("settings.dataManagementDeviceRule", {
                defaultValue:
                  "Workspace paths, SSH keys and system credentials stay device-local.",
              })}
            </span>
          </div>
        </div>
      </section>

      <section className="data-management-card">
        <div className="data-management-card-heading">
          <strong>
            {t("settings.dataManagementSections", {
              defaultValue: "Portable sections in manifest v1",
            })}
          </strong>
          <span className="data-management-card-meta">
            {state ? `device ${state.deviceId.slice(0, 8)}…` : "—"}
          </span>
        </div>
        <div className="data-management-section-list">
          {DATA_SECTIONS.map((section) => (
            <span key={section} className="data-management-section-chip">
              {SECTION_LABELS[section] ?? section}
            </span>
          ))}
        </div>
      </section>
    </div>
  );
}
