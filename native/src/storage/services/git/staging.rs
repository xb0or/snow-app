use napi::bindgen_prelude::*;

use super::{run_git, GitStageResult};

pub fn stage_files(repo_path: &str, file_paths: &[String]) -> Result<GitStageResult> {
    if file_paths.is_empty() {
        return Ok(GitStageResult {
            success: true,
            message: "No files to stage".to_string(),
        });
    }

    let args: Vec<&str> = file_paths.iter().map(|s| s.as_str()).collect();
    let mut full_args = vec!["add", "--"];
    full_args.extend(args);

    match run_git(repo_path, &full_args) {
        Ok(_) => Ok(GitStageResult {
            success: true,
            message: "Files staged successfully".to_string(),
        }),
        Err(e) => Ok(GitStageResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn unstage_files(repo_path: &str, file_paths: &[String]) -> Result<GitStageResult> {
    if file_paths.is_empty() {
        return Ok(GitStageResult {
            success: true,
            message: "No files to unstage".to_string(),
        });
    }

    let args: Vec<&str> = file_paths.iter().map(|s| s.as_str()).collect();
    let mut full_args = vec!["reset", "HEAD", "--"];
    full_args.extend(args);

    match run_git(repo_path, &full_args) {
        Ok(_) => Ok(GitStageResult {
            success: true,
            message: "Files unstaged successfully".to_string(),
        }),
        Err(e) => Ok(GitStageResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn stage_all(repo_path: &str) -> Result<GitStageResult> {
    match run_git(repo_path, &["add", "--all"]) {
        Ok(_) => Ok(GitStageResult {
            success: true,
            message: "All changes staged".to_string(),
        }),
        Err(e) => Ok(GitStageResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn unstage_all(repo_path: &str) -> Result<GitStageResult> {
    match run_git(repo_path, &["reset", "HEAD"]) {
        Ok(_) => Ok(GitStageResult {
            success: true,
            message: "All changes unstaged".to_string(),
        }),
        Err(e) => Ok(GitStageResult {
            success: false,
            message: format!("{e}"),
        }),
    }
}

pub fn discard_changes(repo_path: &str, file_paths: &[String]) -> Result<GitStageResult> {
    if file_paths.is_empty() {
        return Ok(GitStageResult {
            success: true,
            message: "No files to discard".to_string(),
        });
    }

    // Partition into untracked files ("?" workdir status) and tracked files.
    // Untracked files: `git clean -f -- <path>` removes them.
    // Tracked files: `git checkout -- <path>` restores them to HEAD state.
    let mut untracked: Vec<&str> = Vec::new();
    let mut tracked: Vec<&str> = Vec::new();

    // Query the current status to classify each file path.
    let status_output = match run_git(repo_path, &["status", "--porcelain", "-z", "-uall"]) {
        Ok(s) => s,
        Err(e) => {
            return Ok(GitStageResult {
                success: false,
                message: format!("{e}"),
            })
        }
    };

    let path_set: std::collections::HashSet<&str> = file_paths.iter().map(|s| s.as_str()).collect();

    for entry in status_output.split('\0') {
        if entry.is_empty() {
            continue;
        }
        // porcelain format: "XY <path>" (first 3 chars: X=index, Y=workdir, space, then path)
        let xy = &entry[..2];
        let path = entry[3..].trim_start_matches('"');
        if path_set.contains(path) {
            if xy.starts_with('?') {
                untracked.push(path);
            } else {
                tracked.push(path);
            }
        }
    }

    // If a requested path wasn't found in status output, treat it as tracked
    // (checkout -- will handle it or produce an error).
    for p in &path_set {
        if !untracked.contains(p) && !tracked.contains(p) {
            tracked.push(p);
        }
    }

    if !tracked.is_empty() {
        let mut args = vec!["checkout", "--"];
        args.extend(tracked.iter().copied());
        match run_git(repo_path, &args) {
            Ok(_) => {}
            Err(e) => {
                return Ok(GitStageResult {
                    success: false,
                    message: format!("{e}"),
                })
            }
        }
    }

    if !untracked.is_empty() {
        let mut args = vec!["clean", "-f", "--"];
        args.extend(untracked.iter().copied());
        match run_git(repo_path, &args) {
            Ok(_) => {}
            Err(e) => {
                return Ok(GitStageResult {
                    success: false,
                    message: format!("{e}"),
                })
            }
        }
    }

    Ok(GitStageResult {
        success: true,
        message: "Changes discarded successfully".to_string(),
    })
}
