use super::*;

use serde_json::{json, Value};

/// imagegen scope（图像生成设置，DB-backed system_settings 表）：
/// - list：返回所有渠道（id/name/provider/enabled/model/configured）概览
/// - get：读取完整配置（apiKey 脱敏）；key 可选（渠道 id / 渠道名 / 协议类型，缺省全部）
/// - set：value 为 {channels: [...]} 全量替换，或 {<channelId>: {...}} 按 id 合并更新（不存在则追加）
/// - delete：清空图像生成设置（所有渠道都未配置时生图工具不再暴露）
pub(crate) fn execute_imagegen_scope(tool_name: &str, args: &Value) -> napi::Result<Value> {
    match tool_name {
        TOOL_LIST => {
            // 先迁移旧格式（{openai, gemini} 顶层字段）为 channels 数组
            let settings = migrate_imagegen_channels(&load_imagegen_settings_value()?);
            let channels = settings
                .get("channels")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let entries: Vec<Value> = channels
                .iter()
                .map(|channel| {
                    let enabled = channel
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let model = channel
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    json!({
                        "key": channel.get("id").and_then(Value::as_str).unwrap_or(""),
                        "id": channel.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": channel.get("name").and_then(Value::as_str).unwrap_or(""),
                        "provider": channel.get("provider").and_then(Value::as_str).unwrap_or("openai"),
                        "enabled": enabled,
                        "model": model,
                        "configured": enabled
                            && !model.is_empty()
                            && !channel.get("apiKey").and_then(Value::as_str).unwrap_or("").is_empty(),
                    })
                })
                .collect();
            Ok(json!({
                "scope": SCOPE_IMAGEGEN,
                "keys": entries,
                "maxConcurrentImages": settings
                    .get("maxConcurrentImages")
                    .cloned()
                    .unwrap_or_else(|| json!(4)),
                "timeoutSecs": settings
                    .get("timeoutSecs")
                    .cloned()
                    .unwrap_or_else(|| json!(300)),
                "note": "Channels are independent; enable one or more at once. When none is configured the imagegen-generate tool is hidden from the model. maxConcurrentImages (top-level global field, 1-8, default 4) caps how many generation requests run in parallel when the agent asks for several images at once; read/write it via config-get / config-set with key=maxConcurrentImages. timeoutSecs (top-level global field, 60-3600, default 300) is the per-request timeout for image generation (including streaming); raise it if complex/high-resolution prompts time out. Pass provider=<channelId|channelName|openai|gemini> to imagegen-generate to pick a channel.",
            }))
        }
        TOOL_GET => {
            // 先迁移旧格式（{openai, gemini} 顶层字段）为 channels 数组
            let settings = migrate_imagegen_channels(&load_imagegen_settings_value()?);
            let requested_key = args
                .get("key")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            match requested_key.as_deref() {
                // 顶层全局字段：最大并发生成数（1-8，默认 4）
                Some(key) if key.eq_ignore_ascii_case("maxConcurrentImages") => {
                    Ok(json!({
                        "scope": SCOPE_IMAGEGEN,
                        "key": "maxConcurrentImages",
                        "value": settings
                            .get("maxConcurrentImages")
                            .cloned()
                            .unwrap_or_else(|| json!(4)),
                    }))
                }
                // 顶层全局字段：生图请求超时（秒，60-3600，默认 300）
                Some(key) if key.eq_ignore_ascii_case("timeoutSecs") => {
                    Ok(json!({
                        "scope": SCOPE_IMAGEGEN,
                        "key": "timeoutSecs",
                        "value": settings
                            .get("timeoutSecs")
                            .cloned()
                            .unwrap_or_else(|| json!(300)),
                    }))
                }
                Some(key) => {
                    let key_lower = key.to_lowercase();
                    let is_provider_type = key_lower == "openai" || key_lower == "gemini";
                    let channel = settings
                        .get("channels")
                        .and_then(Value::as_array)
                        .and_then(|channels| {
                            channels.iter().find(|channel| {
                                if is_provider_type {
                                    channel
                                        .get("provider")
                                        .and_then(Value::as_str)
                                        .map(|provider| provider.eq_ignore_ascii_case(&key_lower))
                                        .unwrap_or(false)
                                } else {
                                    channel
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .map(|id| id.eq_ignore_ascii_case(key))
                                        .unwrap_or(false)
                                        || channel
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .map(|name| name.eq_ignore_ascii_case(key))
                                            .unwrap_or(false)
                                }
                            })
                        })
                        .cloned();
                    match channel {
                        Some(channel) => Ok(json!({
                            "scope": SCOPE_IMAGEGEN,
                            "key": key,
                            "value": mask_channel_api_key(channel),
                        })),
                        None => Err(Error::new(
                            Status::InvalidArg,
                            format!(
                                "Unknown imagegen channel: \"{key}\". Use config-list scope=imagegen to see available channels."
                            ),
                        )),
                    }
                }
                None => Ok(json!({
                    "scope": SCOPE_IMAGEGEN,
                    "key": "settings",
                    "value": mask_channel_api_key(settings),
                })),
            }
        }
        TOOL_SET => {
            let value = args.get("value").cloned().ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    "value is required for config-set (an object with channels or channel overrides)",
                )
            })?;
            if !value.is_object() {
                return Err(Error::new(
                    Status::InvalidArg,
                    "value must be an object like {channels: [...]} or {<channelId>: {...}}",
                ));
            }

            // 先迁移存储为 channels 数组格式
            let mut settings = migrate_imagegen_channels(&load_imagegen_settings_value()?);
            // 写前备份当前 imagegen 设置（写入期间的临时安全网），
            // 写入成功并验证后清理；防止误操作（如误清全部渠道）无法回滚。
            let backup = ConfigService::backup_db_value(
                "imagegen_settings",
                &serde_json::to_string(&settings).unwrap_or_default(),
            )?;
            // 保留「最大并发生成数」（顶层全局字段，设置面板可调）：本次
            // value 中显式提供时采用新值（规范化到 1-8 整数），否则沿用
            // 现有存储值，避免 config-set 重建 {channels} 时把用户配置的
            // 并发上限静默重置。
            let max_concurrent_images = value
                .get("maxConcurrentImages")
                .cloned()
                .or_else(|| settings.get("maxConcurrentImages").cloned())
                .map(clamp_imagegen_max_concurrent);
            // 保留「生图请求超时（秒）」（顶层全局字段，设置面板可调）：本次
            // value 中显式提供时采用新值（规范化到 60-3600），否则沿用现有
            // 存储值，避免 config-set 重建 {channels} 时把用户配置的超时
            // 静默重置。
            let timeout_secs = value
                .get("timeoutSecs")
                .cloned()
                .or_else(|| settings.get("timeoutSecs").cloned())
                .map(clamp_imagegen_timeout_secs);
            if let Some(channels_value) = value.get("channels") {
                // 全量替换
                if !channels_value.is_array() {
                    return Err(Error::new(
                        Status::InvalidArg,
                        "value.channels must be an array",
                    ));
                }
                settings = json!({ "channels": channels_value.clone() });
                if let Some(max_concurrent_images) = max_concurrent_images {
                    settings["maxConcurrentImages"] = max_concurrent_images;
                }
                if let Some(timeout_secs) = timeout_secs {
                    settings["timeoutSecs"] = timeout_secs;
                }
            } else if let Some(value_map) = value.as_object() {
                // 按渠道 id / 名称合并更新；不存在则追加为新渠道
                let mut channels: Vec<Value> = settings
                    .get("channels")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (key, override_value) in value_map {
                    if key == "channels" || !override_value.is_object() {
                        continue;
                    }
                    let is_legacy_key = key == "openai" || key == "gemini";
                    let mut merged_any = false;
                    for channel in channels.iter_mut() {
                        let matches = if is_legacy_key {
                            channel
                                .get("provider")
                                .and_then(Value::as_str)
                                .map(|provider| provider == key.as_str())
                                .unwrap_or(false)
                        } else {
                            channel
                                .get("id")
                                .and_then(Value::as_str)
                                .map(|id| id == key.as_str())
                                .unwrap_or(false)
                                || channel
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map(|name| name == key.as_str())
                                    .unwrap_or(false)
                        };
                        if matches {
                            let mut merged = channel.clone();
                            if let Some(merged_map) = merged.as_object_mut() {
                                if let Some(override_map) = override_value.as_object() {
                                    for (field, val) in override_map {
                                        merged_map.insert(field.clone(), val.clone());
                                    }
                                }
                            }
                            *channel = merged;
                            merged_any = true;
                        }
                    }
                    if !merged_any {
                        let mut new_channel = override_value.clone();
                        if let Some(map) = new_channel.as_object_mut() {
                            map.entry("id".to_string())
                                .or_insert_with(|| json!(key));
                            map.entry("provider".to_string())
                                .or_insert_with(|| json!("openai"));
                            map.entry("name".to_string())
                                .or_insert_with(|| json!(""));
                        }
                        channels.push(new_channel);
                    }
                }
                settings = json!({ "channels": channels });
                if let Some(max_concurrent_images) = max_concurrent_images {
                    settings["maxConcurrentImages"] = max_concurrent_images;
                }
                if let Some(timeout_secs) = timeout_secs {
                    settings["timeoutSecs"] = timeout_secs;
                }
            }

            save_imagegen_settings_value(&settings)?;
            // 写入成功：清理本次写前备份（临时安全网不再需要）。
            ConfigService::cleanup_backup(backup);
            Ok(json!({
                "scope": SCOPE_IMAGEGEN,
                "key": "settings",
                "value": mask_channel_api_key(settings),
            }))
        }
        TOOL_DELETE => {
            // 写前备份当前 imagegen 设置（写入期间的临时安全网），
            // 清空成功并验证后清理；防止误操作无法回滚。
            let current = migrate_imagegen_channels(&load_imagegen_settings_value()?);
            let backup = ConfigService::backup_db_value(
                "imagegen_settings",
                &serde_json::to_string(&current).unwrap_or_default(),
            )?;
            save_imagegen_settings_value(&json!({}))?;
            ConfigService::cleanup_backup(backup);
            Ok(json!({
                "scope": SCOPE_IMAGEGEN,
                "key": "settings",
                "deleted": true,
            }))
        }
        _ => Err(Error::new(
            Status::GenericFailure,
            format!(
                "Unknown tool: \"{tool_name}\" for MCP server \"{SERVER_ID}\". Available tools: [config-list, config-get, config-set, config-delete]"
            ),
        )),
    }
}

/// 规范化「最大并发生成数」：必须是有限数字，取整后收敛到 1-8 范围
/// （与设置面板 IMAGE_GEN_MAX_CONCURRENT_RANGE 一致）；非法值回退默认 4。
fn clamp_imagegen_max_concurrent(value: Value) -> Value {
    let Some(number) = value.as_f64().filter(|n| n.is_finite()) else {
        return json!(4);
    };
    json!(number.round().clamp(1.0, 8.0) as i64)
}

/// 规范化「生图请求超时（秒）」：必须是有限数字，取整后收敛到 60-3600
/// （与设置面板 IMAGE_GEN_TIMEOUT_RANGE 一致）；非法值回退默认 300。
fn clamp_imagegen_timeout_secs(value: Value) -> Value {
    let Some(number) = value.as_f64().filter(|n| n.is_finite()) else {
        return json!(300);
    };
    json!(number.round().clamp(60.0, 3600.0) as i64)
}

/// 将任意 imagegen 存储格式迁移为 { channels: [...] } 新格式：
/// - 已有 channels 数组 → 原样返回
/// - 旧双渠道 {openai, gemini} → 转为渠道数组
/// - 更旧单渠道（顶层字段）→ 转为单个渠道
fn migrate_imagegen_channels(settings: &Value) -> Value {
    if let Some(channels) = settings.get("channels") {
        if channels.is_array() {
            return settings.clone();
        }
    }

    let mut channels: Vec<Value> = Vec::new();
    for (key, provider) in [("openai", "openai"), ("gemini", "gemini")] {
        if let Some(channel) = settings.get(key) {
            if channel.is_object() {
                let mut migrated = channel.clone();
                if let Some(map) = migrated.as_object_mut() {
                    map.entry("id".to_string()).or_insert_with(|| json!(key));
                    map.entry("provider".to_string())
                        .or_insert_with(|| json!(provider));
                    map.entry("name".to_string()).or_insert_with(|| json!(""));
                }
                channels.push(migrated);
            }
        }
    }

    // 更旧单渠道格式（顶层 apiKey/model/...）
    if channels.is_empty() && settings.get("apiKey").is_some() {
        let old_provider = settings
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("");
        let old_base_url = settings
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_gemini = old_provider == "gemini"
            || old_base_url.contains("generativelanguage")
            || old_base_url.contains("googleapis.com");
        let mut channel = settings.clone();
        if let Some(map) = channel.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(if is_gemini { "gemini" } else { "openai" }),
            );
            map.insert(
                "provider".to_string(),
                json!(if is_gemini { "gemini" } else { "openai" }),
            );
            map.insert("enabled".to_string(), json!(true));
        }
        channels.push(channel);
    }

    json!({ "channels": channels })
}

/// 读取 imagegen_settings 的 JSON 值（无配置时返回空对象）。
fn load_imagegen_settings_value() -> napi::Result<Value> {
    let storage_info = crate::storage::initialize_app_storage()?;
    let database_path = std::path::PathBuf::from(storage_info.database_path);
    let value = crate::storage::services::system_settings::get_system_setting_value(
        &database_path,
        "imagegen_settings",
    )?;
    match value {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw).map_err(|error| {
            Error::from_reason(format!("Failed to parse imagegen settings: {error}"))
        }),
        _ => Ok(json!({})),
    }
}

/// 写入 imagegen_settings。
fn save_imagegen_settings_value(settings: &Value) -> napi::Result<()> {
    let storage_info = crate::storage::initialize_app_storage()?;
    let database_path = std::path::PathBuf::from(storage_info.database_path);
    crate::storage::services::system_settings::set_system_setting(
        &database_path,
        "Image Generation Settings",
        "imagegen_settings",
        &serde_json::to_string(settings).map_err(|error| {
            Error::from_reason(format!("Failed to serialize imagegen settings: {error}"))
        })?,
    )
    .map_err(|error| Error::from_reason(format!("Failed to save imagegen settings: {error}")))
}

/// 递归脱敏对象中的 apiKey 字段（如 sk-****abcd），防止明文密钥外泄。
fn mask_channel_api_key(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut masked = serde_json::Map::new();
            for (key, val) in map {
                if key == "apiKey" {
                    if let Some(text) = val.as_str() {
                        masked.insert(key, json!(mask_api_key(text)));
                        continue;
                    }
                }
                masked.insert(key, mask_channel_api_key(val));
            }
            Value::Object(masked)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(mask_channel_api_key).collect()),
        other => other,
    }
}

pub(crate) fn mask_api_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= 8 {
        return "****".to_string();
    }
    let prefix = &trimmed[..4];
    let suffix = &trimmed[trimmed.len() - 4..];
    format!("{prefix}****{suffix}")
}
