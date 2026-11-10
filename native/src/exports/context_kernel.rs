use std::path::Path;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::api::conversation::context_kernel;

/// 只读：返回当前会话的上下文内核状态 JSON（块、用量、阈值、content store 统计）。
///
/// 仅供 UI 展示；`conversation_id` 为空时返回空串。子代理视图不暴露给 UI。
#[napi]
pub fn get_context_kernel_status(conversation_id: String) -> Result<String> {
    let conversation_id = conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Ok(String::new());
    }
    let storage = crate::storage::initialize_app_storage()?;
    let snapshot = context_kernel::status_snapshot(
        Path::new(&storage.database_path),
        &conversation_id,
    )?;
    Ok(snapshot.to_string())
}
