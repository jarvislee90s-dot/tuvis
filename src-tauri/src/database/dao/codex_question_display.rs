//! 题卡展示账契约（单题 skip 展示域 · DB 层唯一权威）。
//!
//! 两张表（schema.rs 建表 / migration.rs 幂等升级）：
//! * `codex_question_display` —— 展示态权威：PK group4(thread/turn/item/gen)，
//!   decision(active|skipped) / revision（持久单调，正 SafeInt）/ device / time /
//!   `source_snapshot_json`（可空：原题快照只写一次，绝不覆盖非 NULL 存量）。
//!   无行 = 默认 active / revision 0（仅读视图，不落行）。
//! * `codex_question_skip_actions` —— 动作账本：action_id PK + 稳定 seq（MAX+1，
//!   事务内分配，行不删除）；强类型 kind(skip|restore)；完整规范请求正文逐字落库
//!   （摘要不能替代正文，幂等/冲突判定逐字段比较）；Prepared→Claimed→Finished
//!   单向状态机，CAS 推进；revision_at_start 记录动作起点时的展示 revision。
//!
//! 事务纪律：每个写入口都是**单事务**（`unchecked_transaction`，全 SQL 在闭包内，
//! 出错即整体回滚——展示行与动作行要么都落、要么都不落）。`&Connection` 入参是
//! 测试固定的接口；事务经 deref 传用，与 codex_actions.rs 等既有 DAO 同 idiom。
//!
//! 关键语义（与实施合同逐条对应）：
//! * **动作回执冻结（审查 M1）**：所有回执（prepare 重放 / finish 首次与重放 /
//!   settle / restore 首次与重放 / history latest_skip）一律由**原动作行**构造——
//!   decision 由 kind 固定（Skip→Skipped、Restore→Active）、resultRevision =
//!   checked(revision_at_start + 1)（两 kind 的 revision_at_start 均存动作前版本，
//!   start+1 即该动作已 CAS 落盘的版本）、native 取该动作自身落账终态。**绝不读
//!   current display 拼结果**：skip1→restore2 重放 skip 仍 Skipped1，restore2→skip3
//!   重放 restore 仍 Active2；history 外层 current display 与 latest_skip 原动作
//!   分开表达。
//! * canonical 正文由 [`RequestIdentity::for_skip`] /
//!   [`RequestIdentity::for_skip_unbound`] / [`RequestIdentity::for_restore`]
//!   唯一生成（kind + group4 + bindingId(有绑定)/null(无绑定)/expectedRevision，
//!   sha256 hex 摘要绑定），并硬校验 group.thread_id == ThreadKey.thread_id；
//!   restore 的 expectedRevision
//!   同时** immutable 绑定**在请求上（`typed_expected_revision`，构造时固定），
//!   restore() 在 first / replay 任何写之前统一核对「绑定值 == 独立参数」，再核
//!   独立参数与存档 revision_at_start 一致；DAO 层**从不解析正文**（逐字比较只在
//!   同一构造器产物之间），杜绝服务器侧任意构造 canonical 造成版本漂移。
//! * prepare 同事务写 skipped + 递增 revision + Prepared 意图 + 原题快照，任一步
//!   失败全回滚；同 actionId 完整身份（kind/device/session/ThreadKey/group/正文）
//!   任一不同即 Conflict（跨 kind 双向冲突），逐字一致才走幂等 Existing。
//! * claim 仅 Prepared-Skip 且未 claim 的行 CAS 恰好一次，绑定 command digest；
//!   落盘成功后上层才允许 Native Invoke。已 claim 行同 digest 重放 = AlreadyClaimed。
//! * finish 只在已 claim 行上落终态；终态幂等（同回执重放返回既有冻结结果，不同
//!   回执不得晚升级）；**绝不改写展示行**——迟到 finish 不得在 restore 后重藏题卡。
//! * settle_without_invoke 只确认「从未 claim」的 Prepared；效果仅 Unresolved /
//!   Unsupported（从未 Invoke 就不可能有已关闭/已不在的核证）；已 claim 的未知
//!   提交不得降级为「从未调用」。
//! * restore 缺行或已 active → typed AlreadyActive（供上层 HTTP 409，不 fake 0
//!   成功）；skipped 行按持久 revision CAS（不依赖当前 source 是否在场），成功后
//!   revision+1 且动作行直接终态落账（restore 不派发 Native）。
//! * recover 仅在启动且无活 owner 时由上层调用：遗留 Prepared/Claimed 一律终定
//!   Unresolved，禁止恢复后重新 Invoke。
//! * history 线程一次批量取：外层为 current display（decision/revision 展示态），
//!   各 group 的 latest skip 动作按 seq 最新以**冻结结果**单独表达（Restore 不参与
//!   遮蔽原生结果）；快照 NULL（旧行）保守返回 None；存储枚举/JSON 严格解析，
//!   损坏即报 Database，绝不伪造已回答或已关闭。
#![allow(dead_code)]
use crate::services::codex_desktop_ipc::types::ThreadKey;
use crate::services::codex_question_skip::types::{
    DisplayDecision, GroupIdentity, NativeEffect, SkipReason,
};
use rusqlite::{params, Connection, OptionalExtension};

/// JS `Number.MAX_SAFE_INTEGER`：revision 会经 HTTP 回执给前端（positiveSafeInt），
/// 产出侧就必须守住上界，超界视为 RevisionConflict（不可能再产出合法下一版）。
const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RequestKind {
    Skip,
    Restore,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestIdentity {
    action_id: String,
    device_id: String,
    session_id: String,
    thread: ThreadKey,
    group: GroupIdentity,
    kind: RequestKind,
    /// 构造器绑定的 immutable typed expected（审查边界①）：[`RequestIdentity::for_restore`]
    /// 恒为 `Some(expectedRevision)`，与 canonical 正文的 expectedRevision 唯一同源、
    /// 构造时固定不可变；[`RequestIdentity::for_skip`] 恒为 None。restore() 在
    /// first / replay 任何写之前统一核对 `Some(独立参数) == 本绑定值`，杜绝
    /// 「正文版本与独立参数不一致」的请求写库。模块外只能经构造器获得实例；
    /// tests 子模块可按 Rust 隐私规则字面量构造（restore 成功路径必须绑定
    /// `Some(调用参数)`）。
    typed_expected_revision: Option<i64>,
    /// 完整规范请求正文，必须逐字比较，摘要不能替代正文。
    canonical_payload: String,
    payload_digest: String,
}

/// group4 的 canonical JSON 形态（与 wire 语义同源；键序由 serde_json Map 决定，
/// 同输入恒同输出，构造器是 canonical 正文的**唯一**生成点）。
fn group_json(group: &GroupIdentity) -> serde_json::Value {
    serde_json::json!({
        "threadId": group.thread_id,
        "sourceTurnId": group.source_turn_id,
        "sourceItemId": group.source_item_id,
        "contentGeneration": group.content_generation,
    })
}

/// canonical 正文的 sha256 hex 小写摘要（与 remote/pairing.rs 同一编码惯例）。
fn digest_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// skip canonical 正文的**唯一私有生成点**：有绑定写 `bindingId:"<id>"`，
/// 无绑定固定 `bindingId:null`（None 与 Some 是不同请求）。[`RequestIdentity::for_skip`] /
/// [`RequestIdentity::for_skip_unbound`] 与 [`lookup_action`] 的存档身份重建共用本函数；
/// 键序由 serde_json Map 决定，同输入恒同输出。
fn skip_canonical_payload(group: &GroupIdentity, binding: Option<&str>) -> String {
    let binding_value = binding
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "bindingId": binding_value,
        "group": group_json(group),
        "kind": "skip",
    })
    .to_string()
}

impl RequestIdentity {
    fn validate(
        action_id: &str,
        device_id: &str,
        session_id: &str,
        thread: &ThreadKey,
        group: &GroupIdentity,
    ) -> Result<(), StoreError> {
        // 身份硬边界：group.thread_id 必须与解析 ThreadKey.thread_id 一致；
        // 身份各字段非空（空串会让完整身份比较退化为可碰撞形态）。
        if action_id.is_empty()
            || device_id.is_empty()
            || session_id.is_empty()
            || thread.host_id.is_empty()
            || thread.thread_id.is_empty()
            || group.thread_id != thread.thread_id
        {
            return Err(StoreError::InvalidState);
        }
        Ok(())
    }

    /// 生产侧唯一构造入口：skip 请求（有绑定）。canonical 正文由私有生成点
    /// [`skip_canonical_payload`] 唯一生成（kind + group4 + bindingId），sha256 hex
    /// 摘要随正文一并绑定。DAO 层从不解析正文（逐字比较只发生在同一构造器产物
    /// 之间），杜绝服务器侧任意拼正文造成的幂等漂移。bindingId 必须非空——空串
    /// 拒绝；无绑定走 [`RequestIdentity::for_skip_unbound`]，两者是不同请求。
    pub(crate) fn for_skip(
        action_id: String,
        device_id: String,
        session_id: String,
        thread: ThreadKey,
        group: GroupIdentity,
        binding_id: &str,
    ) -> Result<Self, StoreError> {
        Self::validate(&action_id, &device_id, &session_id, &thread, &group)?;
        if binding_id.is_empty() {
            return Err(StoreError::InvalidState);
        }
        let canonical_payload = skip_canonical_payload(&group, Some(binding_id));
        let payload_digest = digest_hex(canonical_payload.as_bytes());
        Ok(Self {
            action_id,
            device_id,
            session_id,
            thread,
            group,
            kind: RequestKind::Skip,
            typed_expected_revision: None,
            canonical_payload,
            payload_digest,
        })
    }

    /// 生产侧唯一构造入口：skip 请求（无绑定）。与 [`RequestIdentity::for_skip`]
    /// 共用同一私有规范生成点（[`skip_canonical_payload`]），正文固定
    /// `bindingId:null`。None 与 Some 是不同请求：无绑定与任一绑定的 canonical
    /// 正文/摘要都不同，同 actionId 复用必然 Conflict；已 issued 的绑定请求重试
    /// 不能经本构造器补 binding。
    pub(crate) fn for_skip_unbound(
        action_id: String,
        device_id: String,
        session_id: String,
        thread: ThreadKey,
        group: GroupIdentity,
    ) -> Result<Self, StoreError> {
        Self::validate(&action_id, &device_id, &session_id, &thread, &group)?;
        let canonical_payload = skip_canonical_payload(&group, None);
        let payload_digest = digest_hex(canonical_payload.as_bytes());
        Ok(Self {
            action_id,
            device_id,
            session_id,
            thread,
            group,
            kind: RequestKind::Skip,
            typed_expected_revision: None,
            canonical_payload,
            payload_digest,
        })
    }

    /// 生产侧唯一构造入口：restore 请求。canonical 正文唯一生成
    /// （kind + group4 + expectedRevision）；expectedRevision 同时作为独立参数
    /// 传入 [`restore`](fn.restore)，DAO 在重放判定前核对两者与存档一致。
    pub(crate) fn for_restore(
        action_id: String,
        device_id: String,
        session_id: String,
        thread: ThreadKey,
        group: GroupIdentity,
        expected_revision: i64,
    ) -> Result<Self, StoreError> {
        Self::validate(&action_id, &device_id, &session_id, &thread, &group)?;
        if expected_revision <= 0 || expected_revision > JS_MAX_SAFE_INTEGER {
            return Err(StoreError::InvalidState);
        }
        let canonical_payload = serde_json::json!({
            "expectedRevision": expected_revision,
            "group": group_json(&group),
            "kind": "restore",
        })
        .to_string();
        let payload_digest = digest_hex(canonical_payload.as_bytes());
        Ok(Self {
            action_id,
            device_id,
            session_id,
            thread,
            group,
            kind: RequestKind::Restore,
            // typed expected 与 canonical 正文同源唯一绑定（构造时固定）
            typed_expected_revision: Some(expected_revision),
            canonical_payload,
            payload_digest,
        })
    }

    // —— 只读 getters（生产 caller 经构造器获得实例后只读；字段本身模块私有，
    //    tests 子模块按 Rust 隐私规则仍可直接构造字面量夹具）——
    pub(crate) fn action_id(&self) -> &str {
        &self.action_id
    }
    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(crate) fn thread(&self) -> &ThreadKey {
        &self.thread
    }
    pub(crate) fn group(&self) -> &GroupIdentity {
        &self.group
    }
    pub(crate) fn kind(&self) -> &RequestKind {
        &self.kind
    }
    /// 构造器绑定的 typed expected（restore 专用；skip 恒 None）。
    pub(crate) fn typed_expected_revision(&self) -> Option<i64> {
        self.typed_expected_revision
    }
    pub(crate) fn canonical_payload(&self) -> &str {
        &self.canonical_payload
    }
    pub(crate) fn payload_digest(&self) -> &str {
        &self.payload_digest
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct QuestionSnapshot {
    pub question_key: String,
    pub index: i64,
    pub title: String,
    pub options: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeReceipt {
    pub effect: NativeEffect,
    pub reason: SkipReason,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActionReceipt {
    pub action_id: String,
    pub display: DisplayDecision,
    pub revision: i64,
    pub native: NativeReceipt,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActionPhase {
    Prepared,
    Claimed,
    Finished,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredAction {
    pub(crate) request: RequestIdentity,
    pub(crate) phase: ActionPhase,
    pub(crate) command_digest: Option<String>,
    pub(crate) receipt: Option<ActionReceipt>,
}

/// 只读 typed 查询：按 actionId 读取动作行，经唯一 typed factory 完整重建请求身份
/// （kind/device/session/ThreadKey(host,thread)/group4/canonical 正文/digest/restore
/// 的 typed expected），并与存档逐字段核对自洽；缺行返回 None（合法查询结果），
/// 存档损坏（正文不可解析 / 摘要不匹配 / 正文与列不自洽 / restore expected 与
/// revision_at_start 漂移 / Claimed 缺 digest）一律 `StoreError::Database`——
/// **绝不宽松默认**。Phase 取自 state 列（Prepared/Claimed/Finished 精确区分，
/// 不以 Unresolved 回执推测终态）；只有 Finished 携带回执，且复用
/// [`action_result`] 的冻结语义（绝不读 current display 拼结果）。
pub(crate) fn lookup_action(
    conn: &Connection,
    action_id: &str,
) -> Result<Option<StoredAction>, StoreError> {
    let row = match load_action(conn, action_id)? {
        Some(row) => row,
        None => return Ok(None),
    };
    let request = rebuild_identity(&row)?;
    // 存档自洽：重建身份与行逐字段核对（canonical 字符串 / digest / 完整身份）。
    if !identity_matches(&row, &request) {
        return Err(StoreError::Database);
    }
    // restore 的 typed expected 与存档 revision_at_start 同源落库（restore 写点
    // revision_at_start = expected_rev）；漂移即存档损坏，不是可容忍差异。
    if row.kind == RequestKind::Restore
        && request.typed_expected_revision() != Some(row.revision_at_start)
    {
        return Err(StoreError::Database);
    }
    let phase = match row.state {
        ActionState::Prepared => ActionPhase::Prepared,
        ActionState::Claimed => ActionPhase::Claimed,
        ActionState::Finished => ActionPhase::Finished,
    };
    // Claimed 行必须已绑定 command digest（claim_cas 恰好一次写入）；缺失即损坏。
    if phase == ActionPhase::Claimed && row.command_digest.is_none() {
        return Err(StoreError::Database);
    }
    // 只有 Finished 有回执；复用 action_result 冻结算法（start+1、kind 定 decision）。
    let receipt = match phase {
        ActionPhase::Finished => Some(action_result(&row)?),
        _ => None,
    };
    Ok(Some(StoredAction {
        request,
        phase,
        command_digest: row.command_digest.clone(),
        receipt,
    }))
}

/// 从存档动作行经唯一 typed factory 重建完整请求身份。正文先严格 JSON 解析再
/// 按 kind 取参（skip：`bindingId` 字符串=有绑定 / null=无绑定；restore：
/// `expectedRevision` 严格 safe-int）；factory 校验不过（含 skip 空绑定、restore
/// 非 safe-int）说明**存档行**不可能由本模块构造器写出 → 损坏即 Database。
fn rebuild_identity(row: &ActionRow) -> Result<RequestIdentity, StoreError> {
    let payload: serde_json::Value = serde_json::from_str(&row.canonical_payload).map_err(|e| {
        log::warn!("codex_question_display: 存档正文 JSON 损坏，保守拒绝: {e}");
        StoreError::Database
    })?;
    let thread = ThreadKey {
        host_id: row.thread_host_id.clone(),
        thread_id: row.thread_id.clone(),
    };
    match row.kind {
        RequestKind::Skip => {
            let binding = match payload.get("bindingId") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => Some(s.as_str()),
                // 缺键 / 非字符串非 null：存档损坏，绝不默认成无绑定
                _ => return Err(StoreError::Database),
            };
            match binding {
                Some(s) => RequestIdentity::for_skip(
                    row.action_id.clone(),
                    row.device_id.clone(),
                    row.session_id.clone(),
                    thread,
                    row.group.clone(),
                    s,
                ),
                None => RequestIdentity::for_skip_unbound(
                    row.action_id.clone(),
                    row.device_id.clone(),
                    row.session_id.clone(),
                    thread,
                    row.group.clone(),
                ),
            }
        }
        RequestKind::Restore => {
            let expected = payload
                .get("expectedRevision")
                .and_then(|v| v.as_i64())
                .ok_or(StoreError::Database)?;
            RequestIdentity::for_restore(
                row.action_id.clone(),
                row.device_id.clone(),
                row.session_id.clone(),
                thread,
                row.group.clone(),
                expected,
            )
        }
    }
    .map_err(|e| {
        // 行数据过不了唯一构造器校验 = 该行不可能由本模块构造器落库 → 存档损坏
        log::warn!("codex_question_display: 存档身份重建未过构造器校验: {e:?}");
        StoreError::Database
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoreError {
    Unimplemented,
    Conflict,
    AlreadyActive,
    RevisionConflict,
    InvalidState,
    Database,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrepareOutcome {
    Prepared { revision: i64 },
    Existing(ActionReceipt),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    Claimed,
    AlreadyClaimed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryEntry {
    pub group: GroupIdentity,
    pub decision: DisplayDecision,
    pub revision: i64,
    pub snapshot: Option<QuestionSnapshot>,
    pub latest_skip: Option<ActionReceipt>,
}

// ============================================================
// 存储枚举的严格双向映射（损坏即 Database，绝不猜测）
// ============================================================

fn db_err(e: rusqlite::Error) -> StoreError {
    log::warn!("codex_question_display: 存储访问失败: {e}");
    StoreError::Database
}

fn kind_to_str(kind: &RequestKind) -> &'static str {
    match kind {
        RequestKind::Skip => "skip",
        RequestKind::Restore => "restore",
    }
}

fn kind_from_str(raw: &str) -> Result<RequestKind, StoreError> {
    match raw {
        "skip" => Ok(RequestKind::Skip),
        "restore" => Ok(RequestKind::Restore),
        _ => Err(StoreError::Database),
    }
}

fn decision_to_str(decision: &DisplayDecision) -> &'static str {
    match decision {
        DisplayDecision::Active => "active",
        DisplayDecision::Skipped => "skipped",
    }
}

fn decision_from_str(raw: &str) -> Result<DisplayDecision, StoreError> {
    match raw {
        "active" => Ok(DisplayDecision::Active),
        "skipped" => Ok(DisplayDecision::Skipped),
        _ => Err(StoreError::Database),
    }
}

fn effect_to_str(effect: &NativeEffect) -> &'static str {
    match effect {
        NativeEffect::ClosedVerified => "closed_verified",
        NativeEffect::NotPresentVerified => "not_present_verified",
        NativeEffect::Unresolved => "unresolved",
        NativeEffect::Unsupported => "unsupported",
    }
}

fn effect_from_str(raw: &str) -> Result<NativeEffect, StoreError> {
    match raw {
        "closed_verified" => Ok(NativeEffect::ClosedVerified),
        "not_present_verified" => Ok(NativeEffect::NotPresentVerified),
        "unresolved" => Ok(NativeEffect::Unresolved),
        "unsupported" => Ok(NativeEffect::Unsupported),
        _ => Err(StoreError::Database),
    }
}

fn reason_to_str(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::None => "none",
        SkipReason::BindingRequired => "binding_required",
        SkipReason::LeaseExpired => "lease_expired",
        SkipReason::ScopeChanged => "scope_changed",
        SkipReason::SourceIncomplete => "source_incomplete",
        SkipReason::Ambiguous => "ambiguous",
        SkipReason::ContentChanged => "content_changed",
        SkipReason::NativeCountUnknown => "native_count_unknown",
        SkipReason::UnsupportedGroup => "unsupported_group",
        SkipReason::UnsupportedBuild => "unsupported_build",
        SkipReason::InvokeUnavailable => "invoke_unavailable",
        SkipReason::NativeUnavailable => "native_unavailable",
        SkipReason::ConcurrentAction => "concurrent_action",
        SkipReason::InvokedUnconfirmed => "invoked_unconfirmed",
        SkipReason::ActionConflict => "action_conflict",
        SkipReason::LeaseConsumed => "lease_consumed",
        SkipReason::StoreUnavailable => "store_unavailable",
    }
}

fn reason_from_str(raw: &str) -> Result<SkipReason, StoreError> {
    match raw {
        "none" => Ok(SkipReason::None),
        "binding_required" => Ok(SkipReason::BindingRequired),
        "lease_expired" => Ok(SkipReason::LeaseExpired),
        "scope_changed" => Ok(SkipReason::ScopeChanged),
        "source_incomplete" => Ok(SkipReason::SourceIncomplete),
        "ambiguous" => Ok(SkipReason::Ambiguous),
        "content_changed" => Ok(SkipReason::ContentChanged),
        "native_count_unknown" => Ok(SkipReason::NativeCountUnknown),
        "unsupported_group" => Ok(SkipReason::UnsupportedGroup),
        "unsupported_build" => Ok(SkipReason::UnsupportedBuild),
        "invoke_unavailable" => Ok(SkipReason::InvokeUnavailable),
        "native_unavailable" => Ok(SkipReason::NativeUnavailable),
        "concurrent_action" => Ok(SkipReason::ConcurrentAction),
        "invoked_unconfirmed" => Ok(SkipReason::InvokedUnconfirmed),
        "action_conflict" => Ok(SkipReason::ActionConflict),
        "lease_consumed" => Ok(SkipReason::LeaseConsumed),
        "store_unavailable" => Ok(SkipReason::StoreUnavailable),
        _ => Err(StoreError::Database),
    }
}

/// 动作状态机的存储词表（CHECK 约束同词表钉死）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionState {
    Prepared,
    Claimed,
    Finished,
}

fn state_from_str(raw: &str) -> Result<ActionState, StoreError> {
    match raw {
        "prepared" => Ok(ActionState::Prepared),
        "claimed" => Ok(ActionState::Claimed),
        "finished" => Ok(ActionState::Finished),
        _ => Err(StoreError::Database),
    }
}

fn state_to_str(state: ActionState) -> &'static str {
    match state {
        ActionState::Prepared => "prepared",
        ActionState::Claimed => "claimed",
        ActionState::Finished => "finished",
    }
}

fn ensure_safe_revision(revision: i64) -> Result<(), StoreError> {
    if revision <= 0 || revision > JS_MAX_SAFE_INTEGER {
        return Err(StoreError::RevisionConflict);
    }
    Ok(())
}

fn parse_snapshot(json: &str) -> Result<QuestionSnapshot, StoreError> {
    serde_json::from_str(json).map_err(|e| {
        log::warn!("codex_question_display: 存量快照 JSON 损坏，保守拒绝: {e}");
        StoreError::Database
    })
}

// ============================================================
// 行读取
// ============================================================

#[derive(Clone, Debug)]
struct DisplayRow {
    decision: DisplayDecision,
    revision: i64,
    device_id: String,
    updated_at: i64,
    snapshot_json: Option<String>,
}

impl DisplayRow {
    /// 无行时的读视图默认值：active / revision 0（不落行，仅读视图）。
    /// 当前仅 prepare/restore 的目标行判断使用 load_display；回执构造一律走
    /// [`action_result`] 的冻结语义，不再借用本默认值。
    #[allow(dead_code)]
    fn default_active() -> Self {
        Self {
            decision: DisplayDecision::Active,
            revision: 0,
            device_id: String::new(),
            updated_at: 0,
            snapshot_json: None,
        }
    }
}

fn load_display(
    conn: &Connection,
    group: &GroupIdentity,
) -> Result<Option<DisplayRow>, StoreError> {
    let raw: Option<(String, i64, String, i64, Option<String>)> = conn
        .query_row(
            "SELECT decision, revision, device_id, updated_at, source_snapshot_json
             FROM codex_question_display
             WHERE thread_id = ?1 AND source_turn_id = ?2 AND source_item_id = ?3 AND content_generation = ?4",
            params![group.thread_id, group.source_turn_id, group.source_item_id, group.content_generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(db_err)?;
    raw.map(
        |(decision, revision, device_id, updated_at, snapshot_json)| {
            Ok(DisplayRow {
                decision: decision_from_str(&decision)?,
                revision,
                device_id,
                updated_at,
                snapshot_json,
            })
        },
    )
    .transpose()
}

#[derive(Clone, Debug)]
struct ActionRow {
    action_id: String,
    seq: i64,
    kind: RequestKind,
    device_id: String,
    session_id: String,
    thread_host_id: String,
    thread_id: String,
    group: GroupIdentity,
    canonical_payload: String,
    payload_digest: String,
    state: ActionState,
    revision_at_start: i64,
    command_digest: Option<String>,
    claimed_at: Option<i64>,
    settled_at: Option<i64>,
    native_effect: Option<NativeEffect>,
    native_reason: Option<SkipReason>,
    created_at: i64,
}

const ACTION_COLUMNS: &str =
    "action_id, seq, kind, device_id, session_id, thread_host_id, thread_id, \
     group_turn_id, group_item_id, group_generation, canonical_payload, payload_digest, \
     state, revision_at_start, command_digest, claimed_at, settled_at, native_effect, native_reason, created_at";

/// 列序与 `ACTION_COLUMNS` 一一对应（20 列）：
/// 0 action_id / 1 seq / 2 kind / 3 device / 4 session / 5 thread_host / 6 thread /
/// 7-9 group(turn,item,gen) / 10 canonical_payload / 11 payload_digest / 12 state /
/// 13 revision_at_start / 14 command_digest / 15 claimed_at / 16 settled_at /
/// 17 native_effect / 18 native_reason / 19 created_at。
type RawActionRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    i64,
);

fn action_row_from(raw: RawActionRow) -> Result<ActionRow, StoreError> {
    Ok(ActionRow {
        action_id: raw.0,
        seq: raw.1,
        kind: kind_from_str(&raw.2)?,
        device_id: raw.3,
        session_id: raw.4,
        thread_host_id: raw.5,
        thread_id: raw.6.clone(),
        group: GroupIdentity {
            thread_id: raw.6,
            source_turn_id: raw.7,
            source_item_id: raw.8,
            content_generation: raw.9,
        },
        canonical_payload: raw.10,
        payload_digest: raw.11,
        state: state_from_str(&raw.12)?,
        revision_at_start: raw.13,
        command_digest: raw.14,
        claimed_at: raw.15,
        settled_at: raw.16,
        native_effect: raw.17.as_deref().map(effect_from_str).transpose()?,
        native_reason: raw.18.as_deref().map(reason_from_str).transpose()?,
        created_at: raw.19,
    })
}

fn load_action(conn: &Connection, action_id: &str) -> Result<Option<ActionRow>, StoreError> {
    let sql =
        format!("SELECT {ACTION_COLUMNS} FROM codex_question_skip_actions WHERE action_id = ?1");
    let raw = conn
        .query_row(&sql, params![action_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
                r.get(16)?,
                r.get(17)?,
                r.get(18)?,
                r.get(19)?,
            ))
        })
        .optional()
        .map_err(db_err)?;
    raw.map(action_row_from).transpose()
}

/// 同 actionId 的完整身份比较：kind / device / session / ThreadKey(host,thread) /
/// group4 / 规范正文 / 摘要逐字段核对——任一不同即冲突，摘要相同正文不同同样是冲突。
fn identity_matches(row: &ActionRow, request: &RequestIdentity) -> bool {
    row.kind == request.kind
        && row.device_id == request.device_id
        && row.session_id == request.session_id
        && row.thread_host_id == request.thread.host_id
        && row.thread_id == request.thread.thread_id
        && row.group == request.group
        && row.canonical_payload == request.canonical_payload
        && row.payload_digest == request.payload_digest
}

/// 动作自身的冻结结果（审查 M1「动作回执不可变」）：decision 由 kind 固定
/// （Skip→Skipped、Restore→Active），resultRevision = checked(revision_at_start + 1)
/// ——两 kind 的 revision_at_start 都持久化为**动作前**的展示版本（skip 存变更前
/// 版本、restore 存 expected 旧版本，见各写点），start+1 恰为该动作产出并已 CAS
/// 落盘的版本。native 取该动作自身已落账的终态；未落账（Prepared/Claimed 在途、
/// restore 无原生面）保守 Unresolved，绝不伪造 closed / not_present。
/// **绝不读 current display 拼结果**：skip1→restore2 后重放 skip 仍是 Skipped1、
/// restore2→skip3 后重放 restore 仍是 Active2；current display 只在 history 外层
/// 单独表达（见 list_thread_history）。
fn action_result(row: &ActionRow) -> Result<ActionReceipt, StoreError> {
    let display = match row.kind {
        RequestKind::Skip => DisplayDecision::Skipped,
        RequestKind::Restore => DisplayDecision::Active,
    };
    let revision = row
        .revision_at_start
        .checked_add(1)
        .ok_or(StoreError::RevisionConflict)?;
    ensure_safe_revision(revision)?;
    let native = match (row.native_effect.clone(), row.native_reason.clone()) {
        (Some(effect), reason) => NativeReceipt {
            effect,
            reason: reason.unwrap_or(SkipReason::None),
        },
        (None, _) => NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::None,
        },
    };
    Ok(ActionReceipt {
        action_id: row.action_id.clone(),
        display,
        revision,
        native,
    })
}

/// 同 group 快照不可变：存量快照非 NULL 且内容不同 → Conflict（绝不覆盖原快照）；
/// NULL 旧行保守兼容（视为尚无快照，允许首次补写）。
fn ensure_snapshot_consistent(
    display: Option<&DisplayRow>,
    snapshot: &QuestionSnapshot,
) -> Result<(), StoreError> {
    if let Some(json) = display.and_then(|d| d.snapshot_json.as_deref()) {
        let stored = parse_snapshot(json)?;
        if stored != *snapshot {
            return Err(StoreError::Conflict);
        }
    }
    Ok(())
}

/// 稳定 seq：事务内 MAX+1（动作行不删除，seq 单调不回收；UNIQUE 兜底）。
fn next_action_seq(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM codex_question_skip_actions",
        [],
        |r| r.get(0),
    )
    .map_err(db_err)
}

/// 单事务包装：闭包内全部 SQL，出错时事务 drop 即整体回滚（含触发器 ABORT 注入），
/// 任何 insert/update 失败都不会留下半截状态。
fn run_in_transaction<T>(
    conn: &Connection,
    work: impl FnOnce(&Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let outcome = work(&tx)?;
    tx.commit().map_err(db_err)?;
    Ok(outcome)
}

// ============================================================
// 七个写/读入口
// ============================================================

/// skip 动作的准备：同事务写「展示行 skipped + 递增 revision + Prepared 意图 +
/// 原题快照」，任一步失败全回滚。同 actionId 完整身份逐字一致才幂等返回 Existing。
pub(crate) fn prepare_skip(
    conn: &Connection,
    request: &RequestIdentity,
    snapshot: &QuestionSnapshot,
    now: i64,
) -> Result<PrepareOutcome, StoreError> {
    run_in_transaction(conn, |tx| prepare_skip_tx(tx, request, snapshot, now))
}

fn prepare_skip_tx(
    conn: &Connection,
    request: &RequestIdentity,
    snapshot: &QuestionSnapshot,
    now: i64,
) -> Result<PrepareOutcome, StoreError> {
    if request.kind != RequestKind::Skip {
        return Err(StoreError::InvalidState);
    }
    // ① actionId 幂等/冲突：完整身份逐字段核对（跨 kind 复用同 id 双向 Conflict）
    if let Some(existing) = load_action(conn, &request.action_id)? {
        if !identity_matches(&existing, request) {
            return Err(StoreError::Conflict);
        }
        // 幂等重放同样受快照不可变约束（同组已有不同快照 → 拒绝，不覆盖）
        ensure_snapshot_consistent(load_display(conn, &request.group)?.as_ref(), snapshot)?;
        // 重放返回原动作的冻结结果（不读 current display）
        return Ok(PrepareOutcome::Existing(action_result(&existing)?));
    }
    let display = load_display(conn, &request.group)?;
    // ② 同 group4 快照不可变：非 NULL 且内容不同 → Conflict，保留原快照
    ensure_snapshot_consistent(display.as_ref(), snapshot)?;
    // ③ revision：有行递增、无行从 1 起；产出侧守正 SafeInt 上界
    let start_revision = display.as_ref().map(|d| d.revision).unwrap_or(0);
    let new_revision = start_revision
        .checked_add(1)
        .ok_or(StoreError::RevisionConflict)?;
    ensure_safe_revision(new_revision)?;
    let snapshot_json = serde_json::to_string(snapshot).map_err(|_| StoreError::Database)?;
    // ④ 展示行写点（skipped + 递增 revision；旧行 NULL 快照在此保守补写一次）
    match &display {
        Some(row) => {
            if row.snapshot_json.is_none() {
                conn.execute(
                    "UPDATE codex_question_display
                     SET decision = 'skipped', revision = ?1, device_id = ?2, updated_at = ?3,
                         source_snapshot_json = ?4
                     WHERE thread_id = ?5 AND source_turn_id = ?6 AND source_item_id = ?7
                       AND content_generation = ?8",
                    params![
                        new_revision,
                        request.device_id,
                        now,
                        snapshot_json,
                        request.group.thread_id,
                        request.group.source_turn_id,
                        request.group.source_item_id,
                        request.group.content_generation,
                    ],
                )
                .map_err(db_err)?;
            } else {
                conn.execute(
                    "UPDATE codex_question_display
                     SET decision = 'skipped', revision = ?1, device_id = ?2, updated_at = ?3
                     WHERE thread_id = ?4 AND source_turn_id = ?5 AND source_item_id = ?6
                       AND content_generation = ?7",
                    params![
                        new_revision,
                        request.device_id,
                        now,
                        request.group.thread_id,
                        request.group.source_turn_id,
                        request.group.source_item_id,
                        request.group.content_generation,
                    ],
                )
                .map_err(db_err)?;
            }
        }
        None => {
            conn.execute(
                "INSERT INTO codex_question_display
                    (thread_id, source_turn_id, source_item_id, content_generation,
                     decision, revision, device_id, updated_at, source_snapshot_json)
                 VALUES (?1, ?2, ?3, ?4, 'skipped', ?5, ?6, ?7, ?8)",
                params![
                    request.group.thread_id,
                    request.group.source_turn_id,
                    request.group.source_item_id,
                    request.group.content_generation,
                    new_revision,
                    request.device_id,
                    now,
                    snapshot_json,
                ],
            )
            .map_err(db_err)?;
        }
    }
    // ⑤ 动作行（Prepared 意图）：revision_at_start = 动作起点的展示 revision
    let seq = next_action_seq(conn)?;
    conn.execute(
        "INSERT INTO codex_question_skip_actions
            (action_id, seq, kind, device_id, session_id, thread_host_id, thread_id,
             group_turn_id, group_item_id, group_generation, canonical_payload, payload_digest,
             state, revision_at_start, created_at)
         VALUES (?1, ?2, 'skip', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'prepared', ?12, ?13)",
        params![
            request.action_id,
            seq,
            request.device_id,
            request.session_id,
            request.thread.host_id,
            request.thread.thread_id,
            request.group.source_turn_id,
            request.group.source_item_id,
            request.group.content_generation,
            request.canonical_payload,
            request.payload_digest,
            start_revision,
            now,
        ],
    )
    .map_err(db_err)?;
    Ok(PrepareOutcome::Prepared {
        revision: new_revision,
    })
}

/// claim CAS：仅 Prepared-Skip 且未 claim 的行恰好占用一次并绑定 command digest；
/// 落盘成功后上层才允许 Native Invoke。同 digest 重放 = AlreadyClaimed（幂等 join），
/// 换 digest / 换身份 / 终态行一律拒绝。
pub(crate) fn claim_cas(
    conn: &Connection,
    request: &RequestIdentity,
    command_digest: &str,
    now: i64,
) -> Result<ClaimOutcome, StoreError> {
    run_in_transaction(conn, |tx| {
        let row = load_action(tx, &request.action_id)?.ok_or(StoreError::InvalidState)?;
        if !identity_matches(&row, request) {
            return Err(StoreError::Conflict);
        }
        if row.kind != RequestKind::Skip {
            return Err(StoreError::InvalidState);
        }
        match row.state {
            ActionState::Prepared => {
                let n = tx
                    .execute(
                        "UPDATE codex_question_skip_actions
                         SET state = 'claimed', command_digest = ?1, claimed_at = ?2
                         WHERE action_id = ?3 AND state = 'prepared'",
                        params![command_digest, now, request.action_id],
                    )
                    .map_err(db_err)?;
                if n == 1 {
                    Ok(ClaimOutcome::Claimed)
                } else {
                    Err(StoreError::InvalidState)
                }
            }
            ActionState::Claimed => {
                if row.command_digest.as_deref() == Some(command_digest) {
                    Ok(ClaimOutcome::AlreadyClaimed)
                } else {
                    Err(StoreError::Conflict)
                }
            }
            // 终态行（含 recover 强制终定的遗留作业）禁止再次 claim → 禁止重 Invoke
            ActionState::Finished => Err(StoreError::InvalidState),
        }
    })
}

/// finish：仅在已 claim 的 Skip 行上落终态。终态幂等（同回执重放返回既有结果），
/// 不同回执不得晚升级；**绝不改写展示行**——迟到 finish 不得在 restore 后重藏题卡。
pub(crate) fn finish(
    conn: &Connection,
    request: &RequestIdentity,
    receipt: &NativeReceipt,
    now: i64,
) -> Result<ActionReceipt, StoreError> {
    run_in_transaction(conn, |tx| {
        let row = load_action(tx, &request.action_id)?.ok_or(StoreError::InvalidState)?;
        if !identity_matches(&row, request) {
            return Err(StoreError::Conflict);
        }
        if row.kind != RequestKind::Skip {
            return Err(StoreError::InvalidState);
        }
        match row.state {
            // 从未 claim 的行不存在合法结果（没有 Invoke 就没有核证回执）
            ActionState::Prepared => Err(StoreError::InvalidState),
            ActionState::Claimed => {
                let n = tx
                    .execute(
                        "UPDATE codex_question_skip_actions
                         SET state = 'finished', native_effect = ?1, native_reason = ?2, settled_at = ?3
                         WHERE action_id = ?4 AND state = 'claimed'",
                        params![
                            effect_to_str(&receipt.effect),
                            reason_to_str(&receipt.reason),
                            now,
                            request.action_id,
                        ],
                    )
                    .map_err(db_err)?;
                if n != 1 {
                    return Err(StoreError::InvalidState);
                }
                // 首次结算回执 = 原动作冻结结果 + 本次落账 native（构造与重放路径
                // 完全同源，保证「首次 == 终态重放」结构性成立）；展示行绝不改写。
                let mut settled = row.clone();
                settled.native_effect = Some(receipt.effect.clone());
                settled.native_reason = Some(receipt.reason.clone());
                action_result(&settled)
            }
            ActionState::Finished => {
                // 终态幂等：完全相同的回执重放返回既有冻结结果；
                // 不同回执不得晚升级（低档结果不得被高档结果覆盖）
                let same_receipt = row.native_effect.as_ref() == Some(&receipt.effect)
                    && row.native_reason.as_ref() == Some(&receipt.reason);
                if same_receipt {
                    action_result(&row)
                } else {
                    Err(StoreError::InvalidState)
                }
            }
        }
    })
}

/// settle_without_invoke：只确认「从未 claim」的 Prepared（明确零投递的保守收口）。
/// 效果仅 Unresolved / Unsupported——从未 Invoke 就不可能有已关闭/已不在的核证；
/// 已 claim 的未知提交不得降级为「从未调用」，已终态行不得二次结算。
pub(crate) fn settle_without_invoke(
    conn: &Connection,
    request: &RequestIdentity,
    receipt: &NativeReceipt,
    now: i64,
) -> Result<ActionReceipt, StoreError> {
    run_in_transaction(conn, |tx| {
        let row = load_action(tx, &request.action_id)?.ok_or(StoreError::InvalidState)?;
        if !identity_matches(&row, request) {
            return Err(StoreError::Conflict);
        }
        if row.kind != RequestKind::Skip {
            return Err(StoreError::InvalidState);
        }
        if row.state != ActionState::Prepared {
            return Err(StoreError::InvalidState);
        }
        if !matches!(
            receipt.effect,
            NativeEffect::Unresolved | NativeEffect::Unsupported
        ) {
            return Err(StoreError::InvalidState);
        }
        let n = tx
            .execute(
                "UPDATE codex_question_skip_actions
                 SET state = 'finished', native_effect = ?1, native_reason = ?2, settled_at = ?3
                 WHERE action_id = ?4 AND state = 'prepared' AND claimed_at IS NULL",
                params![
                    effect_to_str(&receipt.effect),
                    reason_to_str(&receipt.reason),
                    now,
                    request.action_id,
                ],
            )
            .map_err(db_err)?;
        if n != 1 {
            return Err(StoreError::InvalidState);
        }
        // 首次结算回执 = 原动作冻结结果 + 本次落账 native（与重放路径同源构造）
        let mut settled = row.clone();
        settled.native_effect = Some(receipt.effect.clone());
        settled.native_reason = Some(receipt.reason.clone());
        action_result(&settled)
    })
}

/// restore：对 skipped 展示行按**持久 revision CAS** 翻回 active（不依赖当前 source
/// 是否在场）。缺行 / 已 active → typed AlreadyActive（供上层 HTTP 409，不 fake 0
/// 成功）；expected revision 不匹配 → RevisionConflict。Restore 动作行同事务直接
/// 终态落账（restore 不派发 Native Invoke），latest skip 的原生结果不受其遮蔽。
pub(crate) fn restore(
    conn: &Connection,
    request: &RequestIdentity,
    expected_rev: i64,
    now: i64,
) -> Result<ActionReceipt, StoreError> {
    run_in_transaction(conn, |tx| {
        // ① actionId 幂等/冲突：同 id 不同身份（含跨 kind 复用）一律 Conflict
        if let Some(existing) = load_action(tx, &request.action_id)? {
            if !identity_matches(&existing, request) {
                return Err(StoreError::Conflict);
            }
            if existing.kind != RequestKind::Restore {
                return Err(StoreError::InvalidState);
            }
            // 重放前统一核对（边界①）：构造器绑定的 typed expected 必须与独立参数
            // 唯一一致——for_restore 把 expectedRevision 同时写进 canonical 正文与本
            // 绑定；DAO 不解析正文，以「绑定值 == 独立参数」在重放判定前拒绝畸形请求
            //（不解析旧 opaque 夹具正文，正文比较仍是逐字相等）。
            if request.typed_expected_revision != Some(expected_rev) {
                return Err(StoreError::RevisionConflict);
            }
            // 存档起点一致：expected 参数必须与原动作存档的 revision_at_start 一致
            //（防重放漂移到与首次执行不同的版本基点）。
            if existing.revision_at_start != expected_rev {
                return Err(StoreError::RevisionConflict);
            }
            // 幂等重放：返回原动作的冻结结果（Active、start+1），不读 current display
            return action_result(&existing);
        }
        if request.kind != RequestKind::Restore {
            return Err(StoreError::InvalidState);
        }
        // ② 目标展示行：缺行 = 默认即 active（无行可恢复）→ AlreadyActive
        let row = match load_display(tx, &request.group)? {
            Some(row) => row,
            None => return Err(StoreError::AlreadyActive),
        };
        if row.decision == DisplayDecision::Active {
            return Err(StoreError::AlreadyActive);
        }
        // ③ 首次写之前统一核对（边界①）：构造器绑定的 typed expected 必须与独立
        // 参数唯一一致——不一致即畸形请求，先于任何 display 写 / action 落行拒绝，
        // 与 replay 路径同一判定。
        if request.typed_expected_revision != Some(expected_rev) {
            return Err(StoreError::RevisionConflict);
        }
        // ④ 持久 revision CAS（expected 与新 revision 都必须是正 SafeInt）
        ensure_safe_revision(expected_rev)?;
        let new_revision = row
            .revision
            .checked_add(1)
            .ok_or(StoreError::RevisionConflict)?;
        ensure_safe_revision(new_revision)?;
        let n = tx
            .execute(
                "UPDATE codex_question_display
                 SET decision = 'active', revision = ?1, device_id = ?2, updated_at = ?3
                 WHERE thread_id = ?4 AND source_turn_id = ?5 AND source_item_id = ?6
                   AND content_generation = ?7 AND decision = 'skipped' AND revision = ?8",
                params![
                    new_revision,
                    request.device_id,
                    now,
                    request.group.thread_id,
                    request.group.source_turn_id,
                    request.group.source_item_id,
                    request.group.content_generation,
                    expected_rev,
                ],
            )
            .map_err(db_err)?;
        if n != 1 {
            return Err(StoreError::RevisionConflict);
        }
        // ⑤ Restore 动作行直接终态落账；native_effect 保持 NULL（restore 无原生面，
        //    读取层映射为保守 Unresolved，不伪造任何核证）
        let seq = next_action_seq(tx)?;
        tx.execute(
            "INSERT INTO codex_question_skip_actions
                (action_id, seq, kind, device_id, session_id, thread_host_id, thread_id,
                 group_turn_id, group_item_id, group_generation, canonical_payload, payload_digest,
                 state, revision_at_start, settled_at, created_at)
             VALUES (?1, ?2, 'restore', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'finished', ?12, ?13, ?13)",
            params![
                request.action_id,
                seq,
                request.device_id,
                request.session_id,
                request.thread.host_id,
                request.thread.thread_id,
                request.group.source_turn_id,
                request.group.source_item_id,
                request.group.content_generation,
                request.canonical_payload,
                request.payload_digest,
                expected_rev,
                now,
            ],
        )
        .map_err(db_err)?;
        Ok(ActionReceipt {
            action_id: request.action_id.clone(),
            display: DisplayDecision::Active,
            revision: new_revision,
            native: NativeReceipt {
                effect: NativeEffect::Unresolved,
                reason: SkipReason::None,
            },
        })
    })
}

/// recover：仅在启动且不存在活 owner 时由上层调用（本函数不执行任何 Native）。
/// 遗留 Prepared/Claimed 一律终定 Unresolved——已 claim 的是「已调用、结果未知」，
/// 未 claim 的是「永远不会再调用」；终定后禁止再次 claim/Invoke。返回终定行数。
pub(crate) fn recover(conn: &Connection, now: i64) -> Result<usize, StoreError> {
    run_in_transaction(conn, |tx| {
        let claimed = tx
            .execute(
                "UPDATE codex_question_skip_actions
                 SET state = 'finished', native_effect = ?1, native_reason = ?2, settled_at = ?3
                 WHERE state = 'claimed'",
                params![
                    effect_to_str(&NativeEffect::Unresolved),
                    reason_to_str(&SkipReason::InvokedUnconfirmed),
                    now,
                ],
            )
            .map_err(db_err)?;
        let prepared = tx
            .execute(
                "UPDATE codex_question_skip_actions
                 SET state = 'finished', native_effect = ?1, native_reason = ?2, settled_at = ?3
                 WHERE state = 'prepared'",
                params![
                    effect_to_str(&NativeEffect::Unresolved),
                    reason_to_str(&SkipReason::LeaseExpired),
                    now,
                ],
            )
            .map_err(db_err)?;
        Ok(claimed + prepared)
    })
}

/// 线程历史：一次批量取。每个 display group 一条：**外层**为 current display
/// （decision/revision 展示态），latest_skip 为该组最新 skip 动作（按 seq）的
/// **冻结结果**（Skipped、start+1、该动作自身落账 native）——两处分开表达，
/// Restore 不参与遮蔽，后续新动作不改写旧动作回执。快照 NULL（旧行）保守
/// 返回 None；存储枚举/JSON 严格解析，损坏即 Database。
pub(crate) fn list_thread_history(
    conn: &Connection,
    thread_id: &str,
) -> Result<Vec<HistoryEntry>, StoreError> {
    let rows = {
        let mut stmt = conn
            .prepare(
                "SELECT d.source_turn_id, d.source_item_id, d.content_generation,
                        d.decision, d.revision, d.source_snapshot_json,
                        a.action_id, a.native_effect, a.native_reason, a.revision_at_start
                 FROM codex_question_display AS d
                 LEFT JOIN codex_question_skip_actions AS a
                   ON a.thread_id = d.thread_id
                  AND a.group_turn_id = d.source_turn_id
                  AND a.group_item_id = d.source_item_id
                  AND a.group_generation = d.content_generation
                  AND a.kind = 'skip'
                  AND a.seq = (
                        SELECT MAX(a2.seq) FROM codex_question_skip_actions AS a2
                        WHERE a2.thread_id = d.thread_id
                          AND a2.group_turn_id = d.source_turn_id
                          AND a2.group_item_id = d.source_item_id
                          AND a2.group_generation = d.content_generation
                          AND a2.kind = 'skip'
                  )
                 WHERE d.thread_id = ?1
                 ORDER BY d.revision ASC, d.source_turn_id ASC, d.source_item_id ASC,
                          d.content_generation ASC",
            )
            .map_err(db_err)?;
        let mapped = stmt
            .query_map(params![thread_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    // LEFT JOIN：无动作行（迁移而来的 display 旧行）start 为 NULL——
                    // 必须按 Option 读，不得直接读 i64（旧行+新行混合查询才不会整批 Database）
                    r.get::<_, Option<i64>>(9)?,
                ))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;
        mapped
    };
    let mut out = Vec::with_capacity(rows.len());
    for (
        turn,
        item,
        generation,
        decision_raw,
        revision,
        snapshot_json,
        action_id,
        effect_raw,
        reason_raw,
        latest_skip_start_raw,
    ) in rows
    {
        let decision = decision_from_str(&decision_raw)?;
        let snapshot = match snapshot_json.as_deref() {
            Some(json) => Some(parse_snapshot(json)?),
            None => None,
        };
        let latest_skip = match (action_id, latest_skip_start_raw) {
            // 无动作行（如仅迁移而来的 display 旧行）：latest_skip 正常 None，不伪造
            (None, _) => None,
            // 有动作行却缺 revision_at_start：存储损坏（动作行必带起点），保守报
            // Database，不 unwrap_or(0) 伪造动作基点
            (Some(_), None) => return Err(StoreError::Database),
            (Some(id), Some(latest_skip_start)) => {
                let native = match (effect_raw.as_deref(), reason_raw.as_deref()) {
                    // 已落账效果优先；reason 缺失保守按 None（效果仍在，不猜测原因）
                    (Some(effect), reason) => NativeReceipt {
                        effect: effect_from_str(effect)?,
                        reason: match reason {
                            Some(raw) => reason_from_str(raw)?,
                            None => SkipReason::None,
                        },
                    },
                    // 在途（Prepared/Claimed）尚无落账：保守 Unresolved，不伪造已关闭
                    (None, _) => NativeReceipt {
                        effect: NativeEffect::Unresolved,
                        reason: SkipReason::None,
                    },
                };
                // latest_skip = **原动作的冻结结果**（Skip→Skipped、start+1），与外层
                // current display（decision/revision）分开表达：restore 不得遮蔽原生
                // 结果，后续新动作也不得改写旧动作回执。
                let skip_revision = latest_skip_start
                    .checked_add(1)
                    .ok_or(StoreError::RevisionConflict)?;
                ensure_safe_revision(skip_revision)?;
                Some(ActionReceipt {
                    action_id: id,
                    display: DisplayDecision::Skipped,
                    revision: skip_revision,
                    native,
                })
            }
        };
        out.push(HistoryEntry {
            group: GroupIdentity {
                thread_id: thread_id.to_string(),
                source_turn_id: turn,
                source_item_id: item,
                content_generation: generation,
            },
            decision,
            revision,
            snapshot,
            latest_skip,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{migration, schema};
    use rusqlite::params;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migration::migrate(&conn).unwrap();
        conn
    }
    fn req(id: &str, kind: RequestKind) -> RequestIdentity {
        RequestIdentity {
            action_id: id.into(),
            device_id: "device".into(),
            session_id: "session".into(),
            thread: ThreadKey {
                host_id: "local".into(),
                thread_id: "thread".into(),
            },
            group: GroupIdentity {
                thread_id: "thread".into(),
                source_turn_id: "turn".into(),
                source_item_id: "item".into(),
                content_generation: "gen1".into(),
            },
            kind,
            canonical_payload: "canonical-full-payload".into(),
            payload_digest: "digest".into(),
            typed_expected_revision: None,
        }
    }
    fn snapshot() -> QuestionSnapshot {
        QuestionSnapshot {
            question_key: "q1".into(),
            index: 0,
            title: "Full title".into(),
            options: vec!["first".into(), "second".into()],
        }
    }
    fn native(effect: NativeEffect) -> NativeReceipt {
        NativeReceipt {
            effect,
            reason: SkipReason::None,
        }
    }
    fn prepared(c: &Connection, r: &RequestIdentity) -> i64 {
        match prepare_skip(c, r, &snapshot(), 10).expect("skip must prepare") {
            PrepareOutcome::Prepared { revision } => revision,
            other => panic!("expected newly prepared: {other:?}"),
        }
    }
    fn columns(c: &Connection, table: &str) -> Vec<(String, i64)> {
        let mut s = c.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        let result = s
            .query_map([], |r| Ok((r.get(1)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        result
    }
    // 通过生产构造器绑定规范正文与版本。
    fn receipt_request(id: &str, kind: RequestKind, expected_revision: i64) -> RequestIdentity {
        let request = req(id, kind.clone());
        match kind {
            RequestKind::Skip => RequestIdentity::for_skip(
                request.action_id,
                request.device_id,
                request.session_id,
                request.thread,
                request.group,
                "binding",
            ),
            RequestKind::Restore => RequestIdentity::for_restore(
                request.action_id,
                request.device_id,
                request.session_id,
                request.thread,
                request.group,
                expected_revision,
            ),
        }
        .unwrap()
    }

    fn receipt_setup_finished_skip(c: &Connection) -> (RequestIdentity, NativeReceipt) {
        let skip = receipt_request("skip-a", RequestKind::Skip, 0);
        assert_eq!(prepared(c, &skip), 1);
        assert_eq!(
            claim_cas(c, &skip, "command-a", 11),
            Ok(ClaimOutcome::Claimed)
        );
        let result = native(NativeEffect::ClosedVerified);
        let first = finish(c, &skip, &result, 12).unwrap();
        assert_eq!(
            (first.display, first.revision, first.native),
            (DisplayDecision::Skipped, 1, result.clone())
        );
        (skip, result)
    }

    fn receipt_restore(c: &Connection) -> RequestIdentity {
        let request = receipt_request("restore-b", RequestKind::Restore, 1);
        let result = restore(c, &request, 1, 13).unwrap();
        assert_eq!(
            (result.display, result.revision),
            (DisplayDecision::Active, 2)
        );
        request
    }

    #[test]
    fn receipt_contract_prepare_replay_keeps_original_skip_after_restore() {
        let c = db();
        let (skip, result) = receipt_setup_finished_skip(&c);
        receipt_restore(&c);
        assert_eq!(
            prepare_skip(&c, &skip, &snapshot(), 14),
            Ok(PrepareOutcome::Existing(ActionReceipt {
                action_id: skip.action_id.clone(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: result,
            }))
        );
    }

    #[test]
    fn receipt_contract_finish_replay_keeps_original_skip_after_restore() {
        let c = db();
        let (skip, result) = receipt_setup_finished_skip(&c);
        receipt_restore(&c);
        assert_eq!(
            finish(&c, &skip, &result, 14),
            Ok(ActionReceipt {
                action_id: skip.action_id.clone(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: result,
            })
        );
    }

    #[test]
    fn receipt_contract_history_separates_current_display_from_latest_skip() {
        let c = db();
        let (skip, result) = receipt_setup_finished_skip(&c);
        receipt_restore(&c);
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            (history[0].decision.clone(), history[0].revision),
            (DisplayDecision::Active, 2)
        );
        assert_eq!(
            history[0].latest_skip,
            Some(ActionReceipt {
                action_id: skip.action_id.clone(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: result,
            })
        );
    }

    #[test]
    fn receipt_contract_restore_replay_keeps_original_result_after_new_skip() {
        let c = db();
        receipt_setup_finished_skip(&c);
        let restore_request = receipt_restore(&c);
        let later = receipt_request("skip-c", RequestKind::Skip, 0);
        assert_eq!(prepared(&c, &later), 3);
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(
            (history[0].decision.clone(), history[0].revision),
            (DisplayDecision::Skipped, 3)
        );
        assert_eq!(
            restore(&c, &restore_request, 1, 14),
            Ok(ActionReceipt {
                action_id: restore_request.action_id.clone(),
                display: DisplayDecision::Active,
                revision: 2,
                native: native(NativeEffect::Unresolved),
            })
        );
    }

    #[test]
    fn receipt_contract_first_late_finish_keeps_skip_result_and_current_restore() {
        let c = db();
        let skip = receipt_request("skip-a", RequestKind::Skip, 0);
        assert_eq!(prepared(&c, &skip), 1);
        assert_eq!(
            claim_cas(&c, &skip, "command-a", 11),
            Ok(ClaimOutcome::Claimed)
        );
        receipt_restore(&c);
        let result = native(NativeEffect::ClosedVerified);
        let late = finish(&c, &skip, &result, 14).unwrap();
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(
            (history[0].decision.clone(), history[0].revision),
            (DisplayDecision::Active, 2)
        );
        assert_eq!(
            late,
            ActionReceipt {
                action_id: skip.action_id.clone(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: result,
            }
        );
    }

    #[test]
    fn boundary_contract_first_restore_rejects_expected_revision_mismatch_before_writing() {
        let c = db();
        let thread = ThreadKey {
            host_id: "local".into(),
            thread_id: "thread".into(),
        };
        let group = GroupIdentity {
            thread_id: "thread".into(),
            source_turn_id: "turn".into(),
            source_item_id: "item".into(),
            content_generation: "gen1".into(),
        };
        let skip_a = RequestIdentity::for_skip(
            "skip-a".into(),
            "device".into(),
            "session".into(),
            thread.clone(),
            group.clone(),
            "binding-a",
        )
        .unwrap();
        assert_eq!(prepared(&c, &skip_a), 1);
        let restore_b = RequestIdentity::for_restore(
            "restore-b".into(),
            "device".into(),
            "session".into(),
            thread.clone(),
            group.clone(),
            1,
        )
        .unwrap();
        assert_eq!(restore(&c, &restore_b, 1, 11).unwrap().revision, 2);
        let skip_c = RequestIdentity::for_skip(
            "skip-c".into(),
            "device".into(),
            "session".into(),
            thread.clone(),
            group.clone(),
            "binding-c",
        )
        .unwrap();
        assert_eq!(prepared(&c, &skip_c), 3);
        let request = RequestIdentity::for_restore(
            "restore-mismatch".into(),
            "device".into(),
            "session".into(),
            thread,
            group.clone(),
            1,
        )
        .unwrap();

        assert!(
            restore(&c, &request, 3, 12).is_err(),
            "正文版本与独立参数不一致必须拒绝"
        );
        let current = load_display(&c, &group).unwrap().unwrap();
        assert_eq!(
            (current.decision, current.revision),
            (DisplayDecision::Skipped, 3)
        );
        assert!(load_action(&c, "restore-mismatch").unwrap().is_none());
        assert_eq!(
            restore(&c, &request, 1, 13),
            Err(StoreError::RevisionConflict)
        );
        let current = load_display(&c, &group).unwrap().unwrap();
        assert_eq!(
            (current.decision, current.revision),
            (DisplayDecision::Skipped, 3)
        );
        assert!(load_action(&c, "restore-mismatch").unwrap().is_none());
    }

    #[test]
    fn boundary_contract_history_reads_migrated_display_without_action_alongside_new_skip() {
        let c = Connection::open_in_memory().unwrap();
        // 仅构造旧库形态；新行由生产 DAO 写入。
        c.execute_batch("CREATE TABLE codex_question_display (thread_id TEXT NOT NULL, source_turn_id TEXT NOT NULL, source_item_id TEXT NOT NULL, content_generation TEXT NOT NULL, decision TEXT NOT NULL, revision INTEGER NOT NULL, device_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(thread_id,source_turn_id,source_item_id,content_generation)); INSERT INTO codex_question_display VALUES ('thread','turn','legacy','gen1','skipped',7,'device',42);").unwrap();
        schema::init(&c);
        migration::migrate(&c).unwrap();
        let thread = ThreadKey {
            host_id: "local".into(),
            thread_id: "thread".into(),
        };
        let group = GroupIdentity {
            thread_id: "thread".into(),
            source_turn_id: "turn".into(),
            source_item_id: "new".into(),
            content_generation: "gen1".into(),
        };
        let skip = RequestIdentity::for_skip(
            "new-skip".into(),
            "device".into(),
            "session".into(),
            thread,
            group.clone(),
            "binding",
        )
        .unwrap();
        assert_eq!(prepared(&c, &skip), 1);
        let history = list_thread_history(&c, "thread").expect("旧行与新行应可一起读取");
        assert_eq!(history.len(), 2);
        let legacy = history
            .iter()
            .find(|entry| entry.group.source_item_id == "legacy")
            .unwrap();
        assert_eq!(
            (legacy.decision.clone(), legacy.revision),
            (DisplayDecision::Skipped, 7)
        );
        assert_eq!(legacy.snapshot, None);
        assert_eq!(legacy.latest_skip, None);
        let new = history.iter().find(|entry| entry.group == group).unwrap();
        assert_eq!(
            (new.decision.clone(), new.revision),
            (DisplayDecision::Skipped, 1)
        );
        assert_eq!(new.snapshot, Some(snapshot()));
        assert_eq!(new.latest_skip.as_ref().unwrap().action_id, "new-skip");
    }

    #[test]
    fn fresh_schema_and_migration_create_both_tables_with_nullable_snapshot() {
        let c = Connection::open_in_memory().unwrap();
        schema::init(&c);
        migration::migrate(&c).unwrap();
        let display = columns(&c, "codex_question_display");
        assert!(
            display.contains(&("source_snapshot_json".into(), 0)),
            "snapshot column must exist and be nullable: {display:?}"
        );
        assert!(!columns(&c, "codex_question_skip_actions").is_empty());
    }
    #[test]
    fn legacy_display_upgrade_preserves_row_and_null_snapshot_idempotently() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE codex_question_display (thread_id TEXT NOT NULL, source_turn_id TEXT NOT NULL, source_item_id TEXT NOT NULL, content_generation TEXT NOT NULL, decision TEXT NOT NULL, revision INTEGER NOT NULL, device_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(thread_id,source_turn_id,source_item_id,content_generation)); INSERT INTO codex_question_display VALUES ('thread','turn','item','gen1','skipped',7,'device',42);").unwrap();
        migration::migrate(&c).unwrap();
        migration::migrate(&c).unwrap();
        let row: (String, i64, Option<String>) = c
            .query_row(
                "SELECT decision,revision,source_snapshot_json FROM codex_question_display",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("skipped".into(), 7, None));
    }
    #[test]
    fn prepare_persists_skipped_prepared_and_snapshot_atomically() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        let revision = prepared(&c, &r);
        assert!(revision > 0);
        let row: (String, i64, String) = c
            .query_row(
                "SELECT decision,revision,source_snapshot_json FROM codex_question_display",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "skipped");
        assert_eq!(row.1, revision);
        assert_eq!(
            serde_json::from_str::<QuestionSnapshot>(&row.2).unwrap(),
            snapshot()
        );
        let state: String = c
            .query_row(
                "SELECT state FROM codex_question_skip_actions WHERE action_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "prepared");
    }
    #[test]
    fn prepare_rolls_back_display_when_action_insert_fails() {
        let c = db();
        c.execute_batch("CREATE TRIGGER reject_intent BEFORE INSERT ON codex_question_skip_actions BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(prepare_skip(&c, &req("a", RequestKind::Skip), &snapshot(), 10).is_err());
        let count: i64 = c
            .query_row("SELECT count(*) FROM codex_question_display", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn prepare_rolls_back_action_when_display_insert_fails() {
        let c = db();
        c.execute_batch("CREATE TRIGGER reject_display BEFORE INSERT ON codex_question_display BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(prepare_skip(&c, &req("a", RequestKind::Skip), &snapshot(), 10).is_err());
        let count: i64 = c
            .query_row(
                "SELECT count(*) FROM codex_question_skip_actions",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn changed_snapshot_for_same_generation_never_overwrites_original() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        prepared(&c, &r);
        let mut changed = snapshot();
        changed.options.reverse();
        let r2 = req("b", RequestKind::Skip);
        assert!(prepare_skip(&c, &r2, &changed, 11).is_err());
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(history[0].snapshot, Some(snapshot()));
    }
    #[test]
    fn claim_is_single_use_and_checks_full_request_and_command_digest() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        prepared(&c, &r);
        assert_eq!(claim_cas(&c, &r, "command", 11), Ok(ClaimOutcome::Claimed));
        assert_eq!(
            claim_cas(&c, &r, "command", 12),
            Ok(ClaimOutcome::AlreadyClaimed)
        );
        let mut altered = r.clone();
        altered.device_id = "other".into();
        assert!(claim_cas(&c, &altered, "command", 13).is_err());
        assert!(claim_cas(&c, &r, "different", 14).is_err());
    }
    #[test]
    fn finish_requires_claim_and_terminal_result_cannot_be_upgraded() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        prepared(&c, &r);
        assert!(finish(&c, &r, &native(NativeEffect::ClosedVerified), 11).is_err());
        assert_eq!(claim_cas(&c, &r, "command", 12), Ok(ClaimOutcome::Claimed));
        let unresolved = native(NativeEffect::Unresolved);
        let first = finish(&c, &r, &unresolved, 13).unwrap();
        assert_eq!(finish(&c, &r, &unresolved, 14).unwrap(), first);
        let late = finish(&c, &r, &native(NativeEffect::ClosedVerified), 15);
        assert!(late.is_err() || late.unwrap() == first);
    }
    #[test]
    fn claimed_action_cannot_be_settled_as_never_invoked() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        prepared(&c, &r);
        claim_cas(&c, &r, "command", 11).unwrap();
        assert!(settle_without_invoke(&c, &r, &native(NativeEffect::Unsupported), 12).is_err());
        assert_eq!(
            finish(&c, &r, &native(NativeEffect::Unresolved), 13)
                .unwrap()
                .native
                .effect,
            NativeEffect::Unresolved
        );
    }
    #[test]
    fn restore_requires_existing_skipped_revision_and_late_finish_preserves_active() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        let rev = prepared(&c, &r);
        claim_cas(&c, &r, "command", 11).unwrap();
        let restore_req = receipt_request("restore", RequestKind::Restore, rev);
        assert_eq!(
            restore(&c, &restore_req, rev + 1, 12),
            Err(StoreError::RevisionConflict)
        );
        let restored = restore(&c, &restore_req, rev, 13).unwrap();
        assert_eq!(restored.display, DisplayDecision::Active);
        assert!(restored.revision > rev);
        finish(&c, &r, &native(NativeEffect::Unresolved), 14).unwrap();
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(history[0].decision, DisplayDecision::Active);
        assert_eq!(history[0].revision, restored.revision);
        assert_eq!(
            restore(
                &c,
                &req("again", RequestKind::Restore),
                restored.revision,
                15
            ),
            Err(StoreError::AlreadyActive)
        );
        let mut missing = req("missing", RequestKind::Restore);
        missing.group.source_item_id = "absent".into();
        assert_eq!(restore(&c, &missing, 1, 16), Err(StoreError::AlreadyActive));
    }
    #[test]
    fn restart_recovers_prepared_and_claimed_as_unresolved_without_reclaim() {
        let c = db();
        let a = req("a", RequestKind::Skip);
        let mut b = req("b", RequestKind::Skip);
        b.group.source_item_id = "item2".into();
        prepared(&c, &a);
        prepared(&c, &b);
        claim_cas(&c, &b, "command", 11).unwrap();
        assert_eq!(recover(&c, 12), Ok(2));
        assert_eq!(recover(&c, 13), Ok(0));
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(history.len(), 2);
        assert!(history
            .iter()
            .all(|h| h.latest_skip.as_ref().unwrap().native.effect == NativeEffect::Unresolved));
        assert!(claim_cas(&c, &a, "command", 14).is_err());
    }
    #[test]
    fn history_retains_latest_skip_result_across_restore_and_separates_generation() {
        let c = db();
        let a = req("a", RequestKind::Skip);
        let rev = prepared(&c, &a);
        settle_without_invoke(&c, &a, &native(NativeEffect::Unsupported), 11).unwrap();
        restore(
            &c,
            &receipt_request("r", RequestKind::Restore, rev),
            rev,
            12,
        )
        .unwrap();
        let mut next = req("next", RequestKind::Skip);
        next.group.content_generation = "gen2".into();
        prepared(&c, &next);
        settle_without_invoke(&c, &next, &native(NativeEffect::Unresolved), 13).unwrap();
        let history = list_thread_history(&c, "thread").unwrap();
        assert_eq!(history.len(), 2);
        let old = history
            .iter()
            .find(|h| h.group.content_generation == "gen1")
            .unwrap();
        assert_eq!(old.snapshot, Some(snapshot()));
        assert_eq!(old.decision, DisplayDecision::Active);
        assert_eq!(
            old.latest_skip.as_ref().unwrap().native.effect,
            NativeEffect::Unsupported
        );
        assert!(list_thread_history(&c, "other-thread").unwrap().is_empty());
    }
    #[test]
    fn cross_kind_action_id_conflicts_in_both_directions() {
        let c = db();
        let skip = req("same", RequestKind::Skip);
        let rev = prepared(&c, &skip);
        assert_eq!(
            restore(&c, &req("same", RequestKind::Restore), rev, 11),
            Err(StoreError::Conflict)
        );
        restore(
            &c,
            &receipt_request("restore-first", RequestKind::Restore, rev),
            rev,
            12,
        )
        .unwrap();
        assert_eq!(
            prepare_skip(
                &c,
                &req("restore-first", RequestKind::Skip),
                &snapshot(),
                13
            ),
            Err(StoreError::Conflict)
        );
    }
    #[test]
    fn typed_lookup_distinguishes_prepared_and_claimed_from_finished() {
        let c = db();
        let request = receipt_request("typed-phase", RequestKind::Skip, 0);
        prepared(&c, &request);
        let stored = lookup_action(&c, request.action_id()).unwrap().unwrap();
        assert_eq!(stored.request, request);
        assert_eq!(stored.phase, ActionPhase::Prepared);
        assert_eq!(stored.command_digest, None);
        assert_eq!(stored.receipt, None);

        claim_cas(&c, &request, "command-digest", 11).unwrap();
        let stored = lookup_action(&c, request.action_id()).unwrap().unwrap();
        assert_eq!(stored.request, request);
        assert_eq!(stored.phase, ActionPhase::Claimed);
        assert_eq!(stored.command_digest.as_deref(), Some("command-digest"));
        assert_eq!(stored.receipt, None);
    }

    #[test]
    fn typed_lookup_replays_immutable_skip_and_restore_receipts() {
        let c = db();
        let (skip, native_result) = receipt_setup_finished_skip(&c);
        let restored = receipt_restore(&c);
        let later = receipt_request("later-skip", RequestKind::Skip, 0);
        assert_eq!(prepared(&c, &later), 3);

        let stored_skip = lookup_action(&c, skip.action_id()).unwrap().unwrap();
        assert_eq!(stored_skip.request, skip);
        assert_eq!(stored_skip.phase, ActionPhase::Finished);
        assert_eq!(stored_skip.command_digest.as_deref(), Some("command-a"));
        assert_eq!(
            stored_skip.receipt,
            Some(ActionReceipt {
                action_id: "skip-a".into(),
                display: DisplayDecision::Skipped,
                revision: 1,
                native: native_result,
            })
        );
        let stored_restore = lookup_action(&c, restored.action_id()).unwrap().unwrap();
        assert_eq!(stored_restore.request, restored);
        assert_eq!(stored_restore.request.typed_expected_revision(), Some(1));
        assert_eq!(stored_restore.phase, ActionPhase::Finished);
        assert_eq!(stored_restore.command_digest, None);
        assert_eq!(
            stored_restore.receipt,
            Some(ActionReceipt {
                action_id: "restore-b".into(),
                display: DisplayDecision::Active,
                revision: 2,
                native: native(NativeEffect::Unresolved),
            })
        );
    }

    #[test]
    fn typed_lookup_rebuilds_complete_request_identity_and_missing_is_none() {
        let c = db();
        assert!(lookup_action(&c, "missing").unwrap().is_none());
        let base = req("identity", RequestKind::Skip);
        let request = RequestIdentity::for_skip(
            base.action_id,
            "device-distinct".into(),
            "session-distinct".into(),
            ThreadKey {
                host_id: "host-distinct".into(),
                thread_id: "thread".into(),
            },
            base.group,
            "binding-distinct",
        )
        .unwrap();
        prepared(&c, &request);
        let actual = lookup_action(&c, "identity").unwrap().unwrap().request;
        assert_eq!(actual, request);
        assert_eq!(actual.action_id(), "identity");
        assert_eq!(actual.device_id(), "device-distinct");
        assert_eq!(actual.session_id(), "session-distinct");
        assert_eq!(actual.thread().host_id, "host-distinct");
        assert_eq!(actual.thread().thread_id, "thread");
        assert_eq!(actual.group(), request.group());
        assert_eq!(actual.kind(), &RequestKind::Skip);
        assert_eq!(actual.canonical_payload(), request.canonical_payload());
        assert_eq!(actual.payload_digest(), request.payload_digest());
        assert_eq!(actual.typed_expected_revision(), None);
    }

    #[test]
    fn typed_lookup_rejects_corrupt_payload_digest_and_identity() {
        for case in ["invalid-json", "digest", "group", "empty-binding"] {
            let c = db();
            let request = receipt_request("corrupt", RequestKind::Skip, 0);
            prepared(&c, &request);
            let payload = match case {
                "invalid-json" => "{".to_string(),
                "digest" => request.canonical_payload().to_string(),
                "group" => request.canonical_payload().replace("\"gen1\"", "\"other\""),
                "empty-binding" => request.canonical_payload().replace("\"binding\"", "\"\""),
                _ => unreachable!(),
            };
            let digest = if case == "digest" {
                "incorrect".to_string()
            } else {
                digest_hex(payload.as_bytes())
            };
            c.execute("UPDATE codex_question_skip_actions SET canonical_payload=?1, payload_digest=?2 WHERE action_id=?3", params![payload, digest, "corrupt"]).unwrap();
            assert!(
                matches!(lookup_action(&c, "corrupt"), Err(StoreError::Database)),
                "case {case}"
            );
        }
    }

    #[test]
    fn typed_lookup_rejects_restore_expected_inconsistent_with_stored_revision() {
        let c = db();
        receipt_setup_finished_skip(&c);
        let request = receipt_restore(&c);
        let payload = request
            .canonical_payload()
            .replace("\"expectedRevision\":1", "\"expectedRevision\":2");
        assert_ne!(payload, request.canonical_payload());
        c.execute("UPDATE codex_question_skip_actions SET canonical_payload=?1, payload_digest=?2 WHERE action_id=?3", params![payload, digest_hex(payload.as_bytes()), request.action_id()]).unwrap();
        assert!(matches!(
            lookup_action(&c, request.action_id()),
            Err(StoreError::Database)
        ));
    }

    #[test]
    fn unbound_skip_is_distinct_from_bound_and_empty_binding_is_rejected() {
        let c = db();
        let base = req("unbound", RequestKind::Skip);
        let unbound = RequestIdentity::for_skip_unbound(
            base.action_id.clone(),
            base.device_id.clone(),
            base.session_id.clone(),
            base.thread.clone(),
            base.group.clone(),
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_str(unbound.canonical_payload()).unwrap();
        assert_eq!(payload.get("bindingId"), Some(&serde_json::Value::Null));
        assert_eq!(unbound.typed_expected_revision(), None);
        let bound = RequestIdentity::for_skip(
            base.action_id.clone(),
            base.device_id.clone(),
            base.session_id.clone(),
            base.thread.clone(),
            base.group.clone(),
            "binding",
        )
        .unwrap();
        assert_ne!(unbound.canonical_payload(), bound.canonical_payload());
        assert_ne!(unbound.payload_digest(), bound.payload_digest());
        assert!(matches!(
            RequestIdentity::for_skip(
                base.action_id,
                base.device_id,
                base.session_id,
                base.thread,
                base.group,
                "",
            ),
            Err(StoreError::InvalidState)
        ));
        prepared(&c, &unbound);
        assert_eq!(
            lookup_action(&c, "unbound").unwrap().unwrap().request,
            unbound
        );
        assert_eq!(
            prepare_skip(&c, &bound, &snapshot(), 11),
            Err(StoreError::Conflict)
        );
    }

    #[test]
    fn reused_id_rejects_changed_complete_payload_even_with_same_digest() {
        let c = db();
        let r = req("a", RequestKind::Skip);
        prepared(&c, &r);
        let mut changed = r.clone();
        changed.canonical_payload = "different-payload".into();
        assert_eq!(
            prepare_skip(&c, &changed, &snapshot(), 11),
            Err(StoreError::Conflict)
        );
        let count: i64 = c
            .query_row(
                "SELECT count(*) FROM codex_question_skip_actions WHERE action_id=?1",
                params!["a"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
