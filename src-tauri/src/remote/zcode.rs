//! ZCode 官方控制手机 API（生产计划 T7.1 B2a）：配对门内的只读投影。
//!
//! 本切片只交付两个 GET：
//! - `GET /m/api/v1/zcode/connection` → [`ConnectionView`](crate::services::zcode_control::ConnectionView)；
//! - `GET /m/api/v1/zcode/session?sessionId=<原 ID>` →
//!   [`SessionView`](crate::services::zcode_control::SessionView)。
//!
//! 契约（T7.1 接口冻结节）：
//! - 两条路由都注册在既有 `/m/api/v1` 配对 gate 的 nest 内——未配对在 handler 之前
//!   403，绝不触达 Hub／后端；
//! - `Cache-Control: no-store`（门禁下私有状态，与 sessions/official-control 同规）；
//! - GET **不**触发 OAuth、**不**隐式新建 relay；Hub 未装配 → 503 `not_ready`，
//!   绝不伪造「已配对」；
//! - 会话绑定完全由服务端完成：只接受 Tuvis 原始 `sessionId`，先对位当前会话快照
//!   且 `agent_type=zcode`，再交 Hub 精确绑定。手机自报的 `workspacePath` 一律不读
//!   （`TargetKey` 只有 sessionId，workspace 只由宿主 listing 解析）。

use axum::{
    extract::{Query, State},
    http::{header::CACHE_CONTROL, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::server::{RemoteState, SseRegistry};
use crate::services::zcode_control::{codes, TargetKey, ZcodeError, ZcodeHub};
use crate::session::AgentType;

/// 槽条目 =（状态身份键：本状态的 `sse_registry` Arc，Hub）。
type ZcodeHubSlot = (Arc<SseRegistry>, Arc<ZcodeHub>);

/// 每状态一个 ZCode Hub 槽（T7.1 B2a）。
///
/// 为什么不落 `RemoteState` 结构体字段：该结构有 30+ 处结构字面量构造点（生产
/// `STATE` + server/queue/各 tests 夹具），本切片要求**既有构造点零改动**（生产
/// 装配在 T7.1C 收口），故槽按状态的 `sse_registry` Arc 身份挂账——每个
/// `RemoteState` 各持一份该 Arc（生产单例、测试各自新建），地址不随状态移动漂移。
/// 条目持一份 Arc 保活：旧状态的键地址不会被释放后复用而把 hub 串给新状态。
/// 缺省无条目 = None（端点 503 `not_ready`）。
static ZCODE_HUB_SLOTS: Lazy<Mutex<Vec<ZcodeHubSlot>>> = Lazy::new(|| Mutex::new(Vec::new()));

impl RemoteState {
    /// 挂接本状态使用的 ZCode 控制 Hub（生产装配在 T7.1C；测试注入内存 fake hub）。
    /// 同状态重复挂接以最后一次为准。
    pub fn set_zcode_hub(&self, hub: Arc<ZcodeHub>) {
        let mut slots = ZCODE_HUB_SLOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.retain(|(registry, _)| !Arc::ptr_eq(registry, &self.sse_registry));
        slots.push((self.sse_registry.clone(), hub));
    }

    /// 本状态的 Hub；未装配 = None（消费方必须 503 `not_ready`，不得伪造已配对）。
    pub fn zcode_hub(&self) -> Option<Arc<ZcodeHub>> {
        let slots = ZCODE_HUB_SLOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots
            .iter()
            .find(|(registry, _)| Arc::ptr_eq(registry, &self.sse_registry))
            .map(|(_, hub)| hub.clone())
    }
}

/// 状态码 + JSON 载荷的统一 no-store 响应（本文件各端点共用；门禁下私有状态
/// 一律 `Cache-Control: no-store`，与 sessions/official-control 同规）。
fn json_no_store(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

/// GET /m/api/v1/zcode/connection：当前连接视图（wire camelCase）。
///
/// Hub 未装配（生产接线前 / 本机未启用）→ 503 `{error:"not_ready"}`：如实申报，
/// **绝不**伪造 `paired`。GET 只读，不触发 OAuth、不隐式新建连接。
pub async fn connection(State(st): State<Arc<RemoteState>>) -> Response {
    let Some(hub) = st.zcode_hub() else {
        return json_no_store(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": codes::NOT_READY }),
        );
    };
    // ConnectionView 字段全部为本地枚举/整数/字符串，序列化不可达失败；直接 Json。
    ([(CACHE_CONTROL, "no-store")], Json(hub.connection())).into_response()
}

/// GET /m/api/v1/zcode/session?sessionId=<原 ID>：当前 Tuvis ZCode 会话投影。
///
/// 绑定权限全在服务端：
/// 1. 只读参数 `sessionId`（原 ID 原样，不做去横杠/标题/路径推断）；
/// 2. 对位**当前** Tuvis 会话快照（与看板同源）且 `agent_type=zcode`——未知 ID、
///    非 ZCode、已离开快照一律 404；手机不能凭自报 ID 恢复旧会话；
/// 3. 再交 Hub 在宿主 listing 内精确绑定。`workspacePath` 等自报字段一律忽略
///    （`TargetKey` 只有 sessionId）。
pub async fn session(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = params
        .get("sessionId")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return json_no_store(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "bad_request" }),
        );
    };

    // 快照扫描是同步阻塞活（与 /sessions 同源），按既有惯例进阻塞池。
    let scan = st.clone();
    let wanted = session_id.clone();
    let current = tokio::task::spawn_blocking(move || {
        (scan.session_source)()
            .sessions
            .into_iter()
            .find(|s| s.id == wanted)
    })
    .await
    .ok()
    .flatten();
    match current {
        Some(session) if session.agent_type == AgentType::ZCode => {}
        _ => {
            return json_no_store(
                StatusCode::NOT_FOUND,
                serde_json::json!({ "error": codes::NO_SESSION }),
            )
        }
    }

    let Some(hub) = st.zcode_hub() else {
        return json_no_store(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": codes::NOT_READY }),
        );
    };
    let key = TargetKey { session_id };
    match hub.session(&key).await {
        Ok(view) => ([(CACHE_CONTROL, "no-store")], Json(view)).into_response(),
        Err(error) => session_error(error),
    }
}

/// Hub 错误 → HTTP 状态（只下发冻结码；无上游原文）。未就绪/未授权/离线和未知
/// 一律 503，客户端按 error 码引导用户（GET 不自动 OAuth 也不自动连接）。
fn session_error(error: ZcodeError) -> Response {
    let status = match error.code.as_str() {
        codes::NO_SESSION | codes::TARGET_AMBIGUOUS => StatusCode::NOT_FOUND,
        codes::STALE_GENERATION => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    json_no_store(status, serde_json::json!({ "error": error.code }))
}
