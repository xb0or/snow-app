//! 上游 `absorb.ts` + `crush.ts`。
//!
//! absorb 是「让工具结果堆到 50K token 才被 nudge」和「10K 窗口下模型没法干活」之间的
//! 中间层：开启后内核给每个够大的工具结果**追加一段强制提示**，要求模型立刻用
//! absorb 把它蒸馏成摘要；随后调用/结果这一对从后续所有轮次里隐藏，只留模型的 absorb
//! 调用作为持久记录。absorb 调用本身是普通消息，之后的常规压缩管线照样可以再折它。
//!
//! crush 是工具结果的**两段式闸门**：先做机械的确定性缩小（json 折叠 / 代码裁剪 /
//! 日志筛选），只有仍然超阈值才交给模型蒸馏。纯函数：同样的字节每轮产出同样的结果
//! （前缀缓存稳定），内核状态不变。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use super::texts;
use super::ChatContextMessage;

fn format_k(value: i64) -> String {
    if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// absorb.ts
// ---------------------------------------------------------------------------

/// 强制吸收提示。
pub fn build_absorb_prompt(reference: &str, tokens: i64, tool_name: &str) -> String {
    let tool = if tool_name.trim().is_empty() {
        texts::ABSORB_TOOL_NAME
    } else {
        tool_name.trim()
    };
    format!(
        "{marker} This tool result (~{tokens} tokens) will be REMOVED from context. Your IMMEDIATE next action: call {tool}({{ ref: \"{reference}\", summary: \"...\" }}) — summary = distilled essentials only (outcome, key values, exact paths:lines, error text verbatim, decisions). Afterwards work from your summary; do NOT re-run this tool. If the result contains nothing you need, call {tool} with summary \"(nothing needed)\".",
        marker = texts::ABSORB_PROMPT_MARKER,
        tokens = format_k(tokens),
    )
}

/// 开启 absorb 的适配器的系统提示词段落。
pub fn build_absorb_system_prompt(tool_name: &str) -> String {
    let tool = if tool_name.trim().is_empty() {
        texts::ABSORB_TOOL_NAME
    } else {
        tool_name.trim()
    };
    format!(
        "INSTANT TOOL-RESULT ABSORPTION ({tool})\n\nSome tool results end with a {marker} instruction. When you see one, your IMMEDIATE next action must be calling {tool}({{ ref, summary }}) — distill that tool result's essentials into summary: outcome, key values, exact paths:lines, error text verbatim, decisions. The original output is then removed from context; your {tool} summary becomes the only durable record of it, so distill carefully. Never call another tool or answer the user before absorbing a marked result. Do not re-run the original tool afterwards — work from your summary. {tool} calls are ordinary context: the regular compression system may fold them later like any other message.",
        marker = texts::ABSORB_PROMPT_MARKER,
    )
}

/// absorb 是否为该工具结果的在范围内候选：非 ACP 工具、未被排除、不受保护。
pub fn is_absorb_candidate(
    message: &ChatContextMessage,
    enabled: bool,
    min_tool_tokens: i64,
    exclude_tools: &[String],
    tokens: i64,
    upstream_tool_names: &[String],
) -> bool {
    if !enabled {
        return false;
    }
    if message.role.trim() != "tool" {
        return false;
    }
    if super::is_kernel_tool_message(message) {
        return false;
    }
    if upstream_tool_names.iter().any(|name| {
        exclude_tools
            .iter()
            .any(|pattern| super::protection::match_tool_pattern(name, pattern))
    }) {
        return false;
    }
    if super::protection::is_message_protected(
        message,
        &super::protection::ProtectionConfig::default(),
    ) {
        return false;
    }
    if tokens < min_tool_tokens {
        return false;
    }
    // 已经是 CCR 占位符的正文不重复提示（按 ID 引用优先于蒸馏）。
    !super::content::is_stored_placeholder_text(&message.content)
}

#[derive(Clone, Debug)]
pub struct AppendAbsorbPromptsResult {
    pub messages: Vec<ChatContextMessage>,
    pub prompted_count: usize,
}

/// 给每个合格、未吸收的大工具结果追加强制 absorb 提示。
///
/// 这是**每轮视图专用文本**（内核从不持久化它）：提示会一直重新出现，直到模型真的
/// 吸收；一旦调用/结果对被隐藏，提示也随之消失。
pub fn append_absorb_prompts(
    messages: &[ChatContextMessage],
    enabled: bool,
    min_tool_tokens: i64,
    exclude_tools: &[String],
    tool_name: &str,
    token_of: impl Fn(&ChatContextMessage) -> i64,
    ref_of: impl Fn(usize) -> String,
) -> AppendAbsorbPromptsResult {
    let mut out = messages.to_vec();
    let mut prompted = 0usize;
    for (index, message) in out.iter_mut().enumerate() {
        let names = super::segments::tool_names_of(message);
        let tokens = token_of(message);
        if !is_absorb_candidate(
            message,
            enabled,
            min_tool_tokens,
            exclude_tools,
            tokens,
            &names,
        ) {
            continue;
        }
        let reference = ref_of(index);
        if reference.is_empty() {
            continue;
        }
        let prompt = build_absorb_prompt(&reference, tokens, tool_name);
        if message.content.contains(texts::ABSORB_PROMPT_MARKER) {
            // 幂等：把旧提示替换掉而不是叠加。
            if let Some(position) = message.content.find(texts::ABSORB_PROMPT_MARKER) {
                message.content.truncate(position);
                message.content.push_str(&prompt);
            }
        } else {
            if !message.content.ends_with('\n') {
                message.content.push('\n');
            }
            message.content.push('\n');
            message.content.push_str(&prompt);
        }
        prompted += 1;
    }
    AppendAbsorbPromptsResult {
        messages: out,
        prompted_count: prompted,
    }
}

/// absorb 调用的宽松解析结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAbsorb {
    pub r#ref: String,
    pub summary: String,
    pub absorb_call_id: Option<String>,
}

/// 宽松解析 absorb 工具调用参数：接受 ref / messageId / of 与 summary / content 拼写，
/// 也接受被 stringify 的 JSON 载荷。
pub fn parse_absorb_input(input: &serde_json::Value, call_id: Option<&str>) -> Option<ParsedAbsorb> {
    parse_lenient(input, &["ref", "messageId", "message_id", "of", "id"], &["summary", "content", "text"])
        .map(|(reference, summary)| ParsedAbsorb {
            r#ref: reference,
            summary,
            absorb_call_id: call_id.map(ToString::to_string),
        })
}

/// 「ref 键 + 内容键」的通用宽松解析（absorb / image_full / compress 单条都走这里）。
pub fn parse_lenient(
    input: &serde_json::Value,
    ref_keys: &[&str],
    body_keys: &[&str],
) -> Option<(String, String)> {
    let value = match input {
        serde_json::Value::String(raw) => serde_json::from_str::<serde_json::Value>(raw).ok()?,
        other => other.clone(),
    };
    let object = value.as_object()?;
    let reference = ref_keys
        .iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let body = body_keys
        .iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    Some((reference, body))
}

// ---------------------------------------------------------------------------
// crush.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrushKind {
    Json,
    Code,
    Log,
}

impl CrushKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CrushKind::Json => "json",
            CrushKind::Code => "code",
            CrushKind::Log => "log",
        }
    }
}

/// 内容嗅探（上游 `classifyCrushText`）：JSON 形状 → json；高置信度源码 → code；
/// 其余 → log。
pub fn classify_crush_text(text: &str) -> CrushKind {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
            return CrushKind::Json;
        }
    }
    if looks_like_source(text) {
        return CrushKind::Code;
    }
    CrushKind::Log
}

fn looks_like_source(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 8 {
        return false;
    }
    let mut source_hits = 0usize;
    for line in lines.iter() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("def ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("package ")
            || trimmed.starts_with("public ")
            || trimmed.starts_with("private ")
            || trimmed.starts_with("#include")
            || trimmed.starts_with("using ")
            || trimmed.starts_with("fn ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("export ")
        {
            source_hits += 1;
        }
    }
    // 源码要有足够密度的声明行；日志里偶尔出现的 "import" 词不算。
    source_hits * 10 >= lines.len().max(1)
}

#[derive(Clone, Debug)]
pub struct CrushOutput {
    pub text: String,
    pub strategy: String,
    pub lossy: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CrushEvaluation {
    pub kind: String,
    pub text: String,
    pub raw_tokens: i64,
    pub new_tokens: Option<i64>,
    pub reduction: Option<f64>,
    pub strategy: Option<String>,
    pub lossy: Option<bool>,
}

/// 单次入口：分类 → 按顺序派发到第一个匹配类型且被接受的策略。`None` = 原样透传。
pub fn crush_text(
    text: &str,
    min_reduction: f64,
    count_tokens: impl Fn(&str) -> i64,
) -> Option<CrushOutput> {
    let kind = classify_crush_text(text);
    let outcome = match kind {
        CrushKind::Json => crush_json(text).map(|body| CrushOutput {
            text: body,
            strategy: "json-fold".to_string(),
            lossy: false,
        }),
        CrushKind::Code => crush_code(text).map(|body| CrushOutput {
            text: body,
            strategy: "code-trim".to_string(),
            lossy: true,
        }),
        CrushKind::Log => crush_log(text).map(|body| CrushOutput {
            text: body,
            strategy: "log-select".to_string(),
            lossy: true,
        }),
    }?;
    // minReduction 闸门：缩小不够就当作没压（保底不做无意义改写）。
    let before = count_tokens(text) as f64;
    let after = count_tokens(&outcome.text) as f64;
    if before <= 0.0 {
        return None;
    }
    let reduction = 1.0 - (after / before);
    if reduction < min_reduction {
        return None;
    }
    Some(outcome)
}

/// JSON 折叠：**无损**的字段方差折叠。
///
/// 取值方差低的对象数组被压成 `const` + 每行只剩变化字段；连续相同的元素折叠成
/// `identical-run`。带保留键 `__acp_crush` 的载荷无法与折叠结果区分 —— 这类载荷
/// 直接放弃（返回 `None`）。
pub fn crush_json(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let folded = fold_value(&value)?;
    serde_json::to_string(&folded).ok()
}

const CRUSH_KEY: &str = "__acp_crush";

fn contains_reserved_key(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(CRUSH_KEY) || map.values().any(contains_reserved_key)
        }
        serde_json::Value::Array(items) => items.iter().any(contains_reserved_key),
        _ => false,
    }
}

fn fold_value(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Array(items) if !items.is_empty() => {
            if contains_reserved_key(value) {
                return None;
            }
            // 连续相同元素 → identical-run
            let mut runs: Vec<(serde_json::Value, usize)> = Vec::new();
            for item in items {
                match runs.last_mut() {
                    Some((existing, count)) if existing == item => *count += 1,
                    _ => runs.push((item.clone(), 1)),
                }
            }
            if runs.len() < items.len() {
                let folded: Vec<serde_json::Value> = runs
                    .into_iter()
                    .map(|(item, count)| {
                        if count == 1 {
                            item
                        } else {
                            serde_json::json!({ CRUSH_KEY: "identical-run", "count": count, "item": item })
                        }
                    })
                    .collect();
                if folded.len() == items.len() {
                    return None;
                }
                return Some(serde_json::Value::Array(folded));
            }
            // 对象数组 → 字段方差折叠
            let objects: Option<Vec<&serde_json::Map<String, serde_json::Value>>> =
                items.iter().map(|item| item.as_object()).collect();
            let objects = objects?;
            let mut keys: Vec<String> = Vec::new();
            for object in objects.iter() {
                for key in object.keys() {
                    if !keys.contains(key) {
                        keys.push(key.clone());
                    }
                }
            }
            let mut constant = serde_json::Map::new();
            let mut varying: Vec<String> = Vec::new();
            for key in keys {
                let first = objects[0].get(&key);
                let is_constant = objects
                    .iter()
                    .all(|object| object.get(&key) == first)
                    && first.is_some();
                if is_constant {
                    constant.insert(key, first.cloned().unwrap_or(serde_json::Value::Null));
                } else {
                    varying.push(key);
                }
            }
            if varying.is_empty() {
                return None;
            }
            let items_out: Vec<serde_json::Value> = objects
                .iter()
                .map(|object| {
                    let mut row = serde_json::Map::new();
                    for key in varying.iter() {
                        if let Some(value) = object.get(key) {
                            row.insert(key.clone(), value.clone());
                        }
                    }
                    serde_json::Value::Object(row)
                })
                .collect();
            let mut out = serde_json::Map::new();
            out.insert(CRUSH_KEY.to_string(), serde_json::json!("rows"));
            out.insert("rows".to_string(), serde_json::json!(items.len()));
            out.insert("const".to_string(), serde_json::Value::Object(constant));
            out.insert("items".to_string(), serde_json::Value::Array(items_out));
            Some(serde_json::Value::Object(out))
        }
        _ => None,
    }
}

/// 代码裁剪：丢弃整行注释、docstring、连续空行，折叠成一行标记。
///
/// **有损**：被丢弃的行不会保留。代码行、字符串字面量、行尾注释保持字节不变。
pub fn crush_code(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 8 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    let mut elided = 0usize;
    let mut in_block_comment = false;
    let mut in_docstring: Option<&str> = None;
    let marker = "# [acp-crush: elided {} lines]";
    for line in lines.iter() {
        let trimmed = line.trim();
        if let Some(delimiter) = in_docstring {
            elided += 1;
            if trimmed.ends_with(delimiter) && trimmed.len() >= 3 {
                in_docstring = None;
            }
            continue;
        }
        if in_block_comment {
            elided += 1;
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.is_empty() {
            elided += 1;
            continue;
        }
        if trimmed.starts_with("/*") {
            elided += 1;
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with('#') && !trimmed.starts_with("# [acp-crush") {
            elided += 1;
            continue;
        }
        if trimmed.starts_with("//") {
            elided += 1;
            continue;
        }
        for delimiter in ["\"\"\"", "'''"] {
            if trimmed.starts_with(delimiter) {
                elided += 1;
                let rest = &trimmed[delimiter.len()..];
                if !rest.contains(delimiter) {
                    in_docstring = Some(delimiter);
                }
                break;
            }
        }
        if in_docstring.is_some() {
            continue;
        }
        out.push((*line).to_string());
    }
    if elided == 0 || out.is_empty() {
        return None;
    }
    let marker = marker.replace("{}", &elided.to_string());
    // 标记插在被省略区块的头部位置：这里统一放在末尾之前的第一处空位（确定性）。
    if out.len() > 1 {
        out.insert(1, marker);
    } else {
        out.push(marker);
    }
    Some(out.join("\n"))
}

/// 构建/测试日志压缩：先保正文错误与失败，再保去重后的警告，最后按行数上限截断。
///
/// **有损**：噪声行被丢弃。内核不变量：任何丢弃了 ERROR / FAIL 行的结果都会被拒绝，
/// 载荷原样透传。
pub fn crush_log(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 40 {
        return None;
    }
    let mut keep: Vec<usize> = Vec::new();
    let errors: Vec<usize> = (0..lines.len())
        .filter(|index| is_error_line(lines[*index]))
        .collect();
    let failed = errors.is_empty();
    for index in errors.iter() {
        for offset in 0..=3usize {
            if *index >= offset {
                keep.push(index - offset);
            }
            keep.push(index + offset);
        }
    }
    // 摘要行与去重后的警告
    let mut seen_warnings: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("warn") {
            let key = normalize_warning(line);
            if !seen_warnings.contains(&key) {
                seen_warnings.push(key);
                keep.push(index);
            }
        } else if is_summary_line(line) {
            keep.push(index);
        }
    }
    keep.sort_unstable();
    keep.dedup();
    keep.retain(|index| *index < lines.len());
    // 运行期栈帧塌缩成标记
    let mut out: Vec<String> = Vec::new();
    let mut previous_was_frame = false;
    let mut collapsed = 0usize;
    for index in keep.iter() {
        let line = lines[*index];
        if is_runtime_frame(line) {
            collapsed += 1;
            previous_was_frame = true;
            continue;
        }
        if previous_was_frame && collapsed > 0 {
            out.push(format!("  [.. {collapsed} runtime frame(s) collapsed ..]"));
            collapsed = 0;
        }
        previous_was_frame = false;
        out.push(line.to_string());
    }
    if collapsed > 0 {
        out.push(format!("  [.. {collapsed} runtime frame(s) collapsed ..]"));
    }
    const CAP: usize = 100;
    let dropped = out.len().saturating_sub(CAP);
    if dropped > 0 {
        out.truncate(CAP);
    }
    if out.len() >= lines.len() {
        return None;
    }
    let mut footer = String::new();
    if dropped > 0 {
        footer.push_str(&format!(
            "\n[acp-crush: {dropped} more line(s) omitted — {} kept of {}]",
            out.len(),
            lines.len()
        ));
    }
    if failed {
        footer.push_str("\n[acp-crush: no ERROR/FAIL line found in this log]");
    }
    Some(format!("{}{footer}", out.join("\n")))
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("fail")
        || lower.contains("panic")
        || lower.contains("fatal")
        || lower.contains("exception")
        || lower.contains("traceback")
}

fn is_summary_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("summary")
        || lower.contains("total")
        || lower.contains("passed")
        || lower.contains("completed")
        || lower.contains("done in")
}

fn is_runtime_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("at ")
        || trimmed.starts_with("File \"")
        || (trimmed.starts_with("node_modules") || trimmed.contains("node_modules/"))
}

fn normalize_warning(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_ascii_digit())
        .collect::<String>()
        .trim()
        .to_string()
}

/// 两段式闸门判定。
pub fn evaluate_tool_result(
    text: &str,
    absorb_enabled: bool,
    min_tool_tokens: i64,
    crush_enabled: bool,
    min_reduction: f64,
    token_count: i64,
    model_context_limit: i64,
    count_tokens: impl Fn(&str) -> i64,
) -> CrushEvaluation {
    let raw_tokens = count_tokens(text);
    let pressure = model_context_limit > 0 && token_count as f64 >= model_context_limit as f64 * 0.5;
    if !absorb_enabled || raw_tokens < min_tool_tokens {
        return CrushEvaluation {
            kind: "skip".to_string(),
            text: text.to_string(),
            raw_tokens,
            ..Default::default()
        };
    }
    if crush_enabled && (!pressure || raw_tokens >= min_tool_tokens) {
        if let Some(output) = crush_text(text, min_reduction, &count_tokens) {
            let new_tokens = count_tokens(&output.text);
            return CrushEvaluation {
                kind: "crushed".to_string(),
                text: output.text.clone(),
                raw_tokens,
                new_tokens: Some(new_tokens),
                reduction: Some(1.0 - (new_tokens as f64 / raw_tokens.max(1) as f64)),
                strategy: Some(output.strategy),
                lossy: Some(output.lossy),
            };
        }
    }
    CrushEvaluation {
        kind: "distill".to_string(),
        text: text.to_string(),
        raw_tokens,
        ..Default::default()
    }
}

/// 轮内应用（视图专用、每轮确定性）。
pub fn apply_crush_to_messages(
    messages: &[ChatContextMessage],
    enabled: bool,
    min_reduction: f64,
    count_tokens: impl Fn(&str) -> i64,
) -> (Vec<ChatContextMessage>, usize, usize) {
    if !enabled {
        return (messages.to_vec(), 0, 0);
    }
    let mut out = messages.to_vec();
    let (mut crushed, mut distilled) = (0usize, 0usize);
    for message in out.iter_mut() {
        if message.role.trim() != "tool" {
            continue;
        }
        if super::is_kernel_tool_message(message) || super::has_media_payload(message) {
            continue;
        }
        if super::content::is_stored_placeholder_text(&message.content) {
            continue;
        }
        let raw_tokens = count_tokens(&message.content);
        if raw_tokens < 2_000 {
            continue;
        }
        match crush_text(&message.content, min_reduction, &count_tokens) {
            Some(output) if output.lossy => {
                message.content = output.text;
                crushed += 1;
            }
            Some(_) => {}
            None => distilled += 1,
        }
    }
    (out, crushed, distilled)
}
