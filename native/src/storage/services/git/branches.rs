use napi::bindgen_prelude::*;

use super::{is_git_repo, run_git, run_git_raw, GitBranch};

pub fn get_git_branches(repo_path: &str) -> Result<Vec<GitBranch>> {
    if !is_git_repo(repo_path) {
        return Ok(Vec::new());
    }

    let output = run_git(
        repo_path,
        &[
            "branch",
            "--list",
            "--all",
            "--format=%(HEAD)%(refname:short) %(objectname:short) %(upstream:short)",
        ],
    )?;

    let mut branches: Vec<GitBranch> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_current = trimmed.starts_with('*');
        let rest = if is_current {
            trimmed[1..].trim_start()
        } else {
            trimmed
        };

        let parts: Vec<&str> = rest.split_whitespace().collect();
        let name = match parts.first() {
            Some(n) => *n,
            None => continue,
        };

        if name == "HEAD" {
            continue;
        }

        let is_remote = name.contains('/');
        let remote_name = if is_remote {
            let slash_idx = name.find('/').unwrap();
            Some(name[..slash_idx].to_string())
        } else {
            None
        };

        branches.push(GitBranch {
            name: name.to_string(),
            is_current,
            is_remote,
            remote_name,
        });
    }

    Ok(branches)
}

/// Get the current branch name via `git rev-parse --abbrev-ref HEAD`.
/// Returns an empty string for detached HEAD or on error.
pub(crate) fn get_current_branch_name(repo_path: &str) -> Result<String> {
    let output = run_git_raw(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = output.trim();
    if branch.is_empty() || branch == "HEAD" {
        Ok(String::new())
    } else {
        Ok(branch.to_string())
    }
}
