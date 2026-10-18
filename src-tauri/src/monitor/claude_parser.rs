// Claude Code 会话解析 — message.role + content[] 协议
// 移植自 agent-sessions session/parser.rs；公共设施（cwd 归一化、路径编解码、
// git URL 缓存、JSONL 读取）见 monitor::{cwd,path_codec,git,project,jsonl}

use super::cwd::normalize_cwd_for_match;
use super::git::get_github_url;
use super::jsonl::{extract_cwd_from_jsonl, get_recent_jsonl_files, read_recent_lines};
use super::path_codec::{convert_dir_name_to_path, convert_path_to_dir_name};
use super::project::project_name_from_path;
use super::session_scan::SessionFileScan;
use super::status::*;
use crate::adapter::AgentProcess;
use crate::session::model::JsonlMessage;
use crate::session::{jump_supported_for, AgentType, Session, SessionStatus};
use log::info;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

const RECENT_LINES: usize = 500;

/// L2 摘要缓存（monitor::session_scan）：claude 为进程界定有界扫描（目录名预过滤），
/// L1+L2 已足够——窗口化反而会丢空闲超窗的活跃卡（见 session_scan 模块文档）
const CLAUDE_CWD_SCAN: SessionFileScan = SessionFileScan::new("claude-cwd");
const CLAUDE_DIGEST_SCAN: SessionFileScan = SessionFileScan::new("claude-digest");

/// 扫描 ~/.claude/projects，匹配运行中的 Claude 进程
pub fn get_claude_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let mut sessions = Vec::new();

    // cwd -> processes 映射
    let mut cwd_to_processes: HashMap<String, Vec<&AgentProcess>> = HashMap::new();
    let mut expected_dir_names: HashSet<String> = HashSet::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            let normalized = normalize_cwd_for_match(&cwd.to_string_lossy());
            // 目录名比较大小写不敏感：Claude 保留用户 cd 时敲入的盘符/路径大小写
            // （实机同时存在 E--xxx 与 e--xxx 两种目录），sysinfo 返回的可能是另一种
            expected_dir_names.insert(convert_path_to_dir_name(&normalized).to_lowercase());
            cwd_to_processes
                .entry(normalized)
                .or_default()
                .push(process);
        }
    }

    let claude_dir = dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .unwrap_or_default();
    if !claude_dir.exists() {
        return sessions;
    }

    if let Ok(entries) = fs::read_dir(&claude_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !expected_dir_names.contains(&dir_name.to_lowercase()) {
                continue;
            }

            let jsonl_files = get_recent_jsonl_files(&path);
            if jsonl_files.is_empty() {
                continue;
            }

            let mut cwd_to_files: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
            for f in &jsonl_files {
                // L2：cwd 提取走 (mtime,size) 缓存（纯内容函数）；未解析出 cwd 才用目录名反推
                let file_cwd = CLAUDE_CWD_SCAN
                    .parse(f, extract_cwd_from_jsonl)
                    .as_ref()
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| convert_dir_name_to_path(dir_name));
                // 与进程 cwd 同一归一化域（Windows 下小写、无尾部分隔符），两侧才可比
                cwd_to_files
                    .entry(normalize_cwd_for_match(&file_cwd))
                    .or_default()
                    .push(f.clone());
            }

            for (project_path, files) in &cwd_to_files {
                if let Some(procs) = cwd_to_processes.get(project_path) {
                    for (idx, proc) in procs.iter().enumerate() {
                        if let Some(f) = files.get(idx) {
                            if let Some(mut session) = parse_claude_jsonl(f, project_path, proc) {
                                // #134：徽标改用与子代理面板（chips）同源的
                                // monitor::subagents 重算——旧 count_active_subagents
                                // 只扫项目目录一层（旧平铺布局），新布局
                                // <sid>/subagents/ 扫不到恒 0。计数口径 = Running
                                // 过滤数（chips 前端同口径）；缓存键同
                                // ("claude", session_id)，与 remote 端点共用增量缓存。
                                session.active_subagent_count = crate::monitor::subagents::claude::collect_at(f, &session.id)
                                    .iter()
                                    .filter(|v| v.status == crate::monitor::subagents::SubagentStatus::Running)
                                    .count();
                                sessions.push(session);
                            }
                        }
                    }
                }
            }
        }
    }

    info!(
        "Claude: {} sessions from {} processes",
        sessions.len(),
        processes.len()
    );
    sessions
}

/// 纯内容摘要（L2 缓存产物）：只由文件内容决定；100 字符截断随摘要一并缓存
struct ClaudeFileDigest {
    session_id: Option<String>,
    git_branch: Option<String>,
    last_timestamp: Option<String>,
    last_msg_type: Option<String>,
    last_has_tool_use: bool,
    last_has_text: bool,
    last_has_tool_result: bool,
    last_is_local: bool,
    last_is_interrupted: bool,
    last_is_user_input: bool,
    is_compacting: bool,
    last_message: Option<String>,
    /// T7：末条预览消息是否为子 agent 回报（提醒开关打标；谓词与判据同源）
    last_is_subagent_report: bool,
    last_role: Option<String>,
}

fn read_claude_digest(jsonl_path: &Path) -> ClaudeFileDigest {
    let recent = read_recent_lines(jsonl_path, RECENT_LINES);

    let mut session_id = None;
    let mut git_branch = None;
    let mut last_timestamp = None;
    let mut last_message = None;
    let mut last_role = None;
    let mut last_msg_type = None;
    let mut last_has_tool_use = false;
    let mut last_has_text = false;
    let mut last_has_tool_result = false;
    let mut last_is_local = false;
    let mut last_is_interrupted = false;
    let mut last_is_user_input = false;
    let mut found_status = false;
    let mut is_compacting = false;

    for line in recent.iter().rev() {
        if let Ok(msg) = serde_json::from_str::<JsonlMessage>(line) {
            if session_id.is_none() {
                session_id = msg.session_id;
            }
            if git_branch.is_none() {
                git_branch = msg.git_branch;
            }
            if last_timestamp.is_none() {
                last_timestamp = msg.timestamp;
            }

            if !found_status && !is_compacting {
                if msg.is_compact_summary == Some(true) {
                    // compaction 已完成
                } else if msg.subtype.as_deref() == Some("compact_boundary") {
                    is_compacting = true;
                }
            }

            if !found_status {
                if let Some(content) = &msg.message {
                    if let Some(c) = &content.content {
                        let has_content = match c {
                            serde_json::Value::String(s) => !s.is_empty(),
                            serde_json::Value::Array(arr) => !arr.is_empty(),
                            _ => false,
                        };
                        if has_content {
                            last_msg_type = msg.msg_type.clone();
                            last_role = content.role.clone();
                            last_has_tool_use = has_tool_use(c);
                            last_has_text = has_text_block(c);
                            last_has_tool_result = has_tool_result(c);
                            last_is_local = is_local_slash_command(c);
                            last_is_interrupted = is_interrupted_request(c);
                            last_is_user_input = is_waiting_for_user_input(c);
                            found_status = true;
                        }
                    }
                }
            }

            if session_id.is_some() && found_status {
                break;
            }
        }
    }

    // 找最后一条有文本的消息作为预览
    for line in recent.iter().rev() {
        if let Ok(msg) = serde_json::from_str::<JsonlMessage>(line) {
            if let Some(content) = &msg.message {
                if let Some(c) = &content.content {
                    let text = match c {
                        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                        serde_json::Value::Array(arr) => arr.iter().find_map(|v| {
                            v.get("text")
                                .and_then(|t| t.as_str())
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                        }),
                        _ => None,
                    };
                    if text.is_some() {
                        last_message = text;
                        break;
                    }
                }
            }
        }
    }

    // T7 打标：判原文（截断可能切掉尾部标记）；谓词复用判据层信号原语（同源，
    // spec §四.3——前端只消费布尔，永不匹配文案）
    let last_is_subagent_report = last_message
        .as_deref()
        .map(crate::monitor::subagents::claude::is_subagent_report_text)
        .unwrap_or(false);

    let last_message = last_message.map(|m| {
        if m.chars().count() > 100 {
            format!("{}...", m.chars().take(100).collect::<String>())
        } else {
            m
        }
    });

    ClaudeFileDigest {
        session_id,
        git_branch,
        last_timestamp,
        last_msg_type,
        last_has_tool_use,
        last_has_text,
        last_has_tool_result,
        last_is_local,
        last_is_interrupted,
        last_is_user_input,
        is_compacting,
        last_message,
        last_is_subagent_report,
        last_role,
    }
}

/// 解析单个 Claude JSONL 文件（L2 缓存摘要 + 时间叠加/进程盖章现算，保留原签名）
fn parse_claude_jsonl(
    jsonl_path: &Path,
    project_path: &str,
    process: &AgentProcess,
) -> Option<Session> {
    use std::time::SystemTime;

    let d = CLAUDE_DIGEST_SCAN.parse(jsonl_path, read_claude_digest);
    let digest = d.as_ref();
    // 时间叠加每次现算（L2 契约）：recently-modified 参与状态判定，不能进缓存
    let file_recently_modified = jsonl_path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|age| age.as_secs_f32() < 3.0)
        .unwrap_or(false);

    let session_id = digest.session_id.clone()?;
    // 卡片前缀统一 8 位（按字符截取，多字节 id 不 panic），与 hook marker（TUVIS:<id 前 8 位>）保持一致
    let session_title = session_id.chars().take(8).collect::<String>();
    let status = if digest.is_compacting {
        SessionStatus::Compacting
    } else {
        determine_status(
            digest.last_msg_type.as_deref(),
            digest.last_has_tool_use,
            digest.last_has_text,
            digest.last_has_tool_result,
            digest.last_is_local,
            digest.last_is_interrupted,
            digest.last_is_user_input,
            file_recently_modified,
        )
    };

    let project_name = project_name_from_path(project_path);
    Some(Session {
        id: session_id,
        agent_type: AgentType::Claude,
        project_name,
        project_path: project_path.to_string(),
        git_branch: digest.git_branch.clone(),
        github_url: get_github_url(project_path),
        status,
        last_message: digest.last_message.clone(),
        last_message_role: digest.last_role.clone(),
        last_message_subagent_report: digest.last_is_subagent_report,
        flap_from_subagent_activity: false,
        last_activity_at: digest
            .last_timestamp
            .clone()
            .unwrap_or_else(|| "Unknown".to_string()),
        pid: process.pid,
        cpu_usage: process.cpu_usage,
        active_subagent_count: 0,
        form: process.form,
        jump_supported: jump_supported_for(process.form),
        unread: false, // 扫描出的活跃卡默认非未读；未读卡由 adapter 层合并
        title: Some(session_title),
    })
}

#[cfg(test)]
mod title_tests {
    use super::*;
    use crate::session::ProcessForm;

    fn fake_process(pid: u32) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: Some(std::path::PathBuf::from("/work/demo")),
            form: ProcessForm::Cli,
            exe: None,
        }
    }

    /// sessionId 含多字节字符时，标题回退按字符取前 8 位，不得 panic
    #[test]
    fn multibyte_session_id_title_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let jsonl = tmp.path().join("session.jsonl");
        std::fs::write(
            &jsonl,
            concat!(
                r#"{"sessionId":"会话🔥x","cwd":"/work/demo","timestamp":"2026-01-01T00:00:00Z","#,
                r#""type":"user","message":{"role":"user","content":"hi"}}"#
            ),
        )
        .unwrap();
        let session =
            parse_claude_jsonl(&jsonl, "/work/demo", &fake_process(1)).expect("应解析出会话");
        // "会话🔥x" 共 4 个字符，不足 8 位时整串即标题
        assert_eq!(session.title.as_deref(), Some("会话🔥x"));
    }

    /// 末行 = thinking-only assistant（无正文无工具调用）→ Thinking（黄），不得瞬绿。
    /// 这是「长思考期间每落一条 thinking 行就误报任务完成」的回归锁（2026-09-19）
    #[test]
    fn thinking_only_assistant_tail_is_thinking_not_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let jsonl = tmp.path().join("session.jsonl");
        std::fs::write(
            &jsonl,
            concat!(
                r#"{"sessionId":"sess-thinking-1","cwd":"/work/demo","timestamp":"2026-01-01T00:00:00Z","#,
                r#""type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"推演中……"}]}}"#
            ),
        )
        .unwrap();
        let session =
            parse_claude_jsonl(&jsonl, "/work/demo", &fake_process(1)).expect("应解析出会话");
        assert_eq!(session.status, crate::session::SessionStatus::Thinking);
    }

    /// 末行 = assistant 正文（有 text 无 tool_use）→ Idle（绿），确认新参数不误伤完成信号
    #[test]
    fn assistant_text_tail_still_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let jsonl = tmp.path().join("session.jsonl");
        std::fs::write(
            &jsonl,
            concat!(
                r#"{"sessionId":"sess-text-1","cwd":"/work/demo","timestamp":"2026-01-01T00:00:00Z","#,
                r#""type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"完成了"}]}}"#
            ),
        )
        .unwrap();
        let session =
            parse_claude_jsonl(&jsonl, "/work/demo", &fake_process(1)).expect("应解析出会话");
        assert_eq!(session.status, crate::session::SessionStatus::Idle);
    }

    // ==== 2026-10-09 观察台 T7：lastMessage 子 agent 回报打标（同源谓词）====

    /// teammate 报告 / task-notification → true；普通文本 → false（乙.3.4 实证形态）
    #[test]
    fn digest_marks_subagent_report() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("parent.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-08T15:02:26Z","message":{"role":"user","content":"Another Claude session sent a message: <teammate-message teammate_id=\"bg\">任务完成</teammate-message>"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let d = read_claude_digest(&p);
        assert!(d.last_is_subagent_report);
        std::fs::write(
            &p,
            r#"{"type":"user","timestamp":"2026-10-08T15:09:43Z","message":{"role":"user","content":"主 agent 自己的回执复述"}}"#,
        )
        .unwrap();
        let d = read_claude_digest(&p);
        assert!(
            !d.last_is_subagent_report,
            "主会话自身动作不打标（乙.3.4 23:09:43 案例）"
        );
    }

    // ==== #134：看板父卡徽标（active_subagent_count 同源重算组合）====
    // 取舍申报：赋值点在 get_claude_sessions 循环内（读真实 ~/.claude/projects，
    // 无 home/path 注入缝，测试不可达）；此处按赋值点同组合直测
    // 「parse_claude_jsonl 出 id → collect_at → Running 过滤计数」，
    // 布局推导的等价性由 subagents/claude.rs collect_at_equivalent_to_locate_discovery 锁定。

    /// 徽标组合夹具：新布局（项目目录/<sid>.jsonl 父转写 + <sid>/subagents/ 登记）。
    /// running_agents：无停止事件的子代理 id；idle_agents：父转写追加停止通知的 id
    fn badge_fixture(
        dir: &std::path::Path,
        sid: &str,
        running_agents: &[&str],
        idle_agents: &[&str],
    ) -> std::path::PathBuf {
        let proj = dir.join("proj--work--demo");
        let subagents = proj.join(sid).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let parent = proj.join(format!("{sid}.jsonl"));
        let mut parent_body = format!(
            concat!(
                r#"{{"sessionId":"{sid}","cwd":"/work/demo","timestamp":"2026-10-08T07:00:00Z","#,
                r#""type":"user","message":{{"role":"user","content":"hi"}}}}"#,
                "\n"
            ),
            sid = sid
        );
        for id in running_agents.iter().chain(idle_agents) {
            std::fs::write(
                subagents.join(format!("agent-{id}.meta.json")),
                r#"{"agentType":"Plan"}"#,
            )
            .unwrap();
            std::fs::write(subagents.join(format!("agent-{id}.jsonl")), "").unwrap();
        }
        for id in idle_agents {
            // 停止通知（task-id 块）→ 该子代理转 Idle
            parent_body.push_str(&format!(
                concat!(
                    r#"{{"type":"user","timestamp":"2026-10-08T07:40:00Z","message":{{"role":"user","#,
                    r#""content":"<task-notification><task-id>{id}</task-id><summary>Agent stopped</summary></task-notification>"}}}}"#,
                    "\n"
                ),
                id = id
            ));
        }
        std::fs::write(&parent, parent_body).unwrap();
        parent
    }

    /// 徽标计数 = Running 过滤数（chips 前端同口径）：1 Running + 1 Idle → 1
    #[test]
    fn active_subagent_count_counts_running_via_collect_at() {
        crate::monitor::subagents::reset_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let parent = badge_fixture(dir.path(), "sess-badge-1", &["run1"], &["idle1"]);
        let session =
            parse_claude_jsonl(&parent, "/work/demo", &fake_process(1)).expect("应解析出会话");
        // 赋值点同组合（get_claude_sessions 内三行的等价直测）
        let count = crate::monitor::subagents::claude::collect_at(&parent, &session.id)
            .iter()
            .filter(|v| v.status == crate::monitor::subagents::SubagentStatus::Running)
            .count();
        assert_eq!(
            count, 1,
            "新布局下徽标恢复真实值：1 Running（旧实现扫不到恒 0）"
        );
        crate::monitor::subagents::reset_cache_for_tests();
    }

    /// 全 Idle → 0（徽标不显示的渲染条件来源）
    #[test]
    fn active_subagent_count_zero_when_all_idle() {
        crate::monitor::subagents::reset_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let parent = badge_fixture(dir.path(), "sess-badge-2", &[], &["i1", "i2"]);
        let session =
            parse_claude_jsonl(&parent, "/work/demo", &fake_process(1)).expect("应解析出会话");
        let count = crate::monitor::subagents::claude::collect_at(&parent, &session.id)
            .iter()
            .filter(|v| v.status == crate::monitor::subagents::SubagentStatus::Running)
            .count();
        assert_eq!(count, 0, "全 Idle → 0（无子代理徽标不显示）");
        crate::monitor::subagents::reset_cache_for_tests();
    }

    /// 无 subagents 目录（旧平铺布局 / 无子代理会话）→ 0
    #[test]
    fn active_subagent_count_zero_without_subagents_dir() {
        crate::monitor::subagents::reset_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let parent = badge_fixture(dir.path(), "sess-badge-3", &[], &[]);
        let session =
            parse_claude_jsonl(&parent, "/work/demo", &fake_process(1)).expect("应解析出会话");
        let count = crate::monitor::subagents::claude::collect_at(&parent, &session.id)
            .iter()
            .filter(|v| v.status == crate::monitor::subagents::SubagentStatus::Running)
            .count();
        assert_eq!(
            count, 0,
            "无子代理会话一次 is_dir 即止（预算契约 L3 不扩面）"
        );
        crate::monitor::subagents::reset_cache_for_tests();
    }

    /// 端到端（#134 验收主路径）：HOME 重定向走 get_claude_sessions 全链路——
    /// 新布局会话 1 Running + 1 Idle 子代理 → 徽标 = 1。旧实现
    /// count_active_subagents 只扫项目目录一层平铺 agent-*.jsonl，新布局扫不到
    /// → 本用例对旧实现必红（RED 验证锚）。
    #[test]
    fn get_claude_sessions_badge_end_to_end_new_layout() {
        crate::monitor::subagents::reset_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        // 夹具造 <home>/proj--work--demo/<sid>…，整目录搬进 HOME/.claude/projects/
        // 并按 cwd 编码重命名（convert_path_to_dir_name("/work/demo") = "-work-demo"）
        badge_fixture(&home, "sess-e2e-badge", &["run1"], &["idle1"]);
        let projects = home.join(".claude").join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::rename(home.join("proj--work--demo"), projects.join("-work-demo")).unwrap();
        let guard = crate::monitor::test_env::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sessions =
            crate::monitor::test_env::with_home(&home, || get_claude_sessions(&[fake_process(1)]));
        drop(guard);
        assert_eq!(
            sessions.len(),
            1,
            "进程 cwd=/work/demo 匹配 -work-demo 项目目录出卡"
        );
        assert_eq!(
            sessions[0].active_subagent_count, 1,
            "新布局端到端：徽标 = Running 数（旧 count_active_subagents 扫项目目录一层恒 0）"
        );
        crate::monitor::subagents::reset_cache_for_tests();
    }
}
