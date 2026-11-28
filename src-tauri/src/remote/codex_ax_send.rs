//! Codex Desktop macOS 纯文本候选。持久冻结本次动作后，可按需定向加载原会话，
//! 再独立核证目标、填入与提交。结果未知时只读核对，不重按或换通道。

#![cfg(target_os = "macos")]

use axum::http::StatusCode;
use axum::response::Response;

use crate::remote::api::json_no_store;
use crate::services::codex_desktop_ax_ledger::SqliteActionJournal;
use crate::services::codex_desktop_ax_macos::MacAxProvider;
use crate::services::codex_desktop_ax_sender::{
    send_registered, ActionPhase, SendBudget, SendOutcome, SendRefusal, SendRequest, TargetKey,
};
use crate::services::codex_desktop_ipc::types::Delivery;
use crate::services::codex_desktop_window::{
    approved_target_for, evaluate_frame_text, lookup_thread_by_title,
    resolve_current_target_for_row, HostIdentity, IndexLookup, TargetBinding,
};

/// 一次 AX 发送的结果，直接映射成 HTTP 回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxSendVerdict {
    /// 原线程已确认收到（消息出现 + 编辑器被消费）。
    Delivered,
    /// 明确没碰过输入框。
    NotTouched,
    /// 可能改了编辑区但明确没提交。
    TouchedNotSubmitted,
    /// 已按下，结果未确认（含"消息出现但编辑器仍在"的重复风险）。
    MaybeSubmitted,
}

impl AxSendVerdict {
    fn from_outcome(o: SendOutcome) -> Self {
        match o {
            SendOutcome::Submitted => Self::Delivered,
            SendOutcome::NotTouched => Self::NotTouched,
            SendOutcome::TouchedNotSubmitted => Self::TouchedNotSubmitted,
            SendOutcome::MaybeSubmitted => Self::MaybeSubmitted,
        }
    }

    /// 手机端用的稳定字符串。**不得**把 `MaybeSubmitted` 写成任何"没发"的措辞。
    pub fn wire_code(self) -> &'static str {
        match self {
            Self::Delivered => "desktop_ax_delivered",
            Self::NotTouched => "desktop_ax_not_touched",
            Self::TouchedNotSubmitted => "desktop_ax_touched_not_submitted",
            Self::MaybeSubmitted => "desktop_ax_maybe_submitted",
        }
    }
}

/// 拒绝原因 → 稳定的、可翻译的错误码。
///
/// 每一个都对应一个具体的、已检查过的失败面；手机端据此显示不同提示。
fn refusal_code(r: SendRefusal) -> &'static str {
    match r {
        SendRefusal::TargetIncomplete => "ax_target_incomplete",
        SendRefusal::BindingNotMatched => "ax_target_not_current",
        SendRefusal::WindowGone => "ax_session_view_gone",
        SendRefusal::ViewGenerationChanged => "ax_view_changed",
        SendRefusal::ObserveFailed => "ax_observe_failed",
        SendRefusal::NotIdle => "ax_session_busy",
        SendRefusal::EditorAmbiguous => "ax_editor_ambiguous",
        SendRefusal::DraftNotProvablyEmpty => "ax_draft_present",
        SendRefusal::TextNotAdopted => "ax_text_not_adopted",
        SendRefusal::SendControlUnavailable => "ax_send_unavailable",
        SendRefusal::TargetChangedMidFlight => "ax_target_changed",
        SendRefusal::DesktopEdited => "ax_desktop_edited",
        SendRefusal::DuplicateAction => "ax_duplicate_action",
        SendRefusal::DeliveryUnconfirmed => "ax_delivery_unconfirmed",
        SendRefusal::EditorNotConsumed => "ax_editor_not_consumed",
        SendRefusal::JournalUnavailable => "ax_ledger_unavailable",
    }
}

/// 一次 AX 发送尝试的完整输入。
pub struct AxSendInput<'a> {
    pub pid: u32,
    pub host: HostIdentity,
    /// 只读帧文本（由调用方用采样器取得；本模块不自己启动子进程）。
    pub frame_text: &'a str,
    pub view_generation: u64,
    pub row_id: &'a str,
    pub project_group: &'a str,
    pub thread_id: &'a str,
    pub title_hash: &'a str,
    pub workspace: &'a str,
    pub project_label_hash: &'a str,
    pub rollout_path: std::path::PathBuf,
    pub action_id: &'a str,
    pub text: &'a str,
    /// 真实鉴权设备（**不是** action_id）。
    pub device_id: &'a str,
}

/// 执行一次 AX 发送，返回 HTTP 回执。
///
/// 这里**不做**鉴权（调用方已完成设备鉴权），只做通道内的目标核证与状态机执行。
fn run_ax_send(
    input: &AxSendInput<'_>,
    journal: &mut SqliteActionJournal,
    device_still_valid: &impl Fn() -> bool,
) -> (StatusCode, serde_json::Value) {
    // 1) 只读帧解析。
    let report = match evaluate_frame_text(input.frame_text, input.view_generation) {
        Ok(r) => r,
        Err(reason) => {
            return (
                StatusCode::OK,
                serde_json::json!({
                    "status": "desktop_ax_not_touched",
                    "route": "desktop_ax",
                    "actionId": input.action_id,
                    "error": "desktop_ax_refused",
                    "reasonCode": reason,
                    "draftMayBeKept": true,
                }),
            )
        }
    };

    // 2) 批准目标必须来自**独立**宿主记录，不能从被观测帧反取。
    let Some(approved) = approved_target_for(
        &input.host,
        report.rows.len(),
        report.counts.rows_enumerated,
        input.thread_id,
        input.workspace,
        input.title_hash,
        input.project_label_hash,
        input.view_generation,
    ) else {
        return (
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_not_touched",
                "route": "desktop_ax",
                "actionId": input.action_id,
                "reasonCode": "ax_target_incomplete",
                "draftMayBeKept": true,
            }),
        );
    };

    // 3) 目标必须是**当前会话**（阶段一硬边界）。
    let binding = resolve_current_target_for_row(
        &report,
        &approved,
        input.row_id,
        input.project_group,
        |h| lookup_thread_by_title(input.workspace, h).unwrap_or(IndexLookup::None),
    );
    if binding != TargetBinding::Matched {
        let reason = match binding {
            TargetBinding::OtherThread => "ax_target_not_current",
            TargetBinding::Ambiguous => "ax_target_ambiguous",
            _ => "ax_target_unverifiable",
        };
        return (
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_not_touched",
                "route": "desktop_ax",
                "actionId": input.action_id,
                "reasonCode": reason,
                // 明确告诉手机：桌面不在该会话上，草稿请保留。
                "draftMayBeKept": true,
            }),
        );
    }

    // 4) 装配真实提供方。没有辅助功能权限时如实拒绝。
    let Some(mut provider) = MacAxProvider::new(input.pid, input.host.clone()) else {
        return (
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_unavailable",
                "route": "desktop_ax",
                "actionId": input.action_id,
                "reasonCode": "ax_not_trusted",
                "draftMayBeKept": true,
            }),
        );
    };
    provider.bind_thread(input.row_id, input.thread_id, input.rollout_path.clone());

    // 5) 状态机。
    //
    // `device_id` 用**真实鉴权设备**（R2）。旧实现先填空串、再用 `input.action_id`
    // 覆盖——那是拿幂等键冒充设备身份，让跨设备去重与审计全部失真。
    let key = TargetKey {
        device_id: input.device_id.to_string(),
        host: input.host.clone(),
        session_row_id: input.row_id.to_string(),
        project_group: input.project_group.to_string(),
        view_generation: input.view_generation,
        binding_generation: 1,
        thread_id: input.thread_id.to_string(),
        title_hash: input.title_hash.to_string(),
        workspace_key: input.workspace.to_string(),
        project_label_hash: input.project_label_hash.to_string(),
    };

    // **提交前必须真正重新采样**（R2 bullet 3）。
    //
    // 旧实现返回首次 `report.clone()`：那等于"我核证的是我自己的旧结论"，
    // 桌面在填字与提交之间切走也发现不了。这里每次调用都重新取一帧并重新解析。
    let mut report_for = || {
        if !device_still_valid() {
            return None;
        }
        // **每次重新取宿主身份**（复审 Important-7）。
        //
        // 旧实现把锁前采到的 `input.host` 直接传给采样器与解析器：那样"宿主代际
        // 复核"就恒真（自己跟自己比），App 换代后依然声称一致。
        //
        // 正确做法：实时查宿主，用**实时代际**采样；而**期望代际**仍用批准时的
        // `input.view_generation`，这样换代会被真实捕获成不一致。
        let live = live_host_identity(input.pid)?;
        let s = crate::services::codex_desktop_ax_sample::sample_sidebar(
            input.pid,
            live.launch_generation,
            live.login_session,
        )?;
        let text = s.frame_text();
        crate::services::codex_desktop_window::evaluate_frame_text(&text, input.view_generation)
            .ok()
    };
    let request = SendRequest {
        action_id: input.action_id,
        key: &key,
        text: input.text,
    };
    let result = send_registered(
        &provider,
        journal,
        &request,
        SendBudget {
            adopt_polls: 12,
            adopt_poll_interval_ms: 120,
            verify_polls: 16,
            // 真机实测：没有轮询间隔时 12 轮在毫秒级跑完，而 rollout 落盘要秒级，
            // 导致消息已送达却报 maybe_submitted。250ms × 16 ≈ 4 秒。
            verify_poll_interval_ms: 250,
        },
        &mut report_for,
        &|h: &str| lookup_thread_by_title(input.workspace, h).unwrap_or(IndexLookup::None),
    );

    let verdict = AxSendVerdict::from_outcome(result.outcome);
    let mut body = serde_json::json!({
        "status": verdict.wire_code(),
        "route": "desktop_ax",
        "actionId": input.action_id,
        "phase": format!("{:?}", result.phase),
    });
    if let Some(refusal) = result.refusal {
        body["reasonCode"] = serde_json::Value::String(refusal_code(refusal).to_string());
    }
    // 只要可能碰过编辑区，就必须告诉手机"草稿不能当没发过"。
    if verdict != AxSendVerdict::NotTouched {
        body["draftMayBeKept"] = serde_json::Value::Bool(true);
        body["desktopMayHoldUnsent"] =
            serde_json::Value::Bool(verdict == AxSendVerdict::TouchedNotSubmitted);
        body["needsMacConfirmation"] =
            serde_json::Value::Bool(verdict == AxSendVerdict::MaybeSubmitted);
    }
    // **一律 200**：AX 的四档是**业务结果**，不是 HTTP 语义错误。
    //
    // 手机端的 `sessionSend` 对非 2xx 一律抛 `ApiError`（走"网络/服务失败"路径），
    // 那样"明确未触及、请保留草稿"就会被误当成通道故障，草稿处理逻辑也走错分支。
    // 让四档都走 200 + `status` 字段，前端按 `status` 分流。
    (StatusCode::OK, body)
}

/// 读失败也视为有屏障，GET 和 POST 共用；旧动作的只读核对不受此判断替代。
pub(super) fn ax_send_blocked_by_barrier(
    store: &crate::services::codex_control::queue::ActionStore,
    thread_id: &str,
) -> bool {
    store
        .armed_barriers_for_thread(thread_id)
        .map(|barriers| !barriers.is_empty())
        .unwrap_or(true)
}

/// `session_send` 的 AX 分支入口。
///
/// 先核对持久动作和跨通道屏障，再识别真实宿主并按需定向加载原会话。
/// 现场证据缺失时拒绝，结果未知时不改用其它通道。
pub async fn session_send_via_ax(
    st: &std::sync::Arc<crate::remote::server::RemoteState>,
    session: &crate::session::Session,
    device_id: &str,
    device_name: &str,
    text: &str,
    action_id: &str,
) -> Response {
    // **真实鉴权设备**（不是 action_id）。计划 R2 明写「不把 `action_id` 填进
    // `device_id`」——旧实现正是那么干的，导致审计与跨设备去重全部失真。
    debug_assert!(!device_id.trim().is_empty(), "device_id 必须来自真实鉴权");
    // AX 不允许在 IPC 使用另一原线程键时另建一份账本。
    if !super::codex_ipc::resolve_session_thread(st, session)
        .is_some_and(|key| key.thread_id == session.id)
    {
        let (status, mut body) = nav_refusal("ax_target_unverifiable");
        body["actionId"] = serde_json::json!(action_id);
        return json_no_store(status, body);
    }

    // **账本前置**（复审 Important-5）：同 ID 幂等与跨通道屏障是**纯 DB 读**，
    // 不需要任何 AX 探测。旧顺序先解析宿主/采帧后才查账本，重复请求会白跑一遍
    // AX 解析（还要拿单飞锁），既慢又给"重放看起来像真的在发"的错觉。
    //
    // 顺序固定为：**能证明不该发就尽早拒绝 → 再做需要权限的探测**。
    let store = st.codex_control.router.store().clone();
    {
        let probe = crate::services::codex_desktop_ax_ledger::SqliteActionJournal::new(
            store.clone(),
            device_id.to_string(),
            device_name.to_string(),
            "local".to_string(),
            session.id.clone(),
        );
        // 跨通道屏障：本线程**仍然武装着**的保守屏障（任何通道）。
        //
        // 判据与产品既有准入**逐字一致**：`compat_outstanding = 1` 且未被人为解除。
        // **不能**用"所有 state='unknown' 的行"代替——启动恢复产生的
        // `aborted_dispatch_unknown` 里只有一部分会武装屏障，其余是已收敛的审计
        // 事实，既有 IPC 路径不会因它们拦新写入。按"所有 unknown"拦会让历史残留
        // 永久锁死会话（实机：`hi` 线程 3 条 2026-10-30/31 残留，2 条未武装）。
        //
        if ax_send_blocked_by_barrier(&store, &session.id) && !probe.exists(action_id) {
            return json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "status": "desktop_ax_not_touched",
                    "error": "desktop_ax_refused",
                    "reasonCode": "ax_action_unresolved",
                    "route": "desktop_ax",
                    "actionId": action_id,
                    "draftMayBeKept": true,
                }),
            );
        }
        if probe.exists(action_id) {
            let phase = probe.phase_from_ledger(action_id);
            let want_hash = crate::services::codex_desktop_window::sha256_prefix12(text);
            let same_payload = probe.payload_hash_of(action_id).as_deref() == Some(&want_hash);
            let same_device_thread = probe
                .identity_of(action_id)
                .map(|(d, t)| d == device_id && t == session.id)
                .unwrap_or(false);
            if !same_payload || !same_device_thread {
                return json_no_store(
                    StatusCode::OK,
                    serde_json::json!({
                        "status": "desktop_ax_not_touched",
                        "error": "desktop_ax_refused",
                        "reasonCode": "ax_action_id_reused_with_different_payload_or_target",
                        "route": "desktop_ax",
                        "actionId": action_id,
                        "draftMayBeKept": true,
                    }),
                );
            }
            let (status, reason) = match phase {
                Some(ActionPhase::Verified) => ("desktop_ax_delivered", None),
                Some(ActionPhase::AboutToFill)
                | Some(ActionPhase::Filled)
                | Some(ActionPhase::AboutToSubmit)
                | Some(ActionPhase::SubmitAttempted) => (
                    "desktop_ax_maybe_submitted",
                    Some("ax_action_already_in_flight"),
                ),
                _ => ("desktop_ax_not_touched", Some("ax_duplicate_action")),
            };
            let mut body = serde_json::json!({
                "status": status,
                "route": "desktop_ax",
                "actionId": action_id,
                "duplicate": true,
                "draftMayBeKept": true,
            });
            if let Some(r) = reason {
                body["reasonCode"] = serde_json::Value::String(r.to_string());
            }
            if status == "desktop_ax_maybe_submitted" {
                body["needsMacConfirmation"] = serde_json::Value::Bool(true);
            }
            return json_no_store(StatusCode::OK, body);
        }
    }

    // **`session.pid` 是 Codex 的 `exec-server` 子进程，不是主 App。**
    //
    // 实测：会话 PID 2560 = `<主App>/Contents/Resources/codex-cli/.../codex exec-server`，
    // 而 AX 只对主 App 进程（2075 = `ChatGPT.app/Contents/MacOS/ChatGPT`）有效。
    // 直接拿会话 PID 采样会得到 `sample_sidebar = None`（日志里那句
    // "sample_sidebar returned None (pid=2560)"）。
    //
    // 因此这里向上解析到 `.app` 的 MacOS 可执行主进程；解析不出就拒绝。
    let Some(pid) = resolve_app_pid(session.pid) else {
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_unavailable",
                "error": "desktop_ax_refused",
                "reasonCode": "ax_host_pid_unresolved",
                "route": "desktop_ax",
                "actionId": action_id,
                "draftMayBeKept": true,
            }),
        );
    };
    if pid == 0 {
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_unavailable",
                "error": "desktop_ax_refused",
                "reasonCode": "ax_no_host_pid",
                "route": "desktop_ax",
                "actionId": action_id,
                "draftMayBeKept": true,
            }),
        );
    }

    // 宿主身份必须**独立于**被观测帧：从实时进程取启动时间换算成代际。
    let Some(host) = live_host_identity(pid) else {
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_unavailable",
                "error": "desktop_ax_refused",
                "reasonCode": "ax_host_identity_unavailable",
                "route": "desktop_ax",
                "actionId": action_id,
                "draftMayBeKept": true,
            }),
        );
    };

    // 在飞时不排队。租约随阻塞任务持有，HTTP 断连也不会在 AX 尚未结束时释放。
    let Ok(single_flight_guard) = host_single_flight().clone().try_lock_owned() else {
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_not_touched",
                "route": "desktop_ax",
                "actionId": action_id,
                "reasonCode": "ax_session_busy",
            }),
        );
    };

    // 只读采样一帧。采样与发送都在阻塞线程里做——AX 调用是同步的。
    let text = text.to_string();
    let action_id_owned = action_id.to_string();
    let thread_id = session.id.clone();
    let workspace = session.project_path.clone();
    let device_id_owned = device_id.to_string();
    let device_name_owned = device_name.to_string();
    let store_for_task = store.clone();
    let devices = st.store.clone();

    let result = tokio::task::spawn_blocking(move || {
        // guard 随闭包进入阻塞线程：只有当本闭包真正返回（AX 操作与落账都结束）
        // 才释放单飞锁。HTTP 断连不会提前释放它。
        let _single_flight = single_flight_guard;
        let device_still_valid = || {
            devices.with(|conn| {
                super::pairing::device_valid(conn, &device_id_owned, chrono::Utc::now().timestamp_millis())
            })
        };
        if !device_still_valid() {
            return Some(nav_refusal("device_revoked"));
        }
        let mut journal = SqliteActionJournal::new(
            store_for_task,
            &device_id_owned,
            &device_name_owned,
            "local",
            &thread_id,
        );
        if journal.reserve_request(&action_id_owned, &text).is_err() {
            let in_flight = journal.exists(&action_id_owned);
            return Some((StatusCode::OK, serde_json::json!({
                "status": if in_flight { "desktop_ax_maybe_submitted" } else { "desktop_ax_not_touched" },
                "route": "desktop_ax",
                "actionId": action_id_owned,
                "reasonCode": if in_flight { "ax_action_already_in_flight" } else { "ax_action_unresolved" },
                "needsMacConfirmation": in_flight,
            })));
        }
        let mut navigation_attempted = false;
        let response = (|| {
        let sample = match crate::services::codex_desktop_ax_sample::sample_sidebar(
            pid,
            host.launch_generation,
            host.login_session,
        ) {
            Some(v) => v,
            None => {
                log::warn!("ax: sample_sidebar returned None (pid={pid})");
                return None;
            }
        };
        let frame = sample.frame_text();

        let report = match crate::services::codex_desktop_window::evaluate_frame_text(
            &frame,
            host.launch_generation,
        ) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("ax: frame parse failed: {e}");
                return None;
            }
        };

        // 注意：本函数**不再**只做阶段一。下方 `report_after_nav` 块会在证明
        // 目标行可定向后做一次 `AXPress`（阶段二）。阶段一仍然独立成立：
        // 目标已是当前会话时零点击直接继续。
        //
        // 阶段二的拒绝**不会**降级成"随便点一行试试"：`prove_target_row` 必须
        // 先给出 `Navigable`，否则零写入返回。

        // **批准目标必须由请求的 thread 自己推出，不能从当前行反取**。
        //
        // 关键区别（这是防串会话的核心）：
        //  - 从"当前行"取 title/group → 等于用被观测事实给自己作证，
        //    桌面停在 A 时会拿 A 的目标去发 B 的消息。
        //  - 从"请求的 thread"取 → 本地索引给出它自己的 title_hash 与 cwd，
        //    再用它们去要求 current 行对位；不对位就是 `OtherThread` → 拒绝。
        let (req_title_hash, req_cwd) = match thread_title_and_cwd(&thread_id) {
            Some(v) => v,
            None => {
                log::warn!("ax: thread_title_and_cwd failed for {thread_id}");
                return None;
            }
        };
        if std::path::Path::new(&workspace) != std::path::Path::new(&req_cwd)
            || !matches!(lookup_thread_by_title(&req_cwd, &req_title_hash),
                Ok(IndexLookup::Unique { id, .. }) if id == thread_id)
        {
            return Some(nav_refusal("ax_target_unverifiable"));
        }
        let Some(project_label_hash) = project_label_from_index(&req_cwd) else {
            return Some(nav_refusal("ax_target_unverifiable"));
        };
        let target_rows: Vec<_> = sample.rows.iter()
            .filter(|row| row.label_hash == req_title_hash
                && row.group_label_hash == project_label_hash && row.readable)
            .collect();
        let target = match target_rows.as_slice() {
            [row] => *row,
            [] => return Some(nav_refusal("ax_target_row_absent")),
            _ => return Some(nav_refusal("ax_target_row_ambiguous")),
        };
        let row_id = &target.node_id_hash;
        let project_group = &target.group_key;
        // ===== 阶段二：先尝试"切到目标"，再重新核证（用户裁决 2026-11-30）=====
        //
        // 1. 找**目标行**（用请求 thread 的标题哈希，不是当前行）
        // 2. 目标已是当前 → 零点击，继续
        // 3. 否则：prove_target_row 证明可定向 → navigate_once 单次 AXPress
        //    → 只读确认 → **重新采帧**
        // 4. 任何失败都零写入返回，如实区分"没切"与"切了但没发"
        let (report_after_nav, post_nav_frame) = {
            use crate::services::codex_desktop_window::{
                navigate_once, prove_target_row, NavigationOutcome, NavigationRequest,
                TargetRowProof,
            };
            // 导航账本：本次动作内防重（同 action_id 不会点第二次）。
            let mut nav_journal =
                crate::services::codex_desktop_window::InMemoryNavigationJournal::default();

            if target.current {
                log::info!("ax nav: already current, zero press");
                // 零点击：帧没变，沿用最初那份。
                (report.clone(), None::<String>)
            } else {
                let Some(approved_for_row) =
                    crate::services::codex_desktop_window::approved_target_for(
                        &host,
                        report.rows.len(),
                        report.counts.rows_enumerated,
                        &thread_id,
                        &workspace,
                        &req_title_hash,
                        &project_label_hash,
                        host.launch_generation,
                    )
                else {
                    log::warn!("ax nav: approved target incomplete");
                    return Some(nav_refusal("ax_target_incomplete"));
                };
                match prove_target_row(&report, &approved_for_row, &target.node_id_hash) {
                    TargetRowProof::AlreadyCurrent => (report.clone(), None::<String>),
                    TargetRowProof::NotNavigable(refusal) => {
                        log::warn!("ax nav: not navigable: {refusal:?}");
                        return Some(nav_refusal("ax_target_row_not_navigable"));
                    }
                    TargetRowProof::Navigable { .. } => {
                        let nav_req = NavigationRequest {
                            action_id: &action_id_owned,
                            approved: &approved_for_row,
                            target_row_id: &target.node_id_hash,
                        };
                        let Some(mut nav_provider) = MacAxProvider::new(pid, host.clone()) else {
                            return Some(nav_refusal("ax_provider_unavailable"));
                        };
                        if !device_still_valid() {
                            return Some(nav_refusal("device_revoked"));
                        }
                        navigation_attempted = true;
                        let nav_report = navigate_once(
                            &mut nav_provider,
                            &mut nav_journal,
                            &nav_req,
                            &report,
                            12,
                        );
                        log::info!(
                            "ax nav: outcome={:?} phase={:?}",
                            nav_report.outcome,
                            nav_report.phase
                        );
                        match nav_report.outcome {
                            NavigationOutcome::NavigatedConfirmed => {
                                // 切后**必须重新采帧**：导航成功只证明视图换了。
                                match resample_frame(pid, &host) {
                                    Some((fresh, text)) => (fresh, Some(text)),
                                    None => {
                                        return Some(nav_refusal("ax_resample_failed_after_nav"))
                                    }
                                }
                            }
                            NavigationOutcome::NotNavigated => {
                                return Some(nav_refusal("ax_navigation_not_performed"))
                            }
                            NavigationOutcome::NavigatedUnconfirmed
                            | NavigationOutcome::NavigationUnknown => {
                                // 已可能按过：不重试、不切回、不当没发生。
                                return Some(nav_refusal("ax_navigation_unconfirmed"));
                            }
                        }
                    }
                }
            }
        };
        let report = report_after_nav;

        // 切后只核对预先固定的行，不能把此时的 current 重新当成授权目标。
        if current_row_id_in(&report).as_deref() != Some(row_id.as_str())
            || current_row_group_in(&report).as_deref() != Some(project_group.as_str())
        {
            return Some(nav_refusal("ax_target_unverifiable"));
        }

        // rollout 路径：用于只读游标（条数 + 末条指纹）。
        let rollout = match rollout_path_of_thread(&thread_id) {
            Some(v) => v,
            None => {
                log::warn!("ax: rollout_path_of_thread failed for {thread_id}");
                return None;
            }
        };

        // **必须传导航后的帧**（2026-11-30 真机缺陷）。
        //
        // 此前这里传的是最初的 `frame`（导航**前**的字符串）。导航成功后
        // `report_after_nav` 是新帧、`row_id`/`project_group` 也来自新帧，
        // 但 `run_ax_send` 会拿 `frame_text` **重新解析一次**并重新核证——
        // 用旧帧 + 新行身份必然对不上，结果是 `ax_target_not_current`：
        // "导航明明成功了，手机却报桌面不在该会话"。
        //
        // 换句话：导航的效果必须体现在**后续核证用的那份帧**里，否则等于没导航。
        let frame = post_nav_frame.unwrap_or(frame);
        let view_generation = host.launch_generation;
        let input = AxSendInput {
            pid,
            host: host.clone(),
            frame_text: &frame,
            view_generation,
            row_id,
            project_group,
            thread_id: &thread_id,
            title_hash: &req_title_hash,
            workspace: &workspace,
            project_label_hash: &project_label_hash,
            rollout_path: rollout,
            action_id: &action_id_owned,
            text: &text,
            device_id: &device_id_owned,
        };
            Some(run_ax_send(&input, &mut journal, &device_still_valid))
        })();
        let (status, mut body) = response.unwrap_or_else(|| nav_refusal("ax_target_unverifiable"));
        body["actionId"] = serde_json::json!(action_id_owned);
        if navigation_attempted {
            body["desktopViewMayHaveChanged"] = serde_json::json!(true);
        }
        let delivery = match body["status"].as_str() {
            Some("desktop_ax_delivered") => return Some((status, body)), // 已由状态机结算。
            Some("desktop_ax_maybe_submitted" | "desktop_ax_touched_not_submitted") => Delivery::UnknownDelivery,
            _ => Delivery::NotSent,
        };
        journal.settle(&action_id_owned, delivery, body["reasonCode"].as_str());
        Some((status, body))
    })
    .await;

    match result {
        Ok(Some((status, body))) => json_no_store(status, body),
        Ok(None) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "status": "desktop_ax_not_touched",
                "error": "desktop_ax_refused",
                "reasonCode": "ax_target_unverifiable",
                "route": "desktop_ax",
                "actionId": action_id,
                // 明确告诉手机：什么都没做，草稿请保留。
                "draftMayBeKept": true,
            }),
        ),
        Err(_) => json_no_store(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "error": "desktop_ax_refused",
                "reasonCode": "ax_internal",
                "route": "desktop_ax",
                "actionId": action_id,
                "draftMayBeKept": true,
            }),
        ),
    }
}

/// 导航阶段失败的统一回执。
///
/// 语义：**零写入**。导航未发生或未确认，所以桌面草稿未被本次动作改动；
/// 但若 `ax_navigation_unconfirmed`，则**可能已经点过一次侧栏行**——
/// 那种情况下不能让手机以为"什么都没发生"，故该码单独区分。
fn nav_refusal(reason: &str) -> (StatusCode, serde_json::Value) {
    // 已可能点过 = 桌面视图可能已经变了，但仍**没有写编辑器**。
    let navigated_maybe = reason == "ax_navigation_unconfirmed";
    let mut body = serde_json::json!({
        "status": "desktop_ax_not_touched",
        "error": "desktop_ax_refused",
        "reasonCode": reason,
        "route": "desktop_ax",
        "draftMayBeKept": true,
    });
    if navigated_maybe {
        // 明确告知：桌面可能已经切走，但没有发送。
        body["desktopViewMayHaveChanged"] = serde_json::Value::Bool(true);
    }
    (StatusCode::OK, body)
}

/// 重新采一帧并解析（导航后复核用）。
///
/// 返回 **`(解析结果, 帧文本)`**：帧文本必须一起交出去，因为下游 `run_ax_send`
/// 会拿它**再解析一次**做独立核证。只回 `ProbeReport` 会让下游拿到最初的旧帧，
/// 于是"导航成功但核证用的是导航前的视图"——这正是 2026-11-30 真机上的
/// `ax_target_not_current`。
fn resample_frame(
    pid: u32,
    host: &crate::services::codex_desktop_window::HostIdentity,
) -> Option<(crate::services::codex_desktop_window::ProbeReport, String)> {
    // 宿主身份也重新取：导航期间 App 可能换代。
    let live = live_host_identity(pid)?;
    let sample = crate::services::codex_desktop_ax_sample::sample_sidebar(
        pid,
        live.launch_generation,
        live.login_session,
    )?;
    let text = sample.frame_text();
    let report =
        crate::services::codex_desktop_window::evaluate_frame_text(&text, host.launch_generation)
            .ok()?;
    Some((report, text))
}

/// 从会话进程 PID 向上解析到所属 `.app` 的**主可执行文件** PID。
///
/// # 为什么需要
///
/// Codex Desktop 的会话进程是 `codex exec-server`（`ChatGPT.app/Contents/Resources/...`），
/// 它的 PID 与主 App（`ChatGPT.app/Contents/MacOS/ChatGPT`）不同。AX 只认后者。
///
/// 判据：沿父链向上，取**命令行为 `<X>.app/Contents/MacOS/<bin>`** 的那个进程。
/// 找不到就返回 `None`（不猜）。
fn resolve_app_pid(start: u32) -> Option<u32> {
    if start == 0 {
        return None;
    }
    let mut cur = start;
    for _ in 0..8 {
        let out = std::process::Command::new("ps")
            .args(["-p", &cur.to_string(), "-o", "ppid=,command="])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let ppid: u32 = parts.next()?.trim().parse().ok()?;
        let cmd = parts.next().unwrap_or("").trim();

        // 命中：`.app/Contents/MacOS/<bin>` 且不带额外子路径。
        if let Some(idx) = cmd.find(".app/Contents/MacOS/") {
            let tail = &cmd[idx + ".app/Contents/MacOS/".len()..];
            if !tail.is_empty() && !tail.contains('/') && !tail.contains(' ') {
                return Some(cur);
            }
        }
        if ppid == 0 || ppid == cur {
            return None;
        }
        cur = ppid;
    }
    None
}

/// 宿主单飞锁：**同一宿主同时只允许一个 AX 写入在飞**。
///
/// # 为什么需要
///
/// AX 通道没有 App 侧的"提交事务"。两次并发发送会在同一个编辑器上交错写入，
/// 最终可能把 A 的文本和 B 的文本拼在一起提交，或者按两次发送键。计划 R2 bullet 2
/// 明写「同一宿主写入单飞」。
///
/// **它是 Tuvis 侧的互斥，不是 App 侧事务**——本锁只能保证"Tuvis 自己不会并发写"，
/// 不能阻止用户在本机同时编辑。这一点在返回给用户的边界说明里必须如实保留。
///
/// 用进程级静态锁而非 `RemoteState` 字段：`RemoteState` 是共享不可变结构，
/// 加字段会波及全部构造点（含测试 fixture）；而"同一宿主"本来就是进程级事实。
fn host_single_flight() -> &'static std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
        std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
}

/// 由 thread 反查它自己的标题哈希与 cwd（只读本地 state 库）。
fn thread_title_and_cwd(thread_id: &str) -> Option<(String, String)> {
    let home = dirs::home_dir()?;
    let state = crate::monitor::codex_thread_parser::CodexThreadRoots::from_codex_root(
        &home.join(".codex"),
    )
    .state_db;
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&state)?;
    let mut stmt = conn
        .prepare("SELECT COALESCE(title,''), COALESCE(cwd,'') FROM threads WHERE id = ?1 LIMIT 1")
        .ok()?;
    let (title, cwd): (String, String) = stmt
        .query_row([thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok()?;
    if title.is_empty() {
        return None;
    }
    Some((
        crate::services::codex_desktop_window::sha256_prefix12(&title),
        cwd,
    ))
}

/// 只有 App 的项目标签可由索引目录名唯一解释时才采用，不接受候选行自报项目身份。
fn project_label_from_index(cwd: &str) -> Option<String> {
    let label = std::path::Path::new(cwd).file_name()?.to_str()?;
    let home = dirs::home_dir()?;
    let state = crate::monitor::codex_thread_parser::CodexThreadRoots::from_codex_root(
        &home.join(".codex"),
    )
    .state_db;
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&state)?;
    let collision: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE archived = 0 AND cwd <> ?1 \
         AND substr(rtrim(cwd, '/'), -(length(?2) + 1)) = '/' || ?2)",
            [cwd, label],
            |row| row.get(0),
        )
        .ok()?;
    (!collision).then(|| crate::services::codex_desktop_window::sha256_prefix12(label))
}

fn current_row_id_in(
    report: &crate::services::codex_desktop_window::ProbeReport,
) -> Option<String> {
    let cur: Vec<_> = report.rows.iter().filter(|r| r.aria_current).collect();
    if cur.len() != 1 || cur[0].node_id.trim().is_empty() {
        return None;
    }
    Some(cur[0].node_id.clone())
}

fn current_row_group_in(
    report: &crate::services::codex_desktop_window::ProbeReport,
) -> Option<String> {
    let cur: Vec<_> = report.rows.iter().filter(|r| r.aria_current).collect();
    if cur.len() != 1 || cur[0].project_group.trim().is_empty() {
        return None;
    }
    Some(cur[0].project_group.clone())
}

/// 从实时进程取宿主身份：PID + 启动时刻（作为启动代际）+ 登录会话。
///
/// **不取自被观测帧**——否则 `trusted()` 的宿主比对恒真，等于自证。
fn live_host_identity(pid: u32) -> Option<HostIdentity> {
    // 启动时刻用 `ps -o lstart=` 换算 epoch 秒；取不到就返回 None（不猜）。
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let generation = launch_epoch_seconds(raw)?;
    if generation == 0 {
        return None;
    }
    // 登录会话：取控制台的 uid（本仓其它 AX 探针同源做法）。
    let login_session = {
        let meta = std::fs::metadata("/dev/console").ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.uid() as u64
        }
        #[cfg(not(unix))]
        {
            0u64
        }
    };
    if login_session == 0 {
        return None;
    }
    Some(HostIdentity {
        pid,
        launch_generation: generation,
        login_session,
    })
}

/// 把 `ps` 的 `lstart` 文本换算成 epoch 秒。解析不出返回 `None`。
fn launch_epoch_seconds(lstart: &str) -> Option<u64> {
    let out = std::process::Command::new("date")
        .args(["-j", "-f", "%a %b %d %T %Y", lstart, "+%s"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// 某 thread 的 rollout 文件路径（只读查本地 state 库）。
fn rollout_path_of_thread(thread_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let state = crate::monitor::codex_thread_parser::CodexThreadRoots::from_codex_root(
        &home.join(".codex"),
    )
    .state_db;
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&state)?;
    let mut stmt = conn
        .prepare("SELECT rollout_path FROM threads WHERE id = ?1 LIMIT 1")
        .ok()?;
    let path: String = stmt.query_row([thread_id], |r| r.get(0)).ok()?;
    if path.trim().is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(path);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_map_to_distinct_wire_codes() {
        let all = [
            AxSendVerdict::Delivered,
            AxSendVerdict::NotTouched,
            AxSendVerdict::TouchedNotSubmitted,
            AxSendVerdict::MaybeSubmitted,
        ];
        let mut codes: Vec<_> = all.iter().map(|v| v.wire_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len(), "四档必须各有独立 wire code");
    }

    #[test]
    fn maybe_submitted_is_never_worded_as_not_sent() {
        // 回归：`MaybeSubmitted` 的 wire code 不得含 "not_touched"/"not_sent" 之类
        // 会让人以为"完全没动作"的字样。
        let c = AxSendVerdict::MaybeSubmitted.wire_code();
        assert!(!c.contains("not_touched"));
        assert!(!c.contains("not_sent"));
        assert!(c.contains("maybe_submitted"));
    }

    #[test]
    fn every_refusal_has_a_stable_code() {
        let all = [
            SendRefusal::TargetIncomplete,
            SendRefusal::BindingNotMatched,
            SendRefusal::WindowGone,
            SendRefusal::ViewGenerationChanged,
            SendRefusal::ObserveFailed,
            SendRefusal::NotIdle,
            SendRefusal::EditorAmbiguous,
            SendRefusal::DraftNotProvablyEmpty,
            SendRefusal::TextNotAdopted,
            SendRefusal::SendControlUnavailable,
            SendRefusal::TargetChangedMidFlight,
            SendRefusal::DesktopEdited,
            SendRefusal::DuplicateAction,
            SendRefusal::DeliveryUnconfirmed,
            SendRefusal::EditorNotConsumed,
            SendRefusal::JournalUnavailable,
        ];
        let mut codes: Vec<_> = all.iter().map(|r| refusal_code(*r)).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len(), "每个拒绝面必须有独立错误码");
        assert!(codes.iter().all(|c| c.starts_with("ax_")));
    }
}
