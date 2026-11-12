//! 上游 `refs.ts`：消息引用（`mNNNNN`）的分配与查询。
//!
//! 与上游一致：ref 是**按消息顺序**编号的 5 位十进制引用（`m00001`），索引 1 起，
//! 上限 9,999,999。宿主侧另有内容哈希（`stableId`）用于「块覆盖了哪些消息」这类
//! 需要跨重排稳定的判断；两者分工明确：
//!
//! - **ref**：给模型看、给工具用、给边界解析用。按序分配，前缀缓存友好。
//! - **hash**：块的持久覆盖集合。历史被回滚、重排、重试后依然能对上。
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（`refs.ts`，MIT）。

use std::collections::BTreeMap;

/// ref 索引上限（上游 `indexToRef` 的边界）。
pub const MAX_REF_INDEX: usize = 9_999_999;

/// 摘要消息 id 前缀（`acp_summary_b3`）。
pub const SUMMARY_ID_PREFIX: &str = "acp_summary_";

/// 取回注入消息 id 前缀（`acp_retrieved_...`）。
pub const RETRIEVED_ID_PREFIX: &str = "acp_retrieved_";

/// 索引 → ref。索引 0 或越界返回 `None`（上游会抛 RangeError）。
pub fn index_to_ref(index: usize) -> Option<String> {
    if index == 0 || index > MAX_REF_INDEX {
        return None;
    }
    Some(format!("m{index:05}"))
}

/// ref → 索引。只接受 `m` + 十进制数字；`m00000` 与越界返回 `None`。
pub fn ref_to_index(reference: &str) -> Option<usize> {
    let digits = reference.trim().strip_prefix('m')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index: usize = digits.parse().ok()?;
    if index == 0 || index > MAX_REF_INDEX {
        return None;
    }
    Some(index)
}

/// 消息 id 的基 id：第一个 `#` 之前的部分。
///
/// 子投影（`base#callId`、`base#r0`）是同一原始消息的另一种渲染，覆盖判断必须
/// 归一化到 base（上游 issue #231）。
pub fn base_id_of(id: &str) -> &str {
    match id.find('#') {
        Some(position) => &id[..position],
        None => id,
    }
}

/// 活动块渲染出的摘要消息的可见 id（**仅视图用**，绝不写回持久覆盖集合）。
pub fn summary_message_id(block_id: &str) -> String {
    format!("{SUMMARY_ID_PREFIX}{block_id}")
}

pub fn is_summary_message_id(id: &str) -> bool {
    id.starts_with(SUMMARY_ID_PREFIX)
}

pub fn retrieved_message_id(reference: &str) -> String {
    format!("{RETRIEVED_ID_PREFIX}{reference}")
}

pub fn is_retrieved_message_id(id: &str) -> bool {
    id.starts_with(RETRIEVED_ID_PREFIX)
}

/// 上游 `MessageRefMap`：`byRaw`（原始 id → ref）与 `byRef`（ref → 原始 id）。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessageRefMap {
    pub by_raw: BTreeMap<String, String>,
    pub by_ref: BTreeMap<String, String>,
}

pub fn empty_ref_map() -> MessageRefMap {
    MessageRefMap::default()
}

pub fn ref_for_raw(map: &MessageRefMap, raw_id: &str) -> Option<String> {
    map.by_raw.get(raw_id).cloned()
}

pub fn raw_for_ref(map: &MessageRefMap, reference: &str) -> Option<String> {
    map.by_ref.get(reference).cloned()
}

/// 已用的最大 ref 索引（`0` 表示空表）。
pub fn highest_used_index(map: &MessageRefMap) -> usize {
    map.by_ref
        .keys()
        .filter_map(|reference| ref_to_index(reference))
        .max()
        .unwrap_or(0)
}

/// 重建 `byRef` 索引（`byRaw` 是权威方向；修复手改过的表）。
pub fn rebuild_ref_index(map: &mut MessageRefMap) {
    let mut by_ref = BTreeMap::new();
    for (raw, reference) in map.by_raw.iter() {
        by_ref.insert(reference.clone(), raw.clone());
    }
    map.by_ref = by_ref;
}

/// 为尚未分配 ref 的消息补发 ref。返回新分配的条数。
///
/// `is_protected` / `should_skip` 为真时不分配（上游 `assignRefs` 的 dt 语义）。
pub fn assign_refs<F, G>(
    raw_ids: &[String],
    map: &mut MessageRefMap,
    mut next_index: usize,
    is_protected: F,
    should_skip: G,
) -> usize
where
    F: Fn(usize) -> bool,
    G: Fn(usize) -> bool,
{
    let mut assigned = 0usize;
    for (position, raw) in raw_ids.iter().enumerate() {
        if map.by_raw.contains_key(raw) {
            continue;
        }
        if is_protected(position) || should_skip(position) {
            continue;
        }
        let Some(reference) = index_to_ref(next_index) else {
            break;
        };
        next_index += 1;
        map.by_raw.insert(raw.clone(), reference.clone());
        map.by_ref.insert(reference, raw.clone());
        assigned += 1;
    }
    assigned
}
