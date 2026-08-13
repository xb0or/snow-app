use napi::bindgen_prelude::*;

use super::{is_git_repo, run_git, GitFileStatus, GitStatusResult};

fn parse_status_char(c: char) -> String {
    if c == ' ' {
        return String::new();
    }
    c.to_string()
}

fn derive_display_status(index_status: &str, workdir_status: &str) -> String {
    if index_status == "R" {
        return "R".to_string();
    }
    if index_status == "C" {
        return "C".to_string();
    }
    if workdir_status == "?" {
        return "U".to_string();
    }
    if workdir_status == "!" {
        return "I".to_string();
    }
    if index_status == "A" {
        return "A".to_string();
    }
    if index_status == "M" {
        return "M".to_string();
    }
    if index_status == "D" {
        return "D".to_string();
    }
    if workdir_status == "M" {
        return "M".to_string();
    }
    if workdir_status == "D" {
        return "D".to_string();
    }
    if !index_status.is_empty() && !workdir_status.is_empty() {
        return "MM".to_string();
    }
    if !index_status.is_empty() {
        return index_status.to_string();
    }
    if !workdir_status.is_empty() {
        return workdir_status.to_string();
    }
    "?".to_string()
}

// ===== Public API =====

pub fn get_git_status(repo_path: &str, status_limit: i32) -> Result<GitStatusResult> {
    // Gracefully handle non-repo paths: return is_repo=false instead of
    // propagating git's "fatal: not a git repository" error to the UI.
    // This covers both the case where .git doesn't exist and the edge case
    // where .git exists but git itself rejects the path (e.g. corrupted
    // .git, broken worktree pointer, or .git file pointing to a missing
    // gitdir).
    if !is_git_repo(repo_path) {
        return Ok(GitStatusResult {
            is_repo: false,
            current_branch: String::new(),
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            staged_count: 0,
            unstaged_count: 0,
            untracked_count: 0,
            status_limit_hit: false,
        });
    }

    // If is_git_repo returned true but git still fails, distinguish two
    // cases:
    // - git reports "not a git repository" (corrupted repo, broken
    //   worktree, .git file pointing to a missing gitdir, ...): treat as
    //   a non-repo rather than surfacing a raw git error to the user.
    // - any other error (git executable missing from PATH, permission
    //   denied, ...): propagate it so the UI shows a clear message
    //   instead of a misleading "Not a git repository".
    let status_out = match run_git(
        repo_path,
        &["status", "--porcelain=v1", "-b", "--find-renames", "-uall"],
    ) {
        Ok(out) => out,
        Err(e) => {
            if e.to_string().contains("not a git repository") {
                return Ok(GitStatusResult {
                    is_repo: false,
                    current_branch: String::new(),
                    upstream: None,
                    ahead: 0,
                    behind: 0,
                    files: Vec::new(),
                    staged_count: 0,
                    unstaged_count: 0,
                    untracked_count: 0,
                    status_limit_hit: false,
                });
            }
            return Err(e);
        }
    };
    let lines: Vec<&str> = status_out.lines().filter(|l| !l.is_empty()).collect();

    let mut current_branch = String::new();
    let mut upstream: Option<String> = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut files: Vec<GitFileStatus> = Vec::new();

    // Cap the reported change list (mirrors VSCode's `git.statusLimit`).
    // Negative or zero disables the cap.
    let limit: usize = if status_limit > 0 {
        status_limit as usize
    } else {
        usize::MAX
    };
    let mut status_limit_hit = false;

    for line in &lines {
        if line.starts_with("## ") {
            let branch_part = &line[3..];

            // Parse upstream
            if let Some(idx) = branch_part.find("...") {
                let after = &branch_part[idx + 3..];
                let upstream_name = after.split_whitespace().next().unwrap_or("");
                if !upstream_name.is_empty() {
                    upstream = Some(upstream_name.to_string());
                }
            }

            // Parse ahead/behind
            let lower = branch_part.to_lowercase();
            if let Some(ahead_pos) = lower.find("ahead ") {
                let rest = &branch_part[ahead_pos + 6..];
                if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
                    ahead = rest[..end].parse().unwrap_or(0);
                } else {
                    ahead = rest.parse().unwrap_or(0);
                }
            }
            if let Some(behind_pos) = lower.find("behind ") {
                let rest = &branch_part[behind_pos + 7..];
                if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
                    behind = rest[..end].parse().unwrap_or(0);
                } else {
                    behind = rest.parse().unwrap_or(0);
                }
            }

            // Parse branch name
            let branch_name_raw: &str = if let Some(idx) = branch_part.find("...") {
                &branch_part[..idx]
            } else {
                let end = branch_part.find(' ').unwrap_or(branch_part.len());
                &branch_part[..end]
            };

            if branch_name_raw.starts_with("HEAD") {
                current_branch = "HEAD".to_string();
            } else {
                current_branch = branch_name_raw.to_string();
            }
            continue;
        }

        // File status lines: XY <path>
        if line.len() < 3 {
            continue;
        }

        if files.len() >= limit {
            status_limit_hit = true;
            continue;
        }

        let chars: Vec<char> = line.chars().collect();
        let index_status = parse_status_char(chars[0]);
        let workdir_status = parse_status_char(chars[1]);
        let rest = &line[3..];

        let mut file_path = rest.to_string();
        let mut old_path: Option<String> = None;

        if let Some(arrow_idx) = rest.find(" -> ") {
            old_path = Some(rest[..arrow_idx].to_string());
            file_path = rest[arrow_idx + 4..].to_string();
        }

        // Strip surrounding quotes
        if file_path.starts_with('"') && file_path.ends_with('"') && file_path.len() >= 2 {
            file_path = file_path[1..file_path.len() - 1].to_string();
        }

        files.push(GitFileStatus {
            path: file_path,
            old_path,
            index_status: chars[0].to_string(),
            workdir_status: chars[1].to_string(),
            status: derive_display_status(&index_status, &workdir_status),
        });
    }

    let mut staged_count = 0;
    let mut unstaged_count = 0;
    let mut untracked_count = 0;

    for f in &files {
        if f.workdir_status == "?" || f.workdir_status == "!" {
            untracked_count += 1;
        } else {
            if !f.index_status.is_empty() && f.index_status != " " && f.index_status != "?" {
                staged_count += 1;
            }
            if !f.workdir_status.is_empty() && f.workdir_status != " " && f.workdir_status != "?" {
                unstaged_count += 1;
            }
        }
    }

    Ok(GitStatusResult {
        is_repo: true,
        current_branch,
        upstream,
        ahead,
        behind,
        files,
        staged_count,
        unstaged_count,
        untracked_count,
        status_limit_hit,
    })
}
