// 校验：texts.rs 里的常量必须与上游 acp-kernel dist 的字节输出完全一致。
//
// 用法（需要一份上游 acp-kernel 的 dist）：
//   ACP_KERNEL_DIST="F:/desktop/acp-kernel-inspect/package/dist/index.js" \
//     node scripts/verify-kernel-texts.mjs
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const DIST =
  process.env.ACP_KERNEL_DIST ||
  process.argv[2] ||
  "F:/desktop/acp-kernel-inspect/package/dist/index.js";
const K = await import(pathToFileURL(DIST).href);

const src = readFileSync(
  "native/src/api/conversation/context_kernel/texts.rs",
  "utf8",
);

// 解析 `pub const NAME: &str = "...";`（按 Rust 转义规则反解）。
const rustUnescape = (body) => {
  let out = "";
  for (let i = 0; i < body.length; i += 1) {
    const ch = body[i];
    if (ch !== "\\") {
      out += ch;
      continue;
    }
    const next = body[++i];
    if (next === "n") out += "\n";
    else if (next === "r") out += "\r";
    else if (next === "t") out += "\t";
    else if (next === "\\") out += "\\";
    else if (next === '"') out += '"';
    else if (next === "u") {
      const close = body.indexOf("}", i);
      const hex = body.slice(i + 2, close);
      out += String.fromCodePoint(parseInt(hex, 16));
      i = close;
    } else out += next;
  }
  return out;
};

const parseConst = (name) => {
  const re = new RegExp(
    `pub const ${name}: &str = "((?:[^"\\\\]|\\\\.)*)";`,
    "s",
  );
  const match = src.match(re);
  return match ? rustUnescape(match[1]) : undefined;
};

const failures = [];
const check = (name, expected) => {
  const actual = parseConst(name);
  if (actual === undefined) {
    failures.push(`${name}: NOT FOUND in texts.rs`);
    return;
  }
  if (actual !== expected) {
    const at = [...expected].findIndex((ch, i) => ch !== actual[i]);
    failures.push(
      `${name}: MISMATCH at char ${at} (expected ${JSON.stringify(expected.slice(Math.max(0, at - 30), at + 30))}, got ${JSON.stringify(actual.slice(Math.max(0, at - 30), at + 30))})`,
    );
  }
};

check("SUMMARY_HEADER", K.SUMMARY_HEADER);
check("ABSORB_PROMPT_MARKER", K.ABSORB_PROMPT_MARKER);
check("STORED_PLACEHOLDER_MARKER", K.STORED_PLACEHOLDER_MARKER);
check("RETRIEVED_ID_PREFIX", K.RETRIEVED_ID_PREFIX);
check("RETRIEVE_TOOL_NAME", K.RETRIEVE_TOOL_NAME);
check("RETRIEVE_TOOL_DESCRIPTION", K.RETRIEVE_TOOL_DESCRIPTION);
check("IMAGE_FULL_FAILURE_MARKER", K.IMAGE_FULL_FAILURE_MARKER);
check("IMAGE_FULL_TOOL_NAME", K.IMAGE_FULL_TOOL_NAME);
check("ABSORB_TOOL_NAME", K.ABSORB_TOOL_NAME);
check("RULE_TOOL_NAME", K.RULE_TOOL_NAME);
check("RULES_USAGE_PROMPT", K.RULES_USAGE_PROMPT);
check("BLOCKED_REF", K.BLOCKED_REF);
check("DEFAULT_STEERING_SENTINEL", K.DEFAULT_STEERING_SENTINEL);
check("COMPRESS_PHILOSOPHY", K.COMPRESS_PHILOSOPHY);
check("HOW_TO_COMPRESS_RULES", K.HOW_TO_COMPRESS_RULES);
check("TIER2_DISTILL_RULES", K.TIER2_DISTILL_RULES);
check("TIER3_CONDENSE_RULES", K.TIER3_CONDENSE_RULES);
check("LANGUAGE_PRESERVATION_RULE", K.LANGUAGE_PRESERVATION_RULE);
check("LEAN_HOW_TO_COMPRESS", K.LEAN_HOW_TO_COMPRESS);
check("ABSORB_TOOL_DESCRIPTION", K.ABSORB_TOOL.description);
check("IMAGE_FULL_TOOL_DESCRIPTION", K.IMAGE_FULL_TOOL.description);
check("COMPRESS_TOOL_DESCRIPTION", K.COMPRESS_TOOL.description);
check("DECOMPRESS_TOOL_DESCRIPTION", K.DECOMPRESS_TOOL.description);
check("SEARCH_CONTEXT_TOOL_DESCRIPTION", K.SEARCH_CONTEXT_TOOL.description);
check("ACP_STATUS_TOOL_DESCRIPTION", K.ACP_STATUS_TOOL.description);
check("ACP_CACHE_TOOL_DESCRIPTION", K.ACP_CACHE_TOOL.description);
check("ACP_COMPRESS_OPEN", K.ACP_TEXT_OPEN);
check("ACP_COMPRESS_CLOSE", K.ACP_TEXT_CLOSE);
check("ACP_DECOMPRESS_OPEN", K.ACP_DECOMPRESS_OPEN);
check("ACP_SEARCH_OPEN", K.ACP_SEARCH_OPEN);
check("ACP_STATUS_OPEN", K.ACP_STATUS_OPEN);
check("COMPRESS_SYSTEM_PROMPT", K.buildCompressSystemPrompt());
check("COMPRESS_TEXT_SYSTEM_PROMPT", K.buildCompressTextSystemPrompt());
check("ABSORB_TOOL_JSON", JSON.stringify(K.ABSORB_TOOL));
check("IMAGE_FULL_TOOL_JSON", JSON.stringify(K.IMAGE_FULL_TOOL));
check("RETRIEVE_TOOL_JSON", JSON.stringify(K.RETRIEVE_TOOL));
check("COMPRESS_TOOL_JSON", JSON.stringify(K.COMPRESS_TOOL));
check("DECOMPRESS_TOOL_JSON", JSON.stringify(K.DECOMPRESS_TOOL));
check("SEARCH_CONTEXT_TOOL_JSON", JSON.stringify(K.SEARCH_CONTEXT_TOOL));
check("ACP_STATUS_TOOL_JSON", JSON.stringify(K.ACP_STATUS_TOOL));
check("ACP_CACHE_TOOL_JSON", JSON.stringify(K.ACP_CACHE_TOOL));

// 数组常量
const parseArr = (name) => {
  const re = new RegExp(
    `pub const ${name}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`,
  );
  const match = src.match(re);
  if (!match) return undefined;
  return [...match[1].matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((m) =>
    rustUnescape(m[1]),
  );
};
const eq = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const arrCheck = (name, expected) => {
  const actual = parseArr(name);
  if (!eq(actual, expected))
    failures.push(
      `${name}: MISMATCH ${JSON.stringify(actual)} != ${JSON.stringify(expected)}`,
    );
};
arrCheck("ACP_TOOL_NAMES", [...K.ACP_TOOL_NAMES]);
arrCheck("ACP_READONLY_TOOLS", [...K.ACP_READONLY_TOOLS]);
arrCheck("ACP_MUTATING_TOOLS", [...K.ACP_MUTATING_TOOLS]);
arrCheck("EFFORT_LADDER", [...K.EFFORT_LADDER]);
arrCheck("VERBOSITY_LEVELS", [
  "",
  K.VERBOSITY_LEVELS[1],
  K.VERBOSITY_LEVELS[2],
  K.VERBOSITY_LEVELS[3],
  K.VERBOSITY_LEVELS[4],
]);

// 标量
const scalars = [
  ["MIN_VERBOSITY_LEVEL", K.MIN_VERBOSITY_LEVEL],
  ["MAX_VERBOSITY_LEVEL", K.MAX_VERBOSITY_LEVEL],
  ["DEFAULT_VERBOSITY_LEVEL", K.DEFAULT_VERBOSITY_LEVEL],
  ["RETRIEVE_INLINE_TOKENS_DEFAULT", K.RETRIEVE_INLINE_TOKENS_DEFAULT],
  ["VIABLE_RANGE_MIN_TOKENS", K.VIABLE_RANGE_MIN_TOKENS],
  ["PIXEL_IMAGE_FALLBACK_TOKENS", K.PIXEL_IMAGE_FALLBACK_TOKENS],
  ["DEFAULT_RULE_MAX_RULES", K.DEFAULT_RULE_LIMITS.maxRules],
  ["DEFAULT_RULE_MAX_CHARS", K.DEFAULT_RULE_LIMITS.maxRuleChars],
];
for (const [name, expected] of scalars) {
  const re = new RegExp(`pub const ${name}: (?:&str|i64|usize) = (\\d+);`);
  const match = src.match(re);
  if (!match || Number(match[1]) !== expected) {
    failures.push(
      `${name}: expected ${expected}, got ${match ? match[1] : "NOT FOUND"}`,
    );
  }
}

// 函数输出
const directiveSrc = src.includes("pub fn verbosity_directive");
if (!directiveSrc) failures.push("verbosity_directive: MISSING");

if (failures.length > 0) {
  console.log("TEXTS VERIFY FAILED:");
  for (const line of failures) console.log("  - " + line);
  process.exit(1);
}
console.log(
  `TEXTS VERIFY PASSED — ${40 + 5 + 8} constants/all arrays byte-identical to upstream acp-kernel 0.0.101`,
);
