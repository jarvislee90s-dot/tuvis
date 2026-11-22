//! Codex 题卡跳过四路真实 HTTP（计划 B3 / 受控原型合同 §3/§5/§6）。
//!
//! 前缀 `/m/api/v1/codex/question-skip`（gated nest 内层注册：PIN gate 结构性
//! 覆盖，端点不带各自鉴权代码）：
//! - `POST /pair-preview`：只读候选发现（0 Invoke）；
//! - `POST /pair-confirm`：三项确认 + 原 worker 重读（0 Invoke）→ 绑定租约；
//! - `POST /skip`：coordinator 承接（有绑定 = 原 Native 绑定；缺省/null = 明确
//!   Tuvis-only；同包重试沿原 ID 幂等）；
//! - `POST /restore`：只回滚 Tuvis 展示（CAS expectedRevision，不要求当前源
//!   或 Native 在场）。
//!
//! **协议纪律**：cookie 决定 device——body 不收 device/PID/HWND/RID/scope；
//! outer/nested DTO `deny_unknown_fields`；两个确认只接受字面量 `true`、题数
//! 只接受字面量 `1`（缺失/null/错型一律拒绝）；`expectedRevision` 只接受整数
//! 1..=JS_SAFE_MAX；binding 缺省/null 统一 None、空串拒绝。
//! **响应纪律**：成功/业务错误/auth/JSON 提取/method 错误全 no-store——路由层
//! 中间件统一加盖（覆盖 handler 之外的 405/提取拒绝面），handler 内一律经
//! [`super::codex_ipc::json_no_store`]。
//! **ledger-first**：auth/DTO 之后才触碰 coordinator；Finished 原包回放零
//! source/session/Native 查询；新请求才 fresh resolve；来源多题/不完整/错代在
//! 写两表前拒绝；已承接 Unknown 返回 pending（503），不进 definite-refusal 白名单。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Json;
use serde::{de, Deserialize};

use super::api::{bad_request, device_identity, forbidden_defense};
use super::codex_ipc::{find_session_display_sync, json_no_store, resolve_session_thread};
use super::server::RemoteState;
use crate::database::dao::codex_question_display::ActionReceipt;
use crate::services::codex_desktop_ipc::types::ThreadKey;
use crate::services::codex_question_skip::binding::RequestOwner;
use crate::services::codex_question_skip::coordinator::{
    CoordinatorError, LookupOrJoin, MutationInput, MutationKind, OperationWaiter,
};
use crate::services::codex_question_skip::display::{effect_wire, reason_wire};
use crate::services::codex_question_skip::native_discovery::PairConfirmation;
use crate::services::codex_question_skip::types::{GroupIdentity, SkipReason};

/// JS 安全整数上界（与 [`crate::database::dao::codex_question_display`] 同一界；
/// 本模块独立常量避免引入 DAO 私有项）。
const JS_SAFE_MAX: i64 = 9_007_199_254_740_991;

// ---------------------------------------------------------------------------
// DTO（deny_unknown_fields；camelCase；字面量 true/1 在反序列化层强制）
// ---------------------------------------------------------------------------

/// group4 四元组（展示账主键；全量相等才算同组）。
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct GroupIdentityWire {
    thread_id: String,
    source_turn_id: String,
    source_item_id: String,
    content_generation: String,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PreviewReq {
    session_id: String,
    group: GroupIdentityWire,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ConfirmReq {
    session_id: String,
    group: GroupIdentityWire,
    challenge_id: String,
    candidate_token: String,
    #[serde(deserialize_with = "require_true")]
    same_thread_and_question: bool,
    #[serde(deserialize_with = "require_one")]
    native_panel_question_count: u8,
    #[serde(deserialize_with = "require_true")]
    native_draft_will_be_cleared: bool,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SkipReq {
    session_id: String,
    group: GroupIdentityWire,
    action_id: String,
    /// 缺省/null → None（明确 Tuvis-only）；空串在 handler 层拒绝（None 与
    /// Some 是不同请求，重试不得经无绑定构造器补 binding）。
    binding_id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct RestoreReq {
    session_id: String,
    group: GroupIdentityWire,
    action_id: String,
    /// 整数 1..=JS_SAFE_MAX（serde i64 天然拒绝浮点；0 只属于初始读视图）。
    expected_revision: i64,
}

/// 字面量 `true` 反序列化：缺失/null/false/字符串 "true"/数字一律拒绝。
fn require_true<'de, D: de::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(true) => Ok(true),
        _ => Err(de::Error::invalid_value(
            de::Unexpected::Other("non-literal-true"),
            &"literal true",
        )),
    }
}

/// 字面量 `1` 反序列化：缺失/null/0/2/浮点/字符串一律拒绝。
fn require_one<'de, D: de::Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) if n.as_u64() == Some(1) => Ok(1),
        _ => Err(de::Error::invalid_value(
            de::Unexpected::Other("non-literal-one"),
            &"literal 1",
        )),
    }
}

// ---------------------------------------------------------------------------
// no-store 路由层（覆盖 405 / JSON 提取拒绝等非 handler 响应面）
// ---------------------------------------------------------------------------

/// 全响应加盖 `Cache-Control: no-store`（E8：错误面同样不得被中间层缓存）。
pub(crate) async fn force_no_store(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

// ---------------------------------------------------------------------------
// 公共前置（auth → DTO 校验 → session/线程解析）
// ---------------------------------------------------------------------------

struct Resolved {
    device_id: String,
    session_id: String,
    thread: ThreadKey,
    group: GroupIdentity,
}

/// auth（cookie 设备）→ 字段硬边界 → session 定位 → resolver 权威映射（写
/// Session live 事实）。任何失败返回已构造的 no-store 错误响应。
// 早退直接返回已构造的 HTTP 错误，避免重新组装 no-store 响应。
#[allow(clippy::result_large_err)]
fn resolve_request(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    session_id: &str,
    wire: &GroupIdentityWire,
) -> Result<Resolved, Response> {
    let Some((device_id, _)) = device_identity(st, headers) else {
        return Err(forbidden_defense());
    };
    let session_id = session_id.trim().to_string();
    if session_id.is_empty()
        || wire.thread_id.is_empty()
        || wire.source_turn_id.is_empty()
        || wire.source_item_id.is_empty()
        || wire.content_generation.is_empty()
    {
        return Err(bad_request());
    }
    let Some(session) = find_session_display_sync(st, &session_id) else {
        return Err(not_found());
    };
    let Some(thread) = resolve_session_thread(st, &session) else {
        // 非 codex 会话 / rollout 无法解析：不拿 sessionId 冒充 ThreadKey。
        return Err(not_found());
    };
    if wire.thread_id != thread.thread_id {
        // 身份硬边界：group.threadId 必须与解析 ThreadKey 一致（与
        // RequestIdentity::validate 同判据）。
        return Err(not_found());
    }
    Ok(Resolved {
        device_id,
        session_id,
        thread,
        group: GroupIdentity {
            thread_id: wire.thread_id.clone(),
            source_turn_id: wire.source_turn_id.clone(),
            source_item_id: wire.source_item_id.clone(),
            content_generation: wire.content_generation.clone(),
        },
    })
}

fn not_found() -> Response {
    json_no_store(
        StatusCode::NOT_FOUND,
        serde_json::json!({ "error": "not_found" }),
    )
}

/// skip/restore 共用 auth + DTO 前置（M1 审查修复：不在此解析当前会话——
/// 账本查询先行，会话解析只属于 New 分支）。返回 (device_id, 构造中的
/// MutationInput)。
#[allow(clippy::result_large_err)] // 与 resolve_request 同一早退响应契约
fn authed_mutation_input(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    session_id: &str,
    wire: &GroupIdentityWire,
    action_id: &str,
) -> Result<(String, String, GroupIdentity, String), Response> {
    let Some((device_id, _)) = device_identity(st, headers) else {
        return Err(forbidden_defense());
    };
    let session_id = session_id.trim().to_string();
    if session_id.is_empty()
        || action_id.trim().is_empty()
        || wire.thread_id.is_empty()
        || wire.source_turn_id.is_empty()
        || wire.source_item_id.is_empty()
        || wire.content_generation.is_empty()
    {
        return Err(bad_request());
    }
    Ok((
        device_id,
        session_id,
        GroupIdentity {
            thread_id: wire.thread_id.clone(),
            source_turn_id: wire.source_turn_id.clone(),
            source_item_id: wire.source_item_id.clone(),
            content_generation: wire.content_generation.clone(),
        },
        action_id.trim().to_string(),
    ))
}

/// C-R1：经完整请求身份校验的 join 等待（Wait / New→submit_new 并发 join 两路
/// 统一入口）——先唤醒原结算 owner 再 join。settle_loop 首次结算读账临时失败后
/// 等 retry，只 join 等待 done 会让 owner 永久挂起（Prepared 与原线程锁滞留）。
/// retry_settlement 仅唤醒/补只结算任务：不重建 binding、不重进 executor、不再
/// Invoke；先唤醒后 join，通知与等待的交错由 retry 的 permit 语义兜底（先到不
/// 丢）；对存活 owner 至多存一枚 permit，settlement_step 幂等重读账，无副作用。
async fn validated_join(
    coordinator: &std::sync::Arc<
        crate::services::codex_question_skip::coordinator::QuestionSkipCoordinator,
    >,
    action_id: &str,
    mut waiter: OperationWaiter,
) -> Result<ActionReceipt, CoordinatorError> {
    coordinator.retry_settlement(action_id);
    waiter.wait().await
}

/// skip/restore 共用编排（M1 审查修复：ledger-first）——auth → DTO →
/// coordinator 账本查询；**Replay/Wait/Err 不要求旧 session/source/lease 在场**
/// （Finished 原包回放零旧 session 依赖）；仅 New 才 fresh resolve
/// （session→ThreadKey + Session live 事实）并 submit。
async fn mutation_flow(
    st: &Arc<RemoteState>,
    device_id: String,
    input: MutationInput,
    expect_skipped: bool,
) -> Response {
    let group = input.group.clone();
    let Some(coordinator) = st.codex_control.question_skip_coordinator() else {
        // disabled hub：无承接点——保守 Pending（不冒充终态）。
        return operation_pending();
    };
    let result = match coordinator.lookup_or_join(&device_id, &input).await {
        Ok(LookupOrJoin::New) => {
            // 新请求才 fresh resolve；解析失败（会话离场/非 codex/映射缺失）→
            // 404（账内无行，无原回执可回放——只有 New 分支受当前会话存在性约束）。
            let Some(session) = find_session_display_sync(st, &input.session_id) else {
                return not_found();
            };
            let Some(thread) = resolve_session_thread(st, &session) else {
                return not_found();
            };
            if group.thread_id != thread.thread_id {
                // 身份硬边界：group.threadId 必须与解析 ThreadKey 一致（与
                // RequestIdentity::validate 同判据）。
                return not_found();
            }
            // B-R1：线程首次权威使用——提交前完整装载 Claims/Stop 真实事实
            // （失败保持 Unknown → 守卫继续拒绝，不伪造空集）。
            st.codex_control
                .ensure_question_skip_thread_facts(&thread)
                .await;
            // C-R1（审查修复）：New→submit_new 之间可能发现并发 join——返回
            // 的也可能是**已有 owner** 的 waiter（同包先行者已登记），该 owner
            // 可能正处于结算等待，与 Wait 分支同需要唤醒（统一走
            // validated_join；若确为本请求新建 owner，retry_settlement 对
            // 存活 owner 至多存一枚 permit，settlement_step 幂等，无副作用）。
            let action_id = input.action_id.clone();
            match coordinator.submit_new(device_id, input, thread, false) {
                Ok(waiter) => validated_join(&coordinator, &action_id, waiter).await,
                Err(err) => Err(err),
            }
        }
        Ok(LookupOrJoin::Wait(waiter)) => {
            // C-R1：同包重发（完整请求身份校验已在 lookup_or_join 内通过）必须
            // 唤醒原结算 owner——settle_loop 首次结算读账临时失败后等 retry，
            // 只 join 等待 done 会让 owner 永久挂起（Prepared 与原线程锁滞留）。
            // retry_settlement 仅唤醒/补只结算任务：不重建 binding、不重进
            // executor、不再 Invoke；异包 409 在 lookup_or_join 内完成，不会到
            // 此处（不会唤醒错误 owner）。先唤醒后 join：通知与等待的交错由
            // retry 的 permit 语义兜底（先到不丢）。
            validated_join(&coordinator, &input.action_id, waiter).await
        }
        Ok(LookupOrJoin::Replay(receipt)) => Ok(receipt),
        Err(err) => Err(err),
    };
    match result {
        Ok(receipt) => mutation_receipt_response(receipt, &group, expect_skipped),
        Err(err) => map_coordinator_err(err),
    }
}

/// 保守 Pending（503）：已承接/提交未知/事实未装载/来源暂不可用——一律不进
/// definite-refusal 白名单（不诱导重发、不冒充终态）。
fn operation_pending() -> Response {
    json_no_store(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({ "error": "operation_pending" }),
    )
}

fn store_unavailable() -> Response {
    json_no_store(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({ "error": "store_unavailable" }),
    )
}

/// 配对/来源拒绝 → HTTP：与 coordinator `map_binding_refusal` 同判据——确证
/// 未承接才 definite Refused；其余保守 Pending。
fn refuse_to_response(reason: SkipReason) -> Response {
    match reason {
        SkipReason::BindingRequired => json_no_store(
            StatusCode::CONFLICT,
            serde_json::json!({ "error": "binding_required" }),
        ),
        SkipReason::LeaseExpired | SkipReason::LeaseConsumed => json_no_store(
            StatusCode::GONE,
            serde_json::json!({ "error": "lease_expired" }),
        ),
        SkipReason::ScopeChanged => json_no_store(
            StatusCode::CONFLICT,
            serde_json::json!({ "error": "scope_changed" }),
        ),
        _ => operation_pending(),
    }
}

fn map_coordinator_err(err: CoordinatorError) -> Response {
    match err {
        CoordinatorError::Refused { code, status } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_no_store(status, serde_json::json!({ "error": code }))
        }
        CoordinatorError::Pending { .. } => operation_pending(),
        CoordinatorError::StoreUnavailable => store_unavailable(),
    }
}

fn group_wire(group: &GroupIdentity) -> serde_json::Value {
    serde_json::json!({
        "threadId": group.thread_id,
        "sourceTurnId": group.source_turn_id,
        "sourceItemId": group.source_item_id,
        "contentGeneration": group.content_generation,
    })
}

// ---------------------------------------------------------------------------
// 四路 handler
// ---------------------------------------------------------------------------

/// POST /codex/question-skip/pair-preview：只读候选发现（0 Invoke）。
pub(crate) async fn pair_preview(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<PreviewReq>,
) -> Response {
    let resolved = match resolve_request(&st, &headers, &req.session_id, &req.group) {
        Ok(r) => r,
        Err(res) => return res,
    };
    // B-R1：配对路径同为线程首次权威使用——先完整装载再取源。
    st.codex_control
        .ensure_question_skip_thread_facts(&resolved.thread)
        .await;
    let source = match st
        .codex_control
        .question_skip_live()
        .try_source(&resolved.thread)
    {
        Ok(source) => source,
        Err(_) => return operation_pending(),
    };
    let owner = RequestOwner {
        device_id: resolved.device_id,
        thread: resolved.thread,
    };
    let native = st.codex_control.question_skip_native();
    match native
        .pair_preview(owner, resolved.session_id, source, resolved.group.clone())
        .await
    {
        Ok(preview) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "challengeId": preview.challenge_id,
                "candidates": preview.candidates.into_iter().map(|c| serde_json::json!({
                    "token": c.token,
                    "question": c.question.title,
                    "options": c.question.options,
                    "label": c.label,
                })).collect::<Vec<_>>(),
            }),
        ),
        Err(refusal) => refuse_to_response(refusal.0),
    }
}

/// POST /codex/question-skip/pair-confirm：三项确认 + 原 worker 重读 → 绑定租约。
pub(crate) async fn pair_confirm(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<ConfirmReq>,
) -> Response {
    let resolved = match resolve_request(&st, &headers, &req.session_id, &req.group) {
        Ok(r) => r,
        Err(res) => return res,
    };
    if req.challenge_id.is_empty() || req.candidate_token.is_empty() {
        return bad_request();
    }
    // B-R1：同 preview——confirm 前确保该线程事实已完整装载。
    st.codex_control
        .ensure_question_skip_thread_facts(&resolved.thread)
        .await;
    let source = match st
        .codex_control
        .question_skip_live()
        .try_source(&resolved.thread)
    {
        Ok(source) => source,
        Err(_) => return operation_pending(),
    };
    let owner = RequestOwner {
        device_id: resolved.device_id,
        thread: resolved.thread,
    };
    let confirmation = PairConfirmation {
        challenge_id: req.challenge_id,
        candidate_token: req.candidate_token,
        // DTO 字面量层已强制 true/1——这里按合同语义重建。
        same_thread_and_question: req.same_thread_and_question,
        native_panel_question_count: Some(req.native_panel_question_count as usize),
        native_draft_will_be_cleared: req.native_draft_will_be_cleared,
    };
    let native = st.codex_control.question_skip_native();
    match native
        .pair_confirm(
            owner,
            resolved.session_id,
            source,
            resolved.group.clone(),
            confirmation,
        )
        .await
    {
        Ok(binding) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "bindingId": binding.binding_id,
                "expiresAt": binding.expires_at_ms,
            }),
        ),
        Err(refusal) => refuse_to_response(refusal.0),
    }
}

/// POST /codex/question-skip/skip：coordinator 承接（ledger-first；同包幂等）。
pub(crate) async fn skip(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<SkipReq>,
) -> Response {
    let (device_id, session_id, group, action_id) =
        match authed_mutation_input(&st, &headers, &req.session_id, &req.group, &req.action_id) {
            Ok(v) => v,
            Err(res) => return res,
        };
    if req.binding_id.as_deref().is_some_and(|b| b.is_empty()) {
        return bad_request();
    }
    let input = MutationInput {
        action_id,
        session_id,
        group,
        kind: MutationKind::Skip {
            binding_id: req.binding_id.clone(),
        },
    };
    mutation_flow(&st, device_id, input, true).await
}

/// POST /codex/question-skip/restore：只回滚 Tuvis 展示（CAS；不读当前源/Native；
/// ledger-first——原包回放不要求旧 session 在场）。
pub(crate) async fn restore(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<RestoreReq>,
) -> Response {
    let (device_id, session_id, group, action_id) =
        match authed_mutation_input(&st, &headers, &req.session_id, &req.group, &req.action_id) {
            Ok(v) => v,
            Err(res) => return res,
        };
    if req.expected_revision < 1 || req.expected_revision > JS_SAFE_MAX {
        return bad_request();
    }
    let input = MutationInput {
        action_id,
        session_id,
        group,
        kind: MutationKind::Restore {
            expected_revision: req.expected_revision,
        },
    };
    mutation_flow(&st, device_id, input, false).await
}

/// skip/restore 回执 → wire（安全边界：display 决策必须与动作类别一致；不一致
/// = 内部状态异常，保守 503，不伪造回执）。
fn mutation_receipt_response(
    receipt: ActionReceipt,
    group: &GroupIdentity,
    expect_skipped: bool,
) -> Response {
    let display_ok = if expect_skipped {
        receipt.display == crate::services::codex_question_skip::types::DisplayDecision::Skipped
    } else {
        receipt.display == crate::services::codex_question_skip::types::DisplayDecision::Active
    };
    if !display_ok {
        return store_unavailable();
    }
    let decision = if expect_skipped { "skipped" } else { "active" };
    json_no_store(
        StatusCode::OK,
        serde_json::json!({
            "actionId": receipt.action_id,
            "group": group_wire(group),
            "displayDecision": decision,
            "nativeEffect": effect_wire(&receipt.native.effect),
            "reasonCode": reason_wire(&receipt.native.reason),
            "revision": receipt.revision,
        }),
    )
}
