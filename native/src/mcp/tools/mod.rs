use std::path::Path;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::Value;

use crate::storage::services::checkpoint::remote::RemoteCheckpointClient;
use crate::storage::services::checkpoint::CheckpointWorktreeCapture;
use crate::storage::services::system_settings::{
    McpGlobalScopeSettings, McpProjectScopeSettings,
};

enum ToolCheckpointCapture {
    None,
    File {
        checkpoint_ids: Vec<String>,
        work_dir: String,
        file_path: String,
    },
    Worktree(Option<CheckpointWorktreeCapture>),
}

use super::builtin::{get_builtin_servers_with_tools, get_builtin_tools};
use super::servers::remote_workspace::{
    is_ssh_path, is_windows_absolute_path, RemoteWorkspaceCallback, resolve_remote_project_workspace,
    resolve_remote_workspace_path,
};

mod call;
mod collect;
mod plan_write;
mod serialize;

pub use call::call_mcp_tool;
pub use collect::{collect_all_mcp_tools, collect_allowed_mcp_tools};
pub use serialize::{
    tools_as_anthropic_json, tools_as_gemini_json, tools_as_openai_chat_json,
    tools_as_openai_responses_json,
};
pub use super::servers::sub_agents::SUB_AGENT_COMMS_TOOL_FULL_NAMES;
pub(crate) use collect::{
    builtin_scope_server_id, builtin_server_name, load_global_scope, load_project_scope,
    server_id_from_tool_name, with_database_path,
};

// NOTE: list_mcp_tools 和 call_mcp_tool 的 #[napi] 导出在 exports/api.rs 中，
// 此处仅保留内部函数供 exports 层调用。

#[napi(object)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
}

#[napi(object)]
pub struct McpProjectToolStatus {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
    pub enabled: bool,
}

#[napi(object)]
pub struct McpToolStatus {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
    pub enabled: bool,
}

#[napi(object)]
pub struct McpProjectServerStatus {
    pub id: String,
    pub name: String,
    pub source: String,
    pub global_enabled: bool,
    pub enabled: bool,
    pub tools: Vec<McpProjectToolStatus>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct McpTool {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpTool {
    pub fn full_name(&self) -> String {
        format!("{}-{}", self.server_id, self.name)
    }
}

/// requestApproval 工具全名（隶属于 app-control 服务器，仅 Plan Mode 下暴露）。
const REQUEST_APPROVAL_FULL_NAME: &str = "app-control-requestApproval";

/// 所有内置 MCP 服务器 ID（含动态注册的 skills），按长度降序排列，
/// 用于工具名最长前缀匹配。新格式 `{server_id}-{tool_name}` 中，server_id
/// 可能含 `-`（如 `user-interaction`），需通过此列表消除歧义；外部工具的
/// server_name 经 `sanitize_name` 后不含 `-`，可安全用第一个 `-` 分割。
pub const BUILTIN_SERVER_IDS: &[&str] = &[
    "user-interaction",
    "app-control",
    "filesystem",
    "sub-agents",
    "websearch",
    "imagegen",
    "codebase",
    "codelens",
    "browser",
    "config",
    "skills",
    "bash",
    "todo",
    "grep",
    "terminal",
];

/// 将工具全名 `{server_id}-{tool_name}` 拆分为 `(server_id, tool_name)`。
/// 先匹配已知内置 server_id 前缀（最长优先），再回退到首个 `-` 分割
/// （适用于外部工具，其 server_name 不含 `-`）。
pub fn split_tool_full_name(full_name: &str) -> Option<(&str, &str)> {
    for &server_id in BUILTIN_SERVER_IDS {
        if let Some(rest) = full_name.strip_prefix(server_id) {
            if let Some(tool_name) = rest.strip_prefix('-') {
                if !tool_name.is_empty() {
                    return Some((server_id, tool_name));
                }
            }
        }
    }
    let (server_id, tool_name) = full_name.split_once('-')?;
    if server_id.is_empty() || tool_name.is_empty() {
        return None;
    }
    Some((server_id, tool_name))
}

pub async fn list_mcp_tools() -> napi::Result<Vec<McpToolDefinition>> {
    let tools = collect_all_mcp_tools(None, false).await?;
    Ok(to_tool_definitions(&tools))
}

pub async fn list_mcp_server_tools(
    config_server_id: String,
) -> napi::Result<Vec<McpToolStatus>> {
    let tools = super::external::discover_server_tools(None, &config_server_id).await?;
    let global_scope = load_global_scope().await?;
    Ok(to_tool_statuses(&tools, global_scope.as_ref()))
}

pub async fn list_mcp_project_servers(
    project_id: String,
) -> napi::Result<Vec<McpProjectServerStatus>> {
    let project_id = required_value(project_id, "Project id")?;
    let scope = load_project_scope(Some(&project_id))
        .await?
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Project id is required to list project MCP servers".to_string(),
            )
        })?;

    // Image generation tool is only globally available when at least one
    // channel (OpenAI / Gemini) is configured and enabled in Settings ->
    // Image generation. When both are unconfigured the server is globally
    // disabled so the front-end toggle reflects the real state (instead of
    // appearing enabled while the tool is silently excluded from context).
    let imagegen_configured =
        tokio::task::spawn_blocking(|| crate::mcp::servers::imagegen::is_imagegen_configured())
            .await
            .map_err(|error| {
                Error::new(
                    Status::GenericFailure,
                    format!("Failed to check image generation configuration: {error}"),
                )
            })??;

    let mut servers = get_builtin_servers_with_tools()
        .into_iter()
        .map(|(server_id, tools)| {
            let scope_server_id = builtin_scope_server_id(&server_id);
            let enabled = scope.is_server_enabled(&scope_server_id);
            // Reflect imagegen configuration state in global_enabled / error
            // so the front-end toggle stays in sync with collect_all_mcp_tools.
            // The error field uses a stable code (not a localized string) that
            // the front-end maps to the user's language.
            let (global_enabled, error) = if server_id == "imagegen" && !imagegen_configured {
                (false, Some("imagegen:not_configured".to_string()))
            } else {
                (true, None)
            };
            McpProjectServerStatus {
                id: scope_server_id,
                name: builtin_server_name(&server_id).to_string(),
                source: "system".to_string(),
                global_enabled,
                enabled,
                tools: to_project_tool_statuses(&tools, &scope),
                error,
            }
        })
        .collect::<Vec<_>>();

    for external_server in super::external::discover_project_servers(&project_id).await? {
        let scope_server_id =
            super::external::project_scope_server_id(&external_server.config_server_id);
        let project_owned = external_server.source == "project";
        let enabled =
            external_server.enabled && (project_owned || scope.is_server_enabled(&scope_server_id));
        servers.push(McpProjectServerStatus {
            id: scope_server_id,
            name: external_server.name,
            source: external_server.source,
            global_enabled: external_server.global_enabled,
            enabled,
            tools: Vec::new(),
            error: None,
        });
    }

    Ok(servers)
}

pub async fn list_mcp_project_server_tools(
    project_id: String,
    server_id: String,
) -> napi::Result<Vec<McpProjectToolStatus>> {
    let project_id = required_value(project_id, "Project id")?;
    let server_id = required_value(server_id, "MCP server id")?;
    let scope = load_project_scope(Some(&project_id))
        .await?
        .ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "Project id is required to list project MCP server tools".to_string(),
            )
        })?;

    if let Some(builtin_server_id) = server_id.strip_prefix("builtin:") {
        let tools = get_builtin_servers_with_tools()
            .into_iter()
            .find(|(known_server_id, _)| known_server_id == builtin_server_id)
            .map(|(_, tools)| tools)
            .ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    format!("Unknown MCP project server: {server_id}"),
                )
            })?;
        return Ok(to_project_tool_statuses(&tools, &scope));
    }

    let external_server_id = server_id.strip_prefix("external:").ok_or_else(|| {
        Error::new(
            Status::InvalidArg,
            format!("Unknown MCP project server: {server_id}"),
        )
    })?;
    let tools =
        super::external::discover_server_tools(Some(&project_id), external_server_id).await?;
    Ok(to_project_tool_statuses(&tools, &scope))
}

pub async fn set_mcp_project_server_enabled(
    project_id: String,
    server_id: String,
    enabled: bool,
) -> napi::Result<()> {
    let project_id = required_value(project_id, "Project id")?;
    let server_id = required_value(server_id, "MCP server id")?;
    let known_server = if let Some(builtin_server_id) = server_id.strip_prefix("builtin:") {
        get_builtin_servers_with_tools()
            .iter()
            .any(|(known_server_id, _)| known_server_id == builtin_server_id)
    } else if let Some(external_server_id) = server_id.strip_prefix("external:") {
        super::external::discover_project_servers(&project_id)
            .await?
            .iter()
            .any(|server| server.config_server_id == external_server_id)
    } else {
        false
    };
    if !known_server {
        return Err(Error::new(
            Status::InvalidArg,
            format!("Unknown MCP project server: {server_id}"),
        ));
    }

    if let Some(external_server_id) = server_id.strip_prefix("external:") {
        let project_servers = super::external::discover_project_servers(&project_id).await?;
        if project_servers.iter().any(|server| {
            server.config_server_id == external_server_id && server.source == "project"
        }) {
            let external_server_id = external_server_id.to_string();
            return with_database_path(move |database_path| {
                crate::storage::services::project_mcp_server_configs::set_project_mcp_server_enabled(
                    &database_path,
                    &project_id,
                    &external_server_id,
                    enabled,
                )
            })
            .await;
        }
    }

    with_database_path(move |database_path| {
        crate::storage::services::system_settings::set_mcp_project_server_enabled(
            &database_path,
            &project_id,
            &server_id,
            enabled,
        )
    })
    .await
}

pub async fn set_mcp_project_tool_enabled(
    project_id: String,
    tool_name: String,
    enabled: bool,
) -> napi::Result<()> {
    let project_id = required_value(project_id, "Project id")?;
    let tool_name = required_value(tool_name, "MCP tool name")?;
    let tool_exists = if let Some(server_id) = server_id_from_tool_name(&tool_name) {
        if get_builtin_servers_with_tools()
            .iter()
            .any(|(builtin_server_id, _)| builtin_server_id == server_id)
        {
            get_builtin_tools()
                .iter()
                .any(|tool| tool.full_name() == tool_name)
        } else {
            super::external::resolve_project_scope_server(Some(&project_id), &tool_name)
                .await?
                .is_some()
        }
    } else {
        false
    };
    if !tool_exists {
        return Err(Error::new(
            Status::InvalidArg,
            format!("Unknown MCP project tool: {tool_name}"),
        ));
    }

    with_database_path(move |database_path| {
        crate::storage::services::system_settings::set_mcp_project_tool_enabled(
            &database_path,
            &project_id,
            &tool_name,
            enabled,
        )
    })
    .await
}

/// 全局启停单个工具：校验工具存在于全局可见的工具集（内置或全局外部服务器）。
pub async fn set_mcp_tool_enabled(tool_name: String, enabled: bool) -> napi::Result<()> {
    let tool_name = required_value(tool_name, "MCP tool name")?;
    ensure_global_tool_exists(&tool_name).await?;

    with_database_path(move |database_path| {
        crate::storage::services::system_settings::set_mcp_global_tool_enabled(
            &database_path,
            &tool_name,
            enabled,
        )
    })
    .await
}

/// 全局批量启停工具：逐个校验存在性，全部通过后一次写入存储。
pub async fn set_mcp_tools_enabled(tool_names: Vec<String>, enabled: bool) -> napi::Result<()> {
    for tool_name in &tool_names {
        let tool_name = required_value(tool_name.clone(), "MCP tool name")?;
        ensure_global_tool_exists(&tool_name).await?;
    }

    with_database_path(move |database_path| {
        crate::storage::services::system_settings::set_mcp_global_tools_enabled(
            &database_path,
            &tool_names,
            enabled,
        )
    })
    .await
}

/// 项目批量启停工具：逐个校验存在性（builtin/external 分支），全部通过后一次写入存储。
pub async fn set_mcp_project_tools_enabled(
    project_id: String,
    tool_names: Vec<String>,
    enabled: bool,
) -> napi::Result<()> {
    let project_id = required_value(project_id, "Project id")?;
    for tool_name in &tool_names {
        let tool_name = required_value(tool_name.clone(), "MCP tool name")?;
        let tool_exists = if let Some(server_id) = server_id_from_tool_name(&tool_name) {
            if get_builtin_servers_with_tools()
                .iter()
                .any(|(builtin_server_id, _)| builtin_server_id == server_id)
            {
                get_builtin_tools()
                    .iter()
                    .any(|tool| tool.full_name() == tool_name)
            } else {
                super::external::resolve_project_scope_server(Some(&project_id), &tool_name)
                    .await?
                    .is_some()
            }
        } else {
            false
        };
        if !tool_exists {
            return Err(Error::new(
                Status::InvalidArg,
                format!("Unknown MCP project tool: {tool_name}"),
            ));
        }
    }

    with_database_path(move |database_path| {
        crate::storage::services::system_settings::set_mcp_project_tools_enabled(
            &database_path,
            &project_id,
            &tool_names,
            enabled,
        )
    })
    .await
}

/// 校验工具存在于全局可见的工具集（内置工具或已配置的全局外部服务器）中。
async fn ensure_global_tool_exists(tool_name: &str) -> Result<()> {
    if let Some(server_id) = server_id_from_tool_name(tool_name) {
        if get_builtin_servers_with_tools()
            .iter()
            .any(|(builtin_server_id, _)| builtin_server_id == server_id)
        {
            if get_builtin_tools()
                .iter()
                .any(|tool| tool.full_name() == tool_name)
            {
                return Ok(());
            }
        } else if super::external::resolve_project_scope_server(None, tool_name)
            .await?
            .is_some()
        {
            return Ok(());
        }
    }
    Err(Error::new(
        Status::InvalidArg,
        format!("Unknown MCP tool: {tool_name}"),
    ))
}

fn to_tool_definitions(tools: &[McpTool]) -> Vec<McpToolDefinition> {
    tools
        .iter()
        .map(|tool| McpToolDefinition {
            name: tool.full_name(),
            description: tool.description.clone(),
            input_schema_json: serialize_input_schema(tool),
        })
        .collect()
}

fn to_project_tool_statuses(
    tools: &[McpTool],
    scope: &McpProjectScopeSettings,
) -> Vec<McpProjectToolStatus> {
    tools
        .iter()
        .map(|tool| {
            let full_name = tool.full_name();
            McpProjectToolStatus {
                enabled: scope.is_tool_enabled(&full_name),
                name: full_name,
                description: tool.description.clone(),
                input_schema_json: serialize_input_schema(tool),
            }
        })
        .collect()
}

/// 工具状态转换：enabled 反映全局 scope 黑名单（默认全部启用）。
fn to_tool_statuses(
    tools: &[McpTool],
    global_scope: Option<&McpGlobalScopeSettings>,
) -> Vec<McpToolStatus> {
    tools
        .iter()
        .map(|tool| {
            let full_name = tool.full_name();
            McpToolStatus {
                enabled: !global_scope
                    .is_some_and(|scope| scope.disabled_tool_names.contains(&full_name)),
                name: full_name,
                description: tool.description.clone(),
                input_schema_json: serialize_input_schema(tool),
            }
        })
        .collect()
}

fn serialize_input_schema(tool: &McpTool) -> String {
    serde_json::to_string(&tool.input_schema).unwrap_or_else(|_| "{}".to_string())
}

fn required_value(value: String, label: &str) -> Result<String> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{label} is required"),
        ));
    }

    Ok(normalized.to_string())
}

async fn prepare_remote_workspace_args(
    tool_full_name: &str,
    mut args: Value,
    project_id: Option<&str>,
) -> napi::Result<(Value, bool)> {
    let Some(path_field) = remote_workspace_path_field(tool_full_name) else {
        return Ok((args, false));
    };
    let Some(path) = args.get(path_field).and_then(Value::as_str) else {
        return Ok((args, false));
    };
    // Windows 盘符与 UNC 路径属于 App Host（本机）路径，不能拼入 SSH
    // 工作区；直接走本机通道，由 Electron 在本机读取。
    if is_windows_absolute_path(path) {
        return Ok((args, false));
    }
    let remote_project_workspace = resolve_remote_project_workspace(project_id).await?;
    if is_ssh_path(path) {
        // ssh:// 路径必须属于当前项目 SSH 工作区，否则是跨区域操作，
        // 直接拦截，避免缺失 workspaceRoot 时 Electron 抛底层异常。
        let Some(workspace_path) = remote_project_workspace.as_deref() else {
            return Err(Error::new(
                Status::GenericFailure,
                format!(
                    "[BLOCKED] 跨区域操作被拒绝：{path_field}（{path}）是 SSH 工作区路径，但当前项目不是 SSH 工作区。工具只能访问当前项目工作区内的路径。"
                ),
            ));
        };
        if let (
            Some((workspace_authority, workspace_segments)),
            Some((candidate_authority, candidate_segments)),
        ) = (
            plan_write::normalize_ssh_path(workspace_path),
            plan_write::normalize_ssh_path(path),
        ) {
            if workspace_authority == candidate_authority
                && plan_write::remote_segments_start_with(&candidate_segments, &workspace_segments)
            {
                args["workspaceRoot"] = Value::String(workspace_path.to_string());
                return Ok((args, true));
            }
        }
        return Err(Error::new(
            Status::GenericFailure,
            format!(
                "[BLOCKED] 跨区域操作被拒绝：{path} 不属于当前项目的 SSH 工作区（{workspace_path}）。工具只能访问当前项目工作区内的路径。"
            ),
        ));
    }

    let Some(workspace_path) = remote_project_workspace else {
        return Ok((args, false));
    };
    args[path_field] = Value::String(resolve_remote_workspace_path(&workspace_path, path));
    args["workspaceRoot"] = Value::String(workspace_path);
    Ok((args, true))
}

fn remote_workspace_path_field(tool_full_name: &str) -> Option<&'static str> {
    match tool_full_name {
        "filesystem-read" | "filesystem-replace_edit" | "filesystem-create" => Some("filePath"),
        "grep-search" => Some("path"),
        "bash-terminal-execute" => {
            Some("workingDirectory")
        }
        _ => None,
    }
}

/// 解析 project_id 对应的本地（非 SSH）工作区根目录。
/// 通过应用数据库中的 workspace_directories 表查询该项目的本地根路径。
/// 数据库访问放在 Tokio 阻塞池中执行，避免阻塞 N-API 异步运行时。
/// SSH 工作区不在此处理，由 prepare_remote_workspace_args 统一路由到远端。
async fn resolve_local_project_root(project_id: Option<&str>) -> napi::Result<Option<String>> {
    let Some(project_id) = project_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let project_id = project_id.to_string();

    let workspace_path = tokio::task::spawn_blocking(move || {
        let storage_info = crate::storage::initialize_app_storage()?;
        let database_path = std::path::PathBuf::from(storage_info.database_path);
        crate::storage::services::workspace_directories::get_workspace_directory_path(
            &database_path,
            &project_id,
        )
    })
    .await
    .map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to resolve local project workspace: {error}"),
        )
    })??;

    Ok(workspace_path.filter(|path| !is_ssh_path(path)))
}

/// 将本地 filesystem 工具的 filePath 相对路径解析到当前项目根目录。
/// 当 AI 以 "."、"./src"、"src/main.ts" 等相对路径调用 filesystem 工具时，
/// 避免它们被 Rust 解析为 Electron 进程的工作目录（通常并非项目根目录）。
/// 绝对路径、空路径、SSH 路径或无法解析出项目根目录时保持原样。
async fn resolve_local_filesystem_args(
    tool_full_name: &str,
    mut args: Value,
    project_id: Option<&str>,
) -> napi::Result<Value> {
    if !tool_full_name.starts_with("filesystem-") {
        return Ok(args);
    }
    let Some(file_path) = args.get("filePath").and_then(Value::as_str) else {
        return Ok(args);
    };
    let trimmed = file_path.trim();
    if trimmed.is_empty() || is_ssh_path(trimmed) {
        return Ok(args);
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Ok(args);
    }
    let Some(project_root) = resolve_local_project_root(project_id).await? else {
        return Ok(args);
    };

    let resolved = Path::new(&project_root)
        .join(path)
        .to_string_lossy()
        .to_string();
    args["filePath"] = Value::String(resolved);
    Ok(args)
}

fn parse_tool_args(tool_full_name: &str, args_json: &str) -> napi::Result<Value> {
    serde_json::from_str(args_json).map_err(|error| {
        let received = args_json.chars().take(200).collect::<String>();
        let suffix = if args_json.chars().count() > 200 {
            "..."
        } else {
            ""
        };

        Error::new(
            Status::InvalidArg,
            format!(
                "Failed to parse arguments JSON for tool \"{tool_full_name}\": {error}. Received: {received}{suffix}"
            ),
        )
    })
}

fn capture_checkpoint_before_tool(
    tool_full_name: &str,
    args: &Value,
    checkpoint_ids: Vec<String>,
    checkpoint_work_dir: Option<String>,
) -> napi::Result<ToolCheckpointCapture> {
    if checkpoint_ids.is_empty() {
        return Ok(ToolCheckpointCapture::None);
    }
    let work_dir = checkpoint_work_dir.ok_or_else(|| {
        Error::new(
            Status::InvalidArg,
            "Checkpoint working directory is required".to_string(),
        )
    })?;

    match tool_full_name {
        "filesystem-replace_edit" | "filesystem-create" => {
            let file_path = args
                .get("filePath")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::new(
                        Status::InvalidArg,
                        "filePath is required for checkpoint capture".to_string(),
                    )
                })?
                .to_string();
            crate::storage::services::checkpoint::record_checkpoint_file(
                checkpoint_ids.clone(),
                work_dir.clone(),
                file_path.clone(),
            )?;
            Ok(ToolCheckpointCapture::File {
                checkpoint_ids,
                work_dir,
                file_path,
            })
        }
        "bash-terminal-execute" => Ok(ToolCheckpointCapture::Worktree(
            crate::storage::services::checkpoint::capture_checkpoint_worktree_before(
                checkpoint_ids,
                work_dir,
            )?,
        )),
        _ => Ok(ToolCheckpointCapture::None),
    }
}

/// 远程（SSH）工具的 checkpoint before 捕获：文件 IO 经 Electron SFTP 完成，
/// 与本地版本行为一致（filesystem 工具记录单文件，bash 记录整个工作区）。
async fn capture_checkpoint_before_tool_remote(
    tool_full_name: &str,
    args: &Value,
    checkpoint_ids: Vec<String>,
    checkpoint_work_dir: Option<String>,
    on_remote_workspace_command: &RemoteWorkspaceCallback,
) -> napi::Result<ToolCheckpointCapture> {
    if checkpoint_ids.is_empty() {
        return Ok(ToolCheckpointCapture::None);
    }
    let work_dir = checkpoint_work_dir.ok_or_else(|| {
        Error::new(
            Status::InvalidArg,
            "Checkpoint working directory is required".to_string(),
        )
    })?;
    let client = RemoteCheckpointClient::new(on_remote_workspace_command);
    match tool_full_name {
        "filesystem-replace_edit" | "filesystem-create" => {
            let file_path = args
                .get("filePath")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::new(
                        Status::InvalidArg,
                        "filePath is required for checkpoint capture".to_string(),
                    )
                })?
                .to_string();
            crate::storage::services::checkpoint::remote::record_checkpoint_file_remote(
                &client,
                checkpoint_ids.clone(),
                work_dir.clone(),
                file_path.clone(),
            )
            .await?;
            Ok(ToolCheckpointCapture::File {
                checkpoint_ids,
                work_dir,
                file_path,
            })
        }
        "bash-terminal-execute" => Ok(ToolCheckpointCapture::Worktree(
            crate::storage::services::checkpoint::remote::capture_checkpoint_worktree_before_remote(
                &client,
                checkpoint_ids,
                work_dir,
            )
            .await?,
        )),
        _ => Ok(ToolCheckpointCapture::None),
    }
}

/// 远程（SSH）工具的 checkpoint after 捕获。
async fn capture_checkpoint_after_tool_remote(
    capture: ToolCheckpointCapture,
    on_remote_workspace_command: &RemoteWorkspaceCallback,
) -> napi::Result<()> {
    let client = RemoteCheckpointClient::new(on_remote_workspace_command);
    match capture {
        ToolCheckpointCapture::File {
            checkpoint_ids,
            work_dir,
            file_path,
        } => {
            crate::storage::services::checkpoint::remote::record_checkpoint_file_after_remote(
                &client,
                checkpoint_ids,
                work_dir,
                file_path,
            )
            .await
        }
        ToolCheckpointCapture::Worktree(Some(capture)) => {
            crate::storage::services::checkpoint::remote::record_checkpoint_worktree_after_remote(
                &client, capture,
            )
            .await
        }
        ToolCheckpointCapture::None | ToolCheckpointCapture::Worktree(None) => Ok(()),
    }
}

fn capture_checkpoint_after_tool(capture: ToolCheckpointCapture) -> napi::Result<()> {
    match capture {
        ToolCheckpointCapture::File {
            checkpoint_ids,
            work_dir,
            file_path,
        } => crate::storage::services::checkpoint::record_checkpoint_file_after(
            checkpoint_ids,
            work_dir,
            file_path,
        ),
        ToolCheckpointCapture::Worktree(Some(capture)) => {
            crate::storage::services::checkpoint::record_checkpoint_worktree_after(capture)
        }
        ToolCheckpointCapture::None | ToolCheckpointCapture::Worktree(None) => Ok(()),
    }
}

/// 判断 bash 命令是否只读（不会修改工作区文件）。
/// 返回 `true` 表示可安全跳过 checkpoint 工作区快照。
///
/// 保守策略：只有明确命中的只读模式才返回 `true`；包含文件重定向、
/// 命令链/控制流/命令替换、写操作命令或任何不确定情况一律返回
/// `false`（保留快照，保证回滚安全）。
fn is_readonly_bash_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return true;
    }

    // 重定向到文件（> file / >> file）会修改工作区；排除 >/dev/null 与 >& 形式
    if has_file_redirect(trimmed) {
        return false;
    }

    // 命令链 / 控制流 / 命令替换 / 子 shell / 输入重定向无法静态判定读写性：
    // `cd x && rm -rf y`、`echo hi | tee f`、`echo "$(rm -rf x)"`、
    // `for ...; do ...; done`、`{ rm x; }` 等一律保留 checkpoint。
    if trimmed.contains([';', '&', '|', '`', '<', '(', '{', '\n']) {
        return false;
    }

    // curl -o/-O/--output、wget --output-file 等会把响应写入文件：
    // 命中白名单后对剩余参数做写标志兜底（误伤仅损失优化，方向安全）。
    if trimmed.contains(" -o") || trimmed.contains(" -O") || trimmed.contains("--output") {
        return false;
    }

    // 只读命令模式白名单。注意：sed/awk 未列入（sed -i / awk 重定向会写文件），
    // node/python/curl/wget/git 写类子命令未列入；time/for/while/if/source 等
    // 控制流/包裹关键字未列入（可包裹任意命令，无法静态判定），均保守保留
    // checkpoint。
    const READONLY_PATTERNS: &[&str] = &[
        // 纯读命令（可带参数）
        "echo", "ls", "pwd", "grep", "rg", "cat", "head", "tail", "wc",
        "sort", "uniq", "find", "which", "type", "date", "printf",
        "dirname", "basename", "readlink", "realpath", "stat", "file",
        "tree", "du", "df", "nproc", "uname", "hostname", "ps", "top",
        "ping", "nslookup", "dig", "history", "jobs", "true", "false",
        "sleep", "test", "[", "exit", "cd", "export", "unset",
        // git 只读子命令（写类子命令 add/commit/push/pull/checkout 等不在列）
        "git status", "git log", "git diff", "git branch", "git rev-parse",
        "git remote", "git show", "git ls-files", "git tag", "git blame",
        "git reflog", "git describe", "git shortlog", "git config --get",
        "git help",
        // 只读网络探测
        "curl -I", "curl -i", "curl -sI", "wget --spider",
    ];

    READONLY_PATTERNS.iter().any(|pattern| {
        let p = pattern.trim_end();
        trimmed == p
            || (trimmed.len() > p.len()
                && trimmed.starts_with(p)
                && trimmed.as_bytes()[p.len()].is_ascii_whitespace())
    })
}

/// 检测命令字符串中的文件重定向（`>` / `>>`），排除 `>/dev/null` 与 `>&` / `2>&1`。
fn has_file_redirect(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'>' {
                j += 1;
            }
            let after = command[j..].trim_start();
            if !after.starts_with("/dev/null") && !after.starts_with('&') {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}
