//! 持久动作串行调度（Task 7）：`ActionStore` 包装 `DeviceStore`，在 `codex_actions`
//! 表上实现动作登记/派发/落账、普通消息持久 FIFO 与重启恢复，以及 PR #117 兼容
//! fence 的读写与只读核对（reconcile）。
//!
//! 锁纪律：`DeviceStore::with()` 不可重入——本类型每个公开方法恰好一次 `with()`
//! （SQL 全在闭包内完成），**绝不跨 await 持 DB 锁**（调度等待在 `CodexControlRouter`
//! 的 tokio 锁上）。DB 失败一律 `StoreError::Database` 向上传播，调用方（router）
//! 据此零投递。
use std::collections::BTreeMap;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::database::dao::codex_actions as dao;
use crate::inject::headless::codex::{production_queue_holds, QueueEvidence};
use crate::remote::pairing::{self, DeviceStore};
use crate::services::codex_desktop_ipc::types::{
    new_uuid, ActionKind, ActionReceipt, ActionState, CodexActionView, ControlRoute, ProtocolError,
    ThreadKey, ThreadProjection,
};

use super::ActionContext;

// ============================================================
// 错误与载荷哈希
// ============================================================

/// 动作账本错误。`Conflict` = 同 action_id 重放但载荷/设备/会话不一致、CAS 失败、
/// 非法状态跃迁（如 reserved 未终态就清 fence）；`Database` = DB 失败（零投递语义）。
/// 开发者面错误（HTTP 层映射在 Task 8），无用户可见文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Conflict,
    Database(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => write!(f, "codex action conflict"),
            Self::Database(e) => write!(f, "codex action store db error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 参与 payload hash 的载荷（审批答案只存 hash 不存原文——账本同规）。
/// `Answers` 用 `BTreeMap` 保证序列化按键排序（有序答案规范序列化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPayload {
    Text(String),
    Answers(BTreeMap<String, Vec<String>>),
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn kind_str(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Send => "send",
        ActionKind::CommandApproval => "command_approval",
        ActionKind::PermissionsApproval => "permissions_approval",
        ActionKind::UserInput => "user_input",
    }
}

fn kind_from_str(s: &str) -> Option<ActionKind> {
    match s {
        "send" => Some(ActionKind::Send),
        "command_approval" => Some(ActionKind::CommandApproval),
        "permissions_approval" => Some(ActionKind::PermissionsApproval),
        "user_input" => Some(ActionKind::UserInput),
        _ => None,
    }
}

/// 动作载荷指纹（计划 Task 7 明文：现有 sha2 对 kind/thread/requestKey/contextToken/
/// 原文或有序答案做**规范化序列化**——不能只 hash 文本，否则换 thread 重放同文本
/// 会被误判幂等）。canonical JSON（键排序、无空白）后 sha256 hex。
pub fn payload_hash(
    kind: ActionKind,
    key: &ThreadKey,
    request_key: Option<&str>,
    context_token: Option<&str>,
    payload: &ActionPayload,
) -> String {
    let payload_json = match payload {
        ActionPayload::Text(t) => serde_json::json!({ "type": "text", "text": t }),
        ActionPayload::Answers(a) => serde_json::json!({ "type": "answers", "answers": a }),
    };
    let doc = serde_json::json!({
        "kind": kind_str(kind),
        "thread": { "hostId": key.host_id, "threadId": key.thread_id },
        "requestKey": request_key,
        "contextToken": context_token,
        "payload": payload_json,
    });
    sha256_hex(doc.to_string().as_bytes())
}

/// 已组装文本（wire 文本）的 hash——兼容兜底 bridge 在记录 rollout 基线时一并登记，
/// 供 reconcile 在 rollout 追加段中唯一匹配该次用户输入。与 [`payload_hash`]（账本
/// 指纹，含 kind/thread）用途不同：本函数就是纯文本 sha256。
pub fn wire_text_hash(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ============================================================
// ActionStore
// ============================================================

/// 重启后调度所需的完整发送输入（内部读取类型，不发给浏览器）：
/// FIFO 顺序（seq）+ 原文（来源标记恢复后仍只组装一次）+ 原 client_message_id
///（重试同 action 不生成新 id）。
#[derive(Debug, Clone)]
pub struct QueuedAction {
    pub seq: i64,
    pub ctx: ActionContext,
    pub raw_text: String,
    pub client_message_id: String,
}

/// 动作账本（包装 `DeviceStore`；生产 = 全局库，测试 = 内存库注入）。
pub struct ActionStore {
    store: DeviceStore,
}

fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Database(e.to_string())
}

impl ActionStore {
    /// 构造并执行启动恢复：reserved → unknown（writer 已接手而进程消失 = 投递结果
    /// 未知，绝不回 queued）；queued 行不受影响，仍按序可发。
    pub fn new(store: DeviceStore) -> Self {
        let s = Self { store };
        match s.recover_reserved() {
            Ok(0) => {}
            Ok(n) => log::warn!("codex_actions: 启动恢复 {n} 条 reserved 动作为 unknown"),
            Err(e) => log::error!("codex_actions: 启动恢复失败: {e}"),
        }
        s
    }

    /// 单次 `DeviceStore::with()`（不可重入；闭包内完成全部 SQL，不跨 await）。
    fn with<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        self.store.with(f)
    }

    /// 登记动作（幂等）：同 action_id 且 device/thread/kind/requestKey/token/payload
    /// 全同 → 返回既有视图（原状态）；任一不同 → `Conflict`（零新增写入）。同时生成
    /// 并持久化 client_message_id（重试同 action 不换新 id）。
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        ctx: &ActionContext,
        kind: ActionKind,
        request_key: Option<&str>,
        context_token: Option<&str>,
        payload_hash: &str,
        raw_text: Option<&str>,
    ) -> Result<CodexActionView, StoreError> {
        let client_message_id = new_uuid();
        let thread_id = ctx.key.thread_id.clone();
        let device_id = ctx.device_id.clone();
        let action_id = ctx.action_id.clone();
        let device_name = ctx.device_name.clone();
        let kind_s = kind_str(kind);
        self.with(move |c| {
            if let Some(existing) = dao::get_row(c, &action_id).map_err(db_err)? {
                let same = existing.device_id == device_id
                    && existing.thread_id == thread_id
                    && existing.kind == kind_s
                    && existing.payload_hash == payload_hash
                    && existing.request_key.as_deref() == request_key
                    && existing.context_token.as_deref() == context_token;
                if !same {
                    return Err(StoreError::Conflict);
                }
                return view_of(c, existing).map_err(db_err);
            }
            let row = dao::insert_action_conn(
                c,
                &action_id,
                &device_id,
                &device_name,
                &thread_id,
                kind_s,
                request_key,
                context_token,
                payload_hash,
                raw_text,
                &client_message_id,
                now_ms(),
            )
            .map_err(db_err)?;
            view_of(c, row).map_err(db_err)
        })
    }

    /// FIFO 队首（seq 升序），供重启后调度取得全部发送输入。
    pub fn next_queued(&self, key: &ThreadKey) -> Result<Option<QueuedAction>, StoreError> {
        self.with(|c| {
            let Some(row) = dao::next_queued_conn(c, &key.thread_id).map_err(db_err)? else {
                return Ok(None);
            };
            Ok(Some(queued_action_of(row)?))
        })
    }

    /// CAS queued→reserved（与审计起始行同事务；DB 失败零投递）。false = CAS 失败
    /// （已撤/已派发），绝不覆盖既有状态。
    pub fn reserve(&self, action_id: &str, route: ControlRoute) -> Result<bool, StoreError> {
        self.with(|c| dao::reserve_conn(c, action_id, route, now_ms()).map_err(db_err))
    }

    /// 同 permit 把 reserved+DesktopIpc 原子改为 reserved+HeadlessCompat 并记降级审计
    /// （恰好一次，绝不经过 queued）。false = 已交接过 / 状态或路由不符。
    pub fn handoff_not_sent(&self, action_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::handoff_not_sent_conn(c, action_id, now_ms()).map_err(db_err))
    }

    /// 落账回执（仅 reserved 可落账；重复 finish = `Conflict`）。NotSent 同事务清
    /// compat fence（零投递动作没有 Turn，不等待 Turn 结束）。
    pub fn finish(&self, receipt: &ActionReceipt) -> Result<(), StoreError> {
        self.with(|c| dao::finish_conn(c, receipt, now_ms()).map_err(db_err))
    }

    /// 本设备的动作视图（其他设备 / 不存在 → None；HTTP 层 404 语义）。
    pub fn get_for_device(
        &self,
        action_id: &str,
        device_id: &str,
    ) -> Result<Option<CodexActionView>, StoreError> {
        self.with(|c| {
            let Some(row) = dao::get_row(c, action_id).map_err(db_err)? else {
                return Ok(None);
            };
            if row.device_id != device_id {
                return Ok(None);
            }
            view_of(c, row).map(Some).map_err(db_err)
        })
    }

    /// 启动恢复（构造时已调用；重启测试可显式再调）。
    pub fn recover_reserved(&self) -> Result<usize, StoreError> {
        self.with(|c| dao::recover_reserved_conn(c, now_ms()).map_err(db_err))
    }

    /// 本设备在该 Thread 的 queued 列表（position = 跨设备 FIFO 实际位置，1 起）。
    pub fn list_queued(
        &self,
        key: &ThreadKey,
        device_id: &str,
    ) -> Result<Vec<CodexActionView>, StoreError> {
        self.with(|c| {
            let rows = dao::queued_for_thread_conn(c, &key.thread_id).map_err(db_err)?;
            let mut out = Vec::new();
            for (idx, row) in rows.iter().enumerate() {
                if row.device_id != device_id {
                    continue;
                }
                let view = view_of(c, row.clone()).map_err(db_err)?;
                out.push(CodexActionView {
                    position: Some(idx + 1),
                    ..view
                });
            }
            Ok(out)
        })
    }

    /// 撤回：CAS queued→cancelled（仅属主设备），保留 actionId 墓碑、清正文并审计。
    /// reserved/accepted/unknown 不可撤（false）。
    pub fn retract(&self, action_id: &str, device_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::retract_conn(c, action_id, device_id, now_ms()).map_err(db_err))
    }

    /// 派发前复核设备在册（吊销即拒发——「每次真正派发前再核设备在册」）。
    pub fn device_valid(&self, device_id: &str) -> bool {
        self.store
            .with(|c| pairing::device_valid(c, device_id, now_ms()))
    }

    // ---- PR #117 兼容 fence ----

    /// 置 fence（compat bridge 在发送前调用：持久 reserved + fence）。
    pub fn set_compat_fence(
        &self,
        action_id: &str,
        message_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            dao::set_compat_fence_conn(c, action_id, message_id, turn_id, now_ms()).map_err(db_err)
        })
    }

    /// 登记 fence 只读核对证据（rollout 基线 + 已组装文本 hash；随 fence 持久保存，
    /// 跨热重启与应用重启有效）。
    pub fn set_compat_evidence(
        &self,
        action_id: &str,
        rollout_path: Option<&str>,
        rollout_baseline: Option<u64>,
        wire_hash: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            dao::set_compat_evidence_conn(
                c,
                action_id,
                rollout_path,
                rollout_baseline,
                wire_hash,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    /// 该 Thread 是否有在册 fence（fence 在册 = 队列不接力，界面说明需回 Desktop 核对）。
    pub fn has_compat_fence(&self, key: &ThreadKey) -> Result<bool, StoreError> {
        self.with(|c| dao::has_compat_fence_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 清 fence：仅终态动作允许（queued/reserved → `Conflict`）。Accepted/Unknown 必须
    /// 先经只读核对（[`reconcile_compat_fence`]）拿到对应 Turn 完成证据；helper
    /// 「已消费」回执不算证据。
    pub fn clear_compat_fence(&self, action_id: &str) -> Result<(), StoreError> {
        self.with(|c| {
            match dao::clear_compat_fence_conn(c, action_id, now_ms()).map_err(db_err)? {
                true => Ok(()),
                false => Err(StoreError::Conflict),
            }
        })
    }

    /// 读取该 Thread 全部在册 fence（seq 序；reconcile 迭代输入）。
    pub fn compat_fences(&self, key: &ThreadKey) -> Result<Vec<CompatFence>, StoreError> {
        self.with(|c| dao::compat_fences_conn(c, &key.thread_id).map_err(db_err))
    }
}

fn view_of(c: &Connection, row: dao::ActionRow) -> rusqlite::Result<CodexActionView> {
    let position = if row.state == ActionState::Queued {
        dao::queued_for_thread_conn(c, &row.thread_id)?
            .iter()
            .position(|r| r.action_id == row.action_id)
            .map(|i| i + 1)
    } else {
        None
    };
    Ok(CodexActionView {
        action_id: row.action_id,
        state: row.state,
        position,
        receipt: row.receipt,
    })
}

fn queued_action_of(row: dao::ActionRow) -> Result<QueuedAction, StoreError> {
    let kind = kind_from_str(&row.kind)
        .ok_or_else(|| StoreError::Database(format!("unknown codex_actions.kind: {}", row.kind)))?;
    Ok(QueuedAction {
        seq: row.seq,
        ctx: ActionContext {
            action_id: row.action_id,
            device_id: row.device_id,
            device_name: row.device_name,
            kind,
            key: ThreadKey {
                // 服务端仅构造 local（计划公共类型约定）
                host_id: "local".to_string(),
                thread_id: row.thread_id,
            },
        },
        raw_text: row.queued_text.unwrap_or_default(),
        client_message_id: row.client_message_id,
    })
}

pub use dao::CompatFence;

// ============================================================
// 兼容 fence 只读核对（reconcile）
// ============================================================

/// rollout 追加尾窗上限（计划 Task 7：最多 2 MiB）
pub(crate) const FENCE_ROLLOUT_TAIL_CAP: u64 = 2 * 1024 * 1024;

/// 队列副本查询缝（生产 = [`production_queue_holds`]；测试注入替身，绝不触碰真实
/// `~/.codex`）
pub type QueueHoldsSeam = dyn Fn(&str, &str) -> Option<QueueEvidence> + Send + Sync;

/// 只读核对 compat fence（计划 Task 7 接口）：复用既有队列副本查询与有界 rollout
/// 解析（最多追加尾窗 2 MiB），按基线之后**唯一**匹配的用户输入 hash 关联 Turn，再核
/// 其完成。返回 `Ok(true)` 才允许 clear fence——仅 idle 投影、helper「已消费」回执、
/// 「队列没找到」都不足以证明执行完成。
///
/// 返回 `Ok(false)` 的情形：证据字段缺失、读失败、多义（同一文本出现两次）、已被
/// 尾窗截断（无法证明唯一性）、投影与 rollout 归属不一致、消息仍在原生队列、无对应
/// Turn 终态。只读核对不修改 Codex 数据文件。
///
/// `projection` 用于归属一致性核对（rollout 文件名 UUID 必须与投影 thread 一致）；
/// idle/stale 投影本身不构成完成证据，也不构成否决（无原生 owner 时由 rollout 的
/// 明确对应 Turn 终态提供完成证据）。
pub async fn reconcile_compat_fence(
    fence: &CompatFence,
    projection: &ThreadProjection,
) -> Result<bool, ProtocolError> {
    reconcile_compat_fence_with(fence, projection, &|thread, msg_id| {
        production_queue_holds(thread, msg_id)
    })
}

/// seam 可注入版本（测试用；语义同 [`reconcile_compat_fence`]）。
pub fn reconcile_compat_fence_with(
    fence: &CompatFence,
    projection: &ThreadProjection,
    queue_holds: &QueueHoldsSeam,
) -> Result<bool, ProtocolError> {
    // 1. 证据字段完整性（fence 未登记 rollout 基线/文本 hash → 无从核对）
    let Some(path) = fence.rollout_path.as_deref() else {
        return Ok(false);
    };
    let Some(baseline) = fence.rollout_baseline else {
        return Ok(false);
    };
    let Some(wire_hash) = fence.wire_hash.as_deref() else {
        return Ok(false);
    };

    // 2. 归属一致：rollout 文件名必须携带与投影一致的 thread UUID（防拿错 Thread 核对）
    match crate::inject::headless::codex::thread_id_of(path) {
        Some(tid) if tid == projection.thread_id => {}
        _ => return Ok(false),
    }

    // 3. 队列副本：我们的消息仍在原生队列 = 未消费
    if let Some(mid) = fence.queued_message_id.as_deref() {
        if matches!(
            queue_holds(&projection.thread_id, mid),
            Some(QueueEvidence::Ours)
        ) {
            return Ok(false);
        }
    }

    // 4. 有界尾窗读取（最多追加 2 MiB；读失败/截断 → false）
    let Some(tail) = read_rollout_tail(path, baseline, FENCE_ROLLOUT_TAIL_CAP) else {
        return Ok(false);
    };
    if tail.truncated {
        // 追加段超窗：唯一性无法证明（不能「没读到」就当没发生）
        return Ok(false);
    }

    // 5. 基线之后唯一匹配的用户输入 hash → 关联 Turn → 核其终态
    let mut match_count = 0usize;
    let mut completed_after_match = false;
    let mut in_match_turn = false;
    for value in &tail.lines {
        if let Some(text) = rollout_user_input(value) {
            if wire_text_hash(&text) == wire_hash {
                match_count += 1;
                if match_count > 1 {
                    // 多义：同一文本出现两次，无法确定对应哪次投递
                    return Ok(false);
                }
                in_match_turn = true;
                completed_after_match = false;
            } else {
                // 别的输入切开了 Turn 窗口：我们的输入所在 Turn 已无法向后关联
                in_match_turn = false;
            }
        } else if in_match_turn && rollout_task_complete(value, fence.turn_id.as_deref()) {
            completed_after_match = true;
        }
    }
    Ok(match_count == 1 && completed_after_match)
}

struct RolloutTail {
    truncated: bool,
    lines: Vec<serde_json::Value>,
}

/// 读 rollout 基线之后的追加段（最多 `cap` 字节）；文件不增长 → 空段。None = 读失败。
/// 末尾不完整行（并发写入的半个 JSONL）丢弃——保守漏判胜过误判完成。
fn read_rollout_tail(path: &str, baseline: u64, cap: u64) -> Option<RolloutTail> {
    use std::io::{Read, Seek, SeekFrom};
    let len = std::fs::metadata(path).ok()?.len();
    if len <= baseline {
        return Some(RolloutTail {
            truncated: false,
            lines: Vec::new(),
        });
    }
    let region = len - baseline;
    let read_len = region.min(cap.max(1));
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(baseline)).ok()?;
    let mut buf = vec![0u8; read_len as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    let mut text = String::from_utf8_lossy(&buf[..filled]).into_owned();
    // 丢末尾不完整行（无换行结尾 = 可能是并发写入的一半）
    if !text.ends_with('\n') {
        if let Some(pos) = text.rfind('\n') {
            text.truncate(pos + 1);
        } else {
            text.clear();
        }
    }
    let lines = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    Some(RolloutTail {
        truncated: region > read_len,
        lines,
    })
}

/// rollout 用户输入行提取（实测 JSONL 形态，见 monitor::codex_parser 夹具）：
/// `{"type":"response_item","payload":{"type":"message","role":"user",
///   "content":[{"type":"input_text","text":"…"}]}}`
fn rollout_user_input(value: &serde_json::Value) -> Option<String> {
    let payload = value.get("payload")?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("response_item") {
        return None;
    }
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(serde_json::Value::as_str) != Some("user") {
        return None;
    }
    let content = payload.get("content")?.as_array()?;
    let mut text = String::new();
    for item in content {
        if item.get("type").and_then(serde_json::Value::as_str) == Some("input_text") {
            text.push_str(item.get("text").and_then(serde_json::Value::as_str)?);
        }
    }
    (!text.is_empty()).then_some(text)
}

/// Turn 终态行：`{"type":"event_msg","payload":{"type":"task_complete","turn_id":"…"}}`。
/// fence 绑定了 turn_id 时事件必须带同 id；事件缺 turn_id 而需要绑定 → 不算完成。
fn rollout_task_complete(value: &serde_json::Value, want_turn: Option<&str>) -> bool {
    let Some(payload) = value.get("payload") else {
        return false;
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("event_msg") {
        return false;
    }
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("task_complete") {
        return false;
    }
    let got = payload.get("turn_id").and_then(serde_json::Value::as_str);
    match (want_turn, got) {
        (Some(want), Some(actual)) => actual == want,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        }
    }

    /// payload hash：同 payload 同 hash；换 thread / 答案顺序语义 / requestKey 皆变
    #[test]
    fn payload_hash_is_normalized_and_discriminating() {
        let k = key();
        let base = payload_hash(
            ActionKind::Send,
            &k,
            None,
            None,
            &ActionPayload::Text("你好".into()),
        );
        assert_eq!(base.len(), 64);
        // 同输入稳定
        assert_eq!(
            base,
            payload_hash(
                ActionKind::Send,
                &k,
                None,
                None,
                &ActionPayload::Text("你好".into())
            )
        );
        // 不能只 hash 文本：换 thread/kind/token/requestKey/答案都变
        assert_ne!(
            base,
            payload_hash(
                ActionKind::Send,
                &k,
                None,
                Some("tok"),
                &ActionPayload::Text("你好".into())
            )
        );
        assert_ne!(
            base,
            payload_hash(
                ActionKind::UserInput,
                &k,
                None,
                None,
                &ActionPayload::Text("你好".into())
            )
        );
        assert_ne!(
            payload_hash(
                ActionKind::UserInput,
                &k,
                Some("r"),
                None,
                &ActionPayload::Answers(BTreeMap::from([(
                    "q1".into(),
                    vec!["a".into(), "b".into()]
                )]))
            ),
            payload_hash(
                ActionKind::UserInput,
                &k,
                Some("r"),
                None,
                &ActionPayload::Answers(BTreeMap::from([(
                    "q1".into(),
                    vec!["b".into(), "a".into()]
                )]))
            ),
            "有序答案：顺序不同 hash 不同"
        );
        // BTreeMap 键序无关：构造顺序不同、内容相同 → 同 hash
        let m1 = BTreeMap::from([
            ("a".into(), vec!["1".into()]),
            ("b".into(), vec!["2".into()]),
        ]);
        assert_eq!(
            payload_hash(
                ActionKind::UserInput,
                &k,
                None,
                None,
                &ActionPayload::Answers(m1)
            ),
            payload_hash(
                ActionKind::UserInput,
                &k,
                None,
                None,
                &ActionPayload::Answers(BTreeMap::from([
                    ("b".into(), vec!["2".into()]),
                    ("a".into(), vec!["1".into()])
                ]))
            )
        );
        // approve 与 reject 必须可区分（router 用标记文本）
        assert_ne!(
            payload_hash(
                ActionKind::CommandApproval,
                &k,
                Some("n:1"),
                Some("t"),
                &ActionPayload::Text("approve".into())
            ),
            payload_hash(
                ActionKind::CommandApproval,
                &k,
                Some("n:1"),
                Some("t"),
                &ActionPayload::Text("reject".into())
            )
        );
    }

    /// wire hash 纯文本确定性
    #[test]
    fn wire_text_hash_is_plain_sha256() {
        assert_eq!(wire_text_hash("hi"), wire_text_hash("hi"));
        assert_ne!(wire_text_hash("hi"), wire_text_hash("hi "));
        assert_eq!(wire_text_hash("hi").len(), 64);
    }

    /// 旧库升级安全：无 codex_actions 的旧库经 schema::init 幂等补建，既有表数据不丢
    #[test]
    fn schema_init_upgrades_old_db_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        // 模拟旧库：只有老表（inject_queue + write_audit）并带一行数据
        conn.execute_batch(
            "CREATE TABLE inject_queue (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
             agent_type TEXT NOT NULL, device_id TEXT NOT NULL, device_name TEXT NOT NULL, content TEXT NOT NULL, \
             enqueued_at INTEGER NOT NULL, sent_at INTEGER, failed_reason TEXT);
             CREATE TABLE write_audit (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, device_id TEXT NOT NULL, \
             device_name TEXT NOT NULL, agent_type TEXT NOT NULL, session_id TEXT NOT NULL, channel TEXT NOT NULL, \
             action TEXT NOT NULL, summary TEXT NOT NULL, result TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO write_audit (ts, device_id, device_name, agent_type, session_id, channel, action, summary, result) \
             VALUES (1, 'd', 'iPhone', 'claude', 's', 'tmux', 'send', 'sum', 'ok')",
            [],
        )
        .unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        // 新表可用 + 旧数据不丢
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM write_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "既有审计数据不丢");
        conn.execute(
            "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, payload_hash, client_message_id, state, created_at, updated_at) \
             VALUES ('a', 'd', 'n', 't', 'send', 'h', 'cm', 'queued', 1, 1)",
            [],
        )
        .unwrap();
        // 幂等：重复 init 不炸
        crate::database::schema::init(&conn);
    }

    /// DeviceStore::memory 路径（schema::init → migration::migrate）上 ActionStore 可用
    #[test]
    fn store_on_memory_device_store_works() {
        let store = ActionStore::new(DeviceStore::memory());
        let ctx = ActionContext {
            action_id: "a".into(),
            device_id: "no-device".into(),
            device_name: "n".into(),
            kind: ActionKind::Send,
            key: key(),
        };
        let v = store
            .register(&ctx, ActionKind::Send, None, None, "h", Some("原文"))
            .unwrap();
        assert_eq!(v.state, ActionState::Queued);
        assert_eq!(v.position, Some(1));
        let head = store.next_queued(&key()).unwrap().unwrap();
        assert_eq!(head.raw_text, "原文");
        assert!(!head.client_message_id.is_empty());
    }
}
