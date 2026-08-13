use super::*;

use serde_json::{json, Value};

/// 将所有空白字符（含 \r、\n、\t、BOM 等）压缩为单个空格并 trim 首尾。
/// 仅用于比较两段文本是否"内容等价"，不修改原始文件。
/// 这天然解决了 CRLF/LF 行尾差异、多余空格/制表符差异等问题。
pub(crate) fn normalize_whitespace(content: &str) -> String {
    let mut normalized = String::with_capacity(content.len());
    let mut previous_was_whitespace = true;

    for character in content.chars() {
        let is_whitespace = character.is_whitespace() || character == '\u{feff}';
        if is_whitespace {
            if !previous_was_whitespace {
                normalized.push(' ');
            }
        } else {
            normalized.push(character);
        }
        previous_was_whitespace = is_whitespace;
    }

    normalized.trim_end().to_owned()
}

/// 计算两个字符串之间的 Levenshtein 相似度（0.0 ~ 1.0），带提前剪枝优化。
fn compute_levenshtein_similarity(left: &str, right: &str, threshold: f64) -> f64 {
    let left_u16: Vec<u16> = left.encode_utf16().collect();
    let right_u16: Vec<u16> = right.encode_utf16().collect();

    if left_u16.is_empty() {
        return if right_u16.is_empty() { 1.0 } else { 0.0 };
    }
    if right_u16.is_empty() {
        return 0.0;
    }

    let max_length = left_u16.len().max(right_u16.len());
    let length_ratio = left_u16.len().min(right_u16.len()) as f64 / max_length as f64;
    if threshold > 0.0 && length_ratio < threshold {
        return length_ratio;
    }

    let max_distance = (max_length as f64 * (1.0 - threshold)).ceil() as usize;

    // 带提前终止的 Levenshtein 距离
    if left_u16 == right_u16 {
        return 1.0;
    }
    if left_u16.len().abs_diff(right_u16.len()) > max_distance {
        return 0.0;
    }

    let mut previous: Vec<usize> = (0..=right_u16.len()).collect();
    for (left_index, left_unit) in left_u16.iter().enumerate() {
        let mut current = Vec::with_capacity(right_u16.len() + 1);
        current.push(left_index + 1);
        let mut minimum = left_index + 1;

        for (right_index, right_unit) in right_u16.iter().enumerate() {
            let value = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_unit != right_unit));
            current.push(value);
            minimum = minimum.min(value);
        }

        if minimum > max_distance {
            return 0.0;
        }
        previous = current;
    }

    let distance = previous[right_u16.len()];
    1.0 - distance as f64 / max_length as f64
}

/// 根据文件内容的主要行尾风格，调整 text 的行尾以匹配。
/// 若文件以 CRLF 为主，则将 text 中的行尾转为 CRLF；
/// 若文件以 LF 为主，则将 text 中的行尾转为 LF。
/// 若文件为空或无法判定，则原样返回。
pub(crate) fn adapt_line_endings(text: &str, file_content: &str) -> String {
    if file_content.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let crlf_count = file_content.matches("\r\n").count();
    let lf_count = file_content.matches('\n').count();
    let lf_only = lf_count.saturating_sub(crlf_count);

    let use_crlf = crlf_count > lf_only;

    if use_crlf {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        normalized.replace('\n', "\r\n")
    } else {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }
}

/// 如果 searchContent 的每一行都以行号前缀开头（如 "42: " 或 "  10| "），
/// 则剥离所有行号前缀，返回纯内容。否则返回 None。
/// 这处理 AI 从 read 输出中复制了行号前缀的情况。
pub(crate) fn try_strip_line_prefixes(text: &str) -> Option<String> {
    let re = regex::Regex::new(LINE_PREFIX_REGEX).ok()?;

    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let non_empty_count = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if non_empty_count == 0 {
        return None;
    }

    let prefixed_count = lines
        .iter()
        .filter(|l| !l.trim().is_empty() && re.is_match(l))
        .count();

    let ratio = prefixed_count as f64 / non_empty_count as f64;
    if ratio < 0.6 {
        return None;
    }

    let stripped_lines: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else {
                re.replace(line, "").to_string()
            }
        })
        .collect();

    let result = stripped_lines.join("\n");

    if result != text {
        Some(result)
    } else {
        None
    }
}

/// 尝试把 search_content 作为字面子串在完整文件内容中匹配并替换。
/// 覆盖 search_content 只是某一行片段（例如超长单行字符串中的一段）或跨行
/// 片段的场景，这是整行精确/模糊匹配无法命中的情况。先把 search_content 的
/// 行尾适配为文件的行尾风格再做字面查找。成功时返回
/// (新内容, 编辑起始行 0-based, 编辑结束行 0-based inclusive, 总匹配数)。
pub(crate) fn try_substring_replace(
    content: &str,
    search_content: &str,
    replace_content: &str,
    occurrence: usize,
) -> Option<(String, usize, usize, usize)> {
    if search_content.is_empty() {
        return None;
    }
    let adapted_search = adapt_line_endings(search_content, content);
    if adapted_search.is_empty() {
        return None;
    }

    // 收集所有非重叠出现位置（字节索引）。
    let mut positions: Vec<usize> = Vec::new();
    let mut cursor = 0usize;
    while cursor <= content.len() {
        match content[cursor..].find(&adapted_search) {
            Some(rel) => {
                let abs = cursor + rel;
                positions.push(abs);
                cursor = abs + adapted_search.len();
            }
            None => break,
        }
    }
    if positions.is_empty() {
        return None;
    }

    let target = *positions.get(occurrence.saturating_sub(1))?;
    let end = target + adapted_search.len();

    let mut new_content = String::with_capacity(content.len() + replace_content.len());
    new_content.push_str(&content[..target]);
    new_content.push_str(replace_content);
    new_content.push_str(&content[end..]);

    let edit_start_line = content[..target].matches('\n').count();
    let edit_end_line = edit_start_line + replace_content.split('\n').count().saturating_sub(1);

    Some((new_content, edit_start_line, edit_end_line, positions.len()))
}

/// 在文件行数组中，按行滑动窗口查找与 searchContent 最相似的区间。
/// 基于 normalize_whitespace + Levenshtein 距离 + 变窗口 + 首行预过滤。
/// 返回 (起始行号, 结束行号(不含), 相似度)，均为 0-indexed。
pub(crate) fn find_best_line_match_v2(
    search_content: &str,
    file_lines: &[&str],
) -> Option<(usize, usize, f64)> {
    let search_lines: Vec<&str> = search_content.split('\n').collect();
    if search_lines.is_empty() || file_lines.is_empty() {
        return None;
    }

    let base_window = search_lines.len();
    if base_window > file_lines.len() {
        return None;
    }

    let threshold = FUZZY_MATCH_THRESHOLD;
    let normalized_search = normalize_whitespace(search_content);
    let normalized_first_line =
        normalize_whitespace(search_lines.first().copied().unwrap_or_default());

    // 变窗口：大代码块允许窗口大小浮动以改善边界对齐
    let window_delta = if base_window >= 10 {
        (base_window / 5).clamp(3, 15)
    } else {
        0
    };

    let mut best_similarity: f64 = 0.0;
    let mut best_start: usize = 0;
    let mut best_end: usize = 0;

    for start_index in 0..=(file_lines.len() - base_window) {
        // 首行预过滤：首行相似度低于阈值则跳过
        let normalized_candidate_first = normalize_whitespace(file_lines[start_index]);
        if compute_levenshtein_similarity(&normalized_first_line, &normalized_candidate_first, 0.5)
            < 0.5
        {
            continue;
        }

        // 尝试精确窗口大小
        let exact_candidate = file_lines[start_index..start_index + base_window].join("\n");
        let exact_score = if exact_candidate == search_content {
            1.0
        } else {
            compute_levenshtein_similarity(
                &normalized_search,
                &normalize_whitespace(&exact_candidate),
                threshold,
            )
        };

        if exact_score >= 0.9 {
            if exact_score > best_similarity {
                best_similarity = exact_score;
                best_start = start_index;
                best_end = start_index + base_window;
            }
            if best_similarity >= 0.95 {
                return Some((best_start, best_end, best_similarity));
            }
            continue;
        }

        // 大块：尝试变窗口
        if window_delta > 0 {
            let mut score = exact_score;
            let mut end = start_index + base_window;

            for delta in 1..=window_delta {
                // 更小窗口
                if base_window > delta {
                    let smaller = base_window - delta;
                    let candidate = file_lines[start_index..start_index + smaller].join("\n");
                    let s = if candidate == search_content {
                        1.0
                    } else {
                        compute_levenshtein_similarity(
                            &normalized_search,
                            &normalize_whitespace(&candidate),
                            threshold,
                        )
                    };
                    if s > score {
                        score = s;
                        end = start_index + smaller;
                    }
                }

                // 更大窗口
                let larger = base_window + delta;
                if start_index + larger <= file_lines.len() {
                    let candidate = file_lines[start_index..start_index + larger].join("\n");
                    let s = if candidate == search_content {
                        1.0
                    } else {
                        compute_levenshtein_similarity(
                            &normalized_search,
                            &normalize_whitespace(&candidate),
                            threshold,
                        )
                    };
                    if s > score {
                        score = s;
                        end = start_index + larger;
                    }
                }

                if score >= 0.95 {
                    break;
                }
            }

            if score >= threshold && score > best_similarity {
                best_similarity = score;
                best_start = start_index;
                best_end = end;
                if best_similarity >= 0.95 {
                    return Some((best_start, best_end, best_similarity));
                }
            }
        } else if exact_score >= threshold && exact_score > best_similarity {
            best_similarity = exact_score;
            best_start = start_index;
            best_end = start_index + base_window;
            if best_similarity >= 0.95 {
                return Some((best_start, best_end, best_similarity));
            }
        }
    }

    if best_similarity > 0.0 {
        Some((best_start, best_end, best_similarity))
    } else {
        None
    }
}

/// 构建编辑成功后的复核上下文：返回编辑区域前后各 EDIT_REVIEW_CONTEXT_LINES 行
/// 的带行号代码块（编辑行以 ">>>" 标记），供 AI 复核编辑结果是否正确。
///
/// edit_start_line / edit_end_line 是 0-indexed 的行号（闭区间）。
pub(crate) fn build_edit_review_context_lines(
    new_content: &str,
    edit_start_line: usize,
    edit_end_line: usize,
) -> Value {
    let lines: Vec<&str> = new_content.split('\n').collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return json!({
            "startLine": 0,
            "endLine": 0,
            "editedLineStart": 0,
            "editedLineEnd": 0,
            "totalLines": 0,
            "content": ""
        });
    }

    let edit_end = edit_end_line.min(total_lines.saturating_sub(1));

    let context_start = edit_start_line.saturating_sub(EDIT_REVIEW_CONTEXT_LINES);
    let context_end = (edit_end + 1 + EDIT_REVIEW_CONTEXT_LINES).min(total_lines);

    let block: Vec<String> = (context_start..context_end)
        .map(|i| {
            let marker = if i >= edit_start_line && i <= edit_end {
                ">>>"
            } else {
                "   "
            };
            format!("{} {:>6}: {}", marker, i + 1, lines[i])
        })
        .collect();

    json!({
        "startLine": context_start + 1,
        "endLine": context_end,
        "editedLineStart": edit_start_line + 1,
        "editedLineEnd": edit_end + 1,
        "totalLines": total_lines,
        "content": block.join("\n")
    })
}

/// 构建 "searchContent not found" 的详细错误信息，包含最相似区间的上下文。
pub(crate) fn build_search_not_found_error_v2(
    search_content: &str,
    file_lines: &[&str],
    file_path: &str,
    total_lines: usize,
) -> String {
    let search_lines = search_content.split('\n').count();
    let search_preview: String = search_content
        .chars()
        .take(200)
        .collect::<String>()
        .replace('\n', "\\n");

    if let Some((start_line, end_line, similarity)) =
        find_best_line_match_v2(search_content, file_lines)
    {
        let context_start = start_line.saturating_sub(2);
        let context_end = (end_line + 2).min(file_lines.len());

        let context: Vec<String> = (context_start..context_end)
            .map(|i| {
                let marker = if i >= start_line && i < end_line {
                    ">>>"
                } else {
                    "   "
                };
                format!("{} {:>6}: {}", marker, i + 1, file_lines[i])
            })
            .collect();

        let similarity_percent = (similarity * 100.0) as u32;

        return format!(
            "searchContent not found in file (exact match failed).\n\n\
             File: {} ({} lines total)\n\
             searchContent: {} lines, preview: \"{}\"\n\n\
             Closest matching region (similarity: {}%, lines {}-{}):\n\
             {}\n\n\
             The searchContent does not match any part of the file exactly. Common causes:\n\
              1. searchContent was copied from read output and includes line number prefixes (e.g. \"42:...\") - remove them.\n\
              2. searchContent has been paraphrased or retyped instead of copied verbatim.\n\
              3. The file was modified since it was last read.\n\
             Please re-read the file with filesystem-read and copy the EXACT raw source text as searchContent.",
            file_path,
            total_lines,
            search_lines,
            search_preview,
            similarity_percent,
            start_line + 1,
            end_line,
            context.join("\n")
        );
    }

    format!(
        "searchContent not found in file (exact match failed).\n\n\
         File: {} ({} lines total)\n\
         searchContent: {} lines, preview: \"{}\"\n\n\
         No similar content found in the file. The file may have been modified since it was last read.\n\
         Please re-read the file with filesystem-read and copy the EXACT raw source text as searchContent.",
        file_path,
        total_lines,
        search_lines,
        search_preview
    )
}
