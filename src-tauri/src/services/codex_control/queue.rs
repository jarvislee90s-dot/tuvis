//! 持久动作串行调度（Task 7）：`ActionStore` 包装 `DeviceStore`，在 `codex_actions`
//! 表上实现动作登记/派发/落账、普通消息持久 FIFO 与重启恢复，以及 PR #117 兼容
//! fence 的读写与只读核对（reconcile）。
//!
//! 锁纪律：`DeviceStore::with()` 不可重入——本类型每个公开方法恰好一次 `with()`
//! （SQL 全在闭包内完成），**绝不跨 await 持 DB 锁**（调度等待在 `CodexControlRouter`
//! 的 tokio 锁上）。DB 失败一律 `StoreError::Database` 向上传播，调用方（router）
//! 据此零投递。
use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::database::dao::codex_actions as dao;
use crate::database::dao::codex_actions::StopPauseRow;
use crate::inject::headless::codex::{production_queue_holds, QueueEvidence};
use crate::inject::normalize::compose_injection_flagged;
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
        ActionKind::FileChangeApproval => "file_change_approval",
        ActionKind::PermissionsApproval => "permissions_approval",
        ActionKind::UserInput => "user_input",
        // 打断不进动作账本（仅供 ActionContext/审计语义如实），映射仅为 match 穷尽。
        ActionKind::AsyncQuestion => "async_question",
        // 打断不进动作账本（仅供 ActionContext/审计语义如实），映射仅为 match 穷尽。
        ActionKind::Interrupt => "interrupt",
    }
}

/// T07：**会开启真实回合**的动作（Accepted/Unknown 时需持久回合屏障与对账）。
///
/// 普通消息与异步正式答题都开启回合；审批/问答（原生响应）不开启新回合。
pub fn action_opens_turn(kind: ActionKind) -> bool {
    matches!(kind, ActionKind::Send | ActionKind::AsyncQuestion)
}

/// T07：**普通消息 FIFO** 只接收 `send`（异步答题有独立入口，绝不借 FIFO 偷发
/// 信封；暂停/百科顺序语义不同）。
pub fn action_is_fifo_message(kind: ActionKind) -> bool {
    matches!(kind, ActionKind::Send)
}

fn kind_from_str(s: &str) -> Option<ActionKind> {
    match s {
        "send" => Some(ActionKind::Send),
        "command_approval" => Some(ActionKind::CommandApproval),
        "file_change_approval" => Some(ActionKind::FileChangeApproval),
        "permissions_approval" => Some(ActionKind::PermissionsApproval),
        "user_input" => Some(ActionKind::UserInput),
        "async_question" => Some(ActionKind::AsyncQuestion),
        "interrupt" => Some(ActionKind::Interrupt),
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

/// 毫秒级 UNIX 时间（本 crate 单点；remote 层装配的 claim 闭包同样消费）。
pub(crate) fn now_ms() -> i64 {
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
    /// 登记时冻结的最终 wire 文本（`signature_applied=Some(_)` 的行）；
    /// 旧行（None）为原文，派发时按升级前语义（恒带签名）组装一次。
    pub raw_text: String,
    /// 签名冻结值：Some(true)=已签名，Some(false)=裸正文，None=旧行。
    pub signature_applied: Option<bool>,
    pub client_message_id: String,
}

/// 动作账本（包装 `DeviceStore`；生产 = 全局库，测试 = 内存库注入）。
/// H04：重放判定用的账本快照（只读）。
pub struct ActionSnapshot {
    pub request_key: Option<String>,
    pub context_token: Option<String>,
    pub kind: String,
    pub payload_hash: String,
    pub receipt: Option<ActionReceipt>,
    pub state: crate::services::codex_desktop_ipc::types::ActionState,
}

pub struct ActionStore {
    store: DeviceStore,
}

/// 人工解除保守屏障的结果（2026-10-19 无出口屏障修复；HTTP 映射在 remote::codex_ipc：
/// NotFound→404，Released/AlreadyReleased→200，NotEligible→409）。
#[derive(Debug)]
pub enum BarrierRelease {
    /// 不存在 / 非属主设备（不给存在性预言机）
    NotFound,
    /// 本次解除成功（解除后的动作视图 + thread 标识——调用方据此清内存 Turn
    /// 顺序限制并接力 flush）
    Released {
        view: CodexActionView,
        thread_id: String,
    },
    /// 幂等命中：此前已人工解除（返回当前视图；零新写入零审计）
    AlreadyReleased {
        view: CodexActionView,
        thread_id: String,
    },
    /// 不在人工解除范围 → 409。`barrier_in_register` = 派发在飞且将有/已有自动落账
    /// 路径（保守屏障在册，或 compat 兜底回合在飞——handoff 与 fence 武装之间 fence
    /// 尚未落库的瞬态窗口也算，PR #165 终审 Minor：该窗口回 `no_barrier` 会误导排障）；
    /// false = 屏障已被自动路径解除/从未在册。
    NotEligible {
        view: CodexActionView,
        barrier_in_register: bool,
    },
}

/// S4：人工继续（撤销旧派发权 + 解除屏障 + 建唯一后继，同一持久事务内一致完成）
/// 的结果（HTTP 映射在 remote::codex_ipc）。
#[derive(Debug)]
pub enum ManualResume {
    /// 确认期间原动作已收敛（accepted/rejected/cancelled）→ 返回最新状态；
    /// **不建后继、不重发**（设计 §6.2「按过期确认自动再发」的禁止项）。
    Converged { view: CodexActionView },
    /// 幂等重放 / 并发输家：后继已存在。返回原动作视图 + 既有后继 actionId
    ///（输家收到已收敛的最新状态，其自带的新 actionId 不登记）。
    AlreadyResumed {
        view: CodexActionView,
        successor_action_id: String,
    },
    /// 复审 R-T1：写右已被 writer 取得（claim 胜出于撤销）——旧动作**可能已
    /// 投递**且派发尝试仍在途（未收尾）。不建后继、不重发；HTTP 映射为独立状态
    /// `write_right_in_flight`（复审 round 4 V2：**不得**返回 converged——「已收敛」
    /// 只能表达真实终态，在途尝试须保持待确认，用户稍后再核对/收尾后人工继续
    /// 仍可用）。
    WriteRightObtained { view: CodexActionView },
    /// 本次成功：原动作定格（Unknown 事实保持/补齐）+ 写出权撤销 + 屏障解除 +
    /// 后继行登记（queued），全部同一事务。`thread_id` 供调用方清内存 Turn 限制。
    Resumed {
        view: CodexActionView,
        successor_action_id: String,
        thread_id: String,
    },
    /// 不存在（HTTP 404，不给存在性预言机之外的语义）。
    NotFound,
    /// 原动作仍 queued → 走既有撤回（HTTP 409 `queued_use_retract`）。
    Queued { view: CodexActionView },
    /// 正文与原动作 payload 不符（HTTP 409 `manual_resume_text_mismatch`；
    /// 「同 ID 异载荷拒绝」的人工继续面）。
    TextMismatch { view: CodexActionView },
}

fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Database(e.to_string())
}

impl ActionStore {
    /// 构造并执行启动恢复：reserved → unknown（writer 已接手而进程消失 = 投递结果
    /// 未知，绝不回 queued）；queued 行不受影响，仍按序可发。
    /// PR #165 复审 Important：启动恢复时同跑一轮终态行保留清理（墓碑化，批量分批，
    /// 快 SQL；超期存量由后续周期对账逐轮收敛）。
    pub fn new(store: DeviceStore) -> Self {
        let s = Self { store };
        match s.recover_reserved() {
            Ok(0) => {}
            Ok(n) => log::warn!("codex_actions: 启动恢复 {n} 条 reserved 动作为 unknown"),
            Err(e) => log::error!("codex_actions: 启动恢复失败: {e}"),
        }
        let tombstoned = s.tombstone_expired_terminal();
        if tombstoned > 0 {
            log::info!("codex_actions: 启动墓碑化 {tombstoned} 条超期终态行（行保留，重载荷清空）");
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

    /// T08：在**本 store 的库**上执行设置读写（与动作账本/适配器同一 DB；不碰全局
    /// 单例——测试注入的 store 与生产 DeviceStore 走同一路径）。
    pub fn with_settings<R>(&self, f: impl FnOnce(&Connection) -> R) -> Result<R, StoreError> {
        self.with(|conn| Ok(f(conn)))
    }

    /// 登记动作（幂等）：同 action_id 且 device/thread/kind/requestKey/token/payload
    /// 全同 → 返回既有视图（原状态）；任一不同 → `Conflict`（零新增写入）。同时生成
    /// 并持久化 client_message_id（重试同 action 不换新 id）。
    pub fn register(
        &self,
        ctx: &ActionContext,
        kind: ActionKind,
        request_key: Option<&str>,
        context_token: Option<&str>,
        payload_hash: &str,
        raw_text: Option<&str>,
    ) -> Result<CodexActionView, StoreError> {
        self.register_signed(
            ctx,
            kind,
            request_key,
            context_token,
            payload_hash,
            raw_text,
            None,
        )
    }

    /// 带签名冻结值的登记（生产 router 使用；旧 6 参入口保留给既有测试）。
    #[allow(clippy::too_many_arguments)]
    pub fn register_signed(
        &self,
        ctx: &ActionContext,
        kind: ActionKind,
        request_key: Option<&str>,
        context_token: Option<&str>,
        payload_hash: &str,
        raw_text: Option<&str>,
        signature_applied: Option<bool>,
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
            let row = dao::insert_action_conn_signed(
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
                signature_applied,
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

    /// P11（2026-10-31 用户裁决）：接力用的队首——**跳过自身带屏障的行**。
    ///
    /// 屏障标记的是「这条动作投递结果不明，别自动重发它」。它保护的始终是那一行；
    /// 旧实现把「有未清屏障」当成整队停摆，导致一条历史 unknown 让整个会话队列
    /// 永久冻结（实机：空闲也不发）。这里改为只跳过带屏障的那一条，队列其余消息
    /// 照常接力。
    ///
    /// 注意：终态行（unknown/accepted）本就不在 `state='queued'` 里，天然被排除；
    /// 本变体额外覆盖「queued 状态下被人工/异常置了屏障」的形态。
    pub fn next_queued_skip_barriered(
        &self,
        key: &ThreadKey,
    ) -> Result<Option<QueuedAction>, StoreError> {
        self.with(|c| {
            let Some(row) = dao::next_queued_unbarriered_conn(c, &key.thread_id).map_err(db_err)?
            else {
                return Ok(None);
            };
            Ok(Some(queued_action_of(row)?))
        })
    }

    /// 按 action_id 读取单条 queued 行（S2 一次性越队：绕过 FIFO 队首直接派发
    /// 授权动作时取得其发送输入）。非 queued / 不存在 → None（越队只对刚登记的
    /// queued 行有意义）。
    pub fn queued_action_by_id(&self, action_id: &str) -> Result<Option<QueuedAction>, StoreError> {
        self.with(|c| {
            let Some(row) = dao::get_row(c, action_id).map_err(db_err)? else {
                return Ok(None);
            };
            if row.state != ActionState::Queued {
                return Ok(None);
            }
            Ok(Some(queued_action_of(row)?))
        })
    }

    /// CAS queued→reserved（与审计起始行同事务；DB 失败零投递）。false = CAS 失败
    /// （已撤/已派发），绝不覆盖既有状态。
    pub fn reserve(&self, action_id: &str, route: ControlRoute) -> Result<bool, StoreError> {
        self.with(|c| dao::reserve_conn(c, action_id, route, now_ms()).map_err(db_err))
    }

    /// 自然空闲/明确恢复时一次原子领取并冻结普通消息批次。
    pub fn reserve_send_batch(&self, action_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| dao::reserve_send_batch_conn(c, action_id, now_ms()).map_err(db_err))
    }

    /// 核对/人工继续必须沿同一 start-turn 的 canonical action 操作。
    pub fn canonical_batch_action_id(&self, action_id: &str) -> Result<String, StoreError> {
        self.with(|c| {
            c.query_row(
                "SELECT leader_id FROM codex_send_batch_members WHERE member_id=?1",
                [action_id],
                |r| r.get(0),
            )
            .optional()
            .map(|id| id.unwrap_or_else(|| action_id.to_string()))
            .map_err(db_err)
        })
    }

    /// 人工继续的正文校验使用整个冻结批次，不能只重投其中一员。
    pub fn batch_wire_text(&self, action_id: &str) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT m.wire_text FROM codex_send_batch_members m JOIN codex_actions a ON a.action_id=m.member_id WHERE m.leader_id=?1 ORDER BY a.seq").map_err(db_err)?;
            let texts = stmt.query_map([action_id], |r| r.get::<_, String>(0)).map_err(db_err)?.collect::<Result<Vec<_>,_>>().map_err(db_err)?;
            Ok((!texts.is_empty()).then(|| texts.join("\n\n")))
        })
    }

    /// S2：入队前拒绝落账（queued → rejected CAS；写尝试未开始）。false = 已离开
    /// queued（竞态），不覆盖。
    pub fn finish_queued_refusal(&self, receipt: &ActionReceipt) -> Result<bool, StoreError> {
        self.with(|c| dao::finish_queued_refusal_conn(c, receipt, now_ms()).map_err(db_err))
    }

    /// 落账回执（仅 reserved 可落账；重复 finish = `Conflict`）。NotSent 同事务清
    /// compat fence（零投递动作没有 Turn，不等待 Turn 结束）。
    pub fn finish(&self, receipt: &ActionReceipt) -> Result<(), StoreError> {
        self.with(|c| dao::finish_conn(c, receipt, now_ms()).map_err(db_err))
    }

    /// 写前忙回队（reserved→queued，恢复 wire 文本；CAS 失败返回 false）。
    pub fn requeue_after_busy(
        &self,
        action_id: &str,
        wire_text: &str,
        signature_applied: bool,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            dao::requeue_after_busy_conn(c, action_id, wire_text, signature_applied, now_ms())
                .map_err(db_err)
        })
    }

    /// S3：迟到证据归账（reserved/unknown → 真实终态的单次 CAS；已终态 = false，
    /// 同一证据只收敛一次）。消费方：派发任务的迟到等待 + 调度循环对账兜底。
    pub fn finish_late_result(&self, receipt: &ActionReceipt) -> Result<bool, StoreError> {
        self.with(|c| dao::finish_late_result_conn(c, receipt, now_ms()).map_err(db_err))
    }

    // ---- S4：人工继续 / 派发权撤销 / 补绑 ----

    /// 读派发权撤销标记（写出边界检查：旧派发任务在真正 start-turn 前必查）。
    pub fn has_write_revoke(&self, action_id: &str) -> bool {
        self.with(|c| dao::has_write_revoke_conn(c, action_id).map_err(db_err))
            .unwrap_or(false)
    }

    /// 人工继续撤销臂的定格落账（T 写出边界观察到撤销标记时；不写帧、不置屏障）。
    pub fn finish_manual_resume_unknown(
        &self,
        receipt: &ActionReceipt,
    ) -> Result<bool, StoreError> {
        self.with(|c| dao::finish_manual_resume_unknown_conn(c, receipt, now_ms()).map_err(db_err))
    }

    /// S4 补绑：accepted 且屏障在册但缺 turnId 的行，经正式 userMessage 回显找到
    /// Turn 后补绑 Turn 身份（只补绑定槽位，不改写已受理事实）。false = 已绑定 /
    /// 无屏障 / 已人工解除。
    pub fn rebind_turn(&self, action_id: &str, turn_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::rebind_turn_conn(c, action_id, turn_id, now_ms()).map_err(db_err))
    }

    /// H05：落账与同题占用**同一事务**结算（崩溃不留半状态）。
    pub fn finish_and_settle_claim(
        &self,
        receipt: &ActionReceipt,
        turn_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            crate::database::dao::codex_actions::finish_and_settle_claim_conn(
                c,
                receipt,
                turn_id,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    /// H01：把**原动作捕获**的效果目标绑定到动作行（register 后、派发前）。
    pub fn bind_effect_target(
        &self,
        action_id: &str,
        target: &crate::services::codex_desktop_ipc::capability::ObservedTarget,
    ) -> bool {
        self.with(|c| {
            crate::database::dao::codex_actions::bind_effect_target_conn(
                c,
                action_id,
                &target.probe_key,
                &target.package_sha256,
                crate::services::codex_desktop_ipc::capability::CONTRACT_RULE_VERSION,
            )
            .map_err(db_err)
        })
        .unwrap_or(false)
    }

    /// H01：**按原动作归属**结算一次成功证据（同事务写成功表 + 清归属）。
    ///
    /// 不读取“当前目标”——只看动作行在准备时捕获的目标；没有绑定 → None（不猜）。
    pub fn settle_confirmed_effect(
        &self,
        action_id: &str,
    ) -> Option<crate::services::codex_desktop_ipc::capability::CodexOperation> {
        use crate::services::codex_desktop_ipc::capability::CodexOperation;
        self.with(|c| {
            let target = crate::database::dao::codex_actions::take_effect_target_conn(c, action_id)
                .map_err(db_err)?;
            let Some((probe_key, package_sha256, rule_version)) = target else {
                return Ok(None);
            };
            let kind: String = c
                .query_row(
                    "SELECT kind FROM codex_actions WHERE action_id = ?1",
                    [action_id],
                    |r| r.get(0),
                )
                .map_err(db_err)?;
            let operation = match kind.as_str() {
                "command_approval" => CodexOperation::CommandApproval,
                "file_change_approval" => CodexOperation::FileChangeApproval,
                "user_input" => CodexOperation::BlockingInput,
                "async_question" => CodexOperation::AsyncAnswerIdle,
                "send" => CodexOperation::Send,
                _ => return Ok(None),
            };
            let tx = c.unchecked_transaction().map_err(db_err)?;
            crate::database::dao::codex_ipc_probe::record_operation_success_conn(
                &tx,
                &probe_key,
                &package_sha256,
                operation.as_str(),
                rule_version,
                now_ms(),
            )
            .map_err(db_err)?;
            crate::database::dao::codex_actions::clear_effect_target_conn(&tx, action_id)
                .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(Some(operation))
        })
        .ok()
        .flatten()
    }

    /// R02：同题持久排他占用（跨 actionId／设备／重启的唯一裁决点）。
    /// B05：同组多题一次占用（整组事务；任何一题被别端解决/占用 → 整组拒绝）。
    pub fn question_keys_for_action(&self, action_id: &str) -> Result<Vec<String>, StoreError> {
        self.with(|c| {
            crate::database::dao::codex_interactions::question_keys_for_action_conn(c, action_id)
                .map_err(db_err)
        })
    }

    /// G05：既有动作身份（不按设备过滤）——复用 ID 前必须先核设备/线程/载荷。
    pub fn action_identity(
        &self,
        action_id: &str,
    ) -> Option<(String, String, String, ActionSnapshot)> {
        self.with(|c| {
            let row = dao::get_row(c, action_id).map_err(db_err)?;
            Ok(row.map(|row| {
                (
                    row.device_id,
                    row.thread_id,
                    row.payload_hash.clone(),
                    ActionSnapshot {
                        request_key: row.request_key,
                        context_token: row.context_token,
                        kind: row.kind,
                        payload_hash: row.payload_hash,
                        receipt: row.receipt,
                        state: row.state,
                    },
                )
            }))
        })
        .ok()
        .flatten()
    }

    /// G05：按本次新增 keys 收尾占用（不按 action_id 全量清扫）。
    pub fn settle_group_claim_keys(
        &self,
        thread_id: &str,
        action_id: &str,
        claim_owner: &str,
        question_keys: &[String],
        delivery: &str,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            crate::database::dao::codex_interactions::settle_group_claim_keys_conn(
                c,
                thread_id,
                action_id,
                claim_owner,
                question_keys,
                delivery,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    pub fn claim_question_group(
        &self,
        thread_id: &str,
        action_id: &str,
        claim_owner: &str,
        entries: &[crate::database::dao::codex_interactions::GroupClaimEntry<'_>],
        facts: &crate::database::dao::codex_interactions::ClaimFacts<'_>,
    ) -> Result<crate::database::dao::codex_interactions::GroupClaimOutcome, StoreError> {
        self.with(|c| {
            crate::database::dao::codex_interactions::claim_question_group_conn(
                c,
                thread_id,
                action_id,
                claim_owner,
                entries,
                &crate::database::dao::codex_interactions::ClaimFacts {
                    now_ms: now_ms(),
                    ..*facts
                },
            )
            .map_err(db_err)
        })
    }

    pub fn claim_question(
        &self,
        thread_id: &str,
        question_key: &str,
        action_id: &str,
        claim_owner: &str,
        facts: &crate::database::dao::codex_interactions::ClaimFacts<'_>,
    ) -> Result<crate::database::dao::codex_interactions::QuestionClaim, StoreError> {
        self.with(|c| {
            crate::database::dao::codex_interactions::claim_question_conn(
                c,
                thread_id,
                question_key,
                action_id,
                claim_owner,
                &crate::database::dao::codex_interactions::ClaimFacts {
                    now_ms: now_ms(),
                    ..*facts
                },
            )
            .map_err(db_err)
        })
    }

    /// R02：按动作终态收尾占用（见 DAO 规则）。
    pub fn settle_question_claim(&self, action_id: &str, delivery: &str) -> Result<(), StoreError> {
        self.with(|c| {
            crate::database::dao::codex_interactions::settle_question_claim_conn(
                c,
                action_id,
                delivery,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    /// 读动作的归属 thread 与正式消息身份（`client_message_id`）——S4 核对/人工继续
    /// 的输入缝（动作行无对外 thread/cmid 字段，经此单点读）。无行 → None。
    pub fn action_thread_and_cmid(&self, action_id: &str) -> Option<(String, String)> {
        self.with(|c| {
            let pair = c
                .query_row(
                    "SELECT thread_id, client_message_id FROM codex_actions WHERE action_id = ?1",
                    [action_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(db_err)?;
            Ok(pair)
        })
        .ok()
        .flatten()
    }

    /// 读动作的持久 Turn 绑定槽位（`compat_turn_id`；核对 DTO 与 force-continue
    /// 前置判定的单点读）。无行 → None。
    pub fn bound_turn_id(&self, action_id: &str) -> Option<String> {
        self.with(|c| {
            c.query_row(
                "SELECT compat_turn_id FROM codex_actions WHERE action_id = ?1",
                [action_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .map_err(db_err)
        })
        .ok()
        .flatten()
    }

    /// S4 人工继续（设计 §6.2）：撤销旧动作尚未取得的首次写出权 + 解除屏障 +
    /// 建唯一后继，**同一持久事务**内一致完成（DAO 层定序保证中间态不可见）。
    ///
    /// * 文本核验：`text` 的 payload hash 必须与原动作一致（服务器权威——后继重发
    ///   的就是原消息；不符 = 同 ID 异载荷拒绝，零写入）；
    /// * 后继唯一：`codex_manual_resume` 主键 CAS（幂等重放/并发输家 →
    ///   [`ManualResume::AlreadyResumed`] 携既有后继）；
    /// * 派发权撤销：`codex_write_revoke` 单调标记，旧派发任务/重启恢复/迟到 HTTP
    ///   在写出边界（真正 start-turn 前）必查；
    /// * 旧动作定格：unknown 终态清屏障保事实；reserved 在飞 CAS 成 unknown
    ///   （结果不确定事实保持，绝不 NotSent/Cancelled）且不置屏障；
    /// * 复核期间已收敛 → [`ManualResume::Converged`]（返回最新状态，不建后继）。
    #[allow(clippy::too_many_arguments)]
    pub fn manual_resume(
        &self,
        old_action_id: &str,
        device_id: &str,
        device_name: &str,
        new_action_id: &str,
        text: &str,
        reason: Option<&str>,
    ) -> Result<ManualResume, StoreError> {
        // 后继载荷与 wire 在**服务层**组装（与 router.send_inner 同源规则）：
        // payload hash 按原文（thread 维度取自原动作行——权威比对在 DAO 事务内用
        // 事务内读到的行重做，这里只为取得 thread 维度）；wire 按登记时签名设置冻结。
        let old_thread = self.with(|c| {
            Ok::<_, StoreError>(
                dao::get_row(c, old_action_id)
                    .map_err(db_err)?
                    .map(|r| r.thread_id),
            )
        })?;
        let Some(old_thread) = old_thread else {
            return Ok(ManualResume::NotFound);
        };
        let hash = payload_hash(
            ActionKind::Send,
            &ThreadKey {
                host_id: "local".into(),
                thread_id: old_thread,
            },
            None,
            None,
            &ActionPayload::Text(text.to_string()),
        );
        // 签名开关在**登记时**读一次并冻结（与 send_inner 同源）；读失败按关闭处理。
        let signed = self.message_signature_enabled();
        let wire = compose_injection_flagged(device_name, text, signed);
        let dao_r = self.with(|c| {
            dao::manual_resume_conn(
                c,
                old_action_id,
                device_id,
                device_name,
                new_action_id,
                &hash,
                &wire,
                signed,
                reason,
                now_ms(),
            )
            .map_err(db_err)
        })?;
        Ok(match dao_r {
            dao::ManualResumeDao::NotFound => ManualResume::NotFound,
            dao::ManualResumeDao::Converged(row) => ManualResume::Converged {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
            },
            dao::ManualResumeDao::AlreadyResumed(row, successor) => ManualResume::AlreadyResumed {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
                successor_action_id: successor,
            },
            dao::ManualResumeDao::WriteRightObtained(row) => ManualResume::WriteRightObtained {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
            },
            dao::ManualResumeDao::Resumed(row, thread_id) => ManualResume::Resumed {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
                successor_action_id: new_action_id.to_string(),
                thread_id,
            },
            dao::ManualResumeDao::Queued(row) => ManualResume::Queued {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
            },
            dao::ManualResumeDao::TextMismatch(row) => ManualResume::TextMismatch {
                view: self.with(|c| view_of(c, row).map_err(db_err))?,
            },
            dao::ManualResumeDao::Conflict => return Err(StoreError::Conflict),
        })
    }

    /// 落账回执**与** Turn 顺序屏障同一 UPDATE 原子持久化（2026-10-17 复审 C1.3）：
    /// A 路独立派发任务对 Accepted/UnknownDelivery 的唯一落账点——`turn_id` 有值绑
    /// Turn 身份，None 存保守屏障（unknown/缺 turnId），重启后限制同样保留。
    pub fn finish_with_turn_barrier(
        &self,
        receipt: &ActionReceipt,
        turn_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            dao::finish_with_turn_barrier_conn(c, receipt, turn_id, now_ms()).map_err(db_err)
        })
    }

    /// 本设备的动作视图（其他设备 / 不存在 → None；HTTP 层 404 语义）。
    /// H04：同 ID 重放的**账本快照**（设备隔离；用于零预算/幂等判定，不写）。
    pub fn snapshot_for_device(&self, action_id: &str, device_id: &str) -> Option<ActionSnapshot> {
        self.with(|c| {
            let row = dao::get_row(c, action_id).map_err(db_err)?;
            Ok(row
                .filter(|row| row.device_id == device_id)
                .map(|row| ActionSnapshot {
                    request_key: row.request_key,
                    context_token: row.context_token,
                    kind: row.kind,
                    payload_hash: row.payload_hash,
                    receipt: row.receipt,
                    state: row.state,
                }))
        })
        .ok()
        .flatten()
    }

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

    /// S4：无设备隔离的动作视图（核对核心的账本读——属主隔离由端点先行完成，
    /// 核对过程需要跨收敛步骤重读同一行）。无行 → None。
    pub fn get_for_device_unisolated(&self, action_id: &str) -> Option<CodexActionView> {
        self.with(|c| {
            let Some(row) = dao::get_row(c, action_id).map_err(db_err)? else {
                return Ok(None);
            };
            view_of(c, row).map(Some).map_err(db_err)
        })
        .ok()
        .flatten()
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

    /// 线程级是否存在 queued 普通消息（跨设备）——清“无队列的陈旧暂停”专用；
    /// 不得用本设备 `list_queued` 代替，否则会误清别台设备仍在排队的保护闩。
    pub fn has_queued_for_thread(&self, key: &ThreadKey) -> Result<bool, StoreError> {
        self.with(|c| {
            Ok(!dao::queued_for_thread_conn(c, &key.thread_id)
                .map_err(db_err)?
                .is_empty())
        })
    }

    /// 撤回：CAS queued→cancelled（仅属主设备），保留 actionId 墓碑、清正文并审计。
    /// reserved/accepted/unknown 不可撤（false）。
    pub fn retract(&self, action_id: &str, device_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::retract_conn(c, action_id, device_id, now_ms()).map_err(db_err))
    }

    /// 同锁读取正文并撤回；派发 CAS 先赢则绝不返回正文。
    pub fn retract_for_edit(
        &self,
        action_id: &str,
        device_id: &str,
        key: &ThreadKey,
    ) -> Result<Option<String>, StoreError> {
        self.with(|c| {
            let Some(row) = dao::get_row(c, action_id).map_err(db_err)? else {
                return Ok(None);
            };
            if row.device_id != device_id
                || row.thread_id != key.thread_id
                || row.kind != "send"
                || row.state != ActionState::Queued
            {
                return Ok(None);
            }
            let mut text = row.queued_text.unwrap_or_default();
            if row.signature_applied == Some(true)
                && !crate::inject::normalize::is_slash_message(&text)
            {
                let suffix = format!(
                    " [mobile {}]",
                    crate::inject::normalize::normalize_newlines(&row.device_name)
                );
                if let Some(body) = text.strip_suffix(&suffix) {
                    text = body.to_string();
                }
            }
            if !dao::retract_conn(c, action_id, device_id, now_ms()).map_err(db_err)? {
                return Ok(None);
            }
            Ok(Some(text))
        })
    }

    /// 「远程消息带设备签名」设置（缺省关）。登记动作时读一次并冻结，
    /// 之后排队/重启/重试都不再读——保证同一 actionId 的 wire 文本恒定。
    pub fn message_signature_enabled(&self) -> bool {
        self.store
            .with(|c| {
                Ok::<_, StoreError>(crate::inject::normalize::message_signature_enabled_conn(c))
            })
            .unwrap_or(false)
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

    /// P2（2026-10-30）：native 保守屏障的核对证据补录——只补 wire 文本 hash
    /// （尚未登记时写入；幂等，不覆盖 compat bridge 的 rollout 证据）。settle
    /// 路径的 native Unknown 屏障只读核对消费该字段。
    pub fn set_native_wire_evidence(
        &self,
        action_id: &str,
        wire_hash: &str,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            dao::set_native_wire_evidence_conn(c, action_id, wire_hash, now_ms()).map_err(db_err)
        })
    }

    /// 该 Thread 是否有在册 fence（fence 在册 = 队列不接力，界面说明需回 Desktop 核对）。
    pub fn has_compat_fence(&self, key: &ThreadKey) -> Result<bool, StoreError> {
        self.with(|c| dao::has_compat_fence_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 在册 fence 摘要（(route, state, 有绑定 turnId) 三元组）——阻塞原因三态派生。
    pub fn outstanding_fence_summary(
        &self,
        key: &ThreadKey,
    ) -> Result<Vec<(String, String, bool)>, StoreError> {
        self.with(|c| dao::outstanding_fence_summary_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 在册**原生**屏障绑定的 turnId（打断端点的 `expectedTurnId` 安全阀来源）。
    /// 无在册原生屏障 → None。
    pub fn outstanding_native_turn_id(
        &self,
        key: &ThreadKey,
    ) -> Result<Option<String>, StoreError> {
        self.with(|c| dao::outstanding_native_turn_id_conn(c, &key.thread_id).map_err(db_err))
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

    /// 人工解除保守屏障（仅属主设备；CAS 安全 + 幂等 + 单调，DAO 侧同事务审计）。
    pub fn force_release_barrier(
        &self,
        action_id: &str,
        device_id: &str,
    ) -> Result<BarrierRelease, StoreError> {
        self.with(|c| {
            match dao::force_release_barrier_conn(c, action_id, device_id, now_ms())
                .map_err(db_err)?
            {
                dao::ForceReleaseRow::NotFound => Ok(BarrierRelease::NotFound),
                dao::ForceReleaseRow::Released(row) => {
                    let thread_id = row.thread_id.clone();
                    Ok(BarrierRelease::Released {
                        view: view_of(c, row).map_err(db_err)?,
                        thread_id,
                    })
                }
                dao::ForceReleaseRow::AlreadyReleased(row) => {
                    let thread_id = row.thread_id.clone();
                    Ok(BarrierRelease::AlreadyReleased {
                        view: view_of(c, row).map_err(db_err)?,
                        thread_id,
                    })
                }
                dao::ForceReleaseRow::NotEligible(row) => {
                    // 屏障在册，或 compat 兜底回合在飞（fence 尚未武装的瞬态窗口也算
                    // ——该行即将走自动落账，报 no_barrier 会误导用户与排障）
                    let barrier_in_register = row.compat_outstanding
                        || (matches!(row.state, ActionState::Reserved)
                            && row.route == ControlRoute::HeadlessCompat);
                    Ok(BarrierRelease::NotEligible {
                        view: view_of(c, row).map_err(db_err)?,
                        barrier_in_register,
                    })
                }
            }
        })
    }

    /// 该 Thread 全部在册屏障的 action_id（队列视图 `blockedActionIds`——人工解除
    /// 端点的靶向清单，前端按钮据此逐个 force-continue）。
    pub fn barrier_action_ids(&self, key: &ThreadKey) -> Result<Vec<String>, StoreError> {
        self.with(|c| dao::barrier_action_ids_conn(c, &key.thread_id).map_err(db_err))
    }

    // ---- S2：打断后队列持久暂停 + 一次性越队授权 ----

    /// 置 Thread 队列暂停（打断受理/不确定时由打断端点调用；幂等，首次信息保留）。
    /// P3：`pause_turn_id` = 本次停止等待的目标回合（expectedTurnId 安全阀）——
    /// settle 路径按其终态证据自动清除停止类暂停行；None = 未绑定（保守保留）。
    pub fn set_queue_pause(
        &self,
        key: &ThreadKey,
        pause_reason: &str,
        pause_token: &str,
        pause_turn_id: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            dao::set_queue_pause_conn(
                c,
                &key.thread_id,
                pause_reason,
                pause_token,
                pause_turn_id,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    /// P3：该 Thread 的**停止类**暂停行 `(pause_token, pause_turn_id)`（按
    /// paused_at 升序）。settle 路径的自动解除逐行消费；token 为 NULL 的旧行
    /// 保守跳过（无法按 token 定向清除）。
    pub fn stop_pause_rows(&self, key: &ThreadKey) -> Result<Vec<StopPauseRow>, StoreError> {
        self.with(|c| dao::stop_pause_rows_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 终审 R3：撤回本次打断拥有的暂停（token 不匹配/行不存在 → false，不动他人）。
    pub fn clear_queue_pause_owned(
        &self,
        key: &ThreadKey,
        pause_token: &str,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            dao::clear_queue_pause_owned_conn(c, &key.thread_id, pause_token).map_err(db_err)
        })
    }

    /// 读 Thread 暂停状态（None = 未暂停）。
    pub fn queue_pause(&self, key: &ThreadKey) -> Result<Option<QueuePause>, StoreError> {
        self.with(|c| {
            Ok(dao::queue_pause_conn(c, &key.thread_id)
                .map_err(db_err)?
                .map(|(paused_at, pause_reason)| QueuePause {
                    paused_at,
                    pause_reason,
                }))
        })
    }

    /// 用户显式「继续发送」：解除暂停（唯一出口）。返回是否确有暂停被解除。
    pub fn clear_queue_pause(&self, key: &ThreadKey) -> Result<bool, StoreError> {
        self.with(|c| dao::clear_queue_pause_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 建一次性越队授权（绑定具体 action + thread；同 action 重放幂等）。
    pub fn create_bypass_grant(&self, action_id: &str, key: &ThreadKey) -> Result<(), StoreError> {
        self.with(|c| {
            dao::create_bypass_grant_conn(c, action_id, &key.thread_id, now_ms()).map_err(db_err)
        })
    }

    /// CAS 消费授权（写出前恰好一次）。false = 无此授权 / 跨 thread / 重复消费。
    pub fn consume_bypass_grant(
        &self,
        action_id: &str,
        key: &ThreadKey,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            dao::consume_bypass_grant_conn(c, action_id, &key.thread_id, now_ms()).map_err(db_err)
        })
    }

    /// R06：优先处理授权的「尝试」铸造（幂等 CAS）。无行 → attempt 1；未消费行
    /// → 不动（同一次授权的重放）；已消费行 → 重铸为下一 attempt（代际 +1、清
    /// 消费位）——调用前提是动作仍 queued（明确零投递回队后的下一次用户授权）。
    /// 与 [`Self::create_bypass_grant`]（越队发送路径，消费后不重铸）语义分叉。
    pub fn mint_bypass_grant(&self, action_id: &str, key: &ThreadKey) -> Result<(), StoreError> {
        self.with(|c| {
            dao::mint_bypass_grant_conn(c, action_id, &key.thread_id, now_ms()).map_err(db_err)
        })
    }

    /// C02：读停止尝试的占有状态（SQLite 持久，与动作行同生命周期、重启不丢）。
    /// 重放进入时先读：`Claimed | Unknown(_)` → 不得再发停止帧（既有 Unknown 语义）。
    pub fn stop_attempt_phase(&self, action_id: &str) -> Result<dao::StopAttemptPhase, StoreError> {
        self.with(|c| dao::stop_attempt_phase_conn(c, action_id).map_err(db_err))
    }

    /// C02：CAS 占有停止尝试（首帧可能写出之前调用）。false = 行不在账/已有活跃
    /// 占有/旧行未知记录——调用方**不得**发起新停止。
    pub fn claim_stop_attempt(&self, action_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::claim_stop_attempt_conn(c, action_id, now_ms()).map_err(db_err))
    }

    /// C02：升级占有的结果为未知原因（R05 语义保留：重放按记录返回）。
    pub fn resolve_stop_attempt_unknown(
        &self,
        action_id: &str,
        reason: &str,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            dao::resolve_stop_attempt_unknown_conn(c, action_id, reason, now_ms()).map_err(db_err)
        })
    }

    /// C02：CAS 释放占有（仅首写前明确零投递出口调用）——之后的同 action 显式
    /// 重试可再次占有并发起停止。
    pub fn release_stop_attempt(&self, action_id: &str) -> Result<bool, StoreError> {
        self.with(|c| dao::release_stop_attempt_conn(c, action_id, now_ms()).map_err(db_err))
    }

    /// 读取该 Thread 全部在册 fence（seq 序；reconcile 迭代输入）。
    pub fn compat_fences(&self, key: &ThreadKey) -> Result<Vec<CompatFence>, StoreError> {
        self.with(|c| dao::compat_fences_conn(c, &key.thread_id).map_err(db_err))
    }

    /// **原子** compat 派发屏障（R9.1）：fence + 只读核对证据在单条 UPDATE 内持久化
    /// ——成功是 compat bridge 调用真实 helper 的前提；失败（DB 拒绝）时零 helper 调用。
    #[allow(clippy::too_many_arguments)]
    pub fn set_compat_dispatch_barrier(
        &self,
        action_id: &str,
        message_id: Option<&str>,
        turn_id: Option<&str>,
        rollout_path: Option<&str>,
        rollout_baseline: Option<u64>,
        wire_hash: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            dao::set_compat_dispatch_barrier_conn(
                c,
                action_id,
                message_id,
                turn_id,
                rollout_path,
                rollout_baseline,
                wire_hash,
                now_ms(),
            )
            .map_err(db_err)
        })
    }

    /// 有待发工作（queued 普通消息 / 在册屏障）的 thread 全集（hub 启动恢复与周期
    /// 对账的枚举输入，R7.1/R7.2；空集合零开销）。
    /// R9 复审 P2-2：运行期把超龄 reserved 行收敛为 unknown（等待者取消后账本
    /// 不再永久 Reserved；与启动 recover 同语义）。返回 (action_id, thread_id)。
    pub fn mark_stale_reserved_unknown(&self, max_age_ms: i64) -> Vec<(String, String)> {
        self.with(|c| {
            dao::mark_stale_reserved_unknown_conn(c, max_age_ms, now_ms())
                .map_err(|e| StoreError::Database(e.to_string()))
        })
        .unwrap_or_default()
    }

    /// 读取动作的账本终态与回执 turnId（converge 用；不走设备隔离）。
    pub fn action_terminal_state(
        &self,
        action_id: &str,
    ) -> Option<(
        crate::services::codex_desktop_ipc::types::ActionState,
        Option<String>,
    )> {
        self.with(|c| {
            let Ok(mut stmt) =
                c.prepare("SELECT state, receipt_json FROM codex_actions WHERE action_id = ?1")
            else {
                return Ok(None);
            };
            let (state, receipt): (String, Option<String>) =
                match stmt.query_row([action_id], |r| Ok((r.get(0)?, r.get(1)?))) {
                    Ok(v) => v,
                    Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                    Err(_) => return Ok(None),
                };
            let turn_id = receipt.and_then(|json| {
                serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| v.get("turnId").and_then(|t| t.as_str()).map(str::to_string))
            });
            let state = match state.as_str() {
                "queued" => crate::services::codex_desktop_ipc::types::ActionState::Queued,
                "reserved" => crate::services::codex_desktop_ipc::types::ActionState::Reserved,
                "accepted" => crate::services::codex_desktop_ipc::types::ActionState::Accepted,
                "rejected" => crate::services::codex_desktop_ipc::types::ActionState::Rejected,
                "unknown" => crate::services::codex_desktop_ipc::types::ActionState::Unknown,
                _ => crate::services::codex_desktop_ipc::types::ActionState::Cancelled,
            };
            Ok(Some((state, turn_id)))
        })
        .unwrap_or(None)
    }

    pub fn threads_with_pending_work(&self) -> Result<Vec<ThreadKey>, StoreError> {
        self.with(|c| {
            dao::threads_with_pending_work_conn(c)
                .map_err(db_err)
                .map(|v| v.into_iter().map(|t| local_thread_key(&t)).collect())
        })
    }

    /// 某 thread 是否有待发工作（R7.3 引用计数挤出保护：有待发工作的 thread 永不被
    /// unfollow 挤出）。
    pub fn thread_has_pending_work(&self, key: &ThreadKey) -> Result<bool, StoreError> {
        self.with(|c| dao::thread_has_pending_work_conn(c, &key.thread_id).map_err(db_err))
    }

    /// 终态行保留期（PR #165 复审 Important）：超过此时长的终态行（accepted/rejected/
    /// cancelled 且无屏障在册）在下一轮清理中被墓碑化（重载荷置 NULL、**绝不删行**，
    /// actionId 重放幂等由 payload_hash/状态列保全）。state='unknown' 行持有屏障
    /// 语义，永不清理。
    pub(crate) const TERMINAL_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

    /// 墓碑化单批上限（周期对账每轮一批，快 SQL 不阻塞；超期存量逐轮收敛）。
    const TOMBSTONE_BATCH: i64 = 500;

    /// 墓碑化超期终态行（启动恢复 + 周期对账各触发一轮；语义见
    /// [`dao::tombstone_expired_terminal_conn`]）。维护性质尽力而为：失败只记日志
    /// 返回 0，绝不阻塞调用方。
    pub fn tombstone_expired_terminal(&self) -> usize {
        self.with(|c| {
            dao::tombstone_expired_terminal_conn(
                c,
                Self::TERMINAL_RETENTION_MS,
                now_ms(),
                Self::TOMBSTONE_BATCH,
            )
            .map_err(db_err)
        })
        .unwrap_or_else(|e| {
            log::error!("codex_actions: 终态行墓碑化失败（原样保留，下轮再试）: {e}");
            0
        })
    }

    /// R6.3：全部 Turn 顺序绑定行（`Router::new` 重启重建内存限制的输入）。
    ///
    /// P6（2026-10-31 用户实机）：**跳过永远无法释放的行**——`state='unknown'` 且
    /// `compat_turn_id` 为空。`release_turn_if_complete` 只在绑定 turnId 时才能取得
    /// 终态证据（无 turnId 直接返回 false），因此这类行一旦重建进内存限制就**永久**
    /// 冻结该线程：新普通消息一律排队，连空闲会话也发不出去（实机表现）。它不是
    /// 「保守的进行中限制」而是「已收尾的未知投递」——持久屏障仍然保留（不自动重发、
    /// 待核对/人工解除），只是不再冒充在飞回合。
    ///
    /// accepted 缺 turnId 的情形保持不变（有 `converge_with_ledger` 补绑路径）。
    pub fn turn_bindings(&self) -> Result<Vec<TurnBinding>, StoreError> {
        self.with(|c| {
            dao::turn_binding_rows_conn(c).map_err(db_err).map(|rows| {
                rows.into_iter()
                    .filter(|r| {
                        // 只有「unknown 且无回合身份」是可证明永不可释放的形态
                        !(r.state == ActionState::Unknown && r.compat_turn_id.is_none())
                    })
                    .map(|r| TurnBinding {
                        action_id: r.action_id,
                        key: local_thread_key(&r.thread_id),
                        turn_id: r.compat_turn_id,
                    })
                    .collect()
            })
        })
    }
}

/// 服务端 ThreadKey 只构造 local（计划公共类型约定）。
fn local_thread_key(thread_id: &str) -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: thread_id.to_string(),
    }
}

/// R6.3 Turn 顺序绑定（重启后路由器内存重建的输入）：A 路已接受且持久绑定了 turn
/// 身份的动作。
#[derive(Debug, Clone)]
pub struct TurnBinding {
    pub action_id: String,
    pub key: ThreadKey,
    pub turn_id: Option<String>,
}

/// S2：Thread 队列暂停状态（`codex_queue_pause` 行；队列视图与打断回执消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuePause {
    pub paused_at: i64,
    pub pause_reason: String,
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
    // P4：正文预览原样取自账本 queued_text（不剥签名、不改内容），按字符边界截前
    // 120 字符；queued_text 已清（reserved/终态）→ None。
    let text_preview = row.queued_text.as_deref().map(|t| {
        t.chars()
            .take(ACTION_TEXT_PREVIEW_CHARS)
            .collect::<String>()
    });
    Ok(CodexActionView {
        action_id: row.action_id,
        state: row.state,
        position,
        receipt: row.receipt,
        stage: crate::services::codex_desktop_ipc::types::DeliveryStage::of_state(row.state),
        text_preview,
    })
}

/// P4：正文预览截断长度（字符数；前端剥签名后自行决定显示长度）。
const ACTION_TEXT_PREVIEW_CHARS: usize = 120;

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
        signature_applied: row.signature_applied,
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

    /// P4：队列 DTO 正文预览——queued 行带 textPreview（按字符边界截前 120 字符，
    /// 多字节不切坏）；非 queued 行（reserve 后 queued_text 已清）为 None。
    #[test]
    fn action_view_text_preview_truncates_on_char_boundary() {
        let store = ActionStore::new(DeviceStore::memory());
        let ctx = |id: &str| ActionContext {
            action_id: id.into(),
            device_id: "d".into(),
            device_name: "n".into(),
            kind: ActionKind::Send,
            key: key(),
        };
        // 多字节长文：130 个字符（每字符 3 字节 UTF-8）
        let long = "兔".repeat(130);
        store
            .register(
                &ctx("p4-long"),
                ActionKind::Send,
                None,
                None,
                "h",
                Some(&long),
            )
            .unwrap();
        let view = store.get_for_device("p4-long", "d").unwrap().unwrap();
        let preview = view.text_preview.expect("queued 行必须携带 textPreview");
        assert_eq!(preview.chars().count(), 120, "恰好 120 字符");
        assert_eq!(preview, long.chars().take(120).collect::<String>());
        // 短文本原样给前端（不剥签名、不改内容）
        store
            .register(
                &ctx("p4-short"),
                ActionKind::Send,
                None,
                None,
                "h",
                Some("短正文"),
            )
            .unwrap();
        let view = store.get_for_device("p4-short", "d").unwrap().unwrap();
        assert_eq!(view.text_preview.as_deref(), Some("短正文"));
        // reserve 后 queued_text 已清（既有行为）→ None
        store.reserve("p4-short", ControlRoute::DesktopIpc).unwrap();
        let view = store.get_for_device("p4-short", "d").unwrap().unwrap();
        assert_eq!(view.state, ActionState::Reserved);
        assert!(
            view.text_preview.is_none(),
            "非 queued 行 textPreview 为 None"
        );
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
    #[test]
    fn edit_cas_preserves_body_and_removes_only_frozen_signature() {
        let store = ActionStore::new(DeviceStore::memory());
        for (id, signature, raw, expected) in [
            (
                "signed",
                Some(true),
                "正文 [mobile 自写] [mobile n]",
                "正文 [mobile 自写]",
            ),
            (
                "unsigned",
                Some(false),
                "正文 [mobile n]",
                "正文 [mobile n]",
            ),
            ("legacy", None, "[mobile n] 正文 [1]", "[mobile n] 正文 [1]"),
            ("slash", Some(true), "/cmd [mobile n]", "/cmd [mobile n]"),
        ] {
            let ctx = ActionContext {
                action_id: id.into(),
                device_id: "d".into(),
                device_name: "n".into(),
                kind: ActionKind::Send,
                key: key(),
            };
            store
                .register_signed(&ctx, ActionKind::Send, None, None, id, Some(raw), signature)
                .unwrap();
            assert!(store
                .retract_for_edit(id, "other", &key())
                .unwrap()
                .is_none());
            assert_eq!(
                store.retract_for_edit(id, "d", &key()).unwrap().as_deref(),
                Some(expected)
            );
            assert!(store.retract_for_edit(id, "d", &key()).unwrap().is_none());
        }
        let ctx = ActionContext {
            action_id: "reserved".into(),
            device_id: "d".into(),
            device_name: "n".into(),
            kind: ActionKind::Send,
            key: key(),
        };
        store
            .register(
                &ctx,
                ActionKind::Send,
                None,
                None,
                "h",
                Some("reserved text"),
            )
            .unwrap();
        assert!(store.reserve("reserved", ControlRoute::DesktopIpc).unwrap());
        assert!(store
            .retract_for_edit("reserved", "d", &key())
            .unwrap()
            .is_none());
    }
}
