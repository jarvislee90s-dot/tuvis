// 监控解析层：每个工具一个解析器模块（新增工具只需新增 *_parser.rs + 本处一行），
// 跨工具公共设施下沉到 cwd/path_codec/git/project/jsonl 五个职责单一的模块
pub mod app_status;
pub mod claude_config;
pub mod claude_parser;
pub mod codex_parser;
pub mod codex_thread_parser;
pub mod cwd;
pub mod dsh;
pub mod git;
pub mod hook_listener;
pub mod hooks;
pub mod host;
pub mod jsonl;
pub mod kimi_parser;
pub mod openclaw_parser;
pub mod opencode_parser;
pub mod path_codec;
pub mod process;
pub mod project;
pub mod session_scan;
pub mod sqlite;
pub mod status;
pub mod subagents;
pub mod workbuddy_parser;
pub mod zcode_parser;

use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::sync::Mutex;

/// 用户 X 掉的卡（T2「暂离不提示」）：key = (tool_id, session_id, status 小写)，
/// App 与 CLI 形态通用。同一会话状态变化后 key 不匹配 → 卡片自然重现
/// （绿→黄/红或产生新未读）；进程内语义，兔维斯 重启清空（重启后全部重现，可接受）。
/// 不碰 unread_sessions 表——未读卡的 X 走已读（mark_session_read），
/// 本集合只服务活跃卡的「隐藏」语义
pub static SESSION_DISMISALS: Lazy<Mutex<HashSet<(String, String, String)>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

/// dismiss 过滤纯函数（可测）：任意形态卡（App/CLI）按 (tool, session, status 小写)
/// 命中则剔除；状态变化后 key 不匹配自然重现。dismissed 以闭包注入，测试不触全局集合
pub fn filter_dismissed_cards(
    sessions: &mut Vec<crate::session::Session>,
    dismissed: &dyn Fn(&str, &str, &str) -> bool,
) {
    sessions.retain(|s| {
        !dismissed(
            &format!("{:?}", s.agent_type).to_lowercase(),
            &s.id,
            &format!("{:?}", s.status).to_lowercase(),
        )
    });
}

#[cfg(test)]
mod dismissed_filter_tests {
    use super::filter_dismissed_cards;
    use crate::session::model::{AgentType, ProcessForm, Session, SessionStatus};

    fn card(id: &str, agent: AgentType, form: ProcessForm, status: SessionStatus) -> Session {
        Session {
            id: id.into(),
            agent_type: agent,
            project_name: "P".into(),
            project_path: String::new(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 7,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form,
            jump_supported: true,
            unread: false,
        }
    }

    /// T2 语义：写入 (tool, session, status) → 过滤剔除；状态变化 → key 不匹配重现；
    /// CLI 卡不参与；重启语义 = SESSION_DISMISALS 为进程内集合，兔维斯 重启清空后全部重现。
    #[test]
    fn dismissed_app_card_filtered_until_status_changes() {
        let mut sessions = vec![
            card(
                "s1",
                AgentType::WorkBuddy,
                ProcessForm::App,
                SessionStatus::Processing,
            ),
            card(
                "s2",
                AgentType::Codex,
                ProcessForm::App,
                SessionStatus::Idle,
            ),
            card(
                "cli",
                AgentType::Claude,
                ProcessForm::Cli,
                SessionStatus::Processing,
            ),
        ];
        // 模拟用户 X 掉 s1（当时红/运行中）——IPC 存储为小写 key
        let dismissed_keys = std::collections::HashSet::from([(
            "workbuddy".to_string(),
            "s1".to_string(),
            "processing".to_string(),
        )]);
        let dismissed = |t: &str, sid: &str, st: &str| {
            dismissed_keys.contains(&(t.to_string(), sid.to_string(), st.to_string()))
        };

        // 命中：App 卡被剔除（key 全匹配，Debug 形态转小写后一致）
        filter_dismissed_cards(&mut sessions, &dismissed);
        assert!(
            !sessions.iter().any(|s| s.id == "s1"),
            "dismiss 的卡应被过滤"
        );
        assert!(sessions.iter().any(|s| s.id == "s2"), "未 dismiss 的卡保留");
        assert!(
            sessions.iter().any(|s| s.id == "cli"),
            "未被 dismiss 的 CLI 卡保留（与 App 卡同规则：只剔命中的）"
        );

        // 状态变化（绿→等待）：key 不匹配 → 卡片重现
        let mut changed = vec![card(
            "s1",
            AgentType::WorkBuddy,
            ProcessForm::App,
            SessionStatus::Waiting,
        )];
        filter_dismissed_cards(&mut changed, &dismissed);
        assert!(
            changed.iter().any(|s| s.id == "s1"),
            "状态变化后 key 不匹配，卡片应重现"
        );

        // 重启语义（文档化）：SESSION_DISMISALS 为 Lazy 进程内集合，兔维斯 重启即清空，
        // 全部卡片重现；本纯函数测试经注入闭包模拟「空集合」即可覆盖
        filter_dismissed_cards(&mut changed, &|_, _, _| false);
        assert_eq!(changed.len(), 1);
    }

    /// CLI 卡与 App 卡同规则：X 掉即过滤，状态变化后重现（2026-09-14 验收裁决：
    /// 终端类卡此前无 X 无法主动关闭，现全形态接入）
    #[test]
    fn dismissed_cli_card_filtered_until_status_changes() {
        let mut sessions = vec![card(
            "t1",
            AgentType::Claude,
            ProcessForm::Cli,
            SessionStatus::Waiting,
        )];
        let dismissed_keys = std::collections::HashSet::from([(
            "claude".to_string(),
            "t1".to_string(),
            "waiting".to_string(),
        )]);
        let dismissed = |t: &str, sid: &str, st: &str| {
            dismissed_keys.contains(&(t.to_string(), sid.to_string(), st.to_string()))
        };
        filter_dismissed_cards(&mut sessions, &dismissed);
        assert!(sessions.is_empty(), "被 X 的 CLI 卡应被过滤");

        // 状态变化（红→绿）：key 不匹配 → 重现
        let mut changed = vec![card(
            "t1",
            AgentType::Claude,
            ProcessForm::Cli,
            SessionStatus::Finished,
        )];
        filter_dismissed_cards(&mut changed, &dismissed);
        assert!(
            changed.iter().any(|s| s.id == "t1"),
            "CLI 卡状态变化后应重现"
        );
    }
}

/// HOME 环境变量注入基建（#66/#151 测试用）：部分生产入口硬编码
/// `dirs::home_dir()`（services::detect_subagents / subagents::opencode::
/// child_updated_anchors / subagents::codex::collect），无 path/env 参数缝——
/// Unix 下 `dirs::home_dir` 优先读 `$HOME`，重定向 HOME 即可控数据根。
///
/// 串行化纪律（家族先例：`kimi_parser::HOME_LOCK`，见其模块注释的实测教训）：
/// HOME 是**进程级**量，并行测试线程共享——所有 set HOME 的用例必须共用
/// **同一把锁**（services / codex_parser / opencode_parser 三处），各自持私有锁
/// 等于没锁。窗口内其余读 HOME 的并行路径（各家 collect 生产入口）均期待
/// 「无数据 → 空态」，tempdir home 同样返回空，语义安全。
#[cfg(test)]
pub(crate) mod test_env {
    use std::path::Path;
    use std::sync::Mutex;

    pub(crate) static HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 临时把 HOME 指到 `home` 执行 `f`，结束后恢复原值（原本未设则移除）。
    /// 必须持 HOME_ENV_LOCK 调用（锁在调用方拿，便于一个用例内多次切换）
    pub(crate) fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let r = f();
        match old {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        r
    }
}
