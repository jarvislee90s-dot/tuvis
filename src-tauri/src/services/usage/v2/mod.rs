//! 用量看板 V2 服务模块。
//!
//! 本模块落地身份/Token wire 类型、计费规则解析，以及供应商别名/显示渠道解析；
//! 查询、定价与费用模块按依赖顺序追加。

pub mod billing;
pub mod channel;
pub mod cost;
pub mod cost_query;
pub mod currency;
pub mod identity;
pub mod model;
pub mod model_config;
pub mod native_price;
pub mod pricing;
pub mod query;

pub use identity::{merge_bucket, resolve_at, resolve_bucket};
pub use model::*;
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
