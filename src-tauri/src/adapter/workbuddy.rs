// WorkBuddy（腾讯全场景 AI 办公工作台，Electron APP）适配器
// 会话运行时 = APP 内嵌 cli/bin/codebuddy；状态提取见 monitor/workbuddy_parser.rs

use super::*;
use crate::monitor;

/// WorkBuddy 适配器。实例随 `enabled_adapters()` 每轮扫描新建——`heartbeat_
/// observations` 槽因此天然归属**本次扫描调用**（NEEDS_FIX Important-B 终验：
/// 观测经调用级 collector 暂存在此，遍历段解析后统一取走，无进程级交接槽）。
pub struct WorkBuddyAdapter {
    heartbeat_observations: std::sync::Mutex<Vec<(u32, String)>>,
}

impl Default for WorkBuddyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkBuddyAdapter {
    pub fn new() -> Self {
        Self {
            heartbeat_observations: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl AgentAdapter for WorkBuddyAdapter {
    fn name(&self) -> &'static str {
        "WorkBuddy"
    }
    fn agent_type(&self) -> AgentType {
        AgentType::WorkBuddy
    }
    fn process_names(&self) -> &'static [&'static str] {
        // P0-1：进程发现改心跳目录驱动，不使用进程名匹配——Windows 上会话宿主与主进程
        // 同名 WorkBuddy.exe（Electron 以自身作 Node 运行 cli/bin/codebuddy 脚本，无
        // codebuddy 进程），进程名匹配恒空且「父进程同名」会被通用子代理过滤误杀。
        // 返回空切片：detect_all_tools 等入口已防御空切片（见 linker/detector.rs）
        &[]
    }

    fn find_processes(&self, system: &System) -> Vec<AgentProcess> {
        // 心跳目录驱动：枚举 ~/.workbuddy/sessions/<PID>.json 按过滤规则发现会话进程
        monitor::workbuddy_parser::discover_workbuddy_processes(system)
    }

    fn find_sessions(&self, processes: &[AgentProcess]) -> Vec<Session> {
        // NEEDS_FIX Important-B 终验：观测进调用级 collector（随 WalkedScan 返回、
        // 提交段落表），解析段不直写 LAST_SEEN 业务观测表。
        let mut observations = Vec::new();
        let sessions = monitor::workbuddy_parser::get_workbuddy_sessions_collecting(
            processes,
            &mut observations,
        );
        if !observations.is_empty() {
            self.heartbeat_observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(observations);
        }
        sessions
    }

    fn take_heartbeat_observations(&self) -> Vec<(u32, String)> {
        std::mem::take(
            &mut self
                .heartbeat_observations
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    fn base_dir(&self) -> std::path::PathBuf {
        dirs::home_dir().unwrap_or_default().join(".workbuddy")
    }

    fn mcp_format(&self) -> McpFormat {
        // MCP 服务器声明在 ~/.workbuddy/mcp.json（JSON，mcpServers 段）
        McpFormat::Json
    }
    fn mcp_config_path(&self) -> Option<std::path::PathBuf> {
        Some(self.base_dir().join("mcp.json"))
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        super::primary_skill_dir("workbuddy")
            .map(|dir| vec![dir])
            .unwrap_or_else(|| vec![self.base_dir().join("skills")])
    }

    // WorkBuddy 插件为市场化版本化管理，不纳入 兔维斯（spec W3）；无 hook 机制、
    // 无独立子 agent 目录 —— 均沿用 trait 默认实现（空 / false / None）
}
