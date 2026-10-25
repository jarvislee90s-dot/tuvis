//! ZCode 统一远控的 Tuvis 侧基础域（设计 §4.3/§5.2；生产计划 T1.1 起）。
//!
//! 本模块持有：
//! - `config`：官方远控链接的**格式合同**与 settings 持久化——保存只证明「格式已落库」，
//!   绝不表示已连接官方服务或已取得官方身份；
//! - `types`：本域公共 DTO／冻结错误码的唯一定义处；
//! - `auth`：官方 OAuth 授权与**内存凭据**（账号 JWT 只在本进程内存）；
//! - `relay`：官方 terminal relay 的**单连接**生命周期（单 owner、无自动重连）；
//! - `codec`：官方二进制 RPC 编解码（类型标签/VQL、rpc-frame Base64+CRC32）与可靠收帧。
//!
//! 官方授权、适配器与写能力由生产计划逐任务装配；授权不塞进 config，也不进 IPC 大文件。

pub mod auth;
pub mod codec;
pub mod config;
pub mod relay;
pub mod types;

pub use auth::{
    AuthService, OAuthProvider, OpenAuthorize, TerminalToken, AUTHORIZE_TIMEOUT, OAUTH_TOKEN_PATH,
    OFFICIAL_BIGMODEL_APP_ID, OFFICIAL_BIGMODEL_ORIGIN, OFFICIAL_OAUTH_CLIENT_ID,
    OFFICIAL_ZAI_ORIGIN, OFFICIAL_ZCODE_ORIGIN, RELAY_TOKEN_PATH,
};
pub use codec::{
    decode_channel, encode_ack, encode_call, encode_frame, encode_listen, ChannelMessage,
    FrameAssembler, FrameLimits, RpcIdentity, ASSEMBLY_TTL_MS, MAX_CONCURRENT_ASSEMBLIES,
    MAX_FRAGMENTS_PER_MESSAGE, MAX_PHYSICAL_FRAME_BYTES, MAX_STAGED_FRAMES, MAX_TOTAL_CACHED_BYTES,
    REQUEST_TYPE_EVENT_LISTEN, REQUEST_TYPE_PROMISE, RESPONSE_TYPE_EVENT_FIRE,
    RESPONSE_TYPE_INITIALIZE, RESPONSE_TYPE_PROMISE_ERROR, RESPONSE_TYPE_PROMISE_SUCCESS,
};
pub use config::{
    get_config, set_config, validate_official_url, ConfigError, ConfigState, ZcodeRemoteConfig,
    CONFIG_KEY,
};
pub use relay::{
    DeviceRoute, RelayClient, RelayEvent, OFFICIAL_RELAY_WS_ORIGIN, OFFICIAL_RELAY_WS_PATH,
};
pub use types::{codes, ConnectionState, ConnectionView, ZResult, ZcodeError};
