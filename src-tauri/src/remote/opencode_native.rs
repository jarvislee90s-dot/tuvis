//! OpenCode 原生交互的移动端路由（计划任务 8）：薄 handler + 受控 DTO + 旧路径屏障回执。
//!
//! 纪律：
//! * 本模块只做**请求验证、鉴权/设备上下文与 DTO**——不拼原生帧、不建 HTTP client、
//!   不写 DB 业务逻辑；原生读写全经 [`NativeHub`]（services 层），账本经 DAO。
//! * 会话**只**由服务器 `session_source` 缝解析（数据同源铁律）；[`NativeActor`] 只由
//!   既有配对身份 + 签名设置生成，客户端不能指定（也自带不了 endpoint/method/权限）。
//! * 生产门禁：仅 macOS、会话可唯一绑定且契约可读时才进入 Native。策略缺省
//!   `native_only`；两种策略都允许原生，旧验证键不再控制准入。
//!   其他平台一律 [`OC_PLATFORM_UNVERIFIED`]；`--standalone` 等未登记形态发现层
//!   直接失败（`OC_SERVICE_NOT_REGISTERED`），绝不假接入。
//! * 旧路径防双投屏障的**判定**在服务层（`NativeHub::try_legacy_permit` /
//!   `select_native_route` + DAO `owns_session_conn`），本模块只提供对外受控码与
//!   统一回执（[`route_changed_response`]）——守卫不靠「前端没显示旧按钮」。
//!   flush 侧不消费本模块的任何码：它取不到同步许可（切路由进行中）或读到原生归属时
//!   一律返回 `FlushOutcome::Deferred`（本轮不投递/不消费/不落账），由下个 tick 重试；
//!   `OC_ROUTE_CHANGED` 只出现在移动端旧写入口的 HTTP 回执里。
//!
//! ## 路由归属是**可逆**的（评审 I-1/I-2）
//!
//! **认领发生在读请求上**：`GET /opencode/view` 在策略允许原生、且投影成功（服务代际 +
//! 会话三重核对 + 契约可读）时会当场把该会话登记为 `active`——即「打开原生视图就是选择
//! 原生路由」。这是既有进入口；专测分支原生为默认策略，
//! 认领前必须有一次**成功**的权威投影（拿不到当前事实就不写归属）。
//!
//! **退回旧路径同样显式**：仅兼容策略允许 `POST /opencode/route/legacy` 把归属降级为 `draining`，此后
//! 旧写入口不再 409。存在结果未明的原生动作时拒绝交回（409）——结果不明时不允许用
//! 「切回旧路径」绕过只读核对，也不许把未决动作迁到旧 FIFO。
//!
//! 归属行**从不删除**，动作账本与旧 `inject_queue` 行**从不迁移**。

use axum::{
    extract::{Json, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{sse, IntoResponse, Response},
    routing::{get, post},
};
use futures::stream::StreamExt as _;
use serde::Serialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use crate::database::dao::opencode_creates::{self, CreateRecord, CreateWindow};
use crate::database::dao::opencode_reselect::{self, Receipt as ReselectReceipt};
use crate::remote::api::{
    bad_request, bad_request_reason_detail, device_identity, find_session_sync, forbidden_defense,
    json_no_store,
};
use crate::remote::server::RemoteState;
use crate::services::opencode_native::action_codes;
use crate::services::opencode_native::codes;
use crate::services::opencode_native::types::{
    ActionView, NativeActor, NativeCommand, NativeError, PublicNativeView,
};
use crate::session::Session;

/// 旧写入口被原生归属挡下（409）：该会话已切 Native，旧 FIFO 不得再入队/投递。
/// 对外受控码的唯一出处；只有 `api.rs` 的旧写入口（send / queue-jump / approve /
/// question-answer / mode-switch）与「交回旧路径」的拒绝臂消费它——`inject/queue.rs`
/// **不引用**本码（flush 让位走 `FlushOutcome::Deferred`）。
pub const OC_ROUTE_CHANGED: &str = "OC_ROUTE_CHANGED";
/// 同上，用户可读文案（固定句，不含任何会话/服务细节）。
pub const OC_ROUTE_CHANGED_REASON: &str = "该会话已切换至 OpenCode 原生交互，请改用原生入口操作";
/// 「重新进入原生」被拒（409，复用路由码 [`OC_ROUTE_CHANGED`]）：旧 `inject_queue` 仍有未决行。
/// 先由用户处理旧项（撤回或等待投递），再重新进入——不迁移、不重投。
pub const OC_ROUTE_HELD_LEGACY_REASON: &str =
    "该会话还有未处理的旧队列消息，请先处理后再重新进入原生交互";
/// 「重新进入原生」被拒的第二族真因（patch3 O1，H13 证据 §10-O1）：换代期该会话仍有
/// **结果未明的原生动作**（不是旧队列消息）。H13 实测触发全部是这一族，旧共用文案
/// 「还有未处理的旧队列消息」与真因错位、误导排查方向——按触发族分流文案。
/// 指路同时覆盖两条出口：只读核对收敛，或对死代际动作显式「确认终态未知」。
pub const OC_ROUTE_HELD_NATIVE_ACTION_REASON: &str =
    "该会话仍有结果未确认的原生动作：请先只读核对收敛，或对已不可核对的动作显式确认终态未知，再重新进入原生交互";
/// 「交回旧路径」被拒（409，复用路由码 [`OC_ROUTE_CHANGED`]）：仍有结果未明的原生动作。
/// 不复用上句文案——拒绝的理由是「还有未决动作」，不是「该走原生入口」。
pub const OC_ROUTE_HELD_REASON: &str =
    "该会话仍有结果未明的原生动作，请先完成只读核对后再交回旧路径";
/// 平台未验证（409）：本批候选路由只在 macOS 启用；其他平台原流程不变。
pub const OC_PLATFORM_UNVERIFIED: &str = "OC_PLATFORM_UNVERIFIED";
/// 同上，用户可读文案。
pub const OC_PLATFORM_UNVERIFIED_REASON: &str = "OpenCode 原生交互首批仅在 macOS 启用";
/// 旧写入口 `Err` 串里的路由码（端点统一错误壳据此映射到 [`route_changed_response`]）。
pub const ROUTE_CHANGED_ERR: &str = "oc_route_changed";
/// send-info 的原生路由说明（`interactionRouteReason`，固定句）。
pub const NATIVE_ROUTE_REASON: &str =
    "该会话已切换至 OpenCode 原生交互，消息、审批与问答走原生通道";
/// 验证开关键（settings KV）：缺省 false；只认显式 `"true"`（与兼容发送开关同口径）。
pub const SWITCH_KEY: &str = "remote.opencode_native_enabled";
/// 路由词（DTO）：已归原生 / 保持旧流程。
pub const ROUTE_NATIVE: &str = "opencode_native";
pub const ROUTE_LEGACY: &str = "legacy";
/// 表单结果不存在 / 属其他会话 / 读不到（R5b）：统一 404，**不泄露存在性**。
/// 前端只看「没读到」并给出「结果未读到」，不据此推断表单是否存在。
pub const FORM_RESULT_NOT_FOUND: &str = "form_not_found";
/// 同上，固定文案（不回传原生响应体、不区分不存在/他会话/读失败）。
pub const FORM_RESULT_NOT_FOUND_REASON: &str = "表单不存在或结果不可读";

/// GET /m/api/v1/opencode/view 响应体：权威只读投影 + 路由归属。
///
/// `route=opencode_native` = 该会话已归原生（旧写入口会 409）；`route=legacy` =
/// 未进入原生（缺省/开关关闭/旧 pending 未决/**归属未证且无既有归属**），前端保持旧控件。
/// `routeReasonCode` 在「尝试认领被拒」或「同实例归属未证（B1，写被安全门禁用）」时给
/// 受控原因词；**它不等于未归原生**：已有 `active` 归属的会话也可能带该码。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeRouteView {
    #[serde(flatten)]
    pub view: PublicNativeView,
    pub route: &'static str,
    pub route_reason_code: Option<&'static str>,
    pub route_version: Option<i64>,
}

/// 五路由的公共前置：设备身份（403 防御）→ 会话解析（**只**经 `session_source` 缝）
/// → OpenCode 校验（400）→ 平台门（409 OC_PLATFORM_UNVERIFIED）。
struct NativeContext {
    device_id: String,
    device_name: String,
    session: Session,
    expected_target: Option<crate::services::opencode_native::types::NativeTarget>,
    expected_requires_active: bool,
}

#[allow(clippy::result_large_err)] // axum Response 错误变体；受控错误壳统一走 native_error_response
fn check_expected_binding(
    st: &RemoteState,
    ctx: &mut NativeContext,
    token: Option<&str>,
    required: bool,
    allow_draining: bool,
) -> Result<(), Response> {
    if token.is_none() && !required {
        return Ok(());
    }
    if ctx.expected_target.is_none() {
        ctx.expected_target = token.and_then(|token| {
            st.opencode_native
                .cached_target_for_binding(&ctx.session.id, token)
        });
    }
    let matches = token
        .zip(ctx.expected_target.as_ref())
        .is_some_and(|(token, target)| st.opencode_native.binding_matches(target, token));
    if !matches {
        return Err(native_error_response(&NativeError::new(
            codes::OC_BINDING_STALE,
            "the selected native binding changed",
        )));
    }
    let route_matches =
        st.store
            .with(|conn| {
                crate::database::dao::opencode_actions::route_conn(conn, &ctx.session.id).map(
                    |route| match route {
                        Some(route) => ctx
                            .expected_target
                            .as_ref()
                            .is_some_and(|target| route.same_runtime(target))
                            && (route.route_state
                                == crate::database::dao::opencode_actions::ROUTE_ACTIVE
                                && ctx.expected_requires_active
                                || (allow_draining
                                    && route.route_state
                                        == crate::database::dao::opencode_actions::ROUTE_DRAINING)),
                        None => !ctx.expected_requires_active,
                    },
                )
            })
            .unwrap_or(false);
    if route_matches {
        Ok(())
    } else {
        Err(native_error_response(&NativeError::new(
            codes::OC_BINDING_STALE,
            "the selected native route changed",
        )))
    }
}

#[allow(clippy::result_large_err)]
fn expected_binding_header(headers: &HeaderMap) -> Result<Option<&str>, Response> {
    headers
        .get("x-opencode-expected-binding")
        .map(|value| value.to_str().map_err(|_| bad_request()))
        .transpose()
}

async fn resolve_native_context(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    sid: &str,
) -> Result<NativeContext, Response> {
    resolve_native_context_with(st, headers, sid, native_platform_supported()).await
}

/// [`resolve_native_context`] 的可测缝：平台判据由调用方给（生产恒传
/// [`native_platform_supported`]）。
///
/// 为什么需要缝：判据是 `cfg!(target_os = "macos")`，在 macOS 主机上恒真，
/// `OC_PLATFORM_UNVERIFIED` 分支**没有任何办法被执行**——不是「没测」，而是
/// 「测不了」。把判据变成参数后，集成测试可以走同一个函数体验证该分支的受控码
/// 与「零原生请求」性质；真实非 macOS 主机的运行行为仍需在那台机器上验（宿主事实，
/// 不是逻辑缺口）。
async fn resolve_native_context_with(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    sid: &str,
    platform_supported: bool,
) -> Result<NativeContext, Response> {
    resolve_native_context_mode(st, headers, sid, platform_supported, false, None).await
}

async fn resolve_native_context_mode(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    sid: &str,
    platform_supported: bool,
    allow_restored: bool,
    binding_hint: Option<&str>,
) -> Result<NativeContext, Response> {
    let Some((device_id, device_name)) = device_identity(st.as_ref(), headers) else {
        return Err(forbidden_defense());
    };
    let probe_st = st.clone();
    let probe_sid = sid.to_string();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("opencode native 会话扫描任务异常: {e}");
                return Err(internal());
            }
        };
    // 已选择的 active 目标优先；同 ID 的新扫描卡不能暗中更换目录或服务。
    let restored = if allow_restored {
        st.opencode_native
            .restore_active_target(sid)
            .map_err(|error| native_error_response(&error))?
    } else {
        None
    };
    let cached = if allow_restored && restored.is_none() {
        binding_hint.and_then(|token| st.opencode_native.cached_context_for_binding(sid, token))
    } else {
        None
    };
    let (session, expected_target, expected_requires_active) = match (session, restored, cached) {
        (_, Some((session, target)), _) => (session, Some(target), true),
        (_, None, Some((session, target))) => (session, Some(target), false),
        (Some(session), None, None) => (session, None, false),
        (None, None, None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                "no_session",
                "no_session",
                "会话不存在",
            ))
        }
    };
    // 非 OpenCode 会话不假接入：原生接口只服务 OpenCode 默认后台 service。
    if session.agent_type.tool_id() != "opencode" {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "not_opencode",
            "not_opencode",
            "该接口只服务 OpenCode 会话",
        ));
    }
    // 平台门（本批硬边界）：非 macOS 原流程不变，不发现、不绑定、不改写任何状态。
    if !platform_supported {
        return Err(json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        ));
    }
    Ok(NativeContext {
        device_id,
        device_name,
        session,
        expected_target,
        expected_requires_active,
    })
}

/// 平台判据（编译期事实）：本批候选路由只在 macOS 启用。
pub fn native_platform_supported() -> bool {
    cfg!(target_os = "macos")
}

/// 两种交互策略均允许原生；旧验证键仅是历史配置，不再作为第二道总开关。
pub fn native_enabled(_st: &RemoteState) -> bool {
    true
}

/// `session-send-info` 的**候选**判定（评审 Blocker 1）：平台 + 验证开关 + 该会话此刻
/// 可唯一绑定（与 `GET /opencode/view` 的投影前半段同源）。
///
/// 纪律：**只读**。本函数不认领、不写路由行、不发任何写请求；它只回答「现在打开视图
/// 大概率能进原生」。任一条件不成立即 `false`（候选判定永远不是错误响应）。
pub async fn is_native_candidate(st: &Arc<RemoteState>, session: &Session) -> bool {
    if !native_platform_supported() || !native_enabled(st) {
        return false;
    }
    st.opencode_native.can_bind(session).await
}

/// [`native_error_response`] 的状态码映射（单点）：携带附加字段的错误体复用同一映射。
fn native_error_status(err: &NativeError) -> StatusCode {
    match err.code.as_str() {
        // 客户端原因（命令形态/令牌/答案/附件/会话不符）：参数就不对。
        codes::OC_ACTION_INVALID
        | codes::OC_COMMAND_UNSUPPORTED
        | codes::OC_SEND_EMPTY
        | codes::OC_SEND_TOO_LONG
        | codes::OC_ATTACHMENT_REFUSED
        | codes::OC_PATH_INVALID
        | codes::OC_SESSION_MISMATCH
        | codes::OC_ANSWER_INCOMPLETE
        | codes::OC_ANSWER_INVALID
        | codes::OC_ANSWER_UNSUPPORTED
        | codes::OC_FORM_INVALID => StatusCode::BAD_REQUEST,
        // 别人的动作与不存在的动作同码（不做存在性预言机）。
        action_codes::NOT_FOUND => StatusCode::NOT_FOUND,
        // 其余（绑定陈旧 / 已定案 / 冲突 / 不归属 / 服务与传输类）：受控拒绝——
        // 要么零写出，要么结果未知（Unconfirmed 走 200 + ActionView，不落本臂）。
        _ => StatusCode::CONFLICT,
    }
}

/// 统一受控错误回执（NativeError 的 code/message 是受控词与固定文案，零凭据）。
fn native_error_response(err: &NativeError) -> Response {
    json_error(native_error_status(err), &err.code, &err.code, &err.message)
}

/// 结构化错误体（与既有端点同形：error + reasonCode + reason）。
fn json_error(status: StatusCode, error: &str, reason_code: &str, reason: &str) -> Response {
    json_no_store(
        status,
        serde_json::json!({ "error": error, "reasonCode": reason_code, "reason": reason }),
    )
}

fn internal() -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal",
        "内部错误",
    )
}

/// 旧写入口被原生归属挡下时的统一回执（409 OC_ROUTE_CHANGED）——api.rs 六处与
/// inject/queue.rs 的屏障断言共用同一形状。
pub fn route_changed_response() -> Response {
    json_error(
        StatusCode::CONFLICT,
        ROUTE_CHANGED_ERR,
        OC_ROUTE_CHANGED,
        OC_ROUTE_CHANGED_REASON,
    )
}

/// 注册候选路由（与既有端点同一 `api_router`、同一配对门内层 gate——结构性覆盖，
/// 不需要各自鉴权代码）。
/// J5（重选链总耗时）：handler 级 Drop 计时守卫——任意返回路径都记录总耗时
/// （单行结构化日志；分段拆分需 projection 重构，属后续批申报）。
struct J5TimingGuard(&'static str, std::time::Instant);
impl Drop for J5TimingGuard {
    fn drop(&mut self) {
        eprintln!(
            "j5_reselect_timing stage={} total_ms={}",
            self.0,
            self.1.elapsed().as_millis()
        );
    }
}

pub fn routes() -> axum::Router<Arc<RemoteState>> {
    axum::Router::new()
        .route("/opencode/policy", get(native_policy))
        .route("/opencode/policy/events", get(native_policy_events))
        .route("/opencode/view", get(native_view))
        .route("/opencode/agents", get(native_agents))
        .route("/opencode/outbox", get(native_outbox))
        .route("/opencode/legacy-outbox", get(legacy_outbox))
        .route("/opencode/events", get(native_events))
        .route("/opencode/actions", post(native_action_execute))
        // J2（无本地锚发现）：属主设备×会话的需核对动作清单（绑定态无关；非属主
        // =空数组不泄露存在性）。移动端与服务端清单按 action ID 去重合并本地锚。
        .route("/opencode/actions", get(native_action_list))
        // J6（原生新建）：目录核对 + 服务唯一 + 代理创建 + resume 起窗
        .route(
            "/opencode/create-native",
            get(native_create_status).post(native_create_session),
        )
        .route(
            "/opencode/create-native/recover",
            post(native_create_recover),
        )
        .route(
            "/opencode/create-native/preview",
            get(native_create_preview),
        )
        .route("/opencode/actions/{action_id}", get(native_action_read))
        .route(
            "/opencode/actions/{action_id}/confirm-unknown",
            post(native_action_confirm_unknown),
        )
        // 表单结果回读（R5b）：按原 session + formId 只读回读官方状态/答案。
        .route("/opencode/forms/{form_id}", get(native_form_result))
        .route(
            "/opencode/actions/{action_id}/verify",
            post(native_action_verify),
        )
        // 显式交回旧路径（评审 I-1）：让「开关关掉」不再是单向门。
        .route("/opencode/route/legacy", post(native_route_legacy))
        // 显式**重新进入**原生（评审 R-2）：退回之后必须由用户再点一次，不能被一次刷新/
        // 换端/第二台设备的 GET /opencode/view 静默撤销。
        .route("/opencode/route/native", post(native_route_native))
        .route("/opencode/route/native/status", get(native_reselect_status))
        // 候选预览（只读）：确认前固定用户看到的目标；候选变化时确认拒绝。
        .route("/opencode/route/preview", get(native_route_preview))
        // 只读路由核对（丢回执恢复）：纯数据库读，零认领零安装零写入。
        .route("/opencode/route/check", get(native_route_check))
}

/// GET /opencode/route/check?session_id=（**只读路由核对**，丢回执恢复用）。
///
/// 携带 `X-OpenCode-Expected-Binding` 时按绑定核对（restored/cached 恢复路径，
/// 允许 draining）——与 view 的认领入口不同，本端点**不认领、不发现、不安装、
/// 不写任何状态**，只读当前路由行摘要，供「确认 POST 结果未知」的客户端安全核对。
pub async fn native_route_check(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let token = match expected_binding_header(&headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        &sid,
        native_platform_supported(),
        true,
        token,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    if let Err(response) = check_expected_binding(&st, &mut ctx, token, true, true) {
        return response;
    }
    let route = match st
        .store
        .with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &sid))
    {
        Ok(route) => route,
        Err(_) => {
            return native_error_response(&NativeError::new(
                codes::OC_RESPONSE_INVALID,
                "native route unavailable",
            ))
        }
    };
    let Some(route) = route else {
        return json_no_store(
            StatusCode::OK,
            serde_json::json!({ "sessionId": sid, "routeState": "none" }),
        );
    };
    let binding_token = route
        .to_target(&sid)
        .map(|target| st.opencode_native.binding_token_for(&target));
    json_no_store(
        StatusCode::OK,
        serde_json::json!({
            "sessionId": sid,
            "routeState": route.route_state,
            "bindingToken": binding_token,
            "routeVersion": route.route_version,
        }),
    )
}

/// `POST /opencode/route/legacy` 请求体：会话 id + 可选显式 `force`（客户端不决定目标服务/端点）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteLegacyReq {
    pub session_id: String,
    pub binding_token: String,
    pub route_version: i64,
    #[serde(default)]
    pub confirmed_action_ids: Option<Vec<String>>,
    /// **显式运维决定**（评审 Blocker 2，缺省 false）：作废「代际已不可核对」的动作后再
    /// 交回。未知字段仍然拒绝；不传 = 老行为（有未决动作就 409，零变更）。
    #[serde(default)]
    pub force: bool,
}

/// POST /m/api/v1/opencode/route/legacy（**显式交回旧路径**，评审 I-1）。
///
/// 契约：
/// - 与其余五条路由同一配对门（cookie/设备上下文缺失 → 403）、同一会话解析（404/400）
///   与同一平台门（非 macOS → 409 [`OC_PLATFORM_UNVERIFIED`]）；请求体只认 `sessionId`
///   （未知字段 → 400）；
/// - 服务层 `NativeHub::disengage_route`：持同会话 write permit，把 `active` 降级为
///   `draining`；**有结果未明的原生动作就拒绝**（复用 DAO 既有判断，不另立规则），
///   拒绝时零变更；
/// - **幂等**：从未接入 Native 的会话（无路由行）同样返回成功——「现在归旧路径」这个
///   事实成立即为成功，不区分「刚降级」与「本来就没接」；
/// - 不删除任何动作行、不迁移旧 `inject_queue` 行；既有动作此后仍可只读核对。
///
/// 成功 → `200 { sessionId, route: "legacy" }`；拒绝 → `409 OC_ROUTE_CHANGED`
/// （文案用 [`OC_ROUTE_HELD_REASON`]，与「旧入口被挡」的 409 同码不同因）。
pub async fn native_route_legacy(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<RouteLegacyReq>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let sid = req.session_id.trim().to_string();
    if sid.is_empty() {
        return bad_request();
    }
    if req.binding_token.is_empty() || req.route_version < 1 {
        return bad_request();
    }
    let Some(returned_version) = req.route_version.checked_add(1) else {
        return bad_request();
    };
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        &sid,
        native_platform_supported(),
        true,
        Some(&req.binding_token),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) =
        check_expected_binding(&st, &mut ctx, Some(&req.binding_token), true, true)
    {
        return response;
    }
    let Some(target) = ctx.expected_target else {
        return bad_request();
    };
    // 全局登记换向不能证明原服务已不可核对；在来源失效证据闭合前禁止 force 作废。
    if req.force {
        return json_error(
            StatusCode::CONFLICT,
            "OC_FORCE_CONFIRMATION_REQUIRED",
            "OC_FORCE_CONFIRMATION_REQUIRED",
            "强制交回的确认范围尚不可验证",
        );
    }
    let _policy_permit = match crate::services::opencode_native::policy::acquire_legacy(
        &st.store,
        crate::session::AgentType::OpenCode,
    ) {
        Ok(permit) => permit,
        Err(_) => return super::api::legacy_disabled_response(),
    };
    match st
        .opencode_native
        .return_route_for_target_confirmed(
            &target,
            req.force,
            Some(req.route_version),
            req.confirmed_action_ids.as_deref(),
        )
        .await
    {
        Ok((true, abandoned)) => {
            legacy_returned(&sid, &req.binding_token, returned_version, abandoned)
        }
        Ok((false, _)) => route_held_response(&st, &target),
        Err(err) => native_error_response(&err),
    }
}

/// `POST /opencode/route/native` 请求体：只允许会话 id。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteNativeReq {
    pub session_id: String,
    #[serde(default)]
    pub binding_token: Option<String>,
    #[serde(default)]
    pub route_version: Option<i64>,
    /// 候选预览签发的绑定令牌：确认时把登记对象固定为预览目标（缺失时沿用版本 CAS 行为）。
    #[serde(default)]
    pub candidate_token: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

/// POST /m/api/v1/opencode/route/native（**显式重新进入原生交互**，评审 R-2）。
///
/// 与 `route/legacy` 同一组门：配对身份（403）、会话经 `session_source`（404）、
/// OpenCode 工具（400）、平台（409）、**验证开关**（409 [`codes::OC_ROUTE_NOT_OWNED`]）。
/// 通过后走 `NativeHub::reenter_native_route`：先做一次成功投影拿 `NativeTarget`，
/// 再以 `allow_reactivate=true` 调用既有 `try_claim_route`（旧队列未决行仍会拒绝）。
///
/// 幂等：已 `active` 时再调一次同样返回成功（并把 `updated_at` 刷新）。
/// 成功 → `200 { sessionId, route: "opencode_native" }`；旧队列未决 → `409 OC_ROUTE_CHANGED`。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCreateReq {
    pub request_id: String,
    pub directory: String,
    #[serde(default)]
    pub first_sentence: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCreateRequestId {
    request_id: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCreateRecoverReq {
    request_id: String,
    candidate_token: Option<String>,
    #[serde(default)]
    verify_only: bool,
}

fn valid_create_request_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn create_store_error(err: rusqlite::Error) -> NativeError {
    log::warn!("OpenCode 创建账本不可用: {err}");
    NativeError::new(
        action_codes::STORE_FAILED,
        "OpenCode creation record is unavailable",
    )
}

fn find_create_record(
    st: &RemoteState,
    device: &str,
    request_id: &str,
) -> Result<Option<CreateRecord>, NativeError> {
    st.store
        .with(|conn| opencode_creates::get(conn, device, request_id))
        .map_err(create_store_error)
}

fn update_create_record(
    st: &RemoteState,
    device: &str,
    before: &CreateRecord,
    after: &CreateRecord,
) -> Result<bool, NativeError> {
    st.store
        .with(|conn| opencode_creates::update(conn, device, before, after))
        .map_err(create_store_error)
}

fn create_response(status: StatusCode, record: &CreateRecord) -> Response {
    json_no_store(
        status,
        serde_json::json!({
            "requestId": record.request_id,
            "sessionId": record.session_id,
            "directory": record.project.path,
            "creation": if record.has_verified_creation() { "created" } else { "unconfirmed" },
            "window": record.window,
            "reasonCode": record.reason_code,
            "reason": record.reason,
            "firstSentenceStatus": record.first_sentence_status,
        }),
    )
}

fn repeat_create_response(
    record: &CreateRecord,
    directory: &str,
    first_sentence: Option<&str>,
) -> Response {
    if !record.matches_intent(directory, first_sentence) {
        return native_error_response(&NativeError::new(
            codes::OC_ACTION_INVALID,
            "the creation request ID already identifies another intent",
        ));
    }
    create_response(StatusCode::OK, record)
}

fn read_current_creation(st: &RemoteState, device: &str, request_id: &str) -> Response {
    let receipt = st.store.with(|conn| {
        let Some(mut record) = opencode_creates::get(conn, device, request_id)? else {
            return Ok(None);
        };
        if opencode_creates::first_sentence_confirmed(conn, device, &record)? {
            record.first_sentence_status = Some("confirmed".into());
            if record.window == CreateWindow::Opened {
                record.reason_code = None;
                record.reason = None;
            }
        }
        Ok(Some(record))
    });
    match receipt {
        Ok(Some(record)) => create_response(StatusCode::OK, &record),
        Ok(None) => create_not_found(),
        Err(err) => native_error_response(&create_store_error(err)),
    }
}

fn create_not_found() -> Response {
    json_no_store(
        StatusCode::NOT_FOUND,
        serde_json::json!({"error": "not_found"}),
    )
}

pub async fn native_create_status(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(req): Query<NativeCreateRequestId>,
) -> Response {
    let Some((device, _)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    read_current_creation(&st, &device, &req.request_id)
}

/// 先保留请求 ID 和经过核验的服务，再发一次创建请求。重复请求只返回账本事实。
pub async fn native_create_session(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<NativeCreateReq>,
) -> Response {
    use crate::services::opencode_native::{client::NativeClient, discovery::verify_service_info};
    use std::path::Path;

    let Some((device, name)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    if !valid_create_request_id(&req.request_id) {
        return bad_request();
    }
    match find_create_record(&st, &device, &req.request_id) {
        Ok(Some(record)) => {
            return repeat_create_response(&record, &req.directory, req.first_sentence.as_deref())
        }
        Ok(None) => {}
        Err(err) => return native_error_response(&err),
    }
    // 串行同设备同请求的准备阶段，避免竞争请求在占位前各自创建目录。
    let prepare_key = format!("create\0{device}\0{}", req.request_id);
    let Ok(prepare) = st
        .opencode_native
        .legacy_lock(&prepare_key)
        .try_write_owned()
    else {
        return json_no_store(
            StatusCode::CONFLICT,
            serde_json::json!({"error": "conflict", "requestId": req.request_id}),
        );
    };
    match find_create_record(&st, &device, &req.request_id) {
        Ok(Some(record)) => {
            return repeat_create_response(&record, &req.directory, req.first_sentence.as_deref());
        }
        Ok(None) => {}
        Err(err) => return native_error_response(&err),
    }
    let first_sentence = req
        .first_sentence
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if first_sentence.is_some_and(|s| s.chars().count() > crate::remote::api::MAX_SEND_CHARS) {
        return bad_request();
    }
    let mut project = match crate::inject::create_path::validate_existing(&req.directory) {
        Ok(directory) => Some(directory),
        Err(rej) => {
            let missing = std::fs::symlink_metadata(req.directory.trim())
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if !missing || rej.code != "directory_unresolved" {
                return bad_request_reason_detail(rej.code, &rej.message);
            }
            let mut parent = Path::new(req.directory.trim());
            while let Some(next) = parent.parent() {
                parent = next;
                if !std::fs::symlink_metadata(parent)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    break;
                }
            }
            if let Err(error) =
                crate::inject::create_path::validate_existing(&parent.to_string_lossy())
            {
                return bad_request_reason_detail(error.code, &error.message);
            }
            None
        }
    };
    let path = Path::new(
        project
            .as_ref()
            .map_or(req.directory.trim(), |p| p.path.as_str()),
    );
    let hub = &st.opencode_native;
    let verified = async {
        let service = hub.discover_create_service(path)?;
        let client = NativeClient::for_create(service.clone())?;
        let info = client.read_json("/api/info", &[]).await?;
        verify_service_info(&info, &service)?;
        hub.verify_create_generation(path, &service)?;
        Ok::<_, NativeError>((service, client))
    }
    .await;
    let (service, client) = match verified {
        Ok(pinned) => pinned,
        Err(err) => return native_error_response(&err),
    };
    match st.store.with(|conn| {
        opencode_creates::reserve_directory_intent(
            conn,
            &device,
            &req.request_id,
            req.directory.trim(),
            first_sentence,
        )
    }) {
        Ok(true) => {}
        Ok(false) => {
            return json_no_store(
                StatusCode::CONFLICT,
                serde_json::json!({"error": "conflict", "requestId": req.request_id}),
            );
        }
        Err(err) => return native_error_response(&create_store_error(err)),
    }
    let project = match project.take() {
        Some(directory) => directory,
        None => match crate::inject::create_path::prepare_native_create_directory(&req.directory) {
            Ok(directory) => directory,
            Err(rej) => return bad_request_reason_detail(rej.code, &rej.message),
        },
    };
    if let Err(err) = verify_create_directory(&project) {
        return native_error_response(&err);
    }
    let record = CreateRecord::new(
        &device,
        req.request_id.clone(),
        project,
        &service,
        first_sentence.map(str::to_owned),
    );
    match st
        .store
        .with(|conn| opencode_creates::reserve(conn, &device, &record))
    {
        Ok(true) => {}
        Ok(false) => {
            return match find_create_record(&st, &device, &record.request_id) {
                Ok(Some(record)) => {
                    repeat_create_response(&record, &req.directory, req.first_sentence.as_deref())
                }
                Ok(None) => create_not_found(),
                Err(err) => native_error_response(&err),
            }
        }
        Err(err) => return native_error_response(&create_store_error(err)),
    }
    drop(prepare);
    let request_id = record.request_id.clone();
    let owner = device.clone();
    let work = Arc::clone(&st);
    let task = tokio::spawn(create_on_verified_service(
        work, device, name, record, service, client,
    ));
    match task.await {
        Ok(response) => response,
        Err(err) => {
            log::warn!("OpenCode 创建任务未正常返回: {err}");
            match find_create_record(&st, &owner, &request_id) {
                Ok(Some(record)) => create_response(StatusCode::OK, &record),
                Ok(None) => create_not_found(),
                Err(err) => native_error_response(&err),
            }
        }
    }
}

fn verify_create_directory(
    project: &crate::inject::create_path::CreateDirectory,
) -> Result<(), NativeError> {
    if project.still_current() {
        Ok(())
    } else {
        Err(NativeError::new(
            codes::OC_DIRECTORY_MISMATCH,
            "the selected project directory changed",
        ))
    }
}

async fn create_on_verified_service(
    st: Arc<RemoteState>,
    device: String,
    name: String,
    mut record: CreateRecord,
    service: crate::services::opencode_native::types::ServiceDescriptor,
    client: crate::services::opencode_native::client::NativeClient,
) -> Response {
    use crate::services::opencode_native::projection::segment;
    use std::path::Path;
    let before = record.clone();
    let directory = Path::new(&record.project.path);
    if let Err(err) = st
        .opencode_native
        .verify_create_generation(directory, &service)
        .and_then(|_| verify_create_directory(&record.project))
    {
        return set_create_error(
            &st,
            &device,
            &before,
            record,
            err,
            CreateWindow::Blocked,
            StatusCode::CONFLICT,
        );
    }
    let expected_id = record
        .expected_session_id()
        .expect("new creation has a correlation key");
    let result = client
        .write_once(
            "/api/session",
            &serde_json::json!({
                "id": expected_id,
                "title": "New session",
                "location": {"directory": record.project.path},
                "metadata": {"tuvisCreateRequest": record.correlation_key},
            }),
        )
        .await;
    let created = match result {
        Ok(reply) if (200..300).contains(&reply.status) => reply.body,
        Ok(reply) => {
            let err = NativeError::new(
                codes::OC_HTTP_STATUS,
                &format!("OpenCode did not confirm creation: {}", reply.status),
            );
            return set_create_error(
                &st,
                &device,
                &before,
                record,
                err,
                CreateWindow::Pending,
                StatusCode::OK,
            );
        }
        Err(err) => {
            return set_create_error(
                &st,
                &device,
                &before,
                record,
                err,
                CreateWindow::Pending,
                StatusCode::OK,
            )
        }
    };
    let Some(sid) = created
        .as_ref()
        .and_then(|body| body.pointer("/data/id").or_else(|| body.get("id")))
        .and_then(serde_json::Value::as_str)
        .filter(|sid| *sid == expected_id && segment(sid).is_ok())
    else {
        let err = NativeError::new(
            codes::OC_RESPONSE_INVALID,
            "OpenCode did not return the reserved creation session ID",
        );
        return set_create_error(
            &st,
            &device,
            &before,
            record,
            err,
            CreateWindow::Pending,
            StatusCode::OK,
        );
    };
    record.session_id = Some(sid.to_owned());
    record.window = CreateWindow::Created;
    let saved = save_created_receipt(&st, &device, &before, &record);
    match saved {
        Ok(true) => proceed_created_window(st, device, name, record, service, client).await,
        Ok(false) => match find_create_record(&st, &device, &record.request_id) {
            Ok(Some(current))
                if current.session_id == record.session_id
                    && current.matches_service(&service)
                    && current.window == CreateWindow::Created
                    && !current.window_attempted =>
            {
                proceed_created_window(st, device, name, current, service, client).await
            }
            Ok(Some(current)) => create_response(StatusCode::OK, &current),
            Ok(None) => create_not_found(),
            Err(err) => native_error_response(&err),
        },
        Err(err) => {
            record.reason_code = Some(err.code);
            record.reason = Some(err.message);
            create_response(StatusCode::OK, &record)
        }
    }
}

fn save_created_receipt(
    st: &RemoteState,
    device: &str,
    before: &CreateRecord,
    record: &CreateRecord,
) -> Result<bool, NativeError> {
    let _delivery = opencode_creates::WINDOW_DELIVERY
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    update_create_record(st, device, before, record)
}

fn set_create_error(
    st: &RemoteState,
    owner: &str,
    before: &CreateRecord,
    mut record: CreateRecord,
    err: NativeError,
    window: CreateWindow,
    status: StatusCode,
) -> Response {
    record.window = window;
    record.reason_code = Some(err.code);
    record.reason = Some(err.message);
    match update_create_record(st, owner, before, &record) {
        Ok(true) => create_response(status, &record),
        Ok(false) => read_current_creation(st, owner, &before.request_id),
        Err(err) => {
            log::warn!("OpenCode 创建结果暂未记账: {}", err.code);
            create_response(status, &record)
        }
    }
}

fn created_session(sid: &str, directory: &str) -> Session {
    Session {
        id: sid.to_owned(),
        agent_type: crate::session::AgentType::OpenCode,
        project_name: crate::monitor::project::project_name_from_path(directory),
        project_path: directory.to_owned(),
        title: None,
        git_branch: None,
        github_url: None,
        status: crate::session::SessionStatus::Waiting,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: String::new(),
        pid: 0,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: crate::session::ProcessForm::Cli,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    }
}

async fn confirm_created_target(
    st: &RemoteState,
    record: &CreateRecord,
    session: &Session,
    service: &crate::services::opencode_native::types::ServiceDescriptor,
    client: &crate::services::opencode_native::client::NativeClient,
) -> Result<crate::services::opencode_native::types::NativeTarget, NativeError> {
    use crate::services::opencode_native::{discovery::bind_session, projection::segment};
    use std::path::Path;
    let directory = Path::new(&record.project.path);
    let hub = &st.opencode_native;
    hub.verify_create_generation(directory, service)?;
    let sid = segment(&session.id)?;
    let native = client
        .read_json(&format!("/api/session/{sid}"), &[])
        .await?;
    let info = client.read_json("/api/info", &[]).await?;
    let target = bind_session(session, service, &info, &native)?;
    if let Some(key) = record.correlation_key.as_deref() {
        let data = native.get("data").unwrap_or(&native);
        if data
            .pointer("/metadata/tuvisCreateRequest")
            .and_then(serde_json::Value::as_str)
            != Some(key)
        {
            return Err(NativeError::new(
                codes::OC_RESPONSE_INVALID,
                "native creation correlation could not be verified",
            ));
        }
    }
    if target.directory != directory {
        return Err(NativeError::new(
            codes::OC_DIRECTORY_MISMATCH,
            "native session directory does not match the selected project",
        ));
    }
    hub.verify_create_generation(directory, service)?;
    verify_create_directory(&record.project)?;
    Ok(target)
}

async fn proceed_created_window(
    st: Arc<RemoteState>,
    device: String,
    name: String,
    mut record: CreateRecord,
    service: crate::services::opencode_native::types::ServiceDescriptor,
    client: crate::services::opencode_native::client::NativeClient,
) -> Response {
    use crate::inject::restore_pending::{self, RestoreFlow};
    let session = created_session(
        record.session_id.as_deref().expect("known creation ID"),
        &record.project.path,
    );
    let target = match confirm_created_target(&st, &record, &session, &service, &client).await {
        Ok(target) => target,
        Err(err) => {
            let window = if err.code == codes::OC_DIRECTORY_MISMATCH {
                CreateWindow::Blocked
            } else if record.window == CreateWindow::Failed {
                CreateWindow::Failed
            } else {
                CreateWindow::Pending
            };
            return set_create_error(
                &st,
                &device,
                &record,
                record.clone(),
                err,
                window,
                StatusCode::CONFLICT,
            );
        }
    };
    let mut retry_allowed = false;
    for attempt in 0..2 {
        let before = record.clone();
        let create_window = matches!(record.window, CreateWindow::Created | CreateWindow::Failed)
            || (record.window == CreateWindow::Pending && !record.window_attempted);
        record.creation_verified = true;
        if create_window {
            record.window = CreateWindow::Opening;
            record.window_attempted = true;
        }
        if !create_window && before.creation_verified {
            break;
        }
        match update_create_record(&st, &device, &before, &record) {
            Ok(true) => {
                retry_allowed = create_window;
                break;
            }
            Ok(false) if attempt == 0 => {
                match find_create_record(&st, &device, &before.request_id) {
                    Ok(Some(current))
                        if current.session_id == before.session_id
                            && current.matches_service(&service)
                            && current.window == CreateWindow::Created
                            && !current.window_attempted =>
                    {
                        record = current;
                    }
                    Ok(Some(current)) => return create_response(StatusCode::OK, &current),
                    Ok(None) => return create_not_found(),
                    Err(err) => return native_error_response(&err),
                }
            }
            Ok(false) => return read_current_creation(&st, &device, &before.request_id),
            Err(err) => return native_error_response(&err),
        }
    }
    let before = record.clone();
    if record.window == CreateWindow::Opened && !retry_allowed {
        let hub = st.opencode_native.clone();
        let project = record.project.clone();
        let check_session = session.clone();
        let check_service = service.clone();
        let expected_pid = record.confirmed_window.as_ref().map(|window| window.pid);
        let observed = tokio::task::spawn_blocking(move || {
            restore_pending::created_window_identity(
                &check_session,
                &hub,
                &check_service,
                &project,
                expected_pid,
            )
        })
        .await
        .ok()
        .flatten();
        if observed.as_ref() != record.confirmed_window.as_ref() || observed.is_none() {
            let mut current = record.clone();
            current.window = CreateWindow::Pending;
            current.reason_code = Some("OC_RESTORE_PENDING".into());
            return create_response(StatusCode::OK, &current);
        }
    }
    if record.window != CreateWindow::Opened {
        let spawner = st.resume_spawner.clone();
        let hub = st.opencode_native.clone();
        let project = record.project.clone();
        let restore_session = session.clone();
        let restore_service = service.clone();
        let restore = tokio::task::spawn_blocking(move || {
            if retry_allowed {
                restore_pending::open_created_opencode(
                    &restore_session,
                    spawner.as_ref(),
                    &hub,
                    &restore_service,
                    &project,
                )
            } else {
                Ok(restore_pending::recheck_created_opencode(
                    &restore_session,
                    &hub,
                    &restore_service,
                    &project,
                ))
            }
        })
        .await;
        record.reason_code = None;
        record.reason = None;
        record.window = match restore {
            Ok(Ok((
                RestoreFlow::Dispatched { confirmed: true } | RestoreFlow::PendingConfirmed,
                Some(window),
            ))) => {
                record.confirmed_window = Some(window);
                CreateWindow::Opened
            }
            Ok(Ok((RestoreFlow::Abandoned, _))) => CreateWindow::Failed,
            Ok(Err(err)) => {
                let code = restore_pending::created_target_error_code(&err)
                    .or_else(|| crate::inject::resume::resume_validation_error_code(&err));
                record.reason_code = code.map(str::to_owned);
                let window = match code {
                    Some(codes::OC_DIRECTORY_MISMATCH) => CreateWindow::Blocked,
                    Some(_) if restore_pending::created_target_error_code(&err).is_some() => {
                        CreateWindow::Pending
                    }
                    _ => CreateWindow::Failed,
                };
                record.reason = Some(err);
                record.window_attempted = false;
                window
            }
            Ok(Ok(_)) => {
                record.reason_code = Some("OC_RESTORE_PENDING".into());
                CreateWindow::Pending
            }
            Err(err) => {
                log::warn!("OpenCode 创建后的起窗任务没有完成: {err}");
                record.reason_code = Some("OC_RESTORE_PENDING".into());
                CreateWindow::Pending
            }
        };
        if !retry_allowed
            && before.window == CreateWindow::Opening
            && record.window != CreateWindow::Opened
        {
            return create_response(StatusCode::OK, &before);
        }
        match update_create_record(&st, &device, &before, &record) {
            Ok(true) => {}
            Ok(false) => return read_current_creation(&st, &device, &before.request_id),
            Err(err) => return native_error_response(&err),
        }
    }
    if record.window == CreateWindow::Opened && record.first_sentence.is_some() {
        send_created_first_sentence(
            &st,
            &device,
            &name,
            &session,
            &target,
            &service,
            &mut record,
        )
        .await;
    }
    create_response(StatusCode::OK, &record)
}

pub async fn native_create_preview(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(req): Query<NativeCreateRequestId>,
) -> Response {
    let Some((device, _)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    let record = match find_create_record(&st, &device, &req.request_id) {
        Ok(Some(record)) => record,
        Ok(None) => return create_not_found(),
        Err(err) => return native_error_response(&err),
    };
    let Some(sid) = record
        .session_id
        .as_deref()
        .filter(|_| record.window != CreateWindow::Blocked)
    else {
        return create_not_found();
    };
    let session = created_session(sid, &record.project.path);
    let preview = async {
        verify_create_directory(&record.project)?;
        let service = st
            .opencode_native
            .discover_create_service(std::path::Path::new(&record.project.path))?;
        let preview = st
            .opencode_native
            .preview_candidate(&session, &device)
            .await?;
        st.opencode_native.confirm_create_candidate(
            &session,
            &device,
            &preview.preview_token,
            &service,
        )?;
        Ok::<_, NativeError>(preview)
    }
    .await;
    match preview {
        Ok(preview) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "previewToken": preview.preview_token,
                "sessionId": session.id,
                "directory": preview.target.directory,
                "serviceId": preview.target.runtime.service_id,
                "servicePid": preview.target.runtime.pid,
            }),
        ),
        Err(err) => native_error_response(&err),
    }
}

pub async fn native_create_recover(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Json(req): Json<NativeCreateRecoverReq>,
) -> Response {
    use crate::services::opencode_native::{client::NativeClient, discovery::verify_service_info};
    let Some((device, name)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    let mut record = match find_create_record(&st, &device, &req.request_id) {
        Ok(Some(record)) => record,
        Ok(None) => return create_not_found(),
        Err(err) => return native_error_response(&err),
    };
    if record.window == CreateWindow::Blocked || (req.verify_only && record.has_verified_creation())
    {
        return create_response(StatusCode::OK, &record);
    }
    let recovering_receipt = req.verify_only || record.session_id.is_none();
    let expected_id = record.expected_session_id();
    let Some(sid) = record.session_id.as_deref().or(expected_id.as_deref()) else {
        return create_response(StatusCode::OK, &record);
    };
    let session = created_session(sid, &record.project.path);
    let verified = async {
        let service = st
            .opencode_native
            .discover_create_service(std::path::Path::new(&record.project.path))?;
        if !record.matches_service(&service) {
            if recovering_receipt {
                return Err(NativeError::new(
                    codes::OC_GENERATION_MISMATCH,
                    "only the verified creation service may resolve a missing receipt",
                ));
            }
            let token = req.candidate_token.as_deref().ok_or_else(|| {
                NativeError::new(
                    codes::OC_GENERATION_MISMATCH,
                    "the creation service changed; confirm the new target separately",
                )
            })?;
            st.opencode_native
                .confirm_create_candidate(&session, &device, token, &service)?;
        }
        let client = NativeClient::for_create(service.clone())?;
        let info = client.read_json("/api/info", &[]).await?;
        verify_service_info(&info, &service)?;
        Ok::<_, NativeError>((service, client))
    }
    .await;
    let (service, client) = match verified {
        Ok(verified) => verified,
        Err(err) => {
            record.reason_code = Some(err.code);
            record.reason = Some(err.message);
            if record.window == CreateWindow::Opened {
                record.window = CreateWindow::Pending;
            }
            return create_response(StatusCode::OK, &record);
        }
    };
    if recovering_receipt {
        if let Err(err) = confirm_created_target(&st, &record, &session, &service, &client).await {
            record.reason_code = Some(err.code);
            record.reason = Some(err.message);
            return create_response(StatusCode::OK, &record);
        }
        let before = record.clone();
        record.session_id = Some(session.id.clone());
        record.creation_verified = true;
        if !record.window_attempted {
            record.window = CreateWindow::Created;
        }
        record.reason_code = None;
        record.reason = None;
        return match save_created_receipt(&st, &device, &before, &record) {
            Ok(true) => create_response(StatusCode::OK, &record),
            Ok(false) => match find_create_record(&st, &device, &req.request_id) {
                Ok(Some(current)) => create_response(StatusCode::OK, &current),
                Ok(None) => create_not_found(),
                Err(err) => native_error_response(&err),
            },
            Err(err) => native_error_response(&err),
        };
    }
    if !record.matches_service(&service) {
        if let Err(err) = confirm_created_target(&st, &record, &session, &service, &client).await {
            record.reason_code = Some(err.code);
            record.reason = Some(err.message);
            return create_response(StatusCode::CONFLICT, &record);
        }
        let before = record.clone();
        record.adopt_service(&service);
        match update_create_record(&st, &device, &before, &record) {
            Ok(true) => {}
            Ok(false) => return read_current_creation(&st, &device, &before.request_id),
            Err(err) => return native_error_response(&err),
        }
    }
    proceed_created_window(st, device, name, record, service, client).await
}

async fn send_created_first_sentence(
    st: &RemoteState,
    device: &str,
    name: &str,
    session: &Session,
    target: &crate::services::opencode_native::types::NativeTarget,
    service: &crate::services::opencode_native::types::ServiceDescriptor,
    record: &mut CreateRecord,
) {
    use crate::services::opencode_native::types::{DeliveryKind, NativeOperation, SendPayload};
    let hub = &st.opencode_native;
    let before = record.clone();
    let Some(text) = record.first_sentence.clone() else {
        return;
    };
    let result = match hub.verify(device, &record.first_action_id).await {
        Ok(view) => Ok(view),
        Err(err) if err.code == action_codes::NOT_FOUND => {
            async {
                let window = crate::inject::restore_pending::created_window_identity(
                    session,
                    hub,
                    service,
                    &record.project,
                    record.confirmed_window.as_ref().map(|window| window.pid),
                )
                .ok_or_else(|| {
                    NativeError::new(
                        codes::OC_INSTANCE_OWNERSHIP_UNVERIFIED,
                        "the original created window has not been confirmed for sending",
                    )
                })?;
                if record.confirmed_window.as_ref() != Some(&window) {
                    return Err(NativeError::new(
                        codes::OC_BINDING_STALE,
                        "the original created window changed",
                    ));
                }
                let binding_token = hub
                    .claim_created_route(device, &record.request_id, target, &window)
                    .await?;
                let actor = NativeActor {
                    device_id: device.to_owned(),
                    device_name: name.to_owned(),
                    signature_enabled: st
                        .store
                        .with(crate::inject::normalize::message_signature_enabled_conn),
                };
                let action_id = record.first_action_id.clone();
                let command = NativeCommand {
                    action_id: action_id.clone(),
                    session_id: session.id.clone(),
                    binding_token,
                    operation: NativeOperation::Send(SendPayload {
                        text,
                        attachment_paths: Vec::new(),
                        delivery: DeliveryKind::Queue,
                        client_message_id: format!("msg_{action_id}"),
                    }),
                };
                hub.execute_pinned(&actor, session, command, Some(target))
                    .await
            }
            .await
        }
        Err(err) => Err(err),
    };
    match result {
        Ok(view) => {
            record.first_sentence_status = serde_json::to_value(view.state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned));
            record.reason_code = view.reason_code;
            record.reason = None;
            if view.state == crate::services::opencode_native::types::ActionState::Confirmed {
                record.first_sentence = None;
            }
        }
        Err(err) => {
            record.reason_code = Some(err.code);
            record.reason = Some(err.message);
        }
    }
    match update_create_record(st, device, &before, record) {
        Ok(true) => {}
        Ok(false) => {
            if let Ok(Some(latest)) = find_create_record(st, device, &before.request_id) {
                *record = latest;
            }
        }
        Err(err) => log::warn!("OpenCode 新建首句结果暂未记账: {}", err.code),
    }
}

pub async fn native_route_native(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<RouteNativeReq>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // J5（重选链总耗时）：handler 级单行结构化日志——健康链基线与慢链观测共用
    // （IG-f2 >90s 慢链观测归档；分段拆分需 projection 重构，属后续批申报）。
    let _j5 = J5TimingGuard("route_native", std::time::Instant::now());
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let sid = req.session_id.trim().to_owned();
    if sid.is_empty() {
        return bad_request();
    }
    let Some(request_id) = req.request_id.clone() else {
        return execute_native_reselect(&st, &headers, req, None).await;
    };
    if !valid_create_request_id(&request_id) {
        return bad_request();
    }
    let Some((device, _)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    match st
        .store
        .with(|conn| opencode_reselect::reserve(conn, &device, &sid, &request_id))
    {
        Ok(true) => {}
        Ok(false) => return read_native_reselect(&st, &device, &sid, &request_id),
        Err(err) => return native_error_response(&create_store_error(err)),
    }
    let work = st.clone();
    let task_device = device.clone();
    let task_session = sid.clone();
    let task_request = request_id.clone();
    let run = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let result =
            execute_native_reselect(&work, &headers, req, Some((&task_device, &task_request)))
                .await;
        log::info!(
            "opencode_reselect request_id={} stage=operation total_ms={} status={}",
            task_request,
            started.elapsed().as_millis(),
            result.status().as_u16()
        );
        if !result.status().is_success() {
            if let Err(err) = work.store.with(|conn| {
                opencode_reselect::finish(conn, &task_device, &task_session, &task_request, None)
            }) {
                log::warn!("OpenCode 重选结果未能记账: {err}");
            }
            return result;
        }
        read_native_reselect(&work, &task_device, &task_session, &task_request)
    });
    run.await
        .unwrap_or_else(|_| read_native_reselect(&st, &device, &sid, &request_id))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReselectStatusReq {
    session_id: String,
    request_id: String,
}

pub async fn native_reselect_status(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(req): Query<ReselectStatusReq>,
) -> Response {
    let Some((device, _)) = device_identity(&st, &headers) else {
        return forbidden_defense();
    };
    if !valid_create_request_id(&req.request_id) || req.session_id.trim().is_empty() {
        return bad_request();
    }
    read_native_reselect(&st, &device, &req.session_id, &req.request_id)
}

fn read_native_reselect(st: &RemoteState, device: &str, session: &str, request: &str) -> Response {
    use crate::database::dao::{opencode_actions, opencode_pinned_windows};
    let result = st.store.with(|conn| {
        let Some(receipt) = opencode_reselect::get(conn, device, session, request)? else {
            return Ok::<_, rusqlite::Error>(None);
        };
        let mut value = serde_json::json!({"sessionId": session, "requestId": request});
        match receipt {
            ReselectReceipt::InProgress => value["state"] = serde_json::json!("in_progress"),
            ReselectReceipt::NotApplied => value["state"] = serde_json::json!("not_applied"),
            ReselectReceipt::Committed(receipt) => {
                let route = opencode_actions::route_conn(conn, session)?;
                let window = opencode_pinned_windows::pinned_window_conn(conn, session)?;
                let current = route.is_some_and(|route| route.route_version == receipt.route_version && route.to_target(session).is_some_and(|target| st.opencode_native.binding_token_for(&target) == receipt.binding_token) && route.route_state == opencode_actions::ROUTE_ACTIVE)
                    && window.is_some_and(|window| window.window_pid == i64::from(receipt.window.pid)
                        && window.window_started_at_ms == receipt.window.started_at_ms
                        && window.window_boot == receipt.window.boot);
                value = serde_json::json!({
                    "sessionId": session, "requestId": request, "state": "committed", "current": current,
                    "route": ROUTE_NATIVE, "routeVersion": receipt.route_version,
                    "bindingToken": receipt.binding_token, "window": receipt.window,
                });
            }
        }
        Ok(Some(value))
    });
    match result {
        Ok(Some(value)) => json_no_store(StatusCode::OK, value),
        Ok(None) => create_not_found(),
        Err(err) => native_error_response(&create_store_error(err)),
    }
}

async fn execute_native_reselect(
    st: &Arc<RemoteState>,
    headers: &HeaderMap,
    req: RouteNativeReq,
    request: Option<(&str, &str)>,
) -> Response {
    let sid = req.session_id.trim().to_owned();
    let ctx = match resolve_native_context(st, headers, &sid).await {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    // 重新进入原生 = 打开**写**通道：与 `POST /opencode/actions` 同一道开关门。
    if !native_enabled(st) {
        return json_error(
            StatusCode::CONFLICT,
            codes::OC_ROUTE_NOT_OWNED,
            codes::OC_ROUTE_NOT_OWNED,
            "opencode 原生写入口当前未启用",
        );
    }
    let has_route = match st
        .store
        .with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &sid))
    {
        Ok(route) => route.is_some(),
        Err(_) => {
            return native_error_response(&NativeError::new(
                codes::OC_RESPONSE_INVALID,
                "native route unavailable",
            ))
        }
    };
    if has_route && req.route_version.is_none() {
        return native_error_response(&NativeError::new(
            codes::OC_BINDING_STALE,
            "the selected native route version is required",
        ));
    }
    // 候选确认必须携带预览凭据：版本 CAS 只能证旧路由未变，不能固定「用户看到的
    // 候选」——缺 previewToken 的显式选择一律拒绝，绝不回落到「发现谁就登记谁」。
    let Some(candidate_token) = req
        .candidate_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "candidate_required",
            "candidate_required",
            "重新进入原生必须先候选预览，并在确认时携带 previewToken",
        );
    };
    let candidate = (candidate_token, ctx.device_id.as_str());
    match st
        .opencode_native
        .reenter_native_route_recorded(
            &ctx.session,
            req.binding_token.as_deref(),
            req.route_version,
            Some(candidate),
            request,
        )
        .await
    {
        Ok(Some((binding_token, route_version))) => json_no_store(
            StatusCode::OK,
            serde_json::json!({ "sessionId": sid, "route": ROUTE_NATIVE, "bindingToken": binding_token, "routeVersion": route_version }),
        ),
        // 被拒（旧队列未决 / 未决动作挡住换绑）：零变更，如实回受控码。
        // 文案按**触发族**分流（patch3 O1，H13 证据 §10-O1）：认领被拒只有两族真因——
        // 旧 inject_queue 未决行 vs 换代期未决原生动作。此处只读查询用于选择文案
        // （拒绝本身已在服务层原子判定完成，竞态只影响措辞、不影响判定）。
        Ok(None) => {
            let legacy_pending = st
                .store
                .with(|conn| {
                    crate::database::dao::inject_queue::count_opencode_pending_for_session_conn(
                        conn, &sid,
                    )
                })
                .unwrap_or(0)
                > 0;
            let reason = if legacy_pending {
                OC_ROUTE_HELD_LEGACY_REASON
            } else {
                OC_ROUTE_HELD_NATIVE_ACTION_REASON
            };
            json_error(
                StatusCode::CONFLICT,
                OC_ROUTE_CHANGED,
                OC_ROUTE_CHANGED,
                reason,
            )
        }
        Err(err) => native_error_response(&err),
    }
}

/// GET /opencode/route/preview?session_id=（**候选预览**）：为「重新选择当前目标」固定用户看到的目标。
///
/// 与 view 相同的配对/会话/平台门；**只读**：不认领路由、不写任何状态、不动现有连接。
/// 返回脱敏候选摘要 + 服务端签发的候选绑定令牌：TTL 内、同设备、同监听代际才能作为
/// `POST /opencode/route/native` 的 `candidateToken`；候选变化时确认会被拒绝。
pub async fn native_route_preview(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // J5（重选链总耗时）：preview handler 级单行结构化日志（同上）。
    let _j5 = J5TimingGuard("route_preview", std::time::Instant::now());
    let Some(sid) = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let ctx = match resolve_native_context(&st, &headers, &sid).await {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    let previous_route = match st.store.with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &sid)) {
        Ok(route) => route.map(|route| serde_json::json!({
            "routeVersion": route.route_version,
            "bindingToken": route.to_target(&sid).map(|target| st.opencode_native.binding_token_for(&target)),
        })),
        Err(err) => return native_error_response(&create_store_error(err)),
    };
    match st
        .opencode_native
        .preview_candidate(&ctx.session, &ctx.device_id)
        .await
    {
        Ok(preview) => json_no_store(
            StatusCode::OK,
            serde_json::json!({
                "sessionId": ctx.session.id,
                "candidate": {
                    "sessionId": preview.target.session_id,
                    "directory": preview.target.directory.to_string_lossy(),
                    "serviceId": preview.target.runtime.service_id,
                    "pid": preview.target.runtime.pid,
                    "startedAtMs": preview.target.runtime.started_at_ms,
                    "bindingToken": preview.binding_token,
                },
                "expiresAtMs": preview.expires_at_ms,
                "previewToken": preview.preview_token,
                "previousRoute": previous_route,
            }),
        ),
        Err(err) => native_error_response(&err),
    }
}

/// 交回成功：`abandoned` = 本次**作废**的「代际已不可核对」动作条数（非 force 恒 0）。
/// 客户端据此给出「N 条无法核对的动作已作废；若上一次写入其实成功了，可能导致重复发送」。
fn legacy_returned(
    session_id: &str,
    binding_token: &str,
    route_version: i64,
    abandoned: usize,
) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::json!({
            "sessionId": session_id,
            "route": ROUTE_LEGACY,
            "bindingToken": binding_token,
            "routeVersion": route_version,
            "abandoned": abandoned,
        }),
    )
}

/// 交回被拒：`unverifiableActions` > 0 表示「显式 force 可以作废它们后交回」——
/// 前端据此给出带**重复发送风险**的二次确认（绝不静默强推）。
fn route_held_response(
    st: &RemoteState,
    target: &crate::services::opencode_native::types::NativeTarget,
) -> Response {
    let (version, ids) = match st.opencode_native.force_candidates_for_target(target) {
        Ok(value) => value,
        Err(err) => {
            // 预读失败不影响拒绝，但不能提供可执行的强制确认。
            log::error!("opencode 路由交回预读失败: {err:?}");
            (0, Vec::new())
        }
    };
    json_no_store(
        StatusCode::CONFLICT,
        serde_json::json!({
            "error": OC_ROUTE_CHANGED,
            "reasonCode": OC_ROUTE_CHANGED,
            "reason": OC_ROUTE_HELD_REASON,
            "unverifiableActions": ids.len(),
            "unverifiableActionIds": ids,
            "routeVersion": version,
            "forceAvailable": false,
        }),
    )
}

/// GET /m/api/v1/opencode/agents?session_id=：只读 agent 视图（当前 + primary 选项）。
///
/// 与 view 相同的会话/平台门；不认领路由、不发写请求。读不到就返回受控错误，
/// 客户端不得猜测默认 agent。
pub async fn native_agents(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let token = match expected_binding_header(&headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        &sid,
        native_platform_supported(),
        true,
        token,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    if let Err(response) = check_expected_binding(&st, &mut ctx, token, true, false) {
        return response;
    }
    match st
        .opencode_native
        .agent_view_pinned(&ctx.session, ctx.expected_target.as_ref())
        .await
    {
        Ok(view) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(view).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(err) => native_error_response(&err),
    }
}

/// GET /m/api/v1/opencode/view?session_id=（权威只读视图 + 路由认领入口）。
///
/// - 未配对 → 403（gate）；会话不存在 → 404；非 OpenCode → 400；非 macOS → 409
///   OC_PLATFORM_UNVERIFIED；
/// - 开关开 ⇒ 尝试原子认领（`select_native_route`）：成功/既有归属 ⇒
///   `route=opencode_native`；旧 pending 未决被拒 ⇒ `route=legacy` +
///   `routeReasonCode=legacy_pending`（不迁移、不重复提交，用户先处理旧项）；
/// - 开关关 ⇒ 只读不认领（既有 active 归属仍如实报 `opencode_native`——冻结态，
///   旧入口 409、原生只读核对仍可用）。
pub async fn native_view(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    native_view_with(
        State(st),
        headers,
        Query(params),
        native_platform_supported(),
    )
    .await
}

/// [`native_view`] 的可测缝：平台判据由调用方给，**其余完全同一条代码路径**。
///
/// 生产路由只用 [`native_view`]（恒传 [`native_platform_supported`]），HTTP 面不变。
/// 存在的唯一理由：`OC_PLATFORM_UNVERIFIED` 分支在 macOS 主体上不可达，而它是本批
/// 的硬边界，必须有一套真实断言（受控码 + 门在任何原生读写之前）。集成测试
/// `non_macos_platform_gate_refuses_before_any_native_request` 消费本入口。
pub async fn native_view_with(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    platform_supported: bool,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let token = match expected_binding_header(&headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let mut ctx =
        match resolve_native_context_mode(&st, &headers, &sid, platform_supported, true, token)
            .await
        {
            Ok(ctx) => ctx,
            Err(resp) => return resp,
        };
    if let Err(response) = check_expected_binding(&st, &mut ctx, token, false, false) {
        return response;
    }
    let claim = native_enabled(&st);
    match st
        .opencode_native
        .view_and_claim_bound(
            &ctx.session,
            claim,
            ctx.expected_target.as_ref(),
            ctx.expected_requires_active,
        )
        .await
    {
        Ok((view, claim)) => {
            let body = NativeRouteView {
                view,
                route: if claim.owned {
                    ROUTE_NATIVE
                } else {
                    ROUTE_LEGACY
                },
                route_reason_code: claim.declined_reason,
                route_version: claim.route_version,
            };
            json_no_store(
                StatusCode::OK,
                serde_json::to_value(&body).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        Err(err) => {
            // O-M9-2 ①（patch6）：路由行在场时，认领拒绝体追加只读的 `routeVersion`
            //（数据库事实，零凭据零副作用）——候选重启 + 页面刷新后的「尚未接管」面，
            // 显式重选请求据此一次成形，不再因缺版本必 409 成结构性循环（N7 场景 12
            // 能走通的前提 = 页面不刷新的既有缺口）。无路由行（缺省态）不携带该键：
            // 既有错误体三键契约形状不变，仅追加可选字段（schema 变化已申报）。
            let route_version = st
                .store
                .with(|conn| crate::database::dao::opencode_actions::route_conn(conn, &sid))
                .ok()
                .flatten()
                .map(|route| route.route_version);
            match route_version {
                Some(version) => json_no_store(
                    native_error_status(&err),
                    serde_json::json!({
                        "error": err.code,
                        "reasonCode": err.code,
                        "reason": err.message,
                        "routeVersion": version,
                    }),
                ),
                None => native_error_response(&err),
            }
        }
    }
}

/// GET /m/api/v1/opencode/forms/{form_id}?session_id=（原生表单结果，R5b）。
///
/// 只读回读**官方** `state.status` / `state.answer`：不依赖 `metadata.tool`（没有 tool
/// 关联的表单同样可读），也不以「pending 消失」代替结果。
///
/// 门与既有路由同源：配对身份（403）→ `session_source` 会话解析（404）→ OpenCode 工具
/// （400）→ 平台（409）。服务层 `NativeHub::form_result` 再做绑定/代际/目录与
/// `spec.id` 校验。
///
/// **不泄露存在性**：不存在、属其他会话、读失败、id 不符一律 404（同一受控码与固定
/// 文案）；200 才返回 `NativeFormResult` DTO。答案只出现在这次响应里，不落账本。
pub async fn native_form_result(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Path(form_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let form_id = form_id.trim().to_string();
    if form_id.is_empty() {
        return bad_request();
    }
    let token = match expected_binding_header(&headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        &sid,
        native_platform_supported(),
        true,
        token,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    if let Err(response) = check_expected_binding(&st, &mut ctx, token, true, false) {
        return response;
    }
    match st
        .opencode_native
        .form_result_pinned(&ctx.session, &form_id, ctx.expected_target.as_ref())
        .await
    {
        Ok(result) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({})),
        ),
        // 不存在 / 其他会话 / 读不到 / id 不符：同一个 404，不做存在性预言机。
        Err(_) => json_error(
            StatusCode::NOT_FOUND,
            FORM_RESULT_NOT_FOUND,
            FORM_RESULT_NOT_FOUND,
            FORM_RESULT_NOT_FOUND_REASON,
        ),
    }
}

/// GET /m/api/v1/opencode/events?session_id=（修订号 SSE 订阅）。
///
/// 只读订阅：不取验证开关（开关关闭时既有动作仍可只读核对/订阅）。吊销/停机经
/// `sse_registry` 关闭信号断流；hub shutdown（停远程同一调用）丢弃 watch 发送端，
/// 流自然收尾——「RemoteState 注销/停止会关闭原生 watch」。
pub async fn native_events(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    // 订阅恢复目标时，固定原服务代际并在建立流之前重新核验。
    let token = params.get("expected_binding_token").map(String::as_str);
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        &sid,
        native_platform_supported(),
        true,
        token,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(resp) => return resp,
    };
    if let Err(response) = check_expected_binding(&st, &mut ctx, token, true, false) {
        return response;
    }
    let Some(expected) = ctx.expected_target else {
        return bad_request();
    };
    let require_active = ctx.expected_requires_active;
    if let Err(error) = st
        .opencode_native
        .validate_pinned_subscription(&ctx.session, &expected, require_active)
        .await
    {
        return native_error_response(&error);
    }
    let hub = st.opencode_native.clone();
    let device = crate::remote::gate::extract_device(&headers).unwrap_or_default();
    let (conn_guard, close_rx) =
        crate::remote::server::SseConnGuard::register(&st.sse_registry, &device);
    let rx = st.opencode_native.watch(&sid);
    let Some(pin) = hub.capture_subscription_pin(&expected, require_active) else {
        return native_error_response(&NativeError::new(
            codes::OC_TRANSPORT_FAILED,
            "native event source is unavailable",
        ));
    };
    let check_hub = hub.clone();
    let check_pin = pin.clone();
    let revisions = tokio_stream::wrappers::WatchStream::new(rx)
        .take_while(move |_| futures::future::ready(hub.subscription_pin_current(&pin)))
        .map(|revision| {
            Ok::<_, Infallible>(
                sse::Event::default()
                    .event("revision")
                    .data(revision.to_string()),
            )
        });
    // 没有新 revision 时也要发现路由/来源失效，不能让旧流一直 keepalive。
    let invalidated = async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if !check_hub.subscription_pin_current(&check_pin) {
                break;
            }
        }
    };
    let stop = async move {
        tokio::select! { _ = close_rx => {}, _ = invalidated => {} }
    };
    let guarded = revisions.take_until(stop);
    let cleaned = crate::remote::server::CleanupStream {
        inner: guarded,
        _conn: conn_guard,
    };
    sse::Sse::new(cleaned)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

/// POST /m/api/v1/opencode/actions（NativeCommand——唯一原生写入口）。
///
/// - 请求体拒绝未知字段：客户端**不能**自带 raw URL / method / permissions 对象
///   （DTO `deny_unknown_fields`；任何 rejection → 400）；
/// - actor 只由配对身份 + 签名设置生成（[`NativeActor`]），前端不决定 native endpoint；
/// - 开关关闭 ⇒ 撤回新原生写权限：409 OC_ROUTE_NOT_OWNED（既有动作仍可只读核对）；
/// - 成功（含 Unconfirmed 未知结果）⇒ 200 ActionView；零写出拒绝按受控码映射
///   400/404/409（见 [`native_error_response`]）。
pub async fn native_action_execute(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<NativeCommand>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(command) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    if command.action_id.trim().is_empty() || command.session_id.trim().is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    match st
        .opencode_native
        .existing_command_outcome(&device_id, &command)
    {
        Ok(Some(view)) => {
            return json_no_store(
                StatusCode::OK,
                serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        Ok(None) => {}
        Err(error) => return native_error_response(&error),
    }
    let mut ctx = match resolve_native_context_mode(
        &st,
        &headers,
        command.session_id.trim(),
        true,
        true,
        Some(&command.binding_token),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(resp) => {
            // 异步解析期间另一同 ID 请求可能已取得写权；先再次读取账本。
            match st
                .opencode_native
                .existing_command_outcome(&device_id, &command)
            {
                Ok(Some(view)) => {
                    return json_no_store(
                        StatusCode::OK,
                        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
                    )
                }
                Err(error) => return native_error_response(&error),
                Ok(None) if resp.status() == StatusCode::BAD_REQUEST => {
                    return native_error_response(&NativeError::new(
                        codes::OC_RESPONSE_INVALID,
                        "the action target could not be verified",
                    ))
                }
                Ok(None) => return resp,
            }
        }
    };
    // 无 active 路由的本地预暂停只接受已发布视图的原绑定，不能采用同 ID 新扫描卡的目录。
    if ctx.expected_target.is_none()
        && matches!(&command.operation, crate::services::opencode_native::types::NativeOperation::SetOutboxPaused(p) if p.paused)
    {
        let Some(target) = st
            .opencode_native
            .cached_target_for_binding(&command.session_id, &command.binding_token)
        else {
            return native_error_response(&NativeError::new(
                codes::OC_BINDING_STALE,
                "the selected native binding changed",
            ));
        };
        ctx.session.project_path = target.directory.to_string_lossy().into_owned();
        ctx.expected_target = Some(target);
    }
    // 验证开关关闭：撤回**新**原生写权限（已发动作的只读核对不受影响）。
    if !native_enabled(&st) {
        return json_error(
            StatusCode::CONFLICT,
            codes::OC_ROUTE_NOT_OWNED,
            codes::OC_ROUTE_NOT_OWNED,
            "opencode 原生写入口当前未启用",
        );
    }
    // 签名设置经既有单点现读（remote_message_signature，缺省 off）。
    let signature_enabled = st
        .store
        .with(crate::inject::normalize::message_signature_enabled_conn);
    let actor = NativeActor {
        device_id: ctx.device_id,
        device_name: ctx.device_name,
        signature_enabled,
    };
    match st
        .opencode_native
        .execute_pinned(&actor, &ctx.session, command, ctx.expected_target.as_ref())
        .await
    {
        Ok(view) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(err) => native_error_response(&err),
    }
}

/// 只读列出设备自己的冻结记录。旧行缺原服务代际，不能据此授权放弃。
async fn legacy_outbox(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some((device, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    if !crate::services::opencode_native::policy::legacy_retired() {
        return json_no_store(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": "not_found"}),
        );
    }
    let Some(session_id) = params
        .get("session_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    match st.store.with(|conn| {
        crate::database::dao::inject_queue::frozen_opencode_pending_metadata(
            conn, session_id, &device,
        )
    }) {
        Ok(rows) => {
            let items: Vec<_> = rows
                .into_iter()
                .map(|(id, enqueued_at)| {
                    serde_json::json!({
                        "id": id,
                        "enqueuedAt": enqueued_at,
                        "disposition": "blocked_unverified_generation",
                    })
                })
                .collect();
            json_no_store(
                StatusCode::OK,
                serde_json::json!({"count": items.len(), "items": items}),
            )
        }
        Err(_) => json_no_store(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": "OC_STORE_FAILED"}),
        ),
    }
}

/// 本地队列离线读取：已有项的 session 由持久账本确认，无需原生服务在线。
pub async fn native_outbox(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if device_identity(st.as_ref(), &headers).is_none() {
        return forbidden_defense();
    }
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let token = match expected_binding_header(&headers) {
        Ok(Some(token)) if !token.is_empty() => token,
        Ok(_) => {
            return native_error_response(&NativeError::new(
                codes::OC_BINDING_STALE,
                "the selected native binding is required",
            ))
        }
        Err(response) => return response,
    };
    let pinned = {
        let active = match st.opencode_native.restore_active_target(sid) {
            Ok(active) => active.map(|(_, target)| target),
            Err(error) => return native_error_response(&error),
        };
        let target = match active {
            Some(target) if st.opencode_native.binding_matches(&target, token) => Some(target),
            Some(_) => None,
            None => st.opencode_native.cached_target_for_binding(sid, token),
        };
        let Some(target) = target else {
            return native_error_response(&NativeError::new(
                codes::OC_BINDING_STALE,
                "the selected native binding changed",
            ));
        };
        target
    };
    let snapshot = match st
        .opencode_native
        .outbox_view_for_target(sid, &pinned.directory.to_string_lossy())
    {
        Ok(value) => value,
        Err(error) => return native_error_response(&error),
    };
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(snapshot).unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// GET /m/api/v1/opencode/actions/{action_id}（只读动作视图；别人的动作 404）。
/// GET /opencode/actions?session_id=（J2 无本地锚发现）：属主设备×会话的**需核对
/// 动作清单**——动作行（含 runtime_dead 呈现面）+ 前置拒绝（NotSent 视图）。
/// 绑定态无关（旧绑定 409 下仍可用）；非属主设备同名会话查询=空数组（不泄露
/// 存在性，与 read 同口径）；移动端把服务端清单与本地锚按 action ID 去重。
pub async fn native_action_list(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let Some(sid) = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    match st
        .opencode_native
        .actions_for_session(&device_id, &sid)
        .await
    {
        Ok(views) => {
            let items: Vec<serde_json::Value> = views
                .iter()
                .map(|view| serde_json::to_value(view).unwrap_or_else(|_| serde_json::json!({})))
                .collect();
            json_no_store(StatusCode::OK, serde_json::Value::Array(items))
        }
        Err(err) => native_error_response(&err),
    }
}

pub async fn native_action_read(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Path(action_id): Path<String>,
) -> Response {
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let action_id = action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    }
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    match st.opencode_native.action(&device_id, &action_id).await {
        Ok(view) => action_response(view),
        Err(err) => native_error_response(&err),
    }
}

/// POST /m/api/v1/opencode/actions/{action_id}/verify（**只读核对**，不是执行）：
/// 只按权威回读收敛结果不明的动作，绝不重发。开关关闭时同样可用（既有动作仍可核对）。
pub async fn native_action_verify(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Path(action_id): Path<String>,
) -> Response {
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let action_id = action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    };
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    match st.opencode_native.verify(&device_id, &action_id).await {
        Ok(view) => action_response(view),
        Err(err) => native_error_response(&err),
    }
}

/// POST /m/api/v1/opencode/actions/{action_id}/confirm-unknown（patch3 D2，
/// **用户显式确认终态未知**，不是执行也不是核对）：死代际 `acknowledged`/`unconfirmed`
/// 行的用户可达出口。账本转入 `unknown_confirmed` 归档态（判定保持未知，不冒充定案；
/// 审计 = 理由词 `user_confirmed_unknown` + 属主设备 + `updated_at`）；未决屏障解除、
/// 待答占用释放、绝不再派发。
///
/// 拒绝面：非属主/不存在 → 404 `OC_ACTION_NOT_FOUND`（不泄露存在性）；行代际仍可核对
/// （当前活代际）→ 409 `OC_ACTION_STATE`（零变更）。其余状态（终态/派发中/未派发）
/// 幂等原样返回当前视图（与 verify 的重复点击同形态，零改写）。
pub async fn native_action_confirm_unknown(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Path(action_id): Path<String>,
) -> Response {
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let action_id = action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    };
    if !native_platform_supported() {
        return json_error(
            StatusCode::CONFLICT,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED,
            OC_PLATFORM_UNVERIFIED_REASON,
        );
    }
    match st
        .opencode_native
        .confirm_unknown(&device_id, &action_id)
        .await
    {
        Ok(view) => action_response(view),
        Err(err) => native_error_response(&err),
    }
}

fn action_response(view: ActionView) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
    )
}

/// 配对设备只读策略；修改仅允许桌面受控设置命令。
pub async fn native_policy(State(st): State<Arc<RemoteState>>, headers: HeaderMap) -> Response {
    if device_identity(st.as_ref(), &headers).is_none() {
        return forbidden_defense();
    }
    use crate::services::opencode_native::policy::{read_policy, OpenCodeInteractionPolicy};
    let policy = read_policy(&st.store);
    let create_route = if native_platform_supported() {
        ROUTE_NATIVE
    } else if cfg!(windows) && policy == OpenCodeInteractionPolicy::LegacyCompatible {
        "legacy"
    } else {
        "unavailable"
    };
    json_no_store(
        StatusCode::OK,
        serde_json::json!({
            "policy": policy,
            "platform": std::env::consts::OS,
            "createRoute": create_route,
        }),
    )
}

/// 策略流不要求工具绑定或平台就绪；Windows 和旧路径同样必须接收策略失效通知。
pub async fn native_policy_events(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Response {
    if device_identity(st.as_ref(), &headers).is_none() {
        return forbidden_defense();
    }
    let device = crate::remote::gate::extract_device(&headers).unwrap_or_default();
    let (conn_guard, close_rx) =
        crate::remote::server::SseConnGuard::register(&st.sse_registry, &device);
    let (identity, rx) = crate::services::opencode_native::policy::subscribe_policy(&st.store);
    let changes =
        futures::stream::unfold((rx, true, st), move |(mut rx, initial, st)| async move {
            if !initial {
                loop {
                    match rx.recv().await {
                        Ok(key) if key == identity => break,
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            }
            Some((
                Ok::<_, Infallible>(sse::Event::default().event("policy").data("{}")),
                (rx, false, st),
            ))
        });
    let cleaned = crate::remote::server::CleanupStream {
        inner: changes.take_until(close_rx),
        _conn: conn_guard,
    };
    sse::Sse::new(cleaned)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生产判据必须就是编译期事实——不允许出现「运行时可被环境变量/设置打开」的
    /// 旁路（那会让非 macOS 主机意外接入）。
    #[test]
    fn production_platform_predicate_is_macos_only() {
        assert_eq!(native_platform_supported(), cfg!(target_os = "macos"));
        #[cfg(target_os = "macos")]
        assert!(native_platform_supported());
        #[cfg(not(target_os = "macos"))]
        assert!(!native_platform_supported());
    }
}
