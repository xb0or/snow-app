use super::*;

use std::path::PathBuf;

use napi::bindgen_prelude::*;

use super::super::builtin::{get_builtin_servers_with_tools, get_builtin_tools};
use super::super::servers::skills::SkillsService;
use super::super::servers::sub_agents::{sub_agent_comms_tools, SUB_AGENT_COMMS_TOOL_FULL_NAMES};
use crate::storage::services::system_settings::{McpGlobalScopeSettings, McpProjectScopeSettings};

pub async fn collect_all_mcp_tools(
    project_id: Option<&str>,
    include_plan_mode_tool: bool,
) -> Result<Vec<McpTool>> {
    let scope = load_project_scope(project_id).await?;
    let global_scope = load_global_scope().await?;

    // Determine whether the codebase search tool should be included.
    // It requires: (1) a project id, (2) codebase enabled in project scope,
    // and (3) at least one embedded chunk in the vector table.
    let codebase_available = is_codebase_available(project_id).await?;

    // Image generation tool is only exposed when at least one channel
    // (OpenAI / Gemini) is configured and enabled in Settings -> Image
    // generation; when both are unconfigured the tool disappears entirely.
    // The non-sensitive summary of the current default channel is also
    // loaded here (single blocking read) and injected into the tool
    // definition so the agent sees the real model/provider instead of
    // guessing from static text (issue #63).
    let imagegen_context =
        tokio::task::spawn_blocking(|| crate::mcp::servers::imagegen::default_channel_context())
            .await
            .map_err(|error| {
                Error::new(
                    Status::GenericFailure,
                    format!("Failed to check image generation configuration: {error}"),
                )
            })??;
    let imagegen_configured = imagegen_context.is_some();

    let mut tools = get_builtin_tools()
        .into_iter()
        .filter(|tool| {
            // The dedicated approval tool is request-scoped: it must only be
            // exposed to the model while the current request is in Plan Mode.
            if tool.full_name() == REQUEST_APPROVAL_FULL_NAME {
                return include_plan_mode_tool;
            }
            // Sub-agent teammate communication tools are only exposed inside
            // sub-agent contexts (collect_allowed_mcp_tools appends them);
            // the main conversation never sees them.
            if SUB_AGENT_COMMS_TOOL_FULL_NAMES.contains(&tool.full_name().as_str()) {
                return false;
            }
            // Exclude codebase search tool unless the project has codebase
            // enabled and an existing index.
            if tool.server_id == "codebase" && !codebase_available {
                return false;
            }
            // Exclude image generation when no channel is configured.
            if tool.server_id == "imagegen" && !imagegen_configured {
                return false;
            }
            tool_is_enabled(tool, global_scope.as_ref(), scope.as_ref())
        })
        .collect::<Vec<_>>();

    // Inject the current default image channel summary (non-sensitive, no
    // API key) into the imagegen-generate description so the agent can see
    // the actual configured channel/provider/model/size/quality.
    if let Some(context) = imagegen_context {
        if let Some(tool) = tools.iter_mut().find(|tool| {
            tool.server_id == "imagegen" && tool.name == super::super::servers::imagegen::TOOL_GENERATE
        }) {
            tool.description =
                format!("{}\n\nCurrent configuration:\n{}", tool.description, context);
        }
    }

    if let Some(skill_tool) = SkillsService::new().tool(project_id).await? {
        if tool_is_enabled(&skill_tool, global_scope.as_ref(), scope.as_ref()) {
            tools.push(skill_tool);
        }
    }

    match super::super::external::discover_tools(project_id, scope.as_ref()).await {
        // External tools are already filtered by the project scope inside
        // discover_tools; apply the global blacklist on top.
        Ok(external_tools) => tools.extend(
            external_tools
                .into_iter()
                .filter(|tool| tool_is_enabled(tool, global_scope.as_ref(), None)),
        ),
        Err(error) => eprintln!("Failed to discover external MCP tools: {error}"),
    }
    Ok(tools)
}
/// Check whether the codebase search tool should be available for the
/// given project: the project must have codebase enabled AND have at
/// least one embedded chunk in its vector table.
async fn is_codebase_available(project_id: Option<&str>) -> Result<bool> {
    let Some(project_id) = project_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(false);
    };

    let project_id = project_id.to_string();
    let storage_info = crate::storage::initialize_app_storage()?;
    let database_path = PathBuf::from(storage_info.database_path);

    tokio::task::spawn_blocking(move || {
        let scope = crate::storage::services::system_settings::get_codebase_project_scope_settings(
            &database_path,
            &project_id,
        )?;
        if !scope.enabled.unwrap_or(false) {
            return Ok(false);
        }
        match crate::storage::services::codebase_index::get_index_stats(&database_path, &project_id)
        {
            Ok(stats) => Ok(stats.total_chunks > 0),
            Err(_) => Ok(false),
        }
    })
    .await
    .map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to check codebase availability: {error}"),
        )
    })?
}

pub async fn collect_allowed_mcp_tools(
    project_id: Option<&str>,
    tools_json: &str,
    allow_wildcard: bool,
) -> Result<Vec<McpTool>> {
    let configured_names = serde_json::from_str::<Vec<String>>(tools_json).map_err(|error| {
        Error::new(
            Status::InvalidArg,
            format!("Sub-agent tools configuration must be a JSON string array: {error}"),
        )
    })?;
    let configured_names = configured_names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<std::collections::HashSet<_>>();
    let wildcard_enabled = configured_names.contains("*");
    if wildcard_enabled && !allow_wildcard {
        return Err(Error::new(
            Status::InvalidArg,
            "Only built-in sub-agents may enable the wildcard tool configuration".to_string(),
        ));
    }

    let all_tools = collect_all_mcp_tools(project_id, false).await?;
    if wildcard_enabled {
        // Every sub-agent carries the teammate communication tools by default,
        // even with the wildcard configuration.
        let mut result = all_tools;
        result.extend(sub_agent_comms_tools());
        return Ok(result);
    }

    // 部分工具不可用（被项目 scope 禁用、默认禁用未启用、条件工具如
    // codebase/imagegen 未就绪、外部 MCP 服务器禁用或连接失败）时，跳过
    // 不可用工具、保留可用工具，而不是整体失败。整体失败会让 provider
    // 层把子代理请求静默降级为无工具（tools=None），模型只能把工具调用
    // 输出为纯文本（表现为"输出奇怪的 tool_call 文本后立即结束"）。
    let available_names = all_tools
        .iter()
        .map(McpTool::full_name)
        .collect::<std::collections::HashSet<_>>();
    let unavailable_names = configured_names
        .difference(&available_names)
        .filter(|name| !SUB_AGENT_COMMS_TOOL_FULL_NAMES.contains(&name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unavailable_names.is_empty() {
        eprintln!(
            "Sub-agent configured tools are unavailable or disabled for the current project (skipped): {}",
            unavailable_names.join(", ")
        );
    }

    let mut result = all_tools
        .into_iter()
        .filter(|tool| configured_names.contains(&tool.full_name()))
        .collect::<Vec<_>>();
    // Teammate communication tools are always available to every sub-agent,
    // regardless of its configured tool whitelist.
    result.extend(sub_agent_comms_tools());
    Ok(result)
}

/// Built-in server ids that are disabled by default and must be explicitly
/// enabled per project. This keeps their tools out of the model context
/// (saving tokens) until the user opts in.
const DEFAULT_DISABLED_SERVER_IDS: &[&str] = &["terminal"];

fn tool_is_enabled(
    tool: &McpTool,
    global_scope: Option<&McpGlobalScopeSettings>,
    scope: Option<&McpProjectScopeSettings>,
) -> bool {
    // The global blacklist has the highest priority: a tool disabled
    // globally stays disabled regardless of project scope.
    if global_scope
        .is_some_and(|global| global.disabled_tool_names.contains(&tool.full_name()))
    {
        return false;
    }
    // Default-disabled servers are excluded when there is no project
    // scope (no project context = user hasn't opted in).
    if DEFAULT_DISABLED_SERVER_IDS.contains(&tool.server_id.as_str()) {
        let Some(scope) = scope else {
            return false;
        };
        return scope.is_server_enabled(&builtin_scope_server_id(&tool.server_id))
            && scope.is_tool_enabled(&tool.full_name());
    }

    let Some(scope) = scope else {
        return true;
    };

    scope.is_server_enabled(&builtin_scope_server_id(&tool.server_id))
        && scope.is_tool_enabled(&tool.full_name())
}

pub(crate) fn builtin_scope_server_id(server_id: &str) -> String {
    format!("builtin:{server_id}")
}

pub(crate) fn server_id_from_tool_name(tool_name: &str) -> Option<&str> {
    split_tool_full_name(tool_name).map(|(server_id, _)| server_id)
}

pub(crate) fn builtin_server_name(server_id: &str) -> &str {
    match server_id {
        "filesystem" => "Filesystem",
        "bash" => "Terminal",
        "todo" => "TODO",
        "grep" => "Search",
        "websearch" => "Web search",
        "browser" => "Browser",
        "user-interaction" => "User interaction",
        "app-control" => "App Control",
        "sub-agents" => "Sub-agents",
        "codebase" => "Codebase",
        "codelens" => "CodeLens",
        "terminal" => "Terminal Control",
        "config" => "Config",
        "imagegen" => "Image Generation",
        _ => server_id,
    }
}

pub(crate) async fn ensure_project_tool_enabled(
    project_id: Option<&str>,
    tool_name: &str,
) -> Result<()> {
    let global_scope = load_global_scope().await?;
    if global_scope.is_some_and(|scope| scope.disabled_tool_names.contains(tool_name)) {
        return Err(Error::new(
            Status::GenericFailure,
            format!("MCP tool is disabled globally: {tool_name}"),
        ));
    }
    let Some(scope) = load_project_scope(project_id).await? else {
        return Ok(());
    };
    let Some(server_id) = server_id_from_tool_name(tool_name) else {
        return Err(Error::new(
            Status::InvalidArg,
            format!("Invalid MCP tool name: {tool_name}"),
        ));
    };
    let (server_scope_id, project_owned) = if server_id == "skills"
        || get_builtin_servers_with_tools()
            .iter()
            .any(|(builtin_server_id, _)| builtin_server_id == server_id)
    {
        (builtin_scope_server_id(server_id), false)
    } else {
        let resolved_server = super::super::external::resolve_project_scope_server(project_id, tool_name)
            .await?
            .ok_or_else(|| {
                Error::new(
                    Status::GenericFailure,
                    format!("MCP tool is no longer available: {tool_name}"),
                )
            })?;
        (
            resolved_server.scope_server_id,
            resolved_server.project_owned,
        )
    };

    if !project_owned && !scope.is_server_enabled(&server_scope_id) {
        return Err(Error::new(
            Status::GenericFailure,
            format!("MCP server is disabled for the current project: {server_scope_id}"),
        ));
    }
    if !scope.is_tool_enabled(tool_name) {
        return Err(Error::new(
            Status::GenericFailure,
            format!("MCP tool is disabled for the current project: {tool_name}"),
        ));
    }

    Ok(())
}

pub(crate) async fn load_project_scope(project_id: Option<&str>) -> Result<Option<McpProjectScopeSettings>> {
    let Some(project_id) = project_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let project_id = project_id.to_string();
    with_database_path(move |database_path| {
        crate::storage::services::system_settings::get_mcp_project_scope_settings(
            &database_path,
            &project_id,
        )
        .map(Some)
    })
    .await
}

/// 加载全局 MCP 工具级 scope（无记录时 storage 层返回默认空黑名单）。
pub(crate) async fn load_global_scope() -> Result<Option<McpGlobalScopeSettings>> {
    with_database_path(move |database_path| {
        crate::storage::services::system_settings::get_mcp_global_scope_settings(&database_path)
            .map(Some)
    })
    .await
}

pub(crate) async fn with_database_path<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(PathBuf) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let storage_info = crate::storage::initialize_app_storage()?;
        operation(PathBuf::from(storage_info.database_path))
    })
    .await
    .map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to access project MCP scope storage: {error}"),
        )
    })?
}
