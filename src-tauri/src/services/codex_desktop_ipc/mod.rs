//! Codex Desktop IPC 客户端（spec：`docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`）。
//!
//! 与 `inject/headless` 平行的**独立控制面**：直连官方 Codex Desktop 当前持有
//! Thread 的本机 IPC（Windows Named Pipe / macOS Unix socket），控制的是已运行的
//! 官方 Desktop，而不是自行启动的无头执行进程——因此不属于 Headless，也不复用其
//! transport/envelope。协议层（本模块）与平台 transport 分层；`services/codex_control`
//! 负责路由与持久化调度。
//!
//! 安全边界（spec S1–S3）：IPC 只允许本机访问，绝不代理到网络；owner/target 身份
//! 由服务端发现，原始 IPC 参数不落远程 API；远程边界在 Tuvis 设备门禁，IPC 自身
//! 不提供认证。

pub mod codec;
pub mod connection;
pub mod owners;
pub mod profile;
pub mod transport;
pub mod types;
