use std::fs;
use std::path::{Path, PathBuf};

use napi::{Error, Status};
use napi_derive::napi;
use serde::{Deserialize, Serialize};

mod github;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Parsed GitHub URL information used to download a repository archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParsedGitHubUrl {
    /// GitHub owner / org name, e.g. "MayDay-wpf"
    pub owner: String,
    /// Repository name, e.g. "snow-cli"
    pub repo: String,
    /// Branch/tag/commit. When omitted the default branch is used.
    pub r#ref: Option<String>,
    /// Optional sub-directory inside the repository that should be treated as
    /// the skill root (the directory containing `SKILL.md`).
    pub sub_dir: Option<String>,
}

/// Metadata persisted for every skill installed from GitHub so that it can be
/// updated or removed later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledSkillRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub location: String,
    pub source_url: String,
    pub github: ParsedGitHubUrl,
    pub installed_at: String,
    pub commit_sha: Option<String>,
}

#[napi(object)]
pub struct SkillInstallResult {
    pub success: bool,
    pub skill_id: String,
    pub path: String,
    pub installed_at: String,
    pub commit_sha: Option<String>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct SkillBatchInstallResult {
    pub success: bool,
    pub results: Vec<SkillInstallResult>,
    pub installed_count: i64,
    pub total_count: i64,
    pub commit_sha: Option<String>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct GithubSkillRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub location: String,
    pub source_url: String,
    pub installed_at: String,
    pub commit_sha: Option<String>,
}

#[napi(object)]
pub struct SkillUninstallResult {
    pub success: bool,
    pub skill_id: String,
    pub message: String,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

/// Parse a GitHub URL into its components.
///
/// Supported formats:
///  - https://github.com/owner/repo
///  - https://github.com/owner/repo/tree/branch
///  - https://github.com/owner/repo/tree/branch/sub/dir
///  - https://github.com/owner/repo.git
///  - owner/repo
///  - owner/repo@branch
///  - owner/repo@branch:sub/dir
pub fn parse_github_url(input: &str) -> Option<ParsedGitHubUrl> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut working = trimmed.to_string();
    // Strip trailing slash / .git
    if working.ends_with(".git") {
        working = working[..working.len() - 4].to_string();
    }
    while working.ends_with('/') {
        working.pop();
    }

    // Full https / http URL
    // https://github.com/owner/repo/tree/branch/sub/dir
    let url_re =
        regex::Regex::new(r"(?i)^https?://github\.com/([^/]+)/([^/]+)(?:/tree/([^/]+)(/.*)?)?$")
            .ok()?;
    if let Some(caps) = url_re.captures(&working) {
        let owner = caps.get(1)?.as_str().to_string();
        let repo = caps.get(2)?.as_str().to_string();
        let r#ref = caps.get(3).map(|m| m.as_str().to_string());
        let sub_dir = caps.get(4).map(|m| {
            let s = m.as_str();
            // strip leading/trailing slashes
            let s = s.trim_start_matches('/').trim_end_matches('/');
            s.to_string()
        });
        return Some(ParsedGitHubUrl {
            owner,
            repo,
            r#ref: r#ref.filter(|r| !r.is_empty()),
            sub_dir: sub_dir.filter(|s| !s.is_empty()),
        });
    }

    // Shorthand: owner/repo  or  owner/repo@ref  or  owner/repo@ref:sub/dir
    let shorthand_re = regex::Regex::new(r"^([^/\s@]+)/([^/\s@]+)(?:@([^:]+))?(?::(.+))?$").ok()?;
    if let Some(caps) = shorthand_re.captures(&working) {
        let owner = caps.get(1)?.as_str().to_string();
        let repo = caps.get(2)?.as_str().to_string();
        let r#ref = caps.get(3).map(|m| m.as_str().to_string());
        let sub_dir = caps.get(4).map(|m| m.as_str().to_string());
        return Some(ParsedGitHubUrl {
            owner,
            repo,
            r#ref: r#ref.filter(|r| !r.is_empty()),
            sub_dir: sub_dir.filter(|s| !s.is_empty()),
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Registry (installed skills metadata)
// ---------------------------------------------------------------------------

fn get_registry_path() -> PathBuf {
    let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".snow").join("skills-registry.json")
}

fn load_installed_skills() -> Vec<InstalledSkillRecord> {
    let registry_path = get_registry_path();
    let Ok(content) = fs::read_to_string(&registry_path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<InstalledSkillRecord>>(&content) {
        Ok(records) => records,
        Err(_) => Vec::new(),
    }
}

fn save_installed_skills_at(
    records: &[InstalledSkillRecord],
    registry_path: &Path,
) -> napi::Result<()> {
    if let Some(parent) = registry_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to create registry directory: {e}"),
            )
        })?;
    }
    let json = serde_json::to_string_pretty(records).map_err(|e| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to serialize skill registry: {e}"),
        )
    })?;
    let parent = registry_path.parent().ok_or_else(|| {
        Error::new(
            Status::GenericFailure,
            format!("Registry path has no parent: {}", registry_path.display()),
        )
    })?;
    let file_name = registry_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skills-registry.json");
    let temporary = parent.join(format!(".{file_name}.snow-stage-{}", uuid::Uuid::new_v4()));
    let backup = parent.join(format!(
        ".{file_name}.snow-previous-{}",
        uuid::Uuid::new_v4()
    ));
    if let Err(error) = fs::write(&temporary, json) {
        let _ = fs::remove_file(&temporary);
        return Err(Error::new(
            Status::GenericFailure,
            format!("Failed to write staged skill registry: {error}"),
        ));
    }

    let had_previous = registry_path.exists();
    if had_previous {
        fs::rename(registry_path, &backup).map_err(|e| {
            let _ = fs::remove_file(&temporary);
            Error::new(
                Status::GenericFailure,
                format!("Failed to preserve existing skill registry: {e}"),
            )
        })?;
    }
    if let Err(error) = fs::rename(&temporary, registry_path) {
        if had_previous {
            if let Err(restore_error) = fs::rename(&backup, registry_path) {
                return Err(Error::new(
                    Status::GenericFailure,
                    format!(
                        "Failed to replace skill registry: {error}. Automatic restoration failed: {restore_error}. Recovery data was kept at {}",
                        parent.display()
                    ),
                ));
            }
        }
        let _ = fs::remove_file(&temporary);
        return Err(Error::new(
            Status::GenericFailure,
            format!("Failed to replace skill registry: {error}"),
        ));
    }
    if had_previous {
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

fn save_installed_skills(records: &[InstalledSkillRecord]) -> napi::Result<()> {
    save_installed_skills_at(records, &get_registry_path())
}

fn upsert_record_at(record: InstalledSkillRecord, registry_path: &Path) -> napi::Result<()> {
    let mut records = if registry_path.exists() {
        let content = fs::read_to_string(registry_path).map_err(|e| {
            Error::new(
                Status::GenericFailure,
                format!("Failed to read skill registry: {e}"),
            )
        })?;
        serde_json::from_str::<Vec<InstalledSkillRecord>>(&content).unwrap_or_default()
    } else {
        Vec::new()
    };
    let idx = records
        .iter()
        .position(|r| r.id == record.id && r.location == record.location);
    match idx {
        Some(i) => {
            records[i] = record;
        }
        None => records.push(record),
    }
    save_installed_skills_at(&records, registry_path)
}

fn remove_record(skill_id: &str, location: &str) -> napi::Result<()> {
    let records = load_installed_skills();
    let filtered: Vec<InstalledSkillRecord> = records
        .into_iter()
        .filter(|r| !(r.id == skill_id && r.location == location))
        .collect();
    save_installed_skills(&filtered)
}

// ---------------------------------------------------------------------------
// Skill directory helpers
// ---------------------------------------------------------------------------

fn get_skill_directory(skill_id: &str, location: &str, project_root: Option<&Path>) -> PathBuf {
    let segments: Vec<String> = skill_id
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    match location {
        "project" => {
            let root = project_root
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let mut path = root.join(".snow").join("skills");
            for seg in &segments {
                path.push(seg);
            }
            path
        }
        _ => {
            let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let mut path = home.join(".snow").join("skills");
            for seg in &segments {
                path.push(seg);
            }
            path
        }
    }
}

// ---------------------------------------------------------------------------
// Public napi API
// ---------------------------------------------------------------------------

/// Install (or re-install) skill(s) from a GitHub URL.
#[napi]
pub async fn install_skill_from_github(
    url: String,
    location: String,
    project_id: Option<String>,
) -> napi::Result<SkillBatchInstallResult> {
    let project_root = match project_id.as_deref() {
        Some(pid) => resolve_project_root(pid)?,
        None => None,
    };
    let location = location;
    tokio::task::spawn_blocking(move || {
        github::install_skill_from_github_blocking(url, location, project_root)
    })
    .await
    .map_err(|e| {
        Error::new(
            Status::GenericFailure,
            format!("Install skill task failed: {e}"),
        )
    })?
}

/// Uninstall a skill that was installed from GitHub.
#[napi]
pub async fn uninstall_github_skill(
    skill_id: String,
    project_id: Option<String>,
) -> napi::Result<SkillUninstallResult> {
    let skill_id_inner = skill_id.clone();
    let project_root = match project_id.as_deref() {
        Some(pid) => resolve_project_root(pid)?,
        None => None,
    };
    tokio::task::spawn_blocking(move || {
        let records = load_installed_skills();
        let record = records.into_iter().find(|r| r.id == skill_id_inner);
        let Some(record) = record else {
            return Ok(SkillUninstallResult {
                success: false,
                skill_id: skill_id_inner.clone(),
                message: format!("Skill \"{skill_id_inner}\" is not installed from GitHub"),
                error: None,
            });
        };

        let skill_dir = get_skill_directory(&record.id, &record.location, project_root.as_deref());
        github::remove_dir_if_exists(&skill_dir)?;
        remove_record(&record.id, &record.location)?;

        Ok(SkillUninstallResult {
            success: true,
            skill_id: record.id.clone(),
            message: format!("Skill \"{}\" uninstalled", record.id),
            error: None,
        })
    })
    .await
    .map_err(|e| {
        Error::new(
            Status::GenericFailure,
            format!("Uninstall skill task failed: {e}"),
        )
    })?
}

/// List all skills installed from GitHub.
#[napi]
pub async fn list_github_skills() -> napi::Result<Vec<GithubSkillRecord>> {
    tokio::task::spawn_blocking(|| {
        let records = load_installed_skills();
        Ok(records
            .into_iter()
            .map(|r| GithubSkillRecord {
                id: r.id,
                name: r.name,
                description: r.description,
                location: r.location,
                source_url: r.source_url,
                installed_at: r.installed_at,
                commit_sha: r.commit_sha,
            })
            .collect())
    })
    .await
    .map_err(|e| {
        Error::new(
            Status::GenericFailure,
            format!("List github skills task failed: {e}"),
        )
    })?
}

/// Resolve a project's workspace directory path from its id.
fn resolve_project_root(project_id: &str) -> napi::Result<Option<PathBuf>> {
    let storage_info = crate::storage::initialize_app_storage()?;
    let database_path = PathBuf::from(storage_info.database_path);
    let project_path =
        crate::storage::services::workspace_directories::get_workspace_directory_path(
            &database_path,
            project_id,
        )?;
    Ok(project_path.map(PathBuf::from))
}
