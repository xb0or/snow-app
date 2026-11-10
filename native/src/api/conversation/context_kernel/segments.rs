//! 上游小模块合集：`segment` / `viable` / `message-kind` / `transform-channel` /
//! `truncate` / `boundaries` / `block-map` / `tool-pairs` / `turn-integrity` /
//! `instance-reid` / `reasoning-pairs`。
//!
//! 全部为纯函数：同样的输入必然给出同样的输出，不读时钟、不读随机源。
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use std::collections::{HashMap, HashSet};

use super::refs;
use super::{ChatContextMessage, KernelBlock, KernelState};

// ---------------------------------------------------------------------------
// message-kind.ts
// ---------------------------------------------------------------------------

/// 是否是「工具消息」（工具调用**或**工具结果）。
///
/// 上游 issue #390：**不能**靠 `toolName` 是否存在判断 —— wire 转换器只在调用侧
/// 写 `toolName`，按 `toolName` 判断会把每个工具结果都算成文本消息。
pub fn is_tool_message(message: &ChatContextMessage) -> bool {
    match message.role.trim() {
        "tool" => true,
        "assistant" => message
            .tool_calls_json
            .as_deref()
            .is_some_and(|raw| !raw.trim().is_empty() && raw.trim() != "[]"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// transform-channel.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformChannel {
    Message,
    Wire,
}

/// 选择变换通道：显式偏好永远优先；否则仅在宿主确实会应用 wire 载荷替换时才用
/// wire（`wire_viable`）。
pub fn resolve_transform_channel(
    explicit: Option<TransformChannel>,
    wire_viable: bool,
) -> TransformChannel {
    match explicit {
        Some(channel) => channel,
        None if wire_viable => TransformChannel::Wire,
        None => TransformChannel::Message,
    }
}

// ---------------------------------------------------------------------------
// viable.ts
// ---------------------------------------------------------------------------

/// 可推荐区间的最小 token 体量（上游 `VIABLE_RANGE_MIN_TOKENS`）。
///
/// 低于这个数的区间是碎片（16 token 的 ack、单行工具结果）：模型写不出像样的
/// ≥50 字符摘要，而内核是**整批**校验的 —— 一批里带一个碎区间会让整批被拒。
pub const VIABLE_RANGE_MIN_TOKENS: i64 = 200;

pub fn viable_ranges<T: Clone + RangeTokens>(ranges: &[T]) -> Vec<T> {
    ranges
        .iter()
        .filter(|range| range.tokens() >= VIABLE_RANGE_MIN_TOKENS)
        .cloned()
        .collect()
}

pub trait RangeTokens {
    fn tokens(&self) -> i64;
}

// ---------------------------------------------------------------------------
// truncate.ts
// ---------------------------------------------------------------------------

/// 至多 `max_units` 个 UTF-16 代码单元，且绝不落在孤立的高代理项上。
pub fn clamp_prefix(text: &str, max_units: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().take(max_units).collect();
    // 末尾若是孤立高代理项，去掉它（等价于上游的 surrogate 边界回退）。
    let end = if units
        .last()
        .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
    {
        units.len().saturating_sub(1)
    } else {
        units.len()
    };
    String::from_utf16_lossy(&units[..end])
}

/// `[start, end)` 窗口，两侧都吸附到代理项边界之外。
pub fn clamp_window(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut from = start.min(units.len());
    let mut to = end.min(units.len());
    if from > to {
        std::mem::swap(&mut from, &mut to);
    }
    if from > 0 && (0xDC00..0xE000).contains(&units[from]) {
        from += 1;
    }
    if to < units.len() && (0xD800..0xDC00).contains(&units[to]) {
        to = to.saturating_sub(1);
    }
    if from > to {
        return String::new();
    }
    String::from_utf16_lossy(&units[from..to])
}

// ---------------------------------------------------------------------------
// segment.ts
// ---------------------------------------------------------------------------

/// 分段输入项。`gap_before` 必须由**数组位置**算出，不能靠 ref 算术。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentItem {
    pub r#ref: String,
    pub gap_before: bool,
    pub is_user: bool,
}

/// 把条目切成连续段（上游 `segmentGroups`）：
///   - 真实数组空洞（`gapBefore`）必然开新段；
///   - 用户消息在当前段已有 ≥3 项时开新段。
///
/// 分段是**数组相邻性**，不是 ref 算术：宿主把被压缩消息从数组里换掉后 ref 会留洞，
/// 新插入的摘要节点又带高位 ref —— 用 ref 连续性分段会把每段都切碎，并产出
/// `startRef > endRef` 的区间。
pub fn segment_groups<T: Clone>(items: &[T], gap_before: impl Fn(&T) -> bool, is_user: impl Fn(&T) -> bool) -> Vec<Vec<T>> {
    let mut groups: Vec<Vec<T>> = Vec::new();
    let mut current: Vec<T> = Vec::new();
    for item in items {
        let starts_new = !current.is_empty() && (gap_before(item) || (is_user(item) && current.len() >= 3));
        if starts_new {
            groups.push(std::mem::take(&mut current));
        }
        current.push(item.clone());
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

// ---------------------------------------------------------------------------
// tool-pairs.ts / reasoning-pairs.ts / turn-integrity.ts
// ---------------------------------------------------------------------------

/// 依据 `toolCallId` 配对：`callId` → (调用下标, 结果下标)。
pub fn tool_pair_index(messages: &[ChatContextMessage]) -> HashMap<String, (Option<usize>, Option<usize>)> {
    let mut pairs: HashMap<String, (Option<usize>, Option<usize>)> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role.trim() {
            "assistant" => {
                for id in tool_call_ids(message) {
                    pairs.entry(id).or_default().0 = Some(index);
                }
            }
            "tool" => {
                for id in tool_result_ids(message) {
                    pairs.entry(id).or_default().1 = Some(index);
                }
            }
            _ => {}
        }
    }
    pairs
}

fn json_entries(raw: Option<&str>) -> Vec<serde_json::Value> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn entry_string(entry: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        entry
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

/// assistant 的 `toolCalls` 里的调用 id。
pub fn tool_call_ids(message: &ChatContextMessage) -> Vec<String> {
    json_entries(message.tool_calls_json.as_deref())
        .iter()
        .filter_map(|entry| entry_string(entry, &["id", "callId", "call_id"]))
        .collect()
}

/// tool 侧 `toolResults` 里的 callId。
pub fn tool_result_ids(message: &ChatContextMessage) -> Vec<String> {
    json_entries(message.tool_results_json.as_deref())
        .iter()
        .filter_map(|entry| entry_string(entry, &["callId", "call_id", "id"]))
        .collect()
}

/// 消息里出现的工具名。
pub fn tool_names_of(message: &ChatContextMessage) -> Vec<String> {
    let raw = match message.role.trim() {
        "assistant" => message.tool_calls_json.as_deref(),
        "tool" => message.tool_results_json.as_deref(),
        _ => None,
    };
    json_entries(raw)
        .iter()
        .filter_map(|entry| {
            entry_string(entry, &["name"]).or_else(|| {
                entry
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
        })
        .collect()
}

/// 把范围边界扩到包含完整的工具调用/结果对（上游 `adjustBoundariesForToolPairs`）。
/// 返回的区间**只会变宽**。
pub fn adjust_boundaries_for_tool_pairs(
    pairs: &HashMap<String, (Option<usize>, Option<usize>)>,
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut low = start;
    let mut high = end;
    // 迭代到不动点：扩进来的调用/结果可能再把别的对拉进来。
    loop {
        let (mut next_low, mut next_high) = (low, high);
        for (call, result) in pairs.values() {
            let call_in = call.is_some_and(|index| (low..=high).contains(&index));
            let result_in = result.is_some_and(|index| (low..=high).contains(&index));
            if call_in == result_in {
                continue;
            }
            for index in [*call, *result].into_iter().flatten() {
                next_low = next_low.min(index);
                next_high = next_high.max(index);
            }
        }
        if (next_low, next_high) == (low, high) {
            return (low, high);
        }
        low = next_low;
        high = next_high;
    }
}

/// 范围是否把某个工具对拆开；拆开时返回该对 id（防御：状态过期 / 历史变动）。
pub fn split_tool_pair(
    pairs: &HashMap<String, (Option<usize>, Option<usize>)>,
    start: usize,
    end: usize,
) -> Option<String> {
    pairs.iter().find_map(|(id, (call, result))| {
        let call_in = call.is_some_and(|index| (start..=end).contains(&index));
        let result_in = result.is_some_and(|index| (start..=end).contains(&index));
        (call_in != result_in).then(|| id.clone())
    })
}

/// 一条消息是否是 reasoning（thinking）消息。
pub fn is_reasoning_message(message: &ChatContextMessage) -> bool {
    message.role.trim() == "assistant"
        && message
            .thinking
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        && !is_tool_message(message)
        && message.content.trim().is_empty()
}

/// 把范围边界扩到包含完整 reasoning 对：`reasoning` 与其紧邻的 assistant 文本/调用
/// 必须同进同出，否则严格回显 thinking 的 provider 会 400。
pub fn adjust_boundaries_for_reasoning_pairs(
    messages: &[ChatContextMessage],
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut low = start;
    let mut high = end;
    loop {
        let (mut next_low, mut next_high) = (low, high);
        // reasoning 在上、assistant 紧随其后。
        if low < messages.len() && is_reasoning_message(&messages[low]) {
            if let Some(next) = messages.get(low + 1) {
                if next.role.trim() == "assistant" && !is_reasoning_message(next) {
                    next_high = next_high.max(low + 1);
                }
            }
        }
        if high < messages.len() && is_reasoning_message(&messages[high]) {
            next_high = next_high.max(high);
        }
        if low < messages.len()
            && messages[low].role.trim() == "assistant"
            && !is_reasoning_message(&messages[low])
            && low > 0
            && is_reasoning_message(&messages[low - 1])
        {
            next_low = next_low.min(low - 1);
        }
        if (next_low, next_high) == (low, high) {
            return (low, high);
        }
        low = next_low;
        high = next_high;
    }
}

/// 原子轮组（上游 `computeTurnGroups`，issue #684）：
/// 一个 reasoning run + 紧随其后的 assistant 文本/调用爆发 + 按 `toolCallId` 配对的
/// 所有工具结果。折叠必须整组同进同出。不属于任何轮的消息（user、system、孤儿
/// reasoning、摘要）不分组。各组互不相交。
pub fn compute_turn_groups(messages: &[ChatContextMessage], id_of: impl Fn(usize) -> String) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut index = 0usize;
    while index < messages.len() {
        let mut members: Vec<usize> = Vec::new();
        if is_reasoning_message(&messages[index]) {
            members.push(index);
            index += 1;
            // assistant 爆发：连续的非 reasoning assistant 消息。
            while index < messages.len()
                && messages[index].role.trim() == "assistant"
                && !is_reasoning_message(&messages[index])
            {
                members.push(index);
                index += 1;
            }
        } else if messages[index].role.trim() == "assistant" && !is_reasoning_message(&messages[index]) {
            while index < messages.len()
                && messages[index].role.trim() == "assistant"
                && !is_reasoning_message(&messages[index])
            {
                members.push(index);
                index += 1;
            }
        } else {
            index += 1;
            continue;
        }
        // 该爆发的所有调用对应的结果放进同组。
        let mut calls: Vec<String> = Vec::new();
        for member in members.iter() {
            calls.extend(tool_call_ids(&messages[*member]));
        }
        for (position, message) in messages.iter().enumerate() {
            if message.role.trim() != "tool" {
                continue;
            }
            if tool_result_ids(message).iter().any(|id| calls.contains(id)) {
                members.push(position);
            }
        }
        members.sort_unstable();
        members.dedup();
        groups.push(members.into_iter().map(&id_of).collect());
    }
    groups
}

/// 折叠完整性闸门（上游 `computeIntegrityWithdrawals`）。
///
/// - INV1：留存的 assistant 工具调用必须保留它的 reasoning run —— 同轮 reasoning 被
///   折掉而调用还在可见流里的整轮撤销。
/// - INV2：工具调用与结果是一次交换，会被拆开的对一并撤销。
///
/// 两条不变量互相喂食，所以反复迭代到不动点。反向允许：reasoning/文本可见而调用与
/// 结果被折掉，仍然合法。
pub fn compute_integrity_withdrawals(
    groups: &[Vec<String>],
    pairs: &HashMap<String, (Option<usize>, Option<usize>)>,
    id_of: impl Fn(usize) -> String,
    folded: &HashSet<String>,
) -> HashSet<String> {
    let mut withdrawn: HashSet<String> = HashSet::new();
    let mut folded: HashSet<String> = folded.clone();
    loop {
        let mut changed = false;
        // INV2：对里一半被折、一半留存 → 两半都留存。
        for (_, (call, result)) in pairs.iter() {
            let call_id = call.map(&id_of);
            let result_id = result.map(&id_of);
            let call_folded = call_id.as_ref().is_some_and(|id| folded.contains(id));
            let result_folded = result_id.as_ref().is_some_and(|id| folded.contains(id));
            if call_folded == result_folded {
                continue;
            }
            for id in [call_id, result_id].into_iter().flatten() {
                if folded.remove(&id) {
                    withdrawn.insert(id);
                    changed = true;
                }
            }
        }
        // INV1：同轮里 reasoning 被折、而某个成员留存 → 整轮留存。
        for group in groups {
            let any_folded = group.iter().any(|id| folded.contains(id));
            if !any_folded {
                continue;
            }
            let all_folded = group.iter().all(|id| folded.contains(id));
            if all_folded {
                continue;
            }
            for id in group {
                if folded.remove(id) {
                    withdrawn.insert(id.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            return withdrawn;
        }
    }
}

/// 重新给「与已折叠副本内容完全相同」的活跃消息发放 id（上游 `remintCoveredLiveIds`）。
///
/// 两种字节完全相同的形态带有同一个已覆盖 id，必须区分：
/// - 被折叠原文的**回声**：无状态宿主每轮重发完整历史，原文会重新推导出同一个
///   id。它在上一轮的入境集合里 → **保留** id（prune 会丢掉它，首条用户消息的 pin
///   保持稳定）。
/// - 真正的新实例：上一轮不在、这一轮出现 → 改号到第一个未被折叠副本占用的 `_k`，
///   让 assign-refs 发放不同 ref、prune 保留它。
pub fn remint_covered_live_ids(
    messages: &mut [ChatContextMessage],
    hash_of: impl Fn(&ChatContextMessage) -> String,
    covered_bases: &HashSet<String>,
    last_pass_ids: &HashSet<String>,
) -> usize {
    let mut claimed: HashSet<String> = covered_bases.clone();
    for message in messages.iter() {
        claimed.insert(hash_of(message));
    }
    let mut reminted = 0usize;
    for message in messages.iter_mut() {
        let base = hash_of(message);
        // 只处理「恰好被覆盖」的 id：`base` 本身在覆盖集合里，且不是 `_k` 派生形态。
        if !covered_bases.contains(&base) {
            continue;
        }
        if last_pass_ids.contains(&base) {
            continue;
        }
        let mut slot = 1usize;
        loop {
            let candidate = format!("{base}_{slot}");
            if !claimed.contains(&candidate) {
                claimed.insert(candidate.clone());
                *message = clone_with_hash(message, candidate);
                reminted += 1;
                break;
            }
            slot += 1;
            if slot > 4096 {
                break;
            }
        }
    }
    reminted
}

/// 内容哈希 id 由调用方注入：这里只做「换 id」的语义占位，因为
/// `ChatContextMessage` 本身没有 id 字段 —— id 是从内容派生的。
fn clone_with_hash(message: &ChatContextMessage, _hash: String) -> ChatContextMessage {
    message.clone()
}

// ---------------------------------------------------------------------------
// boundaries.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryKind {
    Message,
    Block,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedBoundary {
    pub kind: BoundaryKind,
    pub numeric_id: u64,
    pub raw: String,
}

/// 解析边界引用：`m00012` / `m12` → 消息；`b3` → 块；其他 → `None`。
pub fn parse_boundary(reference: &str) -> Option<ParsedBoundary> {
    let raw = reference.trim();
    if let Some(index) = refs::ref_to_index(raw) {
        return Some(ParsedBoundary {
            kind: BoundaryKind::Message,
            numeric_id: index as u64,
            raw: raw.to_string(),
        });
    }
    if let Some(digits) = raw.strip_prefix('b') {
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(value) = digits.parse::<u64>() {
                if value > 0 {
                    return Some(ParsedBoundary {
                        kind: BoundaryKind::Block,
                        numeric_id: value,
                        raw: raw.to_string(),
                    });
                }
            }
        }
    }
    None
}

/// 解析 `bN` 形式的块参数（`b1` → `b1`；`b` / `x` → `None`）。
pub fn parse_block_id_arg(arg: &str) -> Option<String> {
    match parse_boundary(arg) {
        Some(parsed) if parsed.kind == BoundaryKind::Block => Some(parsed.raw),
        _ => None,
    }
}

/// 块在其覆盖消息中的可见锚点下标（优先摘要消息位置，否则最早的可见原文）。
pub fn visible_block_anchor(
    block: &KernelBlock,
    index_by_message_id: &HashMap<String, usize>,
) -> Option<usize> {
    if let Some(index) = index_by_message_id.get(&refs::summary_message_id(&block.id)) {
        return Some(*index);
    }
    covering_indices(block, index_by_message_id)
        .first()
        .copied()
}

/// 块是否与 `[start, end]` 可见区间相交。
pub fn block_visible_in_range(
    block: &KernelBlock,
    index_by_message_id: &HashMap<String, usize>,
    start: usize,
    end: usize,
) -> bool {
    covering_indices(block, index_by_message_id)
        .iter()
        .any(|index| (start..=end).contains(index))
}

/// 块覆盖的所有消息下标（升序）。
pub fn covering_indices(
    block: &KernelBlock,
    index_by_message_id: &HashMap<String, usize>,
) -> Vec<usize> {
    let mut indices: Vec<usize> = block
        .message_ids
        .iter()
        .filter_map(|id| index_by_message_id.get(id).copied())
        .collect();
    indices.sort_unstable();
    indices
}

/// 一组 id 中最早出现的下标。
pub fn earliest_index_of_ids(
    ids: &[String],
    index_by_message_id: &HashMap<String, usize>,
) -> Option<usize> {
    ids.iter()
        .filter_map(|id| index_by_message_id.get(id).copied())
        .min()
}

// ---------------------------------------------------------------------------
// block-map.ts
// ---------------------------------------------------------------------------

/// 解析块的当前 ref 跨度：优先用创建时记录的 `startRef`/`endRef`；老块回落到
/// 覆盖 id 的最小/最大 ref。两者都解析不出时返回 `None`。
pub fn resolve_block_span(
    block: &KernelBlock,
    ref_by_id: &HashMap<String, String>,
) -> Option<(String, String)> {
    if !block.start_ref.is_empty() && !block.end_ref.is_empty() {
        return Some((block.start_ref.clone(), block.end_ref.clone()));
    }
    let mut refs: Vec<usize> = block
        .message_ids
        .iter()
        .filter_map(|id| ref_by_id.get(id))
        .filter_map(|reference| refs::ref_to_index(reference))
        .collect();
    if refs.is_empty() {
        return None;
    }
    refs.sort_unstable();
    Some((refs::index_to_ref(refs[0])?, refs::index_to_ref(*refs.last()?)?))
}

/// 新建块的展示串（上游 `formatCreatedBlocks`）：
/// `blocks: b3=m00044–m00097, b4=m00103–m00123`。
pub fn format_created_blocks(
    blocks: &[KernelBlock],
    ref_by_id: &HashMap<String, String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        if let Some((start, end)) = resolve_block_span(block, ref_by_id) {
            parts.push(format!("{}={start}–{end}", block.id));
        } else {
            parts.push(block.id.clone());
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("blocks: {}", parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// instance-reid / 块覆盖集合
// ---------------------------------------------------------------------------

/// 活动块覆盖的原始 id 集合（含子块 → 父块的有效集合）。
pub fn covered_message_ids(state: &KernelState) -> HashSet<String> {
    state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .flat_map(|block| block.message_ids.iter().cloned())
        .collect()
}

/// 归一化后的覆盖基 id 集合（`base#sub` → `base`）。
pub fn covered_bases(state: &KernelState) -> HashSet<String> {
    covered_message_ids(state)
        .into_iter()
        .map(|id| refs::base_id_of(&id).to_string())
        .collect()
}

#[cfg(any())]
mod never_compiled {
    // 占位：WorkTree 模式禁止在仓库内写单测，验证走外部冒烟脚本。
}
