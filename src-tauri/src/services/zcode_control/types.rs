//! ZCode 统一远控域的公共类型（生产计划 T1.1 起逐任务增补）。
//!
//! 合同来源：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`
//! 「文件结构与公共合同」表。本文件是该域 Tuvis DTO／错误码的**唯一定义处**，后续
//! 任务（relay/projection/command/hub…）只在此追加，不另立重复类型。
//!
//! 纪律：对外错误码**冻结**（见 [`codes`]），错误体只含码、不含上游原文/链接/token；
//! 内部 cause 不得自动 Debug 到日志（[`ZcodeError`] 因此不携带 cause）。

use std::fmt;

/// 对外错误码（冻结：不得新增、改写或本地化；HTTP error 与 capability reasonCode 共用）。
pub mod codes {
    /// 未保存官方链接/配置不可用。
    pub const NEEDS_CONFIG: &str = "needs_config";
    /// 未取得或已失效的账号凭据，需要重新授权。
    pub const NEEDS_AUTH: &str = "needs_auth";
    /// 网络/relay 不可达。
    pub const OFFLINE: &str = "offline";
    /// 同 role 连接冲突（KICKED 等）。
    pub const CONFLICT: &str = "conflict";
    /// 其他未知失败（官方形态变更、无法归类）。
    pub const UNKNOWN: &str = "unknown";
    /// 投影/桥尚未就绪。
    pub const NOT_READY: &str = "not_ready";
    /// 官方协议版本不受支持。
    pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
    /// 能力缺少实机证据（生产门保持关闭）。
    pub const NEEDS_EVIDENCE: &str = "needs_evidence";
    /// 目标会话不存在。
    pub const NO_SESSION: &str = "no_session";
    /// 目标命中多个候选，拒绝猜测。
    pub const TARGET_AMBIGUOUS: &str = "target_ambiguous";
    /// generation 已过期（旧连接/旧投影）。
    pub const STALE_GENERATION: &str = "stale_generation";
    /// pending 已过期（宿主已撤卡/前进）。
    pub const STALE_PENDING: &str = "stale_pending";
    /// 动作载荷无效。
    pub const INVALID_ACTION: &str = "invalid_action";
    /// 答案载荷无效。
    pub const INVALID_ANSWER: &str = "invalid_answer";
    /// 选项被策略拒绝（如 fullAccess/未知 kind）。
    pub const FORBIDDEN_OPTION: &str = "forbidden_option";
    /// 同 actionId 载荷冲突。
    pub const ACTION_PAYLOAD_CONFLICT: &str = "action_payload_conflict";
    /// 持久化写入失败。
    pub const STORAGE_FAILED: &str = "storage_failed";
    /// 载荷超出上限。
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    /// stop 目标在执行途中变化。
    pub const STOP_TARGET_CHANGED: &str = "stop_target_changed";
}

/// 本域统一错误：`{code:String}`，只允许 [`codes`] 中的冻结码。
///
/// 不携带 cause／上游原文／URL／token：`reqwest` 等底层错误可能内含请求 URL 与
/// 认证头，构造时必须**丢弃**而不是包一层。`Debug` 只输出码，可安全入日志。
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct ZcodeError {
    pub code: String,
}

impl ZcodeError {
    /// 按冻结码构造（`code` 必须是 [`codes`] 常量；本域内部使用）。
    pub(crate) fn of(code: &'static str) -> Self {
        Self {
            code: code.to_owned(),
        }
    }

    /// 需要（重新）授权。
    pub fn needs_auth() -> Self {
        Self::of(codes::NEEDS_AUTH)
    }

    /// 网络/relay 不可达。
    pub fn offline() -> Self {
        Self::of(codes::OFFLINE)
    }

    /// 无法归类的失败。
    pub fn unknown() -> Self {
        Self::of(codes::UNKNOWN)
    }

    /// 是否等于给定冻结码。
    pub fn is(&self, code: &str) -> bool {
        self.code == code
    }
}

impl fmt::Debug for ZcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZcodeError")
            .field("code", &self.code)
            .finish()
    }
}

impl fmt::Display for ZcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code)
    }
}

impl std::error::Error for ZcodeError {}

/// 本域统一结果类型。
pub type ZResult<T> = Result<T, ZcodeError>;

/// relay 连接状态（wire 为 snake_case；与合同 [`ConnectionState`] 一一对应）。
///
/// - `connecting`：已开始握手、`pair_status=waiting` 等配对前态（**不是**已配对）；
/// - `paired`：唯一「已配对」状态（官方 `pair_status=matched`）；
/// - `offline`：网络掉线／路由窗超时／心跳超时／显式断开（终态，不自动重连）；
/// - `conflict`：同 role 被踢（KICKED，终态，不自动重连）；
/// - `needs_auth`：凭据缺失/失效（401/AUTH_FAILED）；
/// - `unknown`：官方形态不符等无法归类的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// 未保存官方链接（由上层 Hub 判定；relay 自身不读取配置）。
    NeedsConfig,
    /// 需要（重新）授权。
    NeedsAuth,
    /// 握手中/等待配对（`waiting` 是合法配对前态）。
    Connecting,
    /// 已配对（官方 `pair_status=matched`）。
    Paired,
    /// 离线终态。
    Offline,
    /// 同 role 冲突终态（KICKED）。
    Conflict,
    /// 无法归类的状态。
    Unknown,
}

/// 连接视图（wire camelCase）：`{state,generation,reasonCode,deviceLabel}`。
///
/// `deviceLabel` 仅供显示，**不能**用于绑定；`reasonCode` 只允许 [`codes`] 的冻结码。
/// `generation` 随显式连接/断开推进，消费方必须丢弃旧代际的迟到结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    pub state: ConnectionState,
    pub generation: u64,
    pub reason_code: Option<String>,
    pub device_label: Option<String>,
}

impl ConnectionView {
    /// 唯一「已配对」判定：`waiting`（connecting）不是人工批准，也不是 paired。
    pub fn is_paired(&self) -> bool {
        self.state == ConnectionState::Paired
    }
}
