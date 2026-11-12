//! 上游 `state.ts` + `sync.ts` + `prune.ts`（辅助原语）+ `decompress.ts` +
//! `render-refs.ts`（token 快照）+ `pipeline.ts`（节点注册表）+ `filter/`（运行时注册）。
//!
//! 这一组是「块生命周期与还原原语」「渲染 token 快照」「节点式管线」「过滤器注册表」。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use super::refs;
use super::rendering::summary_text;
use super::{ChatContextMessage, KernelBlock, KernelState};

// ---------------------------------------------------------------------------
// state.ts
// ---------------------------------------------------------------------------

/// 顶层活动块。
pub fn active_blocks(state: &KernelState) -> Vec<&KernelBlock> {
    state
        .blocks
        .iter()
        .filter(|block| block.folded_into.is_empty())
        .collect()
}

pub fn block_by_id<'a>(state: &'a KernelState, block_id: &str) -> Option<&'a KernelBlock> {
    state.blocks.iter().find(|block| block.id == block_id)
}

pub fn block_by_id_mut<'a>(
    state: &'a mut KernelState,
    block_id: &str,
) -> Option<&'a mut KernelBlock> {
    state.blocks.iter_mut().find(|block| block.id == block_id)
}

/// 下一个块 id（`b1`、`b2`…），并推进计数器。
pub fn allocate_block_id(state: &mut KernelState) -> String {
    let next = state.next_block_id.max(1);
    state.next_block_id = next + 1;
    format!("b{next}")
}

/// 下一个运行 id（`r1`、`r2`…），并推进计数器。
pub fn allocate_run_id(state: &mut KernelState) -> String {
    let next = state.next_run_id.max(1);
    state.next_run_id = next + 1;
    format!("r{next}")
}

/// 活动块里的最高层（无活动块 → 0）。
pub fn highest_active_tier(state: &KernelState) -> u8 {
    active_blocks(state)
        .iter()
        .map(|block| block.tier)
        .max()
        .unwrap_or(0)
}

/// 活动块覆盖的原始消息 id 集合。
pub fn covered_message_ids(state: &KernelState) -> HashSet<String> {
    active_blocks(state)
        .iter()
        .flat_map(|block| block.message_ids.iter().cloned())
        .collect()
}

/// 存活轮数推进：每个活动块 +1；返回达到晋升阈值的块 id（上游 `advanceSurvival`）。
pub fn advance_survival(state: &mut KernelState, promotion_threshold: u32) -> Vec<String> {
    let mut ready: Vec<String> = Vec::new();
    for block in state.blocks.iter_mut() {
        if !block.folded_into.is_empty() {
            continue;
        }
        block.survival = block.survival.saturating_add(1);
        if promotion_threshold > 0 && block.survival >= promotion_threshold {
            ready.push(block.id.clone());
        }
    }
    ready
}

// ---------------------------------------------------------------------------
// sync.ts
// ---------------------------------------------------------------------------

/// 同步块与可见消息：覆盖消息已经不存在的块作废（回滚截断 / fork 之后）。
/// 返回被停用的块 id（按 `blocks` 顺序）。
pub fn sync_blocks(state: &mut KernelState, visible_ids: &HashSet<String>) -> Vec<String> {
    let mut deactivated: Vec<String> = Vec::new();
    let before: Vec<String> = state.blocks.iter().map(|block| block.id.clone()).collect();
    state.blocks.retain(|block| {
        let keep = block
            .message_ids
            .iter()
            .all(|id| visible_ids.contains(id));
        if !keep {
            deactivated.push(block.id.clone());
        }
        keep
    });
    // 父块没了 → 子块重新生效（不能留在 folded_into 的死状态里）。
    let removed: HashSet<String> = before
        .into_iter()
        .filter(|id| deactivated.contains(id))
        .collect();
    for block in state.blocks.iter_mut() {
        if removed.contains(&block.folded_into) {
            block.folded_into.clear();
        }
    }
    deactivated
}

/// 停用块（还原）：活动块被移除，其子块重新生效。
///
/// `deep = true` 时连同子块的子块一起递归还原。返回实际停用的块 id。
pub fn deactivate_blocks(
    state: &mut KernelState,
    block_ids: &[String],
    deep: bool,
) -> Vec<String> {
    let mut queue: Vec<String> = block_ids.to_vec();
    let mut removed: Vec<String> = Vec::new();
    while let Some(block_id) = queue.pop() {
        let Some(block) = block_by_id(state, &block_id) else {
            continue;
        };
        let children = block.children.clone();
        state.blocks.retain(|entry| entry.id != block_id);
        removed.push(block_id.clone());
        for child_id in children {
            if let Some(child) = block_by_id_mut(state, &child_id) {
                child.folded_into.clear();
            }
            // 子块原本被父块合并：父块没了就重新生效；deep 时继续下潜。
            let grand_children: Vec<String> = block_by_id(state, &child_id)
                .map(|child| child.children.clone())
                .unwrap_or_default();
            if deep {
                queue.extend(
                    grand_children
                        .into_iter()
                        .filter(|id| block_by_id(state, id).is_some()),
                );
            }
        }
    }
    removed
}

/// 与给定消息集合相交的块（上游 `findBlocksOverlappingMessages`）。
pub fn find_blocks_overlapping_messages<'a>(
    state: &'a KernelState,
    message_ids: &HashSet<String>,
) -> Vec<&'a KernelBlock> {
    state
        .blocks
        .iter()
        .filter(|block| {
            block
                .message_ids
                .iter()
                .any(|id| message_ids.contains(id))
        })
        .collect()
}

/// 块的活动祖先 id（最近一代在前，上游 `activeAncestorIds`）。
pub fn active_ancestor_ids(state: &KernelState, block_id: &str) -> Vec<String> {
    let mut ancestors: Vec<String> = Vec::new();
    let mut current = block_by_id(state, block_id).map(|block| block.folded_into.clone());
    while let Some(parent) = current.filter(|value| !value.is_empty()) {
        let Some(block) = block_by_id(state, &parent) else {
            break;
        };
        if block.folded_into.is_empty() {
            ancestors.push(block.id.clone());
        }
        current = Some(block.folded_into.clone());
    }
    ancestors
}

/// 块的活动祖先（单个，最近一代）。
pub fn find_active_ancestor(state: &KernelState, block_id: &str) -> Option<String> {
    active_ancestor_ids(state, block_id).into_iter().next()
}

// ---------------------------------------------------------------------------
// decompress.ts
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollectedContent {
    /// 渲染好的人读内容（`count == 0` 时为空串）。
    pub text: String,
    /// 渲染的条目数：直接消息 + 嵌套摘要（`full = false`）或全部消息（`full = true`）。
    pub count: usize,
}

/// 把块的内容收成可读字符串，**不改状态**（缓存安全的还原原语：块保持折叠，
/// 摘要留在原位，完整内容作为文本交给调用方）。
///
/// `full = false`（默认）只升一层：嵌套的活动子块保持折叠，用它们的摘要代替原文。
/// `full = true` 递归穿透所有层，渲染每条有效消息的全文。
pub fn collect_block_content(
    state: &KernelState,
    block: &KernelBlock,
    messages: &[ChatContextMessage],
    ref_of: &[String],
    full: bool,
) -> CollectedContent {
    let mut pieces: Vec<String> = Vec::new();
    let index_by_id: HashMap<&str, usize> = ref_of
        .iter()
        .enumerate()
        .map(|(index, reference)| (reference.as_str(), index))
        .collect();

    let render_index = |index: usize, out: &mut Vec<String>| {
        if let Some(message) = messages.get(index) {
            let reference = ref_of.get(index).cloned().unwrap_or_default();
            out.push(super::rendering::render_message(
                message,
                (!reference.is_empty()).then_some(reference.as_str()),
            ));
        }
    };

    // 该块直接覆盖的消息（未被任何活动子块覆盖的）。
    let nested: HashSet<String> = state
        .blocks
        .iter()
        .filter(|entry| entry.folded_into == block.id && !full)
        .flat_map(|entry| entry.message_ids.iter().cloned())
        .collect();

    for id in block.message_ids.iter() {
        if nested.contains(id) {
            continue;
        }
        if let Some(index) = index_by_id.get(id.as_str()).copied() {
            render_index(index, &mut pieces);
        }
    }
    // 嵌套的活动子块：full=false 渲染摘要；full=true 已经展开过原文，跳过。
    if !full {
        for child in state
            .blocks
            .iter()
            .filter(|entry| entry.folded_into == block.id)
        {
            pieces.push(summary_text(child));
        }
    }
    let count = pieces.len();
    CollectedContent {
        text: pieces.join("\n\n"),
        count,
    }
}

/// 内联还原标记（refold-in-place）：块保持 ACTIVE，ref 与 id 都不移动，只翻标记，
/// 因此 ref 不可变性成立。返回 `(restored, blockId, startRef, endRef)`。
pub fn mark_block_restored_inline(
    state: &mut KernelState,
    block_id: &str,
) -> Option<(bool, String, String, String)> {
    let block = block_by_id_mut(state, block_id)?;
    block.restored_inline = true;
    Some((
        true,
        block.id.clone(),
        block.start_ref.clone(),
        block.end_ref.clone(),
    ))
}

/// 还原预览：`before_active_message_ids` 是还原**前**的活动覆盖集合。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoredPreview {
    pub preview: String,
    pub restored_count: usize,
}

pub fn build_restored_content_preview(
    messages: &[ChatContextMessage],
    before_active_message_ids: &HashSet<String>,
    ref_of: &[String],
    hash_of: impl Fn(&ChatContextMessage) -> String,
) -> RestoredPreview {
    let mut lines: Vec<String> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let hash = hash_of(message);
        if !before_active_message_ids.contains(&hash) {
            continue;
        }
        let reference = ref_of.get(index).cloned().unwrap_or_default();
        lines.push(super::rendering::render_message(
            message,
            (!reference.is_empty()).then_some(reference.as_str()),
        ));
    }
    let restored_count = lines.len();
    RestoredPreview {
        preview: lines.join("\n\n"),
        restored_count,
    }
}

// ---------------------------------------------------------------------------
// prune.ts（辅助原语）
// ---------------------------------------------------------------------------

/// 是否是内核渲染出的摘要消息（**精确形状**，不只是 id 前缀）。
///
/// Snow 侧的摘要是内核注入的内部消息，首行以 `SUMMARY_PREFIX` 开头；只有形状完全
/// 吻合才算，宿主自己写的、恰好沿用前缀的消息不会被误判。
pub fn is_rendered_summary_message(message: &ChatContextMessage) -> bool {
    message.role.trim() == "user"
        && message.origin == crate::storage::services::chat_conversations::ORIGIN_INTERNAL
        && message.content.starts_with(super::SUMMARY_PREFIX)
}

/// 消息 id 的基 id（第一个 `#` 之前）。
pub fn base_id_of(id: &str) -> &str {
    refs::base_id_of(id)
}

/// `id` 是否落在任一被覆盖的原始消息之下（覆盖集合必须是由 `baseIdOf` 归一化后的集合）。
pub fn is_covered(id: &str, covered_bases: &HashSet<String>) -> bool {
    covered_bases.contains(refs::base_id_of(id))
}

// ---------------------------------------------------------------------------
// render-refs.ts（token 快照）
// ---------------------------------------------------------------------------

/// 带稳定 token 快照的渲染：token 数**首次渲染时写入并永久复用**（按 ref 索引）。
/// 快照以持久状态为起点，因此老条目会存活，新条目在本次渲染中补上。
pub fn render_with_snapshot(
    messages: &[ChatContextMessage],
    ref_of: &[String],
    snapshot: &mut HashMap<String, i64>,
    count_tokens: impl Fn(&str) -> i64,
) -> i64 {
    let mut total = 0i64;
    for (index, message) in messages.iter().enumerate() {
        let Some(reference) = ref_of.get(index).filter(|value| !value.is_empty()) else {
            total += count_tokens(&message.content);
            continue;
        };
        let tokens = *snapshot
            .entry(reference.clone())
            .or_insert_with(|| count_tokens(&message.content));
        total += tokens;
    }
    total
}

// ---------------------------------------------------------------------------
// pipeline.ts（节点注册表）
// ---------------------------------------------------------------------------

/// 管线上下文（节点执行期的只读输入 + 可累加效果）。
#[derive(Clone, Debug, Default)]
pub struct PipelineEffects {
    pub nudge: bool,
    pub recommendation: bool,
    pub truncated_count: usize,
    pub terminal_escape: bool,
    pub truncation_skipped: String,
    pub extras: HashMap<String, i64>,
}

/// 管线节点：名字 + 变换闭包 + 可选的启用判定。
pub struct PipelineNode {
    pub name: String,
    pub run: Box<dyn Fn(Vec<ChatContextMessage>, &mut PipelineEffects) -> Vec<ChatContextMessage>>,
    pub enabled: Option<Box<dyn Fn(&[ChatContextMessage]) -> bool>>,
}

impl std::fmt::Debug for PipelineNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipelineNode")
            .field("name", &self.name)
            .finish()
    }
}

/// 按序执行节点；每个节点的效果累加进同一个 `PipelineEffects`。
pub fn run_pipeline(
    nodes: &[PipelineNode],
    messages: Vec<ChatContextMessage>,
    effects: &mut PipelineEffects,
) -> Vec<ChatContextMessage> {
    let mut current = messages;
    for node in nodes {
        if let Some(enabled) = node.enabled.as_ref() {
            if !enabled(&current) {
                continue;
            }
        }
        current = (node.run)(current, effects);
    }
    current
}

/// 默认节点名（与 `porting::DEFAULT_PIPELINE_NODES` 同序）。
pub fn default_pipeline_node_names() -> Vec<String> {
    super::porting::DEFAULT_PIPELINE_NODES
        .iter()
        .map(|name| name.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// filter/index.ts（运行时注册表）
// ---------------------------------------------------------------------------

/// 部件过滤器注册表。
pub struct FilterRegistry {
    filters: Mutex<Vec<(String, Box<dyn Fn(&str) -> Option<String> + Send + Sync>)>>,
}

impl Default for FilterRegistry {
    fn default() -> Self {
        Self {
            filters: Mutex::new(Vec::new()),
        }
    }
}

impl FilterRegistry {
    pub fn register<F>(&self, name: &str, filter: F)
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        let Ok(mut filters) = self.filters.lock() else {
            return;
        };
        if let Some(existing) = filters.iter_mut().find(|(id, _)| id == name) {
            existing.1 = Box::new(filter);
            return;
        }
        filters.push((name.to_string(), Box::new(filter)));
    }

    pub fn unregister(&self, name: &str) -> bool {
        let Ok(mut filters) = self.filters.lock() else {
            return false;
        };
        let before = filters.len();
        filters.retain(|(id, _)| id != name);
        filters.len() != before
    }

    pub fn list(&self) -> Vec<String> {
        self.filters
            .lock()
            .map(|filters| filters.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut filters) = self.filters.lock() {
            filters.clear();
        }
    }

    /// 就地跑一遍所有过滤器，返回 (丢弃数, 修改数)。
    pub fn apply(&self, parts: &mut Vec<String>) -> (usize, usize) {
        let Ok(filters) = self.filters.lock() else {
            return (0, 0);
        };
        let mut dropped = 0usize;
        let mut modified = 0usize;
        for (_, filter) in filters.iter() {
            let mut next: Vec<String> = Vec::with_capacity(parts.len());
            for part in parts.iter() {
                match filter(part) {
                    Some(out) => {
                        if &out != part {
                            modified += 1;
                        }
                        next.push(out);
                    }
                    None => dropped += 1,
                }
            }
            *parts = next;
        }
        (dropped, modified)
    }
}
