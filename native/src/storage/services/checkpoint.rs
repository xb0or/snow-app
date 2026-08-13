use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::time::{SystemTime, UNIX_EPOCH};

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use super::checkpoint_skip::should_skip_pending_copy;
use super::gitignore::GitignoreMatcher;

const OBJECT_DIR_NAME: &str = "objects";
const PENDING_DIR_NAME: &str = "pending";
const MANIFEST_VERSION: u32 = 2;

/// Prefix marking a manifest entry path as an absolute path outside the
/// checkpoint's working directory. Entries whose path starts with this marker
/// store the full absolute filesystem path (after the marker) instead of a
/// path relative to `work_dir`. This lets the checkpoint system record and
/// restore files edited outside the project workspace (e.g. `~/.snow/settings.json`).
const ABSOLUTE_PATH_MARKER: &str = "\x00abs:";

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "out",
    "coverage",
    ".cache",
    ".turbo",
    ".vercel",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".vscode",
    ".vs",
    ".snow",
    ".snowapp",
    "release",
    ".output",
    ".angular",
    ".parcel-cache",
];

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 工作目录读写锁表：常规捕获与 diff 查询持有共享读锁，仅回滚持有
/// 独占写锁。同项目多个会话可并行捕获和展示文件变更，回滚仍与这些操作
/// 互斥。Weak 让长期不再使用的目录锁可自动回收。
static CHECKPOINT_WORK_DIR_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>> =
    OnceLock::new();

/// manifest 级锁表：每个 checkpoint 独立串行 read-modify-write。
/// 同项目的不同会话拥有不同 checkpoint，因此文件编辑仅锁自己的
/// manifest，不再锁住整个工作目录。Weak 避免删除会话后残留锁对象。
static CHECKPOINT_MANIFEST_LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    OnceLock::new();

/// 进程内 diff 缓存上限：超过后整体清空（LRU 之外的简单防膨胀手段，
/// diff 成本远低于全量重算，清空后逐次重建即可）。
const DIFF_CACHE_MAX_ENTRIES: usize = 2048;

struct CachedCheckpointDiff {
    /// original 状态摘要（object_id / git head+path / missing），作为失效依据之一
    original_digest: String,
    current_mtime_ms: u64,
    current_size: u64,
    content: String,
    is_binary: bool,
}

/// 进程内 diff 缓存：key = "{checkpoint_id}:{path}"。
/// 命中条件：original 摘要一致 + 磁盘文件 mtime/size 未变。
/// 工具高频循环下，list_checkpoint_diffs 对未变化文件直接复用已生成的
/// unified diff，避免反复读文件 + TextDiff 全量计算（P0-4 性能优化）。
static DIFF_CACHE: OnceLock<Mutex<HashMap<String, CachedCheckpointDiff>>> = OnceLock::new();

fn diff_cache() -> MutexGuard<'static, HashMap<String, CachedCheckpointDiff>> {
    DIFF_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn original_digest(original: &OriginalState, git: Option<&GitBaseline>, path: &str) -> String {
    match original {
        OriginalState::Missing => "missing".to_string(),
        OriginalState::Object { object_id } => format!("obj:{object_id}"),
        OriginalState::Git => format!(
            "git:{}:{path}",
            git.map(|baseline| baseline.head.as_str()).unwrap_or("?")
        ),
    }
}

#[derive(Serialize, Deserialize)]
struct CheckpointManifest {
    version: u32,
    work_dir: String,
    git: Option<GitBaseline>,
    entries: Vec<CheckpointEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GitBaseline {
    repository_root: String,
    work_dir_prefix: String,
    head: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CheckpointEntry {
    path: String,
    original: OriginalState,
    #[serde(default)]
    expected: Option<OriginalState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OriginalState {
    Missing,
    Object { object_id: String },
    Git,
}
struct PendingFileState {
    /// Snapshot copy of an untracked file (its only pre-command content
    /// source). `None` for git-tracked files or skipped snapshots (see `skipped`).
    snapshot: Option<PathBuf>,
    /// Snapshot skipped (too large / binary ext, COW unavailable): change
    /// cannot be recovered, after-pass skips it.
    skipped: bool,
    /// Pre-command mtime (ms) and size used as a cheap first-pass change
    /// detector for tracked files; a match skips the content read entirely.
    mtime_ms: u64,
    size: u64,
    /// Whether the file was tracked by git when the capture was taken.
    tracked: bool,
}

pub struct CheckpointWorktreeCapture {
    checkpoint_ids: Vec<String>,
    work_dir: String,
    /// Git baseline when the work dir is inside a repository. `Some` selects
    /// the git-driven capture path: the before/after passes run `git diff` /
    /// `git ls-files --others` instead of walking the whole worktree, and only
    /// dirty tracked + untracked files are snapshotted. `None` (non-git work
    /// dir or git failure) falls back to the legacy full-traversal copy path.
    baseline: Option<GitBaseline>,
    /// Single shared path set for all checkpoints in this capture. All
    /// checkpoints are validated against the same `work_dir` during capture,
    /// so one result serves every checkpoint. On the git-driven path this is
    /// the pre-command dirty tracked + untracked set (everything snapshotted);
    /// on the legacy path it is the full worktree file set.
    before_paths: HashSet<String>,
    before_states: HashMap<String, PendingFileState>,
    pending_dir: PathBuf,
}

impl Drop for CheckpointWorktreeCapture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.pending_dir);
    }
}
fn checkpoint_root() -> Result<PathBuf> {
    super::storage_locations::checkpoint_root()
}

fn work_dir_lock(work_dir: &Path) -> Result<Arc<RwLock<()>>> {
    let locks = CHECKPOINT_WORK_DIR_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint work directory lock registry is poisoned"))?;
    if let Some(lock) = locks.get(work_dir).and_then(Weak::upgrade) {
        return Ok(lock);
    }

    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(RwLock::new(()));
    locks.insert(work_dir.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn work_dir_read_guard(lock: &RwLock<()>) -> Result<RwLockReadGuard<'_, ()>> {
    lock.read()
        .map_err(|_| Error::from_reason("Checkpoint work directory lock is poisoned"))
}

fn work_dir_write_guard(lock: &RwLock<()>) -> Result<RwLockWriteGuard<'_, ()>> {
    lock.write()
        .map_err(|_| Error::from_reason("Checkpoint work directory lock is poisoned"))
}

fn manifest_lock(checkpoint_id: &str) -> Result<Arc<Mutex<()>>> {
    let locks = CHECKPOINT_MANIFEST_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock registry is poisoned"))?;
    if let Some(lock) = locks.get(checkpoint_id).and_then(Weak::upgrade) {
        return Ok(lock);
    }

    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    locks.insert(checkpoint_id.to_string(), Arc::downgrade(&lock));
    Ok(lock)
}

fn with_manifest_lock<T>(
    checkpoint_id: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock = manifest_lock(checkpoint_id)?;
    let _guard = lock
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
    operation()
}

fn should_skip_relative(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name
            .to_str()
            .map(|value| SKIP_DIRS.contains(&value))
            .unwrap_or(false),
        _ => false,
    })
}

fn generate_checkpoint_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cp-{}-{}-{}", now.as_secs(), now.subsec_nanos(), count)
}

fn to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn from_forward_slashes(relative: &str) -> PathBuf {
    PathBuf::from(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string()))
}

fn canonical_work_dir(work_dir: &str) -> Result<PathBuf> {
    let root = Path::new(work_dir);
    if !root.exists() {
        return Err(Error::from_reason(format!(
            "Working directory does not exist: {work_dir}"
        )));
    }
    if !root.is_dir() {
        return Err(Error::from_reason(format!(
            "Path is not a directory: {work_dir}"
        )));
    }
    fs::canonicalize(root).map_err(|error| {
        Error::from_reason(format!(
            "Failed to resolve working directory '{}': {error}",
            root.display()
        ))
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Strip Windows extended-length path prefixes so absolute and canonical paths
/// can be compared consistently.
///
/// `fs::canonicalize` on Windows returns paths like `\\?\D:\repo` or
/// `\\?\UNC\server\share`. Logical absolute paths from the AI / UI usually do
/// not include this prefix, so `starts_with` would otherwise reject in-workspace
/// absolute paths (especially for files that do not exist yet).
fn strip_windows_extended_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

fn path_key(path: &Path) -> String {
    let stripped = strip_windows_extended_prefix(path);
    let mut key = stripped.to_string_lossy().replace('\\', "/");
    while key.ends_with('/') && key.len() > 1 {
        key.pop();
    }
    #[cfg(windows)]
    {
        key = key.to_ascii_lowercase();
    }
    key
}

fn is_path_within_root(path: &Path, root: &Path) -> bool {
    let candidate_key = path_key(path);
    let base_key = path_key(root);
    candidate_key == base_key || candidate_key.starts_with(&format!("{base_key}/"))
}

/// Resolve a path that may not exist yet while preserving the same Windows
/// extended-path form as `fs::canonicalize` on the parent directory.
fn resolve_path_for_checkpoint(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|error| {
            Error::from_reason(format!(
                "Failed to resolve checkpoint path '{}': {error}",
                path.display()
            ))
        });
    }

    let normalized = normalize_path(path);
    if let Some(parent) = normalized.parent() {
        if !parent.as_os_str().is_empty() && parent.exists() {
            let parent_canonical = fs::canonicalize(parent).map_err(|error| {
                Error::from_reason(format!(
                    "Failed to resolve checkpoint path parent '{}': {error}",
                    parent.display()
                ))
            })?;
            if let Some(file_name) = normalized.file_name() {
                return Ok(parent_canonical.join(file_name));
            }
        }
    }

    Ok(strip_windows_extended_prefix(&normalized))
}

fn resolve_checkpoint_path(root: &Path, file_path: &str) -> Result<(PathBuf, String)> {
    let supplied = Path::new(file_path);
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        // Join relative paths against the logical root so Windows extended
        // prefixes do not leak into intermediate path components.
        strip_windows_extended_prefix(root).join(supplied)
    };
    let normalized = resolve_path_for_checkpoint(&candidate)?;

    if !is_path_within_root(&normalized, root) {
        // File is outside the checkpoint's working directory (e.g. editing
        // `~/.snow/settings.json`). Store it as an absolute-path-marked entry
        // so the checkpoint can still record and restore it on rollback.
        let abs_key = to_forward_slashes(&strip_windows_extended_prefix(&normalized));
        let marked = format!("{ABSOLUTE_PATH_MARKER}{abs_key}");
        return Ok((normalized, marked));
    }

    let relative = {
        let path_key_value = path_key(&normalized);
        let root_key_value = path_key(root);
        if path_key_value == root_key_value {
            String::new()
        } else {
            let relative_key = path_key_value
                .strip_prefix(&format!("{root_key_value}/"))
                .ok_or_else(|| Error::from_reason("Failed to create checkpoint-relative path"))?;
            relative_key.to_string()
        }
    };
    Ok((normalized, relative))
}

/// Resolve a manifest entry path back to an absolute filesystem path.
///
/// Paths stored with the `ABSOLUTE_PATH_MARKER` prefix are outside-workspace
/// absolute paths and are returned as-is (after stripping the marker).
/// All other paths are treated as relative to `root` and joined accordingly.
fn resolve_manifest_path(root: &Path, manifest_path: &str) -> PathBuf {
    if let Some(abs_path) = manifest_path.strip_prefix(ABSOLUTE_PATH_MARKER) {
        from_forward_slashes(abs_path)
    } else {
        root.join(from_forward_slashes(manifest_path))
    }
}

/// Check whether a manifest entry path should be skipped (e.g. it falls inside
/// a `node_modules` or `.git` directory). Absolute-path-marked entries are
/// never skipped by this check — they represent files outside the workspace
/// that the user explicitly chose to edit.
fn should_skip_manifest_path(manifest_path: &str) -> bool {
    if manifest_path.starts_with(ABSOLUTE_PATH_MARKER) {
        return false;
    }
    should_skip_relative(Path::new(manifest_path))
}

fn checkpoint_dir(checkpoint_id: &str) -> Result<PathBuf> {
    Ok(checkpoint_root()?.join(checkpoint_id))
}
fn manifest_path(checkpoint_id: &str) -> Result<PathBuf> {
    Ok(checkpoint_dir(checkpoint_id)?.join("manifest.json"))
}

/// Check whether a checkpoint manifest file exists on disk.
fn checkpoint_manifest_exists(checkpoint_id: &str) -> bool {
    match manifest_path(checkpoint_id) {
        Ok(path) => path.is_file(),
        Err(_) => false,
    }
}

/// Filter out checkpoint IDs whose manifest no longer exists on disk.
///
/// When a conversation is resumed from history, the frontend reconstructs the
/// `checkpoint_ids` list from persisted message records. Some of those
/// checkpoints may have been deleted (by rollback, compaction cleanup, or
/// new-chat pruning), leaving dangling IDs that would cause `read_manifest`
/// to fail. This helper silently drops them so tool execution can proceed
/// against the still-valid checkpoints.
fn filter_existing_checkpoints(checkpoint_ids: Vec<String>) -> Vec<String> {
    checkpoint_ids
        .into_iter()
        .filter(|id| checkpoint_manifest_exists(id))
        .collect()
}

fn read_manifest(checkpoint_id: &str) -> Result<CheckpointManifest> {
    let path = manifest_path(checkpoint_id)?;
    let json = fs::read_to_string(&path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read checkpoint manifest '{}': {error}",
            path.display()
        ))
    })?;
    let manifest: CheckpointManifest = serde_json::from_str(&json).map_err(|error| {
        Error::from_reason(format!(
            "Failed to parse checkpoint manifest '{}': {error}",
            path.display()
        ))
    })?;
    if manifest.version != MANIFEST_VERSION {
        return Err(Error::from_reason(format!(
            "Unsupported checkpoint format version: {}",
            manifest.version
        )));
    }
    Ok(manifest)
}

fn write_manifest(checkpoint_id: &str, manifest: &CheckpointManifest) -> Result<()> {
    let directory = checkpoint_dir(checkpoint_id)?;
    fs::create_dir_all(&directory).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint directory '{}': {error}",
            directory.display()
        ))
    })?;
    let json = serde_json::to_vec(manifest).map_err(|error| {
        Error::from_reason(format!("Failed to serialize checkpoint manifest: {error}"))
    })?;
    let temporary = directory.join(format!("manifest-{}.tmp", generate_checkpoint_id()));
    fs::write(&temporary, json).map_err(|error| {
        Error::from_reason(format!(
            "Failed to write checkpoint manifest '{}': {error}",
            temporary.display()
        ))
    })?;
    fs::rename(&temporary, directory.join("manifest.json")).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::from_reason(format!("Failed to publish checkpoint manifest: {error}"))
    })
}

fn run_git(work_dir: &Path, args: &[&str]) -> Result<Output> {
    let mut command = crate::utils::process::cmd("git");
    // `safe.directory=*` bypasses Git's dubious-ownership check
    // (CVE-2022-24765), so git works inside WSL (`\\wsl$\...`) and other
    // UNC/network paths where the repo is owned by a different user.
    command
        .args(["-c", "core.quotepath=false", "-c", "safe.directory=*"])
        .args(args)
        .current_dir(work_dir);

    command
        .output()
        .map_err(|error| Error::from_reason(format!("Failed to execute git: {error}")))
}

fn git_text(work_dir: &Path, args: &[&str]) -> Option<String> {
    let output = run_git(work_dir, args).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_git_baseline(work_dir: &Path) -> Option<GitBaseline> {
    let repository_root = git_text(work_dir, &["rev-parse", "--show-toplevel"])?;
    let head = git_text(work_dir, &["rev-parse", "HEAD"])?;
    let repository_root = fs::canonicalize(repository_root).ok()?;
    let prefix = work_dir.strip_prefix(&repository_root).ok()?;
    Some(GitBaseline {
        repository_root: repository_root.to_string_lossy().to_string(),
        work_dir_prefix: to_forward_slashes(prefix),
        head,
    })
}

fn checkpoint_git_ref(checkpoint_id: &str) -> String {
    format!("refs/snow/checkpoints/{checkpoint_id}")
}

fn update_checkpoint_git_ref(
    checkpoint_id: &str,
    baseline: &GitBaseline,
    delete: bool,
) -> Result<()> {
    let repository_root = Path::new(&baseline.repository_root);
    let reference = checkpoint_git_ref(checkpoint_id);
    let output = if delete {
        run_git(repository_root, &["update-ref", "-d", &reference])?
    } else {
        run_git(repository_root, &["update-ref", &reference, &baseline.head])?
    };
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::from_reason(format!(
            "Failed to update checkpoint Git reference: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn collect_worktree_file_paths(root: &Path) -> Result<HashSet<String>> {
    let mut matcher = GitignoreMatcher::from_project_root(root);
    let mut paths = HashSet::new();
    let mut directories = vec![root.to_path_buf()];

    while let Some(directory) = directories.pop() {
        // 进入子目录时加载该目录自己的 .gitignore（root 的规则已由
        // from_project_root 加载）。LIFO 遍历保证父目录规则先于子目录
        // 规则加入 matcher,与 git 的"深层规则覆盖浅层规则"语义一致;
        // 前缀化后的规则锚定到各自目录,不会误伤兄弟目录。
        if directory != root {
            let dir_relative = directory.strip_prefix(root).map_err(|error| {
                Error::from_reason(format!(
                    "Failed to resolve checkpoint-relative directory '{}': {error}",
                    directory.display()
                ))
            })?;
            matcher.load_directory_gitignore(&root, dir_relative);
        }

        let entries = fs::read_dir(&directory).map_err(|error| {
            Error::from_reason(format!(
                "Failed to scan checkpoint directory '{}': {error}",
                directory.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::from_reason(format!("Failed to read checkpoint entry: {error}"))
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|error| {
                Error::from_reason(format!(
                    "Failed to resolve checkpoint-relative path '{}': {error}",
                    path.display()
                ))
            })?;
            if should_skip_relative(relative) {
                continue;
            }

            let file_type = entry.file_type().map_err(|error| {
                Error::from_reason(format!(
                    "Failed to inspect checkpoint path '{}': {error}",
                    path.display()
                ))
            })?;
            if file_type.is_symlink() {
                continue;
            }

            let relative_path = to_forward_slashes(relative);
            if matcher.is_ignored(&relative_path, file_type.is_dir()) {
                continue;
            }

            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file() {
                paths.insert(relative_path);
            }
        }
    }

    Ok(paths)
}

fn git_object_spec(baseline: &GitBaseline, relative: &str) -> String {
    let repository_path = if baseline.work_dir_prefix.is_empty() {
        relative.to_string()
    } else {
        format!(
            "{}/{}",
            baseline.work_dir_prefix.trim_end_matches('/'),
            relative
        )
    };
    format!("{}:{}", baseline.head, repository_path)
}

fn read_git_object(baseline: &GitBaseline, relative: &str) -> Result<Option<Vec<u8>>> {
    let repository_root = Path::new(&baseline.repository_root);
    let object_spec = git_object_spec(baseline, relative);
    let output = run_git(repository_root, &["show", &object_spec])?;
    if output.status.success() {
        Ok(Some(output.stdout))
    } else {
        Ok(None)
    }
}

fn store_object(path: &Path) -> Result<String> {
    let object_dir = checkpoint_root()?.join(OBJECT_DIR_NAME);
    fs::create_dir_all(&object_dir).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object directory: {error}"
        ))
    })?;
    let temporary = object_dir.join(format!("{}.tmp", generate_checkpoint_id()));
    let mut source = File::open(path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read checkpoint source '{}': {error}",
            path.display()
        ))
    })?;
    let mut destination = File::create(&temporary).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object '{}': {error}",
            temporary.display()
        ))
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|error| {
            Error::from_reason(format!("Failed to read checkpoint source: {error}"))
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| {
            Error::from_reason(format!("Failed to write checkpoint object: {error}"))
        })?;
    }
    destination.flush().map_err(|error| {
        Error::from_reason(format!("Failed to flush checkpoint object: {error}"))
    })?;

    let object_id = hasher.finalize().to_hex().to_string();
    let final_path = object_dir.join(&object_id);
    if final_path.exists() {
        let _ = fs::remove_file(&temporary);
    } else if let Err(error) = fs::rename(&temporary, &final_path) {
        // Another session may have published the same content-addressed object
        // after our exists check. Treat that as a successful deduplicated write.
        if final_path.exists() {
            let _ = fs::remove_file(&temporary);
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(Error::from_reason(format!(
                "Failed to publish checkpoint object: {error}"
            )));
        }
    }
    Ok(object_id)
}

fn current_state(path: &Path) -> Result<OriginalState> {
    if !path.exists() {
        return Ok(OriginalState::Missing);
    }
    if !path.is_file() {
        return Err(Error::from_reason(format!(
            "Checkpoint path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(OriginalState::Object {
        object_id: store_object(path)?,
    })
}

fn states_match(
    current: &Path,
    expected: &OriginalState,
    baseline: Option<&GitBaseline>,
    relative: &str,
) -> Result<bool> {
    Ok(classify_change(current, expected, baseline, relative)?.is_none())
}

fn update_expected_state(
    manifest: &mut CheckpointManifest,
    absolute: &Path,
    path: &str,
) -> Result<bool> {
    let Some(entry) = manifest.entries.iter_mut().find(|entry| entry.path == path) else {
        return Ok(false);
    };
    entry.expected = Some(current_state(absolute)?);
    Ok(true)
}

fn capture_entry(
    manifest: &mut CheckpointManifest,
    absolute: &Path,
    relative: &Path,
    original: OriginalState,
) -> Result<()> {
    if relative.as_os_str().is_empty() || should_skip_relative(relative) {
        return Ok(());
    }
    let path = to_forward_slashes(relative);
    let expected = current_state(absolute)?;
    if let Some(entry) = manifest.entries.iter_mut().find(|entry| entry.path == path) {
        entry.expected = Some(expected);
        return Ok(());
    }

    manifest.entries.push(CheckpointEntry {
        path,
        original,
        expected: Some(expected),
    });
    Ok(())
}

fn validate_manifest_work_dir(manifest: &CheckpointManifest, work_dir: &str) -> Result<PathBuf> {
    let requested = canonical_work_dir(work_dir)?;
    let recorded = PathBuf::from(&manifest.work_dir);
    if requested != recorded {
        return Err(Error::from_reason(format!(
            "Checkpoint belongs to '{}', not '{}'",
            recorded.display(),
            requested.display()
        )));
    }
    Ok(requested)
}

/// 捕获阶段的目录校验(工具执行前/后):checkpoint 属于其他目录时返回
/// None,调用方跳过该 checkpoint 并继续,绝不因目录不匹配拦截工具执行。
/// 回滚阶段仍由 validate_manifest_work_dir 严格校验。
fn validate_capture_work_dir(manifest: &CheckpointManifest, work_dir: &str) -> Option<PathBuf> {
    match validate_manifest_work_dir(manifest, work_dir) {
        Ok(root) => Some(root),
        Err(error) => {
            eprintln!("[checkpoint] {error}; skipping checkpoint capture");
            None
        }
    }
}

/// Create an incremental checkpoint without copying the working directory.
/// File content is captured lazily immediately before a tool first changes it.
/// Creation only publishes a new manifest and Git ref, so it does not take the
/// shared work-directory lock used by active tool captures.
pub fn create_checkpoint(work_dir: String) -> Result<String> {
    let root = canonical_work_dir(&work_dir)?;
    let checkpoint_id = generate_checkpoint_id();
    with_manifest_lock(&checkpoint_id, || {
        let manifest = CheckpointManifest {
            version: MANIFEST_VERSION,
            work_dir: root.to_string_lossy().to_string(),
            git: detect_git_baseline(&root),
            entries: Vec::new(),
        };

        write_manifest(&checkpoint_id, &manifest)?;
        if let Some(baseline) = manifest.git.as_ref() {
            if let Err(error) = update_checkpoint_git_ref(&checkpoint_id, baseline, false) {
                let _ = fs::remove_dir_all(checkpoint_dir(&checkpoint_id)?);
                return Err(error);
            }
        }
        Ok(checkpoint_id.clone())
    })
}

/// Capture the original state of one file before a filesystem tool changes it.
pub fn record_checkpoint_file(
    checkpoint_ids: Vec<String>,
    work_dir: String,
    file_path: String,
) -> Result<()> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(());
    }
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    let (absolute, path) = resolve_checkpoint_path(&root, &file_path)?;
    if path.is_empty() || should_skip_manifest_path(&path) {
        return Ok(());
    }

    for checkpoint_id in checkpoint_ids {
        with_manifest_lock(&checkpoint_id, || {
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(_root) = validate_capture_work_dir(&manifest, &work_dir) else {
                return Ok(());
            };
            if manifest.entries.iter().any(|entry| entry.path == path) {
                return Ok(());
            }
            manifest.entries.push(CheckpointEntry {
                path: path.clone(),
                original: current_state(&absolute)?,
                expected: None,
            });
            write_manifest(&checkpoint_id, &manifest)
        })?;
    }
    Ok(())
}

/// Record the state produced by a successful filesystem tool execution.
pub fn record_checkpoint_file_after(
    checkpoint_ids: Vec<String>,
    work_dir: String,
    file_path: String,
) -> Result<()> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(());
    }
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    let (absolute, path) = resolve_checkpoint_path(&root, &file_path)?;
    if path.is_empty() || should_skip_manifest_path(&path) {
        return Ok(());
    }

    for checkpoint_id in checkpoint_ids {
        with_manifest_lock(&checkpoint_id, || {
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(_root) = validate_capture_work_dir(&manifest, &work_dir) else {
                return Ok(());
            };
            if update_expected_state(&mut manifest, &absolute, &path)? {
                write_manifest(&checkpoint_id, &manifest)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// 快照文件到 pending，返回是否已建立。COW 克隆优先（APFS/reflink），失败
/// 回退普通复制；回退时大文件/二进制文件跳过，避免非 git 项目全量复制。
fn copy_pending_file(source: &Path, destination: &Path) -> Result<bool> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::from_reason(format!(
                "Failed to create pending checkpoint directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    if reflink::reflink(source, destination).is_ok() {
        return Ok(true);
    }
    if should_skip_pending_copy(source) {
        return Ok(false);
    }
    fs::copy(source, destination).map_err(|error| {
        Error::from_reason(format!(
            "Failed to capture pending checkpoint file '{}': {error}",
            source.display()
        ))
    })?;
    Ok(true)
}

fn pending_state_matches_current(state: &PendingFileState, current: &Path) -> bool {
    let Some(snapshot) = state.snapshot.as_ref() else {
        // No snapshot (git-tracked file) — content comparison is done against
        // the git object at capture time, not against a pending copy.
        return false;
    };
    // 快速路径：mtime+size 未变 → 未修改（工具写文件必更新 mtime），
    // 避免逐字节对比整个工作树；内容对比兜底。
    if let Ok(meta) = fs::metadata(current) {
        if meta.len() == state.size && mtime_ms(&meta) == state.mtime_ms {
            return true;
        }
    }
    current.is_file() && !files_are_different(current, snapshot)
}

fn pending_state_to_original(state: &PendingFileState) -> Result<OriginalState> {
    let snapshot = state.snapshot.as_ref().ok_or_else(|| {
        Error::from_reason("Cannot materialize an original from a git-tracked pending state")
    })?;
    Ok(OriginalState::Object {
        object_id: store_object(snapshot)?,
    })
}

/// Map repo-root-relative paths (NUL-separated git output, forward slashes)
/// to work-dir-relative paths. Entries outside the work dir are dropped.
fn repo_paths_to_work_relative(output: &[u8], prefix: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for name in output.split(|&byte| byte == 0) {
        if name.is_empty() {
            continue;
        }
        let name = String::from_utf8_lossy(name).replace('\\', "/");
        let name = name.trim_start_matches("./");
        if prefix.is_empty() {
            paths.push(name.to_string());
        } else if let Some(rest) = name.strip_prefix(&format!("{prefix}/")) {
            paths.push(rest.to_string());
        }
    }
    paths
}

/// Resolve the set of git-tracked files (work-dir-relative, forward-slash
/// separated) via `git ls-files`. Returns an empty set when the work dir is
/// not part of the repository (callers then fall back to copying everything).
fn tracked_file_set(baseline: &GitBaseline) -> Result<HashSet<String>> {
    let repository_root = Path::new(&baseline.repository_root);
    let output = run_git(repository_root, &["ls-files", "-z"])?;
    if !output.status.success() {
        return Ok(HashSet::new());
    }
    Ok(repo_paths_to_work_relative(&output.stdout, &baseline.work_dir_prefix)
        .into_iter()
        .collect())
}

/// Tracked files whose working-tree content differs from the pre-command
/// baseline commit (`git diff --name-only`). Because the diff is computed
/// against `baseline.head` — the commit captured when the checkpoint was
/// created — changes committed *during* the command still show up here, which
/// `git status` would silently hide.
fn git_diff_name_only(baseline: &GitBaseline) -> Result<Vec<String>> {
    let repository_root = Path::new(&baseline.repository_root);
    let output = run_git(
        repository_root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &baseline.head,
        ],
    )?;
    if !output.status.success() {
        return Err(Error::from_reason(format!(
            "Failed to list git diff for checkpoint baseline: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(repo_paths_to_work_relative(
        &output.stdout,
        &baseline.work_dir_prefix,
    ))
}

/// Untracked files (full gitignore rules incl. sub-directory `.gitignore` and
/// `.git/info/exclude` applied by git itself), work-dir-relative.
fn git_untracked_paths(baseline: &GitBaseline) -> Result<Vec<String>> {
    let repository_root = Path::new(&baseline.repository_root);
    let output = run_git(
        repository_root,
        &["ls-files", "-o", "--exclude-standard", "-z"],
    )?;
    if !output.status.success() {
        return Err(Error::from_reason(format!(
            "Failed to list untracked files for checkpoint baseline: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(repo_paths_to_work_relative(
        &output.stdout,
        &baseline.work_dir_prefix,
    ))
}

/// Reuse the git baseline stored in a valid checkpoint manifest (saves two
/// `git rev-parse` process spawns on every terminal command). The baseline is
/// only reused when the work dir still matches its repository root;
/// otherwise `None` lets the caller fall back to fresh detection.
///
/// `manifest_baseline` is collected from the first work-dir-matching
/// checkpoint during the validation loop in
/// `capture_checkpoint_worktree_before`; this helper validates it against the
/// canonical work dir.
fn reuse_manifest_git_baseline(baseline: &GitBaseline, root: &Path) -> bool {
    let repository_root = Path::new(&baseline.repository_root);
    root.strip_prefix(repository_root)
        .map(|prefix| to_forward_slashes(prefix) == baseline.work_dir_prefix)
        .unwrap_or(false)
}

/// Detect whether a git-tracked file changed while the command ran, and if so
/// return its original state.
///
/// Two-stage detection keeps the common path cheap:
/// 1. metadata filter — identical mtime + size means the file was untouched
///    and no content I/O happens at all;
/// 2. content confirmation — only files whose metadata changed are read
///    (git object vs current file), so a `touch` alone never records a
///    phantom change.
///
/// The original is `OriginalState::Git`: `GitBaseline.head` is a fixed commit
/// SHA captured when the checkpoint was created, so rollback and diff
/// generation recover the exact pre-command content even if the command
/// committed in the meantime.
fn tracked_file_change(
    manifest: &CheckpointManifest,
    relative_path: &str,
    current: &Path,
    state: &PendingFileState,
) -> Result<Option<OriginalState>> {
    let Ok(meta) = fs::metadata(current) else {
        // File was deleted by the command.
        return Ok(Some(OriginalState::Git));
    };
    if meta.len() == state.size && mtime_ms(&meta) == state.mtime_ms {
        return Ok(None);
    }
    tracked_file_content_change(manifest, relative_path, current)
}

/// Content-level change confirmation for a tracked file that was clean at
/// capture time (no snapshot): compare the current file against the git
/// baseline object. Used by the legacy fallback path (`tracked_file_change`).
fn tracked_file_content_change(
    manifest: &CheckpointManifest,
    relative_path: &str,
    current: &Path,
) -> Result<Option<OriginalState>> {
    let Ok(meta) = fs::metadata(current) else {
        // File was deleted by the command.
        return Ok(Some(OriginalState::Git));
    };
    if meta.is_dir() {
        // Directory-level change (e.g. a submodule pointer): not a regular
        // file, nothing to roll back at this granularity.
        return Ok(None);
    }
    let Some(baseline) = manifest.git.as_ref() else {
        // No git baseline (repository state changed between capture and
        // commit): cannot confirm, conservatively skip.
        return Ok(None);
    };
    let Some(content) = read_git_object(baseline, relative_path)? else {
        // Not present in the baseline (unexpected for a tracked file):
        // treat as added when the file exists.
        return Ok(current.is_file().then_some(OriginalState::Missing));
    };
    if file_differs_from_bytes(current, &content) {
        Ok(Some(OriginalState::Git))
    } else {
        Ok(None) // metadata changed but content identical (e.g. touch)
    }
}

/// Snapshot the current worktree into temporary storage before a terminal
/// command. No manifest entries are committed until the command ends.
///
/// Performance model (fixes the old copy-everything behaviour that serialized
/// concurrent terminal commands on one global lock):
/// - git-driven path (work dir inside a repository): zero worktree traversal,
///   zero full metadata scan. Two git commands (`git diff` against the
///   checkpoint baseline + `git ls-files --others`) yield exactly the files
///   whose pre-command content cannot be recovered from the git object
///   database: dirty tracked files and untracked files. Those are snapshotted;
///   every clean tracked file is left untouched (content recovered from the
///   baseline object at capture time). Dirty tracked files are snapshotted so
///   rollback restores the pre-command content — not the baseline commit,
///   which would silently drop edits made outside this conversation.
/// - legacy fallback (non-git work dir or git failure): full traversal, clean
///   tracked files are not copied, untracked files are copied.
pub fn capture_checkpoint_worktree_before(
    checkpoint_ids: Vec<String>,
    work_dir: String,
) -> Result<Option<CheckpointWorktreeCapture>> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(None);
    }
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;

    // 所有 checkpoint 都与当前目录不匹配:没有任何可捕获目标,
    // 不做无意义的全目录快照。顺带收集可复用的 git 基线
    // (checkpoint 创建时捕获,省两次 rev-parse 进程启动)。
    let mut matched_any = false;
    let mut manifest_baseline = None;
    for checkpoint_id in &checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock
            .lock()
            .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
        if !checkpoint_manifest_exists(checkpoint_id) {
            continue;
        }
        let manifest = read_manifest(checkpoint_id)?;
        if validate_capture_work_dir(&manifest, &work_dir).is_some() {
            matched_any = true;
            if manifest_baseline.is_none() {
                manifest_baseline = manifest.git.clone();
            }
        }
    }
    if !matched_any {
        return Ok(None);
    }

    let pending_dir = checkpoint_root()?
        .join(PENDING_DIR_NAME)
        .join(generate_checkpoint_id());

    // git 驱动路径:复用 manifest 基线(仅当 work_dir 仍位于其仓库根下),
    // 未命中时重新探测。git 命令失败(仓库被移动/删除等)回退旧逻辑,
    // 不阻塞工具执行。
    let baseline = manifest_baseline
        .filter(|baseline| reuse_manifest_git_baseline(baseline, &root))
        .or_else(|| detect_git_baseline(&root));
    if let Some(baseline) = baseline.as_ref() {
        match capture_worktree_before_git(baseline, &root, &pending_dir, &work_dir, &checkpoint_ids)
        {
            Ok(capture) => return Ok(Some(capture)),
            Err(error) => {
                eprintln!(
                    "[checkpoint] git-driven before-capture failed ({error}); falling back to traversal"
                );
            }
        }
    }

    // 非 git 回退:全量遍历,跟踪文件不复制内容(回滚时从 git 对象恢复),
    // 未跟踪文件复制到 pending(唯一内容来源)。tracked 集为空 → 全复制。
    let before_paths = collect_worktree_file_paths(&root)?;
    let tracked = baseline
        .as_ref()
        .map(tracked_file_set)
        .transpose()?
        .unwrap_or_default();

    let mut before_states = HashMap::new();
    for relative_path in &before_paths {
        let absolute = root.join(from_forward_slashes(relative_path));
        let meta = fs::metadata(&absolute).ok();
        let is_tracked = tracked.contains(relative_path);
        let snapshot = if is_tracked {
            None
        } else {
            let snapshot = pending_dir.join(from_forward_slashes(relative_path));
            let copied = match copy_pending_file(&absolute, &snapshot) {
                Ok(copied) => copied,
                Err(error) => {
                    // 文件在遍历后被删除:跳过该文件,不阻塞整个工具执行。
                    if !absolute.exists() {
                        continue;
                    }
                    return Err(error);
                }
            };
            if copied {
                Some(snapshot)
            } else {
                None
            }
        };
        let skipped = !is_tracked && snapshot.is_none();
        before_states.insert(
            relative_path.clone(),
            PendingFileState {
                snapshot,
                skipped,
                mtime_ms: meta.as_ref().map(mtime_ms).unwrap_or(0),
                size: meta.as_ref().map(|meta| meta.len()).unwrap_or(0),
                tracked: is_tracked,
            },
        );
    }

    Ok(Some(CheckpointWorktreeCapture {
        checkpoint_ids,
        work_dir,
        baseline: None,
        before_paths,
        before_states,
        pending_dir,
    }))
}

/// Git-driven before-capture. Snapshots only the files whose pre-command
/// content exists nowhere else: dirty tracked files (`git diff` against the
/// baseline commit) and untracked files (`git ls-files --others`). Both lists
/// come from git itself, so gitignore handling (sub-directory `.gitignore`,
/// `.git/info/exclude`) is authoritative and no worktree traversal or full
/// metadata scan is needed.
fn capture_worktree_before_git(
    baseline: &GitBaseline,
    root: &Path,
    pending_dir: &Path,
    work_dir: &str,
    checkpoint_ids: &[String],
) -> Result<CheckpointWorktreeCapture> {
    let dirty = git_diff_name_only(baseline)?;
    let untracked = git_untracked_paths(baseline)?;
    let dirty_set: HashSet<&String> = dirty.iter().collect();

    let mut before_paths = HashSet::new();
    let mut before_states = HashMap::new();
    for relative_path in dirty.iter().chain(untracked.iter()) {
        let absolute = root.join(from_forward_slashes(relative_path));
        let snapshot = pending_dir.join(from_forward_slashes(relative_path));
        let copied = match copy_pending_file(&absolute, &snapshot) {
            Ok(copied) => copied,
            Err(error) => {
                // 文件在列出后被删除:跳过该文件,不阻塞整个工具执行。
                if !absolute.exists() {
                    continue;
                }
                return Err(error);
            }
        };
        let snapshot = if copied { Some(snapshot) } else { None };
        let skipped = snapshot.is_none();
        before_paths.insert(relative_path.clone());
        before_states.insert(
            relative_path.clone(),
            PendingFileState {
                snapshot,
                skipped,
                mtime_ms: 0,
                size: 0,
                tracked: dirty_set.contains(relative_path),
            },
        );
    }

    Ok(CheckpointWorktreeCapture {
        checkpoint_ids: checkpoint_ids.to_vec(),
        work_dir: work_dir.to_string(),
        baseline: Some(baseline.clone()),
        before_paths,
        before_states,
        pending_dir: pending_dir.to_path_buf(),
    })
}

/// Commit only paths whose state changed while the terminal command ran.
///
/// Git-driven path (capture carried a baseline): two git commands replace the
/// whole second worktree traversal. `git diff` against the pre-command
/// baseline commit lists tracked changes — including changes committed during
/// the command, which `git status` would hide — and `git ls-files --others`
/// lists untracked files. Candidates are the union of those with the
/// snapshotted pre-command files, so a deleted untracked file (invisible to
/// git after deletion) still gets restored.
///
/// Legacy path: the worktree traversal happens **once** and is shared by every
/// checkpoint in the capture (they all validated against the same work_dir),
/// instead of repeating a full scan per checkpoint — the O(checkpoints ×
/// files) blowup that made concurrent terminal commands progressively slower
/// as a conversation accumulated checkpoints.
pub fn record_checkpoint_worktree_after(capture: CheckpointWorktreeCapture) -> Result<()> {
    let root = canonical_work_dir(&capture.work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    // 先筛出仍有效且属于当前 work_dir 的 checkpoint。这里只读取工作目录
    // 与 git 基线，真正写入前会在各自 manifest 锁内重新读取，避免覆盖
    // 同项目其他并行工具刚记录的条目。
    let mut effective_ids = Vec::new();
    let mut root = None;
    for checkpoint_id in &capture.checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock
            .lock()
            .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
        if !checkpoint_manifest_exists(checkpoint_id) {
            continue;
        }
        let manifest = read_manifest(checkpoint_id)?;
        if let Some(matched_root) = validate_capture_work_dir(&manifest, &capture.work_dir) {
            effective_ids.push(checkpoint_id.clone());
            root.get_or_insert(matched_root);
        }
    }
    let Some(root) = root else {
        return Ok(());
    };

    // git 驱动路径:diff(相对命令前基线,含命令期间已提交的变更)+
    // 未跟踪文件现况。候选 = 快照文件 ∪ diff ∪ 未跟踪;不再遍历工作区。
    // 逐文件判断复用在下方循环,非 git 回退走遍历候选。
    let mut candidates = capture.before_paths.clone();
    let mut diff_now: HashSet<String> = HashSet::new();
    if let Some(baseline) = capture.baseline.as_ref() {
        diff_now = git_diff_name_only(baseline)?.into_iter().collect();
        let untracked_now = git_untracked_paths(baseline)?;
        candidates.extend(diff_now.iter().cloned());
        candidates.extend(untracked_now);
    } else {
        let after_paths = collect_worktree_file_paths(&root)?;
        candidates.extend(after_paths);
    }

    for checkpoint_id in effective_ids {
        with_manifest_lock(&checkpoint_id, || {
            if !checkpoint_manifest_exists(&checkpoint_id) {
                return Ok(());
            }
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(root) = validate_capture_work_dir(&manifest, &capture.work_dir) else {
                return Ok(());
            };
            let mut changed = false;

            for relative_path in &candidates {
                let relative = from_forward_slashes(relative_path);
                if should_skip_relative(&relative) {
                    continue;
                }
                let absolute = root.join(&relative);
                let before_state = capture.before_states.get(relative_path);

                // 变更检测 + 原始状态物化:
                // - git 驱动路径:有快照的文件对比快照(original 为 Object);
                //   无快照的 clean tracked 文件仅删除时记录(original 为 Git),
                //   内容修改不记录(无法区分命令副作用与用户手动编辑);
                //   其余为命令新增的未跟踪文件 → Missing。
                // - 回退路径:tracked 元数据过滤 → 内容确认;未跟踪文件对比
                //   快照;新增文件 → Missing。
                let change = match before_state {
                    // 快照被跳过：无法恢复命令前内容，不记录变更
                    Some(state) if state.skipped => None,
                    Some(state) if capture.baseline.is_some() => {
                        if pending_state_matches_current(state, &absolute) {
                            None
                        } else {
                            Some(pending_state_to_original(state)?)
                        }
                    }
                    Some(state) if state.tracked => {
                        tracked_file_change(&manifest, relative_path, &absolute, state)?
                    }
                    Some(state) => {
                        if pending_state_matches_current(state, &absolute) {
                            None
                        } else {
                            Some(pending_state_to_original(state)?)
                        }
                    }
                    None if diff_now.contains(relative_path) => {
                        // 无快照的 clean tracked:仅删除时记录,内容修改不记录
                        match fs::metadata(&absolute) {
                            Err(_) => Some(OriginalState::Git),
                            Ok(_) => None,
                        }
                    }
                    None => absolute.is_file().then_some(OriginalState::Missing),
                };
                let Some(original) = change else {
                    continue;
                };

                capture_entry(&mut manifest, &absolute, &relative, original)?;
                changed = true;
            }

            if changed {
                write_manifest(&checkpoint_id, &manifest)?;
            }
            Ok(())
        })?;
    }
    if let Some(mut cache) = DIFF_CACHE.get().and_then(|cache| cache.lock().ok()) {
        cache.retain(|key, _| {
            !capture
                .checkpoint_ids
                .iter()
                .any(|checkpoint_id| key.starts_with(&format!("{checkpoint_id}:")))
        });
    }
    Ok(())
}

/// Restore only paths that were recorded by mutating tools after this checkpoint.
pub fn restore_checkpoint(checkpoint_id: String, work_dir: String) -> Result<()> {
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_write_guard(&work_dir_lock)?;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
    // If the manifest no longer exists (checkpoint was deleted or corrupted),
    // there is nothing to restore. Return Ok so the rollback flow continues
    // to delete messages without being blocked by a missing checkpoint.
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir(&manifest, &work_dir)?;

    let mut restored_entries = Vec::new();
    for entry in &manifest.entries {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let destination = resolve_manifest_path(&root, &entry.path);
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        if !states_match(&destination, expected, manifest.git.as_ref(), &entry.path)? {
            continue;
        }
        restore_entry(&root, &manifest, entry)?;
        restored_entries.push(entry.path.clone());
    }
    prune_empty_parent_directories(
        &root,
        &manifest
            .entries
            .iter()
            .filter(|entry| restored_entries.contains(&entry.path))
            .cloned()
            .collect::<Vec<_>>(),
    );

    Ok(())
}

fn restore_entry(
    root: &Path,
    manifest: &CheckpointManifest,
    entry: &CheckpointEntry,
) -> Result<()> {
    let destination = resolve_manifest_path(root, &entry.path);
    match &entry.original {
        OriginalState::Missing => {
            if destination.is_file() || destination.is_symlink() {
                fs::remove_file(&destination).map_err(|error| {
                    Error::from_reason(format!(
                        "Failed to remove added file '{}': {error}",
                        destination.display()
                    ))
                })?;
            }
            Ok(())
        }
        OriginalState::Object { object_id } => {
            let source = checkpoint_root()?.join(OBJECT_DIR_NAME).join(object_id);
            restore_file(&source, &destination)
        }
        OriginalState::Git => {
            let baseline = manifest
                .git
                .as_ref()
                .ok_or_else(|| Error::from_reason("Checkpoint Git baseline is missing"))?;
            let content = read_git_object(baseline, &entry.path)?.ok_or_else(|| {
                Error::from_reason(format!(
                    "Checkpoint Git object is missing for '{}'",
                    entry.path
                ))
            })?;
            write_file(&destination, &content)
        }
    }
}

fn restore_file(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Err(Error::from_reason(format!(
            "Checkpoint object not found: {}",
            source.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::from_reason(format!(
                "Failed to create restore directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    fs::copy(source, destination).map_err(|error| {
        Error::from_reason(format!(
            "Failed to restore file '{}': {error}",
            destination.display()
        ))
    })?;
    Ok(())
}

fn write_file(destination: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::from_reason(format!(
                "Failed to create restore directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    fs::write(destination, content).map_err(|error| {
        Error::from_reason(format!(
            "Failed to restore file '{}': {error}",
            destination.display()
        ))
    })
}

fn prune_empty_parent_directories(root: &Path, entries: &[CheckpointEntry]) {
    let mut directories: Vec<PathBuf> = entries
        .iter()
        .filter_map(|entry| {
            resolve_manifest_path(root, &entry.path)
                .parent()
                .map(Path::to_path_buf)
        })
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        let mut current = directory;
        while current.starts_with(root) && current != root {
            if fs::remove_dir(&current).is_err() {
                break;
            }
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent.to_path_buf();
        }
    }
}

/// Delete a checkpoint and release its Git reference. Content-addressed
/// objects are intentionally retained: eager global garbage collection scanned
/// every checkpoint after each best-effort delete and raced concurrent writers.
/// Existing objects are deduplicated by BLAKE3, so retaining them keeps deletes
/// constant-time and avoids re-copying identical file contents later.
pub fn delete_checkpoint(checkpoint_id: String) -> Result<()> {
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
    let directory = checkpoint_dir(&checkpoint_id)?;
    if !directory.exists() {
        return Ok(());
    }

    if let Ok(manifest) = read_manifest(&checkpoint_id) {
        if let Some(baseline) = manifest.git.as_ref() {
            update_checkpoint_git_ref(&checkpoint_id, baseline, true)?;
        }
    }
    fs::remove_dir_all(&directory).map_err(|error| {
        Error::from_reason(format!(
            "Failed to delete checkpoint '{}': {error}",
            checkpoint_id
        ))
    })
}

/// A single file change between the checkpoint snapshot and the current
/// working directory state.
#[napi(object)]
pub struct CheckpointFileChange {
    /// Relative file path (forward-slash separated).
    pub path: String,
    /// "added" (created after checkpoint, will be deleted),
    /// "modified" (content differs, will be reverted),
    /// "deleted" (existed at checkpoint, was removed, will be restored).
    pub change_type: String,
}

/// A file change with a unified diff suitable for rollback preview.
#[napi(object)]
pub struct CheckpointFileDiff {
    pub path: String,
    pub change_type: String,
    pub content: String,
    pub is_binary: bool,
}

fn collect_tracked_entries(manifest: &CheckpointManifest) -> Vec<CheckpointEntry> {
    manifest.entries.clone()
}

/// Compare only paths explicitly recorded while this conversation's tools ran.
pub fn list_checkpoint_changes(
    checkpoint_id: String,
    work_dir: String,
) -> Result<Vec<CheckpointFileChange>> {
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(Vec::new());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir(&manifest, &work_dir)?;
    let tracked = collect_tracked_entries(&manifest);

    let mut changes = Vec::new();
    for entry in tracked {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        let current = resolve_manifest_path(&root, &entry.path);
        if !states_match(&current, expected, manifest.git.as_ref(), &entry.path)? {
            continue;
        }
        if let Some(change_type) = classify_change(
            &current,
            &entry.original,
            manifest.git.as_ref(),
            &entry.path,
        )? {
            changes.push(CheckpointFileChange {
                path: entry.path,
                change_type,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Build unified diffs from checkpoint content to the current working state.
/// This is read-only and is used by the renderer's rollback preview and the
/// file-changes panel.
///
/// `include_all` controls which captured entries are reported:
/// - `false` (rollback preview): only files whose current state still matches
///   the checkpoint's post-change state. These are exactly the files rollback
///   would restore, so the preview matches the restore behaviour.
/// - `true` (file-changes panel): every captured entry is reported as long as
///   its current state differs from the pre-change state. Files that were
///   re-modified by later runs in a shared working tree stay visible, so an
///   earlier conversation's modifications are never erased from the panel.
pub fn list_checkpoint_diffs(
    checkpoint_id: String,
    work_dir: String,
    include_all: bool,
) -> Result<Vec<CheckpointFileDiff>> {
    let root = canonical_work_dir(&work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock is poisoned"))?;
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(Vec::new());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir(&manifest, &work_dir)?;
    let tracked = collect_tracked_entries(&manifest);

    let mut diffs = Vec::new();
    for entry in tracked {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        let current = resolve_manifest_path(&root, &entry.path);
        if !include_all && !states_match(&current, expected, manifest.git.as_ref(), &entry.path)? {
            continue;
        }
        let Some(change_type) = classify_change(
            &current,
            &entry.original,
            manifest.git.as_ref(),
            &entry.path,
        )?
        else {
            continue;
        };

        // 进程内 diff 缓存：original 摘要 + 磁盘 mtime/size 均未变时直接
        // 复用上次生成的 unified diff，避免高频工具循环下反复读文件与
        // TextDiff 全量计算（P0-4 性能优化）。
        let cache_key = format!("{}:{}", checkpoint_id, entry.path);
        let digest = original_digest(&entry.original, manifest.git.as_ref(), &entry.path);
        let cached = {
            let cache = diff_cache();
            let meta = fs::metadata(&current).ok();
            cache.get(&cache_key).and_then(|cached_entry| {
                let meta = meta.as_ref()?;
                (cached_entry.original_digest == digest
                    && cached_entry.current_mtime_ms == mtime_ms(meta)
                    && cached_entry.current_size == meta.len())
                .then_some((cached_entry.content.clone(), cached_entry.is_binary))
            })
        };
        let (content, is_binary) = match cached {
            Some((content, is_binary)) => (content, is_binary),
            None => {
                let original_content =
                    read_original_content(&entry.original, manifest.git.as_ref(), &entry.path)?;
                let current_content = read_current_content(&current)?;
                let (content, is_binary) = build_unified_diff(
                    &entry.path,
                    original_content.as_deref(),
                    current_content.as_deref(),
                );
                let meta = fs::metadata(&current).ok();
                let mut cache = diff_cache();
                if cache.len() >= DIFF_CACHE_MAX_ENTRIES {
                    cache.clear();
                }
                cache.insert(
                    cache_key,
                    CachedCheckpointDiff {
                        original_digest: digest,
                        current_mtime_ms: meta.as_ref().map(mtime_ms).unwrap_or(0),
                        current_size: meta.as_ref().map(|meta| meta.len()).unwrap_or(0),
                        content: content.clone(),
                        is_binary,
                    },
                );
                (content, is_binary)
            }
        };
        diffs.push(CheckpointFileDiff {
            path: entry.path,
            change_type,
            content,
            is_binary,
        });
    }
    diffs.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(diffs)
}

fn read_original_content(
    original: &OriginalState,
    baseline: Option<&GitBaseline>,
    relative: &str,
) -> Result<Option<Vec<u8>>> {
    match original {
        OriginalState::Missing => Ok(None),
        OriginalState::Object { object_id } => {
            let object = checkpoint_root()?.join(OBJECT_DIR_NAME).join(object_id);
            fs::read(&object).map(Some).map_err(|error| {
                Error::from_reason(format!(
                    "Failed to read checkpoint object '{}': {error}",
                    object.display()
                ))
            })
        }
        OriginalState::Git => {
            let baseline =
                baseline.ok_or_else(|| Error::from_reason("Checkpoint Git baseline is missing"))?;
            read_git_object(baseline, relative)
        }
    }
}

fn read_current_content(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_file() {
        return Err(Error::from_reason(format!(
            "Checkpoint path is not a regular file: {}",
            path.display()
        )));
    }
    fs::read(path).map(Some).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read current checkpoint file '{}': {error}",
            path.display()
        ))
    })
}

fn build_unified_diff(
    relative: &str,
    original: Option<&[u8]>,
    current: Option<&[u8]>,
) -> (String, bool) {
    let original_bytes = original.unwrap_or_default();
    let current_bytes = current.unwrap_or_default();
    let Ok(original_text) = std::str::from_utf8(original_bytes) else {
        return (String::new(), true);
    };
    let Ok(current_text) = std::str::from_utf8(current_bytes) else {
        return (String::new(), true);
    };
    if original_bytes.contains(&0) || current_bytes.contains(&0) {
        return (String::new(), true);
    }

    // 行尾归一化后再做行级 diff：Windows 下工具/编辑器常把文件落盘为
    // CRLF，而 original 来自 git/checkpoint 对象（LF）。直接按字节对比
    // 会让每个 CRLF 文件呈现"整文件改动"的数万行假 diff（仓库
    // .gitattributes 注释记载过同类现象）。仅当文本确实含 \r 时才替换，
    // LF-only 文件走零拷贝路径。此处仅归一化展示用的 diff，不修改任何
    // 落盘内容。
    let original_text = if original_text.contains('\r') {
        std::borrow::Cow::Owned(original_text.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(original_text)
    };
    let current_text = if current_text.contains('\r') {
        std::borrow::Cow::Owned(current_text.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(current_text)
    };

    let original_header = original
        .map(|_| format!("a/{relative}"))
        .unwrap_or_else(|| "/dev/null".to_string());
    let current_header = current
        .map(|_| format!("b/{relative}"))
        .unwrap_or_else(|| "/dev/null".to_string());
    let content = TextDiff::from_lines(&original_text, &current_text)
        .unified_diff()
        .context_radius(3)
        .header(&original_header, &current_header)
        .to_string();
    (content, false)
}

fn classify_change(
    current: &Path,
    original: &OriginalState,
    baseline: Option<&GitBaseline>,
    relative: &str,
) -> Result<Option<String>> {
    match original {
        OriginalState::Missing => Ok(current.exists().then(|| "added".to_string())),
        OriginalState::Object { object_id } => {
            if !current.exists() {
                return Ok(Some("deleted".to_string()));
            }
            let object = checkpoint_root()?.join(OBJECT_DIR_NAME).join(object_id);
            Ok(files_are_different(current, &object).then(|| "modified".to_string()))
        }
        OriginalState::Git => {
            let baseline =
                baseline.ok_or_else(|| Error::from_reason("Checkpoint Git baseline is missing"))?;
            let Some(content) = read_git_object(baseline, relative)? else {
                return Ok(current.exists().then(|| "added".to_string()));
            };
            if !current.exists() {
                return Ok(Some("deleted".to_string()));
            }
            Ok(file_differs_from_bytes(current, &content).then(|| "modified".to_string()))
        }
    }
}

fn file_differs_from_bytes(path: &Path, expected: &[u8]) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return true,
    };
    if metadata.len() != expected.len() as u64 {
        return true;
    }
    fs::read(path)
        .map(|content| content != expected)
        .unwrap_or(true)
}

/// Compare two files by size first, then by content. Returns true if they
/// differ (or if either file cannot be read).
fn files_are_different(a: &Path, b: &Path) -> bool {
    let meta_a = match fs::metadata(a) {
        Ok(m) => m,
        Err(_) => return true,
    };
    let meta_b = match fs::metadata(b) {
        Ok(m) => m,
        Err(_) => return true,
    };

    if meta_a.len() != meta_b.len() {
        return true;
    }

    // Same size — compare content byte-by-byte.
    let content_a = match fs::read(a) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let content_b = match fs::read(b) {
        Ok(c) => c,
        Err(_) => return true,
    };

    content_a != content_b
}

