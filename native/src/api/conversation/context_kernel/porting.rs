//! 上游 `pipeline.ts` + `filter/` + `packs.ts` + `prompts.ts` + `surface-config.ts` +
//! `parse-compress-input.ts` + `compress-tools.ts` + `search/`。
//!
//! 这一组是「出站管线的节点序」「消息过滤器注册表」「提示词包解析」「表层文本覆盖」
//! 「compress 参数宽松解析」「ACP 工具 schema」「压缩块检索」。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use std::collections::HashMap;

use super::texts;
use super::ChatContextMessage;

// ---------------------------------------------------------------------------
// pipeline.ts
// ---------------------------------------------------------------------------

/// 上游默认管线节点名（顺序即执行顺序）。
pub const DEFAULT_PIPELINE_NODES: &[&str] = &[
    "reconcile-live-ids",
    "assign-refs",
    "sync-blocks",
    "prune",
    "ccr-store",
    "absorb-hide",
    "crush",
    "absorb-prompt",
    "filter",
    "hide-compress-calls",
    "recommend",
    "nudge-inject",
    "emergency-truncate",
    "render-refs",
];

/// 管线节点效果（可选，做观测/日志用）。
#[derive(Clone, Debug, Default)]
pub struct NodeEffects {
    pub nudge: bool,
    pub truncated_count: usize,
    pub terminal_escape: bool,
    pub truncation_skipped: String,
}

/// 出站视图的节点式管线：每一步都是纯变换，顺序即语义。Snow 侧由
/// `apply_outgoing_view` 内联执行同一顺序；这个结构用于自描述与审计。
#[derive(Clone, Debug, Default)]
pub struct PipelineReport {
    pub nodes: Vec<String>,
    pub effects: NodeEffects,
}

pub fn pipeline_report() -> PipelineReport {
    PipelineReport {
        nodes: DEFAULT_PIPELINE_NODES.iter().map(|name| name.to_string()).collect(),
        effects: NodeEffects::default(),
    }
}

// ---------------------------------------------------------------------------
// filter/index.ts
// ---------------------------------------------------------------------------

/// 一条消息的某个「部件」（文本 / 工具参数 / 工具结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagePart {
    pub kind: String,
    pub text: String,
}

impl MessagePart {
    pub fn text(text: &str) -> Self {
        Self {
            kind: "text".to_string(),
            text: text.to_string(),
        }
    }
}

/// 部件过滤器：往返文本，返回 `None` 表示丢弃该部件。
pub type MessageFilter = fn(&MessagePart) -> Option<MessagePart>;

/// 消息过滤器注册表（名字 → 过滤器）。
pub fn list_message_filters(registry: &HashMap<String, MessageFilter>) -> Vec<String> {
    let mut names: Vec<String> = registry.keys().cloned().collect();
    names.sort();
    names
}

/// 就地跑一遍所有注册的过滤器，返回 (替换数, 丢弃数, 修改数)。
pub fn apply_message_filters(
    parts: &mut Vec<MessagePart>,
    registry: &HashMap<String, MessageFilter>,
) -> (usize, usize, usize) {
    let mut replaced = 0usize;
    let mut dropped = 0usize;
    let mut modified = 0usize;
    let mut filters: Vec<&MessageFilter> = registry.values().collect();
    filters.sort_by_key(|filter| std::ptr::from_ref(*filter) as usize);
    for filter in filters {
        let mut next: Vec<MessagePart> = Vec::with_capacity(parts.len());
        for part in parts.iter() {
            match filter(part) {
                Some(out) => {
                    if &out != part {
                        if out.kind.is_empty() {
                            replaced += 1;
                        } else {
                            modified += 1;
                        }
                    }
                    next.push(out);
                }
                None => dropped += 1,
            }
        }
        *parts = next;
    }
    (replaced, dropped, modified)
}

// ---------------------------------------------------------------------------
// prompts.ts
// ---------------------------------------------------------------------------

/// 可覆盖的提示词文本。每个字段都是**承重**的：这些规则经过数月生产调优，覆盖它们
/// 会降低摘要质量（丢路径 / 签名 / 决策 → 取回失效），因此 `resolve_prompts` 要求
/// `acknowledge_risk`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompts {
    pub compress_philosophy: String,
    pub how_to_compress_rules: String,
    pub tier2_distill_rules: String,
    pub tier3_condense_rules: String,
}

impl Default for Prompts {
    fn default() -> Self {
        Self {
            compress_philosophy: texts::COMPRESS_PHILOSOPHY.to_string(),
            how_to_compress_rules: texts::HOW_TO_COMPRESS_RULES.to_string(),
            tier2_distill_rules: texts::TIER2_DISTILL_RULES.to_string(),
            tier3_condense_rules: texts::TIER3_CONDENSE_RULES.to_string(),
        }
    }
}

pub fn default_prompts() -> Prompts {
    Prompts::default()
}

/// 合并覆盖到内核默认值。所有字段承重，所以**任何**覆盖都需要 `acknowledge_risk`。
///
/// 只有 `string` 类型的覆盖会生效：显式 `null` / 类型不对一律静默丢弃（绝不覆盖一个
/// 好默认），因此一个畸形的部分覆盖不会退化规范规则。
/// `language_preservation` 只做**追加**（#493），不替换任何承重措辞，因此不要求
/// `acknowledge_risk`；默认关闭时结果与默认值字节一致。
pub fn resolve_prompts(
    overrides: &HashMap<String, Option<String>>,
    acknowledge_risk: bool,
    language_preservation: bool,
) -> Result<Prompts, String> {
    let mut prompts = Prompts::default();
    if !overrides.is_empty() && !acknowledge_risk {
        return Err(
            "resolve_prompts: overriding prompt text requires { acknowledgeRisk: true } — every field is load-bearing"
                .to_string(),
        );
    }
    if acknowledge_risk {
        if let Some(Some(value)) = overrides.get("compressPhilosophy") {
            prompts.compress_philosophy = value.clone();
        }
        if let Some(Some(value)) = overrides.get("howToCompressRules") {
            prompts.how_to_compress_rules = value.clone();
        }
        if let Some(Some(value)) = overrides.get("tier2DistillRules") {
            prompts.tier2_distill_rules = value.clone();
        }
        if let Some(Some(value)) = overrides.get("tier3CondenseRules") {
            prompts.tier3_condense_rules = value.clone();
        }
    }
    if language_preservation {
        let rule = texts::LANGUAGE_PRESERVATION_RULE;
        prompts.compress_philosophy = format!("{}\n- {rule}", prompts.compress_philosophy);
        prompts.how_to_compress_rules = format!("{}\n\n{rule}", prompts.how_to_compress_rules);
        prompts.tier2_distill_rules = format!("{}\n\n{rule}", prompts.tier2_distill_rules);
        prompts.tier3_condense_rules = format!("{}\n\n{rule}", prompts.tier3_condense_rules);
    }
    Ok(prompts)
}

// ---------------------------------------------------------------------------
// surface-config.ts
// ---------------------------------------------------------------------------

/// 单个具名段的三态覆盖：`Some(text)` 替换，`Some(None)` 删除，`None`（未出现）保留默认。
pub type SectionOverride = Option<String>;

/// 对有序段列表应用三态覆盖，按原顺序返回存活的文本。
pub fn apply_section_overrides(
    sections: &[(String, String)],
    overrides: &HashMap<String, SectionOverride>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (key, text) in sections {
        match overrides.get(key) {
            None => out.push(text.clone()),
            Some(None) => {}
            Some(Some(replacement)) => out.push(replacement.clone()),
        }
    }
    out
}

/// 深拷贝一个 JSON 形状的工具参数 schema，替换**任意嵌套深度**上匹配覆盖键的属性
/// `description`。没有覆盖时原样返回；绝不改输入。
pub fn clone_with_descriptions(
    schema: &serde_json::Value,
    overrides: &HashMap<String, String>,
) -> serde_json::Value {
    if overrides.is_empty() {
        return schema.clone();
    }
    let mut cloned = schema.clone();
    apply_descriptions(&mut cloned, overrides);
    cloned
}

fn apply_descriptions(value: &mut serde_json::Value, overrides: &HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(properties)) = map.get_mut("properties") {
                for (name, property) in properties.iter_mut() {
                    if let Some(description) = overrides.get(name) {
                        if let Some(object) = property.as_object_mut() {
                            object.insert(
                                "description".to_string(),
                                serde_json::Value::String(description.clone()),
                            );
                        }
                    }
                    apply_descriptions(property, overrides);
                }
            }
            if let Some(items) = map.get_mut("items") {
                apply_descriptions(items, overrides);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                apply_descriptions(item, overrides);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// packs.ts
// ---------------------------------------------------------------------------

/// 提示词包名合法性：小写字母、数字、`-`、`_`，1–64 字符。
pub fn is_valid_pack_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

/// 从原始包 JSON 体里清洗出内核 surface。畸形值被丢弃 —— 一个坏的覆盖绝不会覆盖
/// 掉一个好默认。`adapters` 原样透传给宿主校验。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackSurface {
    pub prompts: HashMap<String, String>,
    pub prompt_sections: HashMap<String, SectionOverride>,
    pub nudge_sections: HashMap<String, Option<String>>,
    pub tool_prompts: HashMap<String, serde_json::Value>,
    pub adapters: serde_json::Value,
}

pub fn sanitize_pack_surface(raw: Option<&serde_json::Value>) -> PackSurface {
    let mut surface = PackSurface {
        adapters: serde_json::Value::Object(serde_json::Map::new()),
        ..Default::default()
    };
    let Some(serde_json::Value::Object(object)) = raw else {
        return surface;
    };
    if let Some(serde_json::Value::Object(prompts)) = object.get("prompts") {
        for (key, value) in prompts {
            if let serde_json::Value::String(text) = value {
                surface.prompts.insert(key.clone(), text.clone());
            }
        }
    }
    if let Some(serde_json::Value::Object(sections)) = object.get("promptSections") {
        for (key, value) in sections {
            match value {
                serde_json::Value::String(text) => {
                    surface
                        .prompt_sections
                        .insert(key.clone(), Some(text.clone()));
                }
                serde_json::Value::Null => {
                    surface.prompt_sections.insert(key.clone(), None);
                }
                _ => {}
            }
        }
    }
    if let Some(serde_json::Value::Object(sections)) = object.get("nudgeSections") {
        for (key, value) in sections {
            match value {
                serde_json::Value::String(text) => {
                    surface.nudge_sections.insert(key.clone(), Some(text.clone()));
                }
                serde_json::Value::Null => {
                    surface.nudge_sections.insert(key.clone(), None);
                }
                _ => {}
            }
        }
    }
    if let Some(serde_json::Value::Object(tools)) = object.get("toolPrompts") {
        for (key, value) in tools {
            if value.is_object() {
                surface.tool_prompts.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(adapters) = object.get("adapters") {
        if adapters.is_object() {
            surface.adapters = adapters.clone();
        }
    }
    surface
}

/// 解析出来的提示词包。
#[derive(Clone, Debug, PartialEq)]
pub struct Pack {
    pub name: String,
    pub surface: PackSurface,
    pub source: String,
}

/// 身份包：没有任何覆盖，处处内核默认。
pub fn default_pack() -> Pack {
    Pack {
        name: "default".to_string(),
        surface: PackSurface::default(),
        source: "builtin:default".to_string(),
    }
}

/// 极简包：一行工具描述，无代码片段与指南；`howToCompress` 槽位放精简版契约。
pub fn lean_pack() -> Pack {
    Pack {
        name: "lean".to_string(),
        surface: PackSurface {
            prompts: HashMap::new(),
            ..Default::default()
        },
        source: "builtin:lean".to_string(),
    }
}

/// 极简包的 how-to-compress 文本（可选追加语言保持规则）。
pub fn lean_how_to_compress(language_preservation: bool) -> String {
    if language_preservation {
        format!(
            "{}\n\n{}",
            texts::LEAN_HOW_TO_COMPRESS,
            texts::LANGUAGE_PRESERVATION_RULE
        )
    } else {
        texts::LEAN_HOW_TO_COMPRESS.to_string()
    }
}

/// 按名字解析包：先宿主提供的包，再内置注册表（`default` → `lean`）。
pub fn resolve_pack(name: &str, host_packs: &HashMap<String, Pack>) -> Option<Pack> {
    let trimmed = name.trim();
    if let Some(pack) = host_packs.get(trimmed) {
        return Some(pack.clone());
    }
    match trimmed {
        "default" => Some(default_pack()),
        "lean" => Some(lean_pack()),
        _ => None,
    }
}

pub fn list_builtin_packs() -> Vec<Pack> {
    vec![default_pack(), lean_pack()]
}

// ---------------------------------------------------------------------------
// parse-compress-input.ts
// ---------------------------------------------------------------------------

/// 一条解析出来的压缩区间。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressRangeSpec {
    pub start_ref: String,
    pub end_ref: String,
    pub summary: String,
    pub topic: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressParseKind {
    Ok,
    EmptyInput,
    NotObject,
    MissingContent,
    ContentNotArray,
    MalformedJson,
    Truncated,
    NoValidRanges,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressParseDiagnostics {
    pub ok: bool,
    pub kind: Option<CompressParseKind>,
    pub quote_salvage: bool,
    pub content_salvage: bool,
    pub raw_prefix: String,
    pub length: usize,
    pub keys: Vec<String>,
    pub invalid_items: usize,
    pub invalid_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedCompressInput {
    pub ranges: Vec<CompressRangeSpec>,
    pub diagnostics: CompressParseDiagnostics,
}

/// 解析 compress 工具参数（任意宿主 wire 形状）。
///
/// 接受：解码后的对象、JSON 字符串（可能带围栏 / 尾逗号 / 裸换行 / 双重 stringify）、
/// 或被截断的 JSON 前缀（救援模式）。非法条目跳过、绝不致命。
pub fn parse_compress_args(input: &serde_json::Value) -> ParsedCompressInput {
    let mut diagnostics = CompressParseDiagnostics::default();
    let value = match input {
        serde_json::Value::String(raw) => {
            diagnostics.raw_prefix = raw.chars().take(800).collect();
            diagnostics.length = raw.len();
            match parse_json_lenient(raw) {
                Some(value) => value,
                None => {
                    diagnostics.kind = Some(CompressParseKind::MalformedJson);
                    return ParsedCompressInput {
                        ranges: Vec::new(),
                        diagnostics,
                    };
                }
            }
        }
        other => other.clone(),
    };
    let Some(object) = value.as_object() else {
        diagnostics.kind = Some(CompressParseKind::NotObject);
        return ParsedCompressInput {
            ranges: Vec::new(),
            diagnostics,
        };
    };
    diagnostics.keys = object.keys().cloned().collect();
    let mut ranges: Vec<CompressRangeSpec> = Vec::new();
    // 扁平单区间形式（无 content）
    if !object.contains_key("content") {
        if let Some(range) = object_range(object) {
            ranges.push(range);
        } else {
            diagnostics.kind = Some(CompressParseKind::MissingContent);
            return ParsedCompressInput {
                ranges,
                diagnostics,
            };
        }
    } else {
        let content = object.get("content").unwrap();
        match content {
            serde_json::Value::Array(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    collect_entry(entry, index, &mut ranges, &mut diagnostics);
                }
            }
            serde_json::Value::String(raw) => {
                collect_line_form(raw, &mut ranges, &mut diagnostics);
            }
            serde_json::Value::Object(_) => {
                // 非数组 content（单个条目对象）→ 救援成规范数组形式（#470）
                diagnostics.content_salvage = true;
                collect_entry(content, 0, &mut ranges, &mut diagnostics);
            }
            serde_json::Value::Null => {
                diagnostics.kind = Some(CompressParseKind::EmptyInput);
            }
            _ => {
                diagnostics.kind = Some(CompressParseKind::ContentNotArray);
            }
        }
    }
    diagnostics.invalid_items = diagnostics.invalid_reasons.len();
    diagnostics.ok = !ranges.is_empty();
    if diagnostics.kind.is_none() {
        diagnostics.kind = Some(if ranges.is_empty() {
            CompressParseKind::NoValidRanges
        } else {
            CompressParseKind::Ok
        });
    }
    ParsedCompressInput {
        ranges,
        diagnostics,
    }
}

fn collect_entry(
    entry: &serde_json::Value,
    index: usize,
    ranges: &mut Vec<CompressRangeSpec>,
    diagnostics: &mut CompressParseDiagnostics,
) {
    match entry {
        serde_json::Value::Object(object) => match object_range(object) {
            Some(range) => ranges.push(range),
            None => diagnostics.invalid_reasons.push(format!(
                "entry {index}: missing range bounds (need startRef/startId and endRef/endId)"
            )),
        },
        serde_json::Value::String(raw) => collect_line_form(raw, ranges, diagnostics),
        _ => diagnostics
            .invalid_reasons
            .push(format!("entry {index}: not a range"),),
    }
}

fn object_range(object: &serde_json::Map<String, serde_json::Value>) -> Option<CompressRangeSpec> {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    let start = pick(&["startId", "startRef", "start", "from"])?;
    let end = pick(&["endId", "endRef", "end", "to"])?;
    let summary = pick(&["summary"]).unwrap_or_default();
    let topic = pick(&["topic", "title"]).unwrap_or_default();
    Some(CompressRangeSpec {
        start_ref: start,
        end_ref: end,
        summary,
        topic,
    })
}

/// 行形式：`m00150–m00220 optional topic` 之后的所有行都是摘要正文。
fn collect_line_form(
    raw: &str,
    ranges: &mut Vec<CompressRangeSpec>,
    diagnostics: &mut CompressParseDiagnostics,
) {
    let text = raw.trim();
    if text.is_empty() {
        return;
    }
    // 双重 stringify：整串是一个 JSON 数组。
    if text.starts_with('[') {
        if let Some(value) = parse_json_lenient(text) {
            if let serde_json::Value::Array(entries) = value {
                for (index, entry) in entries.iter().enumerate() {
                    collect_entry(entry, index, ranges, diagnostics);
                }
                return;
            }
        }
    }
    let mut current: Option<(String, String, String)> = None;
    for line in text.lines() {
        if let Some((start, end, topic)) = parse_range_header(line) {
            if let Some((s, e, t)) = current.take() {
                ranges.push(CompressRangeSpec {
                    start_ref: s,
                    end_ref: e,
                    summary: t.trim().to_string(),
                    topic: String::new(),
                });
            }
            current = Some((start, end, topic));
        } else if let Some((_, _, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((start, end, body)) = current {
        ranges.push(CompressRangeSpec {
            start_ref: start,
            end_ref: end,
            summary: body.trim().to_string(),
            topic: String::new(),
        });
    }
}

/// 解析 `m00150–m00220 optional topic` 头行（也接受 `-` / `..` / `→` 分隔）。
fn parse_range_header(line: &str) -> Option<(String, String, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('m') && !trimmed.starts_with('b') {
        return None;
    }
    let mut split_at = None;
    let chars: Vec<char> = trimmed.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if *ch == '–' || *ch == '—' {
            split_at = Some((index, 1));
            break;
        }
        if *ch == '-' && index > 0 {
            split_at = Some((index, 1));
            break;
        }
    }
    let (index, width) = split_at?;
    let start: String = chars[..index].iter().collect();
    let rest: String = chars[index + width..].iter().collect();
    let rest = rest.trim_start();
    let end_len = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .count();
    if end_len == 0 {
        return None;
    }
    let end: String = rest.chars().take(end_len).collect();
    let topic: String = rest.chars().skip(end_len).collect();
    let valid = |value: &str| {
        super::segments::parse_boundary(value).is_some()
    };
    if !valid(&start) || !valid(&end) {
        return None;
    }
    Some((start, end, topic.trim().to_string()))
}

/// 宽松 JSON：先去围栏与尾逗号，再试一次单引号修复。
pub fn parse_json_lenient(raw: &str) -> Option<serde_json::Value> {
    let mut text = raw.trim().to_string();
    for fence in ["```json", "```JSON", "```"] {
        if text.starts_with(fence) {
            text = text[fence.len()..].to_string();
        }
    }
    if text.ends_with("```") {
        text.truncate(text.len() - 3);
    }
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return Some(value);
    }
    let repaired = repair_trailing_commas(text);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&repaired) {
        return Some(value);
    }
    // 截断救援：补齐括号。
    let balanced = balance_brackets(&repaired);
    serde_json::from_str::<serde_json::Value>(&balanced).ok()
}

fn repair_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch == ',' {
            let mut lookahead = index + 1;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < chars.len() && (chars[lookahead] == '}' || chars[lookahead] == ']') {
                index += 1;
                continue;
            }
        }
        out.push(ch);
        index += 1;
    }
    out
}

fn balance_brackets(text: &str) -> String {
    let mut depth_obj = 0i32;
    let mut depth_arr = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth_obj += 1,
            '}' => depth_obj -= 1,
            '[' => depth_arr += 1,
            ']' => depth_arr -= 1,
            _ => {}
        }
    }
    let mut out = text.to_string();
    if in_string {
        out.push('"');
    }
    for _ in 0..depth_arr.max(0) {
        out.push(']');
    }
    for _ in 0..depth_obj.max(0) {
        out.push('}');
    }
    out
}

/// 没有显式 topic 的行形式条目的展示 / 检索标签：摘要的第一个 markdown 标题，否则
/// 第一行非空文本，截断。
pub fn derive_topic_from_summary(summary: &str) -> Option<String> {
    for line in summary.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let text = trimmed.trim_start_matches('#').trim();
        if text.is_empty() {
            continue;
        }
        let mut chars = text.chars();
        let mut out: String = chars.by_ref().take(60).collect();
        if chars.next().is_some() {
            out.push('…');
        }
        return Some(out);
    }
    None
}

// ---------------------------------------------------------------------------
// compress-tools.ts
// ---------------------------------------------------------------------------

/// 上游 ACP 工具名（Anthropic / OpenAI / Responses / Google 四种 wire 形状同一套名字）。
pub fn acp_tool_names() -> Vec<String> {
    texts::ACP_TOOL_NAMES.iter().map(|name| name.to_string()).collect()
}

pub fn acp_readonly_tools() -> Vec<String> {
    texts::ACP_READONLY_TOOLS
        .iter()
        .map(|name| name.to_string())
        .collect()
}

pub fn acp_mutating_tools() -> Vec<String> {
    texts::ACP_MUTATING_TOOLS
        .iter()
        .map(|name| name.to_string())
        .collect()
}

/// 上游某工具的 schema JSON（解析失败返回 `Null`）。
pub fn tool_schema(name: &str) -> serde_json::Value {
    let raw = match name {
        "compress" => texts::COMPRESS_TOOL_JSON,
        "decompress" => texts::DECOMPRESS_TOOL_JSON,
        "search_context" => texts::SEARCH_CONTEXT_TOOL_JSON,
        "acp_status" => texts::ACP_STATUS_TOOL_JSON,
        "acp_cache" => texts::ACP_CACHE_TOOL_JSON,
        "absorb" => texts::ABSORB_TOOL_JSON,
        "image_full" => texts::IMAGE_FULL_TOOL_JSON,
        "acp_retrieve" => texts::RETRIEVE_TOOL_JSON,
        _ => return serde_json::Value::Null,
    };
    serde_json::from_str(raw).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// search/
// ---------------------------------------------------------------------------

/// 角色权重：用户消息比工具输出更值得命中。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoleWeights {
    pub user: f64,
    pub assistant: f64,
    pub tool: f64,
    pub block: f64,
}

impl Default for RoleWeights {
    fn default() -> Self {
        Self {
            user: 1.5,
            assistant: 1.0,
            tool: 0.6,
            block: 1.0,
        }
    }
}

/// 默认检索算法：加权 BM25（词干）+ 模糊 n-gram，精确率与召回兼顾。
pub const DEFAULT_ALGORITHM: &str = "hybrid";

/// 可检索文档。
#[derive(Clone, Debug)]
pub struct SearchDoc {
    pub id: String,
    pub kind: String,
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub doc: SearchDoc,
    pub score: f64,
}

/// 内置算法及说明（`listSearchAlgorithms`）。
pub fn list_search_algorithms() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "substring",
            "Exact substring counting (original baseline). Predictable, no normalization.",
        ),
        (
            "bm25",
            "BM25 with stemming + CJK bigram tokenization. IR-standard relevance ranking.",
        ),
        (
            "fuzzy",
            "Character bigram overlap. Typo-tolerant, script-agnostic, high recall.",
        ),
        (
            "hybrid",
            "Weighted BM25(stem) + fuzzy n-gram. Default — best precision + recall.",
        ),
    ]
}

/// 文档特征：token 词频（BM25 用）与字符 bigram 集合（fuzzy 用）。
#[derive(Clone, Debug, Default)]
pub struct DocFeatures {
    pub tokens: HashMap<String, usize>,
    pub bigrams: Vec<String>,
    pub chars: usize,
}

fn normalize_token(raw: &str) -> String {
    let lowered = raw.to_ascii_lowercase();
    // 轻量词干：去掉常见的复数 / 时态后缀。
    for suffix in ["ing", "ers", "ies", "ed", "es", "s"] {
        if lowered.len() > suffix.len() + 2 && lowered.ends_with(suffix) {
            return lowered[..lowered.len() - suffix.len()].to_string();
        }
    }
    lowered
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let flush_cjk = |run: &mut Vec<char>, out: &mut Vec<String>| {
        if run.len() == 1 {
            out.push(run[0].to_string());
        } else {
            for window in run.windows(2) {
                out.push(window.iter().collect());
            }
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !cjk_run.is_empty() {
                flush_cjk(&mut cjk_run, &mut tokens);
            }
            current.push(ch);
            continue;
        }
        if is_cjk(ch) {
            if !current.is_empty() {
                tokens.push(normalize_token(&current));
                current.clear();
            }
            cjk_run.push(ch);
            continue;
        }
        if !current.is_empty() {
            tokens.push(normalize_token(&current));
            current.clear();
        }
        if !cjk_run.is_empty() {
            flush_cjk(&mut cjk_run, &mut tokens);
        }
    }
    if !current.is_empty() {
        tokens.push(normalize_token(&current));
    }
    if !cjk_run.is_empty() {
        flush_cjk(&mut cjk_run, &mut tokens);
    }
    tokens
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF)
}

pub fn doc_features(text: &str) -> DocFeatures {
    let mut features = DocFeatures {
        chars: text.chars().count(),
        ..Default::default()
    };
    for token in tokenize(text) {
        *features.tokens.entry(token).or_insert(0) += 1;
    }
    let chars: Vec<char> = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    for window in chars.windows(2) {
        features.bigrams.push(window.iter().collect());
    }
    features
}

fn role_weight(role: &str, kind: &str, weights: RoleWeights) -> f64 {
    if kind == "block" {
        return weights.block;
    }
    match role {
        "user" => weights.user,
        "assistant" => weights.assistant,
        "tool" => weights.tool,
        _ => 1.0,
    }
}

/// 检索压缩块摘要与消息（支持 `substring` / `bm25` / `fuzzy` / `hybrid`）。
pub fn search_blocks(
    query: &str,
    docs: &[SearchDoc],
    limit: usize,
    weights: RoleWeights,
) -> Vec<SearchResult> {
    let query_tokens = tokenize(query);
    let query_lower = query.to_ascii_lowercase();
    if query_lower.trim().is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let features: Vec<DocFeatures> = docs.iter().map(|doc| doc_features(&doc.text)).collect();
    let total_docs = docs.len() as f64;
    let avg_len = features.iter().map(|entry| entry.tokens.len()).sum::<usize>() as f64
        / total_docs.max(1.0);
    let mut document_frequency: HashMap<String, usize> = HashMap::new();
    for feature in features.iter() {
        for token in feature.tokens.keys() {
            *document_frequency.entry(token.clone()).or_insert(0) += 1;
        }
    }
    let query_bigrams: Vec<String> = {
        let chars: Vec<char> = query_lower.chars().filter(|ch| !ch.is_whitespace()).collect();
        chars.windows(2).map(|window| window.iter().collect()).collect()
    };

    let mut results: Vec<SearchResult> = Vec::new();
    for (index, doc) in docs.iter().enumerate() {
        let feature = &features[index];
        // substring（原始基线，可预测、无归一化）
        let substring = if query_lower.trim().is_empty() {
            0.0
        } else {
            doc.text.to_ascii_lowercase().matches(&query_lower).count() as f64
        };
        // BM25
        let mut bm25 = 0.0f64;
        let doc_len = feature.tokens.len().max(1) as f64;
        let k1 = 1.2f64;
        let b = 0.75f64;
        for token in query_tokens.iter() {
            let Some(frequency) = feature.tokens.get(token) else {
                continue;
            };
            let df = *document_frequency.get(token).unwrap_or(&1) as f64;
            let idf = ((total_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            let tf = *frequency as f64;
            bm25 += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * doc_len / avg_len.max(1.0)));
        }
        // fuzzy：字符 bigram 重叠（对拼写错误容错、脚本无关、高召回）
        let fuzzy = if query_bigrams.is_empty() {
            0.0
        } else {
            let hits = query_bigrams
                .iter()
                .filter(|bigram| feature.bigrams.contains(bigram))
                .count() as f64;
            hits / query_bigrams.len() as f64
        };
        let score = if bm25 > 0.0 || fuzzy > 0.0 {
            bm25 * 0.6 + fuzzy * 4.0 * 0.4
        } else {
            substring
        };
        if score <= 0.0 {
            continue;
        }
        results.push(SearchResult {
            doc: doc.clone(),
            score: score * role_weight(&doc.role, &doc.kind, weights),
        });
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.doc.id.cmp(&b.doc.id))
    });
    results.truncate(limit.max(1));
    results
}

/// 把消息列表转成可检索文档。
pub fn message_docs(messages: &[ChatContextMessage], refs: &[String]) -> Vec<SearchDoc> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| !message.content.trim().is_empty())
        .map(|(index, message)| SearchDoc {
            id: refs.get(index).cloned().unwrap_or_default(),
            kind: "message".to_string(),
            role: message.role.trim().to_string(),
            text: message.content.clone(),
        })
        .collect()
}
