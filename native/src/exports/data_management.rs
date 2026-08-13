use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::storage::services::data_management::DatabaseBackupInfo;

fn map_spawn_error(error: tokio::task::JoinError) -> Error {
    Error::from_reason(format!("Data-management worker task failed: {error}"))
}

#[napi(object)]
pub struct DatabaseBackupInfoNapi {
    pub database_path: String,
    pub archive_database_path: Option<String>,
    pub schema_version: i64,
    pub database_size_bytes: i64,
    pub archive_database_size_bytes: Option<i64>,
}

impl From<DatabaseBackupInfo> for DatabaseBackupInfoNapi {
    fn from(value: DatabaseBackupInfo) -> Self {
        Self {
            database_path: value.database_path,
            archive_database_path: value.archive_database_path,
            schema_version: value.schema_version,
            database_size_bytes: value.database_size_bytes as i64,
            archive_database_size_bytes: value
                .archive_database_size_bytes
                .map(|size| size as i64),
        }
    }
}

#[napi]
pub async fn create_database_online_backup(
    main_destination: String,
    archive_destination: Option<String>,
    include_archive: bool,
) -> napi::Result<DatabaseBackupInfoNapi> {
    tokio::task::spawn_blocking(move || {
        crate::storage::services::data_management::create_database_online_backup(
            main_destination,
            archive_destination,
            include_archive,
        )
    })
    .await
    .map_err(map_spawn_error)?
    .map(DatabaseBackupInfoNapi::from)
}

#[napi]
pub async fn quick_check_database(path: String) -> napi::Result<String> {
    tokio::task::spawn_blocking(move || {
        crate::storage::services::data_management::quick_check_database(path)
    })
    .await
    .map_err(map_spawn_error)?
}

#[napi]
pub async fn export_data_management_config(
    sections_json: String,
    include_secrets: bool,
) -> napi::Result<String> {
    tokio::task::spawn_blocking(move || {
        crate::storage::services::data_management::export_config_data(
            sections_json,
            include_secrets,
        )
    })
    .await
    .map_err(map_spawn_error)?
}

#[napi]
pub async fn apply_data_management_config(
    config_json: String,
    sections_json: String,
    replace_selected: bool,
) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || {
        crate::storage::services::data_management::apply_config_data(
            config_json,
            sections_json,
            replace_selected,
        )
    })
    .await
    .map_err(map_spawn_error)?
}
