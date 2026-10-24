//! ZCode 统一远控的 Tuvis 侧基础域（A4，设计 §4.3/§5.2）。
//!
//! 本模块只持有**官方远控链接的格式合同**与 settings 持久化：一种链接 = 一条配置，
//! 保存只证明「格式已落库」，绝不表示已连接官方服务或已取得官方身份。官方授权、
//! 适配器与写能力属于 B 包，先不在此装配。

pub mod config;

pub use config::{
    get_config, set_config, validate_official_url, ConfigError, ConfigState, ZcodeRemoteConfig,
    CONFIG_KEY,
};
