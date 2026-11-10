//! 上下文内核（billion-context `acp-kernel` 的 Snow 适配实现）。
//!
//! 目标：在**出站视图**上做增量、可检索、对前缀缓存友好的折叠，而不是每次
//! 超限就整段生成一份 handoff。完整会话历史仍原样保存在 `chat_messages` 里，
//! 界面展示不受影响 —— 只有发给模型的消息列表被裁剪。
//!
//! 设计要点（与 `acp-kernel` 的契约对齐）：
//! - 消息身份用「角色 + 内容」的内容哈希（`m<hex>`），与数据库 id / 行号解耦，
//!   因此新增、重试、回滚截断都不会让已有块错位；找不到被覆盖消息的块自动失效。
//! - 压缩块（block）保存模型写的摘要 + 被覆盖消息 id 列表；出站视图在块的第一个
//!   消息位置插入一条**字节稳定**的摘要消息，其余被覆盖消息被剪掉。
//! - 首条用户消息永久 pin，最近若干条消息受保护，模型不能压缩它们。
//! - 状态按 `conversation_id + sub_agent_id` 隔离，子代理压缩不会写回父会话。
//!
//! 上游来源：https://github.com/ranxianglei/billion-context（`kernel/`，MIT）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use napi::bindgen_prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::storage::database;
use crate::storage::services::chat_conversations::{
    load_context_messages, ChatContextMessage, ORIGIN_INTERNAL,
};

/// 状态结构版本：结构变化时旧状态会被丢弃，而不是解析出错。
pub const KERNEL_VERSION: &str = "snow-context-kernel/1";

/// 摘要消息首行（界面层如需过滤内部消息，可按此前缀识别）。
const SUMMARY_PREFIX: &str = "## Compressed Context";

/// nudge 消息首行。
pub const NUDGE_PREFIX: &str = "## Context Kernel";

/// 最近 N 条消息受保护，任何块都不能覆盖它们。
const TAIL_PROTECTED_MESSAGES: usize = 6;

/// 单个块最少覆盖的消息数（避免把单条消息压缩成一条摘要，纯亏）。
const MIN_COMPRESS_MESSAGES: usize = 2;

/// 单会话最多块数，防止状态无限增长。
const MAX_BLOCKS: usize = 64;

/// 状态检索默认返回条数。
const DEFAULT_SEARCH_LIMIT: usize = 8;

/// 预览长度（状态 / 检索结果里的单行摘要）。
const PREVIEW_CHARS: usize = 160;

/// CCR（context compression retrieval）：超过该字符数的工具结果在出站视图里
/// 替换为占位符，原文进入 content store，由 `context-kernel-retrieve` 取回。
const CCR_MIN_CHARS: usize = 4000;
const CCR_PREVIEW_CHARS: usize = 240;
const CCR_STORE_MAX_CHARS: usize = 400_000;
const CCR_MARKER: &str = "[context-kernel:stored";

/// 分层折叠上限（tier 1 → 2 → 3）。
const MAX_TIER: u8 = 3;

/// 单条消息的固定开销（角色/分隔符等），与上游 thinking 权重口径对齐。
const MESSAGE_OVERHEAD_TOKENS: i64 = 4;

/// 上下文内核自身工具的消息永不折叠：模型必须能看到自己压过什么。
const KERNEL_TOOL_PREFIX: &str = "context-kernel-";

/// 含媒体载荷的消息永久受保护（折叠会丢失图片引用）。
const MEDIA_MARKERS: &[&str] = &["@@image:", "data:image/", "\"type\":\"image\"", "\"type\": \"image\""];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KernelBlock {
    /// `b1`, `b2`, ...（顶层块；被合并的子块已从 `blocks` 移除）
    pub id: String,
    pub title: String,
    /// 模型写的自包含摘要（原文语言保持不变）。
    pub summary: String,
    /// 被覆盖消息的内容哈希 id，按历史顺序。
    pub message_ids: Vec<String>,
    /// 1 = 原文摘要；2 = 由 tier1 块蒸馏；3 = 由 tier2 块再凝练（上限）。
    pub tier: u8,
    /// 被本块合并的子块 id（decompress 时按原层级还原）。
    pub children: Vec<String>,
    /// 非空表示已被父块合并（父块删除后子块重新生效）。
    pub folded_into: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KernelState {
    pub version: String,
    pub blocks: Vec<KernelBlock>,
    pub next_block_id: u64,
    pub updated_at: String,
    /// CCR content store：`cr<hash>` → 原始工具结果。占位符一旦生成即冻结，
    /// 后续轮次重新计算出同样的占位字节，前缀缓存不受影响。
    pub store: BTreeMap<String, StoredEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoredEntry {
    pub kind: String,
    pub chars: usize,
    pub text: String,
    pub created_at: String,
}

impl KernelState {
    fn fresh() -> Self {
        Self {
            version: KERNEL_VERSION.to_string(),
            blocks: Vec::new(),
            next_block_id: 1,
            updated_at: String::new(),
            store: BTreeMap::new(),
        }
    }

    fn stored_chars(&self) -> usize {
        self.store.values().map(|entry| entry.chars).sum()
    }
}

/// 一条消息的稳定引用，供工具输出与模型选择压缩范围。
#[derive(Clone, Debug)]
pub struct MessageRef {
    pub id: String,
    pub index: usize,
    pub role: String,
    pub preview: String,
    pub tokens: i64,
}

impl MessageRef {
    fn to_json(&self, covered_by: Option<&str>) -> Value {
        json!({
            "ref": self.id,
            "role": self.role,
            "tokens": self.tokens,
            "preview": self.preview,
            "compressed": covered_by.is_some(),
            "blockId": covered_by.unwrap_or(""),
        })
    }
}

/// 一轮请求的出站历史 + 稳定引用表。
struct HistoryView {
    messages: Vec<ChatContextMessage>,
    refs: Vec<MessageRef>,
    positions: HashMap<String, usize>,
}

impl HistoryView {
    fn build(messages: Vec<ChatContextMessage>) -> Self {
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        let mut refs = Vec::with_capacity(messages.len());
        let mut positions = HashMap::new();
        for (index, message) in messages.iter().enumerate() {
            let id = message_id(message, &mut occurrences);
            refs.push(MessageRef {
                id: id.clone(),
                index,
                role: message.role.trim().to_string(),
                preview: preview_of(&message.content, PREVIEW_CHARS),
                tokens: estimate_message_tokens(message),
            });
            positions.insert(id, index);
        }
        Self {
            messages,
            refs,
            positions,
        }
    }

    fn total_tokens(&self) -> i64 {
        self.refs.iter().map(|item| item.tokens).sum()
    }

    /// 首条非 system 消息：永久 pin，不允许被压缩覆盖。
    fn pinned_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .position(|message| !matches!(message.role.trim(), "system" | "developer"))
    }

    fn tail_protected_from(&self) -> usize {
        self.messages.len().saturating_sub(TAIL_PROTECTED_MESSAGES)
    }
}

/// 内容哈希：同样的角色 + 正文 + 工具负载 → 同样的 id，跨轮次稳定。
fn message_id(message: &ChatContextMessage, occurrences: &mut HashMap<String, usize>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(message.role.trim().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(message.content.as_bytes());
    if let Some(raw) = message.tool_calls_json.as_deref() {
        hasher.update(b"\x1f");
        hasher.update(raw.as_bytes());
    }
    if let Some(raw) = message.tool_results_json.as_deref() {
        hasher.update(b"\x1f");
        hasher.update(raw.as_bytes());
    }
    if let Some(raw) = message.thinking.as_deref() {
        hasher.update(b"\x1f");
        hasher.update(raw.as_bytes());
    }
    let digest = hasher.finalize().to_hex().to_string();
    let base = format!("m{}", &digest[..16]);
    let slot = occurrences.entry(base.clone()).or_insert(0);
    *slot += 1;
    if *slot == 1 {
        base
    } else {
        format!("{base}-{}", *slot)
    }
}

fn preview_of(content: &str, limit: usize) -> String {
    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let mut preview: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn estimate_message_tokens(message: &ChatContextMessage) -> i64 {
    // 用真实分词器（o200k_base）而不是 char/4：nudge 阈值必须贴近上游用量，
    // 否则会在远低于阈值时频繁压缩。工具结果与 content 重复时只算一次。
    let mut tokens = crate::api::token_counter::count_tokens(&message.content) as i64;
    if let Some(raw) = message.tool_calls_json.as_deref() {
        tokens += crate::api::token_counter::count_tokens(raw) as i64;
    }
    if message.content.trim().is_empty() {
        if let Some(raw) = message.tool_results_json.as_deref() {
            tokens += crate::api::token_counter::count_tokens(raw) as i64;
        }
    }
    if let Some(thinking) = message.thinking.as_deref() {
        tokens += crate::api::token_counter::count_tokens(thinking) as i64;
    }
    tokens + MESSAGE_OVERHEAD_TOKENS
}

/// 工具调用 id → (assistant 消息下标, tool 结果消息下标)。
/// 折叠块不能把一对调用/结果拆开，否则 provider 会因孤立 tool-call 直接 400。
fn tool_pair_index(messages: &[ChatContextMessage]) -> HashMap<String, (Option<usize>, Option<usize>)> {
    fn entry_ids(raw: Option<&str>, keys: &[&str]) -> Vec<String> {
        let Some(raw) = raw else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Vec::new();
        };
        value
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        keys.iter().find_map(|key| {
                            entry
                                .get(*key)
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(ToString::to_string)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    let mut pairs: HashMap<String, (Option<usize>, Option<usize>)> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role.trim() {
            "assistant" => {
                for id in entry_ids(message.tool_calls_json.as_deref(), &["id", "callId"]) {
                    pairs.entry(id).or_default().0 = Some(index);
                }
            }
            "tool" => {
                for id in entry_ids(message.tool_results_json.as_deref(), &["callId", "id"]) {
                    pairs.entry(id).or_default().1 = Some(index);
                }
            }
            _ => {}
        }
    }
    pairs
}

/// 返回该范围拆开的第一个工具对（用于拒绝非法块）。
fn split_tool_pair(
    pairs: &HashMap<String, (Option<usize>, Option<usize>)>,
    start: usize,
    end: usize,
) -> Option<String> {
    pairs.iter().find_map(|(id, (assistant, tool))| {
        let call_in = assistant.is_some_and(|index| (start..=end).contains(&index));
        let result_in = tool.is_some_and(|index| (start..=end).contains(&index));
        (call_in != result_in).then(|| id.clone())
    })
}

/// 消息里出现的工具名（assistant 的 tool_calls / tool 的 tool_results）。
fn tool_names_of(message: &ChatContextMessage) -> Vec<String> {
    let raw = match message.role.trim() {
        "assistant" => message.tool_calls_json.as_deref(),
        "tool" => message.tool_results_json.as_deref(),
        _ => None,
    };
    let Some(raw) = raw else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            entry
                                .get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                        })
                        .map(ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 含媒体载荷：折叠会丢掉图片引用，因此永久受保护。
fn has_media_payload(message: &ChatContextMessage) -> bool {
    let mut text = message.content.clone();
    if let Some(raw) = message.tool_results_json.as_deref() {
        text.push_str(raw);
    }
    MEDIA_MARKERS.iter().any(|marker| text.contains(marker))
}

/// 内核自身工具的消息：模型必须能看到自己压过什么，不能折叠。
fn is_kernel_tool_message(message: &ChatContextMessage) -> bool {
    if !matches!(message.role.trim(), "assistant" | "tool") {
        return false;
    }
    tool_names_of(message)
        .iter()
        .any(|name| name.starts_with(KERNEL_TOOL_PREFIX))
}

/// 折叠保护原因；`None` = 可折叠。system / 媒体 / 内核工具消息永久保护。
fn compression_blocked_reason(message: &ChatContextMessage) -> Option<&'static str> {
    if matches!(message.role.trim(), "system" | "developer") {
        return Some("system messages are never folded");
    }
    if has_media_payload(message) {
        return Some("the message carries media; folding would lose the image reference");
    }
    if is_kernel_tool_message(message) {
        return Some("context kernel tool messages are never folded");
    }
    None
}

fn summary_message(block: &KernelBlock, span: usize) -> ChatContextMessage {
    let layer = if block.tier > 1 && !block.children.is_empty() {
        format!(
            "\n> Tier {} distillation: this block already merged {} earlier block(s) ({}).",
            block.tier,
            block.children.len(),
            block.children.join(", ")
        )
    } else {
        String::new()
    };
    let content = format!(
        "{SUMMARY_PREFIX} ({id}) — {title}\n\n{summary}\n\n> Covered {span} earlier messages ({first} … {last}).\n> Restore them with `context-kernel-decompress` (blockId \"{id}\"), or search their originals with `context-kernel-search_context`.{layer}",
        id = block.id,
        title = block.title.trim(),
        summary = block.summary.trim(),
        span = span,
        first = block.message_ids.first().cloned().unwrap_or_default(),
        last = block.message_ids.last().cloned().unwrap_or_default(),
        layer = layer,
    );
    ChatContextMessage {
        role: "user".to_string(),
        origin: ORIGIN_INTERNAL.to_string(),
        content,
        tool_calls_json: None,
        tool_results_json: None,
        thinking: None,
        thinking_blocks_json: None,
    }
}

fn nudge_message(estimate: i64, threshold: i64, highest_tier: u8) -> ChatContextMessage {
    let tier_hint = if highest_tier >= MAX_TIER {
        "All active blocks are already tier 3 (the deepest layer); compress a wider span instead of re-folding them."
    } else if highest_tier >= 1 {
        "Blocks already exist: folding a range that fully contains them is allowed and produces a higher tier (2 → 3), which is the right move when the window is still tight."
    } else {
        "Pick a contiguous run of older messages and fold it into a tier-1 summary."
    };
    let content = format!(
        "{NUDGE_PREFIX}\n\nThe outbound conversation is about {estimate} tokens (auto-compress threshold: {threshold}). Fold older history now instead of letting the request overflow:\n\n1. Call `context-kernel-acp_status` to get message refs (`m...`) and the active blocks.\n2. Call `context-kernel-compress` with `from` / `to` refs and a self-contained `summary` that preserves decisions, file paths, failures, and open work. Write the summary in the conversation's language.\n3. {tier_hint}\n4. Protected and rejected automatically: the pinned first user message, the most recent turns, media messages, context-kernel tool messages, and any range that splits a tool call/result pair.\n5. Use `context-kernel-search_context` when a folded detail is needed again, `context-kernel-retrieve` for a stored tool output, and `context-kernel-decompress` to restore a block.\n\nOnly the request view is folded — the user still sees the full transcript."
    );
    ChatContextMessage {
        role: "user".to_string(),
        origin: ORIGIN_INTERNAL.to_string(),
        content,
        tool_calls_json: None,
        tool_results_json: None,
        thinking: None,
        thinking_blocks_json: None,
    }
}

// ---------------------------------------------------------------------------
// CCR: large tool-result placeholders + retrieval
// ---------------------------------------------------------------------------

fn ccr_ref(text: &str) -> String {
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    format!("cr{}", &digest[..16])
}

/// 冻结占位符：同一原文永远生成同一段字节，不需要每轮重写前缀。
fn ccr_placeholder(ref_id: &str, kind: &str, text: &str) -> String {
    let chars = text.chars().count();
    let preview: String = text.chars().take(CCR_PREVIEW_CHARS).collect();
    let ellipsis = if chars > CCR_PREVIEW_CHARS { "…" } else { "" };
    format!(
        "{CCR_MARKER} ref={ref_id} kind={kind} chars={chars}]\n{preview}{ellipsis}\n\n> Original tool output is stored out of band. Call `context-kernel-retrieve` with ref \"{ref_id}\" to read it in full."
    )
}

fn store_original(state: &mut KernelState, kind: &str, text: &str) -> String {
    let ref_id = ccr_ref(text);
    if state.store.contains_key(&ref_id) {
        return ref_id;
    }
    state.store.insert(
        ref_id.clone(),
        StoredEntry {
            kind: kind.to_string(),
            chars: text.chars().count(),
            text: text.to_string(),
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        },
    );
    // 有界存储：超出上限时按 ref 顺序淘汰最早插入的一批（ref 由内容决定，
    // 淘汰只影响 retrieve 可用性，不影响出站字节的稳定性）。
    while state.stored_chars() > CCR_STORE_MAX_CHARS {
        let Some(oldest) = state.store.keys().next().cloned() else {
            break;
        };
        state.store.remove(&oldest);
    }
    ref_id
}

/// 把超长工具结果替换为占位符。只改出站副本，落库内容不动。
fn apply_ccr(messages: &mut [ChatContextMessage], state: &mut KernelState) -> usize {
    let mut replaced = 0usize;
    for message in messages.iter_mut() {
        if message.role.trim() != "tool" {
            continue;
        }
        if !message.content.trim().is_empty()
            && !message.content.starts_with(CCR_MARKER)
            && message.content.chars().count() >= CCR_MIN_CHARS
        {
            let ref_id = store_original(state, "tool_text", &message.content);
            message.content = ccr_placeholder(&ref_id, "tool_text", &message.content);
            replaced += 1;
        }
        let Some(raw) = message.tool_results_json.clone() else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(entries) = value.as_array_mut() else {
            continue;
        };
        let mut changed = false;
        for entry in entries.iter_mut() {
            let Some(result) = entry
                .get("result")
                .and_then(Value::as_str)
                .map(ToString::to_string)
            else {
                continue;
            };
            if result.starts_with(CCR_MARKER) || result.chars().count() < CCR_MIN_CHARS {
                continue;
            }
            let ref_id = store_original(state, "tool_result", &result);
            entry["result"] = Value::String(ccr_placeholder(&ref_id, "tool_result", &result));
            changed = true;
            replaced += 1;
        }
        if changed {
            message.tool_results_json = Some(value.to_string());
        }
    }
    replaced
}

/// `context-kernel-retrieve`: 按 ref 取回被占位符替换的原始工具结果。
fn execute_retrieve(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let ref_id = identifier_arg(args, "ref")?;
    let state = load_state(database_path, conversation_id, sub_agent_id)?;
    let Some(entry) = state.store.get(&ref_id) else {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Unknown or evicted ref: {ref_id}. Placeholders always carry their ref; call context-kernel-acp_status for the current stored-entry count."
            ),
        ));
    };
    Ok(json!({
        "success": true,
        "ref": ref_id,
        "kind": entry.kind,
        "chars": entry.chars,
        "text": entry.text,
    }))
}

/// 最近一次 assistant 响应记录的真实用量（input + output + cache）。
/// nudge 优先使用上游真实 usage，估算器只作补充。
pub fn latest_usage_tokens(database_path: &Path, conversation_id: &str) -> Option<i64> {
    if conversation_id.trim().is_empty() {
        return None;
    }
    let connection = database::open_connection(database_path).ok()?;
    // 取「最近一条带用量的 assistant 行」：provider 会在纯 tool-call 行上省略
    // usage，只看最后一行会把真实用量判成 0，nudge 于是退回估算。
    let total: i64 = connection
        .query_row(
            "SELECT COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)
                    + COALESCE(cache_creation_input_tokens, 0) + COALESCE(cache_read_input_tokens, 0)
                    AS used
               FROM chat_messages
              WHERE conversation_id = ?1
                AND role = 'assistant'
                AND (COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)
                     + COALESCE(cache_creation_input_tokens, 0) + COALESCE(cache_read_input_tokens, 0)) > 0
              ORDER BY rowid DESC
              LIMIT 1",
            rusqlite::params![conversation_id],
            |row| row.get(0),
        )
        .ok()?;
    (total > 0).then_some(total)
}

// ---------------------------------------------------------------------------
// State persistence
// ---------------------------------------------------------------------------

pub fn load_state(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
) -> Result<KernelState> {
    if conversation_id.trim().is_empty() {
        return Ok(KernelState::fresh());
    }
    let connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "open context kernel state", error))?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT state_json
               FROM context_kernel_state
              WHERE conversation_id = ?1 AND sub_agent_id = ?2",
            rusqlite::params![conversation_id, sub_agent_id],
            |row| row.get(0),
        )
        .ok();
    let Some(raw) = raw else {
        return Ok(KernelState::fresh());
    };
    match serde_json::from_str::<KernelState>(&raw) {
        Ok(mut state) if state.version == KERNEL_VERSION => {
            if state.next_block_id == 0 {
                state.next_block_id =
                    state.blocks.len() as u64 + 1;
            }
            Ok(state)
        }
        // 旧版本 / 损坏状态：丢弃即可，历史本身还在数据库里。
        _ => Ok(KernelState::fresh()),
    }
}

fn save_state(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    state: &KernelState,
) -> Result<()> {
    let connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "open context kernel state", error))?;
    let state_json = serde_json::to_string(state)
        .map_err(|error| Error::from_reason(format!("serialize context kernel state: {error}")))?;
    connection
        .execute(
            "INSERT INTO context_kernel_state
                (conversation_id, sub_agent_id, kernel_version, state_json, store_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, '{}', datetime('now', 'localtime'))
             ON CONFLICT(conversation_id, sub_agent_id) DO UPDATE SET
                kernel_version = excluded.kernel_version,
                state_json = excluded.state_json,
                updated_at = datetime('now', 'localtime')",
            rusqlite::params![conversation_id, sub_agent_id, KERNEL_VERSION, state_json],
        )
        .map_err(|error| database::database_error(database_path, "save context kernel state", error))?;
    Ok(())
}

/// 删除会话 / 子代理运行后清理状态；找不到行时静默成功。
pub fn clear_state(database_path: &Path, conversation_id: &str, sub_agent_id: &str) -> Result<()> {
    if conversation_id.trim().is_empty() {
        return Ok(());
    }
    let connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "open context kernel state", error))?;
    connection
        .execute(
            "DELETE FROM context_kernel_state
              WHERE conversation_id = ?1 AND sub_agent_id = ?2",
            rusqlite::params![conversation_id, sub_agent_id],
        )
        .map_err(|error| database::database_error(database_path, "clear context kernel state", error))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

fn block_ranges(
    state: &KernelState,
    positions: &HashMap<String, usize>,
    pairs: &HashMap<String, (Option<usize>, Option<usize>)>,
    blocked: &[bool],
) -> Vec<(usize, usize, KernelBlock)> {
    let mut ranges: Vec<(usize, usize, KernelBlock)> = Vec::new();
    for block in state.blocks.iter() {
        // 已被父块合并的子块不再单独出站：父块的摘要覆盖了同样的消息。
        if !block.folded_into.is_empty() {
            continue;
        }
        let mut indices: Vec<usize> = Vec::with_capacity(block.message_ids.len());
        let mut valid = true;
        for id in block.message_ids.iter() {
            match positions.get(id) {
                Some(index) => indices.push(*index),
                None => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid || indices.len() < MIN_COMPRESS_MESSAGES {
            continue;
        }
        indices.sort_unstable();
        let start = indices[0];
        let end = *indices.last().unwrap_or(&start);
        // 旧状态或历史变动可能让块拆开工具对：这种块必须整体忽略，否则出站请求
        // 会出现孤立 tool-call，provider 直接 400。
        if split_tool_pair(pairs, start, end).is_some() {
            continue;
        }
        // 防御：历史里出现受保护消息（媒体 / 内核工具消息）时，覆盖它的旧块作废。
        if (start..=end).any(|index| blocked.get(index).copied().unwrap_or(false)) {
            continue;
        }
        ranges.push((start, end, block.clone()));
    }
    ranges.sort_by_key(|(start, end, _)| (*start, *end));
    ranges
}

/// 丢弃引用了不存在消息的块（回滚截断 / fork / 历史被压缩边界重置后）。
fn prune_stale_blocks(state: &mut KernelState, positions: &HashMap<String, usize>) -> bool {
    let before = state.blocks.len();
    state.blocks.retain(|block| {
        block.message_ids.len() >= MIN_COMPRESS_MESSAGES
            && block
                .message_ids
                .iter()
                .all(|id| positions.contains_key(id))
    });
    state.blocks.len() != before
}

/// 把被块覆盖的消息替换成摘要消息；返回折叠后视图的 token 估算。
fn fold(messages: &[ChatContextMessage], state: &KernelState) -> (Vec<ChatContextMessage>, i64) {
    let view = HistoryView::build(messages.to_vec());
    let pairs = tool_pair_index(&view.messages);
    let blocked: Vec<bool> = view
        .messages
        .iter()
        .map(|message| compression_blocked_reason(message).is_some())
        .collect();
    let ranges = block_ranges(state, &view.positions, &pairs, &blocked);
    if ranges.is_empty() {
        return (messages.to_vec(), view.total_tokens());
    }

    let mut by_start: HashMap<usize, Vec<(usize, KernelBlock)>> = HashMap::new();
    for (start, end, block) in ranges {
        by_start.entry(start).or_default().push((end, block));
    }

    let mut folded: Vec<ChatContextMessage> = Vec::with_capacity(messages.len());
    let mut index = 0usize;
    while index < messages.len() {
        if let Some(entries) = by_start.get(&index) {
            let end = entries.iter().map(|(end, _)| *end).max().unwrap_or(index);
            for (_, block) in entries {
                folded.push(summary_message(block, block.message_ids.len()));
            }
            index = end + 1;
            continue;
        }
        folded.push(messages[index].clone());
        index += 1;
    }
    let tokens: i64 = folded.iter().map(estimate_message_tokens).sum();
    (folded, tokens)
}

/// 出站视图入口：验证块、折叠历史、必要时追加 nudge。
///
/// 失败时直接返回原历史（上下文管理不能阻断请求）；调用方采用返回值。
pub fn apply_outgoing_view(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    threshold_tokens: Option<i64>,
    observed_tokens: Option<i64>,
    messages: &mut Vec<ChatContextMessage>,
) -> Result<i64> {
    if conversation_id.trim().is_empty() {
        return Ok(0);
    }
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let positions = HistoryView::build(messages.clone()).positions;
    let mut changed = prune_stale_blocks(&mut state, &positions);

    let (mut folded, mut tokens) = fold(messages, &state);
    // CCR：超长工具结果在出站视图里换成冻结占位符，原文进入 content store。
    if apply_ccr(&mut folded, &mut state) > 0 {
        changed = true;
        // 占位符会显著缩小出站体积，nudge 判断必须用压缩后的估算。
        tokens = folded.iter().map(estimate_message_tokens).sum();
    }
    let threshold = threshold_tokens.filter(|value| *value > 0);
    // 子代理没有内核工具（不在其工具白名单内），nudge 只会变成噪声；只折叠可用块。
    if let Some(threshold) = threshold {
        // 真实 usage 优先：估算器只补足上游没给 usage 的情况。
        let effective = tokens.max(observed_tokens.unwrap_or(0));
        if effective >= threshold && sub_agent_id.is_empty() {
            let highest_tier = state
                .blocks
                .iter()
                .filter(|block| block.folded_into.is_empty())
                .map(|block| block.tier)
                .max()
                .unwrap_or(0);
            folded.push(nudge_message(effective, threshold, highest_tier));
        }
    }
    *messages = folded;
    if changed {
        let _ = save_state(database_path, conversation_id, sub_agent_id, &state);
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// 工具执行侧读取历史：与 `prepare_context_request` 的关键步骤保持一致
/// （工具配对修复 + 历史精简），否则模型看到的 ref 在工具侧会偏移。
fn load_history(database_path: &Path, conversation_id: &str) -> Result<Vec<ChatContextMessage>> {
    let mut messages = load_context_messages(database_path, conversation_id)?;
    super::tool_messages::ensure_tool_pairing(&mut messages);
    super::history_trim::trim_history_for_request(&mut messages, database_path);
    Ok(messages)
}

fn identifier_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                format!("Missing required parameter: {key} (a message ref like \"m1a2b3c4\")"),
            )
        })
}

fn covered_lookup(state: &KernelState) -> HashMap<String, String> {
    let mut lookup = HashMap::new();
    // 只认顶层块：子块的 id 集合已被父块覆盖，避免映射到已合并的层。
    for block in state.blocks.iter().filter(|block| block.folded_into.is_empty()) {
        for id in block.message_ids.iter() {
            lookup.insert(id.clone(), block.id.clone());
        }
    }
    lookup
}

/// `context-kernel-compress`: 用模型写的摘要覆盖一段连续历史。
fn execute_compress(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let from = identifier_arg(args, "from")?;
    let to = identifier_arg(args, "to")?;
    let summary = args
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Missing required parameter: summary (self-contained; keep decisions, paths, errors, open work)",
            )
        })?
        .to_string();
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("compressed history")
        .to_string();

    let messages = load_history(database_path, conversation_id)?;
    let view = HistoryView::build(messages);
    let from_index = view.positions.get(&from).copied().ok_or_else(|| {
        Error::new(
            Status::InvalidArg,
            format!("Unknown message ref: {from}. Call context-kernel-acp_status for current refs."),
        )
    })?;
    let to_index = view.positions.get(&to).copied().ok_or_else(|| {
        Error::new(
            Status::InvalidArg,
            format!("Unknown message ref: {to}. Call context-kernel-acp_status for current refs."),
        )
    })?;
    let (start, end) = if from_index <= to_index {
        (from_index, to_index)
    } else {
        (to_index, from_index)
    };
    let span = end - start + 1;
    if span < MIN_COMPRESS_MESSAGES {
        return Err(Error::new(
            Status::InvalidArg,
            format!("A block must cover at least {MIN_COMPRESS_MESSAGES} messages; got {span}."),
        ));
    }
    if let Some(pinned) = view.pinned_index() {
        if start <= pinned && pinned <= end {
            return Err(Error::new(
                Status::InvalidArg,
                "The first user message is pinned and cannot be compressed. Start after it.",
            ));
        }
    }
    let tail_from = view.tail_protected_from();
    if end >= tail_from {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "The most recent {} messages are protected (refs after \"{}\" cannot be compressed). Compress older history.",
                TAIL_PROTECTED_MESSAGES,
                view.refs
                    .get(tail_from.saturating_sub(1))
                    .map(|item| item.id.as_str())
                    .unwrap_or("")
            ),
        ));
    }
    // 永久受保护的消息：system、含媒体的消息、内核自身工具消息。
    for index in start..=end {
        if let Some(reason) = compression_blocked_reason(&view.messages[index]) {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "Ref \"{}\" cannot be folded: {reason}. Choose a range that excludes it.",
                    view.refs[index].id
                ),
            ));
        }
    }
    let pairs = tool_pair_index(&view.messages);
    if let Some(call_id) = split_tool_pair(&pairs, start, end) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "The range splits tool call/result pair \"{call_id}\". Extend `from`/`to` so both the assistant call and its tool result are inside the block."
            ),
        ));
    }

    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    prune_stale_blocks(&mut state, &view.positions);
    let active_top = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .count();
    if active_top >= MAX_BLOCKS {
        return Err(Error::new(
            Status::InvalidArg,
            format!("At most {MAX_BLOCKS} blocks per conversation. Decompress or merge existing blocks first."),
        ));
    }
    let target_ids: Vec<String> = view.refs[start..=end]
        .iter()
        .map(|item| item.id.clone())
        .collect();

    // 与范围重叠的顶层块：完整包含 → 合并为子块（tier 升级）；部分相交 → 拒绝。
    let mut children: Vec<KernelBlock> = Vec::new();
    for block in state.blocks.iter() {
        if !block.folded_into.is_empty() {
            continue;
        }
        let mut indices: Vec<usize> = block
            .message_ids
            .iter()
            .filter_map(|id| view.positions.get(id).copied())
            .collect();
        if indices.is_empty() {
            continue;
        }
        indices.sort_unstable();
        let (block_start, block_end) = (indices[0], indices[indices.len() - 1]);
        if block_end < start || block_start > end {
            continue;
        }
        if block_start < start || block_end > end {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "The range partially overlaps block {}. Extend `from`/`to` so the block is either fully inside or fully outside it, or decompress it first.",
                    block.id
                ),
            ));
        }
        children.push(block.clone());
    }
    let tier = children
        .iter()
        .map(|block| block.tier)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .min(MAX_TIER);

    let id = format!("b{}", state.next_block_id.max(1));
    state.next_block_id = state.next_block_id.max(1) + 1;
    let tokens_before: i64 = view.refs[start..=end].iter().map(|item| item.tokens).sum();
    let child_ids: Vec<String> = children.iter().map(|block| block.id.clone()).collect();
    for child_id in child_ids.iter() {
        if let Some(entry) = state.blocks.iter_mut().find(|block| &block.id == child_id) {
            entry.folded_into = id.clone();
        }
    }
    let block = KernelBlock {
        id: id.clone(),
        title,
        summary,
        message_ids: target_ids.clone(),
        tier,
        children: children.iter().map(|child| child.id.clone()).collect(),
        folded_into: String::new(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    state.blocks.push(block.clone());
    state.updated_at = block.created_at.clone();
    save_state(database_path, conversation_id, sub_agent_id, &state)?;

    let summary_tokens = estimate_message_tokens(&summary_message(&block, target_ids.len()));
    Ok(json!({
        "success": true,
        "blockId": id,
        "tier": tier,
        "mergedBlocks": child_ids,
        "coveredMessages": target_ids.len(),
        "tokensBefore": tokens_before,
        "tokensAfter": summary_tokens,
        "tokensSaved": (tokens_before - summary_tokens).max(0),
        "messageIds": target_ids,
        "note": "Only the outbound view is folded; the stored transcript and the UI are unchanged.",
    }))
}

/// `context-kernel-decompress`: 取消一个块，让原始消息重新可见。
fn execute_decompress(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let block_id = identifier_arg(args, "blockId")?;
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let before = state.blocks.len();
    let mut removed: Option<KernelBlock> = None;
    state.blocks.retain(|block| {
        if block.id == block_id {
            removed = Some(block.clone());
            false
        } else {
            true
        }
    });
    let Some(block) = removed else {
        return Err(Error::new(
            Status::InvalidArg,
            format!("Unknown block id: {block_id}. Call context-kernel-acp_status for active blocks."),
        ));
    };
    // 合并块被还原时，其子块重新生效（保留原层级），而不是一并删除。
    let mut restored_children: Vec<String> = Vec::new();
    for child_id in block.children.iter() {
        if let Some(entry) = state
            .blocks
            .iter_mut()
            .find(|candidate| &candidate.id == child_id)
        {
            if entry.folded_into == block_id {
                entry.folded_into.clear();
                restored_children.push(child_id.clone());
            }
        }
    }
    state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    save_state(database_path, conversation_id, sub_agent_id, &state)?;
    let active_top = state
        .blocks
        .iter()
        .filter(|candidate| candidate.folded_into.is_empty())
        .count();
    Ok(json!({
        "success": true,
        "blockId": block_id,
        "tier": block.tier,
        "restoredMessages": block.message_ids.len(),
        "messageIds": block.message_ids,
        "restoredChildBlocks": restored_children,
        "activeBlocks": active_top,
        "removedBlocks": before.saturating_sub(state.blocks.len()),
    }))
}

/// `context-kernel-search_context`: 在完整历史与块摘要里检索。
fn execute_search(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Missing required parameter: query (keywords to look up in the full transcript)",
            )
        })?
        .to_lowercase();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, 30) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT);

    let messages = load_history(database_path, conversation_id)?;
    let view = HistoryView::build(messages);
    let state = load_state(database_path, conversation_id, sub_agent_id)?;
    let covered = covered_lookup(&state);
    let terms: Vec<&str> = query.split_whitespace().filter(|term| !term.is_empty()).collect();

    let score = |haystack: &str| -> i64 {
        let lowered = haystack.to_lowercase();
        terms
            .iter()
            .map(|term| lowered.matches(term).count() as i64)
            .sum()
    };

    let mut hits: Vec<(i64, Value)> = Vec::new();
    for block in state.blocks.iter() {
        let text = format!("{} {}", block.title, block.summary);
        let score = score(&text);
        if score > 0 {
            hits.push((
                score + 2,
                json!({
                    "kind": "block",
                    "blockId": block.id,
                    "title": block.title,
                    "summary": block.summary,
                    "coveredMessages": block.message_ids.len(),
                }),
            ));
        }
    }
    for item in view.refs.iter() {
        let message = &view.messages[item.index];
        let text = format!("{} {}", message.content, message.thinking.as_deref().unwrap_or(""));
        let score = score(&text);
        if score > 0 {
            hits.push((
                score,
                json!({
                    "kind": "message",
                    "ref": item.id,
                    "role": item.role,
                    "preview": preview_of(&message.content, 320),
                    "compressed": covered.contains_key(&item.id),
                    "blockId": covered.get(&item.id).cloned().unwrap_or_default(),
                }),
            ));
        }
    }
    hits.sort_by(|left, right| right.0.cmp(&left.0));
    let results: Vec<Value> = hits.into_iter().take(limit).map(|(_, value)| value).collect();
    Ok(json!({
        "success": true,
        "query": query,
        "count": results.len(),
        "results": results,
    }))
}

/// `context-kernel-acp_status`: 报告出站用量、阈值、块与可用 ref。
fn execute_status(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    threshold_tokens: Option<i64>,
    args: &Value,
) -> Result<Value> {
    let ref_limit = args
        .get("refLimit")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(4, 200) as usize)
        .unwrap_or(40);
    let messages = load_history(database_path, conversation_id)?;
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let view = HistoryView::build(messages);
    prune_stale_blocks(&mut state, &view.positions);
    let (folded, folded_tokens) = fold(&view.messages, &state);
    // 状态里的 token 估算同样要反映 CCR 占位符（但不写回 store）。
    let mut folded_preview = folded.clone();
    let mut state_preview = state.clone();
    let folded_tokens = if apply_ccr(&mut folded_preview, &mut state_preview) > 0 {
        folded_preview.iter().map(estimate_message_tokens).sum()
    } else {
        folded_tokens
    };
    let covered = covered_lookup(&state);
    let tail_from = view.tail_protected_from();
    let pinned = view.pinned_index();

    let refs_json: Vec<Value> = view
        .refs
        .iter()
        .rev()
        .take(ref_limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|item| {
            let mut value = item.to_json(covered.get(&item.id).map(String::as_str));
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "pinned".into(),
                    Value::Bool(pinned == Some(item.index)),
                );
                object.insert(
                    "tailProtected".into(),
                    Value::Bool(item.index >= tail_from),
                );
            }
            value
        })
        .collect();

    let blocks_json: Vec<Value> = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .map(|block| {
            json!({
                "blockId": block.id,
                "title": block.title,
                "tier": block.tier,
                "children": block.children,
                "coveredMessages": block.message_ids.len(),
                "summaryTokens": estimate_message_tokens(&summary_message(block, block.message_ids.len())),
                "createdAt": block.created_at,
            })
        })
        .collect();
    let highest_tier = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .map(|block| block.tier)
        .max()
        .unwrap_or(0);

    let threshold = threshold_tokens.filter(|value| *value > 0);
    let observed = latest_usage_tokens(database_path, conversation_id);
    Ok(json!({
        "success": true,
        "kernelVersion": KERNEL_VERSION,
        "conversationId": conversation_id,
        "subAgent": !sub_agent_id.is_empty(),
        "totalMessages": view.messages.len(),
        "outboundMessages": folded.len(),
        "estimatedTokens": folded_tokens,
        "observedTokens": observed,
        "rawTokens": view.total_tokens(),
        "thresholdTokens": threshold,
        "compressNeeded": threshold.map(|value| folded_tokens.max(observed.unwrap_or(0)) >= value).unwrap_or(false),
        "compressedMessages": covered.len(),
        "activeBlocks": blocks_json.len(),
        "highestTier": highest_tier,
        "maxTier": MAX_TIER,
        "storedEntries": state.store.len(),
        "storedChars": state.stored_chars(),
        "blocks": blocks_json,
        "refs": refs_json,
        "protected": {
            "pinnedRef": pinned.and_then(|index| view.refs.get(index)).map(|item| item.id.clone()).unwrap_or_default(),
            "tailMessages": TAIL_PROTECTED_MESSAGES,
            "mediaMarkers": MEDIA_MARKERS,
            "kernelTools": KERNEL_TOOL_PREFIX,
        },
    }))
}

/// 供内置工具服务调用的统一入口。
pub fn execute_tool(
    tool_name: &str,
    args: &Value,
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    threshold_tokens: Option<i64>,
) -> Result<Value> {
    match tool_name {
        "compress" => execute_compress(database_path, conversation_id, sub_agent_id, args),
        "decompress" => execute_decompress(database_path, conversation_id, sub_agent_id, args),
        "retrieve" => execute_retrieve(database_path, conversation_id, sub_agent_id, args),
        "search_context" => execute_search(database_path, conversation_id, sub_agent_id, args),
        "acp_status" => execute_status(
            database_path,
            conversation_id,
            sub_agent_id,
            threshold_tokens,
            args,
        ),
        other => Err(Error::new(
            Status::InvalidArg,
            format!("Unknown context-kernel tool: {other}"),
        )),
    }
}

/// 供 UI / 只读 IPC 使用的状态快照（阈值取当前生效 API 档案）。
pub fn status_snapshot(database_path: &Path, conversation_id: &str) -> Result<Value> {
    let threshold = crate::api::config::get_active_api_request_context()
        .ok()
        .and_then(|context| {
            resolve_threshold(
                context.api_config.auto_compress_threshold,
                context.api_config.max_context_tokens,
            )
        });
    execute_status(
        database_path,
        conversation_id,
        "",
        threshold,
        &json!({ "refLimit": 12 }),
    )
}

/// 子代理会话键：子代理请求使用自己的 conversation id，这里额外带上前缀，
/// 避免同一 conversation 的父/子视图共用状态。
pub fn sub_agent_key(is_sub_agent: bool, conversation_id: &str) -> String {
    if is_sub_agent {
        format!("sub:{conversation_id}")
    } else {
        String::new()
    }
}

/// 计算用于出站折叠的阈值：优先 `autoCompressThreshold`，其次上下文的 60%。
pub fn resolve_threshold(
    auto_compress_threshold: Option<i32>,
    max_context_tokens: Option<i32>,
) -> Option<i64> {
    if let Some(value) = auto_compress_threshold.filter(|value| *value > 0) {
        return Some(i64::from(value));
    }
    max_context_tokens
        .filter(|value| *value > 0)
        .map(|value| (i64::from(value) * 3) / 5)
}

/// 工具列表里需要剔除 / 保留的上下文内核工具全名。
pub fn is_context_kernel_tool(full_name: &str) -> bool {
    full_name.starts_with("context-kernel-")
}

/// 按功能开关保留或剔除内核工具；关闭时请求载荷与本特性引入前完全一致。
pub fn filter_tools(tools: Vec<crate::mcp::tools::McpTool>, enabled: bool) -> Vec<crate::mcp::tools::McpTool> {
    if enabled {
        return tools;
    }
    tools
        .into_iter()
        .filter(|tool| !is_context_kernel_tool(&tool.full_name()))
        .collect()
}

/// 检查块 id 集合是否与当前历史一致（供回滚 / fork 后一致性校验使用）。
pub fn active_block_ids(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
) -> Result<Vec<String>> {
    let state = load_state(database_path, conversation_id, sub_agent_id)?;
    Ok(state.blocks.into_iter().map(|block| block.id).collect())
}

/// 历史集合的去重 id 快照，回滚实现可据此裁剪状态。
pub fn message_ids_of(messages: &[ChatContextMessage]) -> HashSet<String> {
    let mut occurrences = HashMap::new();
    messages
        .iter()
        .map(|message| message_id(message, &mut occurrences))
        .collect()
}
