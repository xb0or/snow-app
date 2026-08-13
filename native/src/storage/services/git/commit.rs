use napi::bindgen_prelude::*;

use super::{
    is_git_repo, run_git, GitCheckoutResult, GitCommitResult, GitPushPullResult,
};

pub fn commit_changes(repo_path: &str, message: &str) -> Result<GitCommitResult> {
    if message.trim().is_empty() {
        return Ok(GitCommitResult {
            success: false,
            message: "Commit message is required".to_string(),
            hash: None,
        });
    }

    match run_git(repo_path, &["commit", "-m", message]) {
        Ok(_) => {
            let hash = run_git(repo_path, &["rev-parse", "HEAD"])
                .ok()
                .and_then(|s| {
                    let trimmed = s.trim();
                    if trimmed.len() >= 8 {
                        Some(trimmed[..8].to_string())
                    } else {
                        Some(trimmed.to_string())
                    }
                });

            Ok(GitCommitResult {
                success: true,
                message: "Commit successful".to_string(),
                hash,
            })
        }
        Err(e) => Ok(GitCommitResult {
            success: false,
            message: format!("{e}"),
            hash: None,
        }),
    }
}

pub fn push_changes(repo_path: &str) -> Result<GitPushPullResult> {
    match run_git(repo_path, &["push"]) {
        Ok(stdout) => {
            let msg = if stdout.trim().is_empty() {
                "Push successful".to_string()
            } else {
                stdout.trim().to_string()
            };
            Ok(GitPushPullResult {
                success: true,
                message: msg,
            })
        }
        Err(e) => Ok(GitPushPullResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn pull_changes(repo_path: &str) -> Result<GitPushPullResult> {
    match run_git(repo_path, &["pull"]) {
        Ok(stdout) => {
            let msg = if stdout.trim().is_empty() {
                "Pull successful".to_string()
            } else {
                stdout.trim().to_string()
            };
            Ok(GitPushPullResult {
                success: true,
                message: msg,
            })
        }
        Err(e) => Ok(GitPushPullResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

/// Fetch from the remote without merging. Used by the UI to keep the
/// ahead/behind counts (and thus the "remote has updates" indicator)
/// fresh. Never throws: failures (offline, no remote, auth) are reported
/// via `success: false` so background polling can ignore them silently.
pub fn fetch_remote(repo_path: &str) -> Result<GitPushPullResult> {
    if !is_git_repo(repo_path) {
        return Ok(GitPushPullResult {
            success: false,
            message: "Not a git repository".to_string(),
        });
    }

    // Skip repos without any remote configured — `git fetch` would fail.
    let has_remote = !run_git(repo_path, &["remote"])?.trim().is_empty();
    if !has_remote {
        return Ok(GitPushPullResult {
            success: true,
            message: "No remote configured".to_string(),
        });
    }

    match run_git(repo_path, &["fetch", "--quiet", "--prune"]) {
        Ok(_) => Ok(GitPushPullResult {
            success: true,
            message: "Fetch successful".to_string(),
        }),
        Err(e) => Ok(GitPushPullResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn checkout_branch(repo_path: &str, branch_name: &str) -> Result<GitCheckoutResult> {
    // If the branch name contains '/', it's a remote tracking branch (e.g. "origin/main").
    // Running `git checkout origin/main` would enter detached HEAD state.
    // Instead, extract the local branch name and create a tracking branch.
    if let Some(slash_idx) = branch_name.find('/') {
        let local_name = &branch_name[slash_idx + 1..];

        if !local_name.is_empty() {
            // First, try to checkout the local branch (it may already exist).
            if let Ok(_) = run_git(repo_path, &["checkout", local_name]) {
                return Ok(GitCheckoutResult {
                    success: true,
                    message: format!("Switched to {local_name}"),
                });
            }

            // Local branch doesn't exist; create a new tracking branch.
            match run_git(repo_path, &["checkout", "-b", local_name, branch_name]) {
                Ok(_) => {
                    return Ok(GitCheckoutResult {
                        success: true,
                        message: format!("Switched to {local_name} (tracking {branch_name})"),
                    })
                }
                Err(e) => {
                    return Ok(GitCheckoutResult {
                        success: false,
                        message: format!("{e}"),
                    })
                }
            }
        }
    }

    // Local branch: checkout directly.
    match run_git(repo_path, &["checkout", branch_name]) {
        Ok(_) => Ok(GitCheckoutResult {
            success: true,
            message: format!("Switched to {branch_name}"),
        }),
        Err(e) => Ok(GitCheckoutResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

/// Creates a new branch from the current HEAD and checks it out immediately.
///
/// Uses `git checkout -b <branch_name>` which fails if the branch already
/// exists, preventing accidental overwrites. The caller is responsible for
/// validating the branch name format before calling this function.
pub fn create_branch(repo_path: &str, branch_name: &str) -> Result<GitCheckoutResult> {
    match run_git(repo_path, &["checkout", "-b", branch_name]) {
        Ok(_) => Ok(GitCheckoutResult {
            success: true,
            message: format!("Created and switched to {branch_name}"),
        }),
        Err(e) => Ok(GitCheckoutResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}
