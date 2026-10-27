//! V2 双指标查询域（2026-10-26 桌面速度 V2 计划 Task 6）：区间/桶聚合与只读 dashboard。
//!
//! 模块分工（与 Task 1–5 的事实层解耦；有效性口径只在 dao/speed.rs 的共享 CTE，
//! 与 `model.rs` 的 Task 1 谓词由矩阵契约测试锁定一致）：
//!   model.rs —— 契约 DTO 的 Rust 镜像（serde 串逐字；`?` 字段显式 null）。
//!   sums.rs  —— 充分统计 `SpeedSums` 与纯派生 `derive_metrics`（零合计不冒充 0 速度；
//!               无 IO 无时钟）。
//!   query.rs —— `dashboard(conn, query, now_ms)` 只读入口、桶几何（`bucket_spans` /
//!               offset 钉死的 `bucket_spans_with_offset`）、排序/选择、来源状态视图。
//!
//! **只读纪律**：本域绝不调来源读取器、旧采集或工具根文件系统；总授权复用
//! `usage.enabled`（不设第二开关）；旧 `tps_point` 滚动点与 V2 聚合完全无关。
pub mod model;
pub mod query;
pub mod sums;

pub use model::*;
pub use sums::{derive_metrics, SpeedSums};
