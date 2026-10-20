// Tauri IPC 命令 - 按功能域拆分到子模块

// 开机自动启动（2026-10-11）：官方 autostart 插件命令封装（真值源 = OS 侧插件状态）
pub mod autostart;
pub mod data_management;
// 导出落盘（计划① Task 21，契约 §3 新增 2 条命令；按字母序排在 data_management 之后）
pub mod export;
pub mod manifest;
pub mod mcp;
pub mod notification;
pub mod pet;
pub mod plugin;
pub mod preset;
pub mod resource;
pub mod screenshot;
pub mod session;
pub mod settings;
pub mod skill;
// 用量域（计划① Task 20）：6 条查询/设置命令（按字母序排在 skill 之后）
pub mod usage;
// 用量看板 V2（Task 5）：身份 / Token / 计费配置命令
pub mod usage_v2;
// 升级检查（prerelease 渠道）：GitHub 发现层 + 动态端点安装 + 临时残留清理
pub mod updater;

pub use screenshot::capture_window_screenshot;
pub use session::get_all_sessions;
