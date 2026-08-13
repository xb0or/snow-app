use super::*;

use std::path::PathBuf;

use serde_json::{json, Value};

/// 校验并返回应用数据库路径；native 存储未初始化时给出明确错误。
/// logs scope（只读日志域）：列出/读取/清理 ~/.snow/log 下的应用日志，
/// 供 agent 自主进行异常分析。set 只读；delete 需精确文件名（防路径穿越）。
pub(crate) fn execute_logs_scope(tool_name: &str, args: &Value) -> napi::Result<Value> {
    match tool_name {
            TOOL_LIST => list_log_files(),
            TOOL_GET => read_log_file(args),
            TOOL_SET => Err(Error::new(
                Status::InvalidArg,
                "logs scope is read-only: use config-list / config-get to inspect logs; config-delete removes one log file".to_string(),
            )),
            TOOL_DELETE => delete_log_file(args),
            _ => Err(Error::new(
                Status::GenericFailure,
                format!(
                    "Unknown tool: \"{tool_name}\" for MCP server \"{SERVER_ID}\". Available tools: [config-list, config-get, config-set, config-delete]"
                ),
            )),
        }
}

/// 日志文件名校验（YYYY-MM-DD-level.log，防路径穿越）。
fn valid_log_name(name: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(LOG_FILE_RE).expect("LOG_FILE_RE is a valid regex"));
    re.is_match(name)
}

/// 日志目录（~/.snow/log）。
fn log_dir() -> PathBuf {
    ConfigService::snow_dir().join(LOG_DIR_NAME)
}

/// config-list logs：列出日志文件（按日期倒序）+ 错误摘要。
fn list_log_files() -> napi::Result<Value> {
    let dir = log_dir();
    if !dir.exists() {
        return Ok(json!({
            "scope": SCOPE_LOGS,
            "directory": dir.to_string_lossy(),
            "files": [],
            "summary": { "totalFiles": 0, "totalBytes": 0, "latestErrorFile": null },
        }));
    }
    let mut files: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut latest_error: Option<String> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !valid_log_name(name) {
                continue;
            }
            let metadata = entry.metadata().ok();
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            total_bytes += size;
            let level = name
                .strip_suffix(".log")
                .and_then(|stem| stem.rsplit('-').next())
                .unwrap_or("")
                .to_string();
            if level == "error" {
                if latest_error.is_none() || name > latest_error.as_deref().unwrap_or("") {
                    latest_error = Some(name.to_string());
                }
            }
            let last_modified = metadata
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            files.push(json!({
                "file": name,
                "date": name.get(..10),
                "level": level,
                "size": size,
                "lastModified": last_modified,
            }));
        }
    }
    // 按日期倒序（文件名前缀即日期）。
    files.sort_by(|a, b| {
        b.get("file")
            .and_then(Value::as_str)
            .cmp(&a.get("file").and_then(Value::as_str))
    });
    Ok(json!({
        "scope": SCOPE_LOGS,
        "directory": dir.to_string_lossy(),
        "files": files,
        "summary": {
            "totalFiles": files.len(),
            "totalBytes": total_bytes,
            "latestErrorFile": latest_error,
        },
    }))
}

/// config-get logs：读取指定日志文件的尾部内容。
/// key 支持精确文件名（`2026-08-03-error.log`）或级别简写（error/warn/info/debug，
/// 读取今天的对应文件）。可选 `limit` 控制返回行数（默认 200，最大 2000）。
fn read_log_file(args: &Value) -> napi::Result<Value> {
    let key = required_string(args, "key")?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| (v as usize).clamp(1, LOG_MAX_LINES))
        .unwrap_or(LOG_DEFAULT_LINES);

    let file_name = if valid_log_name(key) {
        key.to_string()
    } else if ["debug", "info", "warn", "error"].contains(&key) {
        format!("{}-{}.log", chrono::Local::now().format("%Y-%m-%d"), key)
    } else {
        return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "Invalid log key: \"{key}\". Use a log file name (e.g. 2026-08-03-error.log) or a level shortcut (debug/info/warn/error for today's file)"
                ),
            ));
    };

    let path = log_dir().join(&file_name);
    if !path.exists() {
        return Ok(json!({
            "scope": SCOPE_LOGS,
            "key": key,
            "file": file_name,
            "exists": false,
            "content": "",
            "totalLines": 0,
            "truncated": false,
        }));
    }
    let file = std::fs::File::open(&path).map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!(
                "Failed to open log file {}: {error}",
                path.to_string_lossy()
            ),
        )
    })?;
    // 环形缓冲保留最后 limit 行，避免大文件全量加载。
    use std::io::BufRead;
    let reader = std::io::BufReader::new(file);
    let mut tail: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(limit);
    let mut total_lines: usize = 0;
    for line in reader.lines().map_while(|l| l.ok()) {
        total_lines += 1;
        if tail.len() == limit {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let truncated = total_lines > limit;
    Ok(json!({
        "scope": SCOPE_LOGS,
        "key": key,
        "file": file_name,
        "exists": true,
        "content": tail.make_contiguous().join("\n"),
        "totalLines": total_lines,
        "returnedLines": tail.len(),
        "truncated": truncated,
        "hint": truncated.then(|| format!("file has {total_lines} lines; showing the last {limit} — read with a larger `limit` if needed")),
    }))
}

/// config-delete logs：删除指定日志文件（仅精确文件名，防路径穿越）。
fn delete_log_file(args: &Value) -> napi::Result<Value> {
    let key = required_string(args, "key")?;
    if !valid_log_name(key) {
        return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "Invalid log key: \"{key}\". config-delete logs only accepts an exact log file name (e.g. 2026-08-03-error.log)"
                ),
            ));
    }
    let path = log_dir().join(key);
    let deleted = if path.exists() {
        std::fs::remove_file(&path).is_ok()
    } else {
        false
    };
    Ok(json!({
        "scope": SCOPE_LOGS,
        "key": key,
        "deleted": deleted,
    }))
}
