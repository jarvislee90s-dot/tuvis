//! ZCode 官方桥的写命令派发（生产计划 T3 · Slice B）。
//!
//! 本文件是官方协议**唯一**的会话写出口：HTTP/前端只能提交 [`ControlAction`]，
//! 信封、clientId、commandId、正文签名都在这里生成，手机无法传入任意 wire。
//!
//! # 单次语义（E3 证据：`SendOnceLoop` @ 固定 SHA）
//!
//! - 一个动作在派发前先经 [`ActionStore::reserve`] 落 `reserved` 行（Slice A；
//!   回执一律映射为保守 `unknown`）：同 actionId 同载荷 → 直接返回既有回执，
//!   **零帧**；载荷不同 → `action_payload_conflict`；
//! - 只有 `New` 才构造**恰一个** `sendConversationCommandV4` 帧；无重发计时器、
//!   无第二次 sendText、无 CLI 降级路径；
//! - ACK 窗 20s（[`ACK_TIMEOUT`]）：该窗口**原样交给端口**，由桥的 RPC 待答条目
//!   按同一期限收口并清账（`not_ready`）；本层**不**再外套超时（外层取消会绕过
//!   桥的 pending 清理）。窗内无应答 → `unknown`（写没写出未知），绝不自动重发；
//!   只有桥**确证零写**的绑定/编码拒绝（`stale_generation` / `no_session` /
//!   `payload_too_large`）才给 `send_failed`（`not_ready` 同时覆盖「就绪闸拒写」
//!   与「写后等应答超窗」，不可区分 → 保守 `unknown`）；
//! - 已 `accepted` 不被无关活动/迟到的失败证据降级（`ActionStore::record_result`
//!   的恰好一次 CAS）。
//!
//! # 正文纪律
//!
//! [`compose_official_text`] 是本路线**唯一**正文出口：保留真实换行/中文/emoji
//! （绝不走终端注入的 `normalize_newlines`，那会把换行变字面 `\n` 两个字符）；
//! 签名开关单点复用 [`message_signature_enabled_conn`]（设置缺省 off），`/` 开头
//! 不加后缀。审计/日志/错误只含类别与计数，不摘正文。
//!
//! # 问答与 snooze（T4.1 Slice B2 + Fix round 1）
//!
//! `Answer` 走「只读幂等预检 → 宿主授权 → 原值映射 → **写前才预留** →（必要时
//! snooze）→ 恰一帧 `resolveInteraction`」：授权/映射失败零帧、零账本行、零审计。
//! `autoResolution` 在场的卡必须先收到 snooze 的 accepted/duplicate/**noop**
//! ACK（noop = 已暂停的幂等回放）**且**卡仍在宿主集合，否则零 resolve。
//! ACK accepted/duplicate 只算 `accepted`（本地消费，**不等于**宿主撤卡），只有
//! ACK 后 10s 内观察到**精确目标**的 pending 代数前进且该 id 消失才升级
//! `confirmed`。`Snooze` 独立动作只暂停计时，不消费卡（结果无论如何都释放本地
//! 在途）。在途闸只在确证零写（send_failed / snooze 失败）、resolve
//! rejected/noop、或宿主撤卡帧（`PendingBook` 整体替换时缺席）时释放；写后
//! unknown 保留闸（绝不自动重发）。答案正文只穿过 wire 与内存，账本只存指纹。
//!
//! # 打断（T4.2 Slice B + Fix round 2）
//!
//! `Stop` 只停「用户点击时新鲜 control 已观察到的既有前台 turn」：`StopPort::session`
//! 取精确目标视图（**拿不到已确认 control 时最多等 [`INITIAL_CONTROL_TIMEOUT`]
//! 10s**——单一 deadline 从首次轮询起算、每次读只拿剩余期限作硬预算；`not_ready`/
//! `offline` 等瞬时错误与 `ready=false` 的旧投影继续轮询；显式 `[]` 立即按空闲
//! 拒绝；窗尽零 stop `not_ready`）→ [`authorize_stop`]（只认 `primaryTurn`，
//! 子 agent/compact 等即使 id 相等也拒绝）→ **写前重取**并复核 session/generation/id →
//! 写前才预留 → 恰一帧 `type=stop`、`payload={expectedForegroundExecutionId}`
//! （[`AGENT_SEND_COMMAND_METHOD`] + 20s ACK 窗）。**永不**为打断合成 `sendText`，
//! 无第二帧、无重试、无 CLI 降级。ACK accepted/duplicate 只算 `accepted`；noop
//! （`guard.stopTargetChanged`）如实显示未停止；没有 accepted ACK 时即使宿主已清空
//! 目标也停在 `unknown`。只有 ACK 后 [`CONFIRM_TIMEOUT`] 内观察到 control revision
//! 前进**且**目标 id 缺席才升级 `confirmed`（新执行单独出现不算；写前视图没有
//! revision 时不做确认，停在 `accepted`）。已受理后宿主终态观察**轮询到期限**
//! （首个「无证据」不是结论），绝不拿兄弟会话/旧代际确认。同 execution 换
//! actionId 的重复点击不做跨 id 去重：第二帧照发，由宿主守卫幂等回 noop 并如实
//! 显示（已知边界，见 `double_tap_new_action_id_defers_to_host_noop`）。
//!
//! # 审批既有卡（T5 + Fix round 1）
//!
//! `Permission` 走「只读幂等预检 → 能力闸 → 宿主授权 → 原值白名单 → 写前预留 →
//! （autoResolution 在场时 snooze）→ 恰一帧 `resolveInteraction`」：已存储的
//! 同 actionId 动作按重放处理，即使闸后来收回也回既有回执；只有**新**动作在
//! `capabilities.approval_reply` **默认关闭**（[`CapabilityView::default`]）时
//! `needs_evidence` 且零帧零行——一次成功的 `sendText` 绝不推断审批可用。只答
//! `kind=permission` 且 option kind ∈ {`allowOnce`,`deny`} 的卡，optionId 原样回传
//! 本卡原值；`fullAccess` 无论 kind、`allowAlways`/常驻/未知 kind 一律
//! `forbidden_option`。autoResolution 卡与问答同款先 snooze（Ok 且卡仍在宿主集合）
//! 再 resolve。ACK accepted/duplicate 只算 `accepted`，10s 内宿主 pending 代数前进
//! 且 id 消失才 `confirmed`；宿主 ACK 前撤卡 → `unknown`。终态收口与 `Answer`
//! 共用 `finish_resolve`（一套回归用例护两侧）。绝不改会话权限模式、绝不用
//! fullAccess、绝无 sendText 合成或重发。
//!
//! # 模型/思考档位（T6 Slice B + Fix round 1）
//!
//! `Selection` 走「只读幂等预检（同 actionId 同载荷 → 既有回执，先于能力闸/
//! 端口/目录读取，重放不受闸/新鲜度/revision 影响）→ 能力闸（`model`/
//! `thought_level` 默认关闭 → `needs_evidence`，零帧且不读目录）→ **新鲜**只读视图 +
//! 官方 `model-selection.getView()` 目录 → [`build_selection_command`]（CAS 必带
//! `baseRevision`、禁 `baseLogEpoch`）→ **写前才预留**（指纹 = kind + 会话 + 代际 +
//! model/thought；`baseRevision` 是执行态、provider 是服务端派生的会话属性，均不入
//! 身份）→ 恰一帧 `type=switchModelConfig`（20s ACK 窗）」。陈旧 CAS（`status=stale` /
//! `reasonCode=*staleRevision*`）→ `not_ready` 刷新提示，**绝不**自动换新 revision
//! 重发；宿主显式拒绝 → `rejected`。ACK accepted/duplicate 只算 `accepted`，
//! [`SELECTION_CONFIRM_TIMEOUT`] 内观察到 revision 前进**且** provider/model/thought
//! 精确等于目标才 `confirmed`。产品保留用户选择，**绝不**回带恢复原值/原模型的写帧，
//! 也绝无 sendText 或第二帧。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::inject::normalize::message_signature_enabled_conn;
use crate::remote::pairing::DeviceStore;
use crate::services::codex_desktop_ipc::types::new_uuid;

use super::action::{action_params, audit_summary, payload_hash, ActionStore, Reservation};
use super::bridge::{BridgeManager, AGENT_CHANNEL};
use super::pending::{build_permission_answer, build_user_answer};
use super::selection::build_selection_command;
use super::types::{
    codes, ActionContext, ActionReceipt, ActionState, ActiveWork, BoundTarget, CapabilityView,
    CommandPort, ControlAction, PendingPort, SelectionPort, SessionView, StopPort, StopRequest,
    ZResult, ZcodeError,
};

/// 官方发送方法名（`zcodeAgent.ts:937`、探针 `SendOnceLoop`）。
pub const AGENT_SEND_COMMAND_METHOD: &str = "sendConversationCommandV4";

/// 信封 `type`（普通文本；`command.ts:95-135` 的 `sendText` 只必带 `text`）。
pub const SEND_TEXT_TYPE: &str = "sendText";

/// 信封 `type`（问答应答；探针 `INTERACTION_COMMAND_TYPES.resolve`，
/// `command.ts:221-234`：payload `{interactionId, answer}`）。
pub const RESOLVE_INTERACTION_TYPE: &str = "resolveInteraction";

/// 信封 `type`（打断；探针 `INTERACTION_COMMAND_TYPES.stop`，
/// `command.ts:161-164`：payload `{expectedForegroundExecutionId}`，守卫 id 必带）。
pub const STOP_TYPE: &str = "stop";

/// ACK 等待窗（20s，与生产计划一致）：原样交给端口（桥的 RPC 待答条目按它收口
/// 并清账），窗内无应答按保守 `unknown`，绝不重发。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);

/// 应答 ACK 后等待宿主撤卡的观察窗（10s，探针 `INTERACTION_TERMINAL_WAIT_MS`）：
/// 只有宿主 pending 代数前进且该 id 消失才升级 `confirmed`，否则停在 `accepted`。
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// 模型/档位切换 ACK 后等待**新鲜**选择的观察窗（10s，生产计划 T6）：ACK
/// accepted/duplicate 只算受理，只有 revision 前进且 provider/model/thought 精确
/// 等于目标才升级 `confirmed`，否则停在 `accepted`。
pub const SELECTION_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// 打断时等待**已确认 control**（`activeWorks` 是数组）到达的有界窗（10s，生产计划
/// T4.2 Step 3）：宿主 control 帧迟到/重订阅窗口内，用户点击最多等到窗前；
/// 超时零 stop、`not_ready`（写没写出不可证 → 上游按保守 `unknown` 收口）。
///
/// 这是**端到端**上界：单一 deadline 从首次轮询起算，单次 `StopPort::session`
/// 读只拿剩余期限作硬预算（生产实现把它分给内部重订阅 RPC，到期 `not_ready`）。
pub const INITIAL_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// 等待已确认 control 的轮询间隔（50ms；有界、不忙等）。
pub const INITIAL_CONTROL_POLL: Duration = Duration::from_millis(50);

/// 单条正文的 UTF-8 字节预算。
///
/// 推导（最坏物理路径）：正文进入消息 JSON 后最坏按 `\u00XX` 转义膨胀 6×
/// （控制字符密集），整条消息随后被 [`super::codec::encode_frame`] Base64 进
/// `dataBase64` 再按 1 MiB 物理信封计量，Base64 再膨胀 4/3 → 合计最坏 8×。
/// 因此预算 = (`MAX_PHYSICAL_FRAME_BYTES` − 8 KiB 固定开销) / 8：**通过本预算的
/// 正文必然能通过编码**（不会出现「预检放行、编码期 `payload_too_large`」）。
/// 超预算在**写帧之前**以 `payload_too_large` 拒绝（零帧零行）。
pub const MAX_TEXT_BYTES: usize =
    (super::codec::MAX_PHYSICAL_FRAME_BYTES.saturating_sub(8_192)) / 8;

/// 生成一次命令 id：rand 128-bit UUIDv4（复用既有 `new_uuid`，不新增依赖）。
pub fn new_command_id() -> String {
    new_uuid()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn invalid_action() -> ZcodeError {
    ZcodeError::of(codes::INVALID_ACTION)
}

/// 组装官方发送正文（本路线**唯一**正文出口）。
///
/// - 纯空白（含只有换行/制表符）→ `invalid_action`（零帧零行）；
/// - 正文原样保留真实换行/中文/emoji，**不做**终端 `normalize_newlines`；
/// - `signature_on` 且正文不以 `/` 开头 → 追加 ` [mobile <设备花名>]`（花名内的
///   换行折叠为空格，避免伪造消息行；花名为空则不追加）；
/// - 组装后超 [`MAX_TEXT_BYTES`] → `payload_too_large`。
pub fn compose_official_text(device_name: &str, text: &str, signature_on: bool) -> ZResult<String> {
    if text.trim().is_empty() {
        return Err(invalid_action());
    }
    let mut composed = String::with_capacity(text.len() + device_name.len() + 12);
    composed.push_str(text);
    if signature_on && !text.starts_with('/') {
        let folded: String = device_name
            .chars()
            .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
            .collect();
        let name = folded.trim();
        if !name.is_empty() {
            composed.push_str(" [mobile ");
            composed.push_str(name);
            composed.push(']');
        }
    }
    if composed.len() > MAX_TEXT_BYTES {
        return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
    }
    Ok(composed)
}

/// stop 唯一可接受的 `ActiveWork.kind`（探针 `interactionBridge.ts` 的
/// `observeStopTarget`：只把 `primaryTurn` 条目当作本轮 turn 的守卫点；前台子
/// agent / compact 等**不是**可停目标，「宁不 stop 不误杀」）。
pub const STOP_TARGET_KIND: &str = "primaryTurn";

/// 该 work 是否是合法的 stop 目标：kind 必须是 [`STOP_TARGET_KIND`] 且带非空
/// `foregroundExecutionId`（探针同款 fail-closed 过滤）。
fn stop_target_id(work: &ActiveWork) -> Option<&str> {
    if work.kind != STOP_TARGET_KIND {
        return None;
    }
    work.foreground_execution_id
        .as_deref()
        .filter(|id| !id.is_empty())
}

/// stop 授权（T4.2 Slice A；**纯函数**，零帧、绝不触发 `sendText`）。
///
/// 打断只允许停止「用户点击时新鲜 control 里**已观察到的既有前台 turn**」：
///
/// - `control = None`（宿主未宣告 `activeWorks` 数组）→ `not_ready`（unknown
///   control；「没看到」绝不等于空闲）；
/// - 显式 `[]` → `stale_pending`（已确认空闲：按钮已过时，零 stop）；
/// - 目标域 = [`STOP_TARGET_KIND`]（`primaryTurn`）且带非空
///   `foregroundExecutionId` 的条目；子 agent / compact / 未知 kind **即使 id
///   精确相等**也拒绝（`stop_target_changed`）——探针 `observeStopTarget` 的
///   fail-closed 规则；
/// - `expected_foreground_execution_id` 必填。缺席（`None`/空串）且可执行目标
///   多于一条 → `target_ambiguous`（绝不随取第一个）；缺席且至多一条 →
///   `invalid_action`（缺少必需的守卫 id）；
/// - 请求 id 必须与某个**可执行目标**的 `foregroundExecutionId` 精确相等，否则
///   `stop_target_changed`（旧按钮/执行已换/非主 turn；宿主守卫会把它当 noop，
///   不能当成功）；
/// - 成功时返回**宿主观察到的原值**，调用方据此填 `expectedForegroundExecutionId`。
pub fn authorize_stop(view: &SessionView, request: &StopRequest) -> ZResult<String> {
    // 未知 control（未宣告 `activeWorks` 数组）：「没看到」不等于空闲，零 stop。
    let Some(control) = view.control.as_ref() else {
        return Err(ZcodeError::of(codes::NOT_READY));
    };
    // 显式 `[]` = 宿主已确认空闲：按钮已过时，零 stop。
    if control.active_works.is_empty() {
        return Err(ZcodeError::of(codes::STALE_PENDING));
    }
    // 目标域：只认 primaryTurn；歧义计数也只数可执行条目（subAgent(None) 等
    // 不能把「唯一主 turn」误判成多目标）。
    let eligible: Vec<&str> = control
        .active_works
        .iter()
        .filter_map(stop_target_id)
        .collect();
    let Some(expected) = request
        .expected_foreground_execution_id
        .as_deref()
        .filter(|id| !id.is_empty())
    else {
        // 守卫 id 必填：多目标缺 id 是歧义（绝不随取第一个）；单目标缺 id 是
        // 缺少必需字段（同样零 stop）。
        return Err(if eligible.len() > 1 {
            ZcodeError::of(codes::TARGET_AMBIGUOUS)
        } else {
            invalid_action()
        });
    };
    // 只接受与某个**可执行目标**精确相等的请求（旧按钮/执行已换/非主 turn →
    // stop_target_changed；宿主守卫会 noop，不能当成功）。
    if !eligible.contains(&expected) {
        return Err(ZcodeError::of(codes::STOP_TARGET_CHANGED));
    }
    Ok(expected.to_owned())
}

/// 命令层 stop 授权：先复核端口返回的视图确属**精确目标**，再走 [`authorize_stop`]。
///
/// 视图不是目标会话/已换代/投影不新鲜（`ready=false`）一律零 stop——「读到的
/// control 不新鲜」绝不等于「control 里没有目标」，也绝不拿兄弟会话视图授权。
fn authorize_stop_on(
    view: &SessionView,
    target: &BoundTarget,
    request: &StopRequest,
) -> ZResult<String> {
    if view.session_id != target.session_id {
        return Err(ZcodeError::of(codes::NO_SESSION));
    }
    if view.generation != target.bridge_generation() {
        return Err(ZcodeError::of(codes::STALE_GENERATION));
    }
    if !view.ready {
        return Err(ZcodeError::of(codes::NOT_READY));
    }
    authorize_stop(view, request)
}

/// ACK 分类（官方 `CommandAck`：`status` ∈ accepted/duplicate/rejected/noop）。
///
/// 只认显式状态：无法解释的应答既不当受理也不谎称拒绝，按保守 `unknown`。
fn classify_ack(ack: &Value) -> (ActionState, Option<String>) {
    match ack.get("status").and_then(Value::as_str) {
        Some("accepted") | Some("duplicate") => (ActionState::Accepted, None),
        Some("noop") => (ActionState::Noop, None),
        Some("rejected") => (ActionState::Rejected, None),
        _ => (ActionState::Unknown, Some(codes::UNKNOWN.to_owned())),
    }
}

/// `switchModelConfig` 的 ACK 分类（官方 `CommandAck.status`；B4 源码口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionAck {
    /// accepted/duplicate：只表示已受理（不等于已生效）。
    Accepted,
    /// CAS 陈旧（`status=stale` 或 `reasonCode` 含 `staleRevision`）：刷新后由用户重试。
    Stale,
    /// 宿主显式拒绝/失败（`rejected`/`failed`）。
    Rejected,
    /// 宿主 noop（如守卫不匹配；不是成功）。
    Noop,
    /// 无法解释的应答：保守未知。
    Unknown,
}

/// `switchModelConfig` ACK 分类：陈旧 CAS 必须先于通用 rejected 识别（B4：CAS
/// `stale` + `proto.staleRevision` 应带 `revisionAtDecision` 由调用方刷新重试）。
fn classify_selection_ack(ack: &Value) -> SelectionAck {
    let status = ack.get("status").and_then(Value::as_str).unwrap_or("");
    let stale_reason = ack
        .get("reasonCode")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason.to_ascii_lowercase().contains("stalerevision"));
    if status == "stale" || stale_reason {
        return SelectionAck::Stale;
    }
    match status {
        "accepted" | "duplicate" => SelectionAck::Accepted,
        "rejected" | "failed" => SelectionAck::Rejected,
        "noop" => SelectionAck::Noop,
        _ => SelectionAck::Unknown,
    }
}

/// 端口错误分类。
///
/// 只有桥**保证零写**的失败（写前绑定/编码拒绝）才是 `send_failed`；
/// `offline`/`not_ready`/`unknown` 可能发生在写帧之后（断网、ACK 超窗、宿主
/// PromiseError），一律按保守 `unknown`——绝不让手机误以为「确定没发出」而重发。
fn classify_port_error(error: &ZcodeError) -> (ActionState, Option<String>) {
    let state = match error.code.as_str() {
        codes::STALE_GENERATION | codes::NO_SESSION | codes::PAYLOAD_TOO_LARGE => {
            ActionState::SendFailed
        }
        _ => ActionState::Unknown,
    };
    (state, Some(error.code.clone()))
}

/// 等待已确认 control 时**可继续轮询**的瞬时读错误。
///
/// 桥在缺口/重订阅窗口内会先 `resync`，而 `resync` 在「订阅成功但快照尚未到」
/// 时返回 `not_ready`，桥暂断时返回 `offline`——两者都不是「没有目标」的结论，
/// 只有窗尽才是。其余错误（`no_session`/`stale_generation`/协议不符等）立即
/// 中止：目标本身已不可信，绝不空转到窗尽。
fn is_transient_stop_read_error(error: &ZcodeError) -> bool {
    matches!(error.code.as_str(), codes::NOT_READY | codes::OFFLINE)
}

/// 一次写动作的派发器（生产：`BridgeManager` + 全局库；测试：fake 端口 + 内存库）。
pub struct CommandSender {
    bridge: Arc<dyn CommandPort>,
    actions: ActionStore,
    store: DeviceStore,
    ack_timeout: Duration,
    /// pending 授权 / snooze / 宿主终态端口：`Answer`/`Snooze`/`Permission` 必需，
    /// 生产接线在 T7.1。
    pending: Option<Arc<dyn PendingPort>>,
    /// stop 授权 / 宿主停止终态端口：`Stop` 必需，生产接线在 T7.1
    /// （`Arc<BridgeManager>`）；未装配时 Stop 一律 `not_ready`（零帧零行）。
    stop: Option<Arc<dyn StopPort>>,
    /// 模型目录 / 选择生效端口：`Selection` 必需，生产接线在 T7.1
    /// （`Arc<BridgeManager>`）；未装配时 Selection 一律 `not_ready`（零帧零行）。
    selection: Option<Arc<dyn SelectionPort>>,
    /// 宿主撤卡观察窗（测试注入短窗；生产 [`CONFIRM_TIMEOUT`]）。
    confirm_timeout: Duration,
    /// 选择生效观察窗（测试注入短窗；生产 [`SELECTION_CONFIRM_TIMEOUT`]）。
    selection_confirm_timeout: Duration,
    /// 未知 control 的有界等待窗（测试注入短窗；生产 [`INITIAL_CONTROL_TIMEOUT`]）。
    initial_control_timeout: Duration,
    /// 未知 control 的轮询间隔（生产 [`INITIAL_CONTROL_POLL`]）。
    initial_control_poll: Duration,
    /// 本派发器装配的能力矩阵（T5 起 `Permission` 的写前闸；默认矩阵全部关闭 =
    /// 审批应答 `needs_evidence`，零帧）。
    ///
    /// **粒度注意**：这是 **sender 级**装配，会话级 DTO 在 `SessionView.capabilities`
    /// （投影侧仍用 [`CapabilityView::default`]）。T7.1 必须按 target 取证据矩阵，
    /// 并同时喂给投影与 sender，避免「手机显示可点、后端 needs_evidence」。
    capabilities: CapabilityView,
}

impl CommandSender {
    /// 生产构造：ACK 窗取 [`ACK_TIMEOUT`]；尚未接 pending 端口（T7.1 用
    /// [`Self::with_pending`] 装配，未装配时 Answer/Snooze 一律 `not_ready`）。
    pub fn new(bridge: Arc<dyn CommandPort>, store: DeviceStore) -> Self {
        Self::with_timeout(bridge, store, ACK_TIMEOUT)
    }

    /// 注入 ACK 窗（测试专用：20s 用例不真等 20s）。
    pub(crate) fn with_timeout(
        bridge: Arc<dyn CommandPort>,
        store: DeviceStore,
        ack_timeout: Duration,
    ) -> Self {
        Self {
            bridge,
            actions: ActionStore::new(store.clone()),
            store,
            ack_timeout,
            pending: None,
            stop: None,
            selection: None,
            confirm_timeout: CONFIRM_TIMEOUT,
            selection_confirm_timeout: SELECTION_CONFIRM_TIMEOUT,
            initial_control_timeout: INITIAL_CONTROL_TIMEOUT,
            initial_control_poll: INITIAL_CONTROL_POLL,
            capabilities: CapabilityView::default(),
        }
    }

    /// 装配本会话能力矩阵（T7.1 生产接线；T5 测试用开闸矩阵）。
    ///
    /// 默认矩阵 = [`CapabilityView::default`]（审批应答关闭），调用方**不得**因
    /// 某次发送成功就推断审批可用——证据面由 T8.2 单独发放。
    pub fn with_capabilities(mut self, capabilities: CapabilityView) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 装配 pending 授权 / snooze / 宿主终态端口（生产 = `BridgeManager`，T7.1 接线）。
    pub fn with_pending(mut self, pending: Arc<dyn PendingPort>) -> Self {
        self.pending = Some(pending);
        self
    }

    /// 装配 stop 授权 / 宿主停止终态端口（生产 = `BridgeManager`，T7.1 接线）。
    pub fn with_stop(mut self, stop: Arc<dyn StopPort>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// 装配模型目录 / 选择生效端口（生产 = `BridgeManager`，T7.1 接线）。
    pub fn with_selection(mut self, selection: Arc<dyn SelectionPort>) -> Self {
        self.selection = Some(selection);
        self
    }

    /// 注入选择生效观察窗（测试专用：10s 用例不真等 10s）。
    #[cfg(test)]
    pub(crate) fn with_selection_confirm_timeout(mut self, timeout: Duration) -> Self {
        self.selection_confirm_timeout = timeout;
        self
    }

    /// 注入宿主撤卡观察窗（测试专用：10s 用例不真等 10s）。
    #[cfg(test)]
    pub(crate) fn with_confirm_timeout(mut self, confirm_timeout: Duration) -> Self {
        self.confirm_timeout = confirm_timeout;
        self
    }

    /// 注入未知 control 的等待窗与轮询间隔（**仅库内单元测试**）。
    ///
    /// 不进入 release 公开面：集成测试用 tokio 虚拟时钟跑**生产**窗
    /// （`start_paused`），无需跨 crate 的测试注入入口。
    #[cfg(test)]
    pub(crate) fn with_initial_control_wait(mut self, timeout: Duration, poll: Duration) -> Self {
        self.initial_control_timeout = timeout;
        self.initial_control_poll = poll;
        self
    }

    /// 执行一次控制动作。
    ///
    /// 已接入 `Send`/`Answer`/`Snooze`/`Stop`/`Permission`/`Selection`；其余变体一律
    /// `invalid_action` 且零帧零行。Answer/Snooze/Stop/Permission/Selection 在未装配
    /// 对应端口时一律 `not_ready`（零帧零行，生产接线在 T7.1）；`Permission` 与
    /// `Selection` 各受能力闸（默认关闭 → `needs_evidence`）。
    pub async fn execute(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        self.execute_with_permits(ctx, target, action, &[]).await
    }

    /// 带**验收许可**的执行入口（T8.2 仅 debug 证据构建调用）。
    ///
    /// `permits` 只含 Hub 已按会话/代际/能力原子消费过的能力名（普通构建恒为空）。
    /// 它只放宽本派发器的**能力闸**一项（`approval_reply` / `model` / `thought_level`）；
    /// 目标、pending 授权、`fullAccess`/常驻 option、stop kind 等守卫全部原样执行。
    pub(crate) async fn execute_with_permits(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
        permits: &[&str],
    ) -> ZResult<ActionReceipt> {
        self.check_action(ctx, target, action)?;
        match action {
            ControlAction::Send { .. } => self.execute_send(ctx, target, action).await,
            ControlAction::Answer { .. } => self.execute_answer(ctx, target, action).await,
            ControlAction::Snooze { .. } => self.execute_snooze(ctx, target, action).await,
            ControlAction::Stop { .. } => self.execute_stop(ctx, target, action).await,
            ControlAction::Permission { .. } => {
                self.execute_permission(ctx, target, action, permits).await
            }
            ControlAction::Selection { .. } => {
                self.execute_selection(ctx, target, action, permits).await
            }
        }
    }

    /// 动作身份校验（所有变体共用）：空 actionId 会成为全局幂等键，必须拒绝。
    fn check_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<()> {
        if ctx.action_id.trim().is_empty() {
            // 空 actionId 会成为全局幂等键（第一个空 id 动作占位，后续一律冲突）。
            return Err(invalid_action());
        }
        if action.session_id() != target.session_id {
            return Err(invalid_action());
        }
        if action.generation() != target.generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        Ok(())
    }

    /// 普通文本发送。
    async fn execute_send(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Send { text, .. } = action else {
            return Err(invalid_action());
        };
        // 正文出口（含空白/预算校验）：校验失败零帧零行，不去碰账本。
        let signature_on = self.store.with(message_signature_enabled_conn);
        let composed = compose_official_text(&ctx.device_name, text, signature_on)?;

        // 幂等指纹只取原始动作参数（不含签名产物）：签名开关变化不改变同一
        // actionId 的语义身份，也绝不因此产生第二次写出。
        let params = action_params(action);
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        // 审计摘要只含类别 + 字数（answer/permission 为答案/选项数），不含正文。
        let summary = audit_summary(action.kind(), &params);
        match self
            .actions
            .reserve_with_kind(ctx, target, action.kind(), &hash, &summary)?
        {
            Reservation::Existing(receipt) => return Ok(receipt),
            Reservation::New => {}
        }

        let receipt = self.dispatch_send(ctx, target, composed).await;
        self.record(&receipt);
        Ok(receipt)
    }

    /// 预留动作（幂等指纹 = kind + 会话 + 代际 + 全部参数，只落指纹不落正文）。
    ///
    /// 同 actionId 同载荷 → 既有回执（调用方直接返回，零帧）；不同载荷/跨设备 → 冲突。
    fn reserve_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        let params = action_params(action);
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        let summary = audit_summary(action.kind(), &params);
        match self
            .actions
            .reserve_with_kind(ctx, target, action.kind(), &hash, &summary)?
        {
            Reservation::Existing(receipt) => Ok(Some(receipt)),
            Reservation::New => Ok(None),
        }
    }

    /// 只读查询既有回执（不写行/审计）：本地校验与授权之前先判幂等重放。
    fn lookup_action(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        self.lookup_by_action(ctx, &target.session_id, target.generation, action)
    }

    /// 只读查询既有回执（**不需要已绑定目标**）：Hub 在连接/能力闸与 bind 之前调用，
    /// 断开/未配对/能力关闭后重试同一 actionId 仍能取回既有 accepted/confirmed/
    /// unknown 回执；冲突错误同样在此直接返回。
    ///
    /// 身份与 [`Self::lookup_action`] **同源**（kind/params/hash 同一派生），会话/代际
    /// 取自动作自身；纯读：零出站帧、零新行、零审计。
    pub(crate) async fn existing_receipt(
        &self,
        ctx: &ActionContext,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        self.lookup_by_action(ctx, action.session_id(), action.generation(), action)
    }

    /// 幂等查询的唯一实现：身份 = actionId + device + session + generation + kind +
    /// payload_hash（由动作参数现算，不读正文）。绑定前后的两个查询入口都走这里，
    /// 避免两套判定口径漂移。
    fn lookup_by_action(
        &self,
        ctx: &ActionContext,
        session_id: &str,
        generation: u64,
        action: &ControlAction,
    ) -> ZResult<Option<ActionReceipt>> {
        let params = action_params(action);
        let hash = payload_hash(action.kind(), session_id, generation, &params)?;
        self.actions
            .existing_receipt_for(ctx, session_id, generation, action.kind(), &hash)
    }

    /// 问答应答：只读幂等预检 → 宿主授权 → 原值映射 → **写前才预留** →
    /// （必要时 snooze）→ 恰一帧 resolve。
    ///
    /// 授权/映射失败发生在预留之前：零帧、零账本行、零审计，actionId 不被占死。
    async fn execute_answer(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Answer {
            interaction_id,
            answers,
            ..
        } = action
        else {
            return Err(invalid_action());
        };
        let Some(pending) = self.pending.as_ref() else {
            // 未装配 pending 端口：不能证明卡在宿主在场，零帧零行。
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        // 幂等重放（同 actionId 同载荷）：只回既有回执，零新行、零帧。
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        // 宿主观察基线（精确目标）：终态要求「该目标代数前进且该 id 不在其集合」。
        let baseline = pending.host_pending_revision(target);
        let card = pending.authorize_pending(target, interaction_id).await?;
        // 原值映射（含 schema 冻结闸/选项集合/重名题干）：失败零帧零行、释放本地在途。
        let answer = match build_user_answer(&card, answers) {
            Ok(answer) => answer,
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        };
        // 写即将发生：此时才预留（审计的 started 只属于真正会离机的动作）。
        match self.reserve_action(ctx, target, action) {
            Ok(None) => {}
            Ok(Some(existing)) => {
                pending.clear_local_inflight(target, interaction_id);
                return Ok(existing);
            }
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        }
        // autoResolution 在场：先 snooze（恰一帧）且必须 Ok（accepted/duplicate/
        // noop=已暂停）且卡仍在宿主集合，否则零 resolve。
        if let Some(receipt) = self
            .snooze_before_resolve(
                ctx,
                target,
                pending.as_ref(),
                interaction_id,
                card.auto_resolution,
            )
            .await
        {
            return Ok(receipt);
        }
        let receipt = self
            .dispatch_resolve(ctx, target, interaction_id, answer)
            .await;
        Ok(self
            .finish_resolve(target, pending.as_ref(), interaction_id, baseline, receipt)
            .await)
    }

    /// 暂停问答卡自动结束计时（独立动作）：只读幂等预检 → 授权 → 写前预留 →
    /// 恰一帧 snooze。不消费卡，结果无论如何都释放本地在途。
    async fn execute_snooze(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Snooze { interaction_id, .. } = action else {
            return Err(invalid_action());
        };
        let Some(pending) = self.pending.as_ref() else {
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        pending.authorize_pending(target, interaction_id).await?;
        match self.reserve_action(ctx, target, action) {
            Ok(None) => {}
            Ok(Some(existing)) => {
                pending.clear_local_inflight(target, interaction_id);
                return Ok(existing);
            }
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        }
        let result = pending
            .snooze(target, interaction_id, &ctx.device_id, self.ack_timeout)
            .await;
        // snooze 不消费卡：用户之后仍要作答，必须放开本地在途闸。
        pending.clear_local_inflight(target, interaction_id);
        match result {
            Ok(()) => {
                let receipt = self.make_receipt(ctx, target, ActionState::Accepted, None);
                self.record(&receipt);
                Ok(receipt)
            }
            Err(error) => {
                // 冻结映射：rejected → invalid_action、超窗 → unknown（noop 已是
                // 幂等成功）；两类都按错误上抛（不是成功回执），账本按可证明性
                // 保守收口。
                let (state, reason_code) = classify_port_error(&error);
                let receipt = self.make_receipt(ctx, target, state, reason_code);
                self.record(&receipt);
                Err(error)
            }
        }
    }

    /// 打断：只停「用户点击时新鲜 control 已观察到的既有前台执行」。
    ///
    /// 顺序：只读幂等预检 → 精确目标 fresh view → [`authorize_stop`] → **写前重取**
    /// 同一目标并复核 session/generation/id → 预留 → 恰一帧 `type=stop`。
    /// 授权失败一律冻结码上抛（零帧零行）；**永不**先合成 `sendText`，无第二帧、
    /// 无重试、无 CLI 降级。
    async fn execute_stop(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Stop {
            expected_foreground_execution_id,
            ..
        } = action
        else {
            return Err(invalid_action());
        };
        let Some(stop) = self.stop.as_ref() else {
            // 未装配 stop 端口：不能证明既有执行在场，零帧零行。
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        // 幂等重放（同 actionId 同载荷）：只回既有回执，零新行、零帧、零重取 control。
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        let request = StopRequest {
            expected_foreground_execution_id: Some(expected_foreground_execution_id.clone()),
        };
        // 新鲜 control（精确目标，必要时由端口只读重同步）：拿不到**已确认**
        // control（`activeWorks` 是数组）时最多等 [`INITIAL_CONTROL_TIMEOUT`]；
        // 超时零帧零行。授权失败同样零帧零行。
        let view = self.wait_for_stop_control(stop.as_ref(), target).await?;
        authorize_stop_on(&view, target, &request)?;
        // 写前立即复核同一目标的 control：按钮已过时（执行换了/空闲/换代）→ 零 stop。
        // 复核读同样有界（端口自持预算，不做取消式超时）。
        let confirm_view = stop.session(target, self.initial_control_timeout).await?;
        let execution_id = authorize_stop_on(&confirm_view, target, &request)?;
        // 确认基线只取写前复核视图的**已知** revision：没有水位就没有因果证据，
        // 绝不把 0 当基线（否则后续任一 revision 都会被误当成「前进」）。
        let baseline_revision = confirm_view.revision;
        // 写即将发生：此时才预留（审计只记类别与计数，守卫 id 不进审计）。
        match self.reserve_action(ctx, target, action)? {
            None => {}
            Some(existing) => return Ok(existing),
        }
        let mut receipt = self.dispatch_stop(ctx, target, &execution_id).await;
        if receipt.state == ActionState::Accepted {
            // 已受理 ≠ 已停止：先落 accepted，再等宿主自己的停止终态（有界 10s）。
            // 基线未知时**不做**确认观察，如实停在 accepted。
            self.record(&receipt);
            if let Some(baseline_revision) = baseline_revision {
                if stop
                    .wait_stop_cleared(
                        target,
                        &execution_id,
                        baseline_revision,
                        self.confirm_timeout,
                    )
                    .await
                {
                    // 只有「control revision 前进 + 目标缺席」才是停止终态；新执行单独出现
                    // 不算（端口保证），超时停在 accepted。
                    receipt.state = ActionState::Confirmed;
                    receipt.reason_code = None;
                }
            }
        }
        self.record(&receipt);
        Ok(receipt)
    }

    /// 审批既有卡的安全应答（T5）：只读幂等预检 → 能力闸 → 宿主授权 →
    /// 原值白名单 → **写前才预留** →（必要时 snooze）→ 恰一帧 `resolveInteraction`。
    ///
    /// 顺序纪律：
    /// - **幂等重放优先于能力闸**：同 actionId 同载荷的已存储动作即使闸已收回也回
    ///   既有回执（零帧零行）；只有**新**动作在 `capabilities.approval_reply`
    ///   （默认关闭）关闭时才 `needs_evidence`、零帧零行零审计（不因一次成功
    ///   sendText 推断审批可用）；
    /// - 卡授权/选项校验失败发生在预留之前：零帧零行，actionId 不被占死；
    /// - 只答 `kind=permission` 且 option kind ∈ {allowOnce, deny} 的卡；
    ///   fullAccess/常驻授权一律 `forbidden_option`（[`build_permission_answer`]）；
    /// - `autoResolution` 在场时与问答同款前置 snooze（决策 §8.2：每个交互 run
    ///   至多一帧 snooze）：必须 Ok（含 noop=已暂停）且卡仍在宿主集合，否则零 resolve；
    /// - ACK accepted/duplicate 只算 `accepted`；ACK 后 [`CONFIRM_TIMEOUT`] 内观察到
    ///   精确目标的宿主 pending 代数前进**且**该 id 消失才升级 `confirmed`；
    ///   宿主在 ACK 前撤卡 → `unknown`；无第二帧、无重发、绝不改会话权限模式。
    async fn execute_permission(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
        permits: &[&str],
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Permission {
            interaction_id,
            option_id,
            ..
        } = action
        else {
            return Err(invalid_action());
        };
        // 幂等重放（同 actionId 同载荷）：只回既有回执，零新行、零帧。先于能力闸，
        // 避免闸收回后把「做过」误报成「没做过」。
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        // 证据闸：审批应答默认关闭，关闭时连 pending 端口都不触碰（零帧）。
        // T8.2 debug 证据构建：Hub 已按会话/代际/能力消费的许可只放宽本闸，
        // 后面的卡授权与 option 白名单（含 fullAccess 拒绝）照常执行。
        if !self.capabilities.approval_reply_enabled() && !permits.contains(&"approval_reply") {
            return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
        }
        let Some(pending) = self.pending.as_ref() else {
            // 未装配 pending 端口：不能证明卡在宿主在场，零帧零行。
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        // 宿主观察基线（精确目标）：终态要求「该目标代数前进且该 id 不在其集合」。
        let baseline = pending.host_pending_revision(target);
        let card = pending.authorize_pending(target, interaction_id).await?;
        // 原值白名单（kind=permission、optionId 属于本卡、非 fullAccess/常驻）：
        // 失败零帧零行、释放本地在途。
        let answer = match build_permission_answer(&card, option_id) {
            Ok(answer) => answer,
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        };
        // 写即将发生：此时才预留（指纹含 interactionId + optionId，不含标签/正文）。
        match self.reserve_action(ctx, target, action) {
            Ok(None) => {}
            Ok(Some(existing)) => {
                pending.clear_local_inflight(target, interaction_id);
                return Ok(existing);
            }
            Err(error) => {
                pending.clear_local_inflight(target, interaction_id);
                return Err(error);
            }
        }
        // autoResolution 在权限卡上同样生效：先暂停计时，未证实零 resolve。
        if let Some(receipt) = self
            .snooze_before_resolve(
                ctx,
                target,
                pending.as_ref(),
                interaction_id,
                card.auto_resolution,
            )
            .await
        {
            return Ok(receipt);
        }

        let receipt = self
            .dispatch_resolve(ctx, target, interaction_id, answer)
            .await;
        Ok(self
            .finish_resolve(target, pending.as_ref(), interaction_id, baseline, receipt)
            .await)
    }

    /// 模型/思考档位切换（T6）：只读幂等预检 → 能力闸 → 新鲜选择视图 → 纯构造器 →
    /// 写前预留 → 恰一帧 `switchModelConfig`（CAS 只带 baseRevision）→ ACK 受理后等新鲜
    /// 选择确认。
    ///
    /// 纪律：
    /// - **幂等重放优先于能力闸/端口/目录读取**：同 actionId 同载荷（kind + 会话 +
    ///   代际 + model/thought）的已存储动作直接回既有回执（零帧零行），即使闸已收回、
    ///   端口未装配、目录暂不可读或宿主 revision 已前进也如此——绝不把「做过」误报成
    ///   「没做过」；
    /// - `model`/`thought_level` 闸默认关闭（`needs_evidence`，零帧零行、不读目录）；
    ///   provider 只取会话视图，绝不接受客户端指定；
    /// - 幂等指纹**不含** `baseRevision`（CAS 水位是执行态，不是用户载荷；切换生效后
    ///   revision 前进，含它会把同 actionId 重试误判成载荷冲突），也不含服务端派生的
    ///   provider（会话归属即 provider，重放时无从在读取目录前取得）；
    /// - 陈旧 CAS（宿主 `status=stale` / `reasonCode=*staleRevision*`）→ `not_ready`
    ///   （刷新后由用户重试），**绝不**自动换新 revision 重发；宿主显式拒绝 →
    ///   `rejected`；两者都不进入确认观察；
    /// - ACK accepted/duplicate 只算 `accepted`；[`SELECTION_CONFIRM_TIMEOUT`] 内观察到
    ///   revision 前进**且** provider/model/thought 精确等于目标才 `confirmed`；
    /// - 产品保留用户选择：绝不回带「恢复原 thought/原模型」的第二帧。
    async fn execute_selection(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        action: &ControlAction,
        permits: &[&str],
    ) -> ZResult<ActionReceipt> {
        let ControlAction::Selection { model, thought, .. } = action else {
            return Err(invalid_action());
        };
        // 幂等重放（同 actionId 同载荷）：只回既有回执，零新行、零帧、零目录读取。
        // 先于能力闸/端口/load：闸收回、断网重启、revision 前进都不得吞掉既有回执。
        if let Some(existing) = self.lookup_action(ctx, target, action)? {
            return Ok(existing);
        }
        // 证据闸：切换能力默认关闭，关闭时连目录都不读（零帧零行零审计）。
        // T8.2 debug 证据构建：Hub 已消费的许可只放宽本闸（每个关闭项各需一条许可）。
        let model_open = self.capabilities.model.enabled || permits.contains(&"model");
        let thought_open =
            self.capabilities.thought_level.enabled || permits.contains(&"thought_level");
        if !model_open || !thought_open {
            return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
        }
        let Some(port) = self.selection.as_ref() else {
            // 未装配选择端口：不能证明目录/新鲜视图在场，零帧零行。
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        // 写前新鲜视图 + 当前 provider 目录（端口只读；绝不接受客户端 provider）。
        let (mut view, selection) = port.load(target).await?;
        if view.session_id != target.session_id {
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        if view.generation != target.bridge_generation() {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        if !view.ready {
            return Err(ZcodeError::of(codes::NOT_READY));
        }
        let provider = selection.provider.clone();
        // 选择视图随新鲜视图一起交给构造器：CAS revision 必填、模型/档位必须属于
        // **当前 provider 目录**（空档位/他模型档位/未知值 → invalid_action）。
        view.selection = Some(selection);
        let command = build_selection_command(&view, model, thought)?;
        let base_revision = view
            .revision
            .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
        // 写即将发生：此时才预留。指纹只覆盖稳定意图（kind + 会话 + 代际 +
        // model/thought），与 `lookup_action`/`action_params` 同一口径：`baseRevision`
        // 是 CAS 执行态（成功后必变），provider 是服务端派生的会话属性，二者都不参与
        // 身份，否则同 actionId 重试会落成 `action_payload_conflict`。审计摘要只记类别。
        let params = serde_json::json!({ "model": model, "thought": thought });
        let hash = payload_hash(
            action.kind(),
            &target.session_id,
            target.generation,
            &params,
        )?;
        let summary = audit_summary(action.kind(), &params);
        match self
            .actions
            .reserve_with_kind(ctx, target, action.kind(), &hash, &summary)?
        {
            Reservation::Existing(receipt) => return Ok(receipt),
            Reservation::New => {}
        }
        // 恰一帧：信封 = 纯构造器产物（type/payload/baseRevision，无 baseLogEpoch）
        // + 可信上下文的 commandId/clientId/sessionId/issuedAt。
        let mut envelope = command;
        let Some(object) = envelope.as_object_mut() else {
            return Err(invalid_action());
        };
        object.insert("commandId".to_owned(), Value::from(new_command_id()));
        object.insert("clientId".to_owned(), Value::from(ctx.device_id.clone()));
        object.insert(
            "sessionId".to_owned(),
            Value::from(target.session_id.clone()),
        );
        object.insert("issuedAt".to_owned(), Value::from(now_ms()));

        let mut receipt = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => {
                let (state, reason_code) = classify_port_error(&error);
                self.make_receipt(ctx, target, state, reason_code)
            }
            Ok(ack) => match classify_selection_ack(&ack) {
                SelectionAck::Accepted => {
                    self.make_receipt(ctx, target, ActionState::Accepted, None)
                }
                SelectionAck::Noop => self.make_receipt(ctx, target, ActionState::Noop, None),
                SelectionAck::Rejected => {
                    self.make_receipt(ctx, target, ActionState::Rejected, None)
                }
                SelectionAck::Unknown => self.make_receipt(
                    ctx,
                    target,
                    ActionState::Unknown,
                    Some(codes::UNKNOWN.to_owned()),
                ),
                SelectionAck::Stale => {
                    // 陈旧 CAS：命令已写出但被宿主拒绝（零生效）。如实落账、向手机
                    // 返回刷新提示；**绝不**换新 revision 自动重发。
                    let receipt = self.make_receipt(
                        ctx,
                        target,
                        ActionState::Rejected,
                        Some(codes::NOT_READY.to_owned()),
                    );
                    self.record(&receipt);
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
            },
        };
        if receipt.state == ActionState::Accepted {
            // 已受理 ≠ 已生效：先落 accepted，再等新鲜选择（revision 前进 + 三项精确）。
            self.record(&receipt);
            if port
                .wait_selection(
                    target,
                    &provider,
                    model,
                    thought,
                    base_revision,
                    self.selection_confirm_timeout,
                )
                .await
            {
                receipt.state = ActionState::Confirmed;
                receipt.reason_code = None;
            }
        }
        self.record(&receipt);
        Ok(receipt)
    }

    /// autoResolution 前置（答案与权限共用）：恰一帧 snooze，必须 Ok
    /// （accepted/duplicate/**noop** = 已暂停）**且**卡仍在宿主集合，否则零 resolve。
    ///
    /// 返回 `Some(receipt)` = 已落账的保守回执（调用方直接返回）；`None` = 计时已
    /// 暂停且卡仍在，可以 resolve。
    async fn snooze_before_resolve(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        pending: &dyn PendingPort,
        interaction_id: &str,
        auto_resolution: bool,
    ) -> Option<ActionReceipt> {
        if !auto_resolution {
            return None;
        }
        if let Err(error) = pending
            .snooze(target, interaction_id, &ctx.device_id, self.ack_timeout)
            .await
        {
            // resolve 从未写出（可证明零写）：释放在途允许重试；snooze 本身幂等
            // （重复/迟到 noop 属已暂停），不会造成双暂停副作用。
            pending.clear_local_inflight(target, interaction_id);
            let (state, reason_code) = classify_port_error(&error);
            let receipt = self.make_receipt(ctx, target, state, reason_code);
            self.record(&receipt);
            return Some(receipt);
        }
        if !pending.host_has_pending(target, interaction_id) {
            // snooze 已写出但宿主集合已无此卡：不猜是否受理，零 resolve。
            pending.clear_local_inflight(target, interaction_id);
            let receipt = self.make_receipt(
                ctx,
                target,
                ActionState::Unknown,
                Some(codes::STALE_PENDING.to_owned()),
            );
            self.record(&receipt);
            return Some(receipt);
        }
        None
    }

    /// resolve 之后的共享收口（答案与权限同一状态机：回归共用一套用例）。
    ///
    /// - 写后非 `accepted` 且宿主已撤卡 → `unknown/stale_pending`（绝不谎称受理）；
    /// - `accepted` → 本地消费并落账，[`CONFIRM_TIMEOUT`] 内观察到宿主撤卡才
    ///   `confirmed`，超时停在 `accepted`；
    /// - `send_failed`（桥保证零写）与 `rejected`/`noop`（可证明未生效）释放在途；
    ///   写后 `unknown`（含 ACK 超窗）保持在途闸，防重复审批。
    async fn finish_resolve(
        &self,
        target: &BoundTarget,
        pending: &dyn PendingPort,
        interaction_id: &str,
        baseline: u64,
        mut receipt: ActionReceipt,
    ) -> ActionReceipt {
        if receipt.state != ActionState::Accepted
            && !pending.host_has_pending(target, interaction_id)
        {
            // 宿主在 ACK 前已撤卡且没有 accepted ACK：不猜受理，保守 unknown。
            receipt.state = ActionState::Unknown;
            receipt.reason_code = Some(codes::STALE_PENDING.to_owned());
        }
        if receipt.state == ActionState::Accepted {
            // 本地消费 ≠ 宿主撤卡：先收口在途，再等宿主自己的整体替换。
            pending.mark_resolved_local(target, interaction_id);
            self.record(&receipt);
            if pending
                .wait_host_cleared(target, interaction_id, baseline, self.confirm_timeout)
                .await
            {
                // 只有「代数前进 + 卡消失」才是因果终态；超时停在 accepted。
                receipt.state = ActionState::Confirmed;
                receipt.reason_code = None;
            }
        } else if receipt.state == ActionState::SendFailed {
            // 确证零写：释放本地在途，用户可重试同一张卡。
            pending.clear_local_inflight(target, interaction_id);
        } else if matches!(receipt.state, ActionState::Rejected | ActionState::Noop) {
            // 宿主显式拒绝/无需处理且卡仍在：本次应答可证明未生效，放开重试；
            // 写后 unknown（含 ACK 超窗）保持闸（防双目重复作答），只由宿主
            // 后续撤卡帧或换代释放。
            pending.clear_local_inflight(target, interaction_id);
        }
        self.record(&receipt);
        receipt
    }

    /// 等**已确认**的 control（`activeWorks` 是数组）到达：宿主 control 帧迟到/
    /// 重订阅窗口内最多等 `initial_control_timeout`（生产 [`INITIAL_CONTROL_TIMEOUT`]）。
    ///
    /// 这是**端到端**上界：单一 deadline 从首次轮询起算，每次 `StopPort::session`
    /// 读都只拿到**剩余期限**作为硬预算（生产实现把预算分给内部重订阅 RPC；不取消
    /// future、不留下未激活订阅），因此 10s 窗不会被单次读拖成 ~30s。
    ///
    /// - 已确认且**新鲜**（`ready=true` 且 `activeWorks` 是数组，含显式 `[]`）→
    ///   立即返回视图（`[]` 由授权层给 `stale_pending`；`ready=false` 的旧投影
    ///   不算「已确认」）；
    /// - 瞬时不可读（`not_ready`/`offline`）不是结论：继续轮询到期限；
    /// - 目标域失配（视图不再是精确目标：兄弟会话/旧代际）→ 立即
    ///   `no_session` / `stale_generation`，绝不空转到窗尽、也绝不拿它授权；
    /// - 窗尽 → `not_ready`（零 stop；上游按保守 `unknown` 收口）。
    ///
    /// 轮询间隔 ≤ `initial_control_poll`（有界、不忙等）。
    async fn wait_for_stop_control(
        &self,
        stop: &dyn StopPort,
        target: &BoundTarget,
    ) -> ZResult<SessionView> {
        let deadline = tokio::time::Instant::now()
            .checked_add(self.initial_control_timeout)
            .unwrap_or_else(tokio::time::Instant::now);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            // 单次读只拿剩余预算（端口契约：到期以 not_ready 收口）；读挂住也不得
            // 越过总窗，且不靠取消 future 收口（那会绕过桥的重订阅清理）。
            match stop.session(target, remaining).await {
                Err(error) if is_transient_stop_read_error(&error) => {
                    // 重订阅窗口/桥暂离线不是结论：继续轮询到期限。
                }
                Err(error) => return Err(error),
                Ok(view) => {
                    // 端口契约保证视图属于精确目标；仍在此复核，绝不拿兄弟/旧代际视图
                    // 等待或授权。
                    if view.session_id != target.session_id {
                        return Err(ZcodeError::of(codes::NO_SESSION));
                    }
                    if view.generation != target.bridge_generation() {
                        return Err(ZcodeError::of(codes::STALE_GENERATION));
                    }
                    // `ready=false` 但 control 仍留在投影里的旧视图不算已确认：
                    // 授权层会按 not_ready 拒绝，等待窗不能被它提前花掉。
                    if view.ready && view.control.is_some() {
                        return Ok(view);
                    }
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::time::sleep(self.initial_control_poll.min(remaining)).await;
        }
    }

    /// 构造并**恰一次**调用 resolveInteraction 端口（channel/method/信封与探针一致）。
    async fn dispatch_resolve(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        interaction_id: &str,
        answer: Value,
    ) -> ActionReceipt {
        let envelope = serde_json::json!({
            "commandId": new_command_id(),
            // clientId 只来自可信调用方上下文（配对门/本机 DB）。
            "clientId": ctx.device_id,
            "sessionId": target.session_id,
            "type": RESOLVE_INTERACTION_TYPE,
            "payload": { "interactionId": interaction_id, "answer": answer },
            "issuedAt": now_ms(),
        });
        let (state, reason_code) = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => classify_port_error(&error),
            Ok(ack) => classify_ack(&ack),
        };
        self.make_receipt(ctx, target, state, reason_code)
    }

    /// 构造并**恰一次**调用 stop 端口（channel/method/信封与探针 `InteractionCommandLoop`
    /// 同形）：`type=stop`、`payload={expectedForegroundExecutionId}`，无重发、无
    /// `sendText` 合成、无 CLI 降级。
    async fn dispatch_stop(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        execution_id: &str,
    ) -> ActionReceipt {
        let envelope = serde_json::json!({
            "commandId": new_command_id(),
            // clientId 只来自可信调用方上下文（配对门/本机 DB）。
            "clientId": ctx.device_id,
            "sessionId": target.session_id,
            "type": STOP_TYPE,
            "payload": { "expectedForegroundExecutionId": execution_id },
            "issuedAt": now_ms(),
        });
        let (state, reason_code) = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => classify_port_error(&error),
            Ok(ack) => classify_ack(&ack),
        };
        self.make_receipt(ctx, target, state, reason_code)
    }

    /// 组装回执（actionId/session/generation 只来自可信上下文）。
    fn make_receipt(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        state: ActionState,
        reason_code: Option<String>,
    ) -> ActionReceipt {
        ActionReceipt {
            action_id: ctx.action_id.clone(),
            session_id: target.session_id.clone(),
            generation: target.generation,
            state,
            reason_code,
        }
    }

    /// 落账回执；失败时只记错误码（不得对手机谎报失败）。
    fn record(&self, receipt: &ActionReceipt) {
        if let Err(error) = self.actions.record_result(receipt) {
            log::error!("zcode_actions: receipt not persisted: {error:?}");
        }
    }

    /// 构造信封并**恰一次**调用发送端口（无重发、无第二次 sendText、无 CLI 降级）。
    ///
    /// 超时由端口拥有：`ack_timeout` 原样传入，桥按它给 RPC 待答条目设期限并在超窗时
    /// 自行清账；这里**不**外套 `tokio::time::timeout`（取消 future 会绕过桥的
    /// `unregister_rpc`，留下永不回收的 pending waiter）。
    async fn dispatch_send(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        text: String,
    ) -> ActionReceipt {
        let envelope = serde_json::json!({
            "commandId": new_command_id(),
            // clientId 只来自可信调用方上下文（配对门/本机 DB），绝不取手机任意字段。
            "clientId": ctx.device_id,
            "sessionId": target.session_id,
            "type": SEND_TEXT_TYPE,
            "payload": { "text": text },
            "issuedAt": now_ms(),
        });
        let (state, reason_code) = match self
            .bridge
            .call(
                target,
                AGENT_CHANNEL,
                AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                self.ack_timeout,
            )
            .await
        {
            Err(error) => classify_port_error(&error),
            Ok(ack) => classify_ack(&ack),
        };
        ActionReceipt {
            action_id: ctx.action_id.clone(),
            session_id: target.session_id.clone(),
            generation: target.generation,
            state,
            reason_code,
        }
    }
}

// ============================================================
// 生产端口：BridgeManager
// ============================================================

impl CommandPort for BridgeManager {
    /// 在绑定会话的桥上发一次 RPC。
    ///
    /// 调用方传入 `args = [envelope]`；本适配层按 E3 合同把它包成官方 params
    /// `{workspacePath, workspaceIdentity?, envelope}`（探针 `SendOnceLoop` 同形），
    /// workspace 归属只来自 [`BoundTarget`]（精确 listing 产物），不接受手机自报。
    fn call<'a>(
        &'a self,
        target: &'a BoundTarget,
        channel: &'a str,
        method: &'a str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<Value>> {
        Box::pin(async move {
            let mut args = args.into_iter();
            let envelope = args
                .next()
                .ok_or_else(|| ZcodeError::of(codes::INVALID_ACTION))?;
            if args.next().is_some() {
                // 信封之外的多余参数一律拒绝（HTTP 不得注入任意 wire 字段）。
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
            let mut params = Map::new();
            params.insert(
                "workspacePath".to_owned(),
                Value::from(target.workspace_path.clone()),
            );
            if let Some(identity) = &target.workspace_identity {
                params.insert(
                    "workspaceIdentity".to_owned(),
                    Value::from(identity.clone()),
                );
            }
            params.insert("envelope".to_owned(), envelope);
            // ACK 窗原样交给桥的 RPC 待答条目：生产 20s 由桥自己的期限收口并清账。
            BridgeManager::call_with_timeout(
                self,
                target,
                channel,
                method,
                vec![Value::Object(params)],
                timeout,
            )
            .await
        })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Semaphore;

    use crate::database::dao::settings;
    use crate::services::zcode_control::types::AnswerEntry;

    const SESSION: &str = "sess_0123456789abcdef0123456789abcdef";
    const GEN: u64 = 7;
    const DEVICE: &str = "phone-1";

    fn target() -> BoundTarget {
        BoundTarget {
            session_id: SESSION.to_string(),
            workspace_path: "/tmp/ws".to_string(),
            workspace_identity: Some("ws-identity".to_string()),
            workspace_key: "ws-identity".to_string(),
            generation: GEN,
            bridge_generation: None,
        }
    }

    fn ctx(action_id: &str) -> ActionContext {
        ActionContext {
            action_id: action_id.to_string(),
            device_id: DEVICE.to_string(),
            device_name: "iPhone".to_string(),
        }
    }

    fn send_action(text: &str) -> ControlAction {
        send_action_at(GEN, text)
    }

    fn send_action_at(generation: u64, text: &str) -> ControlAction {
        ControlAction::Send {
            session_id: SESSION.to_string(),
            generation,
            text: text.to_string(),
        }
    }

    fn accepted_body() -> Value {
        serde_json::json!({ "status": "accepted" })
    }

    /// 一次出站帧记录（测试观测：channel/method/args 原样）。
    #[derive(Debug, Clone)]
    struct Frame {
        target: BoundTarget,
        channel: String,
        method: String,
        args: Vec<Value>,
    }

    /// 脚本化应答。
    #[derive(Debug, Clone)]
    enum Reply {
        Body(Value),
        Error(&'static str),
        /// 桥自己的 RPC 待答窗到期（写后无应答/ACK 丢失）：按真实桥口径在窗到期时
        /// 返回 `not_ready`（并自行清账），调用方只能靠这个期限收口。
        AckLost,
        /// 在应答窗的 `num/den` 比例后返回成功（模拟 15s/20s 的宿主 accepted）。
        BodyAfter {
            body: Value,
            num: u32,
            den: u32,
        },
        /// 在应答窗的 `num/den` 比例后返回端口错误（模拟桥内期限先到的竞态）。
        ErrorAfter {
            code: &'static str,
            num: u32,
            den: u32,
        },
    }

    /// 在途 future 的取消探针：只有 future 被 **drop**（而非正常返回）才会置位。
    struct CancelProbe<'a> {
        flag: &'a AtomicBool,
        armed: bool,
    }

    impl<'a> CancelProbe<'a> {
        fn new(flag: &'a AtomicBool) -> Self {
            Self { flag, armed: true }
        }

        fn disarm(mut self) {
            self.armed = false;
        }
    }

    impl Drop for CancelProbe<'_> {
        fn drop(&mut self) {
            if self.armed {
                self.flag.store(true, Ordering::SeqCst);
            }
        }
    }

    struct FakeBridge {
        frames: Mutex<Vec<Frame>>,
        reply: Mutex<Reply>,
        /// 按次应答队列（双击等并发场景：先到者取队首，空了回落到 `reply`）。
        queued_replies: Mutex<VecDeque<Reply>>,
        wire: AtomicUsize,
        /// 每记录一帧放行一个 permit（测试等待「帧已写出」）。
        recorded: Semaphore,
        /// 门控模式：记录后等 release permit 才应答。
        release: Semaphore,
        gated: bool,
        /// 每次调用收到的应答窗（断言「窗口原样交给端口」）。
        observed_timeouts: Mutex<Vec<Duration>>,
        /// 在途 future 被调用方取消过（生产上等价于桥 pending 泄漏）。
        cancelled: AtomicBool,
    }

    impl FakeBridge {
        fn new(reply: Reply) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Vec::new()),
                reply: Mutex::new(reply),
                queued_replies: Mutex::new(VecDeque::new()),
                wire: AtomicUsize::new(0),
                recorded: Semaphore::new(0),
                release: Semaphore::new(0),
                gated: false,
                observed_timeouts: Mutex::new(Vec::new()),
                cancelled: AtomicBool::new(false),
            })
        }

        fn gated() -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Vec::new()),
                reply: Mutex::new(Reply::Body(accepted_body())),
                queued_replies: Mutex::new(VecDeque::new()),
                wire: AtomicUsize::new(0),
                recorded: Semaphore::new(0),
                release: Semaphore::new(0),
                gated: true,
                observed_timeouts: Mutex::new(Vec::new()),
                cancelled: AtomicBool::new(false),
            })
        }

        fn set_reply(&self, reply: Reply) {
            *self.reply.lock().unwrap() = reply;
        }

        /// 排一次应答（先到先得；队列空时回落到 [`Self::reply`]）。
        fn queue_reply(&self, reply: Reply) {
            self.queued_replies.lock().unwrap().push_back(reply);
        }

        fn wire(&self) -> usize {
            self.wire.load(Ordering::SeqCst)
        }

        fn frames(&self) -> Vec<Frame> {
            self.frames.lock().unwrap().clone()
        }

        fn envelope(&self, index: usize) -> Value {
            self.frames()[index].args[0].clone()
        }

        fn observed_timeouts(&self) -> Vec<Duration> {
            self.observed_timeouts.lock().unwrap().clone()
        }

        fn cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    impl CommandPort for FakeBridge {
        fn call<'a>(
            &'a self,
            target: &'a BoundTarget,
            channel: &'a str,
            method: &'a str,
            args: Vec<Value>,
            timeout: Duration,
        ) -> BoxFuture<'a, ZResult<Value>> {
            Box::pin(async move {
                self.observed_timeouts.lock().unwrap().push(timeout);
                {
                    self.frames.lock().unwrap().push(Frame {
                        target: target.clone(),
                        channel: channel.to_owned(),
                        method: method.to_owned(),
                        args,
                    });
                    self.wire.fetch_add(1, Ordering::SeqCst);
                    self.recorded.add_permits(1);
                }
                if self.gated {
                    let _ = self.release.acquire().await;
                }
                let probe = CancelProbe::new(&self.cancelled);
                let reply = self
                    .queued_replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| self.reply.lock().unwrap().clone());
                let scale = |num: u32, den: u32| timeout.mul_f64(f64::from(num) / f64::from(den));
                let outcome = match reply {
                    Reply::Body(body) => Ok(body),
                    Reply::Error(code) => Err(ZcodeError::of(code)),
                    Reply::AckLost => {
                        tokio::time::sleep(timeout).await;
                        Err(ZcodeError::of(codes::NOT_READY))
                    }
                    Reply::BodyAfter { body, num, den } => {
                        tokio::time::sleep(scale(num, den)).await;
                        Ok(body)
                    }
                    Reply::ErrorAfter { code, num, den } => {
                        tokio::time::sleep(scale(num, den)).await;
                        Err(ZcodeError::of(code))
                    }
                };
                probe.disarm();
                outcome
            })
        }
    }

    fn sender(fake: &Arc<FakeBridge>, store: &DeviceStore) -> CommandSender {
        CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
    }

    fn row_count(store: &DeviceStore) -> i64 {
        store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM zcode_actions", [], |r| r.get(0))
                .unwrap()
        })
    }

    fn audits(store: &DeviceStore) -> Vec<crate::database::dao::write_audit::AuditRow> {
        store.with(|c| crate::database::dao::write_audit::recent_conn(c, 100))
    }

    /// 同 actionId 同载荷：第二次只返回既有回执，线上恰一帧。
    #[tokio::test]
    async fn same_action_id_same_payload_returns_receipt_without_resend() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("你好\n第二行"));

        let first = sender.execute(&ctx, &target(), &action).await.unwrap();
        let second = sender.execute(&ctx, &target(), &action).await.unwrap();

        let wire_send_text_count = fake.wire();
        assert_eq!(wire_send_text_count, 1);
        assert_eq!(second.action_id, first.action_id);
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(second.state, ActionState::Accepted);
        assert_eq!(second.session_id, SESSION);
        assert_eq!(second.generation, GEN);
        assert_eq!(row_count(&store), 1);
    }

    /// 生产只读查询路径（Hub 在连接/许可闸之前依赖的真实实现，不是 Hub fake）：
    /// 账本已有 accepted/unknown 时 `CommandSender::existing_receipt` 原样取回；
    /// 全程零出站帧、零新行、零新审计；跨设备/换载荷/换代际一律冲突。
    #[tokio::test]
    async fn existing_receipt_reads_ledger_without_frames_or_writes() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        // 无行 → None，零帧零行（查询绝不写入）。
        assert!(sender
            .existing_receipt(&ctx("a-none"), &send_action("从未提交"))
            .await
            .unwrap()
            .is_none());
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);

        // accepted：真实派发路径（reserve + record_result）落库，恰一帧。
        let accepted_action = send_action("已经被接下");
        let accepted = sender
            .execute(&ctx("a-accepted"), &target(), &accepted_action)
            .await
            .unwrap();
        assert_eq!(accepted.state, ActionState::Accepted);
        assert_eq!(fake.wire(), 1);

        // unknown：以真实账本 API 预留并落未知终态（ACK 窗尽/断网的生产收口）。
        let unknown_action = send_action("结果未知");
        let params = action_params(&unknown_action);
        let hash = payload_hash(unknown_action.kind(), SESSION, GEN, &params).unwrap();
        let summary = audit_summary(unknown_action.kind(), &params);
        assert_eq!(
            sender
                .actions
                .reserve_with_kind(
                    &ctx("a-unknown"),
                    &target(),
                    unknown_action.kind(),
                    &hash,
                    &summary,
                )
                .unwrap(),
            Reservation::New
        );
        let unknown = ActionReceipt {
            action_id: "a-unknown".into(),
            session_id: SESSION.into(),
            generation: GEN,
            state: ActionState::Unknown,
            reason_code: Some(codes::OFFLINE.into()),
        };
        sender.actions.record_result(&unknown).unwrap();

        let rows_before = row_count(&store);
        let audits_before = audits(&store).len();
        let wire_before = fake.wire();

        // 两种终态都能取回；查询本身零帧、零新行、零新审计。
        for (action_id, action, expected) in [
            ("a-accepted", &accepted_action, &accepted),
            ("a-unknown", &unknown_action, &unknown),
        ] {
            let got = sender
                .existing_receipt(&ctx(action_id), action)
                .await
                .unwrap()
                .expect("账本已有行必须命中");
            assert_eq!(&got, expected, "{action_id} 必须原样取回");
        }
        assert_eq!(fake.wire(), wire_before, "只读查询绝不产生出站帧");
        assert_eq!(row_count(&store), rows_before, "查询绝不新增行");
        assert_eq!(audits(&store).len(), audits_before, "查询绝不新增审计");

        // 同 id 换载荷 → 冲突（绝不返回旧回执）。
        let err = sender
            .existing_receipt(&ctx("a-accepted"), &send_action("换了正文"))
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ACTION_PAYLOAD_CONFLICT);
        // 跨设备同 id → 冲突（不泄漏别的设备的回执）。
        let other_device = ActionContext {
            action_id: "a-accepted".into(),
            device_id: "phone-2".into(),
            device_name: "iPad".into(),
        };
        let err = sender
            .existing_receipt(&other_device, &accepted_action)
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ACTION_PAYLOAD_CONFLICT);
        // 换代际同 id → 冲突（绝不返回旧代际回执）。
        let err = sender
            .existing_receipt(&ctx("a-accepted"), &send_action_at(GEN + 1, "已经被接下"))
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ACTION_PAYLOAD_CONFLICT);
        assert_eq!(fake.wire(), wire_before, "冲突判定同样零帧");
        assert_eq!(row_count(&store), rows_before);
        assert_eq!(audits(&store).len(), audits_before);
    }

    /// Hub 盖好的精确目标：公开代际（Hub）用于校验/落账，桥投影代际只给桥内部。
    /// 真实 ActionStore/CommandSender + fake CommandPort：若 sender 仍用桥代际对拍，本用例必红。
    #[tokio::test]
    async fn real_sender_accepts_hub_stamped_target() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let hub_generation = GEN + 100;
        let mut target = target();
        target.bridge_generation = Some(GEN);
        target.generation = hub_generation;

        let receipt = sender
            .execute(
                &ctx("a-hub-stamped"),
                &target,
                &send_action_at(hub_generation, "合成正文"),
            )
            .await
            .expect("公开代际必须通过 sender 校验");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(receipt.generation, hub_generation, "回执盖公开代际");
        assert_eq!(fake.wire(), 1);

        let stored_generation: i64 = store.with(|c| {
            c.query_row("SELECT generation FROM zcode_actions", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(stored_generation, hub_generation as i64, "账本存公开代际");

        // 反证：动作按桥代际盖章 → 旧代际拒绝，零第二帧。
        let err = sender
            .execute(
                &ctx("a-bridge-stamped"),
                &target,
                &send_action_at(GEN, "合成正文"),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.wire(), 1, "旧代际不得产生第二帧");
    }

    /// 同 actionId 换正文：action_payload_conflict，零第二帧、零新行。
    #[tokio::test]
    async fn action_id_reused_with_different_body_is_conflict() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        let ctx = ctx("a-1");
        sender
            .execute(&ctx, &target(), &send_action("第一段"))
            .await
            .unwrap();

        let reused_id_error = sender
            .execute(&ctx, &target(), &send_action("第二段"))
            .await
            .unwrap_err();

        assert_eq!(reused_id_error.code, "action_payload_conflict");
        assert_eq!(fake.wire(), 1, "冲突不得产生第二帧");
        assert_eq!(row_count(&store), 1);
    }

    /// 写后 ACK 丢失：保守 unknown，重放同 id 零帧。
    #[tokio::test]
    async fn dropped_ack_after_write_is_unknown_and_never_resent() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::AckLost);
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("写后断网"));

        let lost_ack = sender.execute(&ctx, &target(), &action).await.unwrap();
        let before_retry = fake.wire();
        let retry = sender.execute(&ctx, &target(), &action).await.unwrap();

        assert_eq!(lost_ack.state, ActionState::Unknown);
        assert_eq!(retry.state, ActionState::Unknown);
        let retry_wire_count = fake.wire() - before_retry;
        assert_eq!(retry_wire_count, 0);
        assert_eq!(fake.wire(), 1, "全生命周期只允许一帧");
    }

    /// 审计表不可写：预留整体回滚，零命令帧。
    #[tokio::test]
    async fn audit_storage_failure_is_zero_write() {
        let store = DeviceStore::memory();
        store.with(|c| c.execute_batch("DROP TABLE write_audit;").unwrap());
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let err = sender
            .execute(&ctx("a-1"), &target(), &send_action("审计失败"))
            .await
            .unwrap_err();

        assert_eq!(err.code, "storage_failed");
        let commands_after_db_failure = fake.wire();
        assert_eq!(commands_after_db_failure, 0);
        assert_eq!(row_count(&store), 0, "审计失败不得留下账本行");
    }

    /// 正文出口：真实换行/中文/emoji 原样保留；签名只在开关打开时追加；`/` 开头不加。
    #[test]
    fn official_text_preserves_newlines_and_optional_device_signature() {
        assert_eq!(
            compose_official_text("phone", "第一行\n第二行", false).unwrap(),
            "第一行\n第二行"
        );
        assert_eq!(
            compose_official_text("phone", "第一行\n第二行", true).unwrap(),
            "第一行\n第二行 [mobile phone]"
        );
        assert_eq!(
            compose_official_text("手机📱", "表情🌏\n中文", true).unwrap(),
            "表情🌏\n中文 [mobile 手机📱]"
        );
        // CRLF 也不归一（终端 normalize 会把换行变字面 \n 两个字符，这里绝不走那条路）
        assert_eq!(
            compose_official_text("phone", "a\r\nb", false).unwrap(),
            "a\r\nb"
        );
        // 行首斜杠：不加签名（斜杠命令的任何附加文本都会使其失效）
        assert_eq!(
            compose_official_text("phone", "/compact", true).unwrap(),
            "/compact"
        );
        // 纯空白：invalid_action（零帧零行）
        assert_eq!(
            compose_official_text("phone", " \n\t ", true)
                .unwrap_err()
                .code,
            "invalid_action"
        );
        // 超预算：payload_too_large（写帧之前拒绝）
        let huge = "a".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            compose_official_text("phone", &huge, false)
                .unwrap_err()
                .code,
            "payload_too_large"
        );
        // 签名花名内换行折叠为空格（不得凭空多出一行）
        assert_eq!(
            compose_official_text("i\nPhone", "hi", true).unwrap(),
            "hi [mobile i Phone]"
        );
    }

    /// 已受理不因无关活动/多余 ACK 字段被升级为 confirmed。
    #[tokio::test]
    async fn accepted_is_not_confirmed_by_unrelated_activity() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(serde_json::json!({
            "status": "accepted",
            "activeWorks": [{ "kind": "chat", "foregroundExecutionId": "exec-9" }],
            "sessionActivity": 3,
        })));
        let sender = sender(&fake, &store);
        let (ctx, action) = (ctx("a-1"), send_action("正常一条"));

        let first = sender.execute(&ctx, &target(), &action).await.unwrap();
        let unrelated_activity_receipt = sender.execute(&ctx, &target(), &action).await.unwrap();

        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(unrelated_activity_receipt.state, ActionState::Accepted);
        assert_ne!(first.state, ActionState::Confirmed);
        assert_eq!(fake.wire(), 1);
    }

    /// 并发同 id：只派发一次，后到者拿既有保守回执。
    #[tokio::test]
    async fn concurrent_same_id_calls_dispatch_once() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::gated();
        let sender = Arc::new(CommandSender::with_timeout(
            fake.clone(),
            store.clone(),
            Duration::from_secs(5),
        ));
        let (ctx, action) = (ctx("a-1"), send_action("并发"));
        let first = tokio::spawn({
            let sender = sender.clone();
            let (ctx, action, target) = (ctx.clone(), action.clone(), target());
            async move { sender.execute(&ctx, &target, &action).await }
        });
        // 等第一帧真正写出（账本行已插入，派发在途）
        let _written = tokio::time::timeout(Duration::from_secs(2), fake.recorded.acquire())
            .await
            .expect("第一帧未写出")
            .expect("semaphore");
        let second = tokio::spawn({
            let sender = sender.clone();
            let (ctx, action, target) = (ctx.clone(), action.clone(), target());
            async move { sender.execute(&ctx, &target, &action).await }
        });
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("并发第二次调用不得阻塞")
            .unwrap()
            .unwrap();
        assert_eq!(second.state, ActionState::Unknown, "后到者只得保守回执");
        assert_eq!(fake.wire(), 1, "并发同 id 只允许一次派发");

        fake.release.add_permits(1);
        let first = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("首次派发超时")
            .unwrap()
            .unwrap();
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(fake.wire(), 1);
    }

    /// 重启恢复：新进程实例重放同 id 只返回 unknown，零帧。
    #[tokio::test]
    async fn restart_recovery_returns_unknown_without_send() {
        let store = DeviceStore::memory();
        let before = FakeBridge::new(Reply::AckLost);
        let before_sender = sender(&before, &store);
        let (ctx, action) = (ctx("a-1"), send_action("重启前"));
        let crashed = before_sender
            .execute(&ctx, &target(), &action)
            .await
            .unwrap();
        assert_eq!(crashed.state, ActionState::Unknown);
        assert_eq!(before.wire(), 1);

        // 重启后的进程视图：同一持久库 + 新桥
        let after = FakeBridge::new(Reply::Body(accepted_body()));
        let after_sender = sender(&after, &store);
        let recovered = after_sender
            .execute(&ctx, &target(), &action)
            .await
            .unwrap();

        assert_eq!(recovered.state, ActionState::Unknown);
        assert_eq!(after.wire(), 0, "重启后绝不重发");
        assert_eq!(row_count(&store), 1);
    }

    /// 跨设备同 id：冲突且零帧，不泄漏旧回执。
    #[tokio::test]
    async fn cross_device_same_id_is_conflict_without_wire() {
        let store = DeviceStore::memory();
        let owner = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&owner, &store)
            .execute(&ctx("a-1"), &target(), &send_action("主人的消息"))
            .await
            .unwrap();
        assert_eq!(owner.wire(), 1);

        let intruder = FakeBridge::new(Reply::Body(accepted_body()));
        let intruder_ctx = ActionContext {
            action_id: "a-1".to_string(),
            device_id: "phone-2".to_string(),
            device_name: "Pixel".to_string(),
        };
        let err = sender(&intruder, &store)
            .execute(&intruder_ctx, &target(), &send_action("主人的消息"))
            .await
            .unwrap_err();
        assert_eq!(err.code, "action_payload_conflict");
        assert_eq!(intruder.wire(), 0, "跨设备同 id 不得写出");
    }

    /// 纯空白/超预算：写帧之前拒绝，零帧零行。
    #[tokio::test]
    async fn whitespace_only_and_over_budget_send_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let blank = sender
            .execute(&ctx("a-1"), &target(), &send_action("   \n\t  "))
            .await
            .unwrap_err();
        assert_eq!(blank.code, "invalid_action");
        let huge = "a".repeat(MAX_TEXT_BYTES + 1);
        let too_large = sender
            .execute(&ctx("a-2"), &target(), &send_action(&huge))
            .await
            .unwrap_err();
        assert_eq!(too_large.code, "payload_too_large");
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 签名开关读取单点：设置 on 才追加 ` [mobile <花名>]`，off 裸发；`/` 开头不加。
    #[tokio::test]
    async fn signature_setting_controls_official_suffix() {
        let store = DeviceStore::memory();
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "on"));
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        sender
            .execute(&ctx("a-1"), &target(), &send_action("带签名"))
            .await
            .unwrap();
        sender
            .execute(&ctx("a-2"), &target(), &send_action("/compact"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(0)["payload"]["text"],
            serde_json::json!("带签名 [mobile iPhone]")
        );
        assert_eq!(
            fake.envelope(1)["payload"]["text"],
            serde_json::json!("/compact")
        );

        // 关闭后裸发（缺省 off 语义由 message_signature_enabled_conn 保证）
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "off"));
        sender
            .execute(&ctx("a-3"), &target(), &send_action("裸发"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(2)["payload"]["text"],
            serde_json::json!("裸发")
        );
    }

    /// 行首斜杠保持原样且内容逐字不变（不在本批承诺新增 slash 能力）。
    #[tokio::test]
    async fn leading_slash_message_keeps_no_signature() {
        let store = DeviceStore::memory();
        store.with(|c| settings::set_setting_conn(c, "remote_message_signature", "on"));
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&fake, &store)
            .execute(&ctx("a-1"), &target(), &send_action("/permissions\nnext"))
            .await
            .unwrap();
        assert_eq!(
            fake.envelope(0)["payload"]["text"],
            serde_json::json!("/permissions\nnext")
        );
    }

    /// ACK 显式拒绝/noop 如实分类，且不回传宿主原因原文。
    #[tokio::test]
    async fn ack_rejected_and_noop_are_distinct_states() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(serde_json::json!({
            "status": "rejected",
            "reasonCode": "host-secret-reason",
        })));
        let sender = sender(&fake, &store);

        let rejected = sender
            .execute(&ctx("a-1"), &target(), &send_action("被拒"))
            .await
            .unwrap();
        assert_eq!(rejected.state, ActionState::Rejected);
        assert!(rejected.reason_code.is_none(), "宿主原因原文不得下发/落账");

        fake.set_reply(Reply::Body(serde_json::json!({ "status": "noop" })));
        let noop = sender
            .execute(&ctx("a-2"), &target(), &send_action("无需处理"))
            .await
            .unwrap();
        assert_eq!(noop.state, ActionState::Noop);
        assert_ne!(noop.state, ActionState::Accepted, "noop 不是成功");
        assert_eq!(fake.wire(), 2);
    }

    /// 写后失败/断网按 unknown（绝不 send_failed）；写前绑定失败才是 send_failed。
    #[tokio::test]
    async fn write_error_maps_to_unknown_but_bind_error_is_send_failed() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Error(codes::UNKNOWN));
        let sender = sender(&fake, &store);

        for (index, code) in [codes::UNKNOWN, codes::OFFLINE, codes::NOT_READY]
            .into_iter()
            .enumerate()
        {
            fake.set_reply(Reply::Error(code));
            let receipt = sender
                .execute(
                    &ctx(&format!("a-{index}")),
                    &target(),
                    &send_action("可能已写出"),
                )
                .await
                .unwrap();
            assert_eq!(
                receipt.state,
                ActionState::Unknown,
                "{code} 不得判成确证未写出"
            );
            assert_eq!(receipt.reason_code.as_deref(), Some(code));
        }

        // 桥保证零写的绑定代际/会话失败 → send_failed（用户可换 actionId 重试）
        fake.set_reply(Reply::Error(codes::STALE_GENERATION));
        let failed = sender
            .execute(&ctx("a-stale"), &target(), &send_action("零写"))
            .await
            .unwrap();
        assert_eq!(failed.state, ActionState::SendFailed);
        assert_eq!(failed.reason_code.as_deref(), Some(codes::STALE_GENERATION));
    }

    /// 全部变体已接入命令层：未装配端口时一律 not_ready；Permission/Selection
    /// 默认能力矩阵关闭对应闸（needs_evidence，零帧零行；开闸证据由 T8.2 单独发放）。
    #[tokio::test]
    async fn non_send_actions_are_invalid_and_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        // Permission/Selection 默认关闭：即使未装配端口也必须先被能力闸拦下。
        let closed = [
            ControlAction::Permission {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
                option_id: "allowOnce".to_string(),
            },
            ControlAction::Selection {
                session_id: SESSION.to_string(),
                generation: GEN,
                model: "model-a".to_string(),
                thought: "high".to_string(),
            },
        ];
        for action in &closed {
            let err = sender
                .execute(&ctx("a-closed"), &target(), action)
                .await
                .unwrap_err();
            assert_eq!(
                err.code,
                "needs_evidence",
                "{} 默认关闭必须拒绝",
                action.kind()
            );
        }
        let needs_port = [
            ControlAction::Snooze {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
            },
            ControlAction::Answer {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
                answers: vec![AnswerEntry {
                    question_text: "题".to_string(),
                    selected_values: vec!["A".to_string()],
                    free_text: None,
                }],
            },
            ControlAction::Stop {
                session_id: SESSION.to_string(),
                generation: GEN,
                expected_foreground_execution_id: "exec-1".to_string(),
            },
        ];
        for (index, action) in needs_port.iter().enumerate() {
            let err = sender
                .execute(&ctx(&format!("a-p{index}")), &target(), action)
                .await
                .unwrap_err();
            assert_eq!(
                err.code,
                "not_ready",
                "{} 未装配对应端口必须拒绝",
                action.kind()
            );
        }
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 动作自报的会话/代际与绑定目标不一致：零帧零行。
    #[tokio::test]
    async fn mismatched_target_or_generation_is_zero_wire() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);

        let stale = ControlAction::Send {
            session_id: SESSION.to_string(),
            generation: GEN + 1,
            text: "旧代际".to_string(),
        };
        assert_eq!(
            sender
                .execute(&ctx("a-1"), &target(), &stale)
                .await
                .unwrap_err()
                .code,
            "stale_generation"
        );

        let other_session = ControlAction::Send {
            session_id: "sess_other".to_string(),
            generation: GEN,
            text: "别的会话".to_string(),
        };
        assert_eq!(
            sender
                .execute(&ctx("a-2"), &target(), &other_session)
                .await
                .unwrap_err()
                .code,
            "invalid_action"
        );
        assert_eq!(fake.wire(), 0);
        assert_eq!(row_count(&store), 0);
    }

    /// 信封逐字段对齐 E3 证据：channel/method/字段面/可信 clientId/epoch ms。
    #[tokio::test]
    async fn envelope_matches_probe_wire_shape() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        sender(&fake, &store)
            .execute(&ctx("a-1"), &target(), &send_action("信封检查"))
            .await
            .unwrap();

        let frames = fake.frames();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.channel, "zcode-agent");
        assert_eq!(frame.method, "sendConversationCommandV4");
        assert_eq!(frame.target.session_id, SESSION);
        assert_eq!(frame.args.len(), 1, "args 恰一个 envelope");

        let envelope = frame.args[0].as_object().expect("envelope 必须是对象");
        let mut keys: Vec<&str> = envelope.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "clientId",
                "commandId",
                "issuedAt",
                "payload",
                "sessionId",
                "type"
            ]
        );
        assert_eq!(envelope["clientId"], serde_json::json!(DEVICE));
        assert_eq!(envelope["sessionId"], serde_json::json!(SESSION));
        assert_eq!(envelope["type"], serde_json::json!("sendText"));
        let payload = envelope["payload"].as_object().expect("payload");
        assert_eq!(payload.len(), 1, "sendText 只带 text（无 mode/model/CAS）");
        assert_eq!(payload["text"], serde_json::json!("信封检查"));
        let issued_at = envelope["issuedAt"]
            .as_i64()
            .expect("issuedAt 必须是整数 epoch ms");
        assert!(issued_at >= 1_700_000_000_000, "issuedAt 必须是毫秒");

        let command_id = envelope["commandId"].as_str().expect("commandId");
        assert_eq!(command_id.len(), 36, "commandId 必须是 UUID");
        assert_eq!(command_id.as_bytes()[14], b'4', "UUIDv4 版本位");
        assert!(
            matches!(command_id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "UUIDv4 变体位"
        );
        assert_ne!(new_command_id(), new_command_id(), "commandId 必须随机");
    }

    /// ACK 窗必须是 20s，而且必须**原样交给端口**（桥的 RPC 待答条目按它收口）：
    /// 15s 的宿主 accepted 仍在窗内 → Accepted；21s 才到的应答已超窗 → Unknown。
    #[tokio::test]
    async fn send_ack_window_is_twenty_seconds() {
        // 生产窗：20s（brief 写死；桥内 rpc 窗不再是有效上限）。
        assert_eq!(ACK_TIMEOUT, Duration::from_secs(20));

        let store = DeviceStore::memory();
        let production = FakeBridge::new(Reply::AckLost);
        let sender = CommandSender::new(production.clone(), store.clone());
        assert_eq!(sender.ack_timeout, ACK_TIMEOUT, "生产构造必须用 20s ACK 窗");

        // 用注入窗把 20s 时序压到毫秒：fake 按「自身窗的 15/20、21/20」应答，
        // 比例语义与真实的 15s/21s 完全一致。
        let fast = FakeBridge::new(Reply::BodyAfter {
            body: accepted_body(),
            num: 15,
            den: 20,
        });
        let sender =
            CommandSender::with_timeout(fast.clone(), store.clone(), Duration::from_millis(40));
        let within = sender
            .execute(&ctx("a-15"), &target(), &send_action("十五秒的 ACK"))
            .await
            .unwrap();
        assert_eq!(
            within.state,
            ActionState::Accepted,
            "窗内（15/20）的宿主 accepted 必须判已受理"
        );
        assert_eq!(
            fast.observed_timeouts(),
            vec![Duration::from_millis(40)],
            "ACK 窗必须原样交给端口（不得被桥内 rpc 窗抢先收口）"
        );

        let late = FakeBridge::new(Reply::ErrorAfter {
            code: codes::NOT_READY,
            num: 21,
            den: 20,
        });
        let sender = CommandSender::with_timeout(late.clone(), store, Duration::from_millis(40));
        let beyond = sender
            .execute(&ctx("a-21"), &target(), &send_action("二十一秒的 ACK"))
            .await
            .unwrap();
        assert_eq!(
            beyond.state,
            ActionState::Unknown,
            "超出 ACK 窗的应答（桥先收口）只能是保守 unknown"
        );
        assert_eq!(late.wire(), 1, "超窗也不得重发");
    }

    /// 超窗收口由**端口自己的期限**完成：调用方绝不取消在途 future。
    ///
    /// 取消（外层 `tokio::time::timeout` 早于端口期限触发）会绕过桥的
    /// `unregister_rpc` 清理，在 pending 里留下永不回收的 waiter；本用例用
    /// drop 探针锁死「future 正常返回而非被 drop」。
    #[tokio::test]
    async fn timeout_cleans_bridge_pending() {
        let store = DeviceStore::memory();
        // 端口在自身窗的 2 倍处才收口：若调用方还外套 20ms 超时，future 会在
        // 半途被 drop（cancelled 置位），正好复现 pending 泄漏形态。
        let fake = FakeBridge::new(Reply::ErrorAfter {
            code: codes::NOT_READY,
            num: 2,
            den: 1,
        });
        let sender = CommandSender::with_timeout(fake.clone(), store, Duration::from_millis(20));

        let receipt = sender
            .execute(&ctx("a-1"), &target(), &send_action("等端口收口"))
            .await
            .unwrap();

        assert_eq!(receipt.state, ActionState::Unknown);
        assert!(
            !fake.cancelled(),
            "调用方不得取消端口 future：桥必须自行 unregister pending 条目"
        );
        assert_eq!(fake.wire(), 1);
    }

    /// 起始审计与终态审计都带「类别 + 字数」，但绝不摘正文。
    #[tokio::test]
    async fn audit_start_and_terminal_include_count_without_body() {
        let store = DeviceStore::memory();
        let fake = FakeBridge::new(Reply::Body(accepted_body()));
        let sender = sender(&fake, &store);
        sender
            .execute(&ctx("a-1"), &target(), &send_action("你好世界"))
            .await
            .unwrap();

        let rows = audits(&store);
        assert_eq!(rows.len(), 2, "起始 + 终态各一条审计");
        // recent_conn 最新在前：终态 accepted，随后是 started。
        assert_eq!(rows[0].result, "accepted");
        assert_eq!(rows[1].result, "started");
        for row in &rows {
            assert_eq!(row.channel, "zcode_official");
            assert_eq!(row.agent_type, "zcode");
            assert_eq!(row.action, "zcode_send");
            assert_eq!(row.summary, "send chars=4", "摘要只记类别与字数");
            assert!(!row.summary.contains("你好"), "摘要绝不摘正文");
        }
    }

    /// 预算按物理帧最坏膨胀推导（JSON `\u00XX` 6× × Base64 4/3）：
    /// 预算内的控制字符密集正文必须真的能通过编码，否则预检形同虚设。
    #[test]
    fn byte_budget_survives_worst_case_base64_encoding() {
        let boundary = "\u{1}".repeat(MAX_TEXT_BYTES);
        let composed =
            compose_official_text("phone", &boundary, false).expect("预算内必须可以组合");
        let message = serde_json::to_vec(&serde_json::json!({
            "type": SEND_TEXT_TYPE,
            "payload": { "text": composed },
        }))
        .expect("message json");
        super::super::codec::encode_frame(
            &super::super::codec::RpcIdentity::new("bridge-1"),
            1,
            1,
            &message,
        )
        .expect("预算内的最坏转义正文必须可以通过物理帧编码（1 MiB 含 Base64）");
    }

    // ============================================================
    // T4.1 Slice B2：问答应答 / snooze / 宿主终态（tests::answer）
    // ============================================================

    mod answer {
        use super::*;
        use std::collections::BTreeSet;
        use std::time::Instant;

        use serde_json::json;

        use crate::services::zcode_control::types::{
            PendingKind, PendingView, QuestionOption, QuestionView,
        };

        /// 宿主 pending 侧的测试替身（与出站 `FakeBridge` 分开：授权/snooze/终态
        /// 是独立端口；生产两者都是 `BridgeManager`）。
        pub(super) struct FakePending {
            inner: Mutex<PendingState>,
        }

        struct PendingState {
            target: BoundTarget,
            card: PendingView,
            /// 本地仍在册（authorize 可过）：与宿主集合独立。
            local_present: bool,
            /// 宿主最近一帧的卡集合。
            host_ids: BTreeSet<String>,
            host_revision: u64,
            inflight: BTreeSet<String>,
            consumed: BTreeSet<String>,
            snooze_result: Result<(), &'static str>,
            snooze_calls: usize,
            /// 每次 snooze 收到的 clientId（断言与 resolve/send 同源）。
            snooze_clients: Vec<String>,
            /// `wait_host_cleared` 首次轮询时模拟宿主撤卡（推进代数 + 移出集合）。
            clear_on_wait: bool,
        }

        impl FakePending {
            pub(super) fn new(target: BoundTarget, card: PendingView) -> Arc<Self> {
                let interaction_id = card.interaction_id.clone();
                Arc::new(Self {
                    inner: Mutex::new(PendingState {
                        target,
                        card,
                        local_present: true,
                        host_ids: [interaction_id].into_iter().collect(),
                        host_revision: 1,
                        inflight: BTreeSet::new(),
                        consumed: BTreeSet::new(),
                        snooze_result: Ok(()),
                        snooze_calls: 0,
                        snooze_clients: Vec::new(),
                        clear_on_wait: false,
                    }),
                })
            }

            pub(super) fn set_snooze_error(&self, code: &'static str) {
                self.inner.lock().unwrap().snooze_result = Err(code);
            }

            pub(super) fn set_snooze_ok(&self) {
                self.inner.lock().unwrap().snooze_result = Ok(());
            }

            pub(super) fn clear_host_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.remove(&interaction_id);
                state.host_revision += 1;
            }

            /// 宿主帧整体替换为「不含该卡」（镜像 `PendingBook::replace_pendings`：
            /// 撤卡帧同时释放写后未知的在途闸，本地展示同步撤卡）。
            pub(super) fn host_frame_without_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.remove(&interaction_id);
                state.host_revision += 1;
                state.inflight.remove(&interaction_id);
                state.local_present = false;
            }

            /// 宿主后续帧重新列出该 id（同一卡回到展示面）。
            fn host_frame_relists_card(&self) {
                let mut state = self.inner.lock().unwrap();
                let interaction_id = state.card.interaction_id.clone();
                state.host_ids.insert(interaction_id);
                state.host_revision += 1;
                state.local_present = true;
            }

            pub(super) fn clear_on_wait(&self) {
                self.inner.lock().unwrap().clear_on_wait = true;
            }

            pub(super) fn snooze_calls(&self) -> usize {
                self.inner.lock().unwrap().snooze_calls
            }

            fn snooze_clients(&self) -> Vec<String> {
                self.inner.lock().unwrap().snooze_clients.clone()
            }

            pub(super) fn inflight(&self, interaction_id: &str) -> bool {
                self.inner.lock().unwrap().inflight.contains(interaction_id)
            }

            pub(super) fn consumed(&self, interaction_id: &str) -> bool {
                self.inner.lock().unwrap().consumed.contains(interaction_id)
            }
        }

        impl PendingPort for FakePending {
            fn authorize_pending<'a>(
                &'a self,
                target: &'a BoundTarget,
                interaction_id: &'a str,
            ) -> BoxFuture<'a, ZResult<PendingView>> {
                Box::pin(async move {
                    let mut state = self.inner.lock().unwrap();
                    if *target != state.target {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    if state.consumed.contains(interaction_id)
                        || state.inflight.contains(interaction_id)
                    {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    if !state.local_present || state.card.interaction_id != interaction_id {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    state.inflight.insert(interaction_id.to_owned());
                    Ok(state.card.clone())
                })
            }

            fn snooze<'a>(
                &'a self,
                target: &'a BoundTarget,
                _interaction_id: &'a str,
                client_id: &'a str,
                _timeout: Duration,
            ) -> BoxFuture<'a, ZResult<()>> {
                Box::pin(async move {
                    let mut state = self.inner.lock().unwrap();
                    if *target != state.target {
                        return Err(ZcodeError::of(codes::STALE_PENDING));
                    }
                    state.snooze_calls += 1;
                    state.snooze_clients.push(client_id.to_owned());
                    state.snooze_result.map_err(ZcodeError::of)
                })
            }

            fn host_pending_revision(&self, target: &BoundTarget) -> u64 {
                let state = self.inner.lock().unwrap();
                if *target != state.target {
                    return 0;
                }
                state.host_revision
            }

            fn host_has_pending(&self, target: &BoundTarget, interaction_id: &str) -> bool {
                let state = self.inner.lock().unwrap();
                *target == state.target && state.host_ids.contains(interaction_id)
            }

            fn wait_host_cleared<'a>(
                &'a self,
                target: &'a BoundTarget,
                interaction_id: &'a str,
                baseline_revision: u64,
                timeout: Duration,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    let deadline = Instant::now()
                        .checked_add(timeout)
                        .unwrap_or_else(Instant::now);
                    loop {
                        {
                            let mut state = self.inner.lock().unwrap();
                            if *target != state.target {
                                return false;
                            }
                            if state.clear_on_wait && state.host_revision == baseline_revision {
                                state.clear_on_wait = false;
                                state.host_ids.remove(interaction_id);
                                state.host_revision += 1;
                            }
                            if state.host_revision > baseline_revision
                                && !state.host_ids.contains(interaction_id)
                            {
                                return true;
                            }
                        }
                        let now = Instant::now();
                        if now >= deadline {
                            return false;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
            }

            fn mark_resolved_local(&self, target: &BoundTarget, interaction_id: &str) {
                let mut state = self.inner.lock().unwrap();
                if *target != state.target {
                    return;
                }
                state.inflight.remove(interaction_id);
                state.consumed.insert(interaction_id.to_owned());
                state.local_present = false;
            }

            fn clear_local_inflight(&self, target: &BoundTarget, interaction_id: &str) {
                let mut state = self.inner.lock().unwrap();
                if *target != state.target {
                    return;
                }
                state.inflight.remove(interaction_id);
            }
        }

        fn question(
            question_text: &str,
            values: &[&str],
            multi_select: bool,
            free_text_allowed: bool,
        ) -> QuestionView {
            QuestionView {
                header: "H".to_owned(),
                question_text: question_text.to_owned(),
                options: values
                    .iter()
                    .map(|value| QuestionOption {
                        value: (*value).to_owned(),
                        label: (*value).to_owned(),
                        description: None,
                        preview: None,
                    })
                    .collect(),
                multi_select,
                free_text_allowed,
            }
        }

        fn card(
            interaction_id: &str,
            auto_resolution: bool,
            questions: Vec<QuestionView>,
        ) -> PendingView {
            PendingView {
                interaction_id: interaction_id.to_owned(),
                kind: PendingKind::UserInput,
                anchor_row_id: Some(7),
                tool_call_id: Some("call_synthetic_0002".to_owned()),
                created_at: Some(1_700_000_000_000),
                summary: Some("synthetic: pick answers".to_owned()),
                tool_name: Some("AskUserQuestion".to_owned()),
                schema_interaction: None,
                sensitive: false,
                auto_resolution,
                auto_resolution_state: auto_resolution.then(|| "visibleCountdown".to_owned()),
                questions,
                options: Vec::new(),
            }
        }

        fn single_card(interaction_id: &str) -> PendingView {
            card(
                interaction_id,
                false,
                vec![question("原题？", &["A", "B"], false, false)],
            )
        }

        fn fake_pending(card: PendingView) -> Arc<FakePending> {
            FakePending::new(target(), card)
        }

        fn answer_entry(question_text: &str, values: &[&str]) -> AnswerEntry {
            AnswerEntry {
                question_text: question_text.to_owned(),
                selected_values: values.iter().map(|value| (*value).to_owned()).collect(),
                free_text: None,
            }
        }

        fn free_text_entry(question_text: &str, text: &str) -> AnswerEntry {
            AnswerEntry {
                question_text: question_text.to_owned(),
                selected_values: Vec::new(),
                free_text: Some(text.to_owned()),
            }
        }

        fn answer_action(interaction_id: &str, answers: Vec<AnswerEntry>) -> ControlAction {
            answer_action_for(SESSION, GEN, interaction_id, answers)
        }

        fn answer_action_for(
            session_id: &str,
            generation: u64,
            interaction_id: &str,
            answers: Vec<AnswerEntry>,
        ) -> ControlAction {
            ControlAction::Answer {
                session_id: session_id.to_owned(),
                generation,
                interaction_id: interaction_id.to_owned(),
                answers,
            }
        }

        fn answer_sender(
            fake: &Arc<FakeBridge>,
            pending: &Arc<FakePending>,
            store: &DeviceStore,
        ) -> CommandSender {
            CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                .with_pending(pending.clone())
                .with_confirm_timeout(Duration::from_millis(40))
        }

        fn resolve_frames(fake: &FakeBridge) -> Vec<Frame> {
            fake.frames()
                .into_iter()
                .filter(|frame| {
                    frame
                        .args
                        .first()
                        .map(|envelope| envelope["type"] == json!("resolveInteraction"))
                        .unwrap_or(false)
                })
                .collect()
        }

        pub(super) fn stored_state(store: &DeviceStore, action_id: &str) -> Option<String> {
            store.with(|conn| {
                conn.query_row(
                    "SELECT state FROM zcode_actions WHERE action_id = ?1",
                    [action_id],
                    |row| row.get(0),
                )
                .ok()
            })
        }

        /// 账本 + 审计的全列 dump（断言敏感正文从不落库）。
        pub(super) fn db_dump(store: &DeviceStore) -> String {
            store.with(|conn| {
                let mut dump = String::new();
                for table in ["zcode_actions", "write_audit"] {
                    let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                    let columns = statement.column_count();
                    let mut rows = statement.query([]).unwrap();
                    while let Some(row) = rows.next().unwrap() {
                        for index in 0..columns {
                            let value: rusqlite::types::Value = row.get(index).unwrap();
                            dump.push_str(&format!("{value:?}|"));
                        }
                    }
                }
                dump
            })
        }

        /// 已消费/不在册的卡：零帧，绝不复用另一张卡。
        #[tokio::test]
        async fn stale_or_consumed_pending_never_sends() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let action = answer_action("ix-1", vec![answer_entry("原题？", &["A"])]);

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);

            // 同一张卡第二次提交（新 actionId）：本地已消费 → stale_pending，零新帧。
            let err = sender
                .execute(&ctx("a-2"), &target(), &action)
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1, "已消费卡不得再次 resolve");

            // 不存在的卡：同样零帧（不得顺手消费别的卡）。
            let missing = answer_action("ix-missing", vec![answer_entry("原题？", &["A"])]);
            let err = sender
                .execute(&ctx("a-3"), &target(), &missing)
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// 本地消费 ≠ 宿主撤卡：ACK-only 时宿主集合仍列着卡。
        #[tokio::test]
        async fn local_consume_is_not_host_cleared() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert!(pending.consumed("ix-1"), "ACK 受理后本地必须消费");
            assert!(
                pending.host_has_pending(&target(), "ix-1"),
                "本地消费不得改动宿主集合"
            );
            // 线上恰一帧 resolve（不依赖测试替身自己的轮询循环）。
            assert_eq!(resolve_frames(&fake).len(), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
        }

        /// 敏感卡：答案只上 wire，账本只有指纹，审计只有计数。
        #[tokio::test]
        async fn sensitive_question_never_persists_draft() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut sensitive = card(
                "ix-secret",
                false,
                vec![question("口令？", &[], false, true)],
            );
            sensitive.sensitive = true;
            let pending = fake_pending(sensitive);
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action(
                        "ix-secret",
                        vec![free_text_entry("口令？", "机密-草稿-ABC")],
                    ),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);

            // wire 上必须有答案（送达宿主），本地库不得有任何正文/题干痕迹。
            let envelope = fake.envelope(0);
            assert_eq!(
                envelope["payload"]["answer"]["content"]["answers"]["口令？"],
                json!("机密-草稿-ABC")
            );
            let dump = db_dump(&store);
            for forbidden in ["机密", "草稿", "ABC", "口令？"] {
                assert!(
                    !dump.contains(forbidden),
                    "敏感正文/题干不得落库: {forbidden} in {dump}"
                );
            }
            let hash: String = store.with(|conn| {
                conn.query_row(
                    "SELECT payload_hash FROM zcode_actions WHERE action_id = 'a-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            });
            assert_eq!(hash.len(), 64, "账本只存指纹");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
            let summary: String = store.with(|conn| {
                conn.query_row(
                    "SELECT summary FROM zcode_actions WHERE action_id = 'a-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            });
            assert_eq!(summary, "answer answers=1 selected=0");
            assert!(
                audits(&store)
                    .iter()
                    .all(|row| row.summary == "answer answers=1 selected=0"),
                "审计摘要只含类别与计数"
            );
        }

        /// 计划审批（plan_approval）不是问答：看见 Approve 选项也不得代批。
        #[tokio::test]
        async fn plan_approval_is_not_a_question_auto_approval() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut plan = card(
                "ix-plan",
                true,
                vec![question(
                    "Approve plan?",
                    &["Approve", "Reject"],
                    false,
                    false,
                )],
            );
            plan.schema_interaction = Some("plan_approval".to_owned());
            let pending = fake_pending(plan);
            let sender = answer_sender(&fake, &pending, &store);

            let err = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-plan", vec![answer_entry("Approve plan?", &["Approve"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "needs_evidence", "计划审批绝不能按问答代批");
            assert_eq!(fake.wire(), 0, "未支持 schema 必须零 resolve 帧");
            assert_eq!(pending.snooze_calls(), 0, "未支持 schema 也不得先 snooze");
            assert!(!pending.consumed("ix-plan"), "零 value 不得消费卡");
            assert_eq!(stored_state(&store, "a-1"), None, "本地拒绝不得留下账本行");
            assert!(audits(&store).is_empty(), "本地拒绝不得写 started 审计");
        }

        /// snooze 未证实（失败或卡已不在宿主集合）：零 resolve。
        #[tokio::test]
        async fn snooze_unconfirmed_zero_resolve() {
            let store = DeviceStore::memory();

            // ① snooze 失败（超窗/unknown）：零 resolve，回执保守 unknown。
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending.set_snooze_error(codes::UNKNOWN);
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(resolve_frames(&fake).len(), 0, "snooze 未证实不得 resolve");

            // ② snooze 受理但卡不在宿主集合：仍零 resolve。
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = fake_pending(card(
                "ix-2",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending2.clear_host_card();
            let sender2 = answer_sender(&fake2, &pending2, &store);
            let receipt2 = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-2", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt2.state, ActionState::Unknown);
            assert_eq!(pending2.snooze_calls(), 1);
            assert_eq!(resolve_frames(&fake2).len(), 0);
        }

        /// snooze 受理且卡仍在本帧宿主集合：恰一帧 resolve，形状对齐探针。
        #[tokio::test]
        async fn snooze_accepted_then_resolve_single_frame() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A", "B"], false, false)],
            ));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(
                pending.snooze_calls(),
                1,
                "autoResolution 在场恰一次 snooze"
            );
            let frames = resolve_frames(&fake);
            assert_eq!(frames.len(), 1, "恰一帧 resolveInteraction");
            let frame = &frames[0];
            assert_eq!(frame.channel, "zcode-agent");
            assert_eq!(frame.method, "sendConversationCommandV4");
            let envelope = &frame.args[0];
            assert_eq!(envelope["type"], json!("resolveInteraction"));
            assert_eq!(envelope["payload"]["interactionId"], json!("ix-1"));
            assert_eq!(
                envelope["payload"]["answer"],
                json!({
                    "action": "accept",
                    "content": {
                        "answers": { "原题？": "A" },
                        "answer_0": "A",
                        "answer": "A"
                    }
                }),
                "resolve 载荷必须是宿主原值形状"
            );
            assert_eq!(envelope["clientId"], json!(DEVICE));
            assert_eq!(envelope["sessionId"], json!(SESSION));
            assert!(envelope["issuedAt"].as_i64().unwrap() >= 1_700_000_000_000);
            assert_eq!(
                fake.observed_timeouts(),
                vec![Duration::from_millis(20)],
                "ACK 窗原样交给端口"
            );
        }

        /// ACK 只算 accepted；宿主撤卡（代数前进 + id 消失）才升级 confirmed。
        #[tokio::test]
        async fn ack_only_is_accepted_host_clear_is_confirmed() {
            let store = DeviceStore::memory();

            // ① ACK 受理但宿主没撤卡：只报 accepted（绝不伪造 confirmed）。
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let ack_only = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(ack_only.state, ActionState::Accepted);
            assert!(pending.host_has_pending(&target(), "ix-1"));
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));

            // ② ACK 后宿主撤卡（代数前进 + id 消失）才是 confirmed。
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = fake_pending(single_card("ix-1"));
            pending2.clear_on_wait();
            let sender2 = answer_sender(&fake2, &pending2, &store);
            let confirmed = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(confirmed.state, ActionState::Confirmed);
            assert!(!pending2.host_has_pending(&target(), "ix-1"));
            assert_eq!(stored_state(&store, "a-2").as_deref(), Some("confirmed"));
        }

        /// 宿主卡在 ACK 前已消失且没有 accepted ACK：只能 unknown，不得消费。
        #[tokio::test]
        async fn host_clear_before_ack_is_unknown() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({ "status": "noop" })));
            let pending = fake_pending(single_card("ix-1"));
            // 宿主已撤卡（本地投影尚未跟上）。
            pending.clear_host_card();
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(
                receipt.state,
                ActionState::Unknown,
                "无 accepted ACK 且宿主已撤卡只能 unknown"
            );
            assert_eq!(receipt.reason_code.as_deref(), Some("stale_pending"));
            assert_eq!(resolve_frames(&fake).len(), 1, "帧已写出，只收口为 unknown");
            assert!(!pending.consumed("ix-1"), "未受理不得本地消费");
        }

        /// 错会话/错代际：零帧且不消费这张卡（正确目标随后仍可作答）。
        #[tokio::test]
        async fn wrong_session_or_anchor_does_not_consume_other_card() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let other = BoundTarget {
                session_id: "sess_ffffffffffffffffffffffffffffffff".to_owned(),
                ..target()
            };
            let err = sender
                .execute(
                    &ctx("a-1"),
                    &other,
                    &answer_action_for(&other.session_id, GEN, "ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(fake.wire(), 0);

            let newer = BoundTarget {
                generation: GEN + 1,
                ..target()
            };
            let err = sender
                .execute(
                    &ctx("a-2"),
                    &newer,
                    &answer_action_for(SESSION, GEN + 1, "ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, "stale_pending");
            assert_eq!(fake.wire(), 0);

            // 正确目标仍可作答：错目标尝试没有消费/在途锁死这张卡。
            let receipt = sender
                .execute(&ctx("a-3"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// 多题答案合并成**一次** resolve；多选回传宿主 value 数组。
        #[tokio::test]
        async fn multi_question_resolves_once() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let questions = vec![
                question("颜色？", &["Red", "Blue"], false, false),
                question("尺寸？", &["S", "M"], true, false),
            ];
            let pending = fake_pending(card("ix-1", false, questions));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![
                answer_entry("颜色？", &["Red"]),
                answer_entry("尺寸？", &["S", "M"]),
            ];
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            let frames = resolve_frames(&fake);
            assert_eq!(frames.len(), 1, "多题合并为恰一次 resolve");
            let content = &frames[0].args[0]["payload"]["answer"]["content"];
            assert_eq!(content["answers"]["颜色？"], json!("Red"));
            assert_eq!(content["answers"]["尺寸？"], json!("S, M"));
            assert_eq!(content["answer_0"], json!("Red"));
            assert_eq!(content["answer_1"], json!(["S", "M"]));
            assert!(content.get("answer").is_none(), "多题不带单题兼容字段");
        }

        /// 同 actionId 同载荷重放：零第二帧，只回既有回执。
        #[tokio::test]
        async fn answer_reuses_action_id_idempotently() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let action = answer_action("ix-1", vec![answer_entry("原题？", &["A"])]);

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            let second = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();

            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(second, first, "同 actionId 同载荷必须返回既有回执");
            assert_eq!(resolve_frames(&fake).len(), 1, "重放零第二帧");
            assert_eq!(row_count(&store), 1);
        }

        // ============================================================
        // Fix round 1：snooze 幂等 noop / 在途闸释放 / 预留顺序 / 失败面
        // ============================================================

        /// snooze 的 `noop`（宿主「已暂停」）是幂等成功：卡仍在宿主集合 → 照常
        /// 恰一帧 resolve（重复 snooze 不得把卡锁死）。
        #[tokio::test]
        async fn snooze_noop_is_already_snoozed_then_resolves() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            // FakePending 的 `Ok` 即端口语义「accepted/duplicate/noop 均已暂停」。
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(
                pending.snooze_clients(),
                vec![DEVICE.to_owned()],
                "snooze 的 clientId 必须与 resolve 同源（可信设备 id）"
            );
            assert_eq!(resolve_frames(&fake).len(), 1, "noop 后仍必须 resolve");
            assert_eq!(fake.envelope(0)["clientId"], json!(DEVICE));
        }

        /// snooze 被宿主显式拒绝：硬失败，零 resolve，释放在途（可重试）。
        #[tokio::test]
        async fn snooze_rejected_zero_resolve() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(card(
                "ix-1",
                true,
                vec![question("原题？", &["A"], false, false)],
            ));
            pending.set_snooze_error(codes::INVALID_ACTION);
            let sender = answer_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(receipt.reason_code.as_deref(), Some(codes::INVALID_ACTION));
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(resolve_frames(&fake).len(), 0, "rejected 必须零 resolve");
            assert!(
                !pending.inflight("ix-1"),
                "resolve 未写出（可证明零写）必须释放在途"
            );

            // 释放后可重试：宿主下次接受 snooze → 恰一帧 resolve。
            pending.set_snooze_ok();
            let retry = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 1);
        }

        /// resolve ACK `rejected` 且卡仍在宿主集合：释放本地在途，用户可换新
        /// actionId 重试（宿主 alreadyResolved 幂等是最后防线）。
        #[tokio::test]
        async fn rejected_resolve_releases_inflight() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({"status": "rejected"})));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let rejected = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(rejected.state, ActionState::Rejected);
            assert!(!pending.inflight("ix-1"), "rejected 必须释放在途闸");
            assert_eq!(resolve_frames(&fake).len(), 1);

            fake.set_reply(Reply::Body(accepted_body()));
            let retry = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted, "新 actionId 可重试");
            assert_eq!(resolve_frames(&fake).len(), 2, "恰一帧/次，无自动重发");
            assert_eq!(row_count(&store), 2);
        }

        /// 写后 unknown 保留在途闸（不自动重试）；只有宿主撤卡帧才释放。
        #[tokio::test]
        async fn unknown_keeps_gate_until_host_frame() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Error(codes::NOT_READY));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let unknown = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap();
            assert_eq!(unknown.state, ActionState::Unknown);
            assert!(pending.inflight("ix-1"), "写后未知不得自动放开在途闸");
            assert_eq!(resolve_frames(&fake).len(), 1);

            // 宿主帧仍列该卡：闸保持，重试 stale 且零新帧。
            let blocked = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(blocked.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 1, "未知结果绝不自动重发");

            // 宿主帧撤卡（册整体替换缺席）→ 闸释放、展示同步撤下。
            pending.host_frame_without_card();
            assert!(!pending.inflight("ix-1"));
            let hidden = sender
                .execute(
                    &ctx("a-3"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap_err();
            assert_eq!(hidden.code, "stale_pending", "撤卡后本地展示不得复活");

            // 宿主后续帧重新列出该 id：无需人工清闸即可重新作答。
            pending.host_frame_relists_card();
            fake.set_reply(Reply::Body(accepted_body()));
            let retried = sender
                .execute(&ctx("a-4"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(retried.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 2);
        }

        /// 本地答案校验失败：零 resolve、零账本行、零审计（预留晚于校验）。
        #[tokio::test]
        async fn invalid_answer_leaves_no_reservation() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", vec![answer_entry("原题？", &["Z"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "invalid_answer");
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(row_count(&store), 0, "本地拒绝不得留下 reserved 行");
            assert!(audits(&store).is_empty(), "本地拒绝不得写 started 审计");
            assert!(!pending.inflight("ix-1"), "校验失败必须释放在途");
        }

        /// 授权失败（卡已撤/锚过期）：零 resolve、零账本行、零审计。
        #[tokio::test]
        async fn stale_pending_leaves_no_reservation() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-missing", vec![answer_entry("原题？", &["A"])]),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "stale_pending");
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(row_count(&store), 0, "授权失败不得留下 reserved 行");
            assert!(audits(&store).is_empty(), "授权失败不得写 started 审计");
        }

        /// resolve 确证零写（桥绑定拒绝）：send_failed 释放在途并允许新 actionId 重试。
        #[tokio::test]
        async fn resolve_zero_write_releases_inflight() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Error(codes::STALE_GENERATION));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let answers = vec![answer_entry("原题？", &["A"])];

            let failed = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &answer_action("ix-1", answers.clone()),
                )
                .await
                .unwrap();
            assert_eq!(failed.state, ActionState::SendFailed);
            assert!(!pending.inflight("ix-1"), "确证零写必须释放在途闸");

            fake.set_reply(Reply::Body(accepted_body()));
            let retry = sender
                .execute(&ctx("a-2"), &target(), &answer_action("ix-1", answers))
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted);
            assert_eq!(resolve_frames(&fake).len(), 2, "两次各恰一帧");
        }

        /// 独立 Snooze 动作：accepted / rejected / timeout 的收口与零 resolve。
        #[tokio::test]
        async fn standalone_snooze_execute_outcomes() {
            let snooze = ControlAction::Snooze {
                session_id: SESSION.to_string(),
                generation: GEN,
                interaction_id: "ix-1".to_string(),
            };

            // accepted：受理回执，账本 accepted，不消费卡且在途已释放。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            let sender = answer_sender(&fake, &pending, &store);
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(pending.snooze_calls(), 1);
            assert_eq!(pending.snooze_clients(), vec![DEVICE.to_owned()]);
            assert!(!pending.inflight("ix-1"), "snooze 不消费卡，必须释放在途");
            assert!(pending.host_has_pending(&target(), "ix-1"));
            assert_eq!(resolve_frames(&fake).len(), 0);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));

            // rejected：错误原样上抛，账本保守 unknown，零 resolve。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            pending.set_snooze_error(codes::INVALID_ACTION);
            let sender = answer_sender(&fake, &pending, &store);
            let error = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap_err();
            assert_eq!(error.code, "invalid_action");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("unknown"));
            assert!(!pending.inflight("ix-1"));

            // 超窗 unknown：错误上抛，账本 unknown，绝不重发。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = fake_pending(single_card("ix-1"));
            pending.set_snooze_error(codes::UNKNOWN);
            let sender = answer_sender(&fake, &pending, &store);
            let error = sender
                .execute(&ctx("a-1"), &target(), &snooze)
                .await
                .unwrap_err();
            assert_eq!(error.code, "unknown");
            assert_eq!(pending.snooze_calls(), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("unknown"));
        }
    }

    // ============================================================
    // T4.2 Slice A：stop 授权（tests::stop）
    // ============================================================

    mod stop {
        use super::*;

        use crate::services::zcode_control::types::{
            ActiveWork, CapabilityView, ControlView, SessionView, StopRequest,
        };

        fn work(kind: &str, execution_id: Option<&str>) -> ActiveWork {
            ActiveWork {
                kind: kind.to_owned(),
                foreground_execution_id: execution_id.map(str::to_owned),
            }
        }

        fn control(works: Vec<ActiveWork>) -> ControlView {
            ControlView {
                active_works: works,
            }
        }

        fn observed(execution_ids: &[&str]) -> ControlView {
            control(
                execution_ids
                    .iter()
                    .map(|id| work("primaryTurn", Some(id)))
                    .collect(),
            )
        }

        fn view(control: Option<ControlView>) -> SessionView {
            SessionView {
                session_id: SESSION.to_owned(),
                generation: GEN,
                revision: Some(4),
                ready: true,
                pending: Vec::new(),
                control,
                selection: None,
                capabilities: CapabilityView::default(),
            }
        }

        fn request(execution_id: Option<&str>) -> StopRequest {
            StopRequest {
                expected_foreground_execution_id: execution_id.map(str::to_owned),
            }
        }

        fn stop_action(expected: &str) -> ControlAction {
            ControlAction::Stop {
                session_id: SESSION.to_owned(),
                generation: GEN,
                expected_foreground_execution_id: expected.to_owned(),
            }
        }

        /// stop 端口的一次脚本化读：可选延迟 + 视图或瞬时错误。
        #[derive(Clone)]
        struct StopRead {
            delay: Duration,
            result: Result<SessionView, &'static str>,
        }

        impl StopRead {
            fn view(view: SessionView) -> Self {
                Self {
                    delay: Duration::ZERO,
                    result: Ok(view),
                }
            }

            fn error(code: &'static str) -> Self {
                Self {
                    delay: Duration::ZERO,
                    result: Err(code),
                }
            }

            /// 单次读挂住的模拟（生产可因重订阅 RPC 各占一个 10s 窗）。
            fn delayed(mut self, delay: Duration) -> Self {
                self.delay = delay;
                self
            }
        }

        /// stop 端口 fake：按次脚本化 `session` 读（延迟/错误/快照）+ 脚本化宿主终态。
        ///
        /// `reads` 的最后一项在后续调用中重复（首读 = 新鲜 control，第二次 =
        /// 写前复核）。`wait_stop_cleared` 返回脚本布尔（至多睡 `wait_delay`）并记录
        /// `(executionId, baselineRevision, timeout)`——命令层只信端口证据。
        struct FakeStop {
            reads: Mutex<Vec<StopRead>>,
            session_calls: AtomicUsize,
            session_budgets: Mutex<Vec<Duration>>,
            wait_result: AtomicBool,
            wait_delay: Mutex<Duration>,
            wait_calls: Mutex<Vec<(String, u64, Duration)>>,
        }

        impl FakeStop {
            fn new(views: Vec<SessionView>) -> Arc<Self> {
                Self::script(views.into_iter().map(StopRead::view).collect())
            }

            fn script(reads: Vec<StopRead>) -> Arc<Self> {
                assert!(!reads.is_empty(), "fake 至少需要一次读");
                Arc::new(Self {
                    reads: Mutex::new(reads),
                    session_calls: AtomicUsize::new(0),
                    session_budgets: Mutex::new(Vec::new()),
                    wait_result: AtomicBool::new(false),
                    wait_delay: Mutex::new(Duration::ZERO),
                    wait_calls: Mutex::new(Vec::new()),
                })
            }

            fn observed(execution_ids: &[&str]) -> Arc<Self> {
                Self::new(vec![view(Some(observed(execution_ids)))])
            }

            fn idle() -> Arc<Self> {
                Self::new(vec![view(Some(control(Vec::new())))])
            }

            fn unknown() -> Arc<Self> {
                Self::new(vec![view(None)])
            }

            fn generation(generation: u64) -> Arc<Self> {
                let mut stale = view(Some(observed(&["E-existing"])));
                stale.generation = generation;
                Self::new(vec![stale])
            }

            fn sequence(views: Vec<SessionView>) -> Arc<Self> {
                Self::new(views)
            }

            fn set_wait_result(&self, result: bool) {
                self.wait_result.store(result, Ordering::SeqCst);
            }

            fn set_wait_delay(&self, delay: Duration) {
                *self.wait_delay.lock().unwrap() = delay;
            }

            fn session_calls(&self) -> usize {
                self.session_calls.load(Ordering::SeqCst)
            }

            fn session_budgets(&self) -> Vec<Duration> {
                self.session_budgets.lock().unwrap().clone()
            }

            fn wait_calls(&self) -> Vec<(String, u64, Duration)> {
                self.wait_calls.lock().unwrap().clone()
            }
        }

        impl StopPort for FakeStop {
            fn session<'a>(
                &'a self,
                _target: &'a BoundTarget,
                timeout: Duration,
            ) -> BoxFuture<'a, ZResult<SessionView>> {
                Box::pin(async move {
                    let calls = self.session_calls.fetch_add(1, Ordering::SeqCst);
                    self.session_budgets.lock().unwrap().push(timeout);
                    let read = {
                        let reads = self.reads.lock().unwrap();
                        reads[calls.min(reads.len() - 1)].clone()
                    };
                    if !read.delay.is_zero() {
                        // 端口契约：预算内必须返回；读挂住到期 → not_ready（不超窗）。
                        if tokio::time::timeout(timeout, tokio::time::sleep(read.delay))
                            .await
                            .is_err()
                        {
                            return Err(ZcodeError::of(codes::NOT_READY));
                        }
                    }
                    match read.result {
                        Ok(view) => Ok(view),
                        Err(code) => Err(ZcodeError::of(code)),
                    }
                })
            }

            fn wait_stop_cleared<'a>(
                &'a self,
                _target: &'a BoundTarget,
                execution_id: &'a str,
                baseline_revision: u64,
                timeout: Duration,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    self.wait_calls.lock().unwrap().push((
                        execution_id.to_owned(),
                        baseline_revision,
                        timeout,
                    ));
                    let delay = (*self.wait_delay.lock().unwrap()).min(timeout);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    self.wait_result.load(Ordering::SeqCst)
                })
            }
        }

        fn stop_sender(
            fake: &Arc<FakeBridge>,
            stop: &Arc<FakeStop>,
            store: &DeviceStore,
        ) -> CommandSender {
            sender(fake, store)
                .with_stop(stop.clone())
                .with_confirm_timeout(Duration::from_millis(50))
                // 未知 control 的等待窗压到毫秒级（生产 10s）。
                .with_initial_control_wait(Duration::from_millis(60), Duration::from_millis(5))
        }

        /// 出站帧按信封 `type` 过滤（只读观测）。
        fn envelopes_of_type(fake: &FakeBridge, envelope_type: &str) -> Vec<Value> {
            fake.frames()
                .into_iter()
                .filter_map(|frame| {
                    let envelope = frame.args.first()?.clone();
                    (envelope.get("type").and_then(Value::as_str) == Some(envelope_type))
                        .then_some(envelope)
                })
                .collect()
        }

        fn stored_state(store: &DeviceStore, action_id: &str) -> Option<String> {
            store.with(|conn| {
                conn.query_row(
                    "SELECT state FROM zcode_actions WHERE action_id = ?1",
                    [action_id],
                    |row| row.get(0),
                )
                .ok()
            })
        }

        /// 精确命中的请求返回**宿主观察到的原值**（不挑第一个、不改写）。
        #[test]
        fn authorize_stop_returns_exact_observed_execution() {
            let control = control(vec![
                work("subAgent", None),
                work("primaryTurn", Some("E-existing")),
                work("primaryTurn", Some("E-sibling")),
            ]);
            let id = authorize_stop(&view(Some(control)), &request(Some("E-existing")))
                .expect("an observed foreground execution authorizes stop");
            assert_eq!(id, "E-existing");
            assert_ne!(id, "E-sibling", "must never pick another observed work");
        }

        /// control 未知（宿主未宣告 `activeWorks` 数组）：零 stop，`not_ready`。
        #[tokio::test]
        async fn unknown_control_is_zero_stop() {
            for request in [request(Some("E-existing")), request(None)] {
                let error = authorize_stop(&view(None), &request)
                    .expect_err("unknown control must never authorize a stop");
                assert!(
                    error.is(codes::NOT_READY),
                    "unknown control is not_ready, got {error:?}"
                );
            }

            // 命令层：未知 control → not_ready，零帧零行、零审计；未装配端口同样拒绝。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::unknown();
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(error.is(codes::NOT_READY));
            let unwired = sender(&fake, &store)
                .execute(&ctx("a-2"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(unwired.is(codes::NOT_READY), "未装配 stop 端口必须拒绝");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 显式空 `activeWorks` = 已确认空闲：零 stop（按钮已过时）。
        #[tokio::test]
        async fn idle_control_is_zero_stop() {
            for request in [request(Some("E-existing")), request(None)] {
                let error = authorize_stop(&view(Some(control(Vec::new()))), &request)
                    .expect_err("idle control must never authorize a stop");
                assert!(
                    error.is(codes::STALE_PENDING),
                    "idle control is stale_pending, got {error:?}"
                );
            }

            // 命令层：空闲 → stale_pending（如实回执按钮过时），零帧零行。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::idle();
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(error.is(codes::STALE_PENDING));
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);

            // 显式 `[]` 是**已确认空闲**：一次读即拒绝，绝不空等满 10s 窗。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::idle();
            let sender = sender(&fake, &store)
                .with_stop(stop.clone())
                .with_initial_control_wait(Duration::from_secs(1), Duration::from_millis(25));
            let started = std::time::Instant::now();
            let error = sender
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(error.is(codes::STALE_PENDING));
            assert!(
                started.elapsed() < Duration::from_millis(400),
                "显式 [] 必须立即按空闲拒绝，不消耗等待窗，got {:?}",
                started.elapsed()
            );
            assert_eq!(stop.session_calls(), 1, "显式 [] 一次读即可确认空闲");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
        }

        /// 缺守卫 id 且有多个活动：歧义拒绝，绝不随取第一个。
        #[test]
        fn missing_requested_id_with_multiple_works_is_ambiguous() {
            let control = observed(&["E-a", "E-b"]);
            for absent in [None, Some("")] {
                let error = authorize_stop(&view(Some(control.clone())), &request(absent))
                    .expect_err("missing guard id with multiple works is ambiguous");
                assert!(
                    error.is(codes::TARGET_AMBIGUOUS),
                    "multi-work missing id is target_ambiguous, got {error:?}"
                );
            }

            // 只有一条活动时缺 id 仍是缺必需字段（不是「就停它」）。
            let error = authorize_stop(&view(Some(observed(&["E-a"]))), &request(None))
                .expect_err("the guard id is required");
            assert!(error.is(codes::INVALID_ACTION));
        }

        /// 旧按钮（执行已换）：精确不匹配 → `stop_target_changed`，零 stop。
        #[tokio::test]
        async fn changed_execution_rejects_old_button() {
            let error = authorize_stop(&view(Some(observed(&["E-new"]))), &request(Some("E-old")))
                .expect_err("a stale stop button must be rejected");
            assert!(error.is(codes::STOP_TARGET_CHANGED));

            // 只认精确相等：大小写/前缀都不算命中。
            let error = authorize_stop(&view(Some(observed(&["E-New"]))), &request(Some("E-new")))
                .expect_err("exact match only");
            assert!(error.is(codes::STOP_TARGET_CHANGED));

            // 命令层：点击后被晚到的旧快照确认，写前复核发现执行已换 → 零帧零行。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::sequence(vec![
                view(Some(observed(&["E-old"]))),
                view(Some(observed(&["E-new"]))),
            ]);
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-old"))
                .await
                .unwrap_err();
            assert!(error.is(codes::STOP_TARGET_CHANGED));
            assert_eq!(stop.session_calls(), 2, "写前必须重取同一目标的 control");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 键路径：只停新鲜 control 已观察到的既有执行——**不**先触发新执行。
        #[tokio::test]
        async fn production_stops_observed_existing_execution_without_trigger() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            let send_text_frames = envelopes_of_type(&fake, "sendText");
            let stop_frames = envelopes_of_type(&fake, "stop");
            assert_eq!(send_text_frames.len(), 0, "打断绝不为先触发而合成 sendText");
            assert_eq!(stop_frames.len(), 1, "全生命周期恰一帧");
            assert_eq!(
                stop_frames[0]["payload"]["expectedForegroundExecutionId"],
                serde_json::json!("E-existing")
            );
            assert_eq!(
                stop_frames[0]["payload"]
                    .as_object()
                    .map(serde_json::Map::len),
                Some(1),
                "stop payload 只带 expectedForegroundExecutionId"
            );
            assert_eq!(stop_frames[0]["type"], serde_json::json!(STOP_TYPE));
            assert_eq!(stop_frames[0]["sessionId"], serde_json::json!(SESSION));
            assert_eq!(stop_frames[0]["clientId"], serde_json::json!(DEVICE));
            assert!(stop_frames[0].get("baseRevision").is_none(), "stop 免 CAS");

            let frames = fake.frames();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].channel, "zcode-agent");
            assert_eq!(frames[0].method, AGENT_SEND_COMMAND_METHOD);
            assert_eq!(frames[0].target, target());
            assert_eq!(receipt.state, ActionState::Confirmed);
            assert_eq!(receipt.session_id, SESSION);
            assert_eq!(receipt.generation, GEN);
            assert_eq!(fake.wire(), 1);
            assert_eq!(row_count(&store), 1);
            assert_eq!(stop.session_calls(), 2, "新鲜读取 + 写前复核各一次");

            let waits = stop.wait_calls();
            assert_eq!(waits.len(), 1, "ACK 后必须观察一次宿主终态");
            assert_eq!(waits[0].0, "E-existing");
            assert_eq!(waits[0].1, 4, "确认基线 = 写前复核视图的 control revision");
            assert_eq!(waits[0].2, Duration::from_millis(50));
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("confirmed"));
        }

        /// accepted ACK 只表示受理：宿主仍报目标在场时停在 accepted。
        #[tokio::test]
        async fn ack_does_not_prove_stopped() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::observed(&["E-existing"]);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_ne!(receipt.state, ActionState::Confirmed);
            assert_eq!(stop.wait_calls().len(), 1);
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
        }

        /// 宿主守卫 noop（stopTargetChanged）如实显示未停止，绝不升级 confirmed。
        #[tokio::test]
        async fn noop_stop_target_changed_is_noop() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(serde_json::json!({
                "status": "noop",
                "reasonCode": "guard.stopTargetChanged",
            })));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Noop);
            assert_ne!(receipt.state, ActionState::Accepted, "noop 不是成功");
            assert_ne!(receipt.state, ActionState::Confirmed);
            assert!(receipt.reason_code.is_none(), "宿主原因原文不得下发");
            assert!(stop.wait_calls().is_empty(), "noop 不得观察 confirmed");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("noop"));
        }

        /// 宿主撤掉原目标 → confirmed；仅出现新执行（原目标仍在）→ 停在 accepted。
        #[tokio::test]
        async fn host_clear_confirms_and_new_execution_alone_does_not() {
            // 原目标缺席且 revision 前进（端口证据）→ confirmed。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let confirmed = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            assert_eq!(confirmed.state, ActionState::Confirmed);

            // 场上只有新执行、原目标仍被报告在场：新执行不是目标停止的证明。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut host = view(Some(observed(&["E-existing", "E-new"])));
            host.revision = Some(5);
            let stop = FakeStop::new(vec![view(Some(observed(&["E-existing"]))), host]);
            let accepted = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            assert_eq!(accepted.state, ActionState::Accepted);
            assert_ne!(accepted.state, ActionState::Confirmed);
            assert_eq!(stop.wait_calls()[0].0, "E-existing");
        }

        /// stop 端口视图/动作代际过期：零 stop（不猜、不降级到旧按钮）。
        #[tokio::test]
        async fn stale_generation_is_zero_stop() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::generation(GEN + 1);
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(error.is(codes::STALE_GENERATION));
            assert_eq!(stop.session_calls(), 1);
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());

            // 动作自报的旧代际同样零帧。
            let stale_action = ControlAction::Stop {
                session_id: SESSION.to_owned(),
                generation: GEN + 1,
                expected_foreground_execution_id: "E-existing".to_owned(),
            };
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-2"), &target(), &stale_action)
                .await
                .unwrap_err();
            assert!(error.is(codes::STALE_GENERATION));
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
        }

        /// 打断路径**永不**合成/发送 sendText：成功与无效打断都只允许 stop 帧。
        #[tokio::test]
        async fn stop_never_sends_send_text() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let sender = stop_sender(&fake, &stop, &store);

            sender
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            sender
                .execute(&ctx("a-2"), &target(), &stop_action("E-missing"))
                .await
                .unwrap_err();

            assert_eq!(envelopes_of_type(&fake, "sendText").len(), 0);
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1);
            for frame in fake.frames() {
                assert_eq!(
                    frame.args[0]["type"],
                    serde_json::json!(STOP_TYPE),
                    "stop 路径唯一允许的信封类型"
                );
            }
        }

        /// 宿主在 ACK 前已清掉目标、但没有 accepted ACK → 保守 unknown（不确认）。
        #[tokio::test]
        async fn host_clear_before_ack_is_unknown() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::AckLost);
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Unknown);
            assert_ne!(receipt.state, ActionState::Accepted);
            assert_ne!(receipt.state, ActionState::Confirmed);
            assert!(
                stop.wait_calls().is_empty(),
                "没有 accepted ACK 就不得拿宿主现状升级 confirmed"
            );
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1, "超窗绝不重发");
        }

        /// 同 actionId 同载荷重放：只回既有回执，零第二帧、零新行、零重取 control。
        #[tokio::test]
        async fn stop_reuses_action_id_idempotently() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let sender = stop_sender(&fake, &stop, &store);

            let first = sender
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            let second = sender
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(first.state, ActionState::Confirmed);
            assert_eq!(second.state, ActionState::Confirmed);
            assert_eq!(
                envelopes_of_type(&fake, "stop").len(),
                1,
                "同 id 重放零第二帧"
            );
            assert_eq!(stop.session_calls(), 2, "重放不得重新读取 control");
            assert_eq!(row_count(&store), 1);
        }

        /// 未知 control：最多等到有界窗（生产 10s），超时零 stop、`not_ready`
        /// （上游按 `unknown` 收口——没有可证明零写的 stop），绝不立刻拒绝。
        #[tokio::test]
        async fn unknown_control_waits_then_zero_stop() {
            assert_eq!(
                INITIAL_CONTROL_TIMEOUT,
                Duration::from_secs(10),
                "生产等待窗必须 ≤10s"
            );
            let probe = CommandSender::new(
                FakeBridge::new(Reply::Body(accepted_body())),
                DeviceStore::memory(),
            );
            assert_eq!(
                probe.initial_control_timeout, INITIAL_CONTROL_TIMEOUT,
                "生产构造必须用 10s 等待窗"
            );
            assert_eq!(probe.initial_control_poll, INITIAL_CONTROL_POLL);

            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::unknown();
            let started = std::time::Instant::now();
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();

            assert!(
                error.is(codes::NOT_READY),
                "等待窗耗尽如实回 not_ready，got {error:?}"
            );
            assert_eq!(
                classify_port_error(&error).0,
                ActionState::Unknown,
                "未知 control 的等待超时是保守 unknown（没有任何写出的证据）"
            );
            assert!(
                started.elapsed() >= Duration::from_millis(40),
                "必须在窗前轮询，而不是首个未知视图就拒绝"
            );
            assert!(
                stop.session_calls() >= 2,
                "窗内必须反复读同一目标的 control，got {}",
                stop.session_calls()
            );
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 未知 control 在窗外前变为已确认：照常授权并恰一帧 stop（仍零 sendText）。
        #[tokio::test]
        async fn unknown_control_becomes_confirmed_then_stops() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::sequence(vec![
                view(None),
                view(None),
                view(Some(observed(&["E-existing"]))),
            ]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Confirmed);
            let stop_frames = envelopes_of_type(&fake, "stop");
            assert_eq!(stop_frames.len(), 1, "全生命周期恰一帧");
            assert_eq!(
                stop_frames[0]["payload"]["expectedForegroundExecutionId"],
                serde_json::json!("E-existing")
            );
            assert_eq!(
                stop.session_calls(),
                4,
                "窗前两次未知 + 一次已确认 + 写前复核一次"
            );
            assert_eq!(envelopes_of_type(&fake, "sendText").len(), 0);
            assert_eq!(row_count(&store), 1);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("confirmed"));
        }

        /// stop 的 rejected ACK：如实 `Rejected`，不观察宿主终态、不重发。
        #[tokio::test]
        async fn rejected_ack_stays_rejected_without_host_observation() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(serde_json::json!({
                "status": "rejected",
            })));
            let stop = FakeStop::observed(&["E-existing"]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Rejected);
            assert_ne!(receipt.state, ActionState::Accepted);
            assert_ne!(receipt.state, ActionState::Confirmed);
            assert!(stop.wait_calls().is_empty(), "rejected 不是停止证明");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1, "拒绝后绝不重发");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("rejected"));
        }

        /// 等待已确认 control 期间目标换代：立即零 stop（绝不拿旧代际视图授权，
        /// 也不空转到窗尽）；端口在写前拒绝 = 可证零写（`send_failed`）。
        #[tokio::test]
        async fn mid_wait_generation_change_is_zero_stop() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut moved = view(None);
            moved.generation = GEN + 1;
            let stop = FakeStop::sequence(vec![view(None), moved]);
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();

            assert!(
                error.is(codes::STALE_GENERATION),
                "换代必须如实拒绝，got {error:?}"
            );
            assert_eq!(
                classify_port_error(&error).0,
                ActionState::SendFailed,
                "写前拒绝是可证零写（不是写后 unknown）"
            );
            assert_eq!(stop.session_calls(), 2, "第二次轮询即终止，不空转");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 瞬时不可读（重订阅窗口的 not_ready/offline、ready=false 的旧投影）不是
        /// 结论：等待层必须继续轮询到**同一个**总期限，窗前恢复即照常停止。
        #[tokio::test]
        async fn transient_session_error_does_not_abort_wait() {
            // ① 端口先报两次瞬时错误，再给已确认 control → 恰一帧 stop。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::script(vec![
                StopRead::error(codes::NOT_READY),
                StopRead::error(codes::OFFLINE),
                StopRead::view(view(Some(observed(&["E-existing"])))),
                StopRead::view(view(Some(observed(&["E-existing"])))),
            ]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Confirmed);
            assert_eq!(
                stop.session_calls(),
                4,
                "两次瞬时错误 + 一次已确认 + 写前复核"
            );
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1);
            assert_eq!(envelopes_of_type(&fake, "sendText").len(), 0);
            assert_eq!(row_count(&store), 1);

            // ② `ready=false` 但 control 仍在的旧投影不算已确认（不短路等待窗）。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut stale = view(Some(observed(&["E-existing"])));
            stale.ready = false;
            let stop = FakeStop::sequence(vec![
                stale,
                view(Some(observed(&["E-existing"]))),
                view(Some(observed(&["E-existing"]))),
            ]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-2"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Confirmed);
            assert_eq!(stop.session_calls(), 3, "不新鲜视图后的一次轮询 + 写前复核");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1);

            // ③ 全程瞬时错误：用满窗后 not_ready，零帧零行零审计。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::script(vec![StopRead::error(codes::NOT_READY)]);
            let started = std::time::Instant::now();
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-3"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            assert!(
                error.is(codes::NOT_READY),
                "窗尽如实 not_ready, got {error:?}"
            );
            assert!(
                started.elapsed() >= Duration::from_millis(40),
                "必须持续轮询到窗尽（首个错误不是结论），got {:?}",
                started.elapsed()
            );
            assert!(stop.session_calls() >= 2, "窗内必须反复读");
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 「最多等 10s」是**端到端**界：单次端口读（生产重订阅 RPC）只拿**剩余
        /// 期限**作硬预算，读挂住也必须按期收口，绝不让一个窗口被单读拖成 ~30s。
        #[tokio::test]
        async fn control_wait_respects_total_deadline() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::script(vec![
                StopRead::view(view(None)).delayed(Duration::from_millis(200))
            ]);
            let started = std::time::Instant::now();
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap_err();
            let elapsed = started.elapsed();
            assert!(error.is(codes::NOT_READY), "期限收口, got {error:?}");
            assert!(
                elapsed >= Duration::from_millis(40),
                "至少等到窗前，got {elapsed:?}"
            );
            assert!(
                elapsed < Duration::from_millis(150),
                "单次读必须被剩余期限截断（60ms 窗），got {elapsed:?}"
            );
            assert_eq!(stop.session_calls(), 1, "一次挂住的读即耗尽整个窗");
            let budgets = stop.session_budgets();
            assert_eq!(budgets.len(), 1);
            assert!(
                budgets[0] <= Duration::from_millis(60) && budgets[0] >= Duration::from_millis(50),
                "读预算必须是剩余期限（≤60ms 窗），got {:?}",
                budgets[0]
            );
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 探针 `observeStopTarget` 的 fail-closed 规则：只有 `primaryTurn` 条目
        /// 可作为目标；子 agent/compact/未知 kind 即使 id 精确相等也拒绝。
        #[tokio::test]
        async fn non_primary_work_kind_is_rejected() {
            for kind in ["subAgent", "compact", "workflow", "unknown"] {
                let control = control(vec![work(kind, Some("E-sub"))]);
                let error = authorize_stop(&view(Some(control)), &request(Some("E-sub")))
                    .expect_err("non-primary work kind must never authorize a stop");
                assert!(
                    error.is(codes::STOP_TARGET_CHANGED),
                    "{kind} 条目不得成为 stop 目标，got {error:?}"
                );
            }

            // 同一 id 出现在非 primary 条目里、primary 条目另有真实 id：只认后者。
            let mixed = control(vec![
                work("subAgent", Some("E-sub")),
                work("primaryTurn", Some("E-main")),
            ]);
            assert!(
                authorize_stop(&view(Some(mixed.clone())), &request(Some("E-sub")))
                    .expect_err("subagent id must not be smuggled through")
                    .is(codes::STOP_TARGET_CHANGED)
            );
            assert_eq!(
                authorize_stop(&view(Some(mixed)), &request(Some("E-main")))
                    .expect("the primary turn remains a valid target"),
                "E-main"
            );

            // 缺 id 的歧义只数**可执行**条目：subAgent 不制造「多目标」。
            let one_primary = control(vec![
                work("subAgent", None),
                work("primaryTurn", Some("E-main")),
            ]);
            assert!(authorize_stop(&view(Some(one_primary)), &request(None))
                .expect_err("guard id is required")
                .is(codes::INVALID_ACTION));

            // 命令层：非 primary 的 id 即使精确相等也零帧零行零审计。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let stop = FakeStop::new(vec![view(Some(control(vec![work(
                "subAgent",
                Some("E-sub"),
            )])))]);
            let error = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-sub"))
                .await
                .unwrap_err();
            assert!(error.is(codes::STOP_TARGET_CHANGED));
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 基线 revision 未知时**永不**确认：写前视图没有水位 → 照常发 stop，
        /// 但停在 accepted（绝不拿 0 当基线把后续任一 revision 当「前进」）。
        #[tokio::test]
        async fn unknown_baseline_revision_never_confirms() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let mut no_revision = view(Some(observed(&["E-existing"])));
            no_revision.revision = None;
            let stop = FakeStop::sequence(vec![view(Some(observed(&["E-existing"]))), no_revision]);
            stop.set_wait_result(true);
            let receipt = stop_sender(&fake, &stop, &store)
                .execute(&ctx("a-1"), &target(), &stop_action("E-existing"))
                .await
                .unwrap();

            assert_eq!(
                receipt.state,
                ActionState::Accepted,
                "没有水位就没有因果证据，绝不升级 confirmed"
            );
            assert!(
                stop.wait_calls().is_empty(),
                "基线未知时不得开始确认观察（更不得拿 0 当基线）"
            );
            assert_eq!(envelopes_of_type(&fake, "stop").len(), 1, "写仍然发生");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
        }

        /// 双击打断（同一 execution、新 actionId）且首帧仍在途：命令层无跨 actionId
        /// 去重，第二帧照发；宿主守卫把它当 `stopTargetChanged` → `noop`，回执如实
        /// 显示未停止（**不**伪造 accepted/confirmed）。这是已文档化的已知边界：
        /// 在命令层对从未离机的动作落预留/审计会破坏账本纪律，去重交给手机端防连点
        /// 与宿主幂等。
        #[tokio::test]
        async fn double_tap_new_action_id_defers_to_host_noop() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            fake.queue_reply(Reply::Body(accepted_body()));
            fake.queue_reply(Reply::Body(serde_json::json!({
                "status": "noop",
                "reasonCode": "guard.stopTargetChanged",
            })));
            let stop = FakeStop::observed(&["E-existing"]);
            // 首击 ACK 后停在确认窗内（fake 等待 1s，确认窗 2s）。
            stop.set_wait_delay(Duration::from_secs(1));
            let sender = Arc::new(
                sender(&fake, &store)
                    .with_stop(stop.clone())
                    .with_confirm_timeout(Duration::from_secs(2))
                    .with_initial_control_wait(Duration::from_millis(60), Duration::from_millis(5)),
            );
            let first = tokio::spawn({
                let sender = sender.clone();
                let (ctx, target, action) = (ctx("a-1"), target(), stop_action("E-existing"));
                async move { sender.execute(&ctx, &target, &action).await }
            });
            // 首帧已写出且首击处于确认观察中：此时第二击（新 actionId）不得阻塞。
            let _written = tokio::time::timeout(Duration::from_secs(2), fake.recorded.acquire())
                .await
                .expect("首帧未写出")
                .expect("semaphore");
            let second = tokio::time::timeout(
                Duration::from_secs(2),
                sender.execute(&ctx("a-2"), &target(), &stop_action("E-existing")),
            )
            .await
            .expect("第二击不得阻塞")
            .unwrap();

            assert_eq!(second.state, ActionState::Noop, "宿主守卫 noop 如实显示");
            assert_ne!(second.state, ActionState::Confirmed);
            assert!(second.reason_code.is_none(), "宿主原因原文不得下发");
            assert_eq!(
                envelopes_of_type(&fake, "stop").len(),
                2,
                "已知边界：换 actionId 的重复点击照发（由宿主幂等收口）"
            );
            let first = tokio::time::timeout(Duration::from_secs(3), first)
                .await
                .expect("首击超时")
                .unwrap()
                .unwrap();
            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(envelopes_of_type(&fake, "sendText").len(), 0);
        }
    }

    // ============================================================
    // T5：审批既有卡的安全应答（tests::permission）
    // ============================================================

    mod permission {
        use super::*;

        use serde_json::json;

        use crate::services::zcode_control::types::{
            CapabilityFlag, CapabilityView, PendingKind, PendingView, PermissionOptionView,
        };

        use super::answer::{db_dump, stored_state, FakePending};

        fn option(option_id: &str, label: &str, kind: &str) -> PermissionOptionView {
            PermissionOptionView {
                option_id: option_id.to_owned(),
                label: label.to_owned(),
                kind: kind.to_owned(),
            }
        }

        fn card(interaction_id: &str, options: Vec<PermissionOptionView>) -> PendingView {
            PendingView {
                interaction_id: interaction_id.to_owned(),
                kind: PendingKind::Permission,
                anchor_row_id: Some(41),
                tool_call_id: Some("call_synthetic_4101".to_owned()),
                created_at: Some(1_700_000_000_200),
                summary: Some("synthetic: write a scratch file".to_owned()),
                tool_name: Some("Write".to_owned()),
                schema_interaction: None,
                sensitive: false,
                auto_resolution: false,
                auto_resolution_state: None,
                questions: Vec::new(),
                options,
            }
        }

        fn once_card(interaction_id: &str) -> PendingView {
            card(
                interaction_id,
                vec![option("once-id-9", "Allow once", "allowOnce")],
            )
        }

        /// 带倒计时的权限卡（投影层对所有 kind 都会读 `autoResolution`）。
        fn auto_resolution_card(interaction_id: &str) -> PendingView {
            let mut auto = once_card(interaction_id);
            auto.auto_resolution = true;
            auto.auto_resolution_state = Some("visibleCountdown".to_owned());
            auto
        }

        fn permission_action(interaction_id: &str, option_id: &str) -> ControlAction {
            ControlAction::Permission {
                session_id: SESSION.to_owned(),
                generation: GEN,
                interaction_id: interaction_id.to_owned(),
                option_id: option_id.to_owned(),
            }
        }

        fn pending_with(card: PendingView) -> Arc<FakePending> {
            FakePending::new(target(), card)
        }

        /// 测试用开闸矩阵：只开 `approval_reply`（其余保持默认关闭）。
        fn enabled_approval() -> CapabilityView {
            CapabilityView {
                approval_reply: CapabilityFlag {
                    enabled: true,
                    reason_code: None,
                },
                ..CapabilityView::default()
            }
        }

        fn permission_sender(
            fake: &Arc<FakeBridge>,
            pending: &Arc<FakePending>,
            store: &DeviceStore,
        ) -> CommandSender {
            CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                .with_pending(pending.clone())
                .with_capabilities(enabled_approval())
                .with_confirm_timeout(Duration::from_millis(40))
        }

        fn envelopes(fake: &FakeBridge) -> Vec<Value> {
            fake.frames()
                .into_iter()
                .map(|frame| frame.args[0].clone())
                .collect()
        }

        fn resolve_envelopes(fake: &FakeBridge) -> Vec<Value> {
            envelopes(fake)
                .into_iter()
                .filter(|envelope| envelope["type"] == json!("resolveInteraction"))
                .collect()
        }

        /// 宿主 optionId 是权威：kind 只做白名单，绝不把字面 kind 当 optionId。
        #[test]
        fn kind_is_authority_option_id_is_preserved() {
            let card_with_once_id_custom = card(
                "ix-1",
                vec![
                    option("once-id-9", "Allow once", "allowOnce"),
                    option("deny-id-7", "Deny", "deny"),
                ],
            );

            let allow = build_permission_answer(&card_with_once_id_custom, "once-id-9")
                .expect("allowOnce option is answerable");
            assert_eq!(allow["optionId"], json!("once-id-9"));
            assert_eq!(allow, json!({ "optionId": "once-id-9" }));

            let deny = build_permission_answer(&card_with_once_id_custom, "deny-id-7")
                .expect("deny option is answerable");
            assert_eq!(deny["optionId"], json!("deny-id-7"));

            // 字面 kind 不是本卡的 optionId：绝不硬编码/伪造。
            let fabricated = build_permission_answer(&card_with_once_id_custom, "allowOnce")
                .expect_err("kind literal must never be used as the wire optionId");
            assert!(fabricated.is(codes::FORBIDDEN_OPTION));
        }

        /// 常驻授权与 fullAccess 无论 kind 如何都绝不发出：零帧、零账本、零审计。
        #[tokio::test]
        async fn standing_or_full_access_never_sent() {
            for (option_id, kind) in [
                // fullAccess 特权项即使被标成 allowOnce 也必须拒绝。
                ("fullAccess", "allowOnce"),
                ("always-id", "allowAlways"),
                ("session-id", "allow_session"),
                ("project-id", "allow_project"),
                ("custom-id", "custom"),
                ("mystery-id", "whatever"),
            ] {
                let store = DeviceStore::memory();
                let fake = FakeBridge::new(Reply::Body(accepted_body()));
                let pending = pending_with(card(
                    "ix-1",
                    vec![option(option_id, "Standing grant", kind)],
                ));
                let sender = permission_sender(&fake, &pending, &store);

                let error = sender
                    .execute(
                        &ctx("a-1"),
                        &target(),
                        &permission_action("ix-1", option_id),
                    )
                    .await
                    .expect_err("standing/fullAccess grants are never sent");

                assert_eq!(
                    error.code, "forbidden_option",
                    "kind {kind} must be forbidden"
                );
                assert_eq!(fake.wire(), 0, "被策略拒绝不得写出任何帧");
                assert_eq!(row_count(&store), 0, "本地拒绝不得留下账本行");
                assert!(audits(&store).is_empty(), "本地拒绝不得写审计");
                assert!(!pending.consumed("ix-1"), "零 value 不得消费卡");
                assert!(!pending.inflight("ix-1"), "本地拒绝必须释放在途");
            }
        }

        /// 卡不存在/已撤：零审批帧，绝不复用另一张卡。
        #[tokio::test]
        async fn no_card_means_no_approval() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-other"));
            let sender = permission_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-missing", "once-id-9"),
                )
                .await
                .expect_err("missing card cannot be approved");

            assert_eq!(error.code, "stale_pending");
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 审批闸不因一次成功发送而被推断：默认矩阵关闭，关闭时零帧。
        #[tokio::test]
        async fn approval_gate_is_not_inferred_from_send() {
            let default_capabilities = CapabilityView::default();
            assert!(!default_capabilities.approval_reply.enabled);
            assert!(!default_capabilities.approval_reply_enabled());

            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            // 不调用 `with_capabilities`：默认矩阵 = 审批关闭。
            let sender =
                CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                    .with_pending(pending.clone())
                    .with_confirm_timeout(Duration::from_millis(40));

            let sent = sender
                .execute(&ctx("a-send"), &target(), &send_action("hello"))
                .await
                .expect("普通发送不受审批闸影响");
            assert_eq!(sent.state, ActionState::Accepted);
            let frames_after_send = fake.wire();

            let error = sender
                .execute(
                    &ctx("a-perm"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("approval_reply is closed by default");
            assert_eq!(error.code, "needs_evidence");
            assert_eq!(fake.wire(), frames_after_send, "关闭的审批闸必须零帧");
            assert_eq!(resolve_envelopes(&fake).len(), 0);
            assert!(!pending.inflight("ix-1"));
            assert_eq!(stored_state(&store, "a-perm"), None, "关闭路径零账本行");
        }

        /// T8.2 证据许可只放宽能力闸：`fullAccess`/常驻选项与目标/代际守卫全部原样拒绝。
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        #[tokio::test]
        async fn evidence_permit_does_not_bypass_full_access_or_target_guards() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(card(
                "ix-1",
                vec![option("fullAccess", "Full access", "fullAccess")],
            ));
            // 默认矩阵 = 审批关闭；许可由 Hub 消费后以 `permits` 传入本派发器。
            let sender =
                CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                    .with_pending(pending.clone())
                    .with_confirm_timeout(Duration::from_millis(40));
            let permits = ["approval_reply"];

            // fullAccess 特权项：许可在场也必须 forbidden_option，零帧零行零审计。
            let error = sender
                .execute_with_permits(
                    &ctx("evidence-full"),
                    &target(),
                    &permission_action("ix-1", "fullAccess"),
                    &permits,
                )
                .await
                .expect_err("fullAccess 不因许可而放宽");
            assert!(error.is(codes::FORBIDDEN_OPTION), "{:?}", error.code);
            assert_eq!(resolve_envelopes(&fake).len(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
            assert!(!pending.inflight("ix-1"), "本地拒绝必须释放在途");

            // 目标/代际守卫先于能力闸：旧代际动作即便带许可也 stale_generation。
            let stale = ControlAction::Permission {
                session_id: SESSION.to_owned(),
                generation: GEN + 1,
                interaction_id: "ix-1".to_owned(),
                option_id: "fullAccess".to_owned(),
            };
            let error = sender
                .execute_with_permits(&ctx("evidence-stale"), &target(), &stale, &permits)
                .await
                .unwrap_err();
            assert!(error.is(codes::STALE_GENERATION));

            // 他会话动作同样先被目标校验拒绝（invalid_action），许可不参与。
            let wrong_session = ControlAction::Permission {
                session_id: "sess_other".to_owned(),
                generation: GEN,
                interaction_id: "ix-1".to_owned(),
                option_id: "fullAccess".to_owned(),
            };
            let error = sender
                .execute_with_permits(&ctx("evidence-other"), &target(), &wrong_session, &permits)
                .await
                .unwrap_err();
            assert!(error.is(codes::INVALID_ACTION));

            // 许可只属于它自己的能力：别的能力名不得开审批闸。
            let error = sender
                .execute_with_permits(
                    &ctx("evidence-wrong-cap"),
                    &target(),
                    &permission_action("ix-1", "fullAccess"),
                    &["single_select"],
                )
                .await
                .unwrap_err();
            assert!(error.is(codes::NEEDS_EVIDENCE));
            assert_eq!(fake.wire(), 0, "所有拒绝路径零帧");
        }

        /// 开闸后：恰一帧 resolve；ACK 只算 accepted，宿主撤卡才升级 confirmed。
        #[tokio::test]
        async fn enabled_permission_reply_sends_one_resolve() {
            // ① ACK accepted 但宿主仍列卡：只报 accepted。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);
            let accepted = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect("enabled approval sends one resolve");
            assert_eq!(accepted.state, ActionState::Accepted);
            assert!(pending.consumed("ix-1"), "ACK 受理后本地必须消费");
            assert!(
                pending.host_has_pending(&target(), "ix-1"),
                "本地消费不得改动宿主集合"
            );
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));

            let frames = resolve_envelopes(&fake);
            assert_eq!(frames.len(), 1, "恰一帧 resolveInteraction");
            let envelope = &frames[0];
            assert_eq!(envelope["type"], json!("resolveInteraction"));
            assert_eq!(envelope["payload"]["interactionId"], json!("ix-1"));
            assert_eq!(
                envelope["payload"]["answer"],
                json!({ "optionId": "once-id-9" }),
                "应答必须是宿主原值 optionId"
            );
            assert_eq!(envelope["clientId"], json!(DEVICE));
            assert_eq!(envelope["sessionId"], json!(SESSION));
            assert_eq!(
                fake.observed_timeouts(),
                vec![Duration::from_millis(20)],
                "ACK 窗原样交给端口"
            );

            // ② ACK 后宿主撤卡（代数前进 + id 消失）才升级 confirmed。
            let store2 = DeviceStore::memory();
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = pending_with(once_card("ix-1"));
            pending2.clear_on_wait();
            let sender2 = permission_sender(&fake2, &pending2, &store2);
            let confirmed = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect("enabled approval sends one resolve");
            assert_eq!(confirmed.state, ActionState::Confirmed);
            assert!(!pending2.host_has_pending(&target(), "ix-1"));
            assert_eq!(stored_state(&store2, "a-2").as_deref(), Some("confirmed"));
        }

        /// optionId 不在本卡：零帧（绝不按相似度猜或回退到别的选项）。
        #[tokio::test]
        async fn option_not_in_card_is_rejected() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(card(
                "ix-1",
                vec![option("custom-once", "Allow once", "allowOnce")],
            ));
            let sender = permission_sender(&fake, &pending, &store);

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("option outside the card is never answerable");

            assert_eq!(error.code, "forbidden_option");
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
            assert!(!pending.consumed("ix-1"));
            assert!(!pending.inflight("ix-1"));
        }

        /// autoResolution 卡（决策 §8.2：每个交互 run 先暂停计时）：必须先 snooze
        /// 恰一次，且已暂停（accepted/duplicate/noop 均由端口折叠为 Ok）后才 resolve。
        #[tokio::test]
        async fn auto_resolution_permission_snoozes_once_then_resolves() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(auto_resolution_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect("autoResolution permission replies must snooze first");

            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(
                pending.snooze_calls(),
                1,
                "autoResolution 在场恰一次 snooze"
            );
            assert_eq!(
                resolve_envelopes(&fake).len(),
                1,
                "snooze 证实后恰一帧 resolve"
            );
        }

        /// snooze 未证实（失败/宿主已撤卡）：零 resolve，绝不与倒计时竞速。
        #[tokio::test]
        async fn permission_snooze_unconfirmed_zero_resolve() {
            // ① snooze 失败（超窗/unknown）：回执保守 unknown，零 resolve、释放在途。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(auto_resolution_card("ix-1"));
            pending.set_snooze_error(codes::UNKNOWN);
            let sender = permission_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(receipt.reason_code.as_deref(), Some(codes::UNKNOWN));
            assert_eq!(pending.snooze_calls(), 1, "snooze 恰一帧");
            assert_eq!(resolve_envelopes(&fake).len(), 0, "snooze 失败不得 resolve");
            assert!(!pending.inflight("ix-1"), "resolve 零写必须释放在途");

            // ② snooze 受理但宿主集合已无该卡：仍零 resolve。
            let store2 = DeviceStore::memory();
            let fake2 = FakeBridge::new(Reply::Body(accepted_body()));
            let pending2 = pending_with(auto_resolution_card("ix-1"));
            pending2.clear_host_card();
            let sender2 = permission_sender(&fake2, &pending2, &store2);
            let receipt2 = sender2
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .unwrap();
            assert_eq!(receipt2.state, ActionState::Unknown);
            assert_eq!(receipt2.reason_code.as_deref(), Some(codes::STALE_PENDING));
            assert_eq!(pending2.snooze_calls(), 1);
            assert_eq!(resolve_envelopes(&fake2).len(), 0);
        }

        /// 宿主在 ACK 前撤卡且无 accepted：只能 unknown/stale，不消费。
        #[tokio::test]
        async fn host_withdraw_before_ack_is_unknown() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({ "status": "noop" })));
            let pending = pending_with(once_card("ix-1"));
            // 宿主已撤卡（本地投影尚未跟上）。
            pending.clear_host_card();
            let sender = permission_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Unknown);
            assert_eq!(receipt.reason_code.as_deref(), Some(codes::STALE_PENDING));
            assert_eq!(
                resolve_envelopes(&fake).len(),
                1,
                "帧已写出，只收口为 unknown"
            );
            assert!(!pending.consumed("ix-1"), "未受理不得本地消费");
        }

        /// resolve ACK rejected/noop 且卡仍在宿主集合：释放在途，换新 actionId 可重试。
        #[tokio::test]
        async fn rejected_or_noop_resolve_releases_inflight() {
            for (index, status) in ["rejected", "noop"].into_iter().enumerate() {
                let store = DeviceStore::memory();
                let fake = FakeBridge::new(Reply::Body(json!({ "status": status })));
                let pending = pending_with(once_card("ix-1"));
                let sender = permission_sender(&fake, &pending, &store);
                let first = sender
                    .execute(
                        &ctx(&format!("a-{index}-1")),
                        &target(),
                        &permission_action("ix-1", "once-id-9"),
                    )
                    .await
                    .unwrap();
                let expected = if status == "rejected" {
                    ActionState::Rejected
                } else {
                    ActionState::Noop
                };
                assert_eq!(first.state, expected, "{status} 如实分类");
                assert!(
                    !pending.inflight("ix-1"),
                    "{status} 可证明未生效必须释放在途"
                );
                assert_eq!(resolve_envelopes(&fake).len(), 1);

                // 宿主仍列卡：新 actionId 可重试，绝不自动重发。
                fake.set_reply(Reply::Body(accepted_body()));
                let retry = sender
                    .execute(
                        &ctx(&format!("a-{index}-2")),
                        &target(),
                        &permission_action("ix-1", "once-id-9"),
                    )
                    .await
                    .unwrap();
                assert_eq!(retry.state, ActionState::Accepted, "{status} 后可重试");
                assert_eq!(resolve_envelopes(&fake).len(), 2);
            }
        }

        /// 桥确证零写（绑定/编码拒绝）→ `send_failed` 且释放在途，用户可重试。
        #[tokio::test]
        async fn send_failed_resolve_releases_inflight() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Error(codes::STALE_GENERATION));
            let pending = pending_with(once_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);

            let failed = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .unwrap();
            assert_eq!(failed.state, ActionState::SendFailed);
            assert_eq!(failed.reason_code.as_deref(), Some(codes::STALE_GENERATION));
            assert!(!pending.inflight("ix-1"), "确证零写必须释放在途");

            fake.set_reply(Reply::Body(accepted_body()));
            let retry = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .unwrap();
            assert_eq!(retry.state, ActionState::Accepted);
            assert_eq!(resolve_envelopes(&fake).len(), 2, "恰一帧/次，无自动重发");
        }

        /// 闸开启但未装配 pending 端口：`not_ready` 且零帧零行零审计。
        #[tokio::test]
        async fn gate_open_without_pending_port_is_not_ready_zero_frames() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let sender =
                CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                    .with_capabilities(enabled_approval());

            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("no pending port can prove the card exists");
            assert_eq!(error.code, "not_ready");
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());
        }

        /// 闸收回后重放已存储的动作：仍回既有回执（同 actionId 幂等优先于闸的
        /// 开启状态）；只有**新**动作在闸关闭时才 `needs_evidence`。
        #[tokio::test]
        async fn gate_closed_replay_returns_stored_receipt() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            let action = permission_action("ix-1", "once-id-9");
            let open = permission_sender(&fake, &pending, &store);
            let first = open
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .expect("enabled approval");
            assert_eq!(first.state, ActionState::Accepted);

            let frames_after_first = fake.wire();
            let rows_after_first = row_count(&store);
            // 闸收回后的派发器：同一持久库、同一 pending 端口，不调用 with_capabilities。
            let closed =
                CommandSender::with_timeout(fake.clone(), store.clone(), Duration::from_millis(20))
                    .with_pending(pending.clone());
            let replay = closed
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .expect("stored receipt must be replayed even with the gate closed");
            assert_eq!(replay, first, "同 actionId 同载荷必须回既有回执");
            assert_eq!(fake.wire(), frames_after_first, "重放零新帧");
            assert_eq!(row_count(&store), rows_after_first, "重放零新行");

            // 新 actionId 仍被闸拦下：零帧零行。
            let blocked = closed
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("new actions stay closed");
            assert_eq!(blocked.code, "needs_evidence");
            assert_eq!(fake.wire(), frames_after_first, "关闭闸零新帧");
            assert_eq!(row_count(&store), rows_after_first, "关闭闸零新行");
        }

        /// 已消费/已撤的旧卡：零新帧，绝不复用另一个 actionId 重发。
        #[tokio::test]
        async fn stale_permission_card_is_zero_frames() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);

            let first = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect("first approval");
            assert_eq!(first.state, ActionState::Accepted);
            assert_eq!(resolve_envelopes(&fake).len(), 1);

            // 同一张卡第二次提交（新 actionId）：本地已消费 → stale，零新帧。
            let reused = sender
                .execute(
                    &ctx("a-2"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("consumed card cannot be answered twice");
            assert_eq!(reused.code, "stale_pending");
            assert_eq!(resolve_envelopes(&fake).len(), 1);

            // 宿主帧撤卡后同样零帧、不复活。
            pending.host_frame_without_card();
            let removed = sender
                .execute(
                    &ctx("a-3"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect_err("host-removed card is not resurrected");
            assert_eq!(removed.code, "stale_pending");
            assert_eq!(resolve_envelopes(&fake).len(), 1);
        }

        /// 同 actionId 同载荷重放：只回既有回执，零第二帧；换选项即冲突。
        #[tokio::test]
        async fn duplicate_permission_reply_idempotent() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);
            let action = permission_action("ix-1", "once-id-9");

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .expect("first approval");
            let second = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .expect("duplicate actionId replays the receipt");
            assert_eq!(second, first, "同 actionId 同载荷返回既有回执");
            assert_eq!(resolve_envelopes(&fake).len(), 1, "重放零第二帧");
            assert_eq!(row_count(&store), 1);

            // 同 actionId 换 optionId：载荷冲突，零第二帧。
            let conflict = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "deny-id-7"),
                )
                .await
                .expect_err("same actionId with a different option is a conflict");
            assert_eq!(conflict.code, "action_payload_conflict");
            assert_eq!(resolve_envelopes(&fake).len(), 1);
        }

        /// 审批永不合成 sendText / 改会话权限模式；账本只留指纹与计数。
        #[tokio::test]
        async fn permission_never_sends_send_text_or_changes_mode() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let pending = pending_with(once_card("ix-1"));
            let sender = permission_sender(&fake, &pending, &store);

            let receipt = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &permission_action("ix-1", "once-id-9"),
                )
                .await
                .expect("one approval resolve");
            assert_eq!(receipt.state, ActionState::Accepted);

            let frames = envelopes(&fake);
            assert_eq!(frames.len(), 1, "一次审批只允许一帧");
            assert_eq!(frames[0]["type"], json!("resolveInteraction"));
            let payload = frames[0]["payload"].as_object().expect("payload object");
            let mut payload_keys: Vec<&str> = payload.keys().map(String::as_str).collect();
            payload_keys.sort_unstable();
            assert_eq!(payload_keys, vec!["answer", "interactionId"]);
            let answer = payload["answer"].as_object().expect("answer object");
            let mut answer_keys: Vec<&str> = answer.keys().map(String::as_str).collect();
            answer_keys.sort_unstable();
            assert_eq!(answer_keys, vec!["optionId"], "绝不附 freeText/模式字段");

            let wire = serde_json::to_string(&frames).expect("serialize frames");
            for forbidden in ["sendText", "\"mode\"", "permissionMode", "fullAccess"] {
                assert!(
                    !wire.contains(forbidden),
                    "审批帧不得携带 {forbidden}: {wire}"
                );
            }
            assert!(frames[0].get("mode").is_none());

            // 审计/账本只记 kind+计数，不摘选项原值/标签。
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
            let summary: String = store.with(|conn| {
                conn.query_row(
                    "SELECT summary FROM zcode_actions WHERE action_id = 'a-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            });
            assert_eq!(summary, "permission options=1");
            let audit_rows = audits(&store);
            assert!(
                !audit_rows.is_empty(),
                "审批必须真的写过审计（空表上的 all() 是空洞的真）"
            );
            assert!(
                audit_rows
                    .iter()
                    .all(|row| row.summary == "permission options=1"),
                "审计摘要只含类别与选项数"
            );
            let dump = db_dump(&store);
            for forbidden in ["once-id-9", "Allow once"] {
                assert!(
                    !dump.contains(forbidden),
                    "选项原值/标签不得落库: {forbidden}"
                );
            }
        }
    }

    // ============================================================
    // T6 Slice B：同账号模型/思考档位切换（tests::selection）
    // ============================================================

    mod selection {
        use super::*;

        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use serde_json::json;

        use crate::services::zcode_control::selection::SWITCH_MODEL_CONFIG_TYPE;
        use crate::services::zcode_control::types::{
            CapabilityFlag, CapabilityView, ModelChoice, SelectionView, SessionView,
        };

        use super::answer::stored_state;

        const PROVIDER: &str = "acct-current";
        const REVISION: u64 = 7;
        const WAIT_WINDOW: Duration = Duration::from_millis(20);

        /// 一次 `wait_selection` 观测（provider, model, thought, baseline, timeout）。
        type WaitCall = (String, String, String, u64, Duration);

        fn choice(model_id: &str, levels: &[&str]) -> ModelChoice {
            ModelChoice {
                model_id: model_id.to_owned(),
                thought_levels: levels.iter().map(|level| (*level).to_owned()).collect(),
            }
        }

        fn selection(
            model: &str,
            thought: Option<&str>,
            models: Vec<ModelChoice>,
        ) -> SelectionView {
            SelectionView {
                provider: PROVIDER.to_owned(),
                model: model.to_owned(),
                thought: thought.map(str::to_owned),
                models,
            }
        }

        /// 当前 provider 目录：model-a 支持 low/high，model-b 支持 low。
        fn catalog() -> SelectionView {
            selection(
                "model-a",
                Some("low"),
                vec![
                    choice("model-a", &["low", "high"]),
                    choice("model-b", &["low"]),
                ],
            )
        }

        fn view(revision: Option<u64>, selection: SelectionView) -> SessionView {
            SessionView {
                session_id: SESSION.to_owned(),
                generation: GEN,
                revision,
                ready: true,
                pending: Vec::new(),
                control: None,
                selection: Some(selection),
                capabilities: CapabilityView::default(),
            }
        }

        fn selection_action(model: &str, thought: &str) -> ControlAction {
            ControlAction::Selection {
                session_id: SESSION.to_owned(),
                generation: GEN,
                model: model.to_owned(),
                thought: thought.to_owned(),
            }
        }

        /// 选择端口 fake：脚本化的（视图, 目录）+ 脚本化的新鲜确认结果。
        struct FakeSelection {
            view: Mutex<SessionView>,
            selection: Mutex<SelectionView>,
            load_calls: AtomicUsize,
            wait_result: AtomicBool,
            wait_calls: Mutex<Vec<WaitCall>>,
        }

        impl FakeSelection {
            fn new(view: SessionView, selection: SelectionView) -> Arc<Self> {
                Arc::new(Self {
                    view: Mutex::new(view),
                    selection: Mutex::new(selection),
                    load_calls: AtomicUsize::new(0),
                    wait_result: AtomicBool::new(false),
                    wait_calls: Mutex::new(Vec::new()),
                })
            }

            fn set_wait_result(&self, result: bool) {
                self.wait_result.store(result, Ordering::SeqCst);
            }

            /// 模拟宿主已生效：revision 前进、当前 provider/model/thought 变为目标值。
            fn set_state(&self, view: SessionView, selection: SelectionView) {
                *self.view.lock().unwrap() = view;
                *self.selection.lock().unwrap() = selection;
            }

            fn load_calls(&self) -> usize {
                self.load_calls.load(Ordering::SeqCst)
            }

            fn wait_calls(&self) -> Vec<WaitCall> {
                self.wait_calls.lock().unwrap().clone()
            }
        }

        impl SelectionPort for FakeSelection {
            fn load<'a>(
                &'a self,
                _target: &'a BoundTarget,
            ) -> BoxFuture<'a, ZResult<(SessionView, SelectionView)>> {
                Box::pin(async move {
                    self.load_calls.fetch_add(1, Ordering::SeqCst);
                    Ok((
                        self.view.lock().unwrap().clone(),
                        self.selection.lock().unwrap().clone(),
                    ))
                })
            }

            fn wait_selection<'a>(
                &'a self,
                _target: &'a BoundTarget,
                provider: &'a str,
                model: &'a str,
                thought: &'a str,
                baseline_revision: u64,
                timeout: Duration,
            ) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    self.wait_calls.lock().unwrap().push((
                        provider.to_owned(),
                        model.to_owned(),
                        thought.to_owned(),
                        baseline_revision,
                        timeout,
                    ));
                    self.wait_result.load(Ordering::SeqCst)
                })
            }
        }

        fn open_capabilities() -> CapabilityView {
            CapabilityView {
                model: CapabilityFlag {
                    enabled: true,
                    reason_code: None,
                },
                thought_level: CapabilityFlag {
                    enabled: true,
                    reason_code: None,
                },
                ..CapabilityView::default()
            }
        }

        fn selection_sender(
            fake: &Arc<FakeBridge>,
            port: &Arc<FakeSelection>,
            store: &DeviceStore,
        ) -> CommandSender {
            sender(fake, store)
                .with_capabilities(open_capabilities())
                .with_selection(port.clone())
                .with_selection_confirm_timeout(WAIT_WINDOW)
        }

        fn switch_frames(fake: &FakeBridge) -> Vec<Value> {
            fake.frames()
                .into_iter()
                .filter_map(|frame| {
                    let envelope = frame.args.first()?.clone();
                    (envelope.get("type").and_then(Value::as_str) == Some(SWITCH_MODEL_CONFIG_TYPE))
                        .then_some(envelope)
                })
                .collect()
        }

        fn stored_reason(store: &DeviceStore, action_id: &str) -> Option<String> {
            store.with(|conn| {
                conn.query_row(
                    "SELECT reason_code FROM zcode_actions WHERE action_id = ?1",
                    [action_id],
                    |row| row.get(0),
                )
                .ok()
            })
        }

        /// CAS 只带 baseRevision、provider 只取自视图、绝不恢复原 thought。
        #[tokio::test]
        async fn selection_uses_cas_and_never_restores() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            port.set_wait_result(true);
            let sender = selection_sender(&fake, &port, &store);

            let receipt = sender
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap();

            assert_eq!(receipt.state, ActionState::Confirmed);
            let frames = switch_frames(&fake);
            assert_eq!(frames.len(), 1, "恰一帧：绝不回带恢复原值的写帧");
            let envelope = &frames[0];
            assert_eq!(envelope.get("baseRevision"), Some(&Value::from(REVISION)));
            assert!(envelope.get("baseLogEpoch").is_none());
            assert_eq!(envelope["payload"]["provider"], Value::from(PROVIDER));
            assert_eq!(envelope["payload"]["model"], Value::from("model-a"));
            assert_eq!(envelope["payload"]["thought"], Value::from("high"));
            assert!(
                frames
                    .iter()
                    .all(|frame| frame["payload"]["thought"].as_str() != Some("low")),
                "产品保留用户选择：任何帧都不得把原 thought 写回"
            );
            // 确认基线 = 写前视图 revision，等待目标 = 精确三项。
            assert_eq!(
                port.wait_calls(),
                vec![(
                    PROVIDER.to_owned(),
                    "model-a".to_owned(),
                    "high".to_owned(),
                    REVISION,
                    WAIT_WINDOW
                )]
            );
        }

        /// 陈旧 CAS：not_ready（刷新提示），零重发、零确认观察，同 id 重放也只回既有回执。
        #[tokio::test]
        async fn stale_cas_is_not_retried() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(json!({
                "status": "stale",
                "reasonCode": "proto.staleRevision",
            })));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let sender = selection_sender(&fake, &port, &store);
            let action = selection_action("model-a", "high");

            let error = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap_err();
            assert!(error.is(codes::NOT_READY), "陈旧 CAS 只提示刷新");
            assert_eq!(switch_frames(&fake).len(), 1, "绝不自动换新 revision 重发");
            assert!(port.wait_calls().is_empty(), "陈旧 CAS 不得进入确认观察");
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("rejected"));
            // 账本原因与返回码同源：rejected 终态的解释是「刷新后重试」。
            assert_eq!(
                stored_reason(&store, "a-1").as_deref(),
                Some(codes::NOT_READY),
                "落账原因必须与返回的 not_ready 一致"
            );

            // 同 actionId 重放：只回既有回执（state/reason 与落账一致），零新帧、零等待。
            let replay = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            assert_eq!(replay.state, ActionState::Rejected);
            assert_eq!(replay.reason_code.as_deref(), Some(codes::NOT_READY));
            assert_eq!(switch_frames(&fake).len(), 1);
            assert!(port.wait_calls().is_empty());
        }

        /// ACK 只算 accepted；只有新鲜且精确相等才 confirmed。
        #[tokio::test]
        async fn ack_only_is_accepted_fresh_selection_is_confirmed() {
            // 窗内没有新鲜目标 → accepted。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let sender = selection_sender(&fake, &port, &store);
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Accepted);
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("accepted"));
            assert_eq!(port.wait_calls().len(), 1);

            // 新鲜 revision + 三项精确相等 → confirmed。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            port.set_wait_result(true);
            let sender = selection_sender(&fake, &port, &store);
            let receipt = sender
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap();
            assert_eq!(receipt.state, ActionState::Confirmed);
            assert!(receipt.reason_code.is_none());
            assert_eq!(stored_state(&store, "a-1").as_deref(), Some("confirmed"));
        }

        /// 默认能力矩阵关闭：needs_evidence、零帧零行、连目录都不读。
        #[tokio::test]
        async fn selection_gate_default_closed_zero_frames() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let closed = sender(&fake, &store).with_selection(port.clone());

            let error = closed
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap_err();

            assert!(error.is(codes::NEEDS_EVIDENCE));
            assert_eq!(fake.wire(), 0);
            assert_eq!(port.load_calls(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());

            // 只开 model、thought_level 仍关：同样拒绝（两闸同时开启才放行）。
            let half = CapabilityView {
                model: CapabilityFlag {
                    enabled: true,
                    reason_code: None,
                },
                ..CapabilityView::default()
            };
            let sender = sender(&fake, &store)
                .with_capabilities(half)
                .with_selection(port.clone());
            let error = sender
                .execute(&ctx("a-2"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap_err();
            assert!(error.is(codes::NEEDS_EVIDENCE));
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);
        }

        /// 跨 provider 模型与空档位列表：构造器拒绝，零帧零行（预留之前）。
        #[tokio::test]
        async fn selection_rejects_cross_provider_or_empty_levels() {
            // 视图只含当前 provider 目录：另一 provider 的模型名不可见。
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let sender = selection_sender(&fake, &port, &store);
            let error = sender
                .execute(
                    &ctx("a-1"),
                    &target(),
                    &selection_action("model-other-account", "low"),
                )
                .await
                .unwrap_err();
            assert!(error.is(codes::INVALID_ACTION));
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);
            assert!(audits(&store).is_empty());

            // 空档位列表 = 宿主未宣告：任何 thought 都拒绝。
            let empty = selection("model-a", None, vec![choice("model-a", &[])]);
            let port = FakeSelection::new(view(Some(REVISION), empty.clone()), empty);
            let sender = selection_sender(&fake, &port, &store);
            let error = sender
                .execute(&ctx("a-2"), &target(), &selection_action("model-a", "low"))
                .await
                .unwrap_err();
            assert!(error.is(codes::INVALID_ACTION));
            assert_eq!(fake.wire(), 0);
            assert_eq!(row_count(&store), 0);

            // 正对照：同一目录里的合法目标仍可构造并写出（拒绝不是因为整体失效）。
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let sender = selection_sender(&fake, &port, &store);
            let accepted = sender
                .execute(&ctx("a-3"), &target(), &selection_action("model-a", "low"))
                .await
                .unwrap();
            assert_eq!(accepted.state, ActionState::Accepted);
            assert_eq!(fake.wire(), 1);
        }

        /// 恰一帧官方 switchModelConfig：channel/method/信封/窗口都与探针一致。
        #[tokio::test]
        async fn selection_sends_one_switch_command() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            let sender = selection_sender(&fake, &port, &store);

            sender
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "high"))
                .await
                .unwrap();

            let frames = fake.frames();
            assert_eq!(frames.len(), 1, "全生命周期只允许一个命令信封");
            assert_eq!(frames[0].channel, AGENT_CHANNEL);
            assert_eq!(frames[0].method, AGENT_SEND_COMMAND_METHOD);
            assert_eq!(frames[0].args.len(), 1);
            let envelope = &frames[0].args[0];
            assert_eq!(
                envelope.get("type").and_then(Value::as_str),
                Some(SWITCH_MODEL_CONFIG_TYPE)
            );
            assert_eq!(
                envelope["payload"],
                json!({ "provider": PROVIDER, "model": "model-a", "thought": "high" })
            );
            assert_eq!(envelope.get("baseRevision"), Some(&Value::from(REVISION)));
            assert!(envelope.get("baseLogEpoch").is_none());
            assert_eq!(envelope["clientId"], Value::from(DEVICE));
            assert_eq!(envelope["sessionId"], Value::from(SESSION));
            assert!(envelope["commandId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()));
            assert!(envelope["issuedAt"].as_u64().is_some());
            // 20s ACK 窗原样交给端口。
            assert_eq!(fake.observed_timeouts(), vec![Duration::from_millis(20)]);
            assert_eq!(fake.wire(), 1, "绝无 sendText/恢复帧");
        }

        /// 同 actionId 同载荷：只回既有回执，零第二帧；换载荷 → 载荷冲突。
        #[tokio::test]
        async fn selection_reuses_action_id_idempotently() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            port.set_wait_result(true);
            let sender = selection_sender(&fake, &port, &store);
            let action = selection_action("model-a", "high");

            let first = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            let second = sender
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();

            assert_eq!(first.state, ActionState::Confirmed);
            assert_eq!(second.state, ActionState::Confirmed);
            assert_eq!(switch_frames(&fake).len(), 1, "重放不得产生第二帧");
            assert_eq!(port.wait_calls().len(), 1, "重放不得再次等待确认");
            assert_eq!(row_count(&store), 1);

            // 同 id 换 thought：载荷冲突（身份 = 会话 + 代际 + model/thought）。
            let conflict = sender
                .execute(&ctx("a-1"), &target(), &selection_action("model-a", "low"))
                .await
                .unwrap_err();
            assert_eq!(conflict.code, codes::ACTION_PAYLOAD_CONFLICT);
            assert_eq!(switch_frames(&fake).len(), 1);
            assert_eq!(row_count(&store), 1);
        }

        /// 切换已生效（revision 前进）后，同 actionId 同载荷重试必须回既有回执：
        /// 零新帧、零新目录读取、零第二次等待；当前闸/端口新鲜度不再参与。
        #[tokio::test]
        async fn same_action_id_retry_after_revision_advance_returns_receipt() {
            let store = DeviceStore::memory();
            let fake = FakeBridge::new(Reply::Body(accepted_body()));
            let port = FakeSelection::new(view(Some(REVISION), catalog()), catalog());
            port.set_wait_result(true);
            let open = selection_sender(&fake, &port, &store);
            let action = selection_action("model-a", "high");

            let first = open.execute(&ctx("a-1"), &target(), &action).await.unwrap();
            assert_eq!(first.state, ActionState::Confirmed);
            assert_eq!(switch_frames(&fake).len(), 1);
            assert_eq!(port.load_calls(), 1);

            // 宿主已生效：revision 前进，当前值已是目标。
            let applied = selection(
                "model-a",
                Some("high"),
                vec![
                    choice("model-a", &["low", "high"]),
                    choice("model-b", &["low"]),
                ],
            );
            port.set_state(view(Some(REVISION + 1), applied.clone()), applied.clone());

            // 同 actionId 同载荷重放：命中账本，不得重新构造（baseRevision 已变）。
            let replay = open.execute(&ctx("a-1"), &target(), &action).await.unwrap();
            assert_eq!(replay, first, "重放必须回既有回执（confirmed）");
            assert_eq!(switch_frames(&fake).len(), 1, "重放不得产生第二帧");
            assert_eq!(port.load_calls(), 1, "重放不得再次读取目录");
            assert_eq!(port.wait_calls().len(), 1, "重放不得再次等待确认");
            assert_eq!(row_count(&store), 1);

            // 端口视图暂不可用（not ready）：重放仍须回既有回执，不得报 not_ready。
            let mut not_ready = view(Some(REVISION + 2), applied.clone());
            not_ready.ready = false;
            port.set_state(not_ready, applied.clone());
            let replay = open.execute(&ctx("a-1"), &target(), &action).await.unwrap();
            assert_eq!(replay, first, "视图 not_ready 不得吞掉既有回执");
            assert_eq!(switch_frames(&fake).len(), 1);
            assert_eq!(port.load_calls(), 1);

            // 能力闸收回且端口未装配：只读查询先行，仍回既有回执。
            let closed = sender(&fake, &store);
            let replay = closed
                .execute(&ctx("a-1"), &target(), &action)
                .await
                .unwrap();
            assert_eq!(replay, first, "闸收回不得把「做过」误报成「没做过」");
            assert_eq!(switch_frames(&fake).len(), 1);
            assert_eq!(row_count(&store), 1);
        }
    }
}
