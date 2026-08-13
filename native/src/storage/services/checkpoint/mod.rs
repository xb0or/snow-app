use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};

use super::checkpoint_skip::should_skip_pending_copy;
use super::gitignore::GitignoreMatcher;

mod git;
mod manifest;
mod paths;
pub(crate) mod remote;

use self::git::{read_git_object, update_checkpoint_git_ref};
use self::manifest::{read_manifest, write_manifest};
use self::paths::{
    canonical_work_dir, checkpoint_dir, checkpoint_manifest_exists, filter_existing_checkpoints,
    resolve_checkpoint_path, resolve_manifest_path, should_skip_manifest_path,
};

const OBJECT_DIR_NAME: &str = "objects";
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
/// 使用 tokio 锁：本地同步流程（spawn_blocking 内）走 blocking_*，
/// 远程 SSH 流程（async）跨 await 持锁，两种 guard 均可跨线程安全传递。
static CHECKPOINT_WORK_DIR_LOCKS: OnceLock<
    Mutex<HashMap<PathBuf, Weak<AsyncRwLock<()>>>>,
> = OnceLock::new();

/// manifest 级锁表：每个 checkpoint 独立串行 read-modify-write。
/// 同项目的不同会话拥有不同 checkpoint，因此文件编辑仅锁自己的
/// manifest，不再锁住整个工作目录。Weak 避免删除会话后残留锁对象。
static CHECKPOINT_MANIFEST_LOCKS: OnceLock<
    Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
> = OnceLock::new();

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
    /// Content id of the file's pre-command state (BLAKE3 object). `None`
    /// when the capture was skipped (too large / binary ext): the change
    /// cannot be recovered, after-pass skips it.
    object_id: Option<String>,
    /// Content capture skipped: change cannot be recovered, after-pass skips it.
    skipped: bool,
    /// Pre-command mtime (ms) and size used as a cheap first-pass change
    /// detector; a match skips the content read entirely.
    mtime_ms: u64,
    size: u64,
}

/// 进程内工作区指纹缓存：key = "{work_dir}\0{relative}"。命中条件为
/// mtime+size 未变，此时直接复用对象 id，完全跳过内容 IO。这使 before
/// 捕获退化为一次轻量 stat 扫描；只有真实变化过的文件才重新哈希。
static FINGERPRINT_CACHE: OnceLock<Mutex<HashMap<String, FingerprintEntry>>> = OnceLock::new();

/// 指纹缓存上限：超过后整体清空（条目是轻量 stat 元数据，重建成本低）。
const FINGERPRINT_CACHE_MAX_ENTRIES: usize = 100_000;

struct FingerprintEntry {
    mtime_ms: u64,
    size: u64,
    object_id: String,
}

fn fingerprint_cache() -> MutexGuard<'static, HashMap<String, FingerprintEntry>> {
    FINGERPRINT_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fingerprint_key(work_dir: &str, relative: &str) -> String {
    format!("{work_dir}\0{relative}")
}

fn fingerprint_lookup(work_dir: &str, relative: &str, mtime_ms: u64, size: u64) -> Option<String> {
    fingerprint_cache()
        .get(&fingerprint_key(work_dir, relative))
        .filter(|entry| entry.mtime_ms == mtime_ms && entry.size == size)
        .map(|entry| entry.object_id.clone())
}

fn fingerprint_store(work_dir: &str, relative: &str, mtime_ms: u64, size: u64, object_id: String) {
    let mut cache = fingerprint_cache();
    if cache.len() >= FINGERPRINT_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache
        .entry(fingerprint_key(work_dir, relative))
        .or_insert(FingerprintEntry {
            mtime_ms,
            size,
            object_id,
        });
}

pub struct CheckpointWorktreeCapture {
    checkpoint_ids: Vec<String>,
    work_dir: String,
    /// Pre-command file set (every file fingerprinted). All checkpoints are
    /// validated against the same `work_dir` during capture, so one result
    /// serves every checkpoint. Pre-command content lives in the content-
    /// addressed object store (BLAKE3); no git state is involved.
    before_paths: HashSet<String>,
    before_states: HashMap<String, PendingFileState>,
}
fn checkpoint_root() -> Result<PathBuf> {
    super::storage_locations::checkpoint_root()
}

fn work_dir_lock(work_dir: &Path) -> Result<Arc<AsyncRwLock<()>>> {
    let locks = CHECKPOINT_WORK_DIR_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint work directory lock registry is poisoned"))?;
    if let Some(lock) = locks.get(work_dir).and_then(Weak::upgrade) {
        return Ok(lock);
    }

    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(AsyncRwLock::new(()));
    locks.insert(work_dir.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn work_dir_read_guard(lock: &AsyncRwLock<()>) -> Result<tokio::sync::RwLockReadGuard<'_, ()>> {
    Ok(lock.blocking_read())
}

fn work_dir_write_guard(lock: &AsyncRwLock<()>) -> Result<tokio::sync::RwLockWriteGuard<'_, ()>> {
    Ok(lock.blocking_write())
}

/// 远程（SSH）async 流程的读锁：blocking_read 在 tokio worker 线程上会
/// panic（"while the thread is being used to drive asynchronous tasks"），
/// 远程流程必须用异步等待版本。
pub(crate) async fn work_dir_read_guard_async(
    lock: &AsyncRwLock<()>,
) -> tokio::sync::RwLockReadGuard<'_, ()> {
    lock.read().await
}

/// 远程（SSH）async 流程的写锁（仅回滚使用）。
pub(crate) async fn work_dir_write_guard_async(
    lock: &AsyncRwLock<()>,
) -> tokio::sync::RwLockWriteGuard<'_, ()> {
    lock.write().await
}

fn manifest_lock(checkpoint_id: &str) -> Result<Arc<AsyncMutex<()>>> {
    let locks = CHECKPOINT_MANIFEST_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| Error::from_reason("Checkpoint manifest lock registry is poisoned"))?;
    if let Some(lock) = locks.get(checkpoint_id).and_then(Weak::upgrade) {
        return Ok(lock);
    }

    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(checkpoint_id.to_string(), Arc::downgrade(&lock));
    Ok(lock)
}

fn with_manifest_lock<T>(
    checkpoint_id: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock = manifest_lock(checkpoint_id)?;
    let _guard = lock.blocking_lock();
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

/// Stream a file through BLAKE3 and return its hex content id.
fn hash_file(path: &Path) -> Result<String> {
    let mut source = File::open(path).map_err(|error| {
        Error::from_reason(format!(
            "Failed to read checkpoint source '{}': {error}",
            path.display()
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
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Publish the file's content into the content-addressed object store.
/// The object id is the BLAKE3 digest, so identical content is stored once
/// and repeated captures of unchanged files write nothing.
fn store_object(path: &Path) -> Result<String> {
    let object_dir = checkpoint_root()?.join(OBJECT_DIR_NAME);
    fs::create_dir_all(&object_dir).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object directory: {error}"
        ))
    })?;
    let object_id = hash_file(path)?;
    let final_path = object_dir.join(&object_id);
    if final_path.exists() {
        return Ok(object_id);
    }
    let temporary = object_dir.join(format!("{}.tmp", generate_checkpoint_id()));
    fs::copy(path, &temporary).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object '{}': {error}",
            temporary.display()
        ))
    })?;
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

/// Publish in-memory bytes into the content-addressed object store.
/// Used by the remote (SSH) checkpoint flows: file content arrives from
/// Electron via SFTP and is stored with the same BLAKE3 deduplication as
/// locally captured files.
fn store_object_bytes(content: &[u8]) -> Result<String> {
    let object_dir = checkpoint_root()?.join(OBJECT_DIR_NAME);
    fs::create_dir_all(&object_dir).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object directory: {error}"
        ))
    })?;
    let object_id = blake3::Hasher::new()
        .update(content)
        .finalize()
        .to_hex()
        .to_string();
    let final_path = object_dir.join(&object_id);
    if final_path.exists() {
        return Ok(object_id);
    }
    let temporary = object_dir.join(format!("{}.tmp", generate_checkpoint_id()));
    fs::write(&temporary, content).map_err(|error| {
        Error::from_reason(format!(
            "Failed to create checkpoint object '{}': {error}",
            temporary.display()
        ))
    })?;
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
/// Creation only publishes a new manifest, so it does not take the shared
/// work-directory lock used by active tool captures.
///
/// The manifest is fully self-contained: no git baseline is recorded, so
/// checkpoint capture/restore never depends on git state (working tree,
/// index, HEAD), which the user or other conversations may mutate at any time.
pub fn create_checkpoint(work_dir: String) -> Result<String> {
    let root = canonical_work_dir(&work_dir)?;
    let checkpoint_id = generate_checkpoint_id();
    with_manifest_lock(&checkpoint_id, || {
        let manifest = CheckpointManifest {
            version: MANIFEST_VERSION,
            work_dir: root.to_string_lossy().to_string(),
            git: None,
            entries: Vec::new(),
        };

        write_manifest(&checkpoint_id, &manifest)?;
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

/// 判断命令后文件是否仍与命令前状态一致（mtime+size 快速路径，
/// 内容 hash 兜底）。返回 true 表示无变化。
fn pending_state_matches_current(state: &PendingFileState, current: &Path) -> Result<bool> {
    // 快速路径：mtime+size 未变 → 未修改（工具写文件必更新 mtime），
    // 完全跳过内容 IO。
    if let Ok(meta) = fs::metadata(current) {
        if meta.len() == state.size && mtime_ms(&meta) == state.mtime_ms {
            return Ok(true);
        }
    }
    if !current.is_file() {
        return Ok(false);
    }
    let Some(object_id) = state.object_id.as_ref() else {
        return Ok(false);
    };
    // 内容兜底：仅 metadata 变化的文件重新哈希对比（如 touch 场景）。
    Ok(hash_file(current)? == *object_id)
}

fn pending_state_to_original(state: &PendingFileState) -> Result<OriginalState> {
    let object_id = state.object_id.as_ref().ok_or_else(|| {
        Error::from_reason("Cannot materialize an original from a skipped pending state")
    })?;
    Ok(OriginalState::Object {
        object_id: object_id.clone(),
    })
}

/// Snapshot the current worktree before a tool command runs. No manifest
/// entries are committed until the command ends.
///
/// The checkpoint system is fully self-contained ("its own git"): every file
/// is fingerprinted up front (stat + BLAKE3 content id in a deduplicated
/// object store) and the after-pass compares against these fingerprints. No
/// git state (HEAD, index, working tree) is consulted, so changes made by the
/// user or by other conversations — commits, deletes, edits — can never leak
/// into this conversation's rollback list: anything already on disk when the
/// command starts is frozen as "before". Unchanged files hit the fingerprint
/// cache (mtime+size) and cost zero content IO; the object store deduplicates
/// by content, so disk usage is bounded by the worktree's unique content, not
/// by the number of commands or checkpoints.
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
    // 不做无意义的全目录扫描。
    let mut matched_any = false;
    for checkpoint_id in &checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock.blocking_lock();
        if !checkpoint_manifest_exists(checkpoint_id) {
            continue;
        }
        let manifest = read_manifest(checkpoint_id)?;
        if validate_capture_work_dir(&manifest, &work_dir).is_some() {
            matched_any = true;
            break;
        }
    }
    if !matched_any {
        return Ok(None);
    }

    // 全量遍历（跳过 SKIP_DIRS / gitignore / 符号链接），逐文件记录
    // mtime+size 与内容对象 id。指纹缓存命中时零内容 IO。
    let before_paths = collect_worktree_file_paths(&root)?;

    let mut before_states = HashMap::new();
    for relative_path in &before_paths {
        let absolute = root.join(from_forward_slashes(relative_path));
        let meta = fs::metadata(&absolute).ok();
        let mtime = meta.as_ref().map(mtime_ms).unwrap_or(0);
        let size = meta.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let (object_id, skipped) = if let Some(object_id) =
            fingerprint_lookup(&work_dir, relative_path, mtime, size)
        {
            (Some(object_id), false)
        } else if should_skip_pending_copy(&absolute) {
            // 大文件/二进制：不抓取内容，变更不可回滚。
            (None, true)
        } else {
            match store_object(&absolute) {
                Ok(object_id) => {
                    fingerprint_store(&work_dir, relative_path, mtime, size, object_id.clone());
                    (Some(object_id), false)
                }
                Err(_) if !absolute.exists() => continue,
                Err(error) => return Err(error),
            }
        };
        before_states.insert(
            relative_path.clone(),
            PendingFileState {
                object_id,
                skipped,
                mtime_ms: mtime,
                size,
            },
        );
    }

    Ok(Some(CheckpointWorktreeCapture {
        checkpoint_ids,
        work_dir,
        before_paths,
        before_states,
    }))
}

/// Commit only paths whose state changed while the tool command ran.
///
/// The worktree traversal happens **once** and is shared by every checkpoint
/// in the capture (they all validated against the same work_dir), instead of
/// repeating a full scan per checkpoint — the O(checkpoints × files) blowup
/// that made concurrent terminal commands progressively slower as a
/// conversation accumulated checkpoints. Only files whose state differs from
/// the before-fingerprint are recorded; every other file is left untouched.
pub fn record_checkpoint_worktree_after(capture: CheckpointWorktreeCapture) -> Result<()> {
    let root = canonical_work_dir(&capture.work_dir)?;
    let work_dir_lock = work_dir_lock(&root)?;
    let _work_dir_guard = work_dir_read_guard(&work_dir_lock)?;
    // 先筛出仍有效且属于当前 work_dir 的 checkpoint。真正写入前会在各自
    // manifest 锁内重新读取，避免覆盖同项目其他并行工具刚记录的条目。
    let mut effective_ids = Vec::new();
    let mut root = None;
    for checkpoint_id in &capture.checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock.blocking_lock();
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

    // 候选 = 命令前文件集 ∪ 命令后工作树文件集：覆盖新增/删除/修改全部
    // 情形。逐文件与 before 指纹对比，只有真实变化才记录——其他会话或
    // 用户在命令执行前已落盘的改动已固化在 before 指纹里，不会误记。
    let after_paths = collect_worktree_file_paths(&root)?;
    let mut candidates = capture.before_paths.clone();
    candidates.extend(after_paths);

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

                // 变更检测 + 原始状态物化：
                // - before 存在：mtime+size/hash 对比；文件已消失 → 删除
                //   （original 为 before 对象 id）；
                // - before 不存在且命令后存在：命令新建 → Missing。
                let change = match before_state {
                    // 内容抓取被跳过：无法恢复命令前内容，不记录变更
                    Some(state) if state.skipped => None,
                    Some(state) => {
                        if pending_state_matches_current(state, &absolute)? {
                            None
                        } else {
                            Some(pending_state_to_original(state)?)
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
    let _manifest_guard = manifest_lock.blocking_lock();
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
    let _manifest_guard = manifest_lock.blocking_lock();
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
    let _manifest_guard = manifest_lock.blocking_lock();
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
    let _manifest_guard = manifest_lock.blocking_lock();
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
