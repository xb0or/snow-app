//! 用量记录与应用日志的 NAPI 转发。

use super::*;

// ===== Usage records NAPI 导出 =====

#[napi]
pub async fn list_usage_records(
    conversation_id: String,
    directory_id: String,
    limit: i32,
    offset: i32,
) -> napi::Result<crate::storage::services::usage_records::UsageRecordPage> {
    tokio::task::spawn_blocking(move || {
        crate::storage::list_usage_records(conversation_id, directory_id, limit, offset)
    })
    .await
    .map_err(map_spawn_error)?
}

#[napi]
pub async fn get_usage_summary(
    since: String,
    until: String,
) -> napi::Result<crate::storage::services::usage_records::UsageSummary> {
    tokio::task::spawn_blocking(move || crate::storage::get_usage_summary(since, until))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn get_usage_daily_breakdown(
    since: String,
    until: String,
) -> napi::Result<Vec<crate::storage::services::usage_records::DailyUsageBreakdown>> {
    tokio::task::spawn_blocking(move || crate::storage::get_usage_daily_breakdown(since, until))
        .await
        .map_err(map_spawn_error)?
}

// ===== App logs NAPI 导出 =====

#[napi]
pub async fn write_app_log(
    input: crate::storage::services::app_logs::AppLogInput,
) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::write_app_log(input))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn list_app_logs(
    level: String,
    module: String,
    since: String,
    until: String,
    limit: i32,
    offset: i32,
) -> napi::Result<crate::storage::services::app_logs::AppLogPage> {
    tokio::task::spawn_blocking(move || {
        crate::storage::list_app_logs(level, module, since, until, limit, offset)
    })
    .await
    .map_err(map_spawn_error)?
}

#[napi]
pub async fn clear_app_logs() -> napi::Result<u32> {
    tokio::task::spawn_blocking(crate::storage::clear_app_logs)
        .await
        .map_err(map_spawn_error)?
}
