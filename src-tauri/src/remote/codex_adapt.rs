//! T08：**移动端适配详情与开关**（F08）。
//!
//! 端点（复用 `/m/api/v1` 的鉴权/CSRF/Origin/限流/设备写权限防线；不建第二个
//! 无认证 router）：
//! * `GET  /codex/adapt-status?sessionId=…` → [`CodexAdaptDetails`]（只读）；
//! * `POST /codex/adapt-settings {sessionId, action, enabled?}` → 更新后的详情。
//!
//! 纪律（spec §3.2／§11）：
//! * 只接**已授权的本机 Desktop 会话**（`agent_type=codex` 且 `form=App`）；
//! * 详情**不携包路径/原始源码**；设置只操作 `remote.codex_auto_adapt` 与
//!   `remote.codex_auto_adapt_hint_seen` 两个键（其他键/组合一律拒绝）；
//! * 与桌面设置**同源**（同一共用设置服务 + 同一适配器通知）：移动端关闭/重开与
//!   桌面一样触发 notify、按需只读取证与投影重发布；只改库不通知不算实现；
//! * GET/刷新**只读**：没有隐藏的答题/审批发送；前台预算由客户端传剩余值。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::session::ProcessForm;

use super::api::json_no_store;
use super::codex_ipc::{bad_request, find_session_sync, resolve_session_thread};
use super::server::RemoteState;

/// `GET /codex/adapt-status` 查询参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdaptStatusQuery {
    pub session_id: String,
}

/// `POST /codex/adapt-settings` 请求体（严格白名单）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexAdaptChange {
    pub session_id: String,
    /// `set_enabled` | `dismiss_hint`
    pub action: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn desktop_only(session: &crate::session::Session) -> bool {
    session.agent_type.tool_id() == "codex" && matches!(session.form, ProcessForm::App)
}

/// 只读 GET /codex/adapt-status 的会话定位：读显示快照，不触发补扫。
async fn load_session(st: &Arc<RemoteState>, session_id: &str) -> Option<crate::session::Session> {
    let probe_st = st.clone();
    let probe_sid = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        super::api::unique_file_session((probe_st.session_source)().sessions, &probe_sid, None)
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten()
}

/// 动作 POST /codex/adapt-settings 的会话定位：写设置前取新鲜快照（Task 4 F3）。
async fn load_session_fresh(
    st: &Arc<RemoteState>,
    session_id: &str,
) -> Option<crate::session::Session> {
    let probe_st = st.clone();
    let probe_sid = session_id.to_string();
    tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid))
        .await
        .ok()
        .flatten()
}

/// 组装详情（只读；不触发包扫描——能力事实来自已装载的契约证据）。
async fn build_details(
    st: &Arc<RemoteState>,
    session: &crate::session::Session,
    key: Option<&crate::services::codex_desktop_ipc::types::ThreadKey>,
    device_id: Option<&str>,
) -> crate::services::codex_desktop_ipc::adapt_status::CodexAdaptDetails {
    use crate::services::codex_desktop_ipc::adapt_status::CodexAdaptDetails;
    let adapter = &st.codex_control.adapter;
    // 走**本 hub 的库**（与动作账本/适配器同源；测试注入的 store 与生产同路径）。
    let (auto_adapt_enabled, hint_seen) = st
        .codex_control
        .router
        .store()
        .with_settings(|conn| {
            (
                crate::remote::auto_adapt_setting_from(
                    crate::database::dao::settings::get_setting_conn(
                        conn,
                        crate::remote::KEY_AUTO_ADAPT,
                    )
                    .as_deref(),
                )
                .unwrap_or(true),
                crate::remote::codex_auto_adapt_hint_seen_conn(conn),
            )
        })
        .unwrap_or((true, false));
    let status = adapter.adapt_status_cached(None).await;
    let result_unknown = key.zip(device_id).and_then(|(key, device_id)| {
        st.store.with(|conn| conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_actions WHERE thread_id = ?1 AND device_id = ?2 AND state = 'unknown')",
            rusqlite::params![key.thread_id, device_id], |row| row.get(0),
        )).ok()
    });
    CodexAdaptDetails {
        session_id: session.id.clone(),
        system_version: system_version(),
        tuvis_version: env!("CARGO_PKG_VERSION").to_string(),
        desktop_build: status.desktop_build.clone(),
        auto_adapt_enabled,
        hint_seen,
        effective_operations: adapter.diagnostic_operations(key),
        desktop_observation: adapter.desktop_observation(key).await,
        result_unknown,
        last_failure: status.last_failure.clone(),
    }
}

/// 系统版本（OS + 版本；不含机器名/用户名）。
fn system_version() -> String {
    format!(
        "{} {}",
        std::env::consts::OS,
        sysinfo::System::long_os_version().unwrap_or_else(|| "unknown".into())
    )
}

/// `GET /m/api/v1/codex/adapt-status?sessionId=…`
pub async fn codex_adapt_status(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(query): Query<AdaptStatusQuery>,
) -> Response {
    let Some((device_id, _)) = super::codex_ipc::device_identity(st.as_ref(), &headers) else {
        return super::codex_ipc::forbidden_defense();
    };
    let session_id = query.session_id.trim().to_string();
    if session_id.is_empty() {
        return bad_request();
    }
    let Some(session) = load_session(&st, &session_id).await else {
        return super::codex_ipc::not_found();
    };
    if !desktop_only(&session) {
        return super::codex_ipc::not_found();
    }
    // 展示路径不改 session 映射；UUID 只是已装载 IPC 镜像的查询键，不是 AX 对位证据。
    let key = super::codex_ipc::diagnostic_thread_key(&session);
    let details = build_details(&st, &session, key.as_ref(), Some(&device_id)).await;
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&details).unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// `POST /m/api/v1/codex/adapt-settings`
pub async fn codex_adapt_settings(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<CodexAdaptChange>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(change) = match body {
        Ok(value) => value,
        Err(_) => return bad_request(),
    };
    let Some((device_id, _)) = super::codex_ipc::device_identity(st.as_ref(), &headers) else {
        return super::codex_ipc::forbidden_defense();
    };
    let session_id = change.session_id.trim().to_string();
    if session_id.is_empty() {
        return bad_request();
    }
    // 严格组合校验：set_enabled 必须带 enabled；dismiss_hint 不得带 enabled。
    let (apply_toggle, dismiss_hint) = match (change.action.as_str(), change.enabled) {
        ("set_enabled", Some(enabled)) => (Some(enabled), false),
        ("dismiss_hint", None) => (None, true),
        ("set_enabled", None) | ("dismiss_hint", Some(_)) => return bad_request(),
        _ => return bad_request(),
    };
    let Some(session) = load_session_fresh(&st, &session_id).await else {
        return super::codex_ipc::not_found();
    };
    if !desktop_only(&session) {
        return super::codex_ipc::not_found();
    }
    let Some(key) = resolve_session_thread(&st, &session) else {
        return super::codex_ipc::not_found();
    };
    let written = st
        .codex_control
        .router
        .store()
        .with_settings(|conn| {
            if let Some(enabled) = apply_toggle {
                crate::remote::apply_codex_auto_adapt_toggle(conn, enabled);
            } else if dismiss_hint {
                crate::remote::dismiss_codex_auto_adapt_hint_conn(conn);
            }
        })
        .is_ok();
    if !written {
        return json_no_store(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "internal" }),
        );
    }
    let details = build_details(&st, &session, Some(&key), Some(&device_id)).await;
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&details).unwrap_or_else(|_| serde_json::json!({})),
    )
}
