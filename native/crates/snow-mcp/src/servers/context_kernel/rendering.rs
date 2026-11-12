//! 上游 `report.ts` + `handoff.ts` + `rules.ts` 的渲染半边。
//!
//! `report` 给模型看现状与回放；`handoff` 给 Goal / WorkTree 模式与 UI 复用一份可读的
//! 交接文档；`rules` 渲染持久规则段落。全部为纯函数，不读时钟（时间戳由调用方注入）。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use super::refs;
use super::texts;
use super::{ChatContextMessage, KernelBlock, KernelState};

// ---------------------------------------------------------------------------
// rules.ts
// ---------------------------------------------------------------------------

/// 一条持久规则。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleRecord {
    pub id: String,
    pub text: String,
}

/// 规则上限（`config.rules` 可覆盖默认值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleLimits {
    pub max_rules: usize,
    pub max_rule_chars: usize,
}

impl Default for RuleLimits {
    fn default() -> Self {
        Self {
            max_rules: texts::DEFAULT_RULE_MAX_RULES,
            max_rule_chars: texts::DEFAULT_RULE_MAX_CHARS,
        }
    }
}

pub fn list_rules(state: &KernelState) -> Vec<RuleRecord> {
    state
        .rules
        .iter()
        .enumerate()
        .map(|(index, text)| RuleRecord {
            id: format!("rule{}", index + 1),
            text: text.clone(),
        })
        .collect()
}

/// 下一次 `add_rule` 会发放的 id（纯函数，不改状态）。
///
/// 规则在 Snow 侧以纯文本数组存储，id 由序号派生；「绝不重发已发放 id」的语义
/// 在这里退化为「按当前最高存活序号 + 1」，与上游的可观测行为一致。
pub fn allocate_rule_id(state: &KernelState) -> String {
    format!("rule{}", state.rules.len() + 1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddRuleOutcome {
    Ok(RuleRecord),
    Err(String),
}

pub fn add_rule(state: &mut KernelState, text: &str, limits: RuleLimits) -> AddRuleOutcome {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return AddRuleOutcome::Err("rule text is empty".to_string());
    }
    if trimmed.chars().count() > limits.max_rule_chars {
        return AddRuleOutcome::Err(format!(
            "rule text exceeds {} characters",
            limits.max_rule_chars
        ));
    }
    if state.rules.len() >= limits.max_rules {
        return AddRuleOutcome::Err(format!(
            "at most {} rules may be recorded; remove one first",
            limits.max_rules
        ));
    }
    if state.rules.iter().any(|existing| existing == trimmed) {
        return AddRuleOutcome::Err("rule already recorded".to_string());
    }
    state.rules.push(trimmed.to_string());
    AddRuleOutcome::Ok(RuleRecord {
        id: allocate_rule_id(state),
        text: trimmed.to_string(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoveRuleOutcome {
    Ok(RuleRecord),
    Err(String),
}

pub fn remove_rule(state: &mut KernelState, id: &str) -> RemoveRuleOutcome {
    let records = list_rules(state);
    let Some(record) = records.into_iter().find(|rule| rule.id == id) else {
        return RemoveRuleOutcome::Err(format!("no rule with id {id}"));
    };
    state.rules.retain(|text| text != &record.text);
    RemoveRuleOutcome::Ok(record)
}

pub fn clear_rules(state: &mut KernelState) -> usize {
    let count = state.rules.len();
    state.rules.clear();
    count
}

/// 系统提示词里的规则段落。无规则时返回空串，空会话零 token 成本。
pub fn format_rules_for_prompt(state: &KernelState) -> String {
    if state.rules.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = state
        .rules
        .iter()
        .enumerate()
        .map(|(index, text)| format!("- [rule{}] {}", index + 1, text))
        .collect();
    format!(
        "# Persistent rules (recorded via {} — kept across compression)\n{}",
        texts::RULE_TOOL_NAME,
        lines.join("\n")
    )
}

/// 纯序号渲染（`acp_rule` 列出规则时原样返回）。
pub fn format_rules_list(rules: &[RuleRecord]) -> String {
    rules
        .iter()
        .enumerate()
        .map(|(index, rule)| format!("{}. [{}] {}", index + 1, rule.id, rule.text))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// report.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct StatusReportMeta {
    pub pack: String,
    pub pack_version: String,
    pub host: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusScope {
    Compressed,
    Uncompressed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusView {
    Ranges,
    Messages,
}

#[derive(Clone, Debug, Default)]
pub struct StatusReportOptions {
    pub scope: Option<StatusScope>,
    pub view: Option<StatusView>,
    pub tool: String,
    pub sort: String,
    pub limit: usize,
    pub meta: StatusReportMeta,
}

fn format_k(value: i64) -> String {
    if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// 上下文构成统计。
#[derive(Clone, Copy, Debug, Default)]
pub struct ContextBreakdown {
    pub tool: i64,
    pub text: i64,
    pub summaries: i64,
    pub code: i64,
    pub system: i64,
}

impl ContextBreakdown {
    pub fn total(&self) -> i64 {
        self.tool + self.text + self.summaries + self.code + self.system
    }
}

fn pct(total: i64, part: i64) -> String {
    if total <= 0 {
        "0%".to_string()
    } else {
        format!("{}%", (part * 100) / total)
    }
}

fn surface_line(meta: &StatusReportMeta) -> String {
    if meta.pack.is_empty() && meta.host.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if !meta.pack.is_empty() {
        if meta.pack_version.is_empty() {
            parts.push(format!("pack={}", meta.pack));
        } else {
            parts.push(format!("pack={} v{}", meta.pack, meta.pack_version));
        }
    }
    if !meta.host.is_empty() {
        parts.push(format!("host={}", meta.host));
    }
    format!("ACTIVE SURFACE: {}\n\n", parts.join(" | "))
}

/// 压缩块状态报告（上游 `buildStatusReport` 的骨架对齐）。
pub fn build_status_report(
    state: &KernelState,
    breakdown: ContextBreakdown,
    options: &StatusReportOptions,
) -> String {
    if options.scope == Some(StatusScope::Uncompressed) {
        return build_uncompressed_report(state, breakdown, options);
    }
    let total = breakdown.total();
    let mut out = surface_line(&options.meta);
    out.push_str(&format!(
        "CONTEXT BREAKDOWN\n  {} tool ({}) | {} text ({}) | {} summaries ({})\n\n",
        format_k(breakdown.tool),
        pct(total, breakdown.tool),
        format_k(breakdown.text),
        pct(total, breakdown.text),
        format_k(breakdown.summaries),
        pct(total, breakdown.summaries),
    ));
    out.push_str("COMPRESSED BLOCKS\n");
    let active: Vec<&KernelBlock> = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .collect();
    if active.is_empty() {
        out.push_str("  No compressed blocks.\n");
    } else {
        for block in active.iter() {
            let span = if block.start_ref.is_empty() {
                format!("{} msgs", block.message_ids.len())
            } else {
                format!("{}–{}", block.start_ref, block.end_ref)
            };
            let title = if block.title.trim().is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", block.title.trim())
            };
            out.push_str(&format!(
                "  {}  t{}  {}  {}{}\n",
                block.id,
                block.tier,
                span,
                format_k(block.tokens_before),
                title
            ));
        }
    }
    out.push_str(&format!(
        "\nTip: buildStatusReport({{scope:\"uncompressed\", view:\"messages\", tool:\"bash\"}}) for per-message listing"
    ));
    out
}

fn build_uncompressed_report(
    _state: &KernelState,
    breakdown: ContextBreakdown,
    _options: &StatusReportOptions,
) -> String {
    let visible = breakdown.tool + breakdown.text;
    format!(
        "UNCOMPRESSED — {} | {} visible messages\n\n  (no uncompressed messages)",
        format_k(visible),
        if visible > 0 { 1 } else { 0 }
    )
}

/// 单块或全局的「回放」（上游 `buildRecap`）。Goal / WorkTree 模式与 UI 复用。
pub fn build_recap(state: &KernelState, block_id: Option<&str>) -> String {
    let blocks: Vec<&KernelBlock> = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .filter(|block| block_id.is_none_or(|id| block.id == id))
        .collect();
    if blocks.is_empty() {
        return "No active compression blocks.".to_string();
    }
    let lines: Vec<String> = blocks
        .iter()
        .map(|block| {
            let span = if block.start_ref.is_empty() {
                format!("{} msgs", block.message_ids.len())
            } else {
                format!("{}–{}", block.start_ref, block.end_ref)
            };
            let title = block.title.trim();
            let head = if title.is_empty() {
                format!("[{}] t{} · {}", block.id, block.tier, span)
            } else {
                format!("[{}] t{} · {} · {title}", block.id, block.tier, span)
            };
            let saved = (block.tokens_before - block.tokens_after).max(0);
            let body: String = block
                .summary
                .lines()
                .take(24)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{head}\n  saved ~{} tok\n{body}", format_k(saved))
        })
        .collect();
    format!(
        "COMPRESSION RECAP — {} active block(s)\n\n{}",
        blocks.len(),
        lines.join("\n\n")
    )
}

// ---------------------------------------------------------------------------
// handoff.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct HandoffMeta {
    pub title: String,
    pub label: String,
    pub session_id: String,
    pub context_tokens: Option<i64>,
    pub extra_bullets: Vec<String>,
}

/// 单条消息的可读渲染（交接文档里的对话回放）。
pub fn render_message(message: &ChatContextMessage, reference: Option<&str>) -> String {
    let prefix = reference.map(|value| format!("[{value}] ")).unwrap_or_default();
    let role = message.role.trim();
    if role == "tool" {
        let names = super::segments::tool_names_of(message);
        let ids = super::segments::tool_result_ids(message);
        let label = if names.is_empty() {
            "tool result".to_string()
        } else {
            names.join(",")
        };
        let call = ids.first().cloned().unwrap_or_default();
        let mut body = message.content.trim().to_string();
        if body.is_empty() {
            if let Some(raw) = message.tool_results_json.as_deref() {
                body = raw.trim().to_string();
            }
        }
        return format!("{prefix}`{label}({call})` →\n{body}");
    }
    if role == "assistant" && !super::segments::tool_call_ids(message).is_empty() {
        let names = super::segments::tool_names_of(message);
        let label = if names.is_empty() {
            "?".to_string()
        } else {
            names.join(",")
        };
        let args = message.tool_calls_json.clone().unwrap_or_default();
        return format!("{prefix}`{label}()` args:\n{args}");
    }
    let mut out = String::new();
    if let Some(thinking) = message.thinking.as_deref() {
        if !thinking.trim().is_empty() {
            out.push_str(thinking.trim());
            out.push('\n');
        }
    }
    out.push_str(message.content.trim());
    if out.trim().is_empty() {
        return format!("{prefix}_(empty)_");
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct HandoffBlockFull {
    pub block_id: String,
    pub topic: String,
    pub count: usize,
    pub full_text: String,
}

#[derive(Clone, Debug)]
pub struct HandoffInput<'a> {
    pub messages: &'a [ChatContextMessage],
    pub state: &'a KernelState,
    /// 完整对话（含被折叠段的原文）还是折叠后的快照。
    pub full: bool,
    /// 传入的就是已经折叠过的持久快照：原样渲染，不再重新折叠。
    pub folded: bool,
    /// 折叠快照模式下补在文末的「各活动块的原文」。
    pub blocks_full: Vec<HandoffBlockFull>,
    pub meta: HandoffMeta,
    /// 每条消息的 ref（与 `messages` 等长；缺省为空串）。
    pub refs_of: Vec<String>,
}

/// 会话交接文档渲染（上游 `renderHandoff` 的章节骨架）。
pub fn render_handoff(input: &HandoffInput<'_>) -> String {
    let mut out = String::new();
    out.push_str("# billion-context session handoff\n\n");
    out.push_str(&format!(
        "- title: {}\n",
        if input.meta.title.trim().is_empty() {
            "(untitled)"
        } else {
            input.meta.title.trim()
        }
    ));
    if !input.meta.label.trim().is_empty() {
        out.push_str(&format!("- label: {}\n", input.meta.label.trim()));
    }
    out.push_str(&format!("- session id: {}\n", input.meta.session_id));
    if let Some(tokens) = input.meta.context_tokens {
        out.push_str(&format!("- last context tokens: ~{tokens}\n"));
    }
    let total = input.state.blocks.len();
    let active = input
        .state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .count();
    out.push_str(&format!(
        "- compression blocks: {total} (active {active})\n"
    ));
    for bullet in input.meta.extra_bullets.iter() {
        if !bullet.trim().is_empty() {
            out.push_str(&format!("- {}\n", bullet.trim()));
        }
    }

    let count = input.messages.len();
    if input.folded {
        out.push_str(&format!(
            "\n## Conversation (persisted folded snapshot, {count} messages)\n\n"
        ));
    } else if input.full {
        out.push_str(&format!("\n## Full conversation ({count} messages)\n\n"));
    } else {
        out.push_str(&format!(
            "\n## Conversation (folded view as the model saw it, {count} client messages)\n\n"
        ));
    }
    for (index, message) in input.messages.iter().enumerate() {
        let reference = input.refs_of.get(index).cloned().unwrap_or_default();
        out.push_str(&format!(
            "### {}\n\n{}\n",
            message.role.trim(),
            render_message(message, (!reference.is_empty()).then_some(reference.as_str()))
        ));
    }

    for block in input.blocks_full.iter() {
        out.push_str(&format!(
            "\n## Block {} — {}\n\n### Original messages ({})\n\n{}\n",
            block.block_id,
            if block.topic.trim().is_empty() {
                "(untitled)"
            } else {
                block.topic.trim()
            },
            block.count,
            block.full_text
        ));
    }
    out
}

/// 按选择器匹配会话（label / id 前缀，大小写不敏感）。
pub fn match_session<'a, T: HasSessionId>(
    sessions: &'a [T],
    selector: &str,
    label_of: impl Fn(&T) -> Option<String>,
) -> Vec<&'a T> {
    let needle = selector.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return sessions.iter().collect();
    }
    sessions
        .iter()
        .filter(|session| {
            session
                .session_id()
                .to_ascii_lowercase()
                .starts_with(&needle)
                || label_of(session)
                    .is_some_and(|label| label.to_ascii_lowercase().contains(&needle))
        })
        .collect()
}

pub trait HasSessionId {
    fn session_id(&self) -> String;
}

/// 把活动块的展示跨度收集成 `formatBlockMap` 需要的元组。
pub fn block_spans(state: &KernelState) -> Vec<(String, String, String, u8)> {
    state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .map(|block| {
            (
                block.id.clone(),
                block.start_ref.clone(),
                block.end_ref.clone(),
                block.tier,
            )
        })
        .collect()
}

/// 活动块里 tier ≥ 2 的块（nudge 的蒸馏目标）。
pub fn tier_targets(state: &KernelState, tier: u8) -> Vec<KernelBlock> {
    state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty() && block.tier == tier)
        .cloned()
        .collect()
}

/// 摘要消息的文本（`prune` 注入的那条）。
pub fn summary_text(block: &KernelBlock) -> String {
    let span = block.message_ids.len();
    let mut out = String::new();
    out.push_str(texts::SUMMARY_HEADER);
    out.push_str(&format!(" — {}\n", block.id));
    if !block.title.trim().is_empty() {
        out.push_str(&format!("topic: {}\n", block.title.trim()));
    }
    out.push('\n');
    out.push_str(block.summary.trim());
    out.push('\n');
    let span_refs = if block.start_ref.is_empty() {
        String::new()
    } else {
        format!(" ({0}–{1})", block.start_ref, block.end_ref)
    };
    out.push_str(&format!(
        "\n> Covered {span} earlier messages{span_refs}. Restore with decompress (blockId \"{}\").",
        block.id
    ));
    out
}

/// 摘要消息的稳定 id（视图用）。
pub fn summary_message_ref(block: &KernelBlock) -> String {
    refs::summary_message_id(&block.id)
}
