//! SSH 工作区的 checkpoint 远程文件访问层。
//!
//! 本地 checkpoint 用 `std::fs` 直接访问工作区；SSH 工作区（`ssh://` URI）
//! 的文件由 Electron 主进程通过 SFTP 访问，Rust 侧无法直接 IO。本模块把
//! checkpoint 需要的文件操作（stat / 递归列目录 / 读 / 写 / 删）封装为
//! 通过 `RemoteWorkspaceCallback` 转发给 Electron 的异步命令，供远程版
//! checkpoint 流程复用同一套 manifest / 对象存储逻辑。

use base64::Engine;
use napi::bindgen_prelude::*;
use serde_json::{json, Value};

use crate::mcp::servers::remote_workspace::{
    execute_remote_workspace_command, is_ssh_path, RemoteWorkspaceCallback,
};

use super::ABSOLUTE_PATH_MARKER;

/// 单个远程条目的 stat 信息（checkpoint-stat 返回）。
#[derive(Clone, Debug)]
pub struct RemoteFileStat {
    pub is_directory: bool,
    pub size: u64,
    pub mtime_ms: u64,
}

/// 远程工作区文件树条目（相对根目录的 POSIX 路径，已跳过 SKIP_DIRS 与
/// 符号链接——与本地 collect_worktree_file_paths 的语义一致）。
#[derive(Clone, Debug)]
pub struct RemoteTreeEntry {
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
    pub mtime_ms: u64,
}

/// 远程目录的 .gitignore 内容（dir 为相对根目录的 POSIX 路径，根目录为
/// 空字符串）。Rust 侧用与本地相同的 GitignoreMatcher 语义做过滤。
#[derive(Clone, Debug)]
pub struct RemoteGitignore {
    pub dir: String,
    pub content: String,
}

/// checkpoint-list-tree 的完整返回：文件树 + 各目录的 .gitignore 内容。
#[derive(Clone, Debug)]
pub struct RemoteTreeListing {
    pub entries: Vec<RemoteTreeEntry>,
    pub gitignores: Vec<RemoteGitignore>,
}

/// SSH 工作区 checkpoint 的远程文件访问客户端。每个方法发起一次
/// checkpoint-* 远程命令并等待 Electron 侧 SFTP 完成。持有 callback
/// 的借用（napi ThreadsafeFunction 不支持 Clone）。
pub struct RemoteCheckpointClient<'a> {
    on_command: &'a RemoteWorkspaceCallback,
}

impl<'a> RemoteCheckpointClient<'a> {
    pub fn new(on_command: &'a RemoteWorkspaceCallback) -> Self {
        Self { on_command }
    }

    /// 远程 stat：路径不存在时返回 Ok(None)。
    pub async fn stat(&self, path: &str) -> Result<Option<RemoteFileStat>> {
        let result = self
            .run("checkpoint-stat", json!({ "path": path }))
            .await?;
        if !result.get("exists").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(RemoteFileStat {
            is_directory: result
                .get("isDirectory")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            size: result.get("size").and_then(Value::as_u64).unwrap_or(0),
            mtime_ms: result.get("mtimeMs").and_then(Value::as_u64).unwrap_or(0),
        }))
    }

    /// 递归列出远程工作区文件树（含各目录 .gitignore 内容），
    /// 返回相对根目录的 POSIX 路径。
    pub async fn list_tree(&self, root: &str) -> Result<RemoteTreeListing> {
        let result = self
            .run("checkpoint-list-tree", json!({ "path": root }))
            .await?;
        let entries = result
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut tree = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(path) = entry.get("path").and_then(Value::as_str) else {
                continue;
            };
            tree.push(RemoteTreeEntry {
                path: path.to_string(),
                is_directory: entry
                    .get("isDirectory")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
                mtime_ms: entry
                    .get("mtimeMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        let mut gitignores = Vec::new();
        if let Some(items) = result.get("gitignores").and_then(Value::as_array) {
            for item in items {
                let (Some(dir), Some(content)) = (
                    item.get("dir").and_then(Value::as_str),
                    item.get("content").and_then(Value::as_str),
                ) else {
                    continue;
                };
                gitignores.push(RemoteGitignore {
                    dir: dir.to_string(),
                    content: content.to_string(),
                });
            }
        }
        Ok(RemoteTreeListing { entries: tree, gitignores })
    }

    /// 读取远程文件内容；文件不存在时返回 Ok(None)。
    pub async fn read_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let result = self
            .run("checkpoint-read-file", json!({ "path": path }))
            .await?;
        match result.get("content").and_then(Value::as_str) {
            Some(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map(Some)
                .map_err(|error| {
                    Error::from_reason(format!(
                        "Failed to decode remote checkpoint file content: {error}"
                    ))
                }),
            None => Ok(None),
        }
    }

    /// 写入远程文件（自动创建父目录）。
    pub async fn write_bytes(&self, path: &str, content: &[u8]) -> Result<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        self.run(
            "checkpoint-write-file",
            json!({ "path": path, "contentBase64": encoded }),
        )
        .await?;
        Ok(())
    }

    /// 删除远程文件；文件不存在视为成功（恢复 Missing 语义）。
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let result = self
            .run("checkpoint-delete-file", json!({ "path": path }))
            .await?;
        if result
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(Error::from_reason(format!(
                "Failed to delete remote file '{path}'"
            )))
        }
    }

    /// 尝试删除远程空目录；目录不存在或非空时返回 Ok(false)。
    pub async fn remove_dir(&self, path: &str) -> Result<bool> {
        let result = self
            .run("checkpoint-remove-dir", json!({ "path": path }))
            .await?;
        Ok(result
            .get("removed")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    async fn run(&self, operation: &str, args: Value) -> Result<Value> {
        execute_remote_workspace_command(self.on_command, operation, &args, None)
            .await
            .map_err(|error| {
                Error::from_reason(format!(
                    "Remote checkpoint operation '{operation}' failed: {error}"
                ))
            })
    }
}

/// 解析 `ssh://user@host:port/path`，返回 (authority, remote_path)。
fn split_ssh_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("ssh://")?;
    let at = rest.find('@')?;
    let authority = &rest[..=at];
    let host_port_path = &rest[at + 1..];
    let slash = host_port_path.find('/')?;
    let host_port = &host_port_path[..slash];
    let remote_path = &host_port_path[slash..];
    Some((
        format!("{authority}{host_port}"),
        remote_path.to_string(),
    ))
}

/// 归一化 ssh:// URI：去除尾部斜杠。
pub fn normalize_ssh_uri(uri: &str) -> String {
    let trimmed = uri.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 把 ssh:// 文件 URI 解析为 (absolute_uri, 相对工作区根的 POSIX 路径)。
///
/// 与本地 `resolve_checkpoint_path` 对应：工作区内的文件返回相对路径，
/// 工作区之外的绝对路径（跨 authority 或不在根路径下）用
/// `ABSOLUTE_PATH_MARKER` 标记存完整 URI。
pub fn resolve_remote_checkpoint_path(root: &str, file_path: &str) -> (String, String) {
    let Some((root_authority, root_path)) = split_ssh_uri(root) else {
        // 根本身不是合法 ssh:// URI：把文件路径原样拼到根后，交由调用方报错。
        return (
            format!("{}/{}", root.trim_end_matches('/'), file_path),
            file_path.to_string(),
        );
    };
    let root_path = root_path.trim_end_matches('/');
    let Some((authority, path)) = split_ssh_uri(file_path) else {
        // 相对路径：拼到工作区根下。
        return (
            format!("{}/{file_path}", root.trim_end_matches('/')),
            file_path.to_string(),
        );
    };
    if authority == root_authority {
        let path = path.trim_end_matches('/');
        if path == root_path || path.is_empty() {
            return (file_path.to_string(), String::new());
        }
        if let Some(relative) = path.strip_prefix(&format!("{root_path}/")) {
            return (file_path.to_string(), relative.to_string());
        }
    }
    // 工作区之外的绝对路径：标记存储完整 URI。
    (
        file_path.to_string(),
        format!("{ABSOLUTE_PATH_MARKER}{file_path}"),
    )
}

/// 把 manifest 条目路径解析回完整的 ssh:// URI。
pub fn resolve_remote_manifest_path(root: &str, manifest_path: &str) -> String {
    if let Some(absolute) = manifest_path.strip_prefix(ABSOLUTE_PATH_MARKER) {
        absolute.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), manifest_path)
    }
}

/// 远程工作区根目录校验：URI 合法且远端存在且为目录。
pub async fn canonical_work_dir_remote(
    client: &RemoteCheckpointClient<'_>,
    work_dir: &str,
) -> Result<String> {
    let trimmed = work_dir.trim();
    if !is_ssh_path(trimmed) {
        return Err(Error::from_reason(format!(
            "Working directory is not an SSH path: {work_dir}"
        )));
    }
    let normalized = normalize_ssh_uri(trimmed);
    let Some(stats) = client.stat(&normalized).await? else {
        return Err(Error::from_reason(format!(
            "Remote working directory does not exist: {work_dir}"
        )));
    };
    if !stats.is_directory {
        return Err(Error::from_reason(format!(
            "Path is not a directory: {work_dir}"
        )));
    }
    Ok(normalized)
}

// ============================================================================
// 远程（SSH）checkpoint 流程：与 mod.rs 的本地实现一一对应，文件 IO 全部
// 通过 RemoteCheckpointClient 转发给 Electron，manifest / 对象存储 / diff
// 逻辑完全复用本地实现。
// ============================================================================

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    checkpoint_root, checkpoint_manifest_exists, filter_existing_checkpoints, fingerprint_lookup,
    fingerprint_store, manifest_lock, read_manifest, should_skip_manifest_path,
    should_skip_relative, store_object_bytes, to_forward_slashes, work_dir_lock,
    work_dir_read_guard_async, work_dir_write_guard_async, write_manifest, CachedCheckpointDiff,
    CheckpointEntry, CheckpointFileChange, CheckpointFileDiff, CheckpointManifest,
    CheckpointWorktreeCapture, OriginalState, PendingFileState, DIFF_CACHE_MAX_ENTRIES,
    OBJECT_DIR_NAME,
};

use crate::storage::services::checkpoint_skip::should_skip_pending_copy_size;

/// 远程工作目录校验：URI 规范化后与 manifest 记录值比较。
fn validate_manifest_work_dir_remote(
    manifest: &CheckpointManifest,
    work_dir: &str,
) -> Result<String> {
    let requested = normalize_ssh_uri(work_dir);
    let recorded = normalize_ssh_uri(&manifest.work_dir);
    if requested != recorded {
        return Err(Error::from_reason(format!(
            "Checkpoint belongs to '{}', not '{}'",
            recorded, requested
        )));
    }
    Ok(requested)
}

/// 捕获阶段的远程目录校验：不匹配时返回 None 并跳过（与本地行为一致）。
fn validate_capture_work_dir_remote(
    manifest: &CheckpointManifest,
    work_dir: &str,
) -> Option<String> {
    match validate_manifest_work_dir_remote(manifest, work_dir) {
        Ok(root) => Some(root),
        Err(error) => {
            eprintln!("[checkpoint] {error}; skipping checkpoint capture");
            None
        }
    }
}

/// 远程文件树 → 相对路径 → stat 映射（只含常规文件）。
fn remote_stat_map(tree: &[RemoteTreeEntry]) -> HashMap<String, RemoteFileStat> {
    tree.iter()
        .filter(|entry| !entry.is_directory)
        .map(|entry| {
            (
                entry.path.clone(),
                RemoteFileStat {
                    is_directory: false,
                    size: entry.size,
                    mtime_ms: entry.mtime_ms,
                },
            )
        })
        .collect()
}

/// 用各目录 .gitignore 内容构建 matcher（与本地 collect_worktree_file_paths
/// 的加载顺序一致：根目录规则先加载，子目录按深度升序后加载，深层规则
/// 覆盖浅层）。远程不读取工作区父目录与 .git/info/exclude（在 SSH
/// workspace 边界之外）。
fn build_remote_matcher(gitignores: &[RemoteGitignore]) -> crate::storage::services::gitignore::GitignoreMatcher {
    let mut sorted: Vec<&RemoteGitignore> = gitignores.iter().collect();
    sorted.sort_by_key(|gitignore| gitignore.dir.matches('/').count());
    let root_content = sorted
        .iter()
        .find(|gitignore| gitignore.dir.is_empty())
        .map(|gitignore| gitignore.content.as_str());
    let mut matcher =
        crate::storage::services::gitignore::GitignoreMatcher::from_root_content(root_content);
    for gitignore in sorted {
        if gitignore.dir.is_empty() {
            continue;
        }
        matcher.append_directory_content(Path::new(&gitignore.dir), &gitignore.content);
    }
    matcher
}

/// 判断路径是否被 gitignore 忽略：检查路径本身及所有父目录。本地实现在
/// 遍历时对目录逐级过滤；远程树只含文件条目，必须补上父目录检查才能
/// 复现"忽略目录 = 忽略其下全部内容"的 git 语义。
fn is_ignored_with_ancestors(matcher: &crate::storage::services::gitignore::GitignoreMatcher, path: &str) -> bool {
    let mut prefix = String::new();
    for segment in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if matcher.is_ignored(&prefix, true) {
            return true;
        }
    }
    matcher.is_ignored(path, false)
}

/// 远程工作区文件树扫描 + .gitignore 过滤：所有远程 checkpoint 流程统一
/// 使用本函数，保证 before/after 捕获与变更/回滚检测的过滤语义一致。
async fn filter_remote_tree(
    client: &RemoteCheckpointClient<'_>,
    root: &str,
) -> Result<Vec<RemoteTreeEntry>> {
    let listing = client.list_tree(root).await?;
    let matcher = build_remote_matcher(&listing.gitignores);
    Ok(listing
        .entries
        .into_iter()
        .filter(|entry| !is_ignored_with_ancestors(&matcher, &entry.path))
        .collect())
}

/// 读取远程文件当前状态（Missing / Object）。与本地 current_state 对齐：
/// 内容无条件抓取（不应用大小/扩展名跳过）。
async fn current_state_remote(
    client: &RemoteCheckpointClient<'_>,
    path: &str,
) -> Result<OriginalState> {
    let Some(stat) = client.stat(path).await? else {
        return Ok(OriginalState::Missing);
    };
    if stat.is_directory {
        return Err(Error::from_reason(format!(
            "Checkpoint path is not a regular file: {path}"
        )));
    }
    let Some(content) = client.read_bytes(path).await? else {
        return Ok(OriginalState::Missing);
    };
    Ok(OriginalState::Object {
        object_id: store_object_bytes(&content)?,
    })
}

/// 更新 expected 状态的远程版本。
async fn update_expected_state_remote(
    client: &RemoteCheckpointClient<'_>,
    manifest: &mut CheckpointManifest,
    absolute: &str,
    path: &str,
) -> Result<bool> {
    let Some(entry) = manifest.entries.iter_mut().find(|entry| entry.path == path) else {
        return Ok(false);
    };
    entry.expected = Some(current_state_remote(client, absolute).await?);
    Ok(true)
}

/// 记录条目（含 expected）的远程版本：当前内容经 SFTP 读取后存入本地对象库。
async fn capture_entry_remote(
    client: &RemoteCheckpointClient<'_>,
    manifest: &mut CheckpointManifest,
    absolute: &str,
    relative: &Path,
    original: OriginalState,
) -> Result<()> {
    if relative.as_os_str().is_empty() || should_skip_relative(relative) {
        return Ok(());
    }
    let path = to_forward_slashes(relative);
    let expected = current_state_remote(client, absolute).await?;
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

/// 比较远程当前 stat 与某个历史状态（Missing / Object），返回变更类型。
/// 大小不同直接判定；大小相同才读远程内容对比，避免无谓的网络传输。
async fn classify_change_remote(
    client: &RemoteCheckpointClient<'_>,
    stat: Option<&RemoteFileStat>,
    original: &OriginalState,
    relative: &str,
    root: &str,
) -> Result<Option<String>> {
    match original {
        OriginalState::Missing => Ok(stat.map(|_| "added".to_string())),
        OriginalState::Object { object_id } => {
            let Some(stat) = stat else {
                return Ok(Some("deleted".to_string()));
            };
            let object_path = checkpoint_root()?.join(OBJECT_DIR_NAME).join(object_id);
            let object_bytes = match fs::read(&object_path) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(Some("modified".to_string())),
            };
            if stat.size != object_bytes.len() as u64 {
                return Ok(Some("modified".to_string()));
            }
            let absolute = resolve_remote_manifest_path(root, relative);
            let Some(content) = client.read_bytes(&absolute).await? else {
                return Ok(Some("deleted".to_string()));
            };
            Ok((content != object_bytes).then(|| "modified".to_string()))
        }
        OriginalState::Git => Err(Error::from_reason(format!(
            "Checkpoint Git baseline is missing for '{relative}'"
        ))),
    }
}

/// 当前状态是否仍等于 expected（即该文件仍可被回滚恢复）。
async fn states_match_remote(
    client: &RemoteCheckpointClient<'_>,
    stat: Option<&RemoteFileStat>,
    expected: &OriginalState,
    relative: &str,
    root: &str,
) -> Result<bool> {
    Ok(
        classify_change_remote(client, stat, expected, relative, root)
            .await?
            .is_none(),
    )
}

/// 带 manifest 锁的异步操作（远程流程内部需要 await SFTP 调用）。
async fn with_manifest_lock_async<T, Fut>(
    checkpoint_id: &str,
    operation: impl FnOnce() -> Fut,
) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>>,
{
    let lock = manifest_lock(checkpoint_id)?;
    let _guard = lock.lock().await;
    operation().await
}

/// 远程版 before 捕获：一次 list-tree 拿到全树 stat，逐文件指纹化。
/// 未变化的文件命中指纹缓存（mtime+size），零内容 IO。
pub(crate) async fn capture_checkpoint_worktree_before_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_ids: Vec<String>,
    work_dir: String,
) -> Result<Option<CheckpointWorktreeCapture>> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(None);
    }
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;

    // 至少一个 checkpoint 属于当前远程目录才扫描。
    let mut matched_any = false;
    for checkpoint_id in &checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock.lock().await;
        if !checkpoint_manifest_exists(checkpoint_id) {
            continue;
        }
        let manifest = read_manifest(checkpoint_id)?;
        if validate_capture_work_dir_remote(&manifest, &work_dir).is_some() {
            matched_any = true;
            break;
        }
    }
    if !matched_any {
        return Ok(None);
    }

    let tree = filter_remote_tree(client, &root).await?;
    let mut before_paths = HashSet::new();
    let mut before_states = HashMap::new();
    for entry in &tree {
        if entry.is_directory {
            continue;
        }
        let relative = &entry.path;
        if should_skip_relative(Path::new(relative)) {
            continue;
        }
        before_paths.insert(relative.clone());
        let absolute = resolve_remote_manifest_path(&root, relative);
        let (object_id, skipped) =
            if let Some(object_id) = fingerprint_lookup(&work_dir, relative, entry.mtime_ms, entry.size)
            {
                (Some(object_id), false)
            } else if should_skip_pending_copy_size(entry.size, relative) {
                (None, true)
            } else {
                match client.read_bytes(&absolute).await {
                    Ok(Some(content)) => {
                        let object_id = store_object_bytes(&content)?;
                        fingerprint_store(
                            &work_dir,
                            relative,
                            entry.mtime_ms,
                            entry.size,
                            object_id.clone(),
                        );
                        (Some(object_id), false)
                    }
                    Ok(None) => continue, // 扫描后被删除
                    Err(error) => return Err(error),
                }
            };
        before_states.insert(
            relative.clone(),
            PendingFileState {
                object_id,
                skipped,
                mtime_ms: entry.mtime_ms,
                size: entry.size,
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

/// 远程版 after 记录：一次 list-tree 得到命令后的树，与 before 指纹对比，
/// 只记录真实变化的文件。
pub(crate) async fn record_checkpoint_worktree_after_remote(
    client: &RemoteCheckpointClient<'_>,
    capture: CheckpointWorktreeCapture,
) -> Result<()> {
    let root = canonical_work_dir_remote(client, &capture.work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;

    let mut effective_ids = Vec::new();
    for checkpoint_id in &capture.checkpoint_ids {
        let lock = manifest_lock(checkpoint_id)?;
        let _guard = lock.lock().await;
        if !checkpoint_manifest_exists(checkpoint_id) {
            continue;
        }
        let manifest = read_manifest(checkpoint_id)?;
        if validate_capture_work_dir_remote(&manifest, &capture.work_dir).is_some() {
            effective_ids.push(checkpoint_id.clone());
        }
    }
    let Some(root) = effective_ids
        .first()
        .map(|_| root.clone())
    else {
        return Ok(());
    };

    let tree = filter_remote_tree(client, &root).await?;
    let after_stats = remote_stat_map(&tree);
    let mut candidates = capture.before_paths.clone();
    candidates.extend(after_stats.keys().cloned());

    for checkpoint_id in effective_ids {
        with_manifest_lock_async(&checkpoint_id, || async {
            if !checkpoint_manifest_exists(&checkpoint_id) {
                return Ok(());
            }
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(root) = validate_capture_work_dir_remote(&manifest, &capture.work_dir) else {
                return Ok(());
            };
            let mut changed = false;
            for relative_path in &candidates {
                if should_skip_relative(Path::new(relative_path)) {
                    continue;
                }
                let absolute = resolve_remote_manifest_path(&root, relative_path);
                let before_state = capture.before_states.get(relative_path);
                let after_stat = after_stats.get(relative_path);

                let change = match before_state {
                    Some(state) if state.skipped => None,
                    Some(state) => match after_stat {
                        None => Some(super::pending_state_to_original(state)?),
                        Some(stat) => {
                            // mtime+size 相同 → 未变（与本地快速路径一致）。
                            if stat.mtime_ms == state.mtime_ms && stat.size == state.size {
                                None
                            } else {
                                // size 不同直接判变；相同则读内容哈希对比，
                                // 弥补远程 mtime 秒级精度下的同秒改写漏检。
                                let differs = if stat.size != state.size {
                                    true
                                } else {
                                    match client.read_bytes(&absolute).await {
                                        Ok(Some(content)) => {
                                            let current_id = store_object_bytes(&content)?;
                                            Some(current_id) != state.object_id
                                        }
                                        Ok(None) => true,
                                        Err(error) => return Err(error),
                                    }
                                };
                                differs
                                    .then(|| super::pending_state_to_original(state))
                                    .transpose()?
                            }
                        }
                    },
                    None => after_stat.map(|_| OriginalState::Missing),
                };
                let Some(original) = change else {
                    continue;
                };
                capture_entry_remote(
                    client,
                    &mut manifest,
                    &absolute,
                    Path::new(relative_path),
                    original,
                )
                .await?;
                changed = true;
            }
            if changed {
                write_manifest(&checkpoint_id, &manifest)?;
            }
            Ok(())
        })
        .await?;
    }

    if let Some(mut cache) = super::DIFF_CACHE.get().and_then(|cache| cache.lock().ok()) {
        cache.retain(|key, _| {
            !capture
                .checkpoint_ids
                .iter()
                .any(|checkpoint_id| key.starts_with(&format!("{checkpoint_id}:")))
        });
    }
    Ok(())
}

/// 远程版单文件 before 记录（filesystem-replace_edit/create 前）。
pub(crate) async fn record_checkpoint_file_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_ids: Vec<String>,
    work_dir: String,
    file_path: String,
) -> Result<()> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(());
    }
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;
    let (absolute, path) = resolve_remote_checkpoint_path(&root, &file_path);
    if path.is_empty() || should_skip_manifest_path(&path) {
        return Ok(());
    }

    for checkpoint_id in checkpoint_ids {
        with_manifest_lock_async(&checkpoint_id, || async {
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(_root) = validate_capture_work_dir_remote(&manifest, &work_dir) else {
                return Ok(());
            };
            if manifest.entries.iter().any(|entry| entry.path == path) {
                return Ok(());
            }
            manifest.entries.push(CheckpointEntry {
                path: path.clone(),
                original: current_state_remote(client, &absolute).await?,
                expected: None,
            });
            write_manifest(&checkpoint_id, &manifest)
        })
        .await?;
    }
    Ok(())
}

/// 远程版单文件 after 记录（filesystem-replace_edit/create 成功后）。
pub(crate) async fn record_checkpoint_file_after_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_ids: Vec<String>,
    work_dir: String,
    file_path: String,
) -> Result<()> {
    let checkpoint_ids = filter_existing_checkpoints(checkpoint_ids);
    if checkpoint_ids.is_empty() {
        return Ok(());
    }
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;
    let (absolute, path) = resolve_remote_checkpoint_path(&root, &file_path);
    if path.is_empty() || should_skip_manifest_path(&path) {
        return Ok(());
    }

    for checkpoint_id in checkpoint_ids {
        with_manifest_lock_async(&checkpoint_id, || async {
            let mut manifest = read_manifest(&checkpoint_id)?;
            let Some(_root) = validate_capture_work_dir_remote(&manifest, &work_dir) else {
                return Ok(());
            };
            if update_expected_state_remote(client, &mut manifest, &absolute, &path).await? {
                write_manifest(&checkpoint_id, &manifest)?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// 远程版变更列表（回滚确认对话框）。
pub(crate) async fn list_checkpoint_changes_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_id: String,
    work_dir: String,
) -> Result<Vec<CheckpointFileChange>> {
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock.lock().await;
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(Vec::new());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir_remote(&manifest, &work_dir)?;
    let tracked = manifest.entries.clone();

    let tree = filter_remote_tree(client, &root).await?;
    let current = remote_stat_map(&tree);

    let mut changes = Vec::new();
    for entry in tracked {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        let stat = current.get(&entry.path);
        if !states_match_remote(client, stat, expected, &entry.path, &root).await? {
            continue;
        }
        if let Some(change_type) =
            classify_change_remote(client, stat, &entry.original, &entry.path, &root).await?
        {
            changes.push(CheckpointFileChange {
                path: entry.path,
                change_type,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// 远程版 diff 列表（回滚预览 / 文件变更面板）。
pub(crate) async fn list_checkpoint_diffs_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_id: String,
    work_dir: String,
    include_all: bool,
) -> Result<Vec<CheckpointFileDiff>> {
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_read_guard_async(&work_dir_lock).await;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock.lock().await;
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(Vec::new());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir_remote(&manifest, &work_dir)?;
    let tracked = manifest.entries.clone();

    let tree = filter_remote_tree(client, &root).await?;
    let current = remote_stat_map(&tree);

    let mut diffs = Vec::new();
    for entry in tracked {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        let stat = current.get(&entry.path);
        if !include_all
            && !states_match_remote(client, stat, expected, &entry.path, &root).await?
        {
            continue;
        }
        let Some(change_type) =
            classify_change_remote(client, stat, &entry.original, &entry.path, &root).await?
        else {
            continue;
        };

        // 进程内 diff 缓存：original 摘要 + 远程 mtime/size 未变时复用。
        let cache_key = format!("{}:{}", checkpoint_id, entry.path);
        let digest = super::original_digest(&entry.original, manifest.git.as_ref(), &entry.path);
        let cached = {
            let cache = super::diff_cache();
            cache.get(&cache_key).and_then(|cached_entry| {
                let stat = stat?;
                (cached_entry.original_digest == digest
                    && cached_entry.current_mtime_ms == stat.mtime_ms
                    && cached_entry.current_size == stat.size)
                    .then_some((cached_entry.content.clone(), cached_entry.is_binary))
            })
        };
        let (content, is_binary) = match cached {
            Some((content, is_binary)) => (content, is_binary),
            None => {
                let original_content =
                    super::read_original_content(&entry.original, manifest.git.as_ref(), &entry.path)?;
                let absolute = resolve_remote_manifest_path(&root, &entry.path);
                let current_content = match stat {
                    Some(_) => client.read_bytes(&absolute).await?,
                    None => None,
                };
                let (content, is_binary) = super::build_unified_diff(
                    &entry.path,
                    original_content.as_deref(),
                    current_content.as_deref(),
                );
                let mut cache = super::diff_cache();
                if cache.len() >= DIFF_CACHE_MAX_ENTRIES {
                    cache.clear();
                }
                cache.insert(
                    cache_key,
                    CachedCheckpointDiff {
                        original_digest: digest,
                        current_mtime_ms: stat.map(|stat| stat.mtime_ms).unwrap_or(0),
                        current_size: stat.map(|stat| stat.size).unwrap_or(0),
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

/// 远程版回滚：把工作区恢复到 checkpoint 记录的 pre-change 状态。
pub(crate) async fn restore_checkpoint_remote(
    client: &RemoteCheckpointClient<'_>,
    checkpoint_id: String,
    work_dir: String,
) -> Result<()> {
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let root_path = PathBuf::from(&root);
    let work_dir_lock = work_dir_lock(&root_path)?;
    let _work_dir_guard = work_dir_write_guard_async(&work_dir_lock).await;
    let manifest_lock = manifest_lock(&checkpoint_id)?;
    let _manifest_guard = manifest_lock.lock().await;
    if !checkpoint_manifest_exists(&checkpoint_id) {
        return Ok(());
    }
    let manifest = read_manifest(&checkpoint_id)?;
    validate_manifest_work_dir_remote(&manifest, &work_dir)?;

    // 当前树 stat：只恢复仍处于 expected 状态的文件（与本地一致）。
    let tree = filter_remote_tree(client, &root).await?;
    let current = remote_stat_map(&tree);

    let mut restored_entries = Vec::new();
    for entry in &manifest.entries {
        if should_skip_manifest_path(&entry.path) {
            continue;
        }
        let destination = resolve_remote_manifest_path(&root, &entry.path);
        let Some(expected) = entry.expected.as_ref() else {
            continue;
        };
        if !states_match_remote(client, current.get(&entry.path), expected, &entry.path, &root)
            .await?
        {
            continue;
        }
        restore_entry_remote(client, entry, &destination).await?;
        restored_entries.push(entry.path.clone());
    }
    prune_empty_parent_directories_remote(client, &root, &restored_entries).await?;
    Ok(())
}

async fn restore_entry_remote(
    client: &RemoteCheckpointClient<'_>,
    entry: &CheckpointEntry,
    destination: &str,
) -> Result<()> {
    match &entry.original {
        OriginalState::Missing => client.delete_file(destination).await,
        OriginalState::Object { object_id } => {
            let source = checkpoint_root()?.join(OBJECT_DIR_NAME).join(object_id);
            let content = fs::read(&source).map_err(|error| {
                Error::from_reason(format!(
                    "Failed to read checkpoint object '{}': {error}",
                    source.display()
                ))
            })?;
            client.write_bytes(destination, &content).await
        }
        OriginalState::Git => Err(Error::from_reason(
            "Checkpoint Git baseline is missing",
        )),
    }
}

/// 回滚后清理空父目录（最深优先，与本地 prune_empty_parent_directories 一致）。
async fn prune_empty_parent_directories_remote(
    client: &RemoteCheckpointClient<'_>,
    root: &str,
    restored_entries: &[String],
) -> Result<()> {
    let mut directories: Vec<String> = restored_entries
        .iter()
        .filter_map(|path| {
            path.rfind('/')
                .map(|index| path[..index].to_string())
                .filter(|parent| !parent.is_empty())
        })
        .collect();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.matches('/').count()));
    directories.dedup();
    let root_uri = root.trim_end_matches('/');
    for directory in directories {
        let mut current = format!("{root_uri}/{directory}");
        loop {
            if current == root_uri || !current.starts_with(&format!("{root_uri}/")) {
                break;
            }
            if !client.remove_dir(&current).await? {
                break;
            }
            let Some(parent) = current.rfind('/') else {
                break;
            };
            current = current[..parent].to_string();
            if current.len() <= root_uri.len() {
                break;
            }
        }
    }
    Ok(())
}

/// 远程版创建 checkpoint：校验远程工作区存在后仅发布本地 manifest
/// （内容在工具执行前后按需捕获，与本地增量语义一致）。
pub(crate) async fn create_checkpoint_remote(
    client: &RemoteCheckpointClient<'_>,
    work_dir: String,
) -> Result<String> {
    let root = canonical_work_dir_remote(client, &work_dir).await?;
    let checkpoint_id = super::generate_checkpoint_id();
    with_manifest_lock_async(&checkpoint_id, || async {
        let manifest = CheckpointManifest {
            version: super::MANIFEST_VERSION,
            work_dir: root,
            git: None,
            entries: Vec::new(),
        };
        write_manifest(&checkpoint_id, &manifest)?;
        Ok(checkpoint_id.clone())
    })
    .await
}
