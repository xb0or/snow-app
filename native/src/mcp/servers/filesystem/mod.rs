use std::fs;
use std::path::Path;

use base64::Engine;
use napi::bindgen_prelude::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::super::service::McpService;
use super::super::tools::McpTool;
use super::remote_workspace::{
    execute_remote_workspace_command, is_ssh_path, RemoteWorkspaceCallback,
};

mod office;
mod text_codec;
mod fuzzy_edit;
mod io;

use text_codec::{decode_text_bytes, encode_text, encode_text_back, encoding_for_label};

/// 模糊匹配的最低相似度阈值（0.0 ~ 1.0）。
/// 当 searchContent 与文件中某段内容相似度达到此值时，视为匹配成功。
/// 0.75 时误替换率偏高，抬高至 0.85 以降低 AI 转述内容被错误匹配的风险。
const FUZZY_MATCH_THRESHOLD: f64 = 0.85;

/// 编辑成功后，在响应中返回编辑区域前后各多少行上下文供 AI 复核。
const EDIT_REVIEW_CONTEXT_LINES: usize = 5;

/// 当 searchContent 不含行号前缀但文件内容含行号前缀（或反之）时，
/// 逐行剥离前缀后重试匹配。
const LINE_PREFIX_REGEX: &str = r"^\s*\d+[\s\|:]*";

pub struct FilesystemService;

impl FilesystemService {
    pub fn new() -> Self {
        FilesystemService
    }
}

const SERVER_ID: &str = "filesystem";

impl McpService for FilesystemService {
    fn id(&self) -> &str {
        SERVER_ID
    }

    fn tools(&self) -> Vec<McpTool> {
        vec![
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "read".to_string(),
                description: "Read file content with line numbers. Supports text files, images, Office documents (pdf, docx, doc, xlsx, xls, xlsb, xlsm, ods, csv, pptx, ppt), and directories. Legacy .doc/.ppt files are extracted via system tools (macOS textutil, LibreOffice if installed) with a UTF-16 text scan fallback. Text file encoding is auto-detected (UTF-8, UTF-16/32 with BOM, GBK/GB18030, Big5, Shift_JIS, EUC-KR, windows-1252, etc.) and decoded to UTF-8. Office documents are extracted to plain text and can be very long - ALWAYS read them in chunks via startLine/endLine (e.g. read the first 100 lines first, then decide the next range based on the returned totalLines) instead of loading the whole document at once.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "Path to the file to read or directory to list."
                        },
                        "startLine": {
                            "type": "number",
                            "description": "Optional starting line number (1-indexed). Pair with endLine to page through large files and Office documents."
                        },
                        "endLine": {
                            "type": "number",
                            "description": "Optional ending line number (1-indexed). Pair with startLine to page through large files and Office documents."
                        }
                    },
                    "required": ["filePath"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "replace_edit".to_string(),
                description: "Fuzzy search-and-replace editing. Finds searchContent in the file and replaces it with replaceContent. The file's original text encoding is auto-detected and preserved on write-back (the edited file keeps its original encoding and BOM). IMPORTANT: searchContent must be COPIED EXACTLY from the file - do NOT include line number prefixes (like \"42:\") that appear in read output, do NOT retype or paraphrase. Copy the raw source text verbatim. If the exact text is not found, a fuzzy match is attempted; on failure the error includes the closest matching region to help you correct your searchContent. On success the response includes a \"review\" field with the edited region plus surrounding context lines (edited lines marked with \">>>\") - always verify the edit landed correctly. ESCAPE SEQUENCES: text inside string literals (e.g. Rust/Python/JSON source) stores escapes like \\n, \\t, \\\", \\\\ as literal backslash + character pairs in the file. When searchContent or replaceContent touches such text, keep the escapes in their literal form exactly as shown by filesystem-read output - never convert a literal backslash-n into a real newline, and never convert a real newline into a literal \\n. Use a real newline only when the file actually contains one; use a literal escape sequence only when the file text shows that escape.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "Path to the file to edit."
                        },
                        "searchContent": {
                            "type": "string",
                            "description": "The EXACT raw source text to find in the file. Do NOT include line number prefixes from read output. Copy verbatim from the file content. If the file text contains escape sequences (like \\n, \\t, \\\" inside string literals), copy them as literal backslash + character text - do NOT convert them to real newlines/tabs/quotes."
                        },
                        "replaceContent": {
                            "type": "string",
                            "description": "New content to replace with. Match the file's escape style: write a literal backslash-n (two characters) when the file should keep an escape sequence like \\n; write a real newline only when the file actually uses real newlines."
                        },
                        "occurrence": {
                            "type": "number",
                            "description": "Which match to replace if multiple found (1-indexed, default 1)."
                        }
                    },
                    "required": ["filePath", "searchContent", "replaceContent"]
                }),
            },
            McpTool {
                server_id: SERVER_ID.to_string(),
                name: "create".to_string(),
                description: "Create a new file with content. Automatically creates parent directories if needed. If the file already exists, an error is returned with the current file size and line count - use overwrite=true to replace it, or use replace_edit instead to modify the existing file. The optional encoding parameter (default: utf-8) controls the file's byte encoding, e.g. gbk, gb18030, big5, shift_jis, euc-kr, utf-16le, utf-16be, windows-1252.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "Path where the file should be created."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file."
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "Whether to overwrite the file if it already exists (default false)."
                        },
                        "encoding": {
                            "type": "string",
                            "description": "Byte encoding of the created file (default utf-8). Supports encoding labels like gbk, gb18030, big5, shift_jis, euc-kr, utf-16le, utf-16be, windows-1252."
                        }
                    },
                    "required": ["filePath", "content","overwrite"]
                }),
            },
        ]
    }

    fn execute(&self, tool_name: &str, args: &Value) -> napi::Result<Value> {
        match tool_name {
            "read" => self.execute_read(args),
            "replace_edit" => self.execute_replace_edit(args),
            "create" => self.execute_create(args),
            _ => Err(Error::new(
                Status::GenericFailure,
                format!(
                    "Unknown tool: \"{}\" for MCP server \"filesystem\". Available tools: [filesystem-read, filesystem-replace_edit, filesystem-create]",
                    tool_name
                ),
            )),
        }
    }
}

impl FilesystemService {
    pub async fn execute_async(
        &self,
        tool_name: &str,
        args: &Value,
        on_remote_workspace_command: &RemoteWorkspaceCallback,
        cancel_token: Option<&CancellationToken>,
    ) -> napi::Result<Value> {
        let file_path = args.get("filePath").and_then(Value::as_str);
        if file_path.is_some_and(is_ssh_path) {
            return execute_remote_workspace_command(
                on_remote_workspace_command,
                &format!("filesystem-{tool_name}"),
                args,
                cancel_token,
            )
            .await;
        }

        match tool_name {
            "read" => self.execute_read(args),
            "replace_edit" => self.execute_replace_edit(args),
            "create" => self.execute_create(args),
            _ => self.execute(tool_name, args),
        }
    }

    fn execute_read(&self, args: &Value) -> napi::Result<Value> {
        let file_path = args
            .get("filePath")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let keys: Vec<String> = args
                    .as_object()
                    .map(|object| object.keys().cloned().collect())
                    .unwrap_or_default();
                Error::new(
                    Status::InvalidArg,
                    format!(
                        "filePath is required for tool \"filesystem-read\". Received keys: [{}]. Please provide a valid file path.",
                        keys.join(", ")
                    ),
                )
            })?;

        let start_line = args.get("startLine").and_then(|value| value.as_u64());
        let end_line = args.get("endLine").and_then(|value| value.as_u64());

        io::read_path(file_path, start_line, end_line)
    }

    fn execute_replace_edit(&self, args: &Value) -> napi::Result<Value> {
        let file_path = io::normalize_path(
            args
                .get("filePath")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    let keys: Vec<String> = args.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                    Error::new(
                        Status::InvalidArg,
                        format!(
                            "filePath is required for tool \"filesystem-replace_edit\". Received keys: [{}]. Please provide a valid file path.",
                            keys.join(", ")
                        ),
                    )
                })?,
        );

        let search_content = args
            .get("searchContent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    "searchContent is required for tool \"filesystem-replace_edit\". Please provide the content to search for in the file.".to_string(),
                )
            })?;

        let replace_content = args
            .get("replaceContent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::new(
                    Status::InvalidArg,
                    "replaceContent is required for tool \"filesystem-replace_edit\". Please provide the new content to replace with.".to_string(),
                )
            })?;

        let occurrence = args
            .get("occurrence")
            .and_then(|v| v.as_u64())
            .map(|o| o as usize)
            .unwrap_or(1);

        // 按字节读取并自动检测文件原始编码，统一解码为 UTF-8 后在字符串上编辑，
        // 写回时再转回原始编码（含 BOM），保证非 UTF-8 文件编辑后编码不变。
        let bytes = fs::read(&file_path).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to read file: {} (path: {})", e, file_path),
            )
        })?;
        let decoded = decode_text_bytes(&bytes).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to decode file as text: {} (path: {})", e, file_path),
            )
        })?;
        let content = decoded.text;
        let original_encoding = decoded.encoding;
        let had_bom = decoded.had_bom;

        // 检测文件主要使用的行尾风格，并将 replace_content 适配为相同风格，
        // 避免在 CRLF 文件中插入 LF 行尾导致混合行尾。
        let replace_content = fuzzy_edit::adapt_line_endings(replace_content, &content);

        // 全程使用 split('\n') 而非 lines()，保留 \r 在行内容中。
        // 匹配时用 normalize_whitespace 比较（忽略空白差异含 \r），
        // 替换时用 splice 在行数组上操作，天然保持文件原有行尾风格。
        let file_lines: Vec<&str> = content.split('\n').collect();
        let total_lines = file_lines.len();

        // search_lines_variants: 每个元素是 (变体名, 行数组)
        let search_content_stripped = fuzzy_edit::try_strip_line_prefixes(search_content);
        let variants: Vec<(&str, Vec<&str>)> =
            vec![("exact", search_content.split('\n').collect())]
                .into_iter()
                .chain(
                    search_content_stripped
                        .as_ref()
                        .map(|s| ("exact_after_stripping_prefixes", s.split('\n').collect())),
                )
                .collect();

        // Step 1: 精确行级匹配
        // 在 file_lines 中查找与 search 某个变体完全相同的行序列（归一化比较）。
        for (match_type, search_lines) in &variants {
            let search_line_count = search_lines.len();
            if search_line_count == 0 || search_line_count > file_lines.len() {
                continue;
            }

            // 收集所有匹配位置
            let mut match_positions: Vec<usize> = Vec::new();
            for start in 0..=(file_lines.len() - search_line_count) {
                let all_match = search_lines.iter().enumerate().all(|(i, &sline)| {
                    fuzzy_edit::normalize_whitespace(&file_lines[start + i])
                        == fuzzy_edit::normalize_whitespace(sline)
                });
                if all_match {
                    match_positions.push(start);
                }
            }

            if let Some(&target_start) = match_positions.get(occurrence.saturating_sub(1)) {
                let end_line = target_start + search_line_count;

                // 行级替换：用 splice 替换目标行范围
                let replacement_lines: Vec<String> =
                    replace_content.split('\n').map(str::to_owned).collect();
                let mut new_lines: Vec<String> = file_lines.iter().map(|s| s.to_string()).collect();
                new_lines.splice(target_start..end_line, replacement_lines);
                let new_content = new_lines.join("\n");

                let new_bytes =
                    encode_text_back(&new_content, original_encoding, had_bom).map_err(|e| {
                        Error::new(
                            Status::GenericFailure,
                            format!(
                                "Failed to encode edited content back to original encoding: {} (path: {})",
                                e, file_path
                            ),
                        )
                    })?;
                fs::write(&file_path, &new_bytes).map_err(|e| {
                    Error::new(
                        Status::GenericFailure,
                        format!("Failed to write file: {} (path: {})", e, file_path),
                    )
                })?;

                let review = fuzzy_edit::build_edit_review_context_lines(
                    &new_content,
                    target_start,
                    target_start + replace_content.split('\n').count().saturating_sub(1),
                );

                return Ok(json!({
                    "success": true,
                    "totalMatches": match_positions.len(),
                    "occurrence": occurrence,
                    "matchType": match_type,
                    "matchedLineStart": target_start + 1,
                    "matchedLineEnd": end_line,
                    "review": review
                }));
            }
        }

        // Step 1.5: 字面子串匹配
        // 覆盖 search_content 只是某一行片段（例如超长单行字符串中的一段）或
        // 跨行片段的场景，这是整行精确/模糊匹配无法命中的情况。
        if let Some((new_content, edit_start_line, edit_end_line, total_matches)) =
            fuzzy_edit::try_substring_replace(&content, search_content, &replace_content, occurrence)
        {
            let new_bytes =
                encode_text_back(&new_content, original_encoding, had_bom).map_err(|e| {
                    Error::new(
                        Status::GenericFailure,
                        format!(
                            "Failed to encode edited content back to original encoding: {} (path: {})",
                            e, file_path
                        ),
                    )
                })?;
            fs::write(&file_path, &new_bytes).map_err(|e| {
                Error::new(
                    Status::GenericFailure,
                    format!("Failed to write file: {} (path: {})", e, file_path),
                )
            })?;

            let review =
                fuzzy_edit::build_edit_review_context_lines(&new_content, edit_start_line, edit_end_line);

            return Ok(json!({
                "success": true,
                "totalMatches": total_matches,
                "occurrence": occurrence,
                "matchType": "substring",
                "matchedLineStart": edit_start_line + 1,
                "matchedLineEnd": edit_end_line + 1,
                "review": review
            }));
        }

        // Step 2: 模糊行匹配（基于 Levenshtein 距离 + 变窗口 + 预过滤）
        if let Some((start_line, end_line, similarity)) =
            fuzzy_edit::find_best_line_match_v2(search_content, &file_lines)
        {
            if similarity >= FUZZY_MATCH_THRESHOLD {
                let replacement_lines: Vec<String> =
                    replace_content.split('\n').map(str::to_owned).collect();
                let mut new_lines: Vec<String> = file_lines.iter().map(|s| s.to_string()).collect();
                new_lines.splice(start_line..end_line, replacement_lines);
                let new_content = new_lines.join("\n");

                let new_bytes =
                    encode_text_back(&new_content, original_encoding, had_bom).map_err(|e| {
                        Error::new(
                            Status::GenericFailure,
                            format!(
                                "Failed to encode edited content back to original encoding: {} (path: {})",
                                e, file_path
                            ),
                        )
                    })?;
                fs::write(&file_path, &new_bytes).map_err(|e| {
                    Error::new(
                        Status::GenericFailure,
                        format!("Failed to write file: {} (path: {})", e, file_path),
                    )
                })?;

                let review = fuzzy_edit::build_edit_review_context_lines(
                    &new_content,
                    start_line,
                    start_line + replace_content.split('\n').count().saturating_sub(1),
                );

                return Ok(json!({
                    "success": true,
                    "matchType": "fuzzy",
                    "similarity": similarity,
                    "matchedLineStart": start_line + 1,
                    "matchedLineEnd": end_line,
                    "totalLines": total_lines,
                    "review": review
                }));
            }
        }

        // Step 3: 所有匹配策略均失败 - 返回包含最相似区间上下文的详细错误
        let error_msg = fuzzy_edit::build_search_not_found_error_v2(
            search_content,
            &file_lines,
            &file_path,
            total_lines,
        );

        Err(Error::new(Status::GenericFailure, error_msg))
    }

    fn execute_create(&self, args: &Value) -> napi::Result<Value> {
        let file_path = io::normalize_path(
            args
                .get("filePath")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    let keys: Vec<String> = args.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                    Error::new(
                        Status::InvalidArg,
                        format!(
                            "filePath is required for tool \"filesystem-create\". Received keys: [{}]. Please provide a valid file path.",
                            keys.join(", ")
                        ),
                    )
                })?,
        );

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::new(Status::InvalidArg, "content is required for tool \"filesystem-create\". Please provide the content to write to the file.".to_string()))?;

        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 可选的输出编码（默认 UTF-8）。无效 label 直接报错，避免静默回退。
        let encoding = args
            .get("encoding")
            .and_then(|v| v.as_str())
            .map(|label| {
                encoding_for_label(label).ok_or_else(|| {
                    Error::new(
                        Status::InvalidArg,
                        format!(
                            "Unsupported encoding label: \"{}\". Supported labels include: utf-8, gbk, gb18030, big5, shift_jis, euc-kr, utf-16le, utf-16be, windows-1252.",
                            label
                        ),
                    )
                })
            })
            .transpose()?
            .unwrap_or(encoding_rs::UTF_8);

        let path = Path::new(&file_path);

        if path.exists() && !overwrite {
            let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let line_count = fs::read(path)
                .map(|bytes| {
                    // 行数仅为错误信息参考，用 lossy 解码避免非 UTF-8 文件统计失败。
                    String::from_utf8_lossy(&bytes).lines().count()
                })
                .unwrap_or(0);
            return Err(Error::new(
                Status::GenericFailure,
                format!(
                    "File already exists: {} ({} bytes, {} lines). To overwrite this file, set overwrite=true. To modify the existing file, use filesystem-replace_edit instead.",
                    file_path, file_size, line_count
                ),
            ));
        }

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::new(
                        Status::GenericFailure,
                        format!("Failed to create directories: {} (path: {})", e, file_path),
                    )
                })?;
            }
        }

        // 将 UTF-8 内容按指定编码转为字节后写入。
        let bytes = encode_text(content, encoding).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!(
                    "Failed to encode content to \"{}\": {} (path: {})",
                    encoding.name(),
                    e,
                    file_path
                ),
            )
        })?;

        fs::write(path, &bytes).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to write file: {} (path: {})", e, file_path),
            )
        })?;

        let byte_count = bytes.len();
        let line_count = content.lines().count();

        Ok(json!({
            "success": true,
            "path": file_path,
            "bytes": byte_count,
            "lines": line_count
        }))
    }
}
