//! ZCode 统一远控的 Tuvis 侧基础域（设计 §4.3/§5.2；生产计划 T1.1 起）。
//!
//! 本模块持有：
//! - `config`：官方远控链接的**格式合同**与 settings 持久化——保存只证明「格式已落库」，
//!   绝不表示已连接官方服务或已取得官方身份；
//! - `types`：本域公共 DTO／冻结错误码的唯一定义处；
//! - `auth`：官方 OAuth 授权与**内存凭据**（账号 JWT 只在本进程内存）；
//! - `relay`：官方 terminal relay 的**单连接**生命周期（单 owner、无自动重连）；
//! - `codec`：官方二进制 RPC 编解码（类型标签/VQL、rpc-frame Base64+CRC32）与可靠收帧；
//! - `bridge`：工作区桥、任务索引与会话精确绑定（只读）；
//! - `projection`：会话 topic 的精确投影（snapshot/deltas，跨会话零泄漏）；
//! - `pending`：本会话 pending 册与 control 观察（ACK ≠ control 已到）；
//! - `action`：写动作的持久去重账本（`zcode_actions`，actionId 幂等 + 保守 unknown）；
//! - `audit`：动作起始审计（`zcode_official` 通道，只记类别与计数，不摘正文）；
//! - `command`：写命令派发（唯一信封/正文出口，一次 sendText、ACK 20s、绝不重发）。
//!
//! 官方授权、适配器与写能力由生产计划逐任务装配；授权不塞进 config，也不进 IPC 大文件。

pub mod action;
pub mod audit;
pub mod auth;
pub mod bridge;
pub mod codec;
pub mod command;
pub mod config;
pub mod pending;
pub mod projection;
pub mod relay;
pub mod types;

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
    MAX_TEXT_BYTES,
};
pub use config::{
    get_config, set_config, validate_official_url, ConfigError, ConfigState, ZcodeRemoteConfig,
    CONFIG_KEY,
};
pub use relay::{
    DeviceRoute, RelayClient, RelayEvent, OFFICIAL_RELAY_WS_ORIGIN, OFFICIAL_RELAY_WS_PATH,
};
pub use types::{
    codes, ActionContext, ActionReceipt, ActionState, ActiveWork, AnswerEntry, BoundTarget,
    CapabilityFlag, CapabilityView, CommandPort, ConnectionState, ConnectionView, ControlAction,
    ControlView, ModelChoice, PendingKind, PendingView, PermissionOptionView, QuestionOption,
    QuestionView, RelaySink, SelectionView, SessionView, TargetKey, ZResult, ZcodeError,
};
