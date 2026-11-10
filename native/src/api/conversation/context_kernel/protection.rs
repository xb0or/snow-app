//! 上游 `protected.ts` + `recommend.ts` + `nudge-text.ts`。
//!
//! 保护规则决定「哪些消息绝不能压」；推荐引擎决定「哪些剩余区间真的值得压」；
//! nudge 文本是这三者面向模型的最终呈现。全部为纯函数。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use std::collections::HashSet;

use super::refs;
use super::segments::{is_reasoning_message, is_tool_message, tool_call_ids, tool_names_of};
use super::texts;
use super::{ChatContextMessage, KernelBlock};

// ---------------------------------------------------------------------------
// protected.ts
// ---------------------------------------------------------------------------

/// **永远**受保护的工具（与用户配置无关）：内核自己的元数据工具。
///
/// - `compress`：压缩调用携带 decompress / search 依赖的摘要，系统提示词把历史
///   compress 调用当作承重元数据。
/// - `acp_rule`：记录持久规则，每轮重新注入系统提示 —— 压掉就等于悄悄丢失规则。
pub const ALWAYS_PROTECTED_TOOLS: &[&str] = &["compress", texts::RULE_TOOL_NAME];

/// **绝不**进入「软保护最近窗口」的工具结果。
///
/// 这些工具返回大内容（恢复的块、检索命中、文件正文、命令输出）。一旦落在
/// 最近 N 条窗口里，模型就再也收不回这部分上下文，而且它不会出现在可压缩区间
/// 推荐里。把它们排除出保护窗口，模型就能立刻再压一次 —— 它们依然可见。
pub const NEVER_PRESERVE_RECENT_TOOLS: &[&str] =
    &["decompress", "search_context", "read", "bash"];

/// 工具名模式匹配：精确名或尾部 `*` 前缀 glob，**两侧都不区分大小写**。
///
/// 客户端宿主对工具名大小写不一致（opencode `read` vs Claude/ZCode `Read`），
/// 所以每个基于该匹配器的开关都必须把它们视为相等。
pub fn match_tool_pattern(tool_name: &str, pattern: &str) -> bool {
    let name = tool_name.trim().to_ascii_lowercase();
    let pattern = pattern.trim().to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

fn matches_any(tool_name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| match_tool_pattern(tool_name, pattern))
}

/// 保护判定所需的配置切片。
#[derive(Clone, Debug, Default)]
pub struct ProtectionConfig {
    pub protected_tools: Vec<String>,
    /// `preserveRecentTools`：从 `NEVER_PRESERVE_RECENT_TOOLS` 里**减去**这些模式。
    pub preserve_recent_tools: Vec<String>,
    pub protected_latest_tools: Vec<String>,
    pub kernel_tool_prefix: String,
}

impl ProtectionConfig {
    fn is_protected_tool(&self, tool_name: &str) -> bool {
        if ALWAYS_PROTECTED_TOOLS
            .iter()
            .any(|entry| match_tool_pattern(tool_name, entry))
        {
            return true;
        }
        if !self.kernel_tool_prefix.is_empty() && tool_name.starts_with(&self.kernel_tool_prefix) {
            return true;
        }
        matches_any(tool_name, &self.protected_tools)
    }

    /// 是否属于「最近窗口排除」工具（`NEVER_PRESERVE_RECENT_TOOLS` 减去
    /// `preserveRecentTools`）。
    pub fn is_never_preserve_recent(&self, tool_name: &str) -> bool {
        let effective: Vec<&str> = NEVER_PRESERVE_RECENT_TOOLS
            .iter()
            .copied()
            .filter(|builtin| {
                !self
                    .preserve_recent_tools
                    .iter()
                    .any(|pattern| match_tool_pattern(builtin, pattern))
            })
            .collect();
        effective
            .iter()
            .any(|pattern| match_tool_pattern(tool_name, pattern))
    }
}

/// 消息本身（工具名匹配）是否受保护。
pub fn is_message_protected(message: &ChatContextMessage, config: &ProtectionConfig) -> bool {
    tool_names_of(message)
        .iter()
        .any(|name| config.is_protected_tool(name))
}

/// 受保护调用的 `toolCallId` 集合 —— 用它把「只有 toolCallId、没有 toolName」的
/// 结果另一半也保护住，否则受保护交换的结果半会漏进可压缩区间。
pub fn collect_protected_tool_call_ids(
    messages: &[ChatContextMessage],
    config: &ProtectionConfig,
) -> HashSet<String> {
    let mut ids: HashSet<String> = HashSet::new();
    for message in messages {
        if !is_message_protected(message, config) {
            continue;
        }
        ids.extend(tool_call_ids(message));
    }
    ids
}

/// 在 `is_message_protected` 基础上，额外按 `toolCallId` 匹配受保护调用集合。
pub fn is_message_protected_with_pairing(
    message: &ChatContextMessage,
    config: &ProtectionConfig,
    protected_call_ids: &HashSet<String>,
) -> bool {
    if is_message_protected(message, config) {
        return true;
    }
    super::segments::tool_result_ids(message)
        .iter()
        .any(|id| protected_call_ids.contains(id))
}

/// 「只有最新一条」保护集合（累计快照型工具，例如 todo_list）。
#[derive(Clone, Debug, Default)]
pub struct LatestProtected {
    pub call_ids: HashSet<String>,
    pub msg_ids: HashSet<String>,
}

/// 每个 `protectedLatestTools` 模式，只保护**最后**一次匹配的调用及其结果。
pub fn collect_latest_protected(
    messages: &[ChatContextMessage],
    config: &ProtectionConfig,
) -> LatestProtected {
    let mut latest = LatestProtected::default();
    for pattern in config.protected_latest_tools.iter() {
        for (index, message) in messages.iter().enumerate().rev() {
            let names = tool_names_of(message);
            if !names.iter().any(|name| match_tool_pattern(name, pattern)) {
                continue;
            }
            let ids = tool_call_ids(message);
            if ids.is_empty() {
                // 没有 toolCallId 就无法配对 —— 按消息下标保护。
                latest.msg_ids.insert(index.to_string());
            } else {
                latest.call_ids.extend(ids);
            }
            break;
        }
    }
    latest
}

pub fn is_message_latest_protected(
    message: &ChatContextMessage,
    latest: &LatestProtected,
) -> bool {
    if super::segments::tool_result_ids(message)
        .iter()
        .any(|id| latest.call_ids.contains(id))
    {
        return true;
    }
    !tool_call_ids(message).is_empty()
        && tool_call_ids(message)
            .iter()
            .any(|id| latest.call_ids.contains(id))
}

// ---------------------------------------------------------------------------
// recommend.ts
// ---------------------------------------------------------------------------

/// 可压缩区间（面向模型展示；`tokens` 是真实文本量而非估算）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompressibleRange {
    pub start_ref: String,
    pub end_ref: String,
    pub start_index: usize,
    pub end_index: usize,
    pub count: usize,
    pub tokens: i64,
    pub chars: usize,
    pub user_msgs: usize,
    /// 工具消息 token 累计（用于渲染真实的 tool/text 占比）。
    pub tool_tokens: i64,
    /// 文本消息 token 累计。
    pub text_tokens: i64,
    pub dangerous: bool,
}

/// 受保护区间（来自受保护工具的消息）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProtectedRange {
    pub start_ref: String,
    pub end_ref: String,
    pub start_index: usize,
    pub end_index: usize,
    pub count: usize,
    pub tokens: i64,
    pub tools: Vec<String>,
}

/// 推荐的软保护「尾部窗口」ref 集合。
///
/// 两条规则叠加：最后 N 条消息（`preserve_recent_messages`）与最后 N 个 token
/// 反向扩展（`preserve_recent_tokens`）。
pub fn compute_protected_refs(
    messages: &[ChatContextMessage],
    ref_of: &[String],
    preserve_recent_messages: usize,
    preserve_recent_tokens: i64,
    token_of: impl Fn(&ChatContextMessage) -> i64,
    config: &ProtectionConfig,
) -> HashSet<String> {
    let mut protected: HashSet<String> = HashSet::new();
    let visible: Vec<usize> = (0..messages.len())
        .filter(|index| {
            // 只考虑可见、非合成、有 ref 的消息。
            !ref_of.get(*index).is_some_and(|value| value.is_empty())
        })
        .collect();
    let recent_start = visible.len().saturating_sub(preserve_recent_messages);
    for index in visible.iter().skip(recent_start) {
        if let Some(reference) = ref_of.get(*index) {
            protected.insert(reference.clone());
        }
    }
    let mut budget = preserve_recent_tokens;
    for index in visible.iter().rev() {
        if budget <= 0 {
            break;
        }
        let message = &messages[*index];
        // 大结果型工具不进保护窗口：让模型能立刻再压它们。
        if tool_names_of(message)
            .iter()
            .any(|name| config.is_never_preserve_recent(name))
        {
            continue;
        }
        budget -= token_of(message);
        if let Some(reference) = ref_of.get(*index) {
            protected.insert(reference.clone());
        }
    }
    protected
}

/// 分类结果：可压缩 / 受保护 / 跳过。
#[derive(Clone, Debug, Default)]
pub struct ContextRanges {
    pub compressible: Vec<CompressibleRange>,
    pub protected: Vec<ProtectedRange>,
}

/// 构建可压缩与受保护区间（上游 `buildCompressibleRanges`）。
pub fn build_compressible_ranges(
    messages: &[ChatContextMessage],
    ref_of: &[String],
    protected_zone_refs: &HashSet<String>,
    protected_call_ids: &HashSet<String>,
    config: &ProtectionConfig,
    token_of: impl Fn(&ChatContextMessage) -> i64,
    first_is_pinned: bool,
) -> ContextRanges {
    let mut result = ContextRanges::default();
    let mut open_compressible: Option<CompressibleRange> = None;
    let mut open_protected: Option<ProtectedRange> = None;

    let flush_compressible = |open: &mut Option<CompressibleRange>, out: &mut ContextRanges| {
        if let Some(range) = open.take() {
            out.compressible.push(range);
        }
    };
    let flush_protected = |open: &mut Option<ProtectedRange>, out: &mut ContextRanges| {
        if let Some(range) = open.take() {
            out.protected.push(range);
        }
    };

    for (index, message) in messages.iter().enumerate() {
        let reference = ref_of.get(index).cloned().unwrap_or_default();
        if reference.is_empty() {
            flush_compressible(&mut open_compressible, &mut result);
            flush_protected(&mut open_protected, &mut result);
            continue;
        }
        if index == 0 && first_is_pinned {
            continue;
        }
        let is_protected_tool = is_message_protected_with_pairing(message, config, protected_call_ids);
        let in_zone = protected_zone_refs.contains(&reference) || is_reasoning_message(message);

        if in_zone {
            flush_compressible(&mut open_compressible, &mut result);
            flush_protected(&mut open_protected, &mut result);
            continue;
        }
        let tokens = token_of(message);
        if is_protected_tool {
            flush_compressible(&mut open_compressible, &mut result);
            let names = tool_names_of(message);
            match open_protected.as_mut() {
                Some(range) if range.end_index + 1 == index => {
                    range.end_ref = reference;
                    range.end_index = index;
                    range.count += 1;
                    range.tokens += tokens;
                    for name in names {
                        if !range.tools.contains(&name) {
                            range.tools.push(name);
                        }
                    }
                }
                _ => {
                    flush_protected(&mut open_protected, &mut result);
                    open_protected = Some(ProtectedRange {
                        start_ref: reference.clone(),
                        end_ref: reference,
                        start_index: index,
                        end_index: index,
                        count: 1,
                        tokens,
                        tools: names,
                    });
                }
            }
            continue;
        }
        flush_protected(&mut open_protected, &mut result);
        let is_user = message.role.trim() == "user";
        let tool_tokens = if is_tool_message(message) { tokens } else { 0 };
        let text_tokens = tokens - tool_tokens;
        match open_compressible.as_mut() {
            Some(range) if range.end_index + 1 == index => {
                range.end_ref = reference;
                range.end_index = index;
                range.count += 1;
                range.tokens += tokens;
                range.user_msgs += usize::from(is_user);
                range.tool_tokens += tool_tokens;
                range.text_tokens += text_tokens;
            }
            _ => {
                flush_compressible(&mut open_compressible, &mut result);
                open_compressible = Some(CompressibleRange {
                    start_ref: reference.clone(),
                    end_ref: reference,
                    start_index: index,
                    end_index: index,
                    count: 1,
                    tokens,
                    chars: 0,
                    user_msgs: usize::from(is_user),
                    tool_tokens,
                    text_tokens,
                    dangerous: false,
                });
            }
        }
    }
    flush_compressible(&mut open_compressible, &mut result);
    flush_protected(&mut open_protected, &mut result);
    result
}

fn pct(total: i64, part: i64) -> i64 {
    if total <= 0 {
        0
    } else {
        ((part * 100) / total).clamp(0, 100)
    }
}

/// 把相邻区间合并成「每个批次单独就能过 `min_chars`」的批次。
///
/// 合并按**真实文本量**（与 applyCompression 同一口径），而不是 token 估算 ——
/// 一旦宿主注入 CJK 友好的分词器（tokens ≈ chars），`tokens * 4` 会把体量高估
/// 约 4 倍，于是 nudge 推荐的区间在 apply 侧被整批拒绝。
pub fn merge_ranges_to_threshold(
    ranges: &[CompressibleRange],
    min_chars: usize,
) -> Vec<CompressibleRange> {
    let mut merged: Vec<CompressibleRange> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if last.chars < min_chars => {
                last.end_ref = range.end_ref.clone();
                last.end_index = range.end_index;
                last.count += range.count;
                last.tokens += range.tokens;
                last.chars += range.chars;
                last.user_msgs += range.user_msgs;
                if range.dangerous {
                    last.dangerous = true;
                }
            }
            _ => merged.push(range.clone()),
        }
    }
    // 尾批若仍低于阈值：并入前一批（允许超调）；没有前一批则整段丢弃 ——
    // 整个剩余部分都在闸门之下，任何选择都无法通过，推荐它只会产出必被拒的调用。
    if merged.len() > 1 && merged.last().is_some_and(|last| last.chars < min_chars) {
        let tail = merged.pop();
        if let (Some(last), Some(tail)) = (merged.last_mut(), tail) {
            last.end_ref = tail.end_ref;
            last.end_index = tail.end_index;
            last.count += tail.count;
            last.tokens += tail.tokens;
            last.chars += tail.chars;
            last.user_msgs += tail.user_msgs;
            last.dangerous |= tail.dangerous;
        }
    } else if merged.len() == 1 && merged[0].chars < min_chars {
        merged.clear();
    }
    merged
}

// ---------------------------------------------------------------------------
// nudge-text.ts
// ---------------------------------------------------------------------------

fn format_k(value: i64) -> String {
    if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn ref_number(reference: &str) -> usize {
    refs::ref_to_index(reference).unwrap_or_else(|| {
        let digits: String = reference
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        digits.parse().unwrap_or(0)
    })
}

struct MergedRange {
    start_ref: String,
    end_ref: String,
    start_pos: usize,
    end_pos: usize,
    count: usize,
    tokens: i64,
    user_msgs: usize,
    tool_tokens: i64,
    text_tokens: i64,
    compressible_tokens: i64,
    protected_tokens: i64,
    protected_tools: Vec<String>,
    dangerous: bool,
}

/// 压缩区间的人读渲染（上游 `formatRanges`，字节对齐）。
pub fn format_ranges(
    compressible: &[CompressibleRange],
    protected_ranges: &[ProtectedRange],
) -> String {
    if compressible.is_empty() && protected_ranges.is_empty() {
        return "[No specific ranges detected — compress any consumed content.]".to_string();
    }
    let mut entries: Vec<MergedRange> = Vec::new();
    for range in compressible {
        entries.push(MergedRange {
            start_ref: range.start_ref.clone(),
            end_ref: range.end_ref.clone(),
            start_pos: range.start_index,
            end_pos: range.end_index,
            count: range.count,
            tokens: range.tokens,
            user_msgs: range.user_msgs,
            tool_tokens: range.tool_tokens,
            text_tokens: range.text_tokens,
            compressible_tokens: range.tokens,
            protected_tokens: 0,
            protected_tools: Vec::new(),
            dangerous: range.dangerous,
        });
    }
    for range in protected_ranges {
        entries.push(MergedRange {
            start_ref: range.start_ref.clone(),
            end_ref: range.end_ref.clone(),
            start_pos: range.start_index,
            end_pos: range.end_index,
            count: range.count,
            tokens: range.tokens,
            user_msgs: 0,
            tool_tokens: 0,
            text_tokens: 0,
            compressible_tokens: 0,
            protected_tokens: range.tokens,
            protected_tools: range.tools.clone(),
            dangerous: false,
        });
    }
    entries.sort_by_key(|entry| (entry.start_pos, ref_number(&entry.start_ref)));

    let mut merged: Vec<MergedRange> = Vec::new();
    for entry in entries {
        match merged.last_mut() {
            Some(last) if entry.start_pos <= last.end_pos + 1 => {
                last.end_ref = entry.end_ref.clone();
                last.end_pos = last.end_pos.max(entry.end_pos);
                last.count += entry.count;
                last.tokens += entry.tokens;
                last.user_msgs += entry.user_msgs;
                last.tool_tokens += entry.tool_tokens;
                last.text_tokens += entry.text_tokens;
                last.compressible_tokens += entry.compressible_tokens;
                last.protected_tokens += entry.protected_tokens;
                last.dangerous |= entry.dangerous;
                for tool in entry.protected_tools {
                    if !last.protected_tools.contains(&tool) {
                        last.protected_tools.push(tool);
                    }
                }
            }
            _ => merged.push(entry),
        }
    }

    let lines: Vec<String> = merged
        .iter()
        .map(|entry| {
            let suffix = if entry.dangerous && entry.compressible_tokens > 0 {
                "  ⚠️ NOT recommended unless you are certain."
            } else {
                ""
            };
            if entry.protected_tokens > 0 && entry.compressible_tokens == 0 {
                return format!(
                    "  {}–{}  {} msgs  {} [PROTECTED: {} — not compressible]{}",
                    entry.start_ref,
                    entry.end_ref,
                    entry.count,
                    format_k(entry.tokens),
                    entry.protected_tools.join(", "),
                    suffix
                );
            }
            if entry.protected_tokens > 0 && entry.compressible_tokens > 0 {
                return format!(
                    "  {}–{}  {} msgs  {} [{} compressible | {} protected: {}]{} {}",
                    entry.start_ref,
                    entry.end_ref,
                    entry.count,
                    format_k(entry.tokens),
                    format_k(entry.compressible_tokens),
                    format_k(entry.protected_tokens),
                    entry.protected_tools.join(", "),
                    user_note(entry.user_msgs),
                    suffix
                );
            }
            format!(
                "  {}–{}  {} msgs  {} [tool {}% | text {}%] {} {}",
                entry.start_ref,
                entry.end_ref,
                entry.count,
                format_k(entry.tokens),
                pct(entry.tokens, entry.tool_tokens),
                pct(entry.tokens, entry.text_tokens),
                user_note(entry.user_msgs),
                suffix
            )
        })
        .collect();
    format!(
        "Compressible ranges ({}, oldest first):\n{}",
        merged.len(),
        lines.join("\n")
    )
}

fn user_note(count: usize) -> String {
    if count == 0 {
        String::new()
    } else if count > 1 {
        format!("· {count} user msgs")
    } else {
        format!("· {count} user msg")
    }
}

/// 活动块地图（上游 `formatBlockMap`）。
pub fn format_block_map(spans: &[(String, String, String, u8)], max_shown: usize) -> String {
    if spans.is_empty() {
        return String::new();
    }
    let hidden = spans.len().saturating_sub(max_shown);
    let shown: Vec<&(String, String, String, u8)> = if hidden > 0 {
        spans[spans.len() - max_shown..].iter().collect()
    } else {
        spans.iter().collect()
    };
    let items: Vec<String> = shown
        .iter()
        .map(|(block_id, start, end, tier)| {
            let tier_suffix = if *tier > 1 {
                format!(" t{tier}")
            } else {
                String::new()
            };
            format!("{block_id}={start}–{end}{tier_suffix}")
        })
        .collect();
    let prefix = if hidden > 0 {
        format!("…+{hidden} older · ")
    } else {
        String::new()
    };
    format!(
        "Active blocks ({}): {prefix}{}",
        spans.len(),
        items.join(" · ")
    )
}

/// 需要蒸馏的目标块（上游 `formatTierTargetBlocks`）。
pub fn format_tier_target_blocks(blocks: &[&KernelBlock], token_of: impl Fn(&str) -> i64) -> String {
    if blocks.is_empty() {
        return "Target blocks: (none — no tier blocks found)".to_string();
    }
    let lines: Vec<String> = blocks
        .iter()
        .map(|block| {
            let summary_tokens = token_of(&block.summary);
            let topic = if block.title.trim().is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", block.title.trim())
            };
            format!(
                "  {}  {} msgs  {}→{}{}",
                block.id,
                block.message_ids.len(),
                format_k(block.tokens_before),
                format_k(summary_tokens),
                topic
            )
        })
        .collect();
    format!(
        "Target {} blocks to distill ({}):\n{}",
        if blocks[0].tier <= 1 { "tier-1" } else { "tier-2" },
        blocks.len(),
        lines.join("\n")
    )
}

/// nudge 语态（上游 `NudgeVoice`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NudgeVoice {
    Gentle,
    Emergency,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedNudge {
    pub voice: NudgeVoice,
    pub text: String,
}

/// nudge 决策（上游 `NudgeDecision` 的最小可用切片）。
#[derive(Clone, Debug, Default)]
pub struct NudgeDecision {
    pub compressible: Vec<CompressibleRange>,
    pub protected_ranges: Vec<ProtectedRange>,
    pub block_spans: Vec<(String, String, String, u8)>,
    pub emergency: bool,
    pub tier: Option<u8>,
    /// tier ≥ 2 时需要蒸馏的目标块（真实块，用于渲染目标清单）。
    pub tier_targets: Vec<KernelBlock>,
}

fn efficiency_note() -> String {
    format!(
        "This is an efficiency nudge to compress early and keep context lean — not an overflow warning. A separate, stronger alert will appear if the context is actually full.\n\n{}",
        texts::COMPRESS_PHILOSOPHY
    )
}

fn emergency_header() -> String {
    format!(
        "⚠️ Context limit reached — compress now. Prioritize consumed tool outputs.\n\n{}",
        texts::COMPRESS_PHILOSOPHY
    )
}

/// 渲染 nudge 文本（上游 `renderNudgeText`，语态 + 结构对齐）。
pub fn render_nudge_text(decision: &NudgeDecision) -> RenderedNudge {
    let ranges_str = format_ranges(&decision.compressible, &decision.protected_ranges);
    let block_map_str = format_block_map(&decision.block_spans, 8);
    if let Some(tier) = decision.tier.filter(|tier| *tier >= 2) {
        let is_t2 = tier == 2;
        let targets: Vec<&KernelBlock> = decision.tier_targets.iter().collect();
        let block_list = format_tier_target_blocks(&targets, |text| (text.len() as i64 + 3) / 4);
        let start_id = decision
            .block_spans
            .first()
            .map(|(id, _, _, _)| id.clone())
            .unwrap_or_else(|| "b1".to_string());
        let end_id = decision
            .block_spans
            .last()
            .map(|(id, _, _, _)| id.clone())
            .unwrap_or_else(|| "b5".to_string());
        let voice = if decision.emergency {
            NudgeVoice::Emergency
        } else {
            NudgeVoice::Gentle
        };
        let trigger_line = if decision.emergency {
            format!(
                "[EMERGENCY — TIER {tier} {}] Context limit reached — distill NOW into a denser summary to reclaim tokens.",
                if is_t2 { "DISTILLATION" } else { "CONDENSATION" }
            )
        } else {
            format!(
                "[TIER {tier} {} TRIGGER]",
                if is_t2 { "DISTILLATION" } else { "CONDENSATION" }
            )
        };
        let parts = vec![
            efficiency_note(),
            String::new(),
            trigger_line,
            block_list,
            format!(
                "Example: compress({{ content: [{{ startId: \"{start_id}\", endId: \"{end_id}\", summary: \"...\" }}] }})"
            ),
            String::new(),
            texts::HOW_TO_COMPRESS_RULES.to_string(),
            String::new(),
            if is_t2 {
                texts::TIER2_DISTILL_RULES.to_string()
            } else {
                texts::TIER3_CONDENSE_RULES.to_string()
            },
        ];
        return RenderedNudge {
            voice,
            text: parts.join("\n"),
        };
    }
    if decision.emergency {
        return RenderedNudge {
            voice: NudgeVoice::Emergency,
            text: [
                emergency_header(),
                String::new(),
                texts::HOW_TO_COMPRESS_RULES.to_string(),
                String::new(),
                "{ \"topic\": \"...\", \"content\": [{ \"startId\": \"<ID>\", \"endId\": \"<ID>\", \"summary\": \"...\" }] }".to_string(),
                "Only use IDs from visible messages above. Compress older work first.".to_string(),
                String::new(),
                ranges_str,
                if block_map_str.is_empty() { String::new() } else { format!("\n{block_map_str}") },
            ]
            .join("\n"),
        };
    }
    RenderedNudge {
        voice: NudgeVoice::Gentle,
        text: [
            efficiency_note(),
            String::new(),
            texts::HOW_TO_COMPRESS_RULES.to_string(),
            String::new(),
            ranges_str,
            if block_map_str.is_empty() {
                String::new()
            } else {
                format!("\n{block_map_str}")
            },
            String::new(),
            "💡 If you compress, fold the ranges you keep in ONE call — pass multiple content entries (`content: [{...}, {...}]`) or ONE plain string holding every range, each block starting with its 'mNNNNN–mNNNNN topic' header line (most robust through lossy gateways). Ranges the task still needs can wait — they reappear in later nudges.".to_string(),
        ]
        .join("\n"),
    }
}

// 空块占位已移除：tier 目标渲染直接使用 `NudgeDecision.tier_targets` 里的真实块。
