use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;

// ============================================================================
// Security utilities (ported from snow-cli security.utils.ts)
// ============================================================================

const MAX_SCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// Check if a command matches any user-configured sensitive command rules.
/// Uses spawn_blocking to avoid blocking the async runtime with SQLite I/O.
/// Returns a JSON array of matched rules (command_id, pattern, description).
///
/// 同时检测"间接执行脚本"（bash script.sh / source x.sh 等），读取脚本
/// 内容一并匹配，防止先把敏感操作写入脚本再执行来绕过检查。
pub(crate) async fn check_sensitive_commands(
    command: &str,
    working_directory: Option<&str>,
    project_id: Option<&str>,
) -> Vec<Value> {
    let command_owned = command.to_string();
    let working_directory_owned = working_directory.map(str::to_string);
    let project_id_owned = project_id.map(str::to_string);
    match tokio::task::spawn_blocking(move || {
        let mut candidates: Vec<(String, Option<String>)> = vec![(command_owned.clone(), None)];
        for script_path in extract_script_paths(&command_owned) {
            if let Some(content) =
                read_script_content(&script_path, working_directory_owned.as_deref())
            {
                candidates.push((content, Some(script_path)));
            }
        }
        crate::storage::check_sensitive_command_match(candidates, project_id_owned)
    })
    .await
    {
        Ok(Ok(matches)) => matches
            .into_iter()
            .map(|m| {
                json!({
                    "commandId": m.command_id,
                    "pattern": m.pattern,
                    "description": m.description,
                })
            })
            .collect(),
        Ok(Err(_)) | Err(_) => Vec::new(),
    }
}

/// 提取"间接执行脚本"的脚本路径（bash script.sh / source x.sh 等），
/// 排除 `-c` 内联模式（内联内容已在命令行中，正则可直接命中）。
fn extract_script_paths(command: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let shell_re = Regex::new(
        r#"(?i)\b(bash|sh|zsh|dash|ksh|ash|fish|pwsh|powershell)\s+(-[a-zA-Z0-9]+\s+)*([^\s;&|"'`<>]+)"#,
    )
    .expect("valid shell runner regex");
    for captures in shell_re.captures_iter(command) {
        let candidate = captures
            .get(3)
            .map(|m| m.as_str().trim_matches(|c| c == '\'' || c == '"'))
            .unwrap_or("");
        if candidate.is_empty() || candidate.starts_with('-') {
            continue;
        }
        paths.push(candidate.to_string());
    }
    let source_re = Regex::new(r#"(?i)\b(source|\.)\s+([^\s;&|"'`<>]+)"#)
        .expect("valid source regex");
    for captures in source_re.captures_iter(command) {
        let candidate = captures
            .get(2)
            .map(|m| m.as_str().trim_matches(|c| c == '\'' || c == '"'))
            .unwrap_or("");
        if candidate.is_empty() {
            continue;
        }
        paths.push(candidate.to_string());
    }
    paths
}

/// 读取脚本内容用于敏感匹配。仅本地普通文件（拒绝 SSH 路径），支持 `~`
/// 展开与相对 working_directory 解析；不可读/超大/非 UTF-8 时返回 None。
fn read_script_content(script_path: &str, working_directory: Option<&str>) -> Option<String> {
    let expanded = if let Some(rest) = script_path.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").ok()?).join(rest)
    } else {
        PathBuf::from(script_path)
    };
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        PathBuf::from(working_directory?).join(expanded)
    };
    if crate::mcp::servers::remote_workspace::is_ssh_path(
        candidate.to_string_lossy().as_ref(),
    ) {
        return None;
    }
    let metadata = std::fs::metadata(&candidate).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SCRIPT_BYTES {
        return None;
    }
    std::fs::read_to_string(&candidate).ok()
}

/// Self-protection: detect commands that would kill the app's own process.
pub(crate) struct SelfDestructCheck {
    pub(crate) is_self_destructive: bool,
    pub(crate) reason: String,
    pub(crate) suggestion: String,
}

/// Returns a SelfDestructCheck indicating whether the command is self-destructive.
///
/// Since this runs inside the Electron app process, any command that terminates
/// Electron processes by name (e.g. killall, pkill, taskkill) will also kill the app.
pub(crate) fn is_self_destructive_command(command: &str) -> SelfDestructCheck {
    let lower = command.to_lowercase();
    let app_pid = std::process::id();

    // Windows CMD: taskkill targeting electron.exe
    if regex_matches(r"(?i)\btaskkill\b", command)
        && regex_matches(r"(?i)\belectron(\.exe)?\b", command)
    {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "Command would terminate electron.exe processes, including this app itself"
                .to_string(),
            suggestion: format!(
                "This app is running as electron.exe (PID: {}). Use \"taskkill /PID <target_pid>\" for specific processes, excluding PID {}.",
                app_pid, app_pid
            ),
        };
    }

    // Unix: killall electron
    if regex_matches(r"(?i)\bkillall\s+(-\w+\s+)*electron\b", command) {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "killall electron would terminate ALL Electron processes, including this app"
                .to_string(),
            suggestion: format!(
                "Use \"kill <specific_pid>\" to target individual processes, excluding PID {}.",
                app_pid
            ),
        };
    }

    // Unix: pkill electron / pkill -f electron
    if regex_matches(r"(?i)\bpkill\s+(-\w+\s+)*electron\b", command) {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "pkill electron would terminate Electron processes, including this app"
                .to_string(),
            suggestion: format!(
                "Use \"kill <specific_pid>\" to target individual processes, excluding PID {}.",
                app_pid
            ),
        };
    }

    // Also protect against killing node processes
    if regex_matches(r"(?i)\bkillall\s+(-\w+\s+)*node\b", command) {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "killall node would terminate ALL Node.js processes, including this app"
                .to_string(),
            suggestion: format!(
                "Use \"kill <specific_pid>\" to target individual processes, excluding PID {}.",
                app_pid
            ),
        };
    }

    if regex_matches(r"(?i)\bpkill\s+(-\w+\s+)*node\b", command) {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "pkill node would terminate Node.js processes, including this app".to_string(),
            suggestion: format!(
                "Use \"kill <specific_pid>\" to target individual processes, excluding PID {}.",
                app_pid
            ),
        };
    }

    // Windows: Stop-Process targeting node/electron
    if lower.contains("stop-process")
        && (regex_matches(r"(?i)\bnode\b", command) || regex_matches(r"(?i)\belectron\b", command))
    {
        return SelfDestructCheck {
            is_self_destructive: true,
            reason: "Command would terminate Node.js/Electron processes, including this app itself"
                .to_string(),
            suggestion: format!(
                "This app (PID: {}) may be affected. Add a PID exclusion filter.",
                app_pid
            ),
        };
    }

    // Directly targeting the app's own PID
    let pid_str = app_pid.to_string();

    // Check for "kill <pid>" or "kill -9 <pid>" patterns
    let kill_pattern = format!(r"\bkill\s+(-\d+\s+)*{}\b", pid_str);
    let kill9_pattern = format!(r"\bkill\s+-9\s+{}\b", pid_str);
    let stop_process_pattern = format!(r"(?i)\bStop-Process\s+.*-Id\s+{}\b", pid_str);
    let taskkill_pattern = format!(r"(?i)\btaskkill\b.*/PID\s+{}\b", pid_str);

    let pid_patterns = [
        kill_pattern,
        kill9_pattern,
        stop_process_pattern,
        taskkill_pattern,
    ];

    for pattern in &pid_patterns {
        if regex_matches(pattern, command) {
            return SelfDestructCheck {
                is_self_destructive: true,
                reason: format!(
                    "Command directly targets this app process (PID: {})",
                    app_pid
                ),
                suggestion: format!(
                    "PID {} is the Snow App process. Killing it will terminate the current session.",
                    app_pid
                ),
            };
        }
    }

    let _ = lower; // suppress unused warning
    SelfDestructCheck {
        is_self_destructive: false,
        reason: String::new(),
        suggestion: String::new(),
    }
}

/// Helper: compile and test a regex pattern against a string
fn regex_matches(pattern: &str, text: &str) -> bool {
    Regex::new(pattern)
        .map(|r| r.is_match(text))
        .unwrap_or(false)
}
