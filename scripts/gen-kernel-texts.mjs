// 代码生成器：把上游 acp-kernel 的字节稳定常量写成 Rust 源码。
//
// 用法（需要一份上游 acp-kernel 的 dist）：
//   ACP_KERNEL_DIST="F:/desktop/acp-kernel-inspect/package/dist/index.js" \
//     node scripts/gen-kernel-texts.mjs native/src/api/conversation/context_kernel/texts.rs
//
// 不传第二个参数时输出到 stdout 之外的同名 `texts.rs`（当前工作目录）。
import { writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const DIST =
  process.env.ACP_KERNEL_DIST ||
  process.argv[3] ||
  "F:/desktop/acp-kernel-inspect/package/dist/index.js";
const K = await import(pathToFileURL(DIST).href);

const rustStr = (s) => {
  let out = '"';
  for (const ch of s) {
    if (ch === "\\") out += "\\\\";
    else if (ch === '"') out += '\\"';
    else if (ch === "\n") out += "\\n";
    else if (ch === "\r") out += "\\r";
    else if (ch === "\t") out += "\\t";
    else if (ch.codePointAt(0) < 0x20)
      out += "\\u{" + ch.codePointAt(0).toString(16) + "}";
    else out += ch;
  }
  return out + '"';
};

const L = [];
const p = (s = "") => L.push(s);

p("//! 上游 `acp-kernel` 的**字节稳定**常量（禁止手改）。");
p("//!");
p("//! 由 `gen-kernel-texts.mjs` 从 acp-kernel 的 dist 直接导出生成；");
p("//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。");
p("//!");
p("//! 注意：这些文本直接进入前缀缓存敏感的位置（系统提示词尾部、工具结果、");
p("//! 占位符）。改动其中任何一个字符都会让所有会话的前缀缓存失效。");
p("#![allow(dead_code)]");
p();
const toArr = (v) =>
  Array.isArray(v) ? v : v instanceof Set ? [...v] : Object.keys(v ?? {});

p(`/// 生成时使用的上游版本。`);
p(`pub const UPSTREAM_KERNEL_VERSION: &str = ${rustStr("0.0.101")};`);
p();
p();

const strConst = (name, value) => {
  p(`pub const ${name}: &str = ${rustStr(value)};`);
};
const arrConst = (name, values) => {
  p(`pub const ${name}: &[&str] = &[`);
  for (const v of toArr(values)) p(`    ${rustStr(v)},`);
  p("];");
};

strConst("SUMMARY_HEADER", K.SUMMARY_HEADER);
strConst("ABSORB_PROMPT_MARKER", K.ABSORB_PROMPT_MARKER);
strConst("STORED_PLACEHOLDER_MARKER", K.STORED_PLACEHOLDER_MARKER);
strConst("RETRIEVED_ID_PREFIX", K.RETRIEVED_ID_PREFIX);
strConst("RETRIEVE_TOOL_NAME", K.RETRIEVE_TOOL_NAME);
strConst("RETRIEVE_TOOL_DESCRIPTION", K.RETRIEVE_TOOL_DESCRIPTION);
strConst("IMAGE_FULL_FAILURE_MARKER", K.IMAGE_FULL_FAILURE_MARKER);
strConst("IMAGE_FULL_TOOL_NAME", K.IMAGE_FULL_TOOL_NAME);
strConst("ABSORB_TOOL_NAME", K.ABSORB_TOOL_NAME);
strConst("RULE_TOOL_NAME", K.RULE_TOOL_NAME);
strConst("RULES_USAGE_PROMPT", K.RULES_USAGE_PROMPT);
strConst("BLOCKED_REF", K.BLOCKED_REF);
strConst("DEFAULT_STEERING_SENTINEL", K.DEFAULT_STEERING_SENTINEL);
strConst("COMPRESS_PHILOSOPHY", K.COMPRESS_PHILOSOPHY);
strConst("HOW_TO_COMPRESS_RULES", K.HOW_TO_COMPRESS_RULES);
strConst("TIER2_DISTILL_RULES", K.TIER2_DISTILL_RULES);
strConst("TIER3_CONDENSE_RULES", K.TIER3_CONDENSE_RULES);
strConst("LANGUAGE_PRESERVATION_RULE", K.LANGUAGE_PRESERVATION_RULE);
strConst("LEAN_HOW_TO_COMPRESS", K.LEAN_HOW_TO_COMPRESS);
strConst("ABSORB_TOOL_DESCRIPTION", K.ABSORB_TOOL.description);
strConst("IMAGE_FULL_TOOL_DESCRIPTION", K.IMAGE_FULL_TOOL.description);
strConst("COMPRESS_TOOL_DESCRIPTION", K.COMPRESS_TOOL.description);
strConst("DECOMPRESS_TOOL_DESCRIPTION", K.DECOMPRESS_TOOL.description);
strConst("SEARCH_CONTEXT_TOOL_DESCRIPTION", K.SEARCH_CONTEXT_TOOL.description);
strConst("ACP_STATUS_TOOL_DESCRIPTION", K.ACP_STATUS_TOOL.description);
strConst("ACP_CACHE_TOOL_DESCRIPTION", K.ACP_CACHE_TOOL.description);
strConst("ACP_COMPRESS_OPEN", K.ACP_TEXT_OPEN);
strConst("ACP_COMPRESS_CLOSE", K.ACP_TEXT_CLOSE);
strConst("ACP_DECOMPRESS_OPEN", K.ACP_DECOMPRESS_OPEN);
strConst("ACP_SEARCH_OPEN", K.ACP_SEARCH_OPEN);
strConst("ACP_STATUS_OPEN", K.ACP_STATUS_OPEN);
p();

p("/// 工具名（上游 ACP 工具集）。");
arrConst("ACP_TOOL_NAMES", K.ACP_TOOL_NAMES);
arrConst("ACP_READONLY_TOOLS", K.ACP_READONLY_TOOLS);
arrConst("ACP_MUTATING_TOOLS", K.ACP_MUTATING_TOOLS);
p();

p("/// 简洁度阶梯 L1–L4（索引 0 为 L0 = 不下发指令）。");
p("pub const VERBOSITY_LEVELS: &[&str] = &[");
p(`    ${rustStr("")},`);
for (let i = 1; i <= 4; i += 1) p(`    ${rustStr(K.VERBOSITY_LEVELS[i])},`);
p("];");
p();
p("/// 共享 effort 阶梯（升序）。");
arrConst("EFFORT_LADDER", K.EFFORT_LADDER);
p();

p("pub const MIN_VERBOSITY_LEVEL: i64 = " + K.MIN_VERBOSITY_LEVEL + ";");
p("pub const MAX_VERBOSITY_LEVEL: i64 = " + K.MAX_VERBOSITY_LEVEL + ";");
p(
  "pub const DEFAULT_VERBOSITY_LEVEL: i64 = " + K.DEFAULT_VERBOSITY_LEVEL + ";",
);
p(
  "pub const RETRIEVE_INLINE_TOKENS_DEFAULT: i64 = " +
    K.RETRIEVE_INLINE_TOKENS_DEFAULT +
    ";",
);
p(
  "pub const VIABLE_RANGE_MIN_TOKENS: i64 = " + K.VIABLE_RANGE_MIN_TOKENS + ";",
);
p(
  "pub const PIXEL_IMAGE_FALLBACK_TOKENS: i64 = " +
    K.PIXEL_IMAGE_FALLBACK_TOKENS +
    ";",
);
p(
  `pub const DEFAULT_RULE_MAX_RULES: usize = ${K.DEFAULT_RULE_LIMITS.maxRules};`,
);
p(
  `pub const DEFAULT_RULE_MAX_CHARS: usize = ${K.DEFAULT_RULE_LIMITS.maxRuleChars};`,
);
p();

p("/// 全量系统提示词（上游 `buildCompress*SystemPrompt` 的字节输出）。");
strConst("COMPRESS_SYSTEM_PROMPT", K.buildCompressSystemPrompt());
strConst("COMPRESS_TEXT_SYSTEM_PROMPT", K.buildCompressTextSystemPrompt());
p();

p("/// 默认配置（上游 `defaultConfig(200000)` 的 JSON 快照）。");
strConst("DEFAULT_CONFIG_JSON", JSON.stringify(K.defaultConfig(200000)));
p();

p("/// 上游 tool JSON schema（Anthropic 形状）。");
for (const [name, tool] of [
  ["ABSORB_TOOL_JSON", K.ABSORB_TOOL],
  ["IMAGE_FULL_TOOL_JSON", K.IMAGE_FULL_TOOL],
  ["RETRIEVE_TOOL_JSON", K.RETRIEVE_TOOL],
  ["COMPRESS_TOOL_JSON", K.COMPRESS_TOOL],
  ["DECOMPRESS_TOOL_JSON", K.DECOMPRESS_TOOL],
  ["SEARCH_CONTEXT_TOOL_JSON", K.SEARCH_CONTEXT_TOOL],
  ["ACP_STATUS_TOOL_JSON", K.ACP_STATUS_TOOL],
  ["ACP_CACHE_TOOL_JSON", K.ACP_CACHE_TOOL],
]) {
  strConst(name, JSON.stringify(tool));
}
p();

p("/// 简洁度指令：level 0 / 越界 → `None`。");
p("pub fn verbosity_directive(level: i64) -> Option<&'static str> {");
p("    if level <= 0 || level > 4 {");
p("        return None;");
p("    }");
p("    Some(VERBOSITY_LEVELS[level as usize])");
p("}");
p();
p("/// 用 sentinel 包裹的引导块：level 0 → `None`。");
p(
  "pub fn render_steering_block(level: i64, sentinel: &str) -> Option<String> {",
);
p("    let directive = verbosity_directive(level)?;");
p(
  "    Some(format!(\"{sentinel}\\n{directive}\\n</{}>\", sentinel.trim_start_matches('<').trim_end_matches('>')));",
);
p("}");
p();

writeFileSync(process.argv[2] ?? "texts.rs", L.join("\n") + "\n", "utf8");
console.error("wrote", L.length, "lines");
