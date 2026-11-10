# 3-Config File Field Reference

This reference lists the field structure of every config file under
`~/.snow/`, for manual editing or for checking values written via the
`config-manage`. The `config-manage` scope whitelist matches this document:
file-backed settings generally **require an app restart or a UI re-save**
to take effect (`settings.mcpServers` is the exception — it syncs to the
app database automatically and takes effect immediately).

> Scope names for reading/writing each file with `config-manage` (`action=get` /
> `action=set`) are listed in [2-builtin-tools-reference](./2-builtin-tools-reference.md)
> under "File-backed scopes".

## 1. settings.json (scope: `settings`)

Global config shared with Snow CLI. Top-level fields are detailed in
[1-settings-json-reference](./1-settings-json-reference.md):

| Field                      | Type    | Description                                                                                                               |
| -------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------- |
| `mcpServers`               | object  | Global MCP servers, keyed by server name                                                                                  |
| `codebase`                 | object  | Codebase semantic-search config                                                                                           |
| `sensitiveCommands`        | array   | Sensitive-command interception rules                                                                                      |
| `yoloMode`                 | boolean | No-confirmation mode                                                                                                      |
| `vulnerabilityHuntingMode` | boolean | Vulnerability-hunting mode                                                                                                |
| `toolSearchEnabled`        | boolean | Legacy shared field; Snow App no longer reads this switch. Plugin tool search follows the project's enabled tool plugins. |
| `hybridCompressEnabled`    | boolean | Hybrid compression toggle                                                                                                 |
| `teamMode`                 | boolean | Team mode                                                                                                                 |
| `goal`                     | object  | Goal mode config (`defaultTokenBudgetM`: default token budget in millions)                                                |
| `ultraTodoEnabled`         | boolean | Ultra-long TODO list toggle                                                                                               |

## 2. config.json (scope: `snowcfg`)

API key and model config, stored under the `snowcfg` object (shared with
Snow CLI).

| Field                   | Type    | Sensitive | Description                                                                                                                                                                                                                         |
| ----------------------- | ------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `baseUrl`               | string  |           | Main service endpoint                                                                                                                                                                                                               |
| `baseUrlMode`           | string  |           | `auto` / `custom`                                                                                                                                                                                                                   |
| `apiKey`                | string  | 🔒        | Main provider key                                                                                                                                                                                                                   |
| `requestMethod`         | string  |           | Request method, e.g. `chat`                                                                                                                                                                                                         |
| `advancedModel`         | string  |           | Profile's default advanced model; ordinary conversations still use their selected model                                                                                                                                             |
| `basicModel`            | string  |           | Model for conversation titles, AI Commit, `@?` file search, and codebase Agent Review                                                                                                                                               |
| `supportsVision`        | boolean |           | Whether the main model supports vision                                                                                                                                                                                              |
| `visionBaseUrl`         | string  |           | Vision service endpoint                                                                                                                                                                                                             |
| `visionBaseUrlMode`     | string  |           | Vision endpoint mode                                                                                                                                                                                                                |
| `visionApiKey`          | string  | 🔒        | Vision service key                                                                                                                                                                                                                  |
| `visionRequestMethod`   | string  |           | Vision request method                                                                                                                                                                                                               |
| `visionModel`           | string  |           | Vision model name                                                                                                                                                                                                                   |
| `maxContextTokens`      | integer |           | Max context tokens                                                                                                                                                                                                                  |
| `maxTokens`             | integer |           | Max generation tokens per call                                                                                                                                                                                                      |
| `showThinking`          | boolean |           | Show thinking process                                                                                                                                                                                                               |
| `streamIdleTimeoutSec`  | integer |           | Streaming idle timeout (seconds), 60 when empty; the wait for the first data is widened automatically by request body size and grows with each idle-timeout retry; for large attachments or slow relays, 300 or more is recommended |
| `maxRetries`            | integer |           | Max request retries                                                                                                                                                                                                                 |
| `retryDelayMs`          | integer |           | Retry delay (ms); 0 uses exponential backoff                                                                                                                                                                                        |
| `enableAutoCompress`    | boolean |           | Auto-compress toggle                                                                                                                                                                                                                |
| `autoCompressThreshold` | integer |           | Auto-compress threshold (percent)                                                                                                                                                                                                   |
| `contextKernelEnabled`  | boolean |           | Context kernel (billion-context incremental folding) toggle; off by default. When on, normal conversations fold the outbound view on model-driven nudges and the legacy auto handoff is disabled                                    |
| `contextKernel`         | object  |           | Fine-grained context kernel configuration (missing fields take defaults; out-of-range values only make the kernel more conservative). See "Context kernel configuration" below                                                      |
| `toolResultTokenLimit`  | integer |           | Maximum percentage of the model context available to each tool result                                                                                                                                                               |
| `anthropicBeta`         | boolean |           | Anthropic Beta header toggle                                                                                                                                                                                                        |
| `streamingDisplay`      | boolean |           | Streaming display toggle                                                                                                                                                                                                            |
| `systemPromptId`        | string  |           | Active system prompt id                                                                                                                                                                                                             |
| `customHeadersSchemeId` | string  |           | Active custom-header scheme id                                                                                                                                                                                                      |
| `anthropicCacheTTL`     | string  |           | Anthropic cache TTL, e.g. `5m`                                                                                                                                                                                                      |
| `responsesReasoning`    | object  |           | Responses reasoning config (`{enabled, effort}`)                                                                                                                                                                                    |
| `responsesVerbosity`    | string  |           | Responses verbosity, e.g. `medium`                                                                                                                                                                                                  |
| `responsesFastMode`     | boolean |           | Responses fast mode                                                                                                                                                                                                                 |
| `responsesWebSocket`    | boolean |           | Responses WebSocket transport (persistent connection instead of SSE)                                                                                                                                                                |
| `chatThinking`          | object  |           | Thinking-strength config (`{enabled, reasoning_effort}`)                                                                                                                                                                            |

> **Model routing and profile completeness**: Snow App does not switch between
> `basicModel` and `advancedModel` based on prompt complexity. An ordinary
> conversation uses its selected model; `advancedModel` is only the bound profile's
> default advanced model. `basicModel` serves conversation titles, AI Commit, `@?`
> file search, and codebase Agent Review, and the two model classes never fall back
> to each other. Titles use the conversation-bound profile; if it is deleted,
> ordinary-conversation rules fall back to the active profile. An active profile
> requires both `advancedModel` and `basicModel` to be non-empty after trim; an
> inactive draft may leave them empty, and API-key presence is not part of this
> model-completeness check. Vision understanding, image generation, and embedding
> use independent configuration and channels.

### Context kernel configuration (`snowcfg.contextKernel`)

An object; every field may be omitted and then takes its default. Parse failures,
type mismatches and out-of-range values fall back to defaults — an out-of-range value
can only make the kernel more conservative, never break a protection rule.

| Field                    | Type     | Default                                                                                                    | Meaning                                                                               |
| ------------------------ | -------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `tailProtectedMessages`  | integer  | 6                                                                                                          | Protected tail message count (1–64)                                                   |
| `maxBlocks`              | integer  | 64                                                                                                         | Top-level block cap per session (1–512)                                               |
| `ccrMinChars`            | integer  | 4000                                                                                                       | Tool results above this size become CCR placeholders (1000–200000)                    |
| `ccrStoreMaxChars`       | integer  | 400000                                                                                                     | Content-store character cap                                                           |
| `crushMinChars`          | integer  | 12000                                                                                                      | Results above this size get typed compression (crush) first                           |
| `renderRefs`             | boolean  | true                                                                                                       | Render `[mNNNNN]` prefixes on user/assistant text in the outbound view                |
| `snapToTurns`            | boolean  | true                                                                                                       | Snap folding ranges to turn boundaries                                                |
| `viableRangeMinTokens`   | integer  | 800                                                                                                        | Minimum token size for a recommended range                                            |
| `outputSteering`         | boolean  | false                                                                                                      | Append the verbosity directive to the nudge                                           |
| `nudgeOverride`          | string   | empty                                                                                                      | Replace the nudge body (localization / team policy)                                   |
| `protectedTools`         | string[] | `[]`                                                                                                       | Extra protected tool names (trailing `*` glob, case-insensitive)                      |
| `protectedLatestTools`   | string[] | `[]`                                                                                                       | Protect only the newest call of a tool (cumulative snapshots, e.g. `todo_list`)       |
| `preserveRecentTools`    | string[] | `[]`                                                                                                       | Subtract these patterns from the built-in never-preserve-recent list (e.g. `read`)    |
| `preserveRecentMessages` | integer  | 5                                                                                                          | Soft-protected recent message count                                                   |
| `preserveRecentTokens`   | integer  | 5000                                                                                                       | Soft-protected recent token budget (expands backward)                                 |
| `modelContextLimit`      | integer  | 200000                                                                                                     | Model context limit (used for the emergency verdict)                                  |
| `compress`               | object   | `{minCompressRange:5000, maxSummaryLength:20000, minSummaryLength:50}`                                     | Compression gate; `minCompressRange` counts **real characters** (validated per batch) |
| `tiers`                  | object   | `{enabled:true, tier2Trigger:1000, tier3Trigger:2000}`                                                     | Tiered distillation triggers                                                          |
| `nudge`                  | object   | upstream defaults                                                                                          | Nudge cadence and growth gates (`maxContextLimitPct`, `emergencyThresholdPct`, …)     |
| `truncate`               | object   | `{threshold:0.95, terminalEscapeAfter:3}`                                                                  | Emergency truncation                                                                  |
| `absorb`                 | object   | `{enabled:false, toolName:"absorb", minToolTokens:4000, ...}`                                              | Instant tool-result absorption                                                        |
| `crush`                  | object   | `{enabled:false, minReduction:0.1}`                                                                        | Typed deterministic compression                                                       |
| `imageCompression`       | object   | `{enabled:false, minTokens:512, maxDimension:1280, quality:80, format:"webp"}`                             | Screenshot downscaling                                                                |
| `ccr`                    | object   | `{enabled:false, toolName:"acp_retrieve", minToolTokens:4000, maxHeadChars:96, retrieveInlineTokens:4000}` | Content-cached retrieval                                                              |
| `outputSteeringSettings` | object   | `{enabled:false, verbosityLevel:2, effortRouting:true}`                                                    | Output steering (verbosity L0–L4 plus clamp-only effort routing)                      |
| `rules`                  | object   | `{maxRules:50, maxRuleChars:300}`                                                                          | Persistent rule limits                                                                |

The kernel follows the upstream `acp-kernel` 0.0.101 module semantics (`refs` /
`segment` / `viable` / `boundaries` / `block-map` / `tool-pairs` / `turn-integrity` /
`protected` / `recommend` / `nudge-text` / `output-steering` / `report` / `handoff` /
`rules`). The byte-stable texts (verbosity directives, placeholders, absorb /
image_full notices, compression rules, tool schemas) are exported straight from the
upstream dist by `gen-kernel-texts.mjs` and must not be hand-edited.

## 3. proxy-config.json (scope: `proxy`)

Proxy and browser config.

| Field              | Type    | Description                               |
| ------------------ | ------- | ----------------------------------------- |
| `enabled`          | boolean | Proxy toggle                              |
| `host`             | string  | Proxy host, e.g. `127.0.0.1`              |
| `port`             | integer | Proxy port, e.g. `7890`                   |
| `searchEngine`     | string  | Search engine, e.g. `bing` / `duckduckgo` |
| `browserPath`      | string  | Browser executable path                   |
| `browserDebugPort` | integer | Browser debug port                        |

> `blockedPatterns` is not a file field in the current `proxy` scope or in `~/.snow/proxy-config.json`, so it must not be maintained with `config-manage action=set scope=proxy`. It is stored in the app database system setting `proxy_browser_settings`; use the Proxy & Browser settings panel or `config-manage` (`action=get` / `action=set` with `scope=appSettings key=blockedPatterns`).

## 4. active-profile.json (scope: `app`)

The active API profile.

| Field           | Type   | Description             |
| --------------- | ------ | ----------------------- |
| `activeProfile` | string | The active profile name |

## 5. custom-headers.json (scope: `custom-headers`)

Custom request-header schemes (🔒 masked on read).

> **This file is a Snow CLI compatibility SYNC SOURCE, not the app's runtime
> configuration.** At runtime Snow App injects headers from the app database table
> `custom_header_schemes` (same source as the Settings → Custom headers panel).
> Writing this file alone does not change what the app sends; the user must click
> "Sync Snow CLI custom headers" in the panel to import it. To let an agent change
> the app's live headers directly, use `config-manage action=set scope=customHeaderSchemes`
> (see [built-in tools reference](2-builtin-tools-reference.md#config)).

| Field     | Type   | Sensitive | Description                                                                                                                                         |
| --------- | ------ | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `active`  | string |           | Active scheme id                                                                                                                                    |
| `schemes` | array  | 🔒        | Scheme array, items: `{id, name, headers, createdAt}`; `headers` is a `{header: value}` map (may contain sensitive headers such as `Authorization`) |

Header values may use the session-ID placeholder (six equivalent spellings: `{{session_id}}`, `{{sessionId}}`, `{{conversation_id}}`, `{{conversationId}}`, `${session_id}`, `${conversation_id}`); it is expanded to the session ID of the request's conversation when the request is sent and can be combined with other text (e.g. `snow-app-{{session_id}}`). Requests without a session context (e.g. model listing) omit that header (see the personalization, theme, and shortcuts guide).

## 6. system-prompt.json (scope: `system-prompt`)

System-prompt library (🔒 masked on read).

| Field     | Type  | Sensitive | Description                                           |
| --------- | ----- | --------- | ----------------------------------------------------- |
| `active`  | array |           | List of active prompt ids                             |
| `prompts` | array | 🔒        | Prompt array, items: `{id, name, content, createdAt}` |

## 7. theme.json (scope: `theme`)

UI theme config.

| Field                 | Type    | Description                                |
| --------------------- | ------- | ------------------------------------------ |
| `theme`               | string  | Theme name, e.g. `custom`                  |
| `simpleMode`          | boolean | Simple mode                                |
| `diffOpacity`         | number  | Diff-view opacity (float, e.g. `0.62`)     |
| `toolDisplayMode`     | string  | Tool-call display mode, e.g. `full`        |
| `thinkDisplayMode`    | string  | Thinking display mode                      |
| `subAgentDisplayMode` | string  | Sub-agent display mode, e.g. `slots`       |
| `toolIcons`           | object  | Tool icon mapping (`{toolName: iconChar}`) |
| `customColors`        | object  | Custom color scheme                        |

## 8. language.json (scope: `language`)

UI language.

| Field      | Type   | Description                                                                                                                         |
| ---------- | ------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| `language` | string | Language code; supports `en` / `zh-CN` / `zh-TW`. Older installs may store the legacy value `zh` (equivalent to Simplified Chinese) |

## 9. permissions.json (scope: `permissions`)

Tool authorization config.

| Field                 | Type  | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| --------------------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `alwaysApprovedTools` | array | Always-approved tool names (e.g. `["bash-terminal-execute", "filesystem-read"]`). **Always use the FULL tool name** (MCP prefix + tool name, e.g. `bash-terminal-execute`, `filesystem-read`); a legacy unprefixed name such as `terminal-execute` matches no tool and the entry silently does nothing. Applies to all projects; the UI authorization flow reads this list and merges it with project-level approvals. It is also shown (read-only) in the Project tool permissions panel. Bash commands matching sensitive-command rules still force confirmation regardless of this list |

## 10. lsp-config (scope: `lsp-config`)

LSP (Language Server Protocol) server configuration: declares the external
language servers Snow App can launch per language, with their launch
arguments.

> **Current status: DB-backed config scope (app database `lsp_server_configs`
> table).** There is no `~/.snow/lsp-config.json` file anymore; the
> `config-manage` tool hits the table directly (effective immediately, no
> restart). The legacy file is imported once on first start (source=legacy).
> `config-manage` with `action=set` is full-replacement (same semantics as the
> old file) with deep validation.
> Servers are consumed by the `lsp` MCP service (`lsp-diagnostics` /
> `lsp-hover`, see
> [2-guides/7-codebase-index-and-diagnostics](../2-guides/7-codebase-index-and-diagnostics.md)).

| Field           | Type    | Description                                                                       |
| --------------- | ------- | --------------------------------------------------------------------------------- |
| `schemaVersion` | integer | Schema version                                                                    |
| `servers`       | object  | Language-server map, keyed by language (e.g. `typescript`); value structure below |

`servers.<language>` value structure (deeply validated on write per the rules
below; unknown fields are allowed for forward compatibility):

| Field                   | Type            | Description                                                                                        |
| ----------------------- | --------------- | -------------------------------------------------------------------------------------------------- |
| `command`               | string          | Executable that launches the language server (e.g. `typescript-language-server`)                   |
| `args`                  | array\<string\> | Launch arguments (e.g. `["--stdio"]`)                                                              |
| `fileExtensions`        | array\<string\> | File extensions this server handles (e.g. `[".ts", ".tsx"]`)                                       |
| `installCommand`        | string          | Install command used when the server is missing (e.g. `npm install -g typescript-language-server`) |
| `initializationOptions` | object          | Initialization options sent with the LSP `initialize` request                                      |

Example:

```json
{
  "schemaVersion": 1,
  "servers": {
    "typescript": {
      "command": "typescript-language-server",
      "args": ["--stdio"],
      "fileExtensions": [".ts", ".tsx", ".js", ".jsx"],
      "installCommand": "npm install -g typescript-language-server typescript",
      "initializationOptions": {}
    }
  }
}
```

Read/write via the `config-manage` tool (key-level replace, deep validation before
write, automatic backup and atomic write):

```text
config-manage action=get    scope="lsp-config" key="servers"              # read current config
config-manage action=list   scope="lsp-config"                             # list keys and current values
config-manage action=set    scope="lsp-config" key="servers" value={...}   # write (field types validated)
config-manage action=delete scope="lsp-config" key="servers"              # delete (requires confirmation)
```

> As with other file-backed scopes, changes may need an app restart or a UI
> re-save to take effect (see the note at the top of this document);
> `lsp-config` is not project-scoped (no `projectId` semantics).

## 11. buddy.json (scope: `buddy`)

App companion (mascot) config.

| Field       | Type    | Description                                                                                  |
| ----------- | ------- | -------------------------------------------------------------------------------------------- |
| `version`   | integer | Schema version                                                                               |
| `companion` | object  | Companion appearance/personality config (`{rarity, species, name, personality, stats, ...}`) |
| `muted`     | boolean | Whether muted                                                                                |
