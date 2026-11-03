// OpenClaw 会话解析器 — 基于 openclaw.json 配置 + 进程信息
// OpenClaw 使用 Node.js gateway 进程，会话信息从 ~/.openclaw/openclaw.json 的 agents 列表解析

use super::cwd::normalize_cwd_for_match;
use crate::adapter::AgentProcess;
use crate::session::{jump_supported_for, AgentType, Session, SessionStatus};
use log::{debug, info};
use serde::Deserialize;
use std::collections::HashMap;

/// OpenClaw 配置结构
#[derive(Deserialize, Debug)]
struct OpenClawConfig {
    agents: Option<AgentsConfig>,
}

#[derive(Deserialize, Debug)]
struct AgentsConfig {
    list: Option<Vec<AgentEntry>>,
}

#[derive(Deserialize, Debug)]
struct AgentEntry {
    id: String,
    name: Option<String>,
    workspace: Option<String>,
}

/// 获取 OpenClaw 会话。
/// 扫描预算豁免说明（monitor::session_scan）：配置声明 workspace + 零进程空判早退
/// （L1），IO 极小，不接 L2/L3
pub fn get_openclaw_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    if processes.is_empty() {
        return Vec::new();
    }

    let config_path = match dirs::home_dir() {
        Some(h) => h.join(".openclaw").join("openclaw.json"),
        None => {
            debug!("Cannot determine home directory for OpenClaw config");
            return Vec::new();
        }
    };

    let config = match read_openclaw_config(&config_path) {
        Some(c) => c,
        None => {
            debug!("OpenClaw config not found or unreadable: {:?}", config_path);
            return Vec::new();
        }
    };

    let agents = match config.agents.and_then(|a| a.list) {
        Some(list) => list,
        None => {
            debug!("OpenClaw agents list not found in config");
            return Vec::new();
        }
    };

    // cwd -> process 映射（归一化：统一分隔符、去尾部、Windows 下小写）
    let mut cwd_to_process: HashMap<String, &AgentProcess> = HashMap::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            cwd_to_process.insert(normalize_cwd_for_match(&cwd.to_string_lossy()), process);
        }
    }

    let mut sessions = Vec::new();
    let mut matched_pids = std::collections::HashSet::new();

    // 按 workspace 匹配进程
    for agent in &agents {
        let workspace = agent.workspace.as_deref().unwrap_or("");
        if workspace.is_empty() {
            continue;
        }

        let ws = normalize_cwd_for_match(workspace);
        let matching_process = cwd_to_process
            .iter()
            .find(|(cwd, _)| **cwd == ws || cwd.starts_with(&format!("{}/", ws)))
            .map(|(_, p)| *p);

        if let Some(process) = matching_process {
            debug!("OpenClaw agent {} matched to pid={}", agent.id, process.pid);
            matched_pids.insert(process.pid);
            if let Some(session) = build_session(agent, workspace, process) {
                sessions.push(session);
            }
        }
    }

    // 未匹配的进程：回退到第一个默认 agent
    for process in processes {
        if matched_pids.contains(&process.pid) {
            continue;
        }
        if let Some(cwd) = &process.cwd {
            let cwd_str = cwd.to_string_lossy().to_string();
            // 使用进程 cwd 作为 project_path
            if let Some(agent) = agents.iter().find(|a| a.id == "main" || a.id == "default") {
                if let Some(session) = build_session(agent, &cwd_str, process) {
                    sessions.push(session);
                }
            }
        }
    }

    info!(
        "OpenClaw: {} sessions from {} processes",
        sessions.len(),
        processes.len()
    );
    sessions
}

fn read_openclaw_config(path: &std::path::Path) -> Option<OpenClawConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn build_session(
    agent: &AgentEntry,
    project_path: &str,
    process: &AgentProcess,
) -> Option<Session> {
    let project_name = std::path::Path::new(project_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let display_name = agent
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&agent.id);

    let now = chrono::Utc::now();
    let last_activity_at = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    // OpenClaw 状态判断：基于仍逐扫描刷新的 CPU 使用率
    let status = status_from_cpu_usage(process.cpu_usage);

    Some(Session {
        id: agent.id.clone(),
        agent_type: AgentType::OpenClaw,
        project_name,
        project_path: project_path.to_string(),
        git_branch: None,
        github_url: None,
        status,
        last_message: Some(format!("OpenClaw agent: {}", display_name)),
        last_message_role: Some("system".to_string()),
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at,
        pid: process.pid,
        cpu_usage: process.cpu_usage,
        active_subagent_count: 0,
        form: process.form,
        jump_supported: jump_supported_for(process.form),
        unread: false, // 扫描出的活跃卡默认非未读；未读卡由 adapter 层合并
        unread_turned_green_at_ms: None,
        title: Some(display_name.to_string()),
    })
}

fn status_from_cpu_usage(cpu_usage: f32) -> SessionStatus {
    if cpu_usage > 5.0 {
        SessionStatus::Processing
    } else {
        SessionStatus::Idle
    }
}

#[cfg(test)]
mod cpu_status_tests {
    use super::*;

    #[test]
    fn processing_threshold_is_strictly_above_five_percent() {
        assert_eq!(status_from_cpu_usage(0.0), SessionStatus::Idle);
        assert_eq!(status_from_cpu_usage(5.0), SessionStatus::Idle);
        assert_eq!(status_from_cpu_usage(5.1), SessionStatus::Processing);
    }
}
