//! ZCode 统一远控的 Tuvis 侧基础域（设计 §4.3/§5.2；生产计划 T1.1 起）。
//!
//! 本模块持有：
//! - `config`：官方远控链接的**格式合同**与 settings 持久化——保存只证明「格式已落库」，
//!   绝不表示已连接官方服务或已取得官方身份；
//! - `types`：本域公共 DTO／冻结错误码的唯一定义处；
//! - `auth`：官方 OAuth 授权与**内存凭据**（账号 JWT 只在本进程内存）；
//! - `credential_store`：系统凭据存储适配口（含可测 fake）：单条目、UTF-8 JSON 二进制
//!   secret、恢复禁用标记 checked 写入；原生保险箱实现在桌面三平台；
//! - `relay`：官方 terminal relay 的**单连接**生命周期（单 owner、无自动重连）；
//! - `codec`：官方二进制 RPC 编解码（类型标签/VQL、rpc-frame Base64+CRC32）与可靠收帧；
//! - `bridge`：工作区桥、任务索引与会话精确绑定（只读）；
//! - `projection`：会话 topic 的精确投影（snapshot/deltas，跨会话零泄漏）；
//! - `pending`：本会话 pending 册与 control 观察（ACK ≠ control 已到）；
//! - `action`：写动作的持久去重账本（`zcode_actions`，actionId 幂等 + 保守 unknown）；
//! - `audit`：动作起始审计（`zcode_official` 通道，只记类别与计数，不摘正文）；
//! - `command`：写命令派发（唯一信封/正文出口，一次 sendText、ACK 20s、绝不重发）；
//! - `selection`：同账号模型/思考档位的纯构造与目录解析（CAS `baseRevision`、
//!   绝不恢复原值）；
//! - `hub`：进程内**单实例**控制 Hub（桌面与配对手机共享一条物理连接、代际权威、
//!   能力闸与事件广播）；
//! - `evidence`（**仅 debug 证据构建**）：T8.2 未开闸能力的单次验收许可——内存、
//!   120 秒、一次用户动作，只放宽能力闸，不放宽目标/权限/`fullAccess` 守卫。
//!
//! 官方授权、适配器与写能力由生产计划逐任务装配；授权不塞进 config，也不进 IPC 大文件。

pub mod action;
pub mod audit;
pub mod auth;
pub mod bridge;
pub mod codec;
pub mod command;
pub mod config;
// C1 只落适配口与 fake，C2 才接入 AuthService。分期提交期间显式登记 dead_code
// （同仓先例：staged 能力用 allow 注明消费批次）；C2 消费后可移除本属性。
#[allow(dead_code)]
pub(crate) mod credential_store;
mod host;
pub mod hub;
pub mod pending;
pub mod projection;
pub mod relay;
pub mod selection;
pub mod types;

// T8.1 生产链假服务（HTTP 切片）：真实回环 axum 端点的端到端用例，仅测试编译。
#[cfg(test)]
mod wire_e2e;

// T8.2 未开闸能力的「单次验收许可」（默认关）：只允许 **debug 证据构建** 启用。
// 编译门必须放在始终编译的 mod.rs（不能放进仅 debug 注册的 evidence 模块里）——
// release + 该 feature 显式拒绝，杜绝验收许可带进发行包（同 `codex-ipc-e2e-stop`）。
#[cfg(all(feature = "zcode-bridge-evidence", not(debug_assertions)))]
compile_error!("zcode-bridge-evidence is only allowed in debug evidence builds");

#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
pub mod evidence;

#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
pub use evidence::{EvidencePermit, EvidencePermitStore, EVIDENCE_PERMIT_TTL_MS};

pub use action::{
    action_params, audit_summary, payload_hash, ActionStore, Reservation, T3_ACTION_KIND,
};
pub use audit::{AGENT as AUDIT_AGENT, CHANNEL as AUDIT_CHANNEL};
pub use auth::{
    AuthService, OAuthProvider, OpenAuthorize, TerminalToken, AUTHORIZE_TIMEOUT, OAUTH_TOKEN_PATH,
    OFFICIAL_BIGMODEL_APP_ID, OFFICIAL_BIGMODEL_ORIGIN, OFFICIAL_OAUTH_CLIENT_ID,
    OFFICIAL_ZAI_ORIGIN, OFFICIAL_ZCODE_ORIGIN, RELAY_TOKEN_PATH,
};
pub use codec::{
    decode_channel, encode_ack, encode_call, encode_frame, encode_listen, ChannelMessage,
    FrameAssembler, FrameLimits, RpcIdentity, ASSEMBLY_TTL_MS, MAX_CHANNEL_MESSAGE_BYTES,
    MAX_CONCURRENT_ASSEMBLIES, MAX_FRAGMENTS_PER_MESSAGE, MAX_PHYSICAL_FRAME_BYTES,
    MAX_STAGED_FRAMES, MAX_TOTAL_CACHED_BYTES, REQUEST_TYPE_EVENT_LISTEN, REQUEST_TYPE_PROMISE,
    RESPONSE_TYPE_EVENT_FIRE, RESPONSE_TYPE_INITIALIZE, RESPONSE_TYPE_PROMISE_ERROR,
    RESPONSE_TYPE_PROMISE_SUCCESS,
};
pub use command::{
    compose_official_text, new_command_id, CommandSender, ACK_TIMEOUT, AGENT_SEND_COMMAND_METHOD,
    CONFIRM_TIMEOUT, MAX_TEXT_BYTES, RESOLVE_INTERACTION_TYPE, SELECTION_CONFIRM_TIMEOUT,
    STOP_TYPE,
};
pub use config::{
    get_config, set_config, validate_official_url, ConfigError, ConfigState, ZcodeRemoteConfig,
    CONFIG_KEY,
};
pub use hub::{HubBackend, PartsBackend, ZcodeHub};
pub use relay::{
    DeviceRoute, RelayClient, RelayEvent, OFFICIAL_RELAY_WS_ORIGIN, OFFICIAL_RELAY_WS_PATH,
};
pub use selection::{build_selection_command, parse_selection_view, SWITCH_MODEL_CONFIG_TYPE};
pub use types::{
    codes, ActionContext, ActionReceipt, ActionState, ActiveWork, AnswerEntry, BoundTarget,
    CapabilityFlag, CapabilitySource, CapabilityView, CommandPort, ConnectionState, ConnectionView,
    ControlAction, ControlEvent, ControlEventKind, ControlView, HostBuildProbe, HostEvidence,
    ModelChoice, PendingKind, PendingPort, PendingView, PermissionOptionView, QuestionOption,
    QuestionView, RelaySink, SelectionPort, SelectionView, SessionView, StopPort, StopRequest,
    TargetKey, ZResult, ZcodeError, PENDING_CLEARED_POLL, SELECTION_POLL, STOP_CLEARED_POLL,
};
