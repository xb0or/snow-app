use super::*;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use serde_json::Value;
use uuid::Uuid;

use super::super::builtin::{execute_builtin_tool, sanitize_tool_full_name};
use super::collect::ensure_project_tool_enabled;
use super::plan_write::is_allowed_plan_document_write;
use super::super::servers::app_control::{AppControlCallback, AppControlService};
use super::super::servers::bash::{BashService, BashStreamCallback, BashStreamChunk};
use super::super::servers::browser::{BrowserCommandCallback, BrowserService};
use super::super::servers::codebase::CodebaseService;
use super::super::servers::codelens::CodeLensService;
use super::super::servers::config::ConfigService;
use super::super::servers::filesystem::FilesystemService;
use super::super::servers::grep::GrepService;
use super::super::servers::imagegen::ImageGenService;
use super::super::servers::remote_workspace::{is_ssh_path, RemoteWorkspaceCallback};
use super::super::servers::skills::SkillsService;
use super::super::servers::terminal::{TerminalCommandCallback, TerminalService};
use super::super::servers::todo::TodoService;
use super::super::servers::user_interaction::{UserInteractionService, UserQuestionCallback};
use super::super::servers::websearch::WebSearchService;

/// Register a cancellation token for a remote (SSH) tool execution and emit
/// its id as a `tool_execution` stream chunk so the frontend can abort the
/// pending Electron-side command (per-tool stop button / session stop).
/// Returns the id and token; the caller must `unregister_tool_execution` when
/// the execution settles.
fn register_remote_tool_execution(
    on_chunk: &BashStreamCallback,
) -> (String, tokio_util::sync::CancellationToken) {
    let tool_execution_id = Uuid::new_v4().to_string();
    let cancel_token = crate::api::cancel::register_tool_execution(&tool_execution_id);
    on_chunk.call(
        BashStreamChunk {
            stream: "tool_execution".to_string(),
            data: tool_execution_id.clone(),
        },
        ThreadsafeFunctionCallMode::NonBlocking,
    );
    (tool_execution_id, cancel_token)
}

/// Execute an MCP tool and capture incremental checkpoint state immediately
pub async fn call_mcp_tool(
    tool_full_name: String,
    args_json: String,
    project_id: Option<String>,
    checkpoint_ids: Vec<String>,
    checkpoint_work_dir: Option<String>,
    sensitive_authorization_token: Option<String>,
    on_chunk: BashStreamCallback,
    on_browser_command: BrowserCommandCallback,
    on_user_question: UserQuestionCallback,
    on_app_control: AppControlCallback,
    on_remote_workspace_command: RemoteWorkspaceCallback,
    on_terminal_command: TerminalCommandCallback,
    sub_agent_allowed_tools: Option<Vec<String>>,
    plan_mode: bool,
    plan_approved: bool,
) -> napi::Result<String> {
    // Sanitize: AI may copy "[Tool: server-tool#callId]" from conversation
    // history or leak internal XML tags into the tool name. Normalize
    // before any matching or whitelist check.
    let tool_full_name = sanitize_tool_full_name(&tool_full_name);
    let is_sub_agent_call = sub_agent_allowed_tools.is_some();

    if tool_full_name == REQUEST_APPROVAL_FULL_NAME {
        if is_sub_agent_call {
            return Err(Error::new(
                Status::GenericFailure,
                "app-control-requestApproval is reserved for the main conversation; sub-agents cannot request or grant Plan Mode approval"
                    .to_string(),
            ));
        }
        if !plan_mode {
            return Err(Error::new(
                Status::GenericFailure,
                "app-control-requestApproval is only available while Plan Mode is active"
                    .to_string(),
            ));
        }
    }

    let args = parse_tool_args(&tool_full_name, &args_json)?;
    if plan_mode
        && !plan_approved
        && matches!(
            tool_full_name.as_str(),
            "filesystem-replace_edit" | "filesystem-create"
        )
        && (is_sub_agent_call
            || !is_allowed_plan_document_write(project_id.as_deref(), &args).await?)
    {
        let message = if is_sub_agent_call {
            format!(
                "PARENT_PLAN_APPROVAL_REQUIRED: {tool_full_name} cannot run because the main conversation has not approved its Plan Mode plan. Stop this sub-agent task and return control to the main conversation. Do not retry this write and do not request approval from the sub-agent."
            )
        } else {
            format!(
                "Plan Mode write blocked: {tool_full_name} cannot run before explicit user approval. Only plan documents inside .snow/plan or .trellis/tasks may be written during planning. Call app-control-requestApproval first, and retry project-file writes only when that tool returns approved=true."
            )
        };
        return Err(Error::new(Status::GenericFailure, message));
    }

    ensure_project_tool_enabled(project_id.as_deref(), &tool_full_name).await?;

    if let Some(ref allowed_tools) = sub_agent_allowed_tools {
        let wildcard_enabled = allowed_tools.iter().any(|name| name == "*");
        let comms_allowed = SUB_AGENT_COMMS_TOOL_FULL_NAMES.contains(&tool_full_name.as_str());
        if !wildcard_enabled && !comms_allowed && !allowed_tools.iter().any(|name| name == &tool_full_name)
        {
            return Err(Error::new(
                Status::GenericFailure,
                format!("Sub-agent tool is not in the allowed whitelist: {tool_full_name}"),
            ));
        }
    }

    let (args, uses_remote_workspace) =
        prepare_remote_workspace_args(&tool_full_name, args, project_id.as_deref()).await?;

    // 本地（非 SSH）filesystem 工具：将 filePath 的相对路径（如 "."）解析到
    // 当前项目根目录，避免其被解析为 Electron 进程的工作目录。
    let args = if uses_remote_workspace {
        args
    } else {
        resolve_local_filesystem_args(&tool_full_name, args, project_id.as_deref()).await?
    };

    // 只读 bash 命令跳过 checkpoint 工作区快照：git 快照（rev-parse/diff/
    // ls-files + 文件复制）只对可能修改工作区的命令有意义。保守策略——
    // 只有明确命中的只读模式才跳过，任何不确定情况保留 checkpoint。
    let checkpoint_ids = if tool_full_name == "bash-terminal-execute"
        && args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_readonly_bash_command)
    {
        Vec::new()
    } else {
        checkpoint_ids
    };

    let checkpoint_capture = if uses_remote_workspace {
        // SSH 工具：checkpoint 捕获经 Electron SFTP 完成（异步）。
        capture_checkpoint_before_tool_remote(
            &tool_full_name,
            &args,
            checkpoint_ids,
            checkpoint_work_dir,
            &on_remote_workspace_command,
        )
        .await?
    } else {
        let checkpoint_tool_name = tool_full_name.clone();
        let checkpoint_args = args.clone();
        tokio::task::spawn_blocking(move || {
            capture_checkpoint_before_tool(
                &checkpoint_tool_name,
                &checkpoint_args,
                checkpoint_ids,
                checkpoint_work_dir,
            )
        })
        .await
        .map_err(|error| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to capture checkpoint before tool execution: {error}"),
            )
        })??
    };

    let returns_plain_text = tool_full_name == "skills-skill-execute";
    let masking_tool_name = tool_full_name.clone();
    let result = if tool_full_name == "bash-terminal-execute" {
        let terminal_result = BashService::new()
            .execute_terminal_stream(
                &args,
                project_id.as_deref(),
                sensitive_authorization_token.as_deref(),
                on_chunk,
                &on_remote_workspace_command,
            )
            .await;
        if let ToolCheckpointCapture::Worktree(Some(capture)) = checkpoint_capture {
            if uses_remote_workspace {
                capture_checkpoint_after_tool_remote(
                    ToolCheckpointCapture::Worktree(Some(capture)),
                    &on_remote_workspace_command,
                )
                .await?;
            } else {
                tokio::task::spawn_blocking(move || {
                    crate::storage::services::checkpoint::record_checkpoint_worktree_after(capture)
                })
                .await
                .map_err(|error| {
                    Error::new(
                        Status::GenericFailure,
                        format!("Failed to capture checkpoint after tool execution: {error}"),
                    )
                })??;
            }
        }
        terminal_result?
    } else if tool_full_name == "grep-search" {
        // Register a cancellable tool execution only for the SSH branch; the
        // local ripgrep/native search has its own 30s timeout and cannot be
        // aborted through the exec-channel registry.
        let remote_cancel = if args
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(is_ssh_path)
        {
            Some(register_remote_tool_execution(&on_chunk))
        } else {
            None
        };
        let search_result = GrepService::new()
            .execute_search(
                &args,
                &on_remote_workspace_command,
                remote_cancel.as_ref().map(|(_, token)| token),
            )
            .await;
        if let Some((tool_execution_id, _)) = remote_cancel {
            crate::api::cancel::unregister_tool_execution(&tool_execution_id);
        }
        search_result?
    } else if uses_remote_workspace {
        let filesystem_tool = tool_full_name.strip_prefix("filesystem-").ok_or_else(|| {
            Error::new(
                Status::GenericFailure,
                format!("Unsupported remote workspace MCP tool: {tool_full_name}"),
            )
        })?;
        let (tool_execution_id, cancel_token) = register_remote_tool_execution(&on_chunk);
        let fs_result = FilesystemService::new()
            .execute_async(
                filesystem_tool,
                &args,
                &on_remote_workspace_command,
                Some(&cancel_token),
            )
            .await;
        crate::api::cancel::unregister_tool_execution(&tool_execution_id);
        // 无论工具成败都执行 after 捕获（与本地 builtin 分支一致）；
        // after 失败优先于工具错误上报。
        capture_checkpoint_after_tool_remote(checkpoint_capture, &on_remote_workspace_command)
            .await?;
        fs_result?
    } else if tool_full_name == "todo-todo-manage" {
        TodoService::new().execute_async(&args).await?
    } else if tool_full_name == "websearch-websearch-search" {
        WebSearchService::new().execute_search(&args).await?
    } else if tool_full_name == "websearch-websearch-fetch" {
        WebSearchService::new().execute_fetch(&args).await?
    } else if tool_full_name == "imagegen-generate" {
        ImageGenService::new()
            .execute_generate(&args, &on_chunk)
            .await?
    } else if tool_full_name == "imagegen-image-describe" {
        ImageGenService::new().execute_describe(&args).await?
    } else if let Some(tool_name) = tool_full_name.strip_prefix("browser-") {
        BrowserService::new()
            .execute_async(tool_name, &args, &on_browser_command)
            .await?
    } else if let Some(tool_name) = tool_full_name.strip_prefix("terminal-") {
        TerminalService::new()
            .execute_async(tool_name, &args, &on_terminal_command)
            .await?
    } else if tool_full_name == "user-interaction-askUserQuestion" {
        UserInteractionService::new()
            .execute_async(&args, &on_user_question)
            .await?
    } else if let Some(app_control_tool) = tool_full_name.strip_prefix("app-control-") {
        AppControlService::new()
            .execute_async(app_control_tool, &args, &on_app_control, &on_user_question)
            .await?
    } else if let Some(config_tool) = tool_full_name.strip_prefix("config-") {
        // 传入运行时已知的当前会话 projectId（directoryId）。ConfigService
        // 内部会将其注入到支持项目级作用域的调用（hooks/subAgents/skills/
        // settings 项目键），并让 config-list 返回 currentProjectId，
        // 修复 AI 无法获知当前会话项目ID导致项目级配置落到全局的问题。
        ConfigService::new()
            .execute_async(config_tool, &args, project_id.clone())
            .await?
    } else if tool_full_name == "skills-skill-execute" {
        SkillsService::new()
            .execute(&args, project_id.as_deref())
            .await?
    } else if tool_full_name == "codebase-search" {
        CodebaseService::new()
            .execute_search(&args, project_id.as_deref(), &on_chunk)
            .await?
    } else if let Some(codelens_tool) = tool_full_name.strip_prefix("codelens-") {
        let service = CodeLensService::new();
        match codelens_tool {
            "find_definition" => {
                service
                    .execute_find_definition(&args, project_id.as_deref())
                    .await?
            }
            "find_references" => {
                service
                    .execute_find_references(&args, project_id.as_deref())
                    .await?
            }
            "file_outline" => service.execute_file_outline(&args).await?,
            _ => {
                return Err(Error::new(
                    Status::GenericFailure,
                    format!(
                        "Unknown codelens tool: \"{codelens_tool}\". Available tools: [find_definition, find_references, file_outline]"
                    ),
                ));
            }
        }
    } else if let Some(result) =
        super::super::external::call_tool(project_id.as_deref(), &tool_full_name, &args).await?
    {
        result
    } else {
        tokio::task::spawn_blocking(move || {
            let result = execute_builtin_tool(&tool_full_name, &args);
            capture_checkpoint_after_tool(checkpoint_capture)?;
            result
        })
        .await
        .map_err(|error| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to execute MCP tool: {error}"),
            )
        })??
    };

    if returns_plain_text {
        let plain_text = result.as_str().map(str::to_string).ok_or_else(|| {
            Error::new(
                Status::GenericFailure,
                "Skill execution returned an invalid text result".to_string(),
            )
        })?;
        let masked =
            super::super::privacy_mask::mask_tool_result_if_needed(&masking_tool_name, &plain_text)
                .await?;
        return Ok(masked);
    }

    let serialized = serde_json::to_string(&result).map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to serialize result: {error}"),
        )
    })?;
    super::super::privacy_mask::mask_tool_result_if_needed(&masking_tool_name, &serialized).await
}
