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

use std::collections::{HashMap, HashSet};
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KernelBlock {
    /// `b1`, `b2`, ...
    pub id: String,
    pub title: String,
    /// 模型写的自包含摘要（原文语言保持不变）。
    pub summary: String,
    /// 被覆盖消息的内容哈希 id，按历史顺序。
    pub message_ids: Vec<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KernelState {
    pub version: String,
    pub blocks: Vec<KernelBlock>,
    pub next_block_id: u64,
    pub updated_at: String,
}

impl KernelState {
    fn fresh() -> Self {
        Self {
            version: KERNEL_VERSION.to_string(),
            blocks: Vec::new(),
            next_block_id: 1,
            updated_at: String::new(),
        }
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
    let mut chars = message.content.chars().count();
    for raw in [
        message.tool_calls_json.as_deref(),
        message.tool_results_json.as_deref(),
        message.thinking.as_deref(),
        message.thinking_blocks_json.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        chars += raw.chars().count();
    }
    (chars as i64 / 4) + 4
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

fn summary_message(block: &KernelBlock, span: usize) -> ChatContextMessage {
    let content = format!(
        "{SUMMARY_PREFIX} ({id}) — {title}\n\n{summary}\n\n> Covered {span} earlier messages ({first} … {last}).\n> Restore them with `context-kernel-decompress` (blockId \"{id}\"), or search their originals with `context-kernel-search_context`.",
        id = block.id,
        title = block.title.trim(),
        summary = block.summary.trim(),
        span = span,
        first = block.message_ids.first().cloned().unwrap_or_default(),
        last = block.message_ids.last().cloned().unwrap_or_default(),
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

fn nudge_message(estimate: i64, threshold: i64) -> ChatContextMessage {
    let content = format!(
        "{NUDGE_PREFIX}\n\nThe outbound conversation is about {estimate} tokens (auto-compress threshold: {threshold}). Fold older history now instead of letting the request overflow:\n\n1. Call `context-kernel-acp_status` to get message refs (`m...`).\n2. Call `context-kernel-compress` with `from` / `to` refs and a self-contained `summary` that preserves decisions, file paths, failures, and open work.\n3. Never compress the pinned first user message or the most recent turns (protected tail); the tool rejects them.\n4. Use `context-kernel-search_context` when a folded detail is needed again, and `context-kernel-decompress` to restore a block.\n\nOnly the request view is folded — the user still sees the full transcript."
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
) -> Vec<(usize, usize, KernelBlock)> {
    let mut ranges: Vec<(usize, usize, KernelBlock)> = Vec::new();
    for block in state.blocks.iter() {
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
    let ranges = block_ranges(state, &view.positions, &pairs);
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
    messages: &mut Vec<ChatContextMessage>,
) -> Result<i64> {
    if conversation_id.trim().is_empty() {
        return Ok(0);
    }
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let positions = HistoryView::build(messages.clone()).positions;
    let changed = prune_stale_blocks(&mut state, &positions);

    let (mut folded, tokens) = fold(messages, &state);
    let threshold = threshold_tokens.filter(|value| *value > 0);
    // 子代理没有内核工具（不在其工具白名单内），nudge 只会变成噪声；只折叠可用块。
    if let Some(threshold) = threshold {
        if tokens >= threshold && sub_agent_id.is_empty() {
            folded.push(nudge_message(tokens, threshold));
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
    for block in state.blocks.iter() {
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
    for index in start..=end {
        let role = view.messages[index].role.trim();
        if matches!(role, "system" | "developer") {
            return Err(Error::new(
                Status::InvalidArg,
                "System messages cannot be compressed; choose a range of user/assistant/tool messages.",
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
    if state.blocks.len() >= MAX_BLOCKS {
        return Err(Error::new(
            Status::InvalidArg,
            format!("At most {MAX_BLOCKS} blocks per conversation. Decompress or merge existing blocks first."),
        ));
    }
    let target_ids: Vec<String> = view.refs[start..=end]
        .iter()
        .map(|item| item.id.clone())
        .collect();
    let covered = covered_lookup(&state);
    if let Some(existing) = target_ids
        .iter()
        .find_map(|id| covered.get(id).map(|block| (id, block)))
    {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Message {} is already covered by block {}. Decompress that block first.",
                existing.0, existing.1
            ),
        ));
    }

    let id = format!("b{}", state.next_block_id.max(1));
    state.next_block_id = state.next_block_id.max(1) + 1;
    let tokens_before: i64 = view.refs[start..=end].iter().map(|item| item.tokens).sum();
    let block = KernelBlock {
        id: id.clone(),
        title,
        summary,
        message_ids: target_ids.clone(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    state.blocks.push(block.clone());
    state.updated_at = block.created_at.clone();
    save_state(database_path, conversation_id, sub_agent_id, &state)?;

    let summary_tokens = estimate_message_tokens(&summary_message(&block, target_ids.len()));
    Ok(json!({
        "success": true,
        "blockId": id,
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
    state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    save_state(database_path, conversation_id, sub_agent_id, &state)?;
    Ok(json!({
        "success": true,
        "blockId": block_id,
        "restoredMessages": block.message_ids.len(),
        "messageIds": block.message_ids,
        "activeBlocks": state.blocks.len(),
        "removedBlocks": before - state.blocks.len(),
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
        .map(|block| {
            json!({
                "blockId": block.id,
                "title": block.title,
                "coveredMessages": block.message_ids.len(),
                "summaryTokens": estimate_message_tokens(&summary_message(block, block.message_ids.len())),
                "createdAt": block.created_at,
            })
        })
        .collect();

    let threshold = threshold_tokens.filter(|value| *value > 0);
    Ok(json!({
        "success": true,
        "kernelVersion": KERNEL_VERSION,
        "conversationId": conversation_id,
        "subAgent": !sub_agent_id.is_empty(),
        "totalMessages": view.messages.len(),
        "outboundMessages": folded.len(),
        "estimatedTokens": folded_tokens,
        "rawTokens": view.total_tokens(),
        "thresholdTokens": threshold,
        "compressNeeded": threshold.map(|value| folded_tokens >= value).unwrap_or(false),
        "compressedMessages": covered.len(),
        "activeBlocks": blocks_json.len(),
        "blocks": blocks_json,
        "refs": refs_json,
        "protected": {
            "pinnedRef": pinned.and_then(|index| view.refs.get(index)).map(|item| item.id.clone()).unwrap_or_default(),
            "tailMessages": TAIL_PROTECTED_MESSAGES,
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
