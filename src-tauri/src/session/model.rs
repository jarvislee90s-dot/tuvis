use serde::{Deserialize, Serialize};

/// AI 编程工具类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Claude,
    Codex,
    OpenCode,
    OpenClaw,
    Kimi,
    WorkBuddy,
    ZCode,
    Dsh,
}

impl AgentType {
    /// 稳定 tool_id 字符串（与 serde lowercase 形态一致）。此前多处以
    /// `format!("{:?}", variant).to_lowercase()` 推导（PR #46 review M4：adapter/mod.rs
    /// 5 处 + hooks.rs 1 处），依赖「变体名小写恰好等于 tool_id」的隐式约定——
    /// 给枚举加自定义 Debug 或重命名变体会全部同时静默错位，且错的是进程匹配/
    /// 宿主判定这类核心链路。match 穷尽使新增变体在编译期强制补映射
    pub fn tool_id(&self) -> &'static str {
        match self {
            AgentType::Claude => "claude",
            AgentType::Codex => "codex",
            AgentType::OpenCode => "opencode",
            AgentType::OpenClaw => "openclaw",
            AgentType::Kimi => "kimi",
            AgentType::WorkBuddy => "workbuddy",
            AgentType::ZCode => "zcode",
            AgentType::Dsh => "dsh",
        }
    }
}

/// 会话状态（红绿灯五态 + Finished）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Waiting,
    Processing,
    Thinking,
    Compacting,
    Idle,
    Finished,
}

/// 进程形态：CLI 或桌面 APP
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProcessForm {
    Cli,
    App,
}

/// 跳转终端是否可用：Windows 下 CLI 与 App 均可窗口级聚焦（见 window/win32.rs）；
/// macOS 下 CLI 走 TTY 链路、App 走 activate application（W2）；其他平台不支持
pub fn jump_supported_for(form: ProcessForm) -> bool {
    let _ = form; // 矩阵只取决于平台：CLI 与 App 形态同可用性（App 激活链路 W2）
    if cfg!(windows) {
        return true;
    }
    cfg!(target_os = "macos")
}

/// 一次 AI 编程工具的运行实例
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub agent_type: AgentType,
    pub project_name: String,
    pub project_path: String,
    pub title: Option<String>,
    pub git_branch: Option<String>,
    pub github_url: Option<String>,
    pub status: SessionStatus,
    pub last_message: Option<String>,
    pub last_message_role: Option<String>,
    /// lastMessage 是否为「子 agent 回报」触发（观察台 §四 提醒开关的判定信号；
    /// claude 判据层同源打标——teammate-message / task-notification，其余工具
    /// 恒 false = 「未区分」如实申报，spec §四.4）
    pub last_message_subagent_report: bool,
    /// 「本次状态由子 agent 活动引发」打标（终审发现 C / 决策 4）：活跃子 agent 在场
    /// ∧ 最新 hook 事件 ∈ PostToolUse 族 ∧ 事件年龄 < 30s TTL。后端判定、前端只消费
    /// 布尔、永不匹配文案；红/等待语义不受影响（前端对红边不节流不静默）
    pub flap_from_subagent_activity: bool,
    pub last_activity_at: String,
    pub pid: u32,
    pub cpu_usage: f32,
    pub active_subagent_count: usize,
    /// 进程形态（CLI / 桌面 APP）
    pub form: ProcessForm,
    /// 是否支持跳转（CLI=TTY 链路，App=APP 激活）
    pub jump_supported: bool,
    /// 未读标记（W4）：true = 绿色已完成且用户未查看的持久未读卡（APP 类专用）
    pub unread: bool,
    /// 未读池里的首次转绿时间（epoch ms）。与 last_activity_at 的业务活动时间分离：
    /// 前端首见未读卡的新鲜度必须用它判断，不能用旧业务活动时间误杀补偿通知。
    #[serde(default)]
    pub unread_turned_green_at_ms: Option<i64>,
}

/// 全部会话的聚合响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsResponse {
    pub sessions: Vec<Session>,
    pub total_count: usize,
    pub waiting_count: usize,
}

/// JSONL 消息解析结构（Claude / Codex 共用）
#[derive(Debug, Deserialize)]
pub(crate) struct JsonlMessage {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub subtype: Option<String>,
    #[serde(rename = "isCompactSummary")]
    pub is_compact_summary: Option<bool>,
    pub message: Option<MessageContent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageContent {
    pub role: Option<String>,
    pub content: Option<serde_json::Value>,
}

#[cfg(test)]
mod jump_tests {
    use super::*;

    #[test]
    fn jump_supported_matches_platform_matrix() {
        // Windows：CLI 与 App 均可窗口级聚焦；macOS：CLI 走 TTY、App 走 APP 激活（W2）；
        // 其他平台：不支持。两平台结果一致（均支持），故合并断言
        let expected = cfg!(windows) || cfg!(target_os = "macos");
        assert_eq!(jump_supported_for(ProcessForm::Cli), expected);
        assert_eq!(jump_supported_for(ProcessForm::App), expected);
    }

    /// T4 F2b wire 契约：打标字段按 rename_all = camelCase 序列化为
    /// `flapFromSubagentActivity`——前端（桌面 useNotification / 移动 Board）按此键消费，
    /// snake_case 会静默 undefined（先例：watcher TransitionEvent 评审 Important 1）
    #[test]
    fn flap_tag_serializes_camel_case() {
        let s = Session {
            id: "s1".into(),
            agent_type: AgentType::Claude,
            project_name: "p".into(),
            project_path: "/p".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Processing,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: true,
            last_activity_at: String::new(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 1,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: Some(42),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v["flapFromSubagentActivity"], true,
            "wire 键必须为 camelCase 的 flapFromSubagentActivity"
        );
        assert_eq!(
            v["unreadTurnedGreenAtMs"], 42,
            "未读转绿时间必须按 camelCase 下发，前端通知用它判断新鲜度"
        );
        assert!(
            v.get("flap_from_subagent_activity").is_none(),
            "不得出现 snake_case 键（前端契约 camelCase）"
        );
    }
}
