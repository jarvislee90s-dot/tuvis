//! ZCode 统一远控的 Tuvis 侧基础域（设计 §4.3/§5.2；生产计划 T1.1 起）。
//!
//! 本模块持有：
//! - `config`：官方远控链接的**格式合同**与 settings 持久化——保存只证明「格式已落库」，
//!   绝不表示已连接官方服务或已取得官方身份；
//! - `types`：本域公共 DTO／冻结错误码的唯一定义处；
//! - `auth`：官方 OAuth 授权与**内存凭据**（账号 JWT 只在本进程内存）。
//!
//! 官方授权、适配器与写能力由生产计划逐任务装配；授权不塞进 config，也不进 IPC 大文件。

pub mod auth;
pub mod config;
pub mod types;

pub use auth::{
    build_authorize_url, AuthService, OAuthProvider, OpenAuthorize, TerminalToken,
    AUTHORIZE_TIMEOUT, OAUTH_TOKEN_PATH, OFFICIAL_BIGMODEL_APP_ID, OFFICIAL_BIGMODEL_ORIGIN,
    OFFICIAL_OAUTH_CLIENT_ID, OFFICIAL_ZAI_ORIGIN, OFFICIAL_ZCODE_ORIGIN, RELAY_TOKEN_PATH,
};
pub use config::{
    get_config, set_config, validate_official_url, ConfigError, ConfigState, ZcodeRemoteConfig,
    CONFIG_KEY,
};
pub use types::{codes, ZResult, ZcodeError};
