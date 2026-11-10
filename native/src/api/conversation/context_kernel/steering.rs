//! 上游 `output-steering.ts`（issue #355）：轮结构分类、简洁度等级（L0–L4）、
//! 只降不升的 effort 路由。
//!
//! 这一层是**与协议无关的决策半边**：它只做决定，落地由适配器负责。
//!
//! 三条硬边界：
//!  1. **只降不升** —— `lower_effort` 从不意味着「调高」。适配器只下调客户端
//!     **已经发送**的 effort 字段；缺席字段绝不注入（不支持 effort 的模型会 400），
//!     已经在底线之下的值原样不动。
//!  2. **只追加到尾部** —— verbosity 指令写在系统提示词**尾部**（前缀缓存要求
//!     头部字节稳定，绝不前插）。`apply_steering_to_prompt` 会**原地替换**已有的
//!     sentinel 块，所以同一等级重复应用是字节稳定的空操作。
//!  3. **字节稳定的指令** —— L1–L4 文本跨版本冻结，改一个字符就会击穿所有钉在该
//!     等级的会话前缀缓存。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use super::refs;
use super::texts;
use super::ChatContextMessage;

// ---------------------------------------------------------------------------
// 轮结构分类
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnKind {
    NewUserAsk,
    MechanicalContinuation,
    ErrorContinuation,
    Unknown,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::NewUserAsk => "new_user_ask",
            TurnKind::MechanicalContinuation => "mechanical_continuation",
            TurnKind::ErrorContinuation => "error_continuation",
            TurnKind::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuralBlockKind {
    Text,
    Image,
    Document,
    ToolResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructuralBlock {
    pub kind: StructuralBlockKind,
    pub is_error: bool,
}

#[derive(Clone, Debug, Default)]
pub struct StructuralMessage {
    pub role: String,
    pub blocks: Vec<StructuralBlock>,
}

/// 纯结构分类**最后一轮**：只看块组成，不做内容模式匹配。
///
/// 保守设计：任何无法识别的组合都给 `unknown`（绝不猜），因为无法识别的组合可能
/// 带着我们看不见的用户意图。
pub fn classify_turn(messages: &[StructuralMessage]) -> TurnKind {
    let Some(last) = messages.last() else {
        return TurnKind::Unknown;
    };
    if last.blocks.is_empty() {
        return TurnKind::Unknown;
    }
    let user_signal = last.blocks.iter().any(|block| {
        matches!(
            block.kind,
            StructuralBlockKind::Text | StructuralBlockKind::Image | StructuralBlockKind::Document
        )
    });
    let tool_results: Vec<&StructuralBlock> = last
        .blocks
        .iter()
        .filter(|block| block.kind == StructuralBlockKind::ToolResult)
        .collect();
    if tool_results.is_empty() {
        return if user_signal {
            TurnKind::NewUserAsk
        } else {
            TurnKind::Unknown
        };
    }
    if tool_results.iter().any(|block| block.is_error) {
        TurnKind::ErrorContinuation
    } else {
        TurnKind::MechanicalContinuation
    }
}

/// 从内核消息列表推导结构摘要（Snow 侧的规范化契约）。
///
/// 只关心**最后一条**消息的组成；工具结果 → `tool_result`，图片/文档 → 媒体，
/// user/assistant 文本 → `text`。
pub fn structural_summary(messages: &[ChatContextMessage]) -> Vec<StructuralMessage> {
    messages
        .iter()
        .map(|message| {
            let role = message.role.trim().to_string();
            let mut blocks: Vec<StructuralBlock> = Vec::new();
            let has_media = super::has_media_payload(message);
            if message.role.trim() == "tool" {
                blocks.push(StructuralBlock {
                    kind: StructuralBlockKind::ToolResult,
                    is_error: tool_result_is_error(message),
                });
            } else if has_media {
                blocks.push(StructuralBlock {
                    kind: StructuralBlockKind::Image,
                    is_error: false,
                });
            } else if !message.content.trim().is_empty() {
                blocks.push(StructuralBlock {
                    kind: StructuralBlockKind::Text,
                    is_error: false,
                });
            }
            StructuralMessage { role, blocks }
        })
        .collect()
}

fn tool_result_is_error(message: &ChatContextMessage) -> bool {
    let raw = message.tool_results_json.as_deref().unwrap_or("");
    raw.contains("\"isError\":true")
        || raw.contains("\"is_error\":true")
        || raw.contains("\"error\":true")
        || message.content.trim_start().starts_with("Error:")
        || message.content.trim_start().starts_with("error:")
}

// ---------------------------------------------------------------------------
// 决策
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSteeringConfig {
    /// 总开关。默认 **关** —— 两个杠杆在显式打开前都是惰性的。
    pub enabled: bool,
    /// 简洁度等级 0–4（0 = 不下发指令）。默认 2。
    pub verbosity_level: i64,
    /// effort 路由子开关。`enabled` 打开时默认也打开。
    pub effort_routing: bool,
}

impl Default for OutputSteeringConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            verbosity_level: texts::DEFAULT_VERBOSITY_LEVEL,
            effort_routing: true,
        }
    }
}

pub fn default_output_steering_config() -> OutputSteeringConfig {
    OutputSteeringConfig::default()
}

/// 解析原始简洁度：**省略**字段静默取默认；**存在但越界**回落并给出警告
/// （诚实输出 —— 合法的部分配置不该被指控为格式错误）。
pub fn resolve_verbosity_level(value: Option<&serde_json::Value>) -> (i64, Option<String>) {
    match value {
        None => (texts::DEFAULT_VERBOSITY_LEVEL, None),
        Some(serde_json::Value::Number(number)) => {
            if let Some(level) = number.as_i64() {
                if (texts::MIN_VERBOSITY_LEVEL..=texts::MAX_VERBOSITY_LEVEL).contains(&level) {
                    return (level, None);
                }
            }
            (
                texts::DEFAULT_VERBOSITY_LEVEL,
                Some(format!(
                    "[config] outputSteering.verbosityLevel must be an integer 0-4; got {number} — falling back to {}",
                    texts::DEFAULT_VERBOSITY_LEVEL
                )),
            )
        }
        Some(other) => (
            texts::DEFAULT_VERBOSITY_LEVEL,
            Some(format!(
                "[config] outputSteering.verbosityLevel must be an integer 0-4; got {other} — falling back to {}",
                texts::DEFAULT_VERBOSITY_LEVEL
            )),
        ),
    }
}

/// 校验原始 `outputSteering` 配置值。格式错误回落默认值而不是抛错；警告作为数据
/// 返回给宿主记录。非对象输入直接得到纯默认值。
pub fn resolve_output_steering_config(
    value: Option<&serde_json::Value>,
) -> (OutputSteeringConfig, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let Some(serde_json::Value::Object(object)) = value else {
        return (OutputSteeringConfig::default(), warnings);
    };
    let enabled = match object.get("enabled") {
        None => false,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Null) => false,
        Some(other) => {
            warnings.push(format!(
                "[config] outputSteering.enabled must be a boolean; got {other} — falling back to false"
            ));
            false
        }
    };
    let raw_level = match object.get("verbosityLevel") {
        Some(serde_json::Value::Null) => Some(serde_json::Value::Null),
        Some(value) => Some(value.clone()),
        None => None,
    };
    let (level, warning) = resolve_verbosity_level(raw_level.as_ref());
    if let Some(warning) = warning {
        warnings.push(warning);
    }
    let effort_routing = match object.get("effortRouting") {
        None => true,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(other) => {
            warnings.push(format!(
                "[config] outputSteering.effortRouting must be a boolean; got {other} — falling back to true"
            ));
            true
        }
    };
    (
        OutputSteeringConfig {
            enabled,
            verbosity_level: level,
            effort_routing,
        },
        warnings,
    )
}

/// 引导决策。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSteeringDecision {
    pub turn_kind: TurnKind,
    /// 要下发的等级；0 = 什么都不下发。禁用时恒为 0。
    pub verbosity_level: i64,
    /// 只降不升的路由：真 ⇔ 启用 && 开启 effort 路由 && 机械式续跑。
    pub lower_effort: bool,
}

/// 单点决策入口：最后一轮的结构摘要 + 已解析配置 → 引导决策。纯函数。
pub fn decide_output_steering(
    messages: &[StructuralMessage],
    config: &OutputSteeringConfig,
) -> OutputSteeringDecision {
    let turn_kind = classify_turn(messages);
    if !config.enabled {
        return OutputSteeringDecision {
            turn_kind,
            verbosity_level: 0,
            lower_effort: false,
        };
    }
    OutputSteeringDecision {
        turn_kind,
        verbosity_level: config.verbosity_level,
        lower_effort: config.effort_routing
            && turn_kind == TurnKind::MechanicalContinuation,
    }
}

// ---------------------------------------------------------------------------
// effort 阶梯
// ---------------------------------------------------------------------------

/// 只降底线计算：返回要写入的值，字段缺席/无法识别/已在底线之下时返回 `None`。
///
/// 从不抬升：显式要求 `minimal` 的客户端保持 `minimal`。无法识别的值原样不动而不是
/// 强转 —— 把一个恰好低于底线的未知值强转成底线就是抬升。
pub fn clamp_effort_to_floor(current: Option<&str>, floor: Option<&str>) -> Option<String> {
    let index_of = |value: &str| {
        texts::EFFORT_LADDER
            .iter()
            .position(|entry| entry.eq_ignore_ascii_case(value))
    };
    let current = current?;
    let floor = floor.unwrap_or("low");
    let current_index = index_of(current.trim())?;
    let floor_index = index_of(floor.trim())?;
    if current_index <= floor_index {
        return None;
    }
    Some(texts::EFFORT_LADDER[floor_index].to_string())
}

// ---------------------------------------------------------------------------
// 系统提示词落地
// ---------------------------------------------------------------------------

/// 引导块的放置结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteeringPlacement {
    pub updated: String,
    pub changed: bool,
}

fn sentinel_close(sentinel: &str) -> String {
    let name = sentinel
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    format!("</{name}>")
}

/// 幂等地把 `block` 放进 `existing`：已有被 `sentinel` 包裹的块时**原地替换**，
/// 否则追加到尾部。**从不前插**。重复应用同一块返回 `changed: false` 且字节完全
/// 一致（重试安全）。畸形（未闭合）的 sentinel 块会被一直消费到字符串结尾，避免
/// 重复应用时出现重复闭合标签或块无限增长。
pub fn apply_steering_to_prompt(
    existing: &str,
    block: &str,
    sentinel: &str,
) -> SteeringPlacement {
    let open = sentinel.trim();
    let close = sentinel_close(sentinel);
    if let Some(start) = existing.find(open) {
        let tail_start = start + open.len();
        let end = existing[tail_start..]
            .find(&close)
            .map(|offset| tail_start + offset + close.len())
            .unwrap_or(existing.len());
        let updated = format!("{}{}{}", &existing[..start], block, &existing[end..]);
        let changed = updated != existing;
        return SteeringPlacement { updated, changed };
    }
    if existing.is_empty() {
        return SteeringPlacement {
            updated: block.to_string(),
            changed: true,
        };
    }
    SteeringPlacement {
        updated: format!("{existing}\n\n{block}"),
        changed: true,
    }
}

/// 便捷入口：按等级渲染引导块并落到提示词尾部。
pub fn steer_prompt(existing: &str, level: i64, sentinel: &str) -> SteeringPlacement {
    match texts::render_steering_block(level, sentinel) {
        Some(block) => apply_steering_to_prompt(existing, &block, sentinel),
        None => SteeringPlacement {
            updated: existing.to_string(),
            changed: false,
        },
    }
}

/// 结构摘要里最后一条消息的 ref（观测/日志用）。
pub fn last_ref_of(references: &[String]) -> Option<String> {
    references
        .iter()
        .rev()
        .find(|reference| refs::ref_to_index(reference).is_some())
        .cloned()
}
