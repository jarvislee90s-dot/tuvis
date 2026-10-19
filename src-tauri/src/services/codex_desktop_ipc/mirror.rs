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
//!   path 直接对 `conversationState` 本体寻址（R1：真实形状无前缀，
//!   来源 reviewer 2026-10-17 实读 Windows 原始日志，脱敏夹具同步）；
//! * 低 revision 重放忽略；revision 缺口 → stale + need_snapshot；
//!   新 epoch（换代后）不与旧 revision 比大小（状态已清）；
//! * 预算：单 Thread 镜像 16 MiB；投影正文尾窗 1 MiB / 500 条并标 truncated；
//!   最新一条自身超窗时截头保底保留（绝不把整份投影清成 0 条）。

use super::history_view::HistoryView;
use super::pending::PendingProjector;
use super::types::{
    CapabilitySet, ContentState, ControlAvailability, OwnerLease, ProjectedMessage, ProtocolError,
    ThreadKey, ThreadProjection,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// 单 Thread 镜像字节预算。
pub const MIRROR_BUDGET: usize = 16 * 1024 * 1024;
/// 全体镜像总字节预算（2026-10-17 复审 C4）：所有 ThreadMirror 保留状态的合计上限
///——64 个关注名额不是镜像字节数上界（64 × 16MiB ≫ 64MiB）。adopt 前原子预留；
/// patch 原子应用的 clone 是暂态（逐 Thread ≤ 16MiB 上界，不并入长期账目）。
pub const GLOBAL_MIRROR_BUDGET: usize = 64 * 1024 * 1024;
/// 投影正文最大条数。
pub const PROJECTION_MAX_MESSAGES: usize = 500;
/// 投影正文尾窗字节预算。
pub const PROJECTION_TAIL_BYTES: usize = 1024 * 1024;
/// 单条正文尾窗保底上限：最新一条自身超 [`PROJECTION_TAIL_BYTES`] 时该条不整条
/// 裁掉，正文截头到该字节数（字符边界对齐）加显式截断标记。
pub const PROJECTION_HEAD_BYTES: usize = 64 * 1024;

/// 全体镜像字节账本：claim(key, old, new) 原子预留，超总预算返回 false（镜像转
/// stale + Capacity 拒绝，等内存释放后由下次 ensure/事件救活）；mirror 卸载/
/// 清态时释放。
#[derive(Default)]
pub struct MirrorLedger {
    total: usize,
    per_key: HashMap<ThreadKey, usize>,
    cap: usize,
}

impl MirrorLedger {
    /// 生产账本（全局 64MiB）。
    pub fn global() -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap: GLOBAL_MIRROR_BUDGET,
        }
    }

    /// 测试账本（自定义上限）。
    pub fn with_cap(cap: usize) -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap,
        }
    }

    /// 原子预留：把 key 的占用从 old 换成 new；合计超上限 → false（不做部分生效）。
    pub fn claim(&mut self, key: &ThreadKey, old: usize, new: usize) -> bool {
        let delta = new as i64 - old as i64;
        let projected = self.total as i64 + delta;
        if projected < 0 || projected as usize > self.cap {
            return false;
        }
        self.total = projected as usize;
        self.per_key.insert(key.clone(), new);
        true
    }

    /// 退场释放。
    pub fn release(&mut self, key: &ThreadKey, size: usize) {
        self.total = self.total.saturating_sub(size);
        self.per_key.remove(key);
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// 账本上限（拒绝原因文案用）。
    pub fn cap_of(&self) -> usize {
        self.cap
    }
}

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
    /// conversationState 本体（R1：snapshot 存 `change["conversationState"]`；
    /// patch 直接对该本体寻址——reviewer 2026-10-17 实读 Windows 原始日志定案，
    /// 真实形状无 `conversationState` 路径前缀）
    state: Option<Value>,
    pending: PendingProjector,
    /// Turn 列表完整性（`turnHistory.history.isComplete`；不完整时 adapter 决定
    /// 是否调只读 load-complete-history）
    history_complete: bool,
    /// C4：全体镜像字节账本（生产挂全局账本；None = 独立镜像只受单 Thread 预算）
    ledger: Option<Arc<std::sync::Mutex<MirrorLedger>>>,
    /// 本镜像当前保留字节数（账本对账依据）
    size: usize,
}

impl Drop for ThreadMirror {
    fn drop(&mut self) {
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
    }
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
            ledger: None,
            size: 0,
        }
    }

    /// 挂全局字节账本（adapter 构造镜像的唯一生产入口）。
    pub fn with_ledger(key: ThreadKey, ledger: Arc<std::sync::Mutex<MirrorLedger>>) -> Self {
        let mut m = Self::new(key);
        m.ledger = Some(ledger);
        m
    }

    pub fn key(&self) -> &ThreadKey {
        &self.key
    }

    /// 镜像当前绑定的 owner lease（2026-10-17 复审 C3：respond 三阶段同代复核锚——
    /// 阶段 1 记录、阶段 3 与现行 lease 全同比较；换代后旧载荷零写入）。
    pub fn lease(&self) -> Option<&OwnerLease> {
        self.lease.as_ref()
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

    /// 只读 hydrate 触发判据（2026-10-19 复审 P2-2）：**列表完整性不够**——Turn
    /// 列表完整但某个 turn 的 items 分页未载完/淘汰/明确未加载（itemsView）时同样
    /// 需要 load-complete-history；结构不可读也做有界只读重载尝试（可能是坏快照）。
    /// 无快照不触发（follow 首帧随后到达再判）。目标 Turn 终态证据不走本判据。
    pub fn needs_hydration(&self) -> bool {
        match self.state.as_ref() {
            None => false,
            Some(state) => HistoryView::new(state).needs_hydration(),
        }
    }

    /// 原始 conversationState 本体（pending permissions 回显等只读用途；不进远程 API）。
    pub fn raw_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// 校验 pending context_token（委托 pending 投影器）。
    pub fn token_valid(&self, request_key: &str, context_token: &str) -> bool {
        self.pending.token_valid(request_key, context_token)
    }

    /// 指定 Turn 实体的状态（2026-10-17 复审 C1：Turn 顺序屏障的目标终态证据来源）。
    /// 2026-10-18 真机形态适配：证据经统一读层（[`HistoryView`]）——按 canonical
    /// 引用链（islands entries.value → 实体）并以实体 `turnId` 字段认领目标；
    /// 实体 map key（`turn:`/`tail:` 合成键）不是 id，无 `type` 字段是正常形态，
    /// 未被引用的孤立实体不作数。不在场 / 无镜像 → None（调用方按「无证据」保守）。
    pub fn turn_status(&self, turn_id: &str) -> Option<String> {
        let state = self.state.as_ref()?;
        HistoryView::new(state)
            .turn_status(turn_id)
            .map(str::to_string)
    }

    /// 指定 Turn 是否已以终态出现在当前镜像（completed/failed）。
    /// 不在场 / inProgress / 未知状态 / stale / 无镜像 → false（保守不释放）。
    pub fn turn_is_terminal(&self, turn_id: &str) -> bool {
        !self.stale
            && matches!(
                self.turn_status(turn_id).as_deref(),
                Some("completed") | Some("failed")
            )
    }

    /// 标记过期（连接断开/缺口/预算超限）。保留最后视图但禁写（adapter 侧门）。
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// owner 失效（`client-status-changed(disconnected)` 等）：清状态 + 全部 pending
    /// token 失效（区别于 [`mark_stale`](Self::mark_stale) 的保视图只读）。之后只能
    /// 由换代 lease 的新 snapshot 重建。
    pub fn invalidate(&mut self) {
        self.clear_state();
    }

    fn clear_state(&mut self) {
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
        self.size = 0;
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
        // 只存 conversationState 本体：patch 相对该本体寻址（R1 真实根路径）
        let state = change
            .get("conversationState")
            .cloned()
            .unwrap_or(Value::Null);
        self.adopt(rev, state)?;
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
        // 2026-10-18 形态适配：完整性从**新 state** 派生（snapshot/patch 同一判定
        // 单点），不再沿用上一状态的缓存 bool
        self.adopt(rev, next)?;
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    /// 提交新状态（预算校验：单 Thread 16MiB + 全体 64MiB；超限 stale + 拒绝，
    /// 不冒充无 owner）。完整性在此**单点派生**：`turnHistory.history.isComplete ===
    /// true` 才算 Turn 列表完整（真机形态在 history 层；缺字段 ≠ 完整——hydrate
    /// 触发判据不能缺省当完整）。
    fn adopt(&mut self, rev: u64, state: Value) -> Result<(), ProtocolError> {
        let size = serde_json::to_vec(&state)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MIRROR_BUDGET {
            self.stale = true;
            return Err(ProtocolError::Capacity(format!(
                "mirror exceeds per-thread budget: {size} > {MIRROR_BUDGET}"
            )));
        }
        // C4：全体总预算——原子预留（old→new 换占），超限整批作废转 stale
        if let Some(ledger) = &self.ledger {
            let mut l = ledger.lock().expect("ledger lock");
            if !l.claim(&self.key, self.size, size) {
                self.stale = true;
                return Err(ProtocolError::Capacity(format!(
                    "mirror exceeds global budget: total would exceed {} bytes",
                    l.cap_of()
                )));
            }
        }
        self.size = size;
        self.revision = Some(rev);
        self.history_complete = HistoryView::new(&state).turn_list_complete();
        self.state = Some(state);
        self.stale = false;
        Ok(())
    }

    /// 统一状态投影（远程 API / SSE / 移动端唯一消费形态）。
    /// token 缓存存于 PendingProjector 内部互斥锁，`&self` 即可投影。
    pub fn project(&self, caps: CapabilitySet) -> ThreadProjection {
        // R1：state 即 conversationState 本体，按根直读
        let state = self.state.clone().unwrap_or(Value::Null);
        let messages = project_messages(&state);
        let content_state = content_state_of(self.state.as_ref());
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
            content_state,
            truncated: messages.1,
            messages: messages.0,
            pending,
        }
    }
}

/// 内容健康状态（与写能力正交——能发送不代表历史已载入；读取出错不得呈现为
/// 「真实空会话」）：
/// * 无状态（尚未收到首帧 snapshot）→ Loading；
/// * `turnHistory.history` 形态不可读 → Unsupported（不伪造 0 条空历史）；
/// * Turn 列表不完整（history.isComplete ≠ true）→ Loading（hydrate 判据同源）；
/// * Turn 列表完整但任一 turn 的 items 分页未载完 → Partial；
/// * 其余 → Ready（此时真实 0 条才允许显示「暂无消息」）。stale 只影响写侧，
///   不改内容健康语义（保留最后视图照常展示）。
fn content_state_of(state: Option<&Value>) -> ContentState {
    let Some(state) = state else {
        return ContentState::Loading;
    };
    let view = HistoryView::new(state);
    if !view.readable() || view.health() != super::history_view::ViewHealth::Ok {
        // 结构不可读 ≠ 真空会话（2026-10-19 复审 P2-3：非法 entries/items 类型/
        // 悬空引用不得 Ready+0 条）
        ContentState::Unsupported
    } else if !view.turn_list_complete() {
        ContentState::Loading
    } else if view.items_incomplete() {
        ContentState::Partial
    } else {
        ContentState::Ready
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
    // 2026-10-18 形态适配：经统一读层（canonical 引用链 + turnId 字段认领），
    // 不再要求实体 type=="turn"（真机 turn 实体无该字段）
    {
        let view = HistoryView::new(state);
        let mut saw_failed = false;
        for turn in view.turns() {
            match turn.status {
                Some("inProgress") => return "active".into(),
                Some("failed") => saw_failed = true,
                _ => {}
            }
        }
        if saw_failed {
            return "failed".into();
        }
    }
    if declared == Some("idle") {
        "idle".into()
    } else {
        "unknown".into()
    }
}

/// 按正史顺序投影消息，尾窗 1 MiB / 500 条，超出标 truncated。
/// 返回 (消息, 是否截断)。
/// 字节尾窗**绝不把最新一条整条裁掉**：单条自身超 1 MiB（agentMessage 贴大日志/
/// 大代码块真实可达）→ 保底保留该 1 条，正文截头（[`PROJECTION_HEAD_BYTES`]）+
/// 显式标记，truncated 置位；多条累计超限才丢弃较旧条目（维持既有逻辑）。
/// 2026-10-18 真机形态适配：经统一读层走 canonical 引用链（entries.value → 实体，
/// turnId 字段认领），正文取 `turn.items[]` **内联对象**（camelCase）——
/// `userMessage.content[]` 的 text 块、`agentMessage.text`；`error.message` 走系统
/// 安全摘要；`reasoning` 按产品语义过滤；未知 kind 保留 `[unsupported item]` 摘要，
/// 绝不静默滤成 0 条。
/// 开场用户正文的纯提取（计划 Task 6）：**只处理已经由 HistoryView 验明身份的
/// Turn**；`params.input` 非文本块不猜内容。返回 `(投影 ID, 正文)`。
///
/// 投影 ID = `{turnId}/opening/{clientUserMessageId}`——它是**显示身份**，不是投递
/// 身份：不参与 `ThreadMirror` 健康、完整性、runtime、送达或屏障。
fn opening_input(turn_id: &str, params: &Value) -> Option<(String, String)> {
    let client = params.get("clientUserMessageId")?.as_str()?;
    if client.is_empty() {
        return None;
    }
    let text = params
        .get("input")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    Some((format!("{turn_id}/opening/{client}"), text))
}

fn project_messages(state: &Value) -> (Vec<ProjectedMessage>, bool) {
    let view = HistoryView::new(state);
    let mut all: Vec<ProjectedMessage> = Vec::new();
    // P2-2（复审）：键迁移双引用窗口（同 turnId 两个被引用实体）下 items 可能
    // 重叠——按 item id 跨 turn 去重，绝不追加重复气泡
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let turns = view.turns();
    // 第一遍：已有可渲染正式 userMessage 的 turnId——后备只在**没有正式用户项**时
    // 出现；跨键副本先看到后备、后看到正式项时也必须由本遍压住。
    let mut has_formal_user: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for turn in &turns {
        if turn.items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("userMessage")
                && user_message_text(item).is_some()
        }) {
            has_formal_user.insert(turn.turn_id);
        }
    }
    for turn in turns {
        // 开场后备：Turn 有 `params.input` 而正式 userMessage 尚未到达（或已被历史
        // 裁剪）时补一条用户气泡；正式项到达后由第一遍压住，同一逻辑消息不重复。
        let opening = turn.params.and_then(|p| opening_input(turn.turn_id, p));
        if !has_formal_user.contains(turn.turn_id) {
            if let Some((id, text)) = &opening {
                if seen_ids.insert(id.clone()) {
                    all.push(ProjectedMessage {
                        id: id.clone(),
                        role: "user".to_string(),
                        kind: "userMessage".to_string(),
                        text: text.clone(),
                        ts: None,
                    });
                }
            }
        }
        for (idx, item) in turn.items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                continue; // 非对象 item：无可读形态，保守跳过（不猜）
            };
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            // 稳定标识 = item.id；缺 id 的兜底拼接不作长期标识（仅保序去重可用）。
            // **正式 userMessage 的 clientId 与 params.clientUserMessageId 相等时**
            // 复用开场投影 ID：同一逻辑消息在「后备 → 正式」替换时身份不变。
            let mut id = obj
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}/{}", turn.turn_id, idx));
            if kind == "userMessage" {
                if let (Some((opening_id, _)), Some(client)) = (
                    opening.as_ref(),
                    obj.get("clientId").and_then(Value::as_str),
                ) {
                    if opening_id.ends_with(&format!("/opening/{client}")) {
                        id = opening_id.clone();
                    }
                }
            }
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let (role, text) = match kind {
                "userMessage" => {
                    let Some(text) = user_message_text(item) else {
                        continue; // 无可渲染正文（content 缺/空）不造空泡
                    };
                    ("user", text)
                }
                "agentMessage" => {
                    let text = obj
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if text.is_empty() {
                        continue; // 流式空文本占位不造空泡（文本经 replace 原位出现）
                    }
                    ("assistant", text)
                }
                // 推理过程不上墙（复审裁决：允许按产品语义过滤 reasoning）
                "reasoning" => continue,
                "error" => ("system", error_item_summary(item)),
                "" => continue, // 无 type 的 item：形态不明，保守跳过
                other => ("system", format!("[unsupported item: {other}]")),
            };
            all.push(ProjectedMessage {
                id,
                role: role.to_string(),
                kind: kind.to_string(),
                text,
                ts: obj.get("ts").and_then(Value::as_i64),
            });
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
        if cut >= all.len() {
            // 最新一条自身超字节预算（倒序首轮 i == 0 → cut == all.len()）：不得
            // 整条裁成 0 条投影——移动端会把有内容的会话显示成「暂无消息」，与
            // 本文件「未知 kind 绝不静默滤成 0 条」同纪律。保底保留该条，正文
            // 单独截头 + 显式标记；truncated 已在循环内置位。
            let last = all.last_mut().expect("cut == all.len() 蕴含非空");
            last.text = truncate_text_head(&last.text, PROJECTION_HEAD_BYTES);
        } else {
            all = all.split_off(cut);
        }
    }
    (all, truncated)
}

/// userMessage 正文：`content[]` 中 `type=="text"` 块的 `text` 拼接（真机形态；
/// 块级包装字段如 text_elements 不下发）。无可渲染正文 → None。
fn user_message_text(item: &Value) -> Option<String> {
    let blocks = item.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// error item 的系统摘要：`message` 为桌面侧给用户看的错误文案（截断到 240 字符，
/// 不透传 errorInfo/additionalDetails 等内部结构）；缺 message → 固定占位。
fn error_item_summary(item: &Value) -> String {
    const CAP: usize = 240;
    let raw = item
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match raw {
        Some(m) if m.len() <= CAP => format!("[error] {m}"),
        Some(m) => {
            let mut cut = CAP;
            while !m.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("[error] {}…", &m[..cut])
        }
        None => "[error]".to_string(),
    }
}

/// 正文截头（字节上限，按字符边界回退）+ 显式截断标记（自报原文总字符数）。
/// 形态对齐 `inject::normalize::truncate_with_size_marker` 的展示截断纪律——
/// 绝不静默砍；未超限逐字原文。
fn truncate_text_head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "DISPLAY_TRUNCATED: {}…已截断，原文共 {} 字符",
        &text[..cut],
        text.chars().count()
    )
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
        // turns=[] 是常态：正文来自 turnHistory 正史（内联 items，稳定 item ID）
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].id, "00000000-0000-4000-8000-0000000000it1");
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[1].id, "00000000-0000-4000-8000-0000000000it2");
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
    fn real_root_path_patch_updates_mirror() {
        // reviewer 2026-10-17 复现形状：patch 直接对 conversationState 本体寻址
        //（path 无 conversationState 前缀），snapshot 后 replace threadRuntimeStatus.type。
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.project(caps()).runtime_status, "idle");
        // 先推进到 batch[4] 的基线（revision 212），再应用真实根路径 replace
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [0usize, 1, 3] {
            m.apply(&lease(), &batches[idx]).unwrap();
        }
        let eff = m.apply(&lease(), &batches[4]).unwrap();
        assert!(eff.changed, "真实根路径增量必须可应用");
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        assert_eq!(p.runtime_status, "active", "runtime_status 增量生效");
        assert!(!p.stale);
        assert!(!m.is_stale());
        assert_eq!(p.revision, Some(213));
    }

    #[test]
    fn patch_batch_is_atomic() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 一批：第一条合法（replace 内联 agentMessage 文本）+ 第二条非法（replace
        // 不存在的键）（R1：path 直接对 conversationState 本体寻址，无前缀）
        let batch = serde_json::json!({
            "type": "patches",
            "baseRevision": 209,
            "revision": 210,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey",
                           "turn:00000000-0000-4000-8000-0000000000tu1", "items", 1, "text"],
                  "value": "更新" },
                { "op": "replace",
                  "path": ["noSuchKey"],
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
        assert_eq!(
            p.messages[1].id, "00000000-0000-4000-8000-0000000000it2",
            "item ID 稳定"
        );
        // batch[1]：新增 turn；batch[3]：remove pending（跳过 batch[2] 缺口样本）
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [1usize, 3] {
            let eff = m.apply(&lease(), &batches[idx]).unwrap();
            assert!(eff.changed, "顺序批 {idx} 应可应用");
        }
        let p = m.project(caps());
        assert!(p.pending.is_empty(), "remove 后 pending 消失");
        assert_eq!(p.messages.len(), 4, "新 turn 的内联 items 入墙（无重复）");
        assert_eq!(p.revision, Some(212));
    }

    #[test]
    fn projection_tail_window_marks_truncated() {
        // 构造 600 条消息的快照：截到 500 条并标 truncated（真实内联 items 形态）
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..600 {
            let turn_id = format!("t{i}");
            let item_id = format!("i{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "userMessage", "id": item_id,
                                        "content": [ { "type": "text", "text": "m" } ] } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": Value::Object(entities),
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
    fn oversized_newest_message_is_head_truncated_not_dropped() {
        // 单条正文自身超 1 MiB（agentMessage 贴大日志/大代码块真实可达）：投影
        // 不得清成 0 条——保底保留最新 1 条，正文截头 + 显式标记，truncated 置位
        let original_chars = PROJECTION_TAIL_BYTES / 3 + 1000;
        let body = "汉".repeat(original_chars); // 3 B/char → 字节数 > 1 MiB
        assert!(body.len() > PROJECTION_TAIL_BYTES);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                "items": [ { "type": "agentMessage", "id": "big1",
                                             "text": body } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1",
                                                     "value": "turn:t1" } ] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.messages.len(),
            1,
            "超窗最新条必须保底保留，不得投影成 0 条"
        );
        assert!(p.truncated, "截头分支必须置位 truncated");
        let text = &p.messages[0].text;
        // 展示截断标记沿用 `DISPLAY_TRUNCATED: ` 前缀（与 normalize 同款口径，
        // 错误码对账门禁要求自报码）；头部原文紧随其后。
        const MARK: &str = "DISPLAY_TRUNCATED: ";
        assert!(text.starts_with(MARK), "截断标记前缀: {text}");
        assert!(text.contains('汉'), "保留头部原文");
        assert!(text.len() < body.len(), "正文确实被截短");
        assert!(text.len() < PROJECTION_TAIL_BYTES, "截头后落回尾窗预算内");
        let expected_tail = format!("…已截断，原文共 {original_chars} 字符");
        assert!(text.ends_with(&expected_tail), "显式自报原文总字符数");
        // 64 KiB 切点不在 3 B 字符边界 → 回退对齐后头部必须是整字符（无半个字符）
        let head_len = text.len() - MARK.len() - expected_tail.len();
        assert_eq!(head_len % 3, 0, "切点落在字符边界");
    }

    #[test]
    fn byte_tail_window_drops_only_older_messages() {
        // 多条累计超 1 MiB：旧条被裁、最新条整条保留（维持既有丢弃逻辑，
        // 不触发单条截头——3×400 KB 才破窗，最新 2 条 < 1 MiB）
        let body = "a".repeat(400_000);
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..5 {
            let turn_id = format!("t{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "agentMessage",
                                        "id": format!("m{i}"), "text": body } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": entities,
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "累计超窗只裁旧条：保留最新 2 条");
        assert_eq!(p.messages[0].id, "m3");
        assert_eq!(p.messages[1].id, "m4");
        assert_eq!(p.messages[1].text.len(), 400_000, "最新条整条保留不截头");
        assert!(!p.messages[1].text.contains("已截断"), "无单条截断标记");
        assert!(p.truncated, "裁旧条必须置位 truncated");
    }

    #[test]
    fn global_mirror_budget_bounds_all_mirrors() {
        // C4：全体总预算——两镜像合计超上限 → 后到者 Capacity + stale；内存释放后
        // 可再收（不靠 OOM 证明有界）
        let ledger = Arc::new(std::sync::Mutex::new(MirrorLedger::with_cap(300)));
        let mut a = ThreadMirror::with_ledger(key(), ledger.clone());
        let mut b = ThreadMirror::with_ledger(
            ThreadKey {
                host_id: "local".into(),
                thread_id: "00000000-0000-4000-8000-0000000000m2".into(),
            },
            ledger.clone(),
        );
        let snap = |rev: u64, blob: &str| {
            serde_json::json!({
                "type": "snapshot", "revision": rev,
                "conversationState": { "turns": [], "requests": [], "big": blob }
            })
        };
        a.apply(&lease(), &snap(1, &"x".repeat(200))).unwrap();
        let after_a = ledger.lock().unwrap().total();
        // B 的 200B 会让合计超 300 → 拒绝（账目不变）
        let err = b.apply(&lease(), &snap(1, &"y".repeat(200))).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(b.is_stale());
        assert_eq!(ledger.lock().unwrap().total(), after_a, "拒绝不部分生效");
        // A 缩小 → B 可再收（合计仍 < 上限）
        a.apply(&lease(), &snap(2, "small")).unwrap();
        b.apply(&lease(), &snap(2, &"y".repeat(200))).unwrap();
        let after_b = ledger.lock().unwrap().total();
        assert!(after_b < 300 && after_b > 200, "合计有界：{after_b}");
        // Drop 释放占用（账目只减不增）
        drop(a);
        let after_drop = ledger.lock().unwrap().total();
        assert!(after_drop < after_b, "Drop 释放：{after_drop} < {after_b}");
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
    fn turn_terminal_evidence_requires_the_bound_turn_itself() {
        // 2026-10-17 复审 C1：终态证据按 **turnId 精确查询**——本 turn inProgress、
        // 实体不在场、镜像 stale 一律不是证据；跨 turn「完成不互为证据」的负例在
        // router 层钉死（release 只查绑定 id，见 codex_control_remote 的
        // turn_restriction_needs_matching_completion_evidence）。
        // 2026-10-18 形态适配：实体按真实形态（无 type、turnId 字段、entries.value
        // 引用、键与 id 解耦）。
        let mut m = ThreadMirror::new(key());
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] },
                        "turn:other": { "turnId": "turn-other",
                                        "status": "completed", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" },
                        { "key": "turn:other", "value": "turn:other" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("turn-mine"),
            "本 turn inProgress 不是证据"
        );
        assert!(
            m.turn_is_terminal("turn-other"),
            "turn-other 自身已终态（对它自己的绑定就是证据——精确按 id，不模糊）"
        );
        assert!(!m.turn_is_terminal("turn-absent"), "实体不在场不是证据");
        // 本 turn 转 completed → 证据成立
        let done = serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:mine", "status"],
                  "value": "completed" }
            ]
        });
        m.apply(&lease(), &done).unwrap();
        assert!(
            m.turn_is_terminal("turn-mine"),
            "绑定 turn 以终态在场 = 证据"
        );
        // failed 也是终态
        assert_eq!(m.turn_status("turn-mine").as_deref(), Some("completed"));
        // stale 镜像一律无证据
        m.mark_stale();
        assert!(!m.turn_is_terminal("turn-mine"), "stale 镜像不出证据");
    }

    #[test]
    fn unknown_item_kind_projects_safe_summary_only() {
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                    "items": [ { "type": "mcp_tool_call", "id": "raw1",
                                        "arguments": { "cmd": "secret-cmd", "token": "TKN" } } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1", "value": "turn:t1" } ] }]
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

    // ===== contentState（内容健康状态，与写能力正交）=====

    #[test]
    fn content_state_ready_only_when_turns_and_items_complete() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert_eq!(m.project(caps()).content_state, ContentState::Ready);
        // history.isComplete=false → Loading（hydrate 判据同源）
        let mut incomplete = observed_change("canonical-before-load.sanitized.json");
        incomplete["conversationState"]["turnHistory"]["history"]["isComplete"] =
            serde_json::json!(false);
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &incomplete).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Loading);
        // Turn 列表完整但 items 分页未载完 → Partial
        let mut partial = observed_change("canonical-before-load.sanitized.json");
        partial["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &partial).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Partial);
        // 无 turnHistory.history（形态不可读）→ Unsupported（不冒充空历史）
        let mut broken = observed_change("canonical-before-load.sanitized.json");
        broken["conversationState"]
            .as_object_mut()
            .unwrap()
            .remove("turnHistory");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &broken).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Unsupported);
        // 尚无任何 snapshot → Loading
        let empty = ThreadMirror::new(key());
        assert_eq!(empty.project(caps()).content_state, ContentState::Loading);
    }

    // ===== 2026-10-18 真机金样（observed/）：canonical 实体形态适配 =====
    // 证据来源与形态事实见 observed/README.md。以下红→绿基线针对本轮修复：
    // 实体按合成键索引（turn:/tail: 前缀）、turn 实体无 type 以 turnId 字段标识、
    // islands.entries 为 {key,value} 对象、items 为内联 camelCase 对象、
    // isComplete 在 history 层。派生负例（改字段构造）在测试名以 derived 标注。

    fn observed_change(name: &str) -> Value {
        fixture(&format!("observed/{name}"))
    }

    #[test]
    fn observed_golden_target_turn_terminal_evidence() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        // 目标 turn 实体键是 turn:<uuid>：证据按实体 turnId 字段认领（键不是 id）
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000006")
                .as_deref(),
            Some("completed"),
            "真实形态下必须能按 turnId 字段读到终态（旧读者查键恒 None）"
        );
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "屏障放行的完成证据在真机形态下必须成立"
        );
        // tail 合成键实体：键尾 UUID(…0021) ≠ turnId(…0011)——只能按字段认领
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000011")
                .as_deref(),
            Some("completed"),
            "从键尾猜业务 ID 必然失配，按字段必中"
        );
        assert!(!m.turn_is_terminal("00000000-0000-4000-8000-000000000099"));
    }

    #[test]
    fn observed_golden_projection_inline_camelcase_items() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let conversation: Vec<(String, String, String)> = p
            .messages
            .iter()
            .filter(|msg| msg.role != "system")
            .map(|msg| (msg.id.clone(), msg.role.clone(), msg.text.clone()))
            .collect();
        assert_eq!(
            conversation.len(),
            6,
            "6 条用户/助手正文（reasoning 过滤；error 走系统摘要不计正文）"
        );
        let ids: Vec<&str> = p.messages.iter().map(|msg| msg.id.as_str()).collect();
        // Task 6 起：有 `params.input` 且正式 userMessage.clientId 与之相等的**逻辑
        // 开场消息**使用 turnId+clientUserMessageId 的投影身份（保证「后备 → 正式」同
        // 一条消息）；其余 item 仍用 item.id。canonical 顺序与 error 摘要不变。
        assert_eq!(
            ids,
            [
                "00000000-0000-4000-8000-000000000002/opening/00000000-0000-4000-8000-000000000001",
                "00000000-0000-4000-8000-000000000004",
                "00000000-0000-4000-8000-000000000006/opening/00000000-0000-4000-8000-000000000005",
                "00000000-0000-4000-8000-000000000009",
                "00000000-0000-4000-8000-000000000011/opening/00000000-0000-4000-8000-000000000010",
                "00000000-0000-4000-8000-000000000013",
                "00000000-0000-4000-8000-000000000014",
                "00000000-0000-4000-8000-000000000015",
                "00000000-0000-4000-8000-000000000016",
                "00000000-0000-4000-8000-000000000017",
            ],
            "逻辑开场 ID（opening）与 item.id 两类标识，canonical 顺序（error 保留为系统摘要）"
        );
        assert_eq!(conversation[0].1, "user");
        assert_eq!(conversation[0].2, "Synthetic user 1");
        assert_eq!(conversation[1].1, "assistant");
        assert_eq!(conversation[1].2, "Synthetic reply 1");
        assert_eq!(conversation[5].2, "Synthetic reply 3");
        assert!(p.messages[5].kind == "error" && p.messages[5].role == "system");
        let rendered = serde_json::to_string(&p).unwrap();
        assert!(!rendered.contains("text_elements"), "块级包装字段不下发");
    }

    #[test]
    fn observed_golden_reload_snapshot_keeps_ids_stable() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let first = m.project(caps());
        // load-complete-history 后的重放 snapshot（rev 4）：同内容不得追加重复气泡
        m.apply(
            &lease(),
            &observed_change("canonical-after-load.sanitized.json"),
        )
        .unwrap();
        let second = m.project(caps());
        assert_eq!(second.messages.len(), first.messages.len(), "reload 不追加");
        assert_eq!(second.messages, first.messages, "id 与顺序稳定");
    }

    #[test]
    fn observed_derived_busy_fallback_sees_real_inprogress_turn() {
        // 派生负例：真实结构 + turn 0011 置 inProgress（SYNTHETIC derived）
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["tail:0:local:00000000-0000-4000-8000-000000000021"]["status"] =
            serde_json::json!("inProgress");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.runtime_status, "active",
            "真实 inProgress turn 必须计入忙态（旧读者要求 type=turn 恒漏）"
        );
    }

    #[test]
    fn observed_derived_patch_recomputes_completeness() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert!(m.is_history_complete());
        // 派生 patch：history.isComplete → false；采用后必须按新 state 重算
        let patch = serde_json::json!({
            "type": "patches", "baseRevision": 3, "revision": 4,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "isComplete"],
                  "value": false }
            ]
        });
        m.apply(&lease(), &patch).unwrap();
        assert!(
            !m.is_history_complete(),
            "patch 采用后完整性必须从新 state 重算（不得沿用旧缓存 true）"
        );
    }

    #[test]
    fn observed_derived_missing_iscomplete_is_not_complete() {
        // 派生：删去 history.isComplete——缺字段 ≠ 完整
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]
            .as_object_mut()
            .unwrap()
            .remove("isComplete");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.is_history_complete(),
            "缺 isComplete 不得默认完整（ hydrate 触发判据）"
        );
    }

    #[test]
    fn observed_derived_unreferenced_entity_is_not_evidence() {
        // 派生：目标 turnId 只出现在未被 islands 引用的孤立实体上——不放行
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        let entities = hist["entitiesByKey"].as_object_mut().unwrap();
        let target = entities["turn:00000000-0000-4000-8000-000000000006"].clone();
        entities.remove("turn:00000000-0000-4000-8000-000000000006");
        entities.insert("orphan".into(), target);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "未被 canonical 引用的孤立实体不得作为完成证据"
        );
    }

    /// 在 observed 金样上为目标 Turn 追加一个副本（同 turnId，可指定 status/items；
    /// `prepend` 把副本引用插到 canonical 序列最前——顺序敏感性回归用）。
    fn with_target_copy(prepend: bool, status: Option<&str>, items: Value) -> Value {
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        {
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            let mut entity = serde_json::json!({
                "turnId": "00000000-0000-4000-8000-000000000006",
                "items": items,
            });
            if let Some(st) = status {
                entity["status"] = serde_json::json!(st);
            }
            entities.insert("tail:9:local:dupref".into(), entity);
        }
        let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
        let reference = serde_json::json!({
            "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
        });
        if prepend {
            entries.insert(0, reference);
        } else {
            entries.push(reference);
        }
        change
    }

    #[test]
    fn observed_derived_duplicate_turn_reference_dedup_and_ambiguity() {
        // 报告 §4：两个被引用实体**都携带同一个 item ID**——只有生产去重逻辑
        // 在场才能通过本条（旧断言只在副本里放 dup-item，删掉 seen_ids 也能绿）
        let shared = serde_json::json!([{ "type": "agentMessage", "id": "dup-item",
                                          "text": "重复气泡候选" }]);
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut change["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities["turn:00000000-0000-4000-8000-000000000006"]["items"] = shared.clone();
            entities.insert(
                "tail:9:local:dupref".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000006",
                    "status": "completed",
                    "items": shared,
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            entries.push(serde_json::json!({
                "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
            }));
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
        let p = m.project(caps());
        let dup: Vec<_> = p
            .messages
            .iter()
            .filter(|msg| msg.id == "dup-item")
            .collect();
        assert_eq!(dup.len(), 1, "双引用重叠 items 按去重，不追加重复气泡");

        // 空占位副本在前 + 原体正文在后：去重不得吞掉原体已加载的正文（顺序断言）
        let mut ordered = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut ordered["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities.insert(
                "tail:9:local:emptycopy".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000011",
                    "items": []
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            let pos = entries
                .iter()
                .position(|e| e["value"] == "tail:0:local:00000000-0000-4000-8000-000000000021")
                .expect("tail turn entry");
            entries.insert(
                pos,
                serde_json::json!({
                    "key": "tail:9:local:emptycopy", "value": "tail:9:local:emptycopy"
                }),
            );
        }
        let mut m_ordered = ThreadMirror::new(key());
        m_ordered.apply(&lease(), &ordered).unwrap();
        let p2 = m_ordered.project(caps());
        assert_eq!(
            p2.messages
                .iter()
                .filter(|msg| msg
                    .id
                    .ends_with("/opening/00000000-0000-4000-8000-000000000010"))
                .count(),
            1,
            "空占位副本在前不得遮蔽/重复原体已加载正文（Task 6 起该逻辑消息用 opening ID）"
        );
    }

    #[test]
    fn observed_derived_multi_copy_status_is_order_independent() {
        // 报告 2.1（P1）：缺 status 副本出现在前/后必须同判——多义一律无证据
        for prepend in [true, false] {
            let mut m = ThreadMirror::new(key());
            m.apply(
                &lease(),
                &with_target_copy(prepend, None, serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "缺 status 副本（prepend={prepend}）在场即多义，不得按另一副本放行"
            );
            let mut m2 = ThreadMirror::new(key());
            m2.apply(
                &lease(),
                &with_target_copy(prepend, Some("inProgress"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "状态冲突（prepend={prepend}）不得放行"
            );
            let mut m3 = ThreadMirror::new(key());
            m3.apply(
                &lease(),
                &with_target_copy(prepend, Some("completed"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                m3.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "多副本同 completed 可认定终态（prepend={prepend}）"
            );
        }
    }

    #[test]
    fn observed_derived_structure_errors_are_not_ready_empty() {
        // 报告 2.3：结构错误不得 Ready + 0 条（不冒充真空会话）
        let mut bad_entries = observed_change("canonical-before-load.sanitized.json");
        bad_entries["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!(["not-an-object"]);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &bad_entries).unwrap();
        assert_eq!(
            m.project(caps()).content_state,
            ContentState::Unsupported,
            "非法 entries 必须 Unsupported，不得 Ready+0 条"
        );
        let mut bad_items = observed_change("canonical-before-load.sanitized.json");
        bad_items["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["items"] =
            serde_json::json!("not-an-array");
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &bad_items).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Unsupported);
        let mut dangling = observed_change("canonical-before-load.sanitized.json");
        dangling["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"][0]
            ["value"] = serde_json::json!("no-such-entity");
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &dangling).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Unsupported);
        let mut not_loaded = observed_change("canonical-before-load.sanitized.json");
        not_loaded["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsView"] =
            serde_json::json!("notLoaded");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &not_loaded).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Partial);
        // 合法空 canonical 历史（真空会话）仍 Ready
        let empty = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {}, "islands": [{ "entries": [] }]
                } }
            }
        });
        let mut m5 = ThreadMirror::new(key());
        m5.apply(&lease(), &empty).unwrap();
        let p = m5.project(caps());
        assert_eq!(p.content_state, ContentState::Ready, "合法空历史=Ready");
        assert!(p.messages.is_empty());
    }

    #[test]
    fn observed_derived_items_incomplete_needs_hydration() {
        // 报告 2.2：列表完整 + items 未加载 → needs_hydration 必须为真
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.is_history_complete(), "列表完整性不变");
        assert!(
            m.needs_hydration(),
            "items 未加载必须触发只读 hydrate（旧判据只看列表完整性）"
        );
        // 目标终态证据不受其他 Turn 正文缺口影响
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
    }

    #[test]
    fn observed_derived_entry_reference_is_value_not_key() {
        // CODE 派生（Desktop 自身读法）：引用取 entry.value；key 不参与查找
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            let target = entries.remove(1); // turn:…006
            assert_eq!(target["value"], "turn:00000000-0000-4000-8000-000000000006");
            // key 改成无关值、value 不变 → 仍按 value 引用
            entries.insert(
                1,
                serde_json::json!({ "key": "unrelated-ref", "value": target["value"] }),
            );
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "entry.key 与 value 不同时按 value 引用（Desktop 同款读法）"
        );
        // value 指向不存在的实体 → 无证据（key 命中也不算）
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            entries[1] = serde_json::json!({ "key": "turn:00000000-0000-4000-8000-000000000006",
                                             "value": "no-such-entity" });
        }
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &change).unwrap();
        assert!(
            !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "引用失效 = 无证据；key 不兜底"
        );
    }

    // ============================================================
    // Task 6：已有 Turn 开场用户消息的显示后备
    // ============================================================

    /// 合成 canonical state（键 = "turn:<id>"，实体按 turnId 字段认领）
    fn opening_state(turns: Vec<(&str, Value, Value)>) -> Value {
        let mut map = serde_json::Map::new();
        let mut entries = Vec::new();
        for (tid, params, items) in turns {
            let key = format!("turn:{tid}");
            map.insert(
                key.clone(),
                serde_json::json!({
                    "turnId": tid,
                    "status": "inProgress",
                    "params": params,
                    "items": items,
                }),
            );
            entries.push(serde_json::json!({ "key": key, "value": key }));
        }
        serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": Value::Object(map),
                    "islands": [{ "entries": entries }],
                }
            }
        })
    }

    fn project_of(state: Value) -> ThreadProjection {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &serde_json::json!({ "type": "snapshot", "revision": 1, "conversationState": state }),
        )
        .unwrap();
        m.project(caps())
    }

    fn input_params(client: &str, text: &str) -> Value {
        serde_json::json!({
            "clientUserMessageId": client,
            "input": [{ "type": "text", "text": text }],
        })
    }

    /// 正式用户项到达后，同一逻辑消息沿用开场投影 ID（后备 → 正式替换，不追加第二条）
    #[test]
    fn opening_input_then_user_message_keeps_one_identity() {
        let fallback = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]));
        assert_eq!(fallback.messages.len(), 1, "只有开场后备一条");
        assert_eq!(fallback.messages[0].id, "t1/opening/c1");
        assert_eq!(fallback.messages[0].role, "user");
        assert_eq!(fallback.messages[0].text, "hello");

        let formal = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([{
                "type": "userMessage",
                "id": "m1",
                "clientId": "c1",
                "content": [{ "type": "text", "text": "hello" }],
            }]),
        )]));
        assert_eq!(formal.messages.len(), 1, "正式项压住后备，不重复");
        assert_eq!(
            formal.messages[0].id, "t1/opening/c1",
            "同 clientUserMessageId ⇒ 同一投影身份（replace 不追加气泡）"
        );
        assert_eq!(formal.messages[0].text, "hello");
    }

    /// 跨 Turn 相同正文不合并（ID 含 turnId，天然不同）
    #[test]
    fn opening_does_not_merge_equal_text_across_turns() {
        let p = project_of(opening_state(vec![
            ("t1", input_params("c1", "same"), serde_json::json!([])),
            ("t2", input_params("c2", "same"), serde_json::json!([])),
        ]));
        let users: Vec<&str> = p
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(users, ["same", "same"], "两条独立开场，不按正文去重");
    }

    /// null turnId / 未被引用的孤立实体不产生用户后备
    #[test]
    fn opening_ignores_null_turn_and_unreferenced_entity() {
        let state = serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": {
                        "turn:null": {
                            "turnId": null,
                            "params": {
                                "clientUserMessageId": "c1",
                                "input": [{ "type": "text", "text": "orphan" }]
                            },
                            "items": []
                        },
                        "turn:orphan": {
                            "turnId": "orphan",
                            "params": {
                                "clientUserMessageId": "c2",
                                "input": [{ "type": "text", "text": "unreferenced" }]
                            },
                            "items": []
                        }
                    },
                    "islands": [{ "entries": [] }]
                }
            }
        });
        let p = project_of(state);
        assert!(
            p.messages.is_empty(),
            "未验明身份的实体不得产出用户后备: {:?}",
            p.messages
        );
    }

    /// 键迁移副本：同一 turnId 两实体，一个只有开场、一个已有正式项 → 正式项胜出且仅一条
    #[test]
    fn opening_formal_copy_wins_during_key_migration() {
        let mut state = opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]);
        // 第二份副本（另一个键）带正式 userMessage
        let entities = state["turnHistory"]["history"]["entitiesByKey"]
            .as_object_mut()
            .unwrap();
        entities.insert(
            "tail:0:local:t1".to_string(),
            serde_json::json!({
                "turnId": "t1",
                "status": "inProgress",
                "params": input_params("c1", "hello"),
                "items": [{
                    "type": "userMessage",
                    "id": "m1",
                    "clientId": "c1",
                    "content": [{ "type": "text", "text": "hello" }],
                }],
            }),
        );
        let entries = state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        entries.push(serde_json::json!({ "key": "tail:0:local:t1", "value": "tail:0:local:t1" }));

        let p = project_of(state);
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(users.len(), 1, "跨键副本不重复: {users:?}");
        assert_eq!(users[0].id, "t1/opening/c1");
        assert_eq!(users[0].text, "hello");
    }

    /// Task 6 真机可追溯样本（DERIVED，源 `raw/t4-1.jsonl` T4 rev3）：Turn 只有
    /// `params.input` 时投影出开场用户气泡。
    #[test]
    fn observed_derived_opening_falls_back_to_turn_input() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-opening.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert!(!users.is_empty(), "开场后备必须出现");
        assert!(
            users.iter().any(|m| m.id.contains("/opening/")),
            "后备 ID 必须带 /opening/ 身份: {users:?}"
        );
    }

    /// Task 6 忙态负例（DERIVED，源 `raw/t7-1.jsonl`）：第二次 start-turn 的 input 被
    /// 写进同一 Turn 的 params，但**没有新 userMessage** —— 该文本不得成为幽灵气泡。
    #[test]
    fn observed_derived_busy_second_prompt_is_not_a_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-busy-negative.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        assert!(
            !p.messages
                .iter()
                .any(|m| m.text.contains("Synthetic undelivered busy prompt")),
            "未落 Turn 的第二条 prompt 不得进投影: {:?}",
            p.messages
                .iter()
                .map(|m| (&m.id, &m.text))
                .collect::<Vec<_>>()
        );
    }
}
