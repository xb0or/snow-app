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

// ---------------------------------------------------------------------------
// 上游模块移植（`acp-kernel` 0.0.101）
// ---------------------------------------------------------------------------
//
// 每个子模块对应上游一个 `.ts` 模块，保持同名语义与字节稳定文本；子模块是纯函数，
// 通过 `super::` 访问本文件的类型。
pub mod protection;
pub mod refs;
pub mod rendering;
pub mod segments;
pub mod steering;
pub mod texts;

/// 状态结构版本：结构变化时旧状态会被丢弃，而不是解析出错。
pub const KERNEL_VERSION: &str = "snow-context-kernel/2";

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
const MEDIA_MARKERS: &[&str] = &[
    "@@image:",
    "data:image/",
    "\"type\":\"image\"",
    "\"type\": \"image\"",
];

/// 折叠范围自动对齐到「轮」（user 开启新轮，assistant + 其工具调用/结果同轮）。
const SNAP_TO_TURNS: bool = true;

/// 建议区间的最小体量：低于这个 token 数的范围不值得折叠。
const VIABLE_RANGE_MIN_TOKENS: i64 = 800;

/// 状态里最多给出多少个建议区间。
const MAX_SUGGESTED_RANGES: usize = 5;

/// 出站视图是否在 user/assistant 文本前渲染 `[ref]`（计划里的 renderTags:
/// text-only）。只改文本消息，绝不写入工具调用参数；前缀由内容哈希决定，
/// 跨轮次字节一致。
const RENDER_REFS: bool = true;

/// 折叠规则（acp_rule）上限：条数与单条长度。
const MAX_RULES: usize = 32;
const MAX_RULE_CHARS: usize = 500;

/// 超过该字符数的工具结果在出站视图里先做类型化压缩（crush），再决定是否占位。
const CRUSH_MIN_CHARS: usize = 12_000;
const CRUSH_HEAD_CHARS: usize = 2_000;
const CRUSH_TAIL_CHARS: usize = 800;
const CRUSH_SAMPLE_ITEMS: usize = 3;

/// 分层折叠（tiers）：tier1 摘要积累到 `tier2_trigger` 条后建议蒸馏，依次类推。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TiersConfig {
    pub enabled: bool,
    pub tier2_trigger: i64,
    pub tier3_trigger: i64,
}

impl Default for TiersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tier2_trigger: 1000,
            tier3_trigger: 2000,
        }
    }
}

/// nudge 频率与增长闸门（上游 `config.nudge`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NudgeConfig {
    pub max_context_limit_pct: f64,
    pub min_context_limit_pct: f64,
    pub frequency: u32,
    pub iteration_threshold: u32,
    pub force: String,
    pub growth_ratio: f64,
    pub growth_floor: i64,
    pub growth_cap: i64,
    pub min_growth_floor: i64,
    pub min_growth_ratio: f64,
    pub emergency_threshold_pct: f64,
    pub tier2_growth_multiplier: f64,
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            max_context_limit_pct: 0.75,
            min_context_limit_pct: 0.45,
            frequency: 5,
            iteration_threshold: 15,
            force: "soft".to_string(),
            growth_ratio: 0.05,
            growth_floor: 50_000,
            growth_cap: 50_000,
            min_growth_floor: 20_000,
            min_growth_ratio: 0.45,
            emergency_threshold_pct: 0.95,
            tier2_growth_multiplier: 1.5,
        }
    }
}

/// 危急裁剪（上游 `config.truncate`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TruncateConfig {
    pub threshold: f64,
    pub terminal_escape_after: u32,
}

impl Default for TruncateConfig {
    fn default() -> Self {
        Self {
            threshold: 0.95,
            terminal_escape_after: 3,
        }
    }
}

/// 压缩闸门（上游 `config.compress`）。`min_compress_range` 是**真实字符数**。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompressLimits {
    pub min_compress_range: usize,
    pub max_summary_length: usize,
    pub min_summary_length: usize,
}

impl Default for CompressLimits {
    fn default() -> Self {
        Self {
            min_compress_range: 5_000,
            max_summary_length: 20_000,
            min_summary_length: 50,
        }
    }
}

/// absorb（即时工具结果吸收）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AbsorbSettings {
    pub enabled: bool,
    pub tool_name: String,
    pub min_tool_tokens: i64,
    pub context_threshold_pct: f64,
    pub exclude_tools: Vec<String>,
}

impl Default for AbsorbSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tool_name: "absorb".to_string(),
            min_tool_tokens: 4_000,
            context_threshold_pct: 0.0,
            exclude_tools: Vec::new(),
        }
    }
}

/// crush（类型化确定性压缩）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrushSettings {
    pub enabled: bool,
    pub min_reduction: f64,
}

impl Default for CrushSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_reduction: 0.1,
        }
    }
}

/// 图片下采样（上游 `config.imageCompression`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageCompressionSettings {
    pub enabled: bool,
    pub min_tokens: i64,
    pub max_dimension: u32,
    pub quality: u8,
    pub format: String,
}

impl Default for ImageCompressionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_tokens: 512,
            max_dimension: 1_280,
            quality: 80,
            format: "webp".to_string(),
        }
    }
}

/// CCR（内容缓存取回）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CcrSettings {
    pub enabled: bool,
    pub tool_name: String,
    pub min_tool_tokens: i64,
    pub exclude_tools: Vec<String>,
    pub max_head_chars: usize,
    pub retrieve_inline_tokens: i64,
}

impl Default for CcrSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tool_name: "acp_retrieve".to_string(),
            min_tool_tokens: 4_000,
            exclude_tools: Vec::new(),
            max_head_chars: 96,
            retrieve_inline_tokens: 4_000,
        }
    }
}

/// 输出引导（上游 `config.outputSteering`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OutputSteeringSettings {
    pub enabled: bool,
    pub verbosity_level: i64,
    pub effort_routing: bool,
}

impl Default for OutputSteeringSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            verbosity_level: 2,
            effort_routing: true,
        }
    }
}

/// 持久规则上限（上游 `config.rules`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RulesSettings {
    pub max_rules: usize,
    pub max_rule_chars: usize,
}

impl Default for RulesSettings {
    fn default() -> Self {
        Self {
            max_rules: 50,
            max_rule_chars: 300,
        }
    }
}

/// 内核配置：默认值即当前行为，可由 API 档案 `snowcfg.contextKernel` 覆盖。
///
/// 解析失败、类型不符或越界一律回落到默认值（validate 后使用），保证内核
/// 不会因为配置写错而改变出站行为。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KernelConfig {
    /// 受保护的尾部消息数。
    pub tail_protected_messages: usize,
    /// 单会话顶层块上限。
    pub max_blocks: usize,
    /// 工具结果超过该字符数即进入 CCR（占位 / 类型化压缩）。
    pub ccr_min_chars: usize,
    /// content store 的字符上限（超出按 ref 顺序淘汰）。
    pub ccr_store_max_chars: usize,
    /// 超过该字符数的结果先做类型化压缩（crush）而不是纯占位。
    pub crush_min_chars: usize,
    /// 出站视图是否渲染 `[ref]` 前缀。
    pub render_refs: bool,
    /// 折叠范围是否按轮对齐。
    pub snap_to_turns: bool,
    /// 建议区间的最小 token 体量。
    pub viable_range_min_tokens: i64,
    /// 是否把输出引导（verbosity directive）附加到 nudge。
    pub output_steering: bool,
    /// 覆盖 nudge 正文（多语言 / 团队规范）。
    pub nudge_override: String,

    // ---- 上游 `Config` 对齐段 ----
    pub tiers: TiersConfig,
    pub nudge: NudgeConfig,
    pub truncate: TruncateConfig,
    pub compress: CompressLimits,
    pub protected_tools: Vec<String>,
    pub protected_latest_tools: Vec<String>,
    pub preserve_recent_tools: Vec<String>,
    pub preserve_recent_messages: usize,
    pub preserve_recent_tokens: i64,
    pub model_context_limit: i64,
    pub absorb: AbsorbSettings,
    pub crush: CrushSettings,
    pub image_compression: ImageCompressionSettings,
    pub ccr: CcrSettings,
    /// 上游 `config.outputSteering`（JSON 键名 `outputSteeringSettings`）。
    pub output_steering_settings: OutputSteeringSettings,
    pub rules: RulesSettings,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            tail_protected_messages: TAIL_PROTECTED_MESSAGES,
            max_blocks: MAX_BLOCKS,
            ccr_min_chars: CCR_MIN_CHARS,
            ccr_store_max_chars: CCR_STORE_MAX_CHARS,
            crush_min_chars: CRUSH_MIN_CHARS,
            render_refs: RENDER_REFS,
            snap_to_turns: SNAP_TO_TURNS,
            viable_range_min_tokens: VIABLE_RANGE_MIN_TOKENS,
            output_steering: false,
            nudge_override: String::new(),
            tiers: TiersConfig::default(),
            nudge: NudgeConfig::default(),
            truncate: TruncateConfig::default(),
            compress: CompressLimits::default(),
            protected_tools: Vec::new(),
            protected_latest_tools: Vec::new(),
            preserve_recent_tools: Vec::new(),
            preserve_recent_messages: 5,
            preserve_recent_tokens: 5_000,
            model_context_limit: 200_000,
            absorb: AbsorbSettings::default(),
            crush: CrushSettings::default(),
            image_compression: ImageCompressionSettings::default(),
            ccr: CcrSettings::default(),
            output_steering_settings: OutputSteeringSettings::default(),
            rules: RulesSettings::default(),
        }
    }
}

impl KernelConfig {
    /// 收敛到安全范围：越界值只会让内核变保守，不会突破保护规则。
    fn validated(mut self) -> Self {
        self.tail_protected_messages = self.tail_protected_messages.clamp(1, 64);
        self.max_blocks = self.max_blocks.clamp(1, 512);
        self.ccr_min_chars = self.ccr_min_chars.clamp(1_000, 200_000);
        self.ccr_store_max_chars = self.ccr_store_max_chars.clamp(10_000, 4_000_000);
        self.crush_min_chars = self.crush_min_chars.clamp(self.ccr_min_chars, 500_000);
        self.viable_range_min_tokens = self.viable_range_min_tokens.clamp(100, 200_000);
        self.nudge_override = self.nudge_override.trim().to_string();
        self.model_context_limit = self.model_context_limit.clamp(1_000, 20_000_000);
        self.preserve_recent_messages = self.preserve_recent_messages.clamp(1, 64);
        self.preserve_recent_tokens = self.preserve_recent_tokens.clamp(0, 1_000_000);
        self.compress.min_compress_range = self.compress.min_compress_range.clamp(100, 1_000_000);
        self.compress.min_summary_length = self.compress.min_summary_length.clamp(10, 10_000);
        self.compress.max_summary_length = self
            .compress
            .max_summary_length
            .clamp(self.compress.min_summary_length, 500_000);
        self.rules.max_rules = self.rules.max_rules.clamp(1, 500);
        self.rules.max_rule_chars = self.rules.max_rule_chars.clamp(16, 8_000);
        self.absorb.min_tool_tokens = self.absorb.min_tool_tokens.clamp(100, 2_000_000);
        self.ccr.min_tool_tokens = self.ccr.min_tool_tokens.clamp(100, 2_000_000);
        self.ccr.retrieve_inline_tokens = self.ccr.retrieve_inline_tokens.clamp(100, 2_000_000);
        self.ccr.max_head_chars = self.ccr.max_head_chars.clamp(16, 4_096);
        self.image_compression.min_tokens = self.image_compression.min_tokens.clamp(16, 1_000_000);
        self.image_compression.max_dimension = self.image_compression.max_dimension.clamp(64, 8_192);
        self.image_compression.quality = self.image_compression.quality.clamp(10, 100);
        self.crush.min_reduction = self.crush.min_reduction.clamp(0.0, 0.95);
        self.output_steering_settings.verbosity_level =
            self.output_steering_settings.verbosity_level.clamp(0, 4);
        if self.tiers.tier2_trigger < 1 {
            self.tiers.tier2_trigger = 1;
        }
        if self.tiers.tier3_trigger < self.tiers.tier2_trigger {
            self.tiers.tier3_trigger = self.tiers.tier2_trigger;
        }
        self
    }

    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 解析当前生效 API 档案里的 `snowcfg.contextKernel` 配置对象。
pub fn resolve_config() -> KernelConfig {
    let Ok(context) = crate::api::config::get_active_api_request_context() else {
        return KernelConfig::default();
    };
    let Ok(root) = serde_json::from_str::<Value>(&context.api_config.config_json) else {
        return KernelConfig::default();
    };
    let Some(section) = root
        .get("snowcfg")
        .and_then(|snowcfg| snowcfg.get("contextKernel"))
    else {
        return KernelConfig::default();
    };
    serde_json::from_value::<KernelConfig>(section.clone())
        .map(KernelConfig::validated)
        .unwrap_or_default()
}

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
    /// 折叠收益：被覆盖消息的 token 估算 / 摘要消息的 token 估算。
    pub tokens_before: i64,
    pub tokens_after: i64,
    /// absorb 产生的块：记录「已完成工作」而非普通历史摘要。
    pub absorbed: bool,
    pub created_at: String,
    /// 创建时被覆盖消息的展示跨度（`m00044` / `m00097`）；老块为空，运行时回落推算。
    pub start_ref: String,
    pub end_ref: String,
    /// 摘要的 token / 字符规模（折叠经济学用）。
    pub summary_tokens: i64,
    pub summary_chars: usize,
    /// 存活轮数（`advanceSurvival`）：达到 `promotionThreshold` 后可被升层。
    pub survival: u32,
    /// 内容曾被内联恢复（`decompress` 的 refold-in-place 标记）。
    pub restored_inline: bool,
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
    /// 用户/项目级折叠规则（acp_rule）：作为压缩约束注入 nudge 与摘要要求。
    pub rules: Vec<String>,
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
            rules: Vec::new(),
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
    /// 语义类型：user / assistant / tool-call / tool-result / media / internal / system。
    pub kind: String,
    pub preview: String,
    pub tokens: i64,
}

impl MessageRef {
    fn to_json(&self, covered_by: Option<&str>) -> Value {
        json!({
            "ref": self.id,
            "role": self.role,
            "kind": self.kind,
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
                kind: message_kind(message).to_string(),
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

    fn tail_protected_from(&self, cfg: &KernelConfig) -> usize {
        self.messages
            .len()
            .saturating_sub(cfg.tail_protected_messages)
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
pub fn has_media_payload(message: &ChatContextMessage) -> bool {
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

fn nudge_message(
    estimate: i64,
    threshold: i64,
    highest_tier: u8,
    rules: &[String],
    steering: Option<(&'static str, &'static str)>,
    cfg: &KernelConfig,
) -> ChatContextMessage {
    let tier_hint = if highest_tier >= MAX_TIER {
        "All active blocks are already tier 3 (the deepest layer); compress a wider span instead of re-folding them."
    } else if highest_tier >= 1 {
        "Blocks already exist: folding a range that fully contains them is allowed and produces a higher tier (2 → 3), which is the right move when the window is still tight."
    } else {
        "Pick a contiguous run of older messages and fold it into a tier-1 summary."
    };
    let rules_section = if rules.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nFOLDING RULES (binding — honour them in every summary):\n{}",
            rules
                .iter()
                .map(|rule| format!("- {rule}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let steering_section = match steering {
        Some((kind, directive)) => format!("\n\nOUTPUT STEERING ({kind}): {directive}"),
        None => String::new(),
    };
    let content = if cfg.nudge_override.is_empty() {
        format!(
            "{NUDGE_PREFIX}\n\nThe outbound conversation is about {estimate} tokens (auto-compress threshold: {threshold}). Fold older history now instead of letting the request overflow:\n\n1. Call `context-kernel-acp_status` to get message refs (`m...`), the active blocks and `suggestedRanges` (turn-aligned, already checked against every protection rule).\n2. Call `context-kernel-compress` with `from` / `to` refs and a self-contained `summary` that preserves decisions, file paths, failures, and open work. Write the summary in the conversation's language.\n3. {tier_hint}\n4. Work that is finished and no longer needs re-verification can go through `context-kernel-absorb` instead (completion-state summary).\n5. Protected and rejected automatically: the pinned first user message, the most recent turns, media messages, context-kernel tool messages, and any range that splits a tool call/result pair.\n6. Use `context-kernel-search_context` when a folded detail is needed again, `context-kernel-retrieve` for a stored tool output, and `context-kernel-decompress` to restore a block.\n\nOnly the request view is folded — the user still sees the full transcript.{rules_section}{steering_section}"
        )
    } else {
        // 档案覆盖了 nudge 正文：仍附上规则与引导，保证约束不丢。
        format!(
            "{NUDGE_PREFIX}\n\n{estimate} / {threshold} tokens.\n\n{}{rules_section}{steering_section}",
            cfg.nudge_override
        )
    };
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

fn store_original(state: &mut KernelState, kind: &str, text: &str, max_chars: usize) -> String {
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
    while state.stored_chars() > max_chars {
        let Some(oldest) = state.store.keys().next().cloned() else {
            break;
        };
        state.store.remove(&oldest);
    }
    ref_id
}

/// 把超长工具结果替换为占位符 / 类型化压缩视图。只改出站副本，落库内容不动。
fn apply_ccr(
    messages: &mut [ChatContextMessage],
    state: &mut KernelState,
    cfg: &KernelConfig,
) -> usize {
    let mut replaced = 0usize;
    for message in messages.iter_mut() {
        if message.role.trim() != "tool" {
            continue;
        }
        if !message.content.trim().is_empty()
            && !message.content.starts_with(CCR_MARKER)
            && message.content.chars().count() >= cfg.ccr_min_chars
        {
            let original = message.content.clone();
            let ref_id = store_original(state, "tool_text", &original, cfg.ccr_store_max_chars);
            message.content = ccr_view(&ref_id, "tool_text", &original, cfg);
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
            if result.starts_with(CCR_MARKER) || result.chars().count() < cfg.ccr_min_chars {
                continue;
            }
            let ref_id = store_original(state, "tool_result", &result, cfg.ccr_store_max_chars);
            entry["result"] = Value::String(ccr_view(&ref_id, "tool_result", &result, cfg));
            changed = true;
            replaced += 1;
        }
        if changed {
            message.tool_results_json = Some(value.to_string());
        }
    }
    replaced
}

/// 出站占位文本：小结果用纯占位（省 token），超大结果先做类型化压缩（保信息）。
fn ccr_view(ref_id: &str, kind: &str, text: &str, cfg: &KernelConfig) -> String {
    let chars = text.chars().count();
    if chars < cfg.crush_min_chars {
        return ccr_placeholder(ref_id, kind, text);
    }
    let (crushed, crushed_kind) = crush_tool_result(text);
    format!(
        "{CCR_MARKER} ref={ref_id} kind={crushed_kind} chars={chars}]\n{crushed}\n\n> Crushed view: structure and head/tail kept, the full output is stored out of band. Call `context-kernel-retrieve` with ref \"{ref_id}\" for the original."
    )
}

/// 类型化压缩：超长工具输出保留结构信息（JSON 结构 / 日志首尾），而不是直接丢弃。
fn crush_tool_result(text: &str) -> (String, &'static str) {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(crushed) = crush_json(&value) {
                return (crushed, "json");
            }
        }
    }
    (crush_head_tail(text), "text")
}

fn crush_json(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return None;
            }
            let sample: Vec<String> = items
                .iter()
                .take(CRUSH_SAMPLE_ITEMS)
                .map(|item| truncate_chars(&item.to_string(), 400))
                .collect();
            Some(format!(
                "[json-array] {} item(s); first {}: {}{}",
                items.len(),
                sample.len(),
                sample.join(" | "),
                if items.len() > sample.len() { "; …" } else { "" }
            ))
        }
        Value::Object(map) => {
            if map.is_empty() {
                return None;
            }
            let keys: Vec<&str> = map.keys().take(30).map(String::as_str).collect();
            let preview = map
                .iter()
                .take(CRUSH_SAMPLE_ITEMS)
                .map(|(key, value)| format!("{key}={}", truncate_chars(&value.to_string(), 200)))
                .collect::<Vec<_>>()
                .join(" | ");
            Some(format!(
                "[json-object] {} key(s): {}; sample: {}",
                map.len(),
                keys.join(", "),
                preview
            ))
        }
        _ => None,
    }
}

fn crush_head_tail(text: &str) -> String {
    let total = text.chars().count();
    let head: String = text.chars().take(CRUSH_HEAD_CHARS).collect();
    let tail: String = {
        let tail_chars: Vec<char> = text.chars().rev().take(CRUSH_TAIL_CHARS).collect();
        tail_chars.into_iter().rev().collect()
    };
    let omitted = total.saturating_sub(CRUSH_HEAD_CHARS + CRUSH_TAIL_CHARS);
    format!("{head}\n… {omitted} chars omitted …\n{tail}")
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// 隐藏已被消费的内核压缩/吸收调用：块仍有效时，把它的 assistant tool_call 与
/// 对应 tool 结果**成对**移出出站视图（保留配对，provider 不会看到孤立 tool-call）。
/// 摘要本身已经在块里，这些调用只剩重复 token。
fn hide_consumed(messages: Vec<ChatContextMessage>, state: &KernelState) -> Vec<ChatContextMessage> {
    let active: HashSet<&str> = state.blocks.iter().map(|block| block.id.as_str()).collect();
    if active.is_empty() {
        return messages;
    }
    let mut consumed: HashSet<String> = HashSet::new();
    for message in messages.iter() {
        if message.role.trim() != "tool" {
            continue;
        }
        let Some(raw) = message.tool_results_json.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(entries) = value.as_array() else {
            continue;
        };
        for entry in entries {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            if !matches!(
                name,
                "context-kernel-compress" | "context-kernel-absorb" | "context-kernel-decompress"
            ) {
                continue;
            }
            let block_id = entry
                .get("result")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|parsed| {
                    parsed
                        .get("blockId")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .unwrap_or_default();
            if !block_id.is_empty() && active.contains(block_id.as_str()) {
                if let Some(call_id) = entry.get("callId").and_then(Value::as_str) {
                    consumed.insert(call_id.to_string());
                }
            }
        }
    }
    if consumed.is_empty() {
        return messages;
    }
    messages
        .into_iter()
        .filter(|message| match message.role.trim() {
            "tool" => {
                let Some(raw) = message.tool_results_json.as_deref() else {
                    return true;
                };
                let Ok(value) = serde_json::from_str::<Value>(raw) else {
                    return true;
                };
                let Some(entries) = value.as_array() else {
                    return true;
                };
                !(!entries.is_empty()
                    && entries.iter().all(|entry| {
                        entry
                            .get("callId")
                            .and_then(Value::as_str)
                            .map(|id| consumed.contains(id))
                            .unwrap_or(false)
                    }))
            }
            "assistant" => {
                let Some(raw) = message.tool_calls_json.as_deref() else {
                    return true;
                };
                let Ok(value) = serde_json::from_str::<Value>(raw) else {
                    return true;
                };
                let Some(entries) = value.as_array() else {
                    return true;
                };
                !(!entries.is_empty()
                    && entries.iter().all(|entry| {
                        let id = entry
                            .get("id")
                            .and_then(Value::as_str)
                            .or_else(|| entry.get("callId").and_then(Value::as_str))
                            .unwrap_or("");
                        consumed.contains(id)
                    }))
            }
            _ => true,
        })
        .collect()
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

/// 一轮对话的 [start, end] 下标区间：user/system 开启新轮，assistant 与其
/// 工具调用/结果属于同一轮。折叠必须按轮对齐，否则会把一次交互切成两半。
fn turn_spans(messages: &[ChatContextMessage]) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, message) in messages.iter().enumerate() {
        let opens_turn = matches!(message.role.trim(), "user" | "system" | "developer");
        if opens_turn {
            if let Some(previous) = start.take() {
                if previous < index {
                    spans.push((previous, index - 1));
                }
            }
            start = Some(index);
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(previous) = start {
        spans.push((previous, messages.len().saturating_sub(1)));
    }
    spans.retain(|(from, to)| from <= to);
    spans
}

/// 把范围向外扩张到完整的轮边界（fold 至少覆盖整轮）。
fn expand_to_turn_bounds(
    messages: &[ChatContextMessage],
    start: usize,
    end: usize,
    cfg: &KernelConfig,
) -> (usize, usize, bool) {
    if !cfg.snap_to_turns {
        return (start, end, false);
    }
    let mut low = start;
    let mut high = end;
    for (from, to) in turn_spans(messages) {
        if to >= low && from <= high {
            low = low.min(from);
            high = high.max(to);
        }
    }
    (low, high, low != start || high != end)
}

/// 候选折叠区间：按轮对齐、排除 pin/尾部/受保护消息，体量达标才返回。
///
/// 允许区间包含已有块（那是 tier 升级合并的场景），但要求至少含一条未覆盖
/// 消息或一个可完整吞掉的块，否则折了也没有收益。
fn viable_ranges(
    view: &HistoryView,
    state: &KernelState,
    cfg: &KernelConfig,
) -> Vec<(usize, usize)> {
    let covered = covered_lookup(state);
    let pinned = view.pinned_index();
    let tail_from = view.tail_protected_from(cfg);
    let usable: Vec<bool> = view
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if compression_blocked_reason(message).is_some() {
                return false;
            }
            pinned != Some(index) && index < tail_from
        })
        .collect();

    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    for (from, to) in turn_spans(&view.messages) {
        if (from..=to).all(|index| usable[index]) {
            current = Some(match current {
                Some((run_start, _)) => (run_start, to),
                None => (from, to),
            });
        } else if let Some(run) = current.take() {
            runs.push(run);
        }
    }
    if let Some(run) = current {
        runs.push(run);
    }

    runs.into_iter()
        .filter(|(from, to)| {
            if to - from + 1 < MIN_COMPRESS_MESSAGES {
                return false;
            }
            let tokens: i64 = view.refs[*from..=*to].iter().map(|item| item.tokens).sum();
            if tokens < cfg.viable_range_min_tokens {
                return false;
            }
            let has_new = (*from..=*to).any(|index| !covered.contains_key(&view.refs[index].id));
            let has_mergeable_block = state
                .blocks
                .iter()
                .filter(|block| block.folded_into.is_empty())
                .any(|block| {
                    let mut indices: Vec<usize> = block
                        .message_ids
                        .iter()
                        .filter_map(|id| view.positions.get(id).copied())
                        .collect();
                    if indices.is_empty() {
                        return false;
                    }
                    indices.sort_unstable();
                    indices[0] >= *from && indices[indices.len() - 1] <= *to
                });
            has_new || has_mergeable_block
        })
        .take(MAX_SUGGESTED_RANGES)
        .collect()
}

/// 一个区间跨了多少轮（用于状态展示与模型决策）。
fn turns_in_range(messages: &[ChatContextMessage], start: usize, end: usize) -> usize {
    turn_spans(messages)
        .into_iter()
        .filter(|(from, to)| *to >= start && *from <= end)
        .count()
}

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

/// 状态持久化 + 折叠日志（append-only journal）。
///
/// journal 是状态的重建来源：状态行丢失、损坏、或历史被回滚/分叉时，按事件
/// 顺序重放 fold / merge / decompress 即可重建 `CompressionState`。
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
    drop(connection);
    if let Some(raw) = raw {
        if let Ok(mut state) = serde_json::from_str::<KernelState>(&raw) {
            if state.version == KERNEL_VERSION {
                if state.next_block_id == 0 {
                    state.next_block_id = state.blocks.len() as u64 + 1;
                }
                return Ok(state);
            }
        }
    }
    // 状态行缺失 / 版本不符 / 解析失败：按折叠日志重建，并把结果写回。
    let rebuilt = rebuild_state_from_journal(database_path, conversation_id, sub_agent_id)?;
    if !rebuilt.blocks.is_empty() {
        let _ = save_state(database_path, conversation_id, sub_agent_id, &rebuilt);
        eprintln!(
            "context kernel: rebuilt {} block(s) from the fold journal",
            rebuilt.blocks.len()
        );
    }
    Ok(rebuilt)
}

/// 按 `context_kernel_folds` 顺序重放，重建状态。
pub fn rebuild_state_from_journal(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
) -> Result<KernelState> {
    let mut state = KernelState::fresh();
    if conversation_id.trim().is_empty() {
        return Ok(state);
    }
    let connection = database::open_connection(database_path)
        .map_err(|error| database::database_error(database_path, "open context kernel journal", error))?;
    let mut statement = connection
        .prepare(
            "SELECT event, block_id, parent_block_id, tier, title, summary, message_ids_json,
                    tokens_before, tokens_after, created_at
               FROM context_kernel_folds
              WHERE conversation_id = ?1 AND sub_agent_id = ?2
              ORDER BY id ASC",
        )
        .map_err(|error| database::database_error(database_path, "prepare kernel journal", error))?;
    let rows = statement
        .query_map(rusqlite::params![conversation_id, sub_agent_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
            ))
        })
        .map_err(|error| database::database_error(database_path, "query kernel journal", error))?;
    for row in rows {
        let (event, block_id, parent_block_id, tier, title, summary, ids_json, before, after, created_at) =
            row.map_err(|error| {
                database::database_error(database_path, "read kernel journal", error)
            })?;
        match event.as_str() {
            "fold" | "merge" | "absorb" => {
                let message_ids: Vec<String> = serde_json::from_str(&ids_json).unwrap_or_default();
                if message_ids.len() < MIN_COMPRESS_MESSAGES {
                    continue;
                }
                if !parent_block_id.is_empty() {
                    if let Some(child) = state.blocks.iter_mut().find(|block| block.id == block_id) {
                        child.folded_into = parent_block_id.clone();
                        continue;
                    }
                }
                if state.blocks.iter().any(|block| block.id == block_id) {
                    continue;
                }
                let next = state
                    .blocks
                    .iter()
                    .filter_map(|block| {
                        block
                            .id
                            .strip_prefix('b')
                            .and_then(|value| value.parse::<u64>().ok())
                    })
                    .max()
                    .unwrap_or(0)
                    .max(state.next_block_id.saturating_sub(1));
                state.next_block_id = next + 2;
                state.blocks.push(KernelBlock {
                    id: block_id,
                    title,
                    summary,
                    message_ids,
                    tier: tier.clamp(1, MAX_TIER as i64) as u8,
                    children: Vec::new(),
                    folded_into: String::new(),
                    tokens_before: before,
                    tokens_after: after,
                    absorbed: event == "absorb",
                    created_at,
                    ..KernelBlock::default()
                });
            }
            "decompress" => {
                let children: Vec<String> = state
                    .blocks
                    .iter()
                    .filter(|block| block.folded_into == block_id)
                    .map(|block| block.id.clone())
                    .collect();
                state.blocks.retain(|block| block.id != block_id);
                for child_id in children {
                    if let Some(child) = state.blocks.iter_mut().find(|block| block.id == child_id) {
                        child.folded_into.clear();
                    }
                }
            }
            _ => {}
        }
    }
    // 父子关系补齐：父块的 children 由 folded_into 反推。
    let links: Vec<(String, String)> = state
        .blocks
        .iter()
        .filter(|block| !block.folded_into.is_empty())
        .map(|block| (block.folded_into.clone(), block.id.clone()))
        .collect();
    for (parent_id, child_id) in links {
        if let Some(parent) = state.blocks.iter_mut().find(|block| block.id == parent_id) {
            if !parent.children.contains(&child_id) {
                parent.children.push(child_id);
            }
        }
    }
    Ok(state)
}

/// 追加一条折叠日志（失败不阻断工具执行：journal 只是重建来源）。
fn journal_append(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    event: &str,
    block: &KernelBlock,
    parent_block_id: &str,
) {
    let Ok(connection) = database::open_connection(database_path) else {
        return;
    };
    let ids_json = serde_json::to_string(&block.message_ids).unwrap_or_else(|_| "[]".to_string());
    let _ = connection.execute(
        "INSERT INTO context_kernel_folds
            (id, conversation_id, sub_agent_id, event, block_id, parent_block_id, tier, title,
             summary, message_ids_json, tokens_before, tokens_after, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, datetime('now', 'localtime'))",
        rusqlite::params![
            database::create_snowflake_id(),
            conversation_id,
            sub_agent_id,
            event,
            block.id,
            parent_block_id,
            i64::from(block.tier),
            block.title,
            block.summary,
            ids_json,
            block.tokens_before,
            block.tokens_after,
        ],
    );
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
///
/// `render_refs` 打开时给 user/assistant 文本前缀 `[ref]`（renderTags:
/// text-only），模型无需调用 acp_status 就能引用消息；工具消息不动，避免破坏
/// 结果解析。
fn fold(
    messages: &[ChatContextMessage],
    state: &KernelState,
    cfg: &KernelConfig,
) -> (Vec<ChatContextMessage>, i64) {
    let render_refs = cfg.render_refs;
    let view = HistoryView::build(messages.to_vec());
    let pairs = tool_pair_index(&view.messages);
    let protection = protection_config(cfg);
    let protected_calls = protection::collect_protected_tool_call_ids(&view.messages, &protection);
    let blocked: Vec<bool> = view
        .messages
        .iter()
        .map(|message| {
            compression_blocked_reason(message).is_some()
                || protection::is_message_protected_with_pairing(
                    message,
                    &protection,
                    &protected_calls,
                )
        })
        .collect();
    let ranges = block_ranges(state, &view.positions, &pairs, &blocked);
    if ranges.is_empty() {
        let visible = hide_consumed(view.messages.clone(), state);
        let decorated = decorate_refs(&view, Some(visible), render_refs);
        let tokens: i64 = decorated.iter().map(estimate_message_tokens).sum();
        return (decorated, tokens);
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
    let visible = hide_consumed(folded, state);
    let decorated = decorate_refs(&view, Some(visible), render_refs);
    let tokens: i64 = decorated.iter().map(estimate_message_tokens).sum();
    (decorated, tokens)
}

/// 给未折叠的文本消息加 `[ref]` 前缀（幂等：已带同前缀的不重复加）。
///
/// 只处理 user/assistant 文本消息；工具消息与内核注入的内部消息（摘要、
/// nudge、宿主事实）不挂 ref，避免破坏结果解析或污染内部文本。
fn decorate_refs(
    view: &HistoryView,
    folded: Option<Vec<ChatContextMessage>>,
    render_refs: bool,
) -> Vec<ChatContextMessage> {
    let mut messages = match folded {
        Some(folded) => folded,
        None => view.messages.clone(),
    };
    if !render_refs {
        return messages;
    }
    // 折叠后的视图与原始下标不再一一对应，按内容指纹重新匹配引用。
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut lookup: HashMap<String, String> = HashMap::new();
    for message in view.messages.iter() {
        let id = message_id(message, &mut occurrences);
        lookup.insert(fingerprint(message), id);
    }
    for message in messages.iter_mut() {
        if !matches!(message.role.trim(), "user" | "assistant") {
            continue;
        }
        if message.origin == ORIGIN_INTERNAL || message.content.trim().is_empty() {
            continue;
        }
        let Some(id) = lookup.get(&fingerprint(message)) else {
            continue;
        };
        let prefix = format!("[{id}] ");
        if !message.content.starts_with(&prefix) {
            message.content = format!("{prefix}{}", message.content);
        }
    }
    messages
}

/// JSON 数组且非空（判断 assistant 是否真的带工具调用）。
fn has_json_entries(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().map(|entries| !entries.is_empty()))
        .unwrap_or(false)
}

/// 一条消息的语义类型（refs / 检索结果里给模型看，避免它猜）。
fn message_kind(message: &ChatContextMessage) -> &'static str {
    if message.origin == ORIGIN_INTERNAL {
        return "internal";
    }
    match message.role.trim() {
        "system" | "developer" => "system",
        "tool" => "tool-result",
        "assistant" => {
            if has_media_payload(message) {
                "media"
            } else if message
                .tool_calls_json
                .as_deref()
                .is_some_and(has_json_entries)
            {
                "tool-call"
            } else {
                "assistant"
            }
        }
        "user" => {
            if has_media_payload(message) {
                "media"
            } else {
                "user"
            }
        }
        _ => "other",
    }
}

/// 输出引导（output-steering 的决策部分）：按最近轮次性质给出 verbosity 指令。
/// 默认关闭，只有档案里显式打开 `outputSteering` 才会写进 nudge。
fn steering_directive(messages: &[ChatContextMessage]) -> Option<(&'static str, &'static str)> {
    let recent: Vec<&ChatContextMessage> = messages.iter().rev().take(30).collect();
    if recent.is_empty() {
        return None;
    }
    let tool_heavy = recent
        .iter()
        .filter(|message| message.role.trim() == "tool")
        .count();
    let media = recent
        .iter()
        .filter(|message| has_media_payload(message))
        .count();
    if tool_heavy >= 8 {
        Some((
            "tool_heavy",
            "Tool-heavy stretch: keep replies to the decision and the next action; do not restate tool output.",
        ))
    } else if media > 0 {
        Some((
            "visual",
            "Media in play: refer to images by their file path instead of describing them at length.",
        ))
    } else {
        None
    }
}

/// 用于 ref 匹配的稳定指纹（角色 + 正文 + 工具负载）。
fn fingerprint(message: &ChatContextMessage) -> String {
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
    hasher.finalize().to_hex().to_string()
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
    let cfg = resolve_config();
    let positions = HistoryView::build(messages.clone()).positions;
    let mut changed = prune_stale_blocks(&mut state, &positions);

    let (mut folded, mut tokens) = fold(messages, &state, &cfg);
    // CCR：超长工具结果在出站视图里换成冻结占位符，原文进入 content store。
    if apply_ccr(&mut folded, &mut state, &cfg) > 0 {
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
            let steering = if cfg.output_steering || cfg.output_steering_settings.enabled {
                let structural = steering::structural_summary(&folded);
                let decision = steering::decide_output_steering(
                    &structural,
                    &steering::OutputSteeringConfig {
                        enabled: true,
                        verbosity_level: cfg.output_steering_settings.verbosity_level,
                        effort_routing: cfg.output_steering_settings.effort_routing,
                    },
                );
                texts::verbosity_directive(decision.verbosity_level)
                    .map(|text| (decision.turn_kind.as_str(), text))
                    .or_else(|| steering_directive(&folded))
            } else {
                None
            };
            folded.push(nudge_message(
                effective,
                threshold,
                highest_tier,
                &state.rules,
                steering,
                &cfg,
            ));
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
    compress_range(
        database_path,
        conversation_id,
        sub_agent_id,
        args,
        "compress",
    )
}

/// `context-kernel-absorb`: 把「已经完成的工作」折叠成一条 absorbed 块。
/// 机制与 compress 相同，但摘要必须以完成态陈述（done / verified / 产物），
/// 便于后续轮次直接信任而不重新验证。
fn execute_absorb(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    compress_range(
        database_path,
        conversation_id,
        sub_agent_id,
        args,
        "absorb",
    )
}

/// 折叠核心：`kind` 区分普通压缩与 absorb（写进块标记与折叠日志）。
fn compress_range(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
    kind: &str,
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
    let absorbed = kind == "absorb";
    let default_title = if absorbed {
        "absorbed completed work"
    } else {
        "compressed history"
    };
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default_title)
        .to_string();

    let messages = load_history(database_path, conversation_id)?;
    let view = HistoryView::build(messages);
    let cfg = resolve_config();
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
    // 按轮对齐：范围向外扩张到完整的轮，避免把一次交互切成两半。
    let (start, end, snapped) = expand_to_turn_bounds(&view.messages, start, end, &cfg);
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
    let tail_from = view.tail_protected_from(&cfg);
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
    if active_top >= cfg.max_blocks {
        return Err(Error::new(
            Status::InvalidArg,
            format!("At most {} blocks per conversation. Decompress or merge existing blocks first.", cfg.max_blocks),
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
    let mut placeholder = KernelBlock {
        id: id.clone(),
        title,
        summary,
        message_ids: target_ids.clone(),
        tier,
        children: children.iter().map(|child| child.id.clone()).collect(),
        folded_into: String::new(),
        tokens_before,
        tokens_after: 0,
        absorbed,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        start_ref: view.refs[start].id.clone(),
        end_ref: view.refs[end].id.clone(),
        summary_tokens: 0,
        summary_chars: 0,
        survival: 0,
        restored_inline: false,
    };
    placeholder.summary_chars = placeholder.summary.chars().count();
    placeholder.tokens_after = estimate_message_tokens(&summary_message(&placeholder, target_ids.len()));
    placeholder.summary_tokens = placeholder.tokens_after;
    let block = placeholder;
    let summary_tokens = block.tokens_after;
    state.blocks.push(block.clone());
    state.updated_at = block.created_at.clone();
    save_state(database_path, conversation_id, sub_agent_id, &state)?;
    // 折叠日志：先写被合并的子块，再写父块，保证重放顺序确定。
    for child in children.iter() {
        journal_append(database_path, conversation_id, sub_agent_id, "merge", child, &id);
    }
    let fold_event = if absorbed { "absorb" } else { "fold" };
    journal_append(database_path, conversation_id, sub_agent_id, fold_event, &block, "");
    Ok(json!({
        "success": true,
        "blockId": id,
        "kind": kind,
        "absorbed": absorbed,
        "tier": tier,
        "mergedBlocks": child_ids,
        "snappedToTurns": snapped,
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
    journal_append(
        database_path,
        conversation_id,
        sub_agent_id,
        "decompress",
        &block,
        "",
    );
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

/// `context-kernel-acp_rule`: 折叠规则的增删查（作为压缩约束注入 nudge）。
fn execute_rule(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let mut changed = false;
    match action.as_str() {
        "list" => {}
        "add" => {
            let raw = args
                .get("rule")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        Status::InvalidArg,
                        "Missing required parameter: rule (the constraint text to remember)",
                    )
                })?;
            let rule = truncate_chars(raw, MAX_RULE_CHARS);
            if !state.rules.iter().any(|existing| existing == &rule) {
                if state.rules.len() >= MAX_RULES {
                    return Err(Error::new(
                        Status::InvalidArg,
                        format!("At most {MAX_RULES} folding rules; remove one first."),
                    ));
                }
                state.rules.push(rule);
                changed = true;
            }
        }
        "remove" => {
            let rule_arg = args
                .get("rule")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let index_arg = args
                .get("index")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            let removed = if let Some(rule) = rule_arg {
                let before = state.rules.len();
                state.rules.retain(|existing| existing != rule);
                before - state.rules.len()
            } else if let Some(index) = index_arg {
                if index < state.rules.len() {
                    state.rules.remove(index);
                    1
                } else {
                    0
                }
            } else {
                return Err(Error::new(
                    Status::InvalidArg,
                    "action=remove needs either `rule` (exact text) or `index` (from action=list).",
                ));
            };
            if removed == 0 {
                return Err(Error::new(
                    Status::InvalidArg,
                    "No matching folding rule found; call action=list for the current list.",
                ));
            }
            changed = true;
        }
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("Unknown action: {other} (list | add | remove)"),
            ))
        }
    }
    if changed {
        state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        save_state(database_path, conversation_id, sub_agent_id, &state)?;
    }
    Ok(json!({
        "success": true,
        "action": action,
        "count": state.rules.len(),
        "maxRules": MAX_RULES,
        "rules": state.rules,
    }))
}

/// `context-kernel-acp_cache`: 折叠经济学报告（省了多少、按层分布、CCR 占比）。
fn execute_cache(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    threshold_tokens: Option<i64>,
    args: &Value,
) -> Result<Value> {
    let include_blocks = args
        .get("includeBlocks")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let messages = load_history(database_path, conversation_id)?;
    let mut state = load_state(database_path, conversation_id, sub_agent_id)?;
    let view = HistoryView::build(messages);
    let cfg = resolve_config();
    prune_stale_blocks(&mut state, &view.positions);
    let (folded, folded_tokens) = fold(&view.messages, &state, &cfg);
    let mut preview = folded.clone();
    let mut state_preview = state.clone();
    let outbound_tokens = if apply_ccr(&mut preview, &mut state_preview, &cfg) > 0 {
        preview.iter().map(estimate_message_tokens).sum()
    } else {
        folded_tokens
    };
    let top: Vec<&KernelBlock> = state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .collect();
    let saved_by_folding: i64 = top
        .iter()
        .map(|block| (block.tokens_before - block.tokens_after).max(0))
        .sum();
    let mut tier_breakdown = [0usize; (MAX_TIER + 1) as usize];
    for block in top.iter() {
        let index = usize::from(block.tier.min(MAX_TIER));
        tier_breakdown[index] += 1;
    }
    let raw_tokens = view.total_tokens();
    let ratio = if raw_tokens > 0 {
        ((outbound_tokens as f64 / raw_tokens as f64) * 100.0).round() / 100.0
    } else {
        1.0
    };
    let blocks_json: Vec<Value> = if include_blocks {
        top.iter()
            .map(|block| {
                json!({
                    "blockId": block.id,
                    "tier": block.tier,
                    "children": block.children,
                    "coveredMessages": block.message_ids.len(),
                    "tokensBefore": block.tokens_before,
                    "tokensAfter": block.tokens_after,
                    "tokensSaved": (block.tokens_before - block.tokens_after).max(0),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "success": true,
        "conversationId": conversation_id,
        "rawTokens": raw_tokens,
        "outboundTokens": outbound_tokens,
        "tokensSavedByFolding": saved_by_folding,
        "tokensSavedTotal": (raw_tokens - outbound_tokens).max(0),
        "compressionRatio": ratio,
        "observedTokens": latest_usage_tokens(database_path, conversation_id),
        "thresholdTokens": threshold_tokens.filter(|value| *value > 0),
        "activeBlocks": top.len(),
        "highestTier": top.iter().map(|block| block.tier).max().unwrap_or(0),
        "maxTier": MAX_TIER,
        "tierBreakdown": {
            "tier1": tier_breakdown[1],
            "tier2": tier_breakdown.get(2).copied().unwrap_or(0),
            "tier3": tier_breakdown.get(3).copied().unwrap_or(0),
        },
        "storedEntries": state.store.len(),
        "storedChars": state.stored_chars(),
        "blocks": blocks_json,
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
    let cfg = resolve_config();
    prune_stale_blocks(&mut state, &view.positions);
    let (mut folded, mut folded_tokens) = fold(&view.messages, &state, &cfg);
    // 状态里的 token 估算与出站预览都必须是「真实出站字节」：就地应用 CCR/crush。
    if apply_ccr(&mut folded, &mut state, &cfg) > 0 {
        folded_tokens = folded.iter().map(estimate_message_tokens).sum();
    }
    let covered = covered_lookup(&state);
    let tail_from = view.tail_protected_from(&cfg);
    let pinned = view.pinned_index();
    // 出站预览：折叠 + ref 渲染 + CCR/crush + 隐藏已消费调用之后的真实前几行，
    // 既给 UI 用，也让“出站字节到底长什么样”可被验证。
    let outbound_preview: Vec<Value> = folded
        .iter()
        .take(16)
        .map(|message| {
            json!({
                "role": message.role,
                "origin": message.origin,
                "tokens": estimate_message_tokens(message),
                "head": preview_of(&message.content, 160),
            })
        })
        .collect();
    // 建议区间：按轮对齐、排除受保护消息、体量达标，模型可以直接照抄。
    let suggested_json: Vec<Value> = viable_ranges(&view, &state, &cfg)
        .into_iter()
        .map(|(from, to)| {
            json!({
                "from": view.refs[from].id,
                "to": view.refs[to].id,
                "turns": turns_in_range(&view.messages, from, to),
                "messages": to - from + 1,
                "tokens": view.refs[from..=to].iter().map(|item| item.tokens).sum::<i64>(),
            })
        })
        .collect();

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
                "tokensBefore": block.tokens_before,
                "tokensAfter": block.tokens_after,
                "tokensSaved": (block.tokens_before - block.tokens_after).max(0),
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
    let tokens_saved_by_folding: i64 = blocks_json
        .iter()
        .map(|block| block.get("tokensSaved").and_then(Value::as_i64).unwrap_or(0))
        .sum();
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
        "tokensSavedByFolding": tokens_saved_by_folding,
        "thresholdTokens": threshold,
        "compressNeeded": threshold.map(|value| folded_tokens.max(observed.unwrap_or(0)) >= value).unwrap_or(false),
        "compressedMessages": covered.len(),
        "activeBlocks": blocks_json.len(),
        "highestTier": highest_tier,
        "maxTier": MAX_TIER,
        "storedEntries": state.store.len(),
        "storedChars": state.stored_chars(),
        "rules": state.rules,
        "config": cfg.to_json(),
        "absorbedBlocks": state
            .blocks
            .iter()
            .filter(|block| block.folded_into.is_empty() && block.absorbed)
            .count(),
        "blocks": blocks_json,
        "suggestedRanges": suggested_json,
        "outboundPreview": outbound_preview,
        "refs": refs_json,
        "protected": {
            "pinnedRef": pinned.and_then(|index| view.refs.get(index)).map(|item| item.id.clone()).unwrap_or_default(),
            "tailMessages": cfg.tail_protected_messages,
            "mediaMarkers": MEDIA_MARKERS,
            "kernelTools": KERNEL_TOOL_PREFIX,
        },
    }))
}

/// `context-kernel-recap`: 出站块的压缩回放（上游 `buildRecap`）。
///
/// Goal / WorkTree 模式与 UI 直接复用：现状 + 每个活动块的跨度、层数与已省 token。
fn execute_recap(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let state = load_state(database_path, conversation_id, sub_agent_id)?;
    let block_id = args
        .get("blockId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let text = rendering::build_recap(&state, block_id);
    Ok(json!({
        "success": true,
        "blockId": block_id.unwrap_or(""),
        "recap": text,
    }))
}

/// `context-kernel-handoff`: 交接文档（上游 `renderHandoff`）。
///
/// 供 Goal / WorkTree 模式在交接点生成一份可读快照；`full` 打开时连同被折叠段
/// 的原文一起渲染，`folded` 打开时按持久快照原样渲染。
fn execute_handoff(
    database_path: &Path,
    conversation_id: &str,
    sub_agent_id: &str,
    args: &Value,
) -> Result<Value> {
    let state = load_state(database_path, conversation_id, sub_agent_id)?;
    let cfg = resolve_config();
    let messages = load_history(database_path, conversation_id)?;
    let view = HistoryView::build(messages);
    let pairs = tool_pair_index(&view.messages);
    let ranges = block_ranges(&state, &view.positions, &pairs, &folded_blocked(&state, &view, &cfg));
    let folded = fold(&view.messages, &state, &cfg).0;
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(true);
    // 折叠后的视图与原始下标不再一一对应，refs 由 `render_message` 按内容指纹重挂；
    // 这里显式传空表，避免给出错位的 ref。
    let folded_refs_of: Vec<String> = Vec::new();
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let blocks_full: Vec<rendering::HandoffBlockFull> = if full {
        ranges
            .iter()
            .map(|(start, end, block)| rendering::HandoffBlockFull {
                block_id: block.id.clone(),
                topic: block.title.clone(),
                count: block.message_ids.len(),
                full_text: view.messages[*start..=*end]
                    .iter()
                    .map(|message| rendering::render_message(message, None))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            })
            .collect()
    } else {
        Vec::new()
    };
    let rendered = rendering::render_handoff(&rendering::HandoffInput {
        messages: &folded,
        state: &state,
        full,
        folded: false,
        blocks_full,
        meta: rendering::HandoffMeta {
            title,
            label: String::new(),
            session_id: conversation_id.to_string(),
            context_tokens: latest_usage_tokens(database_path, conversation_id),
            extra_bullets: Vec::new(),
        },
        refs_of: folded_refs_of,
    });
    Ok(json!({
        "success": true,
        "handoff": rendered,
        "blocks": state.blocks.len(),
        "active": state.blocks.iter().filter(|block| block.folded_into.is_empty()).count(),
    }))
}

/// 供 `handoff` 复用的「永不折叠」位图：系统 / 媒体 / 内核工具消息，以及配置里
/// 声明的受保护工具（含其配对的另一半）。
fn folded_blocked(state: &KernelState, view: &HistoryView, cfg: &KernelConfig) -> Vec<bool> {
    let _ = state;
    let protection = protection_config(cfg);
    let protected_calls = protection::collect_protected_tool_call_ids(&view.messages, &protection);
    view.messages
        .iter()
        .map(|message| {
            compression_blocked_reason(message).is_some()
                || protection::is_message_protected_with_pairing(
                    message,
                    &protection,
                    &protected_calls,
                )
        })
        .collect()
}

/// 由内核配置构造保护规则切片。
fn protection_config(cfg: &KernelConfig) -> protection::ProtectionConfig {
    protection::ProtectionConfig {
        protected_tools: cfg.protected_tools.clone(),
        preserve_recent_tools: cfg.preserve_recent_tools.clone(),
        protected_latest_tools: cfg.protected_latest_tools.clone(),
        kernel_tool_prefix: KERNEL_TOOL_PREFIX.to_string(),
    }
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
        "absorb" => execute_absorb(database_path, conversation_id, sub_agent_id, args),
        "acp_rule" => execute_rule(database_path, conversation_id, sub_agent_id, args),
        "decompress" => execute_decompress(database_path, conversation_id, sub_agent_id, args),
        "retrieve" => execute_retrieve(database_path, conversation_id, sub_agent_id, args),
        "search_context" => execute_search(database_path, conversation_id, sub_agent_id, args),
        "acp_cache" => execute_cache(
            database_path,
            conversation_id,
            sub_agent_id,
            threshold_tokens,
            args,
        ),
        "acp_status" => execute_status(
            database_path,
            conversation_id,
            sub_agent_id,
            threshold_tokens,
            args,
        ),
        "recap" => execute_recap(database_path, conversation_id, sub_agent_id, args),
        "handoff" => execute_handoff(database_path, conversation_id, sub_agent_id, args),
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
