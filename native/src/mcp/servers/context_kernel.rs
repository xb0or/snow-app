//! 上下文内核内置服务：把 billion-context `acp-kernel` 的模型驱动压缩能力
//! 暴露为 Snow 的内置工具。
//!
//! 工具只操作「出站视图」状态（压缩块 / 摘要），不修改会话历史、不写工作区，
//! 因此全部列为只读工具（免审批）。状态按 `conversation_id + 子代理标记` 隔离。
//!
//! 上游来源：https://github.com/ranxianglei/billion-context（`.acp-kernel`，MIT）。

#![allow(dead_code)]

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

use super::super::service::McpService;
use super::super::tools::McpTool;
use crate::api::conversation::context_kernel;

const SERVER_ID: &str = "context-kernel";

/// 阈值来自当前 API 档案：`autoCompressThreshold` 优先，否则取上下文窗口的 60%。
fn resolve_threshold() -> Option<i64> {
    let context = crate::api::config::get_active_api_request_context().ok()?;
    context_kernel::resolve_threshold(
        context.api_config.auto_compress_threshold,
        context.api_config.max_context_tokens,
    )
}

pub struct ContextKernelService {
    db_path: String,
}

impl ContextKernelService {
    pub fn new() -> Self {
        let db_path = crate::storage::initialize_app_storage()
            .map(|info| info.database_path)
            .unwrap_or_default();
        Self { db_path }
    }

    /// 异步入口：由 `call_mcp_tool` 分发层注入当前会话与调用方身份。
    pub async fn execute_async(
        &self,
        tool_name: &str,
        args: &Value,
        conversation_id: Option<&str>,
        is_sub_agent_call: bool,
    ) -> napi::Result<Value> {
        let conversation_id = conversation_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::new(
                    Status::GenericFailure,
                    "context-kernel tools require an active conversation; the folded view is unavailable until the conversation is created.",
                )
            })?
            .to_string();
        let db_path = self.db_path.clone();
        if db_path.trim().is_empty() {
            return Err(Error::new(
                Status::GenericFailure,
                "Context kernel state is unavailable: app storage is not initialized.",
            ));
        }
        let tool_name = tool_name.to_string();
        let args = args.clone();
        let sub_agent_id = context_kernel::sub_agent_key(is_sub_agent_call, &conversation_id);
        let threshold = resolve_threshold();

        tokio::task::spawn_blocking(move || {
            context_kernel::execute_tool(
                &tool_name,
                &args,
                std::path::Path::new(&db_path),
                &conversation_id,
                &sub_agent_id,
                threshold,
            )
        })
        .await
        .map_err(|error| {
            Error::new(
                Status::GenericFailure,
                format!("HOST_OPERATION_FAILED: context kernel task join error: {error}"),
            )
        })?
    }
}

impl McpService for ContextKernelService {
    fn id(&self) -> &str {
        SERVER_ID
    }

    fn tools(&self) -> Vec<McpTool> {
        vec![
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "compress".to_string(),
                description: "Fold a contiguous range of older conversation messages into one durable summary. ONLY the outbound request view is folded: the stored transcript and the UI keep every original message.\\n\\nWHEN: the context kernel nudge reports the request is near the auto-compress threshold, or older turns are no longer needed verbatim.\\n\\nHOW:\\n1. Call `context-kernel-acp_status` and read `refs` (each entry has `ref`, `role`, `tokens`, `pinned`, `tailProtected`, `compressed`).\\n2. Pick a contiguous range of OLD messages: never the pinned first user message and never a ref where `tailProtected` is true.\\n3. Write a self-contained `summary` in the SAME language as the conversation. Preserve: the user's actual request and constraints, decisions and their reasons, exact file paths and symbols, commands and errors, verification results, and what is still open. Drop chatter and redundant tool noise.\\n4. Call this tool with `from`, `to`, `summary` and an optional `title`.\\n\\nRULES: a range must cover at least 2 messages, must not overlap an existing block (decompress it first), and system messages cannot be compressed. Refs are stable across turns; ranges that reference unknown refs are rejected with the current refs available via `context-kernel-acp_status`.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "First message ref of the range, e.g. \"m1a2b3c4\" (from context-kernel-acp_status)."
                        },
                        "to": {
                            "type": "string",
                            "description": "Last message ref of the range (inclusive). May be before or after `from`; order is normalised."
                        },
                        "title": {
                            "type": "string",
                            "description": "Short label for the block, e.g. \"repo recon and build fix\"."
                        },
                        "summary": {
                            "type": "string",
                            "description": "Self-contained summary the next turns can rely on. Keep the conversation language, decisions, paths, commands, errors, and open work."
                        }
                    },
                    "required": ["from", "to", "summary"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "decompress".to_string(),
                description: "Restore a compressed block: the original messages become visible to the model again and the block is removed from the outbound view. Use it when a folded detail is needed verbatim, then re-compress a tighter range if the window is still tight.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "blockId": {
                            "type": "string",
                            "description": "Block id from context-kernel-acp_status, e.g. \"b1\"."
                        }
                    },
                    "required": ["blockId"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "search_context".to_string(),
                description: "Search the FULL stored transcript (including messages hidden behind compressed blocks) plus block summaries by keyword. Returns matching message refs and blocks with previews. Use it instead of decompressing everything, then compress a tighter range only if needed.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords to look up (case-insensitive; multiple words are scored independently)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results (1-30, default 8)."
                        }
                    },
                    "required": ["query"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "retrieve".to_string(),
                description: "Read the original content behind a `[context-kernel:stored ref=...]` placeholder. Large tool results are frozen into placeholders in the outbound view to keep requests small; this tool returns the full stored text by ref.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ref": {
                            "type": "string",
                            "description": "Stored ref from the placeholder header, e.g. \"cr1a2b3c4d5e6f7890\"."
                        }
                    },
                    "required": ["ref"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "acp_status".to_string(),
                description: "Report the context kernel state: estimated outbound tokens, threshold, whether compression is needed, active blocks, and the stable message refs (newest `refLimit` entries) with role, token estimate, preview, pinned and tailProtected flags. Call it before compress to choose a range.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "refLimit": {
                            "type": "integer",
                            "description": "How many recent message refs to return (4-200, default 40)."
                        }
                    }
                }),
            },
        ]
    }

    /// 同步入口不可用：内核工具必须拿到调用期会话上下文。
    fn execute(&self, tool_name: &str, _args: &Value) -> napi::Result<Value> {
        Err(Error::new(
            Status::GenericFailure,
            format!(
                "Tool \"{tool_name}\" of server \"{SERVER_ID}\" requires the async call_mcp_tool path (conversation context). Available tools: [context-kernel-compress, context-kernel-decompress, context-kernel-retrieve, context-kernel-search_context, context-kernel-acp_status]"
            ),
        ))
    }
}
