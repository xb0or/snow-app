//! 导入资源（数据库迁移）的 NAPI 转发。

use super::*;

#[napi]
pub async fn list_import_resources() -> napi::Result<Vec<ImportResourceRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_import_resources)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_import_resources(items: Vec<ImportResourceInput>) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_import_resources(items))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn commit_import_transaction(input: ImportDatabaseTransactionInput) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::commit_import_transaction(input))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn release_import_resource(
    input: ImportResourceReleaseInput,
) -> napi::Result<ImportResourceRelease> {
    tokio::task::spawn_blocking(move || crate::storage::release_import_resource(input))
        .await
        .map_err(map_spawn_error)?
}
