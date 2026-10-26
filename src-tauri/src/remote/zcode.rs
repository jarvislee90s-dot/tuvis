//! ZCode 官方控制手机 API（生产计划 T7.1 B2）：配对门内的只读投影与写动作。
//!
//! 本切片交付：
//! - `GET /m/api/v1/zcode/connection` → [`ConnectionView`](crate::services::zcode_control::ConnectionView)；
//! - `GET /m/api/v1/zcode/session?sessionId=<原 ID>` →
//!   [`SessionView`](crate::services::zcode_control::SessionView)；
//! - `POST /m/api/v1/zcode/actions` `{actionId, action}` →
//!   [`ActionReceipt`](crate::services::zcode_control::ActionReceipt)；
//! - `GET /m/api/v1/zcode/events?sessionId=<原 ID>` → SSE（首帧连接+会话状态，
//!   随后连接/本会话事件；lag 只发 `resync`；吊销/停服即时断流）。
//!
//! 契约（T7.1 接口冻结节）：
//! - 各路由都注册在既有 `/m/api/v1` 配对 gate 的 nest 内——未配对在 handler 之前
//!   403，绝不触达 Hub／后端；
//! - `Cache-Control: no-store`（门禁下私有状态，与 sessions/official-control 同规）；
//! - GET **不**触发 OAuth、**不**隐式新建 relay；Hub 未装配 → 503 `not_ready`，
//!   绝不伪造「已配对」；
//! - 会话绑定完全由服务端完成：只接受 Tuvis 原始 `sessionId`，先对位当前会话快照
//!   且 `agent_type=zcode`，再交 Hub 精确绑定。手机自报的 `workspacePath` 一律不读
//!   （`TargetKey` 只有 sessionId，workspace 只由宿主 listing 解析）；
//! - 写动作的设备身份**只**来自配对门与本机 DB（[`ActionContext`] 不实现
//!   `Deserialize`）；请求体只允许 `actionId` + `action`，任何设备/路径/能力/
//!   供应商等额外字段一律 400 `invalid_action`，绝不透传给 Hub。

use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::{header::CACHE_CONTROL, HeaderMap, StatusCode},
    response::{sse, IntoResponse, Response, Sse},
    Json,
};
use futures::stream::StreamExt as _;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use super::server::{CleanupStream, RemoteState, SseRegistry};
use crate::services::zcode_control::{
    codes, ActionContext, ControlAction, ControlEvent, ControlEventKind, TargetKey, ZcodeError,
    ZcodeHub,
};
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

/// 当前 Tuvis 会话快照里按原 ID 对位出的 ZCode 会话。
///
/// 只认**快照里活着的** `agent_type=zcode` 会话：未知 ID、非 ZCode、已离开快照、
/// **或已被看板「叉掉」（`board_hidden_ids` 软归档）** 一律 None——手机不能凭自报
/// ID 越权恢复/控制任何会话。快照扫描与看板隐藏集合同源，均按既有惯例进阻塞池。
async fn current_zcode_session(
    st: &Arc<RemoteState>,
    session_id: &str,
) -> Option<crate::session::Session> {
    let scan = st.clone();
    let wanted = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        // 看板隐藏 = 用户已从可见面移除：隐藏/归档的活会话同样不进入控制面。
        if (scan.board_hidden_ids)().iter().any(|id| id == &wanted) {
            return None;
        }
        (scan.session_source)()
            .sessions
            .into_iter()
            .find(|s| s.id == wanted)
    })
    .await
    .ok()
    .flatten()
    .filter(|session| session.agent_type == AgentType::ZCode)
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

    if current_zcode_session(&st, &session_id).await.is_none() {
        return json_no_store(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": codes::NO_SESSION }),
        );
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

// ---------------------------------------------------------------------------
// GET /m/api/v1/zcode/events（T7.1 B3，SSE）
// ---------------------------------------------------------------------------

/// 事件 → SSE 帧的过滤单点（handler 与跨 crate 集成测试共用同一实现）。
///
/// `pub` 仅为 `tests/` 可见（`doc(hidden)`，不向设置/IPC 暴露）：
/// - 连接事件（`sessionId = None`）恒转发——两台手机共享同一条连接的状态；
/// - 会话事件只转发**请求的那个会话**——别的会话的投影不出网，缺 `sessionId`
///   的会话类事件保守丢弃；
/// - 动作事件还要求**发起设备 = 请求设备**（`ControlEvent.device_id` 只留在
///   进程内）：同一会话下别的手机的动作 id 一律不出网；缺设备维度保守丢弃；
/// - 事件体只序列化 Hub 的 Tuvis DTO（`ControlEvent`），绝无 JWT/token/官方原文。
#[doc(hidden)]
pub fn control_frame(
    event: &ControlEvent,
    session_id: &str,
    device_id: &str,
) -> Option<(&'static str, String)> {
    let name = match event.kind {
        ControlEventKind::Connection => "connection",
        ControlEventKind::Session => {
            if event.session_id.as_deref() != Some(session_id) {
                return None;
            }
            "session"
        }
        ControlEventKind::Action => {
            if event.session_id.as_deref() != Some(session_id)
                || event.device_id.as_deref() != Some(device_id)
            {
                return None;
            }
            "action"
        }
    };
    serde_json::to_string(event).ok().map(|data| (name, data))
}

/// GET /m/api/v1/zcode/events?sessionId=<原 ID>：配对手机的控制事件流（SSE）。
///
/// - 首帧 `snapshot` = 当前 [`ConnectionView`](crate::services::zcode_control::ConnectionView)
///   与（可绑定时）当前会话投影（含 `ready`）；随后转发 Hub 的
///   [`ControlEvent`](crate::services::zcode_control::ControlEvent)（连接恒转发，
///   会话只发本会话，动作只发本会话 + 本设备，见 [`control_frame`]）；
/// - 广播落后（lag）只发 `resync` 帧，客户端据此重 GET connection/session，
///   **绝不重放动作**；
/// - 注册进既有 [`SseRegistry`]：设备吊销/停服经 `take_until` 立即断流；
///   客户端断开由 [`CleanupStream`] 反注册；keep-alive 沿用 axum 15s 空注释帧；
/// - 与 GET 同规：不触发 OAuth、不隐式连接；未配对在 gate 层 403（结构化覆盖），
///   Hub 未装配 503 `not_ready`，未知/非 ZCode/已离开快照的会话 404 `no_session`。
pub async fn events(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
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
    // 会话归属授权在订阅之前完成：自报 ID 不能拉起任何不在快照里的会话事件面。
    if current_zcode_session(&st, &session_id).await.is_none() {
        return json_no_store(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": codes::NO_SESSION }),
        );
    }
    let Some(hub) = st.zcode_hub() else {
        return json_no_store(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": codes::NOT_READY }),
        );
    };
    // M4 T0a 同规：注册连接，吊销/停服信号经 take_until 终止流。
    let device = super::gate::extract_device(&headers).unwrap_or_default();
    let (conn_id, close_rx) = st.sse_registry.register(&device);
    let registry = st.sse_registry.clone();
    // 先订阅、后取首帧（顺序刻意，勿换）：订阅与首帧之间产生的跃迁落进广播缓冲，
    // 在首帧之后依次送出；反序会永久漏掉窗口内的更新。
    let rx = hub.subscribe();
    let initial = serde_json::json!({
        "connection": hub.connection(),
        "session": hub
            .session(&TargetKey {
                session_id: session_id.clone(),
            })
            .await
            .ok(),
    });
    let initial_json = serde_json::to_string(&initial).unwrap_or_else(|_| "{}".to_string());
    let first = futures::stream::once(async move {
        Ok::<sse::Event, Infallible>(sse::Event::default().event("snapshot").data(initial_json))
    });
    let wanted = session_id.clone();
    let wanted_device = device.clone();
    let updates = BroadcastStream::new(rx).filter_map(move |msg| {
        let wanted = wanted.clone();
        let wanted_device = wanted_device.clone();
        async move {
            match msg {
                Ok(event) => control_frame(&event, &wanted, &wanted_device)
                    .map(|(name, data)| Ok(sse::Event::default().event(name).data(data))),
                // lag = 中间事件已丢：只报 resync，由客户端重 GET 自愈；不重放动作。
                Err(BroadcastStreamRecvError::Lagged(_)) => Some(Ok(sse::Event::default()
                    .event("resync")
                    .data(r#"{"reason":"lag"}"#))),
            }
        }
    });
    let guarded = first.chain(updates).take_until(close_rx);
    let cleaned = CleanupStream {
        inner: guarded,
        reg: registry,
        device,
        conn_id,
    };
    (
        [(CACHE_CONTROL, "no-store")],
        Sse::new(cleaned).keep_alive(sse::KeepAlive::default()),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /m/api/v1/zcode/actions（T7.1 B2b）
// ---------------------------------------------------------------------------

/// 请求体顶层允许键：只有动作 id 与动作本体。设备/路径/能力/供应商字段
/// （`deviceId`、`deviceName`、`workspacePath`、`capability`、`provider` …）不在其中，
/// 出现即 400 `invalid_action`，绝不透传 Hub。
const REQUEST_KEYS: &[&str] = &["actionId", "action"];

/// 各动作允许的字段集（= [`ControlAction`] 的 wire 形态，唯一许可清单）。
/// 未知 `kind` 与契约外多余字段一律拒绝（手机不能追加任何自报字段）。
const ACTION_KEYS: &[(&str, &[&str])] = &[
    ("send", &["kind", "sessionId", "generation", "text"]),
    (
        "snooze",
        &["kind", "sessionId", "generation", "interactionId"],
    ),
    (
        "answer",
        &[
            "kind",
            "sessionId",
            "generation",
            "interactionId",
            "answers",
        ],
    ),
    (
        "permission",
        &[
            "kind",
            "sessionId",
            "generation",
            "interactionId",
            "optionId",
        ],
    ),
    (
        "stop",
        &[
            "kind",
            "sessionId",
            "generation",
            "expectedForegroundExecutionId",
        ],
    ),
    (
        "selection",
        &["kind", "sessionId", "generation", "model", "thought"],
    ),
];

/// 问答答案条目允许字段（[`AnswerEntry`](crate::services::zcode_control::AnswerEntry)
/// 的 wire 形态）；条目内多余字段同样拒绝。
const ANSWER_KEYS: &[&str] = &["questionText", "selectedValues", "freeText"];

/// POST /m/api/v1/zcode/actions：配对手机提交一次控制动作。
///
/// 安全边界：
/// 1. 请求体只允许 `{actionId, action}`（[`REQUEST_KEYS`]/[`ACTION_KEYS`] 严格白名单）
///    ——设备身份、workspace、能力、供应商等字段在解析层拒绝为 400 `invalid_action`；
/// 2. 与 GET 同级授权门：只接受当前 Tuvis 快照里 `agent_type=zcode` 的会话——隐藏/
///    归档（已离开快照）或非 ZCode 的会话凭自报 ID 一律 404 `no_session`，零 hub 调用，
///    手机不能越过看板可见性边界恢复旧会话；
/// 3. [`ActionContext`] 的设备 id/花名只来自配对 gate 的 cookie 与本机 DB，**绝不**
///    读请求体（ActionContext 本身不实现 Deserialize）；
/// 4. 代际/绑定/能力闸全部由 Hub 判定；本函数只做错误码 → 状态码映射。
pub async fn actions(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Json(raw) = match body {
        Ok(parsed) => parsed,
        // 非 JSON / 形状不符 → invalid_action；超 axum 请求体上限 → payload_too_large。
        Err(rejection) => {
            return action_error(if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                codes::PAYLOAD_TOO_LARGE
            } else {
                codes::INVALID_ACTION
            })
        }
    };
    let Some((action_id, action)) = parse_action_request(raw) else {
        return action_error(codes::INVALID_ACTION);
    };
    // 授权门先于任何 hub 调用（与 GET 同一判据）：隐藏/归档/非 ZCode → 404，零绑定零执行。
    if current_zcode_session(&st, action.session_id())
        .await
        .is_none()
    {
        return action_error(codes::NO_SESSION);
    }
    // 设备身份的可信源：配对 gate 已验过 cookie；这里只取 id + 花名（DB）。
    let Some(device_id) = super::gate::extract_device(&headers) else {
        // gate 已拦设备，理论不可达——兜底与 gate 同形（空体 403，不泄露区分）。
        return StatusCode::FORBIDDEN.into_response();
    };
    let device_name = st
        .store
        .with(|conn| super::pairing::device_name(conn, &device_id));
    let Some(hub) = st.zcode_hub() else {
        return action_error(codes::NOT_READY);
    };
    let ctx = ActionContext {
        action_id,
        device_id,
        device_name,
    };
    match hub.execute(ctx, action).await {
        // accepted/confirmed/noop/unknown 均 200，由回执 `state` 区分（受理 ≠ 已生效）。
        Ok(receipt) => ([(CACHE_CONTROL, "no-store")], Json(receipt)).into_response(),
        Err(error) => action_error(&error.code),
    }
}

/// 严格解析动作请求体：只有冻结形状才交 Hub；任何偏差一律 `None`（→ 400）。
///
/// 白名单在**解析层**生效（先查原始 JSON 的键集，再交给 serde 反序列化），
/// 因此即使 serde 对内部 tag 枚举默认忽略未知字段，请求体也塞不进任何契约外字段。
fn parse_action_request(raw: Value) -> Option<(String, ControlAction)> {
    let request = raw.as_object()?;
    if !request
        .keys()
        .all(|key| REQUEST_KEYS.contains(&key.as_str()))
    {
        return None;
    }
    let action_id = request.get("actionId")?.as_str()?.trim().to_string();
    if action_id.is_empty() {
        return None;
    }
    let action = request.get("action")?.as_object()?;
    let kind = action.get("kind")?.as_str()?;
    let allowed = ACTION_KEYS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, keys)| *keys)?;
    if !action.keys().all(|key| allowed.contains(&key.as_str())) {
        return None;
    }
    if kind == "answer" {
        for answer in action.get("answers")?.as_array()? {
            let entry = answer.as_object()?;
            if !entry.keys().all(|key| ANSWER_KEYS.contains(&key.as_str())) {
                return None;
            }
        }
    }
    let action = serde_json::from_value(Value::Object(action.clone())).ok()?;
    Some((action_id, action))
}

/// 动作错误码 → HTTP 状态（T7.1 冻结映射；只下发码，无上游原文/凭据）。
///
/// - 404：`no_session`（及无法唯一确定目标的 `target_ambiguous`）；
/// - 409：旧代际、未就绪、能力未开闸（`needs_evidence`）、同 ID 载荷冲突、
///   pending/stop 目标已失效；
/// - 400：参数类（非法动作/答案/选项/超限）；
/// - 500：审计/账本持久化失败；
/// - 其余连接态（`needs_config`/`needs_auth`/`offline`/`conflict`/`unknown`/
///   `unsupported_protocol` …）→ 503，客户端按码引导（不自动 OAuth/重连）。
fn action_error(code: &str) -> Response {
    let status = match code {
        codes::NO_SESSION | codes::TARGET_AMBIGUOUS => StatusCode::NOT_FOUND,
        codes::STALE_GENERATION
        | codes::NOT_READY
        | codes::NEEDS_EVIDENCE
        | codes::ACTION_PAYLOAD_CONFLICT
        | codes::STALE_PENDING
        | codes::STOP_TARGET_CHANGED => StatusCode::CONFLICT,
        codes::INVALID_ACTION
        | codes::INVALID_ANSWER
        | codes::FORBIDDEN_OPTION
        | codes::PAYLOAD_TOO_LARGE => StatusCode::BAD_REQUEST,
        codes::STORAGE_FAILED => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    json_no_store(status, serde_json::json!({ "error": code }))
}
