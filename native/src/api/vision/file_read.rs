//! 磁盘图片读取与视觉分析（image-describe 工具入口）。

use super::*;

/// 读取磁盘上的图片并用视觉模型分析（`image-describe` 工具入口）。
///
/// 路径校验：
/// - 绝对路径：直接读取（用户本地任意目录的图片，如项目中的 UI 设计稿）；
/// - 相对路径：必须位于 `upload/` 目录内（相对数据库文件所在目录），拒绝穿越。
///
/// 限制：单张 20MB 上限；仅接受图片扩展名。视觉配置复用主 API 配置的
/// vision 通道（chat / responses / anthropic / gemini），结果走
/// [`describe_image`] 的 blake3 内容缓存。
pub(crate) async fn describe_image_file(path: &str, user_prompt: &str) -> Result<String> {
    use std::fs;

    let trimmed = path.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return Err(Error::from_reason(
            "Image path must not be empty".to_string(),
        ));
    }

    // 1. 解析磁盘路径
    let file_path = if std::path::Path::new(&trimmed).is_absolute() {
        std::path::PathBuf::from(&trimmed)
    } else {
        if !trimmed.starts_with("upload/") || trimmed.contains("..") {
            return Err(Error::from_reason(format!(
                "Invalid image path: \"{path}\". Use an absolute file path (e.g. C:/path/to/design.png) or a relative path under the conversation's upload/ directory."
            )));
        }
        let storage_info = crate::storage::initialize_app_storage()?;
        let database_path = std::path::PathBuf::from(storage_info.database_path);
        database_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(&trimmed)
    };

    // 2. 大小与类型校验
    const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
    let metadata = fs::metadata(&file_path).map_err(|error| {
        Error::from_reason(format!("Cannot read image file \"{path}\": {error}"))
    })?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(Error::from_reason(format!(
            "Image file \"{path}\" exceeds the {}MB size limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        )));
    }
    let bytes = fs::read(&file_path).map_err(|error| {
        Error::from_reason(format!("Cannot read image file \"{path}\": {error}"))
    })?;
    let mime_type = guess_image_mime(&file_path);
    if !mime_type.starts_with("image/") {
        return Err(Error::from_reason(format!(
            "File \"{path}\" is not a supported image (detected {mime_type})"
        )));
    }

    // 3. 复用视觉管线（配置 + 客户端 + 缓存）
    let context = crate::api::config::get_active_api_request_context()?;
    let vision_config = VisionApiConfig::from(&context.api_config, &context.custom_headers)?;
    let client = crate::api::http_client::build_proxied_client()
        .await
        .map_err(|error| {
            Error::from_reason(format!("Failed to create vision HTTP client: {error}"))
        })?;

    let image = ChatImage {
        media_type: mime_type.clone(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        data_url: String::new(),
        source: None,
    };
    // 工具入口没有请求级取消令牌（工具执行取消走独立的 tool 注册表），
    // 因此不参与 select! 取消竞争，行为与之前一致。
    describe_image(&client, &vision_config, &image, user_prompt, None).await
}

/// 按扩展名猜测图片 MIME；不支持的类型返回 `application/octet-stream`。
fn guess_image_mime(path: &std::path::Path) -> String {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "application/octet-stream".to_string();
    };
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "webp" => "image/webp".to_string(),
        "gif" => "image/gif".to_string(),
        "bmp" => "image/bmp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}
