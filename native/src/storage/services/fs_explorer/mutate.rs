use std::fs;
use std::path::{Path, PathBuf};

use napi::bindgen_prelude::*;

fn resolve_workspace_entry(root_path: &str, entry_path: &str) -> Result<(PathBuf, PathBuf)> {
    let root = fs::canonicalize(root_path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to resolve workspace root '{}': {}",
            root_path, error
        ))
    })?;
    let entry = fs::canonicalize(entry_path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to resolve workspace entry '{}': {}",
            entry_path, error
        ))
    })?;

    if entry == root || !entry.starts_with(&root) {
        return Err(Error::from_reason(
            "Workspace entry is outside the workspace root",
        ));
    }

    Ok((root, entry))
}

fn validate_entry_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    let is_single_normal_component = matches!(
        Path::new(trimmed).components().next(),
        Some(std::path::Component::Normal(_))
    ) && Path::new(trimmed).components().count() == 1;

    if !is_single_normal_component {
        return Err(Error::from_reason(
            "Entry name must be a single file or directory name",
        ));
    }

    Ok(trimmed)
}

pub fn rename_workspace_entry(root_path: &str, entry_path: &str, new_name: &str) -> Result<()> {
    let (_root, entry) = resolve_workspace_entry(root_path, entry_path)?;
    let name = validate_entry_name(new_name)?;
    let parent = entry
        .parent()
        .ok_or_else(|| Error::from_reason("Workspace entry does not have a parent directory"))?;
    let destination = parent.join(name);

    if destination.exists() {
        return Err(Error::from_reason(format!(
            "A workspace entry named '{}' already exists",
            name
        )));
    }

    fs::rename(&entry, &destination).map_err(|error| {
        Error::from_reason(format!(
            "Failed to rename workspace entry '{}': {}",
            entry.display(),
            error
        ))
    })
}

pub fn delete_workspace_entry(root_path: &str, entry_path: &str) -> Result<()> {
    let (_root, entry) = resolve_workspace_entry(root_path, entry_path)?;
    let result = if entry.is_dir() {
        fs::remove_dir_all(&entry)
    } else {
        fs::remove_file(&entry)
    };

    result.map_err(|error| {
        Error::from_reason(format!(
            "Failed to delete workspace entry '{}': {}",
            entry.display(),
            error
        ))
    })
}
