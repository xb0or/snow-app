use std::path::{Component, Path, PathBuf};

use napi::bindgen_prelude::*;
use serde_json::Value;

use super::collect::with_database_path;
use super::super::servers::remote_workspace::{is_ssh_path, resolve_remote_workspace_path};

const PLAN_WRITE_DIRECTORIES: [[&str; 2]; 2] = [[".snow", "plan"], [".trellis", "tasks"]];

pub(crate) async fn is_allowed_plan_document_write(
    project_id: Option<&str>,
    args: &Value,
) -> napi::Result<bool> {
    let Some(file_path) = args
        .get("filePath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(false);
    };
    let Some(project_id) = project_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return Ok(false);
    };
    let workspace_path = with_database_path(move |database_path| {
        crate::storage::services::workspace_directories::get_workspace_directory_path(
            &database_path,
            &project_id,
        )
    })
    .await?;
    let Some(workspace_path) = workspace_path else {
        return Ok(false);
    };

    if is_ssh_path(&workspace_path) {
        return Ok(is_allowed_remote_plan_write(&workspace_path, file_path));
    }

    let workspace_path = PathBuf::from(workspace_path);
    let requested_path = PathBuf::from(file_path);
    tokio::task::spawn_blocking(move || {
        is_allowed_local_plan_write(&workspace_path, &requested_path)
    })
    .await
    .map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to validate Plan Mode write path: {error}"),
        )
    })
}

fn is_allowed_local_plan_write(workspace_path: &Path, requested_path: &Path) -> bool {
    let Some(workspace_path) = lexical_normalize_path(workspace_path) else {
        return false;
    };
    if !workspace_path.is_absolute() {
        return false;
    }

    let candidate_path = if requested_path.is_absolute() {
        lexical_normalize_path(requested_path)
    } else {
        lexical_normalize_path(&workspace_path.join(requested_path))
    };
    let Some(candidate_path) = candidate_path else {
        return false;
    };

    PLAN_WRITE_DIRECTORIES.iter().any(|segments| {
        let allowed_root = workspace_path.join(segments[0]).join(segments[1]);
        path_is_descendant(&candidate_path, &allowed_root)
            && !path_contains_symlink(&workspace_path, &candidate_path)
    })
}

fn lexical_normalize_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut normal_depth = 0usize;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth == 0 || !normalized.pop() {
                    return None;
                }
                normal_depth -= 1;
            }
            Component::Normal(segment) => {
                normalized.push(segment);
                normal_depth += 1;
            }
        }
    }

    Some(normalized)
}

fn path_is_descendant(candidate_path: &Path, root_path: &Path) -> bool {
    let candidate_components = candidate_path.components().collect::<Vec<_>>();
    let root_components = root_path.components().collect::<Vec<_>>();
    candidate_components.len() > root_components.len()
        && root_components
            .iter()
            .zip(candidate_components.iter())
            .all(|(root, candidate)| local_component_eq(*root, *candidate))
}

fn local_component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    if cfg!(windows) {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    } else {
        left == right
    }
}

fn path_contains_symlink(workspace_path: &Path, candidate_path: &Path) -> bool {
    let workspace_depth = workspace_path.components().count();
    let mut current_path = workspace_path.to_path_buf();

    for component in candidate_path.components().skip(workspace_depth) {
        current_path.push(component.as_os_str());
        match std::fs::symlink_metadata(&current_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }

    false
}

fn is_allowed_remote_plan_write(workspace_path: &str, requested_path: &str) -> bool {
    let resolved_path =
        resolve_remote_workspace_path(workspace_path, &requested_path.trim().replace('\\', "/"));
    let Some((workspace_authority, workspace_segments)) = normalize_ssh_path(workspace_path) else {
        return false;
    };
    let Some((candidate_authority, candidate_segments)) = normalize_ssh_path(&resolved_path) else {
        return false;
    };
    if workspace_authority != candidate_authority
        || !remote_segments_start_with(&candidate_segments, &workspace_segments)
    {
        return false;
    }

    let relative_segments = &candidate_segments[workspace_segments.len()..];
    PLAN_WRITE_DIRECTORIES.iter().any(|segments| {
        relative_segments.len() > segments.len()
            && relative_segments[0] == segments[0]
            && relative_segments[1] == segments[1]
    })
}

pub(crate) fn normalize_ssh_path(path: &str) -> Option<(String, Vec<String>)> {
    let normalized = path.trim().replace('\\', "/");
    let remainder = normalized.strip_prefix("ssh://")?;
    let (authority, raw_path) = remainder.split_once('/').unwrap_or((remainder, ""));
    if authority.is_empty() {
        return None;
    }

    let mut segments = Vec::new();
    for segment in raw_path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            value => segments.push(value.to_string()),
        }
    }

    Some((authority.to_string(), segments))
}

pub(crate) fn remote_segments_start_with(candidate: &[String], root: &[String]) -> bool {
    candidate.len() >= root.len()
        && root
            .iter()
            .zip(candidate.iter())
            .all(|(root_segment, candidate_segment)| root_segment == candidate_segment)
}
