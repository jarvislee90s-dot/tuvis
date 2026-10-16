//! Revision Mirror：官方 Desktop `thread-stream-state-changed`（v11）的镜像与
//! Tuvis 状态投影（spec 3.3：IPC 原始 state 不直接进入远程 API）。
//!
//! 纪律：
//! * mirror 绑定当前 lease；换 owner（lease 身份变化）**先清状态**再重绑，
//!   禁止仅由调用方口头保证；
//! * `turns=[]` 是常态——正文以 `turnHistory.history.entitiesByKey/islands`
//!   为正史，按 islands[].entries 顺序取（不遍历无序 map 当时间序）；
//! * patch 严格校验（数组 add=插入 / remove=删除 / replace=只替换现有索引），
//!   一批中任一 op 非法 → 整批原子拒绝 → stale + 重拉 snapshot；
//! * 低 revision 重放忽略；revision 缺口 → stale + need_snapshot；
//!   新 epoch（换代后）不与旧 revision 比大小（状态已清）；
//! * 预算：单 Thread 镜像 16 MiB；投影正文尾窗 1 MiB / 500 条并标 truncated。

use super::pending::PendingProjector;
use super::types::{
    CapabilitySet, ControlAvailability, OwnerLease, ProjectedMessage, ProtocolError, ThreadKey,
    ThreadProjection,
};
use serde_json::Value;

/// 单 Thread 镜像字节预算。
pub const MIRROR_BUDGET: usize = 16 * 1024 * 1024;
/// 投影正文最大条数。
pub const PROJECTION_MAX_MESSAGES: usize = 500;
/// 投影正文尾窗字节预算。
pub const PROJECTION_TAIL_BYTES: usize = 1024 * 1024;

/// apply 的效果：`changed` 是否产生新状态；`need_snapshot` 是否需要重拉快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorEffect {
    pub changed: bool,
    pub need_snapshot: bool,
}

impl MirrorEffect {
    const QUIET: Self = Self {
        changed: false,
        need_snapshot: false,
    };
}

pub struct ThreadMirror {
    key: ThreadKey,
    lease: Option<OwnerLease>,
    revision: Option<u64>,
    stale: bool,
    /// change 根（含 conversationState）；patch 相对该根寻址
    state: Option<Value>,
    pending: PendingProjector,
    /// turnHistory.isComplete（不完整时 adapter 决定是否调 load-complete-history）
    history_complete: bool,
}

impl ThreadMirror {
    pub fn new(key: ThreadKey) -> Self {
        Self {
            key,
            lease: None,
            revision: None,
            stale: false,
            state: None,
            pending: PendingProjector::new(),
            history_complete: true,
        }
    }

    pub fn key(&self) -> &ThreadKey {
        &self.key
    }

    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn is_history_complete(&self) -> bool {
        self.history_complete
    }

    /// 标记过期（连接断开/缺口/预算超限）。保留最后视图但禁写（adapter 侧门）。
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    fn clear_state(&mut self) {
        self.revision = None;
        self.state = None;
        self.stale = true;
        self.history_complete = true;
        self.pending.reset();
    }

    /// 应用一帧 `thread-stream-state-changed` 的 change。
    pub fn apply(
        &mut self,
        lease: &OwnerLease,
        change: &Value,
    ) -> Result<MirrorEffect, ProtocolError> {
        // 换 owner：先清状态再重绑（世代不同一概不比较 revision）
        if self.lease.as_ref().is_none_or(|l| {
            l.epoch != lease.epoch
                || l.generation != lease.generation
                || l.client_id != lease.client_id
        }) {
            self.clear_state();
            self.lease = Some(lease.clone());
        }

        let change_type = change.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match change_type {
            "snapshot" => self.apply_snapshot(change),
            "patches" => self.apply_patches(change),
            _ => {
                // 未知 change 形态：保守 stale + 重拉（不猜测语义）
                self.stale = true;
                Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                })
            }
        }
    }

    fn apply_snapshot(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let Some(rev) = rev else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 低/等 revision 重放忽略（新绑定 revision=None 时接受任意）
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        // 存整个 change 根：patch 路径以 conversationState 起头（change 根寻址）
        let state = change
            .get("conversationState")
            .map_or(Value::Null, |_| change.clone());
        self.adopt(rev, state, is_complete(change))?;
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    fn apply_patches(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let base = change.get("baseRevision").and_then(|r| r.as_u64());
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let patches = change.get("patches").and_then(|p| p.as_array()).cloned();
        let (Some(base), Some(rev), Some(patches)) = (base, rev, patches) else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 重放：低于等于当前 revision 的旧批直接忽略
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        let Some(cur) = self.revision else {
            // 尚无快照基线：缺口
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        if base != cur {
            // revision 缺口/乱序：整批不应用，stale + 重拉
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        }
        // 原子应用：clone 上逐条 op，任一失败整批作废
        let Some(state) = self.state.clone() else {
            self.stale = true;
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        let mut next = state;
        for op in &patches {
            if apply_op(&mut next, op).is_err() {
                self.stale = true;
                return Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                });
            }
        }
        self.adopt(rev, next, self.history_complete)?;
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    /// 提交新状态（预算校验；超限 stale + 拒绝，不冒充无 owner）。
    fn adopt(
        &mut self,
        rev: u64,
        state: Value,
        history_complete: bool,
    ) -> Result<(), ProtocolError> {
        let size = serde_json::to_vec(&state)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MIRROR_BUDGET {
            self.stale = true;
            return Err(ProtocolError::Capacity(format!(
                "mirror exceeds per-thread budget: {size} > {MIRROR_BUDGET}"
            )));
        }
        self.revision = Some(rev);
        self.state = Some(state);
        self.history_complete = history_complete;
        self.stale = false;
        Ok(())
    }

    /// 统一状态投影（远程 API / SSE / 移动端唯一消费形态）。
    /// token 缓存存于 PendingProjector 内部互斥锁，`&self` 即可投影。
    pub fn project(&self, caps: CapabilitySet) -> ThreadProjection {
        let root = self.state.clone().unwrap_or(Value::Null);
        let state = root
            .get("conversationState")
            .cloned()
            .unwrap_or(Value::Null);
        let messages = project_messages(&state);
        let requests = state
            .get("requests")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let pending = self.pending.project(&requests, &caps);
        ThreadProjection {
            thread_id: self.key.thread_id.clone(),
            revision: self.revision,
            stale: self.stale,
            availability: availability_of(&caps),
            capabilities: caps,
            runtime_status: runtime_status(&state),
            truncated: messages.1,
            messages: messages.0,
            pending,
        }
    }
}

fn availability_of(caps: &CapabilitySet) -> ControlAvailability {
    if caps.send && caps.state {
        ControlAvailability::Controllable
    } else if caps.send {
        ControlAvailability::SendOnly
    } else {
        ControlAvailability::CompatOnly
    }
}

/// runtime status：threadRuntimeStatus.type + turn 状态综合；缺字段 unknown。
fn runtime_status(state: &Value) -> String {
    let declared = state
        .get("threadRuntimeStatus")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str());
    match declared {
        Some("active") => return "active".into(),
        Some("systemError") => return "failed".into(),
        Some("idle") => {}
        Some("notLoaded") => {}
        Some(_) => return "unknown".into(),
        None => {}
    }
    // idle/缺省时看 turn 历史：有 inProgress → active；有 failed → failed
    if let Some(entities) = state
        .get("turnHistory")
        .and_then(|h| h.get("history"))
        .and_then(|h| h.get("entitiesByKey"))
        .and_then(|e| e.as_object())
    {
        for entity in entities.values() {
            if entity.get("type").and_then(|t| t.as_str()) == Some("turn")
                && entity.get("status").and_then(|s| s.as_str()) == Some("inProgress")
            {
                return "active".into();
            }
        }
    }
    if declared == Some("idle") {
        "idle".into()
    } else {
        "unknown".into()
    }
}

fn is_complete(change: &Value) -> bool {
    change
        .get("conversationState")
        .and_then(|s| s.get("turnHistory"))
        .and_then(|h| h.get("isComplete"))
        .and_then(|b| b.as_bool())
        .unwrap_or(true)
}

/// 按正史顺序投影消息，尾窗 1 MiB / 500 条，超出标 truncated。
/// 返回 (消息, 是否截断)。
fn project_messages(state: &Value) -> (Vec<ProjectedMessage>, bool) {
    let entities = state
        .get("turnHistory")
        .and_then(|h| h.get("history"))
        .and_then(|h| h.get("entitiesByKey"));
    let Some(entities) = entities else {
        return (Vec::new(), false);
    };
    let islands = state
        .get("turnHistory")
        .and_then(|h| h.get("history"))
        .and_then(|h| h.get("islands"))
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();

    // 按 islands[].entries 顺序取 turn id（不遍历无序 map 当时间序）
    let mut all: Vec<ProjectedMessage> = Vec::new();
    for island in &islands {
        let Some(entries) = island.get("entries").and_then(|e| e.as_array()) else {
            continue;
        };
        for turn_id in entries {
            let Some(turn_id) = turn_id.as_str() else {
                continue;
            };
            let Some(turn) = entities.get(turn_id) else {
                continue; // 严格：缺实体跳过，不猜
            };
            if turn.get("type").and_then(|t| t.as_str()) != Some("turn") {
                continue;
            }
            let Some(item_ids) = turn.get("items").and_then(|i| i.as_array()) else {
                continue;
            };
            for item_id in item_ids {
                let Some(item_id) = item_id.as_str() else {
                    continue;
                };
                let Some(item) = entities.get(item_id) else {
                    continue;
                };
                if let Some(msg) = project_item(item_id, item) {
                    all.push(msg);
                }
            }
        }
    }

    // 尾窗：500 条 / 1 MiB
    let mut truncated = false;
    if all.len() > PROJECTION_MAX_MESSAGES {
        all = all.split_off(all.len() - PROJECTION_MAX_MESSAGES);
        truncated = true;
    }
    let mut total: usize = 0;
    let mut cut = 0usize;
    for (i, m) in all.iter().rev().enumerate() {
        total += m.text.len();
        if total > PROJECTION_TAIL_BYTES {
            cut = all.len() - i;
            truncated = true;
            break;
        }
    }
    if cut > 0 {
        all = all.split_off(cut);
    }
    (all, truncated)
}

/// 单条 item → 投影消息。已知文本类（user_message/agent_message）直出；
/// 未知 kind 只给安全文字摘要，不下发原始参数。
fn project_item(id: &str, item: &Value) -> Option<ProjectedMessage> {
    let kind = item.get("type").and_then(|t| t.as_str())?;
    let raw = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let (role, text) = match kind {
        "user_message" => ("user", raw.to_string()),
        "agent_message" => ("assistant", raw.to_string()),
        // 未知 kind 只投影安全文字摘要，不下发原始工具参数
        other => ("system", format!("[非文本项：{other}]")),
    };
    Some(ProjectedMessage {
        id: id.to_string(),
        role: role.to_string(),
        kind: kind.to_string(),
        text,
        ts: item.get("ts").and_then(|t| t.as_i64()),
    })
}

/// 单个 patch op（严格形态）：add=数组插入/对象新增，replace=只替换现有，
/// remove=删除；path 数组元素为对象键（字符串）或数组下标（整数）。
fn apply_op(root: &mut Value, op: &Value) -> Result<(), ProtocolError> {
    let op_name = op
        .get("op")
        .and_then(|o| o.as_str())
        .ok_or_else(|| ProtocolError::InvalidState("patch op missing".into()))?;
    let path: Vec<&Value> = op
        .get("path")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().collect())
        .ok_or_else(|| ProtocolError::InvalidState("patch path missing".into()))?;
    if path.is_empty() {
        return Err(ProtocolError::InvalidState("patch path empty".into()));
    }
    let value = op.get("value").cloned();

    // 逐级下行（父节点必须存在）
    let mut node = root;
    for seg in &path[..path.len() - 1] {
        node = descend(node, seg)?;
    }
    let last = path[path.len() - 1];
    match op_name {
        "add" => {
            if let Some(idx) = index_of(last)? {
                if !node.is_array() {
                    return Err(ProtocolError::InvalidState("add into non-array".into()));
                }
                let arr = node.as_array_mut().expect("checked");
                if idx > arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "array add index {idx} > len {}",
                        arr.len()
                    )));
                }
                arr.insert(idx, value.unwrap_or(Value::Null));
                Ok(())
            } else {
                let key = key_of(last)?;
                if !node.is_object() {
                    return Err(ProtocolError::InvalidState("add into non-object".into()));
                }
                node.as_object_mut()
                    .expect("checked")
                    .insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "replace" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr[idx] = value.unwrap_or(Value::Null);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-object".into()))?;
                if !obj.contains_key(&key) {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace missing key {key}"
                    )));
                }
                obj.insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "remove" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr.remove(idx);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-object".into()))?;
                if obj.remove(&key).is_none() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove missing key {key}"
                    )));
                }
                Ok(())
            }
        }
        other => Err(ProtocolError::InvalidState(format!(
            "unknown patch op: {other}"
        ))),
    }
}

fn descend<'a>(node: &'a mut Value, seg: &Value) -> Result<&'a mut Value, ProtocolError> {
    if let Some(idx) = index_of(seg)? {
        let arr = node
            .as_array_mut()
            .ok_or_else(|| ProtocolError::InvalidState("descend into non-array".into()))?;
        arr.get_mut(idx)
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend index {idx} missing")))
    } else {
        let key = key_of(seg)?;
        node.get_mut(key.as_str())
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend key {key} missing")))
    }
}

fn index_of(seg: &Value) -> Result<Option<usize>, ProtocolError> {
    match seg {
        Value::Number(n) => Ok(Some(
            n.as_u64()
                .ok_or_else(|| ProtocolError::InvalidState("array index must be u64".into()))?
                as usize,
        )),
        Value::String(_) => Ok(None),
        _ => Err(ProtocolError::InvalidState(
            "path segment must be string or number".into(),
        )),
    }
}

fn key_of(seg: &Value) -> Result<String, ProtocolError> {
    match seg {
        Value::String(s) => Ok(s.clone()),
        _ => Err(ProtocolError::InvalidState(
            "object key must be string".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::{PendingKind, ThreadKey};

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "00000000-0000-4000-8000-0000000000m1".into(),
        }
    }

    fn lease() -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "owner-1".into(),
            epoch: 1,
            generation: 1,
        }
    }

    fn caps() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: true,
            blocking_input: false,
        }
    }

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/codex-desktop-ipc/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    /// macos-snapshot.json 的 change 段（apply 的入参形态）。
    fn macos_change() -> Value {
        fixture("macos-snapshot.json")["params"]["change"].clone()
    }

    #[test]
    fn empty_turns_uses_canonical_history() {
        let mut m = ThreadMirror::new(key());
        let eff = m.apply(&lease(), &macos_change()).unwrap();
        assert!(eff.changed);
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        // turns=[] 是常态：正文来自 turnHistory 正史（用户+助手两条，稳定 item ID）
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].id, "itm-u1");
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[1].id, "itm-a1");
        assert_eq!(p.messages[1].role, "assistant");
        // 完整 pending：permissions 请求（macOS profile permissions=true → actionable）
        assert_eq!(p.pending.len(), 1);
        assert_eq!(p.pending[0].kind, PendingKind::PermissionsApproval);
        assert!(p.pending[0].actionable);
        assert_eq!(p.pending[0].request_key, "n:12");
        assert_eq!(p.runtime_status, "idle");
        assert_eq!(p.revision, Some(209));
        assert!(!p.stale);
    }

    #[test]
    fn patch_batch_is_atomic() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 一批：第一条合法（replace 文本）+ 第二条非法（replace 不存在的键）
        let batch = serde_json::json!({
            "type": "patches",
            "baseRevision": 209,
            "revision": 210,
            "patches": [
                { "op": "replace",
                  "path": ["conversationState", "turnHistory", "history", "entitiesByKey", "itm-a1", "text"],
                  "value": "更新" },
                { "op": "replace",
                  "path": ["conversationState", "noSuchKey"],
                  "value": 1 }
            ]
        });
        let eff = m.apply(&lease(), &batch).unwrap();
        assert!(!eff.changed, "非法批次不得部分生效");
        assert!(eff.need_snapshot);
        assert!(m.is_stale());
        // 原子性：第一条的效果也不在
        let p = m.project(caps());
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK");
    }

    #[test]
    fn revision_gap_requires_snapshot() {
        let mut m = ThreadMirror::new(key());
        // 先落到 revision=7
        let seed = serde_json::json!({
            "type": "snapshot", "revision": 7, "conversationState": { "turns": [], "requests": [] }
        });
        m.apply(&lease(), &seed).unwrap();
        // 在 revision=7 的镜像上，baseRevision=6 / revision=8 的增量：缺口不应用
        let gap = serde_json::json!({
            "type": "patches", "baseRevision": 6, "revision": 8, "patches": []
        });
        let eff = m.apply(&lease(), &gap).unwrap();
        assert!(eff.need_snapshot);
        assert!(!eff.changed);
        assert!(m.project(caps()).stale);
        assert_eq!(m.revision(), Some(7), "缺口批不推进 revision");
    }

    #[test]
    fn stale_replays_are_ignored_quietly() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 低 revision 的重复 snapshot / patch 静默忽略
        let old_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 100, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease(), &old_snapshot).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
        assert_eq!(m.revision(), Some(209), "旧 snapshot 不回退镜像");
        let old_patch = serde_json::json!({
            "type": "patches", "baseRevision": 209, "revision": 209, "patches": []
        });
        let eff = m.apply(&lease(), &old_patch).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
    }

    #[test]
    fn new_epoch_clears_state_without_revision_comparison() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.revision(), Some(209));
        // 换代 lease（epoch=2）：不比较旧 revision，先清状态等新 snapshot
        let mut lease2 = lease();
        lease2.epoch = 2;
        lease2.generation = 2;
        let old_revision_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 5, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease2, &old_revision_snapshot).unwrap();
        assert!(eff.changed, "新 epoch 接受任意 revision 的首个 snapshot");
        assert_eq!(m.revision(), Some(5), "换代后不与旧 revision 比大小");
        assert!(!m.is_stale());
    }

    #[test]
    fn streaming_replace_updates_same_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        let patches = fixture("patches.json")["batches"][0].clone();
        let eff = m.apply(&lease(), &patches).unwrap();
        assert!(eff.changed);
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "流式 replace 不追加重复气泡");
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK（流式追加）");
        assert_eq!(p.messages[1].id, "itm-a1", "item ID 稳定");
        // batch[1]：新增 turn；batch[3]：remove pending（跳过 batch[2] 缺口样本）
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [1usize, 3] {
            let eff = m.apply(&lease(), &batches[idx]).unwrap();
            assert!(eff.changed, "顺序批 {idx} 应可应用");
        }
        let p = m.project(caps());
        assert!(p.pending.is_empty(), "remove 后 pending 消失");
        assert_eq!(p.revision, Some(212));
    }

    #[test]
    fn projection_tail_window_marks_truncated() {
        // 构造 600 条消息的快照：截到 500 条并标 truncated
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..600 {
            let turn_id = format!("t{i}");
            let item_id = format!("i{i}");
            entries.push(serde_json::json!(turn_id));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "id": turn_id, "type": "turn", "status": "completed",
                                    "items": [item_id] }),
            );
            entities.insert(
                item_id.clone(),
                serde_json::json!({ "id": item_id, "type": "user_message", "text": "m", "ts": i }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "isComplete": true,
                    "history": { "entitiesByKey": Value::Object(entities),
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 500);
        assert!(p.truncated);
        assert_eq!(p.messages[0].id, "i100", "保留尾窗（最新 500 条）");
        assert_eq!(p.messages[499].id, "i599");
    }

    #[test]
    fn oversized_mirror_is_rejected_and_stale() {
        let blob = "x".repeat(MIRROR_BUDGET + 1);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": { "turns": [], "requests": [], "big": blob }
        });
        let mut m = ThreadMirror::new(key());
        let err = m.apply(&lease(), &change).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(m.is_stale());
    }

    #[test]
    fn unknown_item_kind_projects_safe_summary_only() {
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "isComplete": true,
                    "history": {
                        "entitiesByKey": {
                            "t1": { "id": "t1", "type": "turn", "status": "completed",
                                    "items": ["raw1"] },
                            "raw1": { "id": "raw1", "type": "mcp_tool_call",
                                      "arguments": { "cmd": "secret-cmd", "token": "TKN" } }
                        },
                        "islands": [{ "entries": ["t1"] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, "system");
        assert_eq!(p.messages[0].kind, "mcp_tool_call");
        let rendered = serde_json::to_string(&p.messages[0]).unwrap();
        assert!(!rendered.contains("secret-cmd"), "原始参数不下发");
        assert!(!rendered.contains("TKN"), "参数值不下发");
    }
}
