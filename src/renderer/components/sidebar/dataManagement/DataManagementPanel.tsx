import { Cloud, DatabaseBackup, FileDown, X } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../../../i18n";
import type { DataManagementProgress } from "../../../../preload";
import { BackupRestoreTab } from "./BackupRestoreTab";
import { CloudSyncTab } from "./CloudSyncTab";
import { ImportExportTab } from "./ImportExportTab";
import { useDataManagement } from "./useDataManagement";
import "./dataManagement.css";

type DataManagementTab = "import-export" | "backup-restore" | "cloud-sync";

type DataManagementPanelProps = {
  onClose?: () => void;
};

const renderProgressLabel = (
  progress: DataManagementProgress,
  t: (key: string, options?: { defaultValue?: string }) => string
): string => {
  if (progress.status === "completed") {
    return t("settings.dataManagementTaskCompleted", {
      defaultValue: "Task completed",
    });
  }
  if (progress.status === "cancelled") {
    return t("settings.dataManagementTaskCancelled", {
      defaultValue: "Task cancelled",
    });
  }
  if (progress.status === "failed") {
    return t("settings.dataManagementTaskFailed", {
      defaultValue: "Task failed",
    });
  }
  return progress.phase;
};

export function DataManagementPanel({
  onClose,
}: DataManagementPanelProps): React.JSX.Element {
  const { t } = useI18n();
  const [activeTab, setActiveTab] = useState<DataManagementTab>("import-export");
  const {
    state,
    settings,
    progress,
    isLoading,
    isSaving,
    error,
    updateSettings,
    previewImport,
    exportConfig,
    importConfig,
    createBackup,
    restoreBackup,
    deleteBackup,
    testSync,
    runSync,
    resolveConflict,
  } = useDataManagement();

  const tabs: Array<{
    id: DataManagementTab;
    icon: typeof FileDown;
    label: string;
  }> = [
    {
      id: "import-export",
      icon: FileDown,
      label: t("settings.dataManagementImportExportTab", {
        defaultValue: "Import & export",
      }),
    },
    {
      id: "backup-restore",
      icon: DatabaseBackup,
      label: t("settings.dataManagementBackupRestoreTab", {
        defaultValue: "Backup & restore",
      }),
    },
    {
      id: "cloud-sync",
      icon: Cloud,
      label: t("settings.dataManagementCloudSyncTab", {
        defaultValue: "Cloud sync",
      }),
    },
  ];

  return (
    <div className="api-settings-page data-management-page" role="region">
      <div className="api-settings-page-header">
        <div className="api-settings-title-group">
          <div className="data-management-title-row">
            <DatabaseBackup size={17} strokeWidth={1.8} aria-hidden="true" />
            <strong>
              {t("settings.dataManagement", {
                defaultValue: "Data management",
              })}
            </strong>
          </div>
          <span className="settings-item-description">
            {t("settings.dataManagementInfo", {
              defaultValue:
                "Portable settings, database snapshots and encrypted multi-device sync.",
            })}
          </span>
        </div>
        {onClose && (
          <button
            className="icon-btn ghost"
            onClick={onClose}
            type="button"
            aria-label={t("settings.dataManagementClosePanel", {
              defaultValue: "Close data management",
            })}
            title={t("settings.dataManagementClosePanel", {
              defaultValue: "Close data management",
            })}
          >
            <X size={15} strokeWidth={1.8} />
          </button>
        )}
      </div>

      <div className="data-management-summary">
        <span className="data-management-device-pill">
          <DatabaseBackup size={13} aria-hidden="true" />
          {isLoading
            ? t("common.loading", { defaultValue: "Loading..." })
            : state?.deviceName ?? "Snow App"}
        </span>
        <span className="data-management-summary-meta">
          {state
            ? t("settings.dataManagementManifestVersion", {
                values: { version: state.manifestFormatVersion },
                defaultValue: "Manifest v{{version}}",
              })
            : "—"}
        </span>
      </div>

      <div className="data-management-tabs" role="tablist">
        {tabs.map((tab) => {
          const Icon = tab.icon;
          const isActive = activeTab === tab.id;
          return (
            <button
              key={tab.id}
              className={`data-management-tab ${isActive ? "active" : ""}`}
              type="button"
              role="tab"
              aria-selected={isActive}
              onClick={() => setActiveTab(tab.id)}
            >
              <Icon size={14} strokeWidth={1.8} aria-hidden="true" />
              <span>{tab.label}</span>
            </button>
          );
        })}
      </div>

      {progress && progress.status !== "running" && (
        <div className={`data-management-progress ${progress.status}`} role="status">
          <span>{renderProgressLabel(progress, t)}</span>
          {progress.error && <span>{progress.error}</span>}
        </div>
      )}
      {error && <div className="data-management-error" role="alert">{error}</div>}

      {activeTab === "import-export" ? (
        <ImportExportTab
          state={state}
          onPreviewImport={previewImport}
          onExport={exportConfig}
          onImport={importConfig}
        />
      ) : activeTab === "backup-restore" ? (
        <BackupRestoreTab
          state={state}
          settings={settings}
          isSaving={isSaving}
          onUpdateSettings={updateSettings}
          onCreate={createBackup}
          onRestore={restoreBackup}
          onDelete={deleteBackup}
        />
      ) : (
        <CloudSyncTab
          state={state}
          settings={settings}
          isSaving={isSaving}
          onUpdateSettings={updateSettings}
          onTestConnection={testSync}
          onSync={runSync}
          onResolveConflict={resolveConflict}
        />
      )}
    </div>
  );
}
