// TPS 短时抽样域（Spec: docs/superpowers/specs/2026-10-24-tps-sampling-design.md）。
// 与旧 usage 域的关系：完全复用既有 Token/费用账本与采集授权，本域只做「近期 TPS」
// 的独立状态与查询（Spec §6：不改变旧 usage Token 账本、水位、采集间隔或费用计算）。
//
// 模块分工（计划 Task 1/2）：
//   * model.rs  —— 纯值对象：Spec §6 固定类型的 Rust 侧（snake_case；IPC camelCase 镜像
//                  由 Task 7/8 的命令层完成），含枚举与 TpsObservation 的内在有效性谓词；
//   * policy.rs —— 采样口径纯函数：build_point / snapshot 及其单元测试（无 IO、无时钟、
//                  无全局态，时间一律由入参注入）；
//   * store.rs  —— 存储域层：commit_batch / save_points / load_recent / purge /
//                  tps_state 读写（SQL 在 database/dao/tps.rs；事务边界与批次上限在此）。
//   * install.rs —— 一次统计设置（计划 Task 5）：统计键安装所有权（preview/apply/
//                  uninstall，记录落 tps_state 的 install: 前缀）+ 本机接收器守卫
//                  （loopback/端口/1MiB/Bearer 纯函数；socket accept loop 归 Task 7）。
//   * scheduler.rs —— 调度器（计划 Task 7）：纯时间/租约状态机（tick/heartbeat/
//                  manual_refresh；无 IO 无时钟，tokio 循环在 collector 接线）。
//   * collector.rs —— 采集编排（计划 Task 7）：跑源 + commit_batch + 建点 + 慢节奏
//                  清理 + 轮级单飞 + 运行接线（spawn_runtime；总授权复用 usage.enabled）。
//   * receiver.rs —— Claude 本机 OTLP 接收器（计划 Task 7 / 裁决 R10）：127.0.0.1 回环
//                  监听、真实 bind 选端口、1MiB/Bearer 守卫、诊断随批提交。
//   * query.rs   —— 查询面（计划 Task 7）：快照/曲线只读投影 + 来源状态真实化与分类。
// 错误类型：本域复用 `crate::services::usage::UsageError`（码表登记纪律见 usage/error.rs），
// 不另造错误类型。

pub mod collector;
pub mod install;
pub mod model;
pub mod policy;
pub mod query;
pub mod receiver;
pub mod scheduler;
pub mod sources;
pub mod store;
pub mod v2;

pub use model::*;
pub use policy::{build_point, build_point_with_trigger, snapshot};
