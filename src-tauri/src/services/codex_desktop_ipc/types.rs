//! Codex Desktop IPC 公共类型（计划「文件结构与公共接口总约定」的唯一实现处，后文引用不重定义）。
//!
//! serde 约定：结构体字段 camelCase，枚举值 snake_case；这些类型直接进入远程
//! HTTP/SSE DTO（`remote/codex_ipc.rs`），前端 TS DTO 与此处逐字段对齐。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 本机 Thread 复合键。`host_id` 服务端只构造 `local`（探测定案：`durable`/`remote-*`
/// 恒 `no-client-found`），不接受远程输入。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    #[serde(rename = "hostId")]
    pub host_id: String,
    #[serde(rename = "threadId")]
    pub thread_id: String,
}

/// 投递结果。`NotSent` = 保守判定**零投递**（写尝试开始前即失败）；`UnknownDelivery`
/// = 已发出但结果未知——**禁止**自动重试、禁止自动 fallback（spec S5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    NotSent,
    Accepted,
    Rejected,
    UnknownDelivery,
}

/// 动作类型。只承载真正可执行的动作，未知协议类别不进这里（投影里走
/// `PendingKind::Unsupported`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Send,
    CommandApproval,
    PermissionsApproval,
    UserInput,
}

/// 动作账本状态（`codex_actions.state`；与 Task 7 DAO 共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Queued,
    Reserved,
    Accepted,
    Rejected,
    Unknown,
    Cancelled,
}

/// 控制路由。`HeadlessCompat` 仅普通消息可降级；审批/结构化回答绝不降级为普通消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRoute {
    DesktopIpc,
    HeadlessCompat,
    None,
}

/// 能力四档（spec F8）：用户看到的是实际能力，不是「理论上支持」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAvailability {
    Controllable,
    SendOnly,
    CompatOnly,
    Unavailable,
}

/// 平台能力集。`false` 表示该 build 尚未活体验证，不表示协议实现不存在；
/// 只有 Task 10 真机闭环 PASS 才允许翻转 profile 常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySet {
    pub send: bool,
    pub state: bool,
    pub command: bool,
    pub permissions: bool,
    pub blocking_input: bool,
}

/// 完成的动作回执（持久化于 `codex_actions.receipt_json`，HTTP 原样返回）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReceipt {
    #[serde(rename = "actionId")]
    pub action_id: String,
    pub delivery: Delivery,
    pub route: ControlRoute,
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
    #[serde(rename = "envelopeId")]
    pub envelope_id: Option<String>,
    #[serde(rename = "reasonCode")]
    pub reason_code: Option<String>,
}

/// 动作视图（发给浏览器；不携带正文/答案）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexActionView {
    #[serde(rename = "actionId")]
    pub action_id: String,
    pub state: ActionState,
    /// 当前设备视角的 FIFO 位置（1 起）；非 queued 为 None。
    pub position: Option<usize>,
    pub receipt: Option<ActionReceipt>,
}

/// 队列视图。`blocked_reason_code` 非 None（如 `compat_outstanding`）时排队不派发，
/// 前端必须展示说明而不是让用户盲等。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexQueueView {
    pub actions: Vec<CodexActionView>,
    #[serde(rename = "blockedReasonCode")]
    pub blocked_reason_code: Option<String>,
}

/// 官方 Desktop 持有租约：owner 身份随连接生命周期变化（spec F6），epoch 由
/// supervisor 每次重连递增，generation 按 Thread 单调增加；旧 lease 一律不得用于
/// 后续控制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerLease {
    pub key: ThreadKey,
    #[serde(rename = "clientId")]
    pub client_id: String,
    pub epoch: u64,
    pub generation: u64,
}

/// Thread 状态投影（spec 3.3：IPC 原始 state 不直接进入远程 API）。正文尾窗
/// 1 MiB / 500 条，超出标 `truncated`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadProjection {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub revision: Option<u64>,
    pub stale: bool,
    pub availability: ControlAvailability,
    pub capabilities: CapabilitySet,
    /// idle / active / failed / unknown（由 runtime status 与 turn history 推得）
    pub runtime_status: String,
    /// 内容健康状态（2026-10-18 形态适配）：与写能力正交——能发送不代表历史已
    /// 载入；读取出错（unsupported）不得呈现为「真实空会话」
    pub content_state: ContentState,
    pub messages: Vec<ProjectedMessage>,
    pub truncated: bool,
    pub pending: Vec<PendingView>,
}

/// 内容健康状态（`contentState`）：ready 且真实 0 条才允许显示「暂无消息」；
/// loading/partial 必须标记未全载入；unsupported 表示读取异常待核对。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentState {
    /// 首帧 snapshot 未到 / Turn 列表未载完（history.isComplete ≠ true）
    Loading,
    /// Turn 列表与各 turn items 均完整（此时真实 0 条 = 真空会话）
    Ready,
    /// Turn 列表完整但部分 turn 的 items 未载完（itemsPagination 标记）
    Partial,
    /// `turnHistory.history` 形态不可读——读取异常，不冒充空历史
    Unsupported,
}

/// 投影消息（正文条目）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedMessage {
    pub id: String,
    pub role: String,
    pub kind: String,
    pub text: String,
    pub ts: Option<i64>,
}

/// 待处理交互类别。未知 method → `Unsupported`（actionable=false，只显示
/// 「此请求暂不支持远程处理」，不暴露 raw method/参数对象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    CommandApproval,
    PermissionsApproval,
    UserInput,
    Unsupported,
}

/// 待处理请求视图。`request_key` 是无损可逆字符串（`n:<十进制>` / `s:<原串>`），
/// 浏览器只回显 key/token；`context_token` 是服务端随机 UUID，绑定
/// boot/epoch/generation/thread/request，不含 owner 文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    #[serde(rename = "requestKey")]
    pub request_key: String,
    #[serde(rename = "contextToken")]
    pub context_token: String,
    pub kind: PendingKind,
    pub actionable: bool,
    #[serde(rename = "disabledReasonCode")]
    pub disabled_reason_code: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    #[serde(rename = "permissionSummary")]
    pub permission_summary: Option<String>,
    pub questions: Vec<QuestionView>,
}

/// 结构化问题（Plan blocking input）。`is_secret` 的回答不入审计/日志/错误文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionView {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(rename = "isOther")]
    pub is_other: bool,
    #[serde(rename = "isSecret")]
    pub is_secret: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// 用户对待处理请求的动作。答案以原问题 ID 为键（不按显示顺序当 ID）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingAction {
    Approve,
    Reject,
    Answer(BTreeMap<String, Vec<String>>),
}

/// Desktop pending id 按不透明标识处理（spec 3.5）：兼容整数和字符串，拒 float。
/// 不作为 JSON number 发给浏览器——`request_key` 为无损可逆字符串。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PendingRequestId {
    String(String),
    Integer(serde_json::Number),
}

impl PendingRequestId {
    /// `n:<十进制整数>` / `s:<原字符串>`（无损失逆变换，见 `from_key`）。
    pub fn to_key(&self) -> String {
        match self {
            Self::String(s) => format!("s:{s}"),
            Self::Integer(n) => format!("n:{n}"),
        }
    }

    /// 逆变换；非规范形态（含 float、前缀缺失、非十进制）一律 `InvalidId`。
    pub fn from_key(key: &str) -> Result<Self, ProtocolError> {
        if let Some(rest) = key.strip_prefix("n:") {
            if !is_canonical_decimal(rest) {
                return Err(ProtocolError::InvalidId(format!(
                    "non-canonical integer requestKey: {key}"
                )));
            }
            let n: u64 = rest.parse().map_err(|_| {
                ProtocolError::InvalidId(format!("integer requestKey out of range: {key}"))
            })?;
            Ok(Self::Integer(serde_json::Number::from(n)))
        } else if let Some(rest) = key.strip_prefix("s:") {
            Ok(Self::String(rest.to_string()))
        } else {
            Err(ProtocolError::InvalidId(format!(
                "requestKey missing n:/s: prefix: {key}"
            )))
        }
    }
}

/// 只认 `0` 或不以 0 开头的纯 ASCII 十进制（拒绝 `+5`/`05`/`1.5`/空串）。
fn is_canonical_decimal(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    s == "0" || !s.starts_with('0')
}

impl<'de> Deserialize<'de> for PendingRequestId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(Self::String(s)),
            serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => Ok(Self::Integer(n)),
            serde_json::Value::Number(_) => Err(serde::de::Error::custom(
                "float pending request id rejected (opaque id must be integer or string)",
            )),
            other => Err(serde::de::Error::custom(format!(
                "pending request id must be a string or integer, got: {other}"
            ))),
        }
    }
}

/// 协议/控制面错误。`reason_code` 上报 HTTP 层时映射为机器码字符串。
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolError {
    /// 方法不在 profile 版本表（未知 build/method 不盲目启用）
    UnsupportedMethod(String),
    /// requestKey / request id 非法
    InvalidId(String),
    /// 帧长度/UTF-8/JSON 非法（接收侧须关闭该连接）
    InvalidFrame(String),
    /// 状态机非法跃迁（如非法 patch/revision 空洞的具体形态由 mirror 判定）
    InvalidState(String),
    /// 过期上下文：旧 epoch/generation/过期 context_token
    StaleContext(String),
    /// 超出有界资源（帧预算/队列满/镜像预算）
    Capacity(String),
    Timeout(String),
    Disconnected(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMethod(m) => write!(f, "unsupported method: {m}"),
            Self::InvalidId(m) => write!(f, "invalid id: {m}"),
            Self::InvalidFrame(m) => write!(f, "invalid frame: {m}"),
            Self::InvalidState(m) => write!(f, "invalid state: {m}"),
            Self::StaleContext(m) => write!(f, "stale context: {m}"),
            Self::Capacity(m) => write!(f, "capacity exceeded: {m}"),
            Self::Timeout(m) => write!(f, "timeout: {m}"),
            Self::Disconnected(m) => write!(f, "disconnected: {m}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// 机器码字符串（HTTP 层 reasonCode / 审计用）。
impl ProtocolError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedMethod(_) => "unsupported_method",
            Self::InvalidId(_) => "invalid_id",
            Self::InvalidFrame(_) => "invalid_frame",
            Self::InvalidState(_) => "invalid_state",
            Self::StaleContext(_) => "stale_context",
            Self::Capacity(_) => "capacity",
            Self::Timeout(_) => "timeout",
            Self::Disconnected(_) => "disconnected",
        }
    }
}

/// UUID v4（自实现：仓库无 uuid crate 依赖，`rand::fill` 为既有惯例）。
/// 三类 UUID（action / envelope / clientUserMessageId）都从这里出，连续生成互不相等，
/// 且满足 [`crate::inject::headless::codex::is_uuid`]。
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex = |slice: &[u8]| -> String {
        slice
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_id_key_roundtrips_losslessly() {
        // 浏览器侧大整数精度：9007199254740993 > 2^53，只有 key 字符串形态无损
        let big = PendingRequestId::from_key("n:9007199254740993").unwrap();
        assert_eq!(big.to_key(), "n:9007199254740993");
        assert_eq!(
            serde_json::to_value(&big).unwrap(),
            serde_json::json!(9007199254740993u64)
        );
        assert_ne!(
            PendingRequestId::from_key("n:12").unwrap(),
            PendingRequestId::from_key("s:12").unwrap()
        );
        // 整数与字符串互逆
        let s = PendingRequestId::String("abc/日本:12".into());
        assert_eq!(PendingRequestId::from_key(&s.to_key()).unwrap(), s);
        let n = PendingRequestId::Integer(serde_json::Number::from(12u64));
        assert_eq!(PendingRequestId::from_key(&n.to_key()).unwrap(), n);
    }

    #[test]
    fn pending_id_rejects_float_and_malformed_keys() {
        assert_eq!(
            PendingRequestId::from_key("n:1.5").unwrap_err(),
            ProtocolError::InvalidId("non-canonical integer requestKey: n:1.5".into())
        );
        assert!(matches!(
            PendingRequestId::from_key("12"),
            Err(ProtocolError::InvalidId(_))
        ));
        assert!(matches!(
            PendingRequestId::from_key("n:+5"),
            Err(ProtocolError::InvalidId(_))
        ));
        assert!(matches!(
            PendingRequestId::from_key("n:05"),
            Err(ProtocolError::InvalidId(_))
        ));
        assert!(matches!(
            PendingRequestId::from_key("x:12"),
            Err(ProtocolError::InvalidId(_))
        ));
    }

    #[test]
    fn pending_id_deserialize_rejects_float_wire_value() {
        let v: PendingRequestId = serde_json::from_value(serde_json::json!(12)).unwrap();
        assert_eq!(
            v,
            PendingRequestId::Integer(serde_json::Number::from(12u64))
        );
        let v: PendingRequestId = serde_json::from_value(serde_json::json!("req-9")).unwrap();
        assert_eq!(v, PendingRequestId::String("req-9".into()));
        // f64 形态（1e2 也算 float）拒绝——不透明标识不接受浮点
        assert!(serde_json::from_value::<PendingRequestId>(serde_json::json!(1.5)).is_err());
        assert!(serde_json::from_value::<PendingRequestId>(serde_json::json!(1e2)).is_err());
        assert!(serde_json::from_value::<PendingRequestId>(serde_json::json!(null)).is_err());
    }

    #[test]
    fn uuids_are_distinct_and_is_uuid_compatible() {
        let a = new_uuid();
        let b = new_uuid();
        let c = new_uuid();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        for u in [a, b, c] {
            assert!(
                crate::inject::headless::codex::is_uuid(&u),
                "{u} 不满足 is_uuid"
            );
        }
    }

    #[test]
    fn shared_types_serialize_camel_case_snake_enums() {
        let caps = CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
        };
        let v = serde_json::to_value(caps).unwrap();
        assert!(v.get("send").is_some());
        assert!(v.get("blockingInput").is_some());
        assert_eq!(
            serde_json::to_value(Delivery::UnknownDelivery).unwrap(),
            serde_json::json!("unknown_delivery")
        );
        assert_eq!(
            serde_json::to_value(ControlAvailability::CompatOnly).unwrap(),
            serde_json::json!("compat_only")
        );
        let view = CodexQueueView {
            actions: vec![CodexActionView {
                action_id: "a1".into(),
                state: ActionState::Queued,
                position: Some(1),
                receipt: None,
            }],
            blocked_reason_code: None,
        };
        let v = serde_json::to_value(&view).unwrap();
        assert!(v["actions"][0]["actionId"].is_string());
        assert!(v["blockedReasonCode"].is_null());
    }
}
