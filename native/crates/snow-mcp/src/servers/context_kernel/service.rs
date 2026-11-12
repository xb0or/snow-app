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

use crate::service::McpService;
use crate::tools::McpTool;
use crate::servers::context_kernel as context_kernel;

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
        // 子代理的状态键按**运行（run）**隔离，且必须与出站视图用同一套解析：
        // 否则工具会读到另一个桶，模型手里的 ref 与它看到的视图对不上。
        let sub_agent_id = if is_sub_agent_call {
            context_kernel::resolve_sub_agent_key(
                std::path::Path::new(&self.db_path),
                &conversation_id,
                None,
            )
        } else {
            String::new()
        };
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
                description: "Fold a contiguous range of older conversation messages into one durable summary block. ONLY the outbound request view is folded: the stored transcript and the UI keep every original message.\\n\\nPHILOSOPHY\\n- Fold proactively when the context kernel nudge reports the request is near the auto-compress threshold, or when older turns are no longer needed verbatim.\\n- A summary is a hand-off, not a transcript: write what the next turns must know, in the language of the conversation.\\n- Never invent or guess: if a detail matters and you are not sure, keep that message outside the range and search it later.\\n- Prefer several small, precise folds over one giant summary; a block that is larger than what it replaces is useless.\\n\\nHOW\\n1. Call `context-kernel-acp_status`: it returns `refs` (`ref`, `role`, `tokens`, `pinned`, `tailProtected`, `compressed`), the active `blocks`, and `highestTier`.\\n2. Pick a contiguous range of OLD messages. Skip anything `pinned`, `tailProtected`, or `compressed`.\\n3. Write a self-contained `summary`: the user's actual request and constraints, decisions and their reasons, exact file paths and symbols, commands and their output, errors, verification results, and what is still open.\\n4. Call this tool with `from`, `to`, `summary` and an optional `title`.\\n\\nLAYERS\\n- Folding plain messages creates a tier-1 block.\\n- Folding a range that FULLY CONTAINS existing blocks merges them: their summaries become children of a higher-tier block (tier 2, then tier 3 max). Use this when the window is still tight after the first folds.\\n- A range that only PARTIALLY overlaps an existing block is rejected: extend the range to swallow the whole block, or decompress it first.\\n- `context-kernel-decompress` on a merged block reactivates its children at their original tier.\\n\\nALWAYS PROTECTED (the tool rejects these ranges with a reason and the offending ref)\\n- the pinned first user message;\\n- the most recent turns (protected tail);\\n- messages carrying media (`@@image:` / inline images) — folding would lose the image reference;\\n- context-kernel tool messages themselves — you must keep seeing what you already folded;\\n- any range that splits an assistant tool call from its tool result.\\n\\nRefs are stable across turns (content hash), so a range stays valid after retries; unknown refs are rejected with the current list from `context-kernel-acp_status`.".to_string(),
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
                description: "Restore a compressed block: the original messages become visible to the model again and the block is removed from the outbound view. If the block had merged child blocks (tier 2/3), those children are reactivated at their original tier instead of being dropped. Use it when a folded detail is needed verbatim, then re-compress a tighter range if the window is still tight.".to_string(),
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
                name: "absorb".to_string(),
                description: "Absorb a span of FINISHED work: same mechanics as `compress`, but the block is marked as absorbed and the summary must state completion (what is done, verified, produced, and where), so later turns can trust it instead of re-verifying. Use it for completed sub-tasks, resolved incidents, finished refactors, or long tool sequences whose outcome is already known. The pair of tool messages behind an absorbed call is hidden from the outbound view once the block exists.\\n\\nRules are identical to `context-kernel-compress` (turn-aligned range, minimum 2 messages, the same permanent protections, layered merging into tier 2/3). Prefer absorb over compress whenever the requirement is 'this is done' rather than 'this is history'.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "First message ref of the range (from context-kernel-acp_status)."
                        },
                        "to": {
                            "type": "string",
                            "description": "Last message ref of the range (inclusive)."
                        },
                        "title": {
                            "type": "string",
                            "description": "Short label, e.g. \"schema 70 migration done\"."
                        },
                        "summary": {
                            "type": "string",
                            "description": "Completion-state summary: what was done, how it was verified, where the artifacts are, and what remains explicitly out of scope."
                        }
                    },
                    "required": ["from", "to", "summary"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "acp_rule".to_string(),
                description: "Persistent folding rules for this conversation: `list` returns them, `add` remembers one, `remove` deletes one. Active rules are injected into every compression nudge and must be honoured in the summaries the model writes (e.g. \"always keep exact file paths\", \"summaries in Chinese\", \"never fold prompts the user repeated\").".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove"],
                            "description": "Which operation to run."
                        },
                        "rule": {
                            "type": "string",
                            "description": "For add: the rule text (max 500 chars). For remove: the exact text to delete."
                        },
                        "index": {
                            "type": "integer",
                            "description": "For remove: the 0-based index from action=list (alternative to `rule`)."
                        }
                    },
                    "required": ["action"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "acp_cache".to_string(),
                description: "Fold economics report: raw vs outbound tokens, tokens saved by folding (per block and total), compression ratio, tier breakdown, CCR stored entries, latest observed usage and the current threshold. Read-only; use it to decide whether another fold round is worth it (or whether to raise a block's tier) instead of guessing.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "includeBlocks": {
                            "type": "boolean",
                            "description": "Include the per-block breakdown (default true)."
                        }
                    }
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "recap".to_string(),
                description: "Compression recap: every active block with its span (`mNNNNN–mNNNNN`), tier, tokens saved and the head of its summary. Pass `blockId` to recap a single block. Read-only; use it in handoffs and long-running goals to restate what has already been folded without decompressing anything.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "blockId": {
                            "type": "string",
                            "description": "Optional: recap only this block, e.g. \"b3\"."
                        }
                    }
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "handoff".to_string(),
                description: "Render a readable session handoff document (title, session id, context tokens, block counts, the conversation as the model sees it). `full: true` (default) re-attaches the original messages of every folded range. Intended for Goal / WorkTree mode handoffs and for auditing what the outbound view contains. Read-only.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Optional handoff title."
                        },
                        "full": {
                            "type": "boolean",
                            "description": "Include the original messages of folded ranges (default true)."
                        }
                    }
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "acp_status".to_string(),
                description: "Report the context kernel state: estimated outbound tokens (real tokenizer), latest observed upstream usage, threshold, whether compression is needed, active blocks with their tier/children, `highestTier`/`maxTier`, the protected rules in force, and the stable message refs (newest `refLimit` entries) with role, token estimate, preview, pinned/tailProtected/compressed flags. Call it before compress to choose a range.".to_string(),
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
                "Tool \"{tool_name}\" of server \"{SERVER_ID}\" requires the async call_mcp_tool path (conversation context). Available tools: [context-kernel-compress, context-kernel-absorb, context-kernel-decompress, context-kernel-retrieve, context-kernel-search_context, context-kernel-acp_status, context-kernel-acp_cache, context-kernel-acp_rule, context-kernel-recap, context-kernel-handoff]"
            ),
        ))
    }
}
