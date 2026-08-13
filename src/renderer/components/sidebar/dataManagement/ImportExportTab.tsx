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

const SECTION_KEYS: Record<string, string> = {
  "api-config": "settings.dataManagementSectionApiConfig",
  "model-settings": "settings.dataManagementSectionModelSettings",
  "system-settings": "settings.dataManagementSectionSystemSettings",
  mcp: "settings.dataManagementSectionMcp",
  prompts: "settings.dataManagementSectionPrompts",
  hooks: "settings.dataManagementSectionHooks",
  "sub-agents": "settings.dataManagementSectionSubAgents",
  "keyboard-shortcuts": "settings.dataManagementSectionKeyboardShortcuts",
  theme: "settings.dataManagementSectionTheme",
  skills: "settings.dataManagementSectionSkills",
  plugins: "settings.dataManagementSectionPlugins",
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
      ? window.prompt(t("settings.dataManagementExportPasswordPrompt", { defaultValue: "Set an encryption password for this export" })) ?? ""
      : undefined;
    if (includeSecrets && !password) return;
    setBusy(true);
    try {
      const result = await onExport({
        sections: [...DATA_SECTIONS],
        includeSecrets,
        password,
      });
      if (result)
        setMessage(
          t("settings.dataManagementExportedRows", {
            values: { rows: result.rows },
            defaultValue: "Exported {{rows}} configuration rows",
          })
        );
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
        ? window.prompt(t("settings.dataManagementImportPasswordPrompt", { defaultValue: "Enter the package encryption password" })) ?? ""
        : undefined;
      if (nextPreview.encrypted && !password) return;
      if (password) {
        const decryptedPreview = await onPreviewImport(password);
        if (!decryptedPreview) return;
        nextPreview = decryptedPreview;
      }
      setPreview(nextPreview);
      const description = t("settings.dataManagementImportDescription", {
        values: { rows: nextPreview.rows, sections: nextPreview.sections.length },
        defaultValue: "{{rows}} rows, {{sections}} sections",
      });
      if (
        !window.confirm(
          t("settings.dataManagementImportConfirm", {
            values: { description },
            defaultValue: "Import this configuration package ({{description}})?",
          })
        )
      )
        return;
      const result = await onImport({
        sections: [...DATA_SECTIONS],
        password,
        replaceSelected,
      });
      if (result)
        setMessage(
          t("settings.dataManagementImportedRows", {
            values: { rows: result.rows },
            defaultValue: "Imported {{rows}} configuration rows",
          })
        );
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
          <div className="data-management-stack-actions">
            <button className="data-management-primary-button" disabled={busy} onClick={() => void handleExport()} type="button">
              {includeSecrets
                ? t("settings.dataManagementExportEncryptedPackage", { defaultValue: "Export encrypted package" })
                : t("settings.dataManagementExportPackage", { defaultValue: "Export package" })}
            </button>
            <div className="data-management-checkbox-row">
              <label className="toggle-switch">
                <input type="checkbox" hidden checked={includeSecrets} onChange={(event) => setIncludeSecrets(event.target.checked)} />
                <span className="toggle-slider" />
              </label>
              <span>{t("settings.dataManagementIncludeSensitive", { defaultValue: "Include sensitive configuration (requires encryption)" })}</span>
            </div>
          </div>
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
          <div className="data-management-stack-actions">
            <button className="data-management-primary-button" disabled={busy} onClick={() => void handleImport()} type="button">
              {t("settings.dataManagementImportAndPreview", { defaultValue: "Import and preview changes" })}
            </button>
            <div className="data-management-checkbox-row">
              <label className="toggle-switch">
                <input type="checkbox" hidden checked={replaceSelected} onChange={(event) => setReplaceSelected(event.target.checked)} />
                <span className="toggle-slider" />
              </label>
              <span>{t("settings.dataManagementReplaceSelected", { defaultValue: "Replace selected sections" })}</span>
            </div>
          </div>
        </section>
      </div>

      {message && <div className="data-management-muted-note" role="status">{message}</div>}

      {preview && (
        <section className="data-management-card data-management-preview-card">
          <div className="data-management-card-heading">
            <ShieldCheck size={16} aria-hidden="true" />
            <strong>{t("settings.dataManagementLastImportPreview", { defaultValue: "Last import preview" })}</strong>
          </div>
          <p>
            {t("settings.dataManagementPreviewSummary", {
              values: {
                rows: preview.rows,
                sections: preview.sections.length,
                bytes: preview.estimatedBytes.toLocaleString(),
              },
              defaultValue: "{{rows}} rows across {{sections}} sections; estimated payload {{bytes}} bytes.",
            })}
            {preview.deviceSpecificItems > 0
              ? t("settings.dataManagementPreviewRedacted", {
                  values: { count: preview.deviceSpecificItems },
                  defaultValue: " {{count}} device-specific values are redacted.",
                })
              : ""}
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
            {state
              ? t("settings.dataManagementDeviceShort", {
                  values: { id: state.deviceId.slice(0, 8) },
                  defaultValue: "device {{id}}",
                })
              : "—"}
          </span>
        </div>
        <div className="data-management-section-list">
          {DATA_SECTIONS.map((section) => (
            <span key={section} className="data-management-section-chip">
              {t(SECTION_KEYS[section] ?? section, { defaultValue: section })}
            </span>
          ))}
        </div>
      </section>
    </div>
  );
}
