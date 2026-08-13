//! 插件与插件市场的 NAPI 转发。

use super::*;

#[napi]
pub async fn list_plugins() -> napi::Result<Vec<PluginRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_plugins)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_plugins(items: Vec<PluginInput>) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_plugins(items))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn set_plugin_state(plugin_id: String, state: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::set_plugin_state(plugin_id, state))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_plugin(plugin_id: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_plugin(plugin_id))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn list_plugin_marketplaces() -> napi::Result<Vec<PluginMarketplaceRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_plugin_marketplaces)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_plugin_marketplace(item: PluginMarketplaceInput) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_plugin_marketplace(item))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_plugin_marketplace(marketplace_id: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_plugin_marketplace(marketplace_id))
        .await
        .map_err(map_spawn_error)?
}
