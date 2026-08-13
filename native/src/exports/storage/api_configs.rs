//! API 配置、系统提示词与自定义请求头方案的 NAPI 转发。

use super::*;

#[napi]
pub async fn list_api_configs() -> napi::Result<Vec<ApiConfigRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_api_configs)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_api_config(config: ApiConfigInput) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_api_config(config))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_api_config(profile_name: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_api_config(profile_name))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn list_system_prompts() -> napi::Result<Vec<SystemPromptItemRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_system_prompts)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_system_prompt(item: SystemPromptItemInput) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_system_prompt(item))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_system_prompt(prompt_id: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_system_prompt(prompt_id))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn list_custom_header_schemes() -> napi::Result<Vec<CustomHeaderSchemeRecord>> {
    tokio::task::spawn_blocking(crate::storage::list_custom_header_schemes)
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn upsert_custom_header_scheme(item: CustomHeaderSchemeInput) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::upsert_custom_header_scheme(item))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_custom_header_scheme(scheme_id: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_custom_header_scheme(scheme_id))
        .await
        .map_err(map_spawn_error)?
}
