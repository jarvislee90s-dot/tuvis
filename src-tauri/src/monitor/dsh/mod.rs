// dsh（DeepSeek harness）监控解析 — M1 只读底座
// 会话存储/事件语义全部依据 M0 探测报告（research/dsh-probe-2026-09-13-report.md）
// 双数据源：storages/session_projcache（首选）+ sessions/<项目>/<会话>/session.vN.jsonl.zstd（兜底）

pub mod decode;
pub mod log;
pub mod preview;
pub mod projcache;
pub mod status;

use crate::adapter::AgentProcess;
use crate::monitor::session_scan::SessionFileScan;
use crate::session::{AgentType, ProcessForm, Session, SessionStatus};

pub use status::LockState;

// L2 内容缓存（monitor::session_scan 预算层，AGENTS.md 新工具接入模板同款）：
// 代际日志的解析产物按 (mtime, size) 缓存——前端 3 秒轮询下未变化的历史会话
// 零解码零解析。M1 实测真机 77 会话/解压 155MB 每轮全量重扫 12.5s（debug），
// 远超 3s 轮询间隔且跑在 IPC 线程上，是 dev 模式整机卡顿的根因
const DSH_LOG_SCAN: SessionFileScan = SessionFileScan::new("dsh-log");

/// 单个会话代际日志的纯内容解析产物（不含 lock / 时间叠加，可安全跨轮询缓存）
#[derive(Debug, Clone)]
pub(crate) struct DshSessionDigest {
    pub(crate) header: log::DshHeader,
    pub(crate) is_subagent: bool,
    pub(crate) facts: status::TurnFacts,
    /// (role, text) 预览（真人输入/助手回复取 seq 更新者，注入已过滤）
    pub(crate) preview: (Option<String>, Option<String>),
    /// 日志侧标题（session/title 最新事件；projcache 优先的叠加在外层按轮现算）
    pub(crate) log_title: Option<String>,
    /// 事件流最大 time（未读锚点；None = 无 time 字段时外层兜底 mtime）
    pub(crate) max_event_time_ms: Option<i64>,
    /// 所选代际文件 mtime（静默兜底 / last_activity 兜底输入）
    pub(crate) log_mtime_ms: i64,
}

/// 读取（带 L2 缓存）代际日志解析产物：内容未变 → 同一 Arc 秒回；损坏/不可读
/// → 缓存 None（下次 mtime/size 变化时自动重试，与无缓存版"读不到→跳过"同语义）
fn load_digest(
    scan: &SessionFileScan,
    gen_path: &std::path::Path,
) -> std::sync::Arc<Option<DshSessionDigest>> {
    scan.parse(gen_path, build_digest)
}

// 纯内容解析（session_scan.rs 要求：时间叠加绝不在此函数内）。
// TEST_DIGEST_CALLS：测试可见的调用计数（thread_local——scan_sessions 是
// 同步调用链，计数与断言同线程，天然免疫并行测试对全局计数器的污染）
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DIGEST_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn build_digest(gen_path: &std::path::Path) -> Option<DshSessionDigest> {
    #[cfg(test)]
    TEST_DIGEST_CALLS.with(|c| c.set(c.get() + 1));
    let bytes = std::fs::read(gen_path).ok()?;
    let text = if gen_path.extension().map(|e| e == "zstd").unwrap_or(false) {
        decode::decode_zstd_frames(&bytes).ok()?.text
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    let header = log::parse_header(&text)?;
    let events = log::parse_events(&text);
    let preview = preview::extract(&events);
    let log_title = preview::title(&events, None);
    let max_event_time_ms = events.iter().filter_map(|e| e.time).max();
    let log_mtime_ms = std::fs::metadata(gen_path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(DshSessionDigest {
        is_subagent: log::is_subagent(&header),
        header,
        facts: status::scan_facts(&events),
        preview,
        log_title,
        max_event_time_ms,
        log_mtime_ms,
    })
}

/// dsh 数据根：$DSH_HOME 覆盖（M0 F14 优先级：env > ~/.dsh），测试注入用
pub fn dsh_home() -> std::path::PathBuf {
    dsh_home_with(&dirs::home_dir().unwrap_or_default())
}

/// dsh 数据根（home 注入版）：skill_dir_for_tool 注册表与 adapter 保持同一
/// 路径单源（kimi/zcode 的 *_home_with 同款模式）
pub fn dsh_home_with(home_dir: &std::path::Path) -> std::path::PathBuf {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".dsh"))
}

/// 桌面宿主特征单源（cmdline_is_dsh_host 桌面分支与跳转分派共用）：
/// 任一参数含 "dsh-desktop-host"（包路径子串，最强特征）或以
/// `\.dsh\profiles\desktop` / `/.dsh/profiles/desktop` 结尾（用户数据 profile 路径）
pub fn cmdline_is_dsh_desktop_host(cmd: &[std::ffi::OsString]) -> bool {
    let tokens: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    tokens.iter().any(|t| {
        t.contains("dsh-desktop-host")
            || t.ends_with("\\.dsh\\profiles\\desktop")
            || t.ends_with("/.dsh/profiles/desktop")
    })
}

/// dsh 宿主 cmdline 判定门（单源，M0 §5 + H1 桌面端扩展）：两类形态任一命中即宿主——
/// ① web 宿主：cmdline 含 "dsh"（精确令牌，或 /dsh、\dsh 路径结尾）与 "web"（精确令牌）；
/// ② 桌面宿主（v0.2.0+）：DeepSeek Harness.exe 内嵌 dsh-desktop-host 进程，无 node 令牌，
///    特征判定复用 cmdline_is_dsh_desktop_host；
///    --expose-internals 为 Electron 通用旗子，单独过弱，不作独立判据。
/// find_dsh_processes（进程发现）与 host::tool_host_alive_in（宿主存活判定）
/// 共用本口径，防两处判定漂移
pub fn cmdline_is_dsh_host(cmd: &[std::ffi::OsString]) -> bool {
    let tokens: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let has_dsh = tokens
        .iter()
        .any(|t| t == "dsh" || t.ends_with("/dsh") || t.ends_with("\\dsh"));
    let has_web = tokens.iter().any(|t| t == "web");
    (has_dsh && has_web) || cmdline_is_dsh_desktop_host(cmd)
}

/// 当前桌面上 dsh 桌面端宿主进程 pid（H1 跳转分派用；无则 None → 走 web 宿主路径）
pub fn find_dsh_desktop_host_pid(system: &sysinfo::System) -> Option<u32> {
    system
        .processes()
        .iter()
        .find(|(_, p)| !p.cmd().is_empty() && cmdline_is_dsh_desktop_host(p.cmd()))
        .map(|(pid, _)| pid.as_u32())
}

/// 进程发现：node 进程且 cmdline 含 "dsh" 与 "web" 令牌（M0 §5：进程名是 node，
/// 必须按 cmdline 判定；取命中的第一个作为宿主——esbuild 等子进程 cmdline 无此二令牌）
pub fn find_dsh_processes(system: &sysinfo::System) -> Vec<AgentProcess> {
    let mut out = Vec::new();
    for (pid, process) in system.processes() {
        let cmd = process.cmd();
        if cmd.is_empty() {
            continue;
        }
        // 判定口径单源委托 cmdline_is_dsh_host（host.rs 存活判定同款）
        if cmdline_is_dsh_host(cmd) {
            out.push(AgentProcess {
                pid: pid.as_u32(),
                cpu_usage: process.cpu_usage(),
                cwd: process.cwd().map(|p| p.to_path_buf()),
                exe: process.exe().map(|p| p.to_path_buf()),
                form: ProcessForm::App,
            });
            // 只需一个宿主（卡片 pid 用）——命中即取第一个，后续同名进程不再收集
            break;
        }
    }
    out
}

/// 会话锁持有探测（零干扰）：lock 文件不存在 → Unknown；有文件则问 lsof 是否有进程开着它
fn probe_lock_state(session_dir: &std::path::Path) -> LockState {
    let lock = session_dir.join("session.lock");
    if !lock.exists() {
        return LockState::Unknown; // v0 旧目录无锁文件（M0 F3）
    }
    let out = std::process::Command::new("lsof")
        .arg("-t")
        .arg(&lock)
        .output();
    match out {
        Ok(o) if o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty() => {
            LockState::Held
        }
        Ok(_) => LockState::Free,     // 文件在但无人持有 → 写入者已死
        Err(_) => LockState::Unknown, // lsof 不可用（如 Windows）→ 静默兜底
    }
}

/// 会话聚合：宿主在位时扫描会话目录出卡；未运行 → 无卡。
/// 出现周期对齐 zcode/codex（用户 2026-09-14 验收裁决）：仅最近 24h 有活动的
/// 会话出卡，历史超窗会话不主动上板
pub fn get_dsh_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(host) = processes.first() else {
        return Vec::new();
    };
    scan_sessions(&dsh_home(), host, &DSH_LOG_SCAN)
}

/// 卡片出现窗口（对齐 zcode `CARD_WINDOW_MS` 语义）：最近 24h 内有活动的会话
/// 才出卡——历史超窗会话不主动上板，开局不再 flood 几十张历史已完成卡
const CARD_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// 出卡上限（对齐 zcode `RECENT_SESSIONS_LIMIT`）：按活跃度倒序取最近 N 张
const RECENT_SESSIONS_LIMIT: usize = 100;

/// 内部扫描（home 注入，测试直调——避免 DSH_HOME 环境变量在并行测试中互踩）。
/// scan 注入式（R1 同思路）：测试用私有 SessionFileScan，防全局 namespace 被
/// 并行测试的 retain_existing 互踩（含本测试注入条目被别处清掉的时序问题）
fn scan_sessions(
    home: &std::path::Path,
    host: &AgentProcess,
    scan: &SessionFileScan,
) -> Vec<Session> {
    let sessions_root = home.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_root) else {
        return Vec::new();
    };
    let now_ms = chrono::Utc::now().timestamp_millis();

    // (活跃度毫秒, 卡) 成对收集：窗口过滤 + 活跃度倒序截断后统一出卡
    let mut cards: Vec<(i64, Session)> = Vec::new();
    // 本轮扫描命中的代际日志全集（缓存收敛用，防已删会话的孤儿条目常驻）
    let mut live_logs: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for project_dir in entries.flatten() {
        let ppath = project_dir.path();
        if !ppath.is_dir() {
            continue;
        }
        let name = project_dir.file_name().to_string_lossy().to_string();
        if name.starts_with("session_projcache") || name == "workspace.json" {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(&ppath) else {
            continue;
        };
        for sdir in session_dirs.flatten() {
            let spath = sdir.path();
            if !spath.is_dir() {
                continue;
            }
            // 代际选择（取代际最大者）。预过滤反模式禁令（AGENTS.md L3-4）：窗口判定
            // 必须发生在打开/解压文件之前——先 stat 代际文件 mtime，超窗且无缓存命中
            // → 文件不读不解压（冷启动 77 会话 155MB 全量解码的教训）
            let Some((version, gen_path)) = log::generation_logs(&spath).pop() else {
                continue;
            };
            let mtime_ms = std::fs::metadata(&gen_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);
            let fresh = match mtime_ms {
                Some(t) => t >= now_ms - CARD_WINDOW_MS,
                None => true, // stat 不可得 → 不预过滤（与无缓存语义一致）
            };
            if !fresh && scan.peek::<Option<DshSessionDigest>>(&gen_path).is_none() {
                continue; // 超窗且从未解析过 → 跳过，文件不打开
            }
            // 缓存保活先于 digest 判空：解析失败缓存为 None 的条目（0 字节新建/
            // 损坏文件）也是有效缓存——若因 None 不保活，retain_existing 当轮逐出，
            // 窗内坏文件每轮重读重解压（L2 对解析失败失效，评审 R4）
            let digest_arc = load_digest(scan, &gen_path);
            live_logs.insert(gen_path);
            let Some(digest) = digest_arc.as_ref().clone() else {
                continue;
            };
            let header = &digest.header;
            if digest.is_subagent {
                continue; // 子 Agent 不出卡（M0 F7）
            }
            // 版本门（设计 P5 + 备忘 A8）：header.version 超出已知集（0..=3）→
            // 降级卡"格式待适配"（未知语义不猜——探测红线），不影响其他会话
            if let Some(v) = header.version {
                if !(0..=3).contains(&v) {
                    ::log::warn!("dsh: 会话 {} 为未知代际 v{}，出降级卡", header.id, v);
                    // 超窗拦截已前移到 stat 预过滤（两分支共用），此处无需重复
                    cards.push((
                        digest.log_mtime_ms,
                        Session {
                            id: header.id.clone(),
                            agent_type: AgentType::Dsh,
                            project_name: header
                                .cwd
                                .as_deref()
                                .map(crate::monitor::project::project_name_from_path)
                                .unwrap_or_else(|| name.clone()),
                            project_path: header.cwd.clone().unwrap_or_default(),
                            title: Some(format!("dsh 格式待适配（v{}）", v)),
                            git_branch: None,
                            github_url: None,
                            status: SessionStatus::Idle,
                            last_message: None,
                            last_message_role: None,
                            last_message_subagent_report: false,
                            flap_from_subagent_activity: false,
                            last_activity_at: chrono::DateTime::from_timestamp_millis(
                                digest.log_mtime_ms,
                            )
                            .map(|d| d.to_rfc3339())
                            .unwrap_or_default(),
                            pid: host.pid,
                            cpu_usage: host.cpu_usage,
                            active_subagent_count: 0,
                            form: ProcessForm::App,
                            jump_supported: crate::session::jump_supported_for(ProcessForm::App),
                            unread: false,
                            unread_turned_green_at_ms: None,
                        },
                    ));
                    continue;
                }
            }

            // 双源：projcache（identity 过校验才可用；小文件每轮直读——量级远低于日志）
            let cache = projcache::load(home, &header.id)
                .filter(|_| projcache_identity_ok(home, header, version));

            // 状态：lock 交叉判定 + 静默兜底。lock 仅开裔回合才探测（lsof 子进程，
            // 终审 I2）——闭合回合的 derive 不消费 lock，探测是纯浪费
            let lock = if digest.facts.has_open_turn() {
                probe_lock_state(&spath)
            } else {
                LockState::Unknown
            };
            let silence_ms = now_ms - digest.log_mtime_ms;
            let outcome = status::derive_facts(&digest.facts, lock, Some(silence_ms));

            let (role, text) = digest.preview.clone();
            let title = cache
                .as_ref()
                .and_then(|c| c.title.clone())
                .or_else(|| digest.log_title.clone());
            let last_activity_ms = cache
                .as_ref()
                .and_then(|c| c.last_prompt_at)
                .or(digest.max_event_time_ms)
                .unwrap_or(digest.log_mtime_ms);

            // 出现周期已在 stat 预过滤统一执行（mtime 口径，含"缓存命中例外"）；
            // 此处不再按 last_activity 重复拦截——两个口径不一致会把缓存命中的
            // 超窗会话重新拦掉（R2 测试抓取），出卡排序活跃度仍用 last_activity
            cards.push((
                last_activity_ms,
                Session {
                    id: header.id.clone(),
                    agent_type: AgentType::Dsh,
                    project_name: header
                        .cwd
                        .as_deref()
                        .map(crate::monitor::project::project_name_from_path)
                        .unwrap_or_else(|| name.clone()),
                    project_path: header.cwd.clone().unwrap_or_default(),
                    title,
                    git_branch: None,
                    github_url: None,
                    status: outcome.status,
                    last_message: text,
                    last_message_role: role,
                    last_message_subagent_report: false,
                    flap_from_subagent_activity: false,
                    last_activity_at: chrono::DateTime::from_timestamp_millis(last_activity_ms)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    pid: host.pid,
                    cpu_usage: host.cpu_usage,
                    active_subagent_count: 0,
                    form: ProcessForm::App,
                    jump_supported: crate::session::jump_supported_for(ProcessForm::App),
                    // 未读态不由扫描侧自判：dsh 卡是 App 形态，由 adapter 层未读池
                    // 管线（W4）统一标记/清除——绿卡「池行在⇒未读、已读删行⇒P1-3 剔除」，
                    // 与 codex/zcode 数据驱动绿卡同一套出现周期
                    unread: false,
                    unread_turned_green_at_ms: None,
                },
            ));
        }
    }
    // 收敛缓存：清掉本轮未命中的代际日志条目（会话目录已被删除等）
    scan.retain_existing(&live_logs);
    take_recent(cards, RECENT_SESSIONS_LIMIT)
}

/// 活跃度倒序取最近 `limit` 张（纯函数，LIMIT 语义对齐 zcode SQL `ORDER BY
/// time_updated DESC LIMIT n`）。同毫秒并列时按收集序稳定排序
fn take_recent(cards: Vec<(i64, Session)>, limit: usize) -> Vec<Session> {
    let mut pairs = cards;
    pairs.sort_by_key(|(ms, _)| std::cmp::Reverse(*ms));
    pairs.into_iter().take(limit).map(|(_, s)| s).collect()
}

/// projcache identity 校验（5 字段；坏记录路径在此收敛）
fn projcache_identity_ok(home: &std::path::Path, header: &log::DshHeader, version: i64) -> bool {
    let path = home
        .join("storages/session_projcache/sessions")
        .join(format!("{}.json", header.id));
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    v.get("record")
        .and_then(|r| r.get("identity"))
        .map(|ident| projcache::identity_matches(ident, header, version))
        .unwrap_or(false)
}

// ===== 单元测试：宿主 cmdline 双令牌门（C1 终审：host.rs 存活判定同源口径的防漂移锁）=====
#[cfg(test)]
mod cmdline_gate_tests {
    use super::{cmdline_is_dsh_desktop_host, cmdline_is_dsh_host};
    use std::ffi::OsString;

    fn cmd(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn dual_tokens_qualify_as_host() {
        // 精确双令牌（node + dsh + web）
        assert!(cmdline_is_dsh_host(&cmd(&[
            "node",
            "/usr/local/bin/dsh",
            "web"
        ])));
        assert!(cmdline_is_dsh_host(&cmd(&["node", "dsh", "web"])));
        // 令牌顺序无关（extra 参数不影响；但脚本路径须以 /dsh、\dsh 结尾或恰为
        // "dsh"——/opt/dsh/cli.js 这类路径中段形态不算，见 substring 用例）
        assert!(cmdline_is_dsh_host(&cmd(&[
            "/usr/local/bin/node",
            "web",
            "dsh"
        ])));
        assert!(cmdline_is_dsh_host(&cmd(&[
            "node",
            "/opt/dsh",
            "web",
            "--port=4173"
        ])));
    }

    #[test]
    fn path_suffix_dsh_qualifies() {
        // 路径结尾 /dsh（POSIX）与 \dsh（Windows）均算 dsh 令牌
        assert!(cmdline_is_dsh_host(&cmd(&[
            "node",
            "/opt/dsh/bin/dsh",
            "web"
        ])));
        assert!(cmdline_is_dsh_host(&cmd(&[
            "node",
            "C:\\tools\\dsh\\bin\\dsh",
            "web"
        ])));
    }

    #[test]
    fn single_token_is_not_host() {
        // 只含其一 → 非宿主（esbuild 等子进程形态）
        assert!(!cmdline_is_dsh_host(&cmd(&["node", "/opt/dsh/bin/dsh"])));
        assert!(!cmdline_is_dsh_host(&cmd(&["node", "web"])));
        assert!(!cmdline_is_dsh_host(&cmd(&[])));
    }

    #[test]
    fn substring_tokens_do_not_qualify() {
        // "dshweb" 子串不构成 dsh 令牌、"webview" 子串不构成 web 令牌（防误匹配）
        assert!(!cmdline_is_dsh_host(&cmd(&["node", "dshweb", "web"])));
        assert!(!cmdline_is_dsh_host(&cmd(&["node", "dsh", "webview"])));
        // 子串出现在路径中间同样不算（仅路径结尾 /dsh|\dsh 认可）
        assert!(!cmdline_is_dsh_host(&cmd(&[
            "node",
            "/opt/dsh-web/cli.js",
            "web"
        ])));
    }

    #[test]
    fn desktop_host_tokens_qualify() {
        // 实测桌面端 cmdline（v0.2.0-rc.2，2026-09-27 取证，spec §3 证据 4）：
        // "DeepSeek Harness.exe" --expose-internals
        //   "…\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
        //   "…\app.asar\dsh"  "C:\Users\<u>\.dsh\profiles\desktop"  …
        assert!(cmdline_is_dsh_host(&cmd(&[
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe",
            "--expose-internals",
            r"D:\Program Files\Deepseek-Harness\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js",
            r"D:\Program Files\Deepseek-Harness\resources\app.asar\dsh",
            r"C:\Users\bunny\.dsh\profiles\desktop",
            r"D:\Program Files\Deepseek-Harness\resources\runtime\primary-runtime",
        ])));
        // POSIX 形态路径同样命中（跨平台口径，macOS 桌面端对齐探测回填前先保口径）
        assert!(cmdline_is_dsh_host(&cmd(&[
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js",
            "/Users/u/.dsh/profiles/desktop",
        ])));
    }

    #[test]
    fn desktop_tokens_do_not_match_unrelated_electron() {
        // --expose-internals 是 Electron 通用旗子，不能单独作判据；
        // 无 dsh-desktop-host / profiles\desktop 特征的进程不命中
        assert!(!cmdline_is_dsh_host(&cmd(&[
            r"C:\Apps\SomeElectron.exe",
            "--expose-internals",
            r"C:\Apps\some\lib\index.js",
        ])));
    }

    #[test]
    fn desktop_kernel_matches_only_desktop() {
        let desktop = cmd(&[
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe",
            r"...\@deepseek-ai\dsh-desktop-host\lib\index.js",
            r"C:\Users\bunny\.dsh\profiles\desktop",
        ]);
        let web = cmd(&["node", "dsh", "web"]);
        assert!(cmdline_is_dsh_desktop_host(&desktop));
        assert!(!cmdline_is_dsh_desktop_host(&web));
    }
}

// ===== 集成测试（Task 8）：会话编排流水线（home 注入直调，不触真机 ~/.dsh）=====
#[cfg(test)]
mod integration_tests {
    use super::*;
    /// 函数内私有缓存实例：namespace 进程内唯一（原子计数后缀）——registry 是
    /// 全局 HashMap、键含 namespace，同名 namespace 的"不同实例"实为同一批条目，
    /// 会互相 retain 清掉对方的注入（本测试并行失败的真根因）
    fn test_scan() -> SessionFileScan {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        SessionFileScan::new(Box::leak(format!("dsh-log-test-{n}").into_boxed_str()))
    }
    use crate::session::{ProcessForm, SessionStatus};

    /// 造一个隔离 dsh home：一个项目 + 一个会话（zstd 单帧事件）
    fn make_home(events: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions/--tmp-proj--/session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        // JSON 花括号不能进 format! 格式串——header 行用普通字面量变量拼接
        let header_line = "{\"type\":\"session\",\"version\":3,\"id\":\"session-abc\",\"cwd\":\"/tmp/proj\",\"createdAt\":1000,\"isSeeded\":false}";
        let frame =
            zstd::stream::encode_all(format!("{header_line}\n{events}").as_bytes(), 3).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();
        dir
    }

    fn fake_host() -> AgentProcess {
        AgentProcess {
            pid: 42,
            cpu_usage: 0.5,
            cwd: None,
            exe: None,
            form: ProcessForm::App,
        }
    }

    #[test]
    fn emits_card_with_status_and_preview() {
        let home = make_home(
            "{\"type\":\"turn/start\",\"seq\":4,\"data\":{}}\n\
             {\"type\":\"user/message\",\"seq\":8,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"source\":{\"kind\":\"user\"}}}\n\
             {\"type\":\"assistant/message\",\"seq\":9,\"data\":{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}}\n\
             {\"type\":\"turn/end\",\"seq\":10,\"data\":{\"reason\":{\"kind\":\"completed\"}}}\n",
        );
        let sessions = scan_sessions(home.path(), &fake_host(), &test_scan());
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "session-abc");
        assert_eq!(s.agent_type, crate::session::AgentType::Dsh);
        assert_eq!(s.status, SessionStatus::Finished);
        assert_eq!(s.last_message.as_deref(), Some("done"));
        assert_eq!(s.last_message_role.as_deref(), Some("assistant"));
        assert_eq!(s.form, ProcessForm::App);
        assert_eq!(s.pid, 42);
        // 未读态由 adapter 层未读池管线标记（W4），扫描侧恒 false——
        // 对齐 zcode/codex 数据驱动绿卡的「池行在⇒未读、已读⇒剔除」周期
        assert!(!s.unread, "扫描侧不自判未读（池管线统一标记）");
    }

    #[test]
    fn skips_subagent_and_missing_host() {
        // 子 Agent 会话不出卡
        let dir = tempfile::tempdir().unwrap();
        let sess = dir
            .path()
            .join("sessions/--tmp-proj--/3b8a0933-0000-0000-0000-000000000000");
        std::fs::create_dir_all(&sess).unwrap();
        let frame = zstd::stream::encode_all(
            b"{\"type\":\"session\",\"version\":0,\"id\":\"sub-1\",\"cwd\":\"/tmp\",\"origin\":\"subagent\",\"delegationDepth\":1}\n".as_slice(),
            3).unwrap();
        std::fs::write(sess.join("session.jsonl.zstd"), &frame).unwrap();
        let with_host = scan_sessions(dir.path(), &fake_host(), &test_scan());
        assert!(with_host.is_empty(), "子 Agent 过滤");
        // 宿主不在 → 无卡（与其他工具一致；不入扫描，无需 home）
        assert!(get_dsh_sessions(&[]).is_empty());
    }

    #[test]
    fn digest_cache_reuses_unchanged_log() {
        // L2 内容缓存（session_scan.rs 预算层）：(mtime,size) 未变 ⇒ 复用同一份
        // 解析产物 Arc——3 秒轮询下 77 个历史会话不再每轮全量解码
        let home = make_home(
            "{\"type\":\"turn/start\",\"seq\":4,\"data\":{}}\n\
             {\"type\":\"turn/end\",\"seq\":6,\"data\":{\"reason\":{\"kind\":\"completed\"}}}\n",
        );
        let dir = home.path().join("sessions/--tmp-proj--/session-abc");
        let (_, gen) = log::generation_logs(&dir).pop().unwrap();
        // 私有缓存实例：与全局 DSH_LOG_SCAN 隔离——并行测试/真实扫描的
        // retain_existing 会逐出全局命名空间的临时条目（评审 R1 flaky 根因），
        // 断言缓存语义必须用不受外扰的实例
        let scan = SessionFileScan::new("dsh-log-test-isolated");
        let first = load_digest(&scan, &gen);
        let second = load_digest(&scan, &gen);
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "文件未变应命中缓存返回同一 Arc"
        );
        // 逐出隔离回归锁：另一实例对同名文件做全量逐出，不影响本实例的命中
        // （复现 R1 flaky 机理：并行测试/真实扫描清同一全局 namespace）
        let other = SessionFileScan::new("dsh-log-test-other");
        let _ = load_digest(&other, &gen);
        other.retain_existing(&std::collections::HashSet::new());
        let third = load_digest(&scan, &gen);
        assert!(
            std::sync::Arc::ptr_eq(&second, &third),
            "其他实例的逐出不得影响本实例的缓存命中"
        );
    }

    #[test]
    fn parse_failure_entry_survives_retain_and_is_not_reparsed() {
        // 评审 R4 回归锁：解析失败缓存为 None 的条目也是有效缓存（mtime/size
        // 未变 ⇒ 下轮免重读）。若 None 条目不参与 live_logs 保活，retain_existing
        // 当轮逐出 → 窗内损坏/0 字节文件每轮重复读取解压，L2 对解析失败失效
        let now = chrono::Utc::now().timestamp_millis();
        let home = make_home_timed("session-broken", now);
        let dir = home.path().join("sessions/--tmp-proj--/session-broken");
        let (_, gen) = log::generation_logs(&dir).pop().unwrap();
        std::fs::write(&gen, b"not-a-zstd-file").unwrap(); // 覆写为非法字节，mtime 即刻 fresh
        let host = fake_host();
        let scan = test_scan();
        TEST_DIGEST_CALLS.with(|c| c.set(0));
        assert!(
            scan_sessions(home.path(), &host, &scan).is_empty(),
            "解析失败不出卡"
        );
        let second = scan_sessions(home.path(), &host, &scan);
        assert!(second.is_empty(), "两轮均不出卡");
        TEST_DIGEST_CALLS.with(|c| {
            assert_eq!(
                c.get(),
                1,
                "第二轮应命中 None 缓存条目，不得重复读取解析失败文件"
            );
        });
    }

    /// 造一个带显式事件 time 的隔离 dsh home（单会话，completed 收尾）
    fn make_home_timed(session_id: &str, event_time_ms: i64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions/--tmp-proj--").join(session_id);
        std::fs::create_dir_all(&sess).unwrap();
        let frame = zstd::stream::encode_all(
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{sid}\",\"cwd\":\"/tmp/proj\",\"createdAt\":1000,\"isSeeded\":false}}\n{{\"type\":\"turn/end\",\"seq\":2,\"time\":{t},\"data\":{{\"reason\":{{\"kind\":\"completed\"}}}}}}\n",
                sid = session_id,
                t = event_time_ms
            )
            .as_bytes(),
            3,
        )
        .unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();
        dir
    }

    #[test]
    fn sessions_outside_card_window_do_not_emit() {
        // 出现周期（对齐 zcode 24h 窗口，用户 2026-09-14 验收裁决）：超窗历史
        // 会话不主动上板。窗口口径 = 代际文件 mtime（AGENTS.md L3-4 预过滤的
        // 判定依据，事件 time 可以更老——mtime 新说明文件刚被 dsh 触碰过）
        let now = chrono::Utc::now().timestamp_millis();
        let old_home = make_home_timed("session-old", now - 25 * 3600 * 1000);
        // 事件 time 超窗 25h，但把代际文件 mtime 也拨到 25h 前（真实历史会话形态）
        let dir = old_home.path().join("sessions/--tmp-proj--/session-old");
        let (_, gen) = log::generation_logs(&dir).pop().unwrap();
        filetime::set_file_mtime(
            &gen,
            filetime::FileTime::from_unix_time((now - 25 * 3600 * 1000) / 1000, 0),
        )
        .unwrap();
        let fresh_home = make_home_timed("session-fresh", now - 3600 * 1000);
        let host = fake_host();
        assert!(
            scan_sessions(old_home.path(), &host, &test_scan()).is_empty(),
            "超窗历史会话不出卡"
        );
        assert_eq!(
            scan_sessions(fresh_home.path(), &host, &test_scan()).len(),
            1,
            "窗内会话出卡"
        );
    }

    #[test]
    fn cold_scan_skips_stale_digest_and_cache_hit_exception_works() {
        // R2（AGENTS.md L3-4）：冷缓存下超窗文件不得被打开/解压——行为证明：
        // 超窗会话的日志写成非法 zstd 字节（若被打开，build_digest 只是失败，
        // 无法区分；改用独占方式——将超窗文件替换为 FIFO 不可行，测试环境用
        // chmod 000 目录替代：文件在不可读目录下，若预过滤生效 scan 不触它，
        // 不会产生任何权限错误日志路径；直接断言=出卡结果不受影响 + 超窗无卡）
        let now = chrono::Utc::now().timestamp_millis();
        let home = make_home_timed("session-fresh", now - 3600 * 1000);
        // 再造一个超窗会话，其目录置为不可读（打开必失败）
        let stale_dir = home.path().join("sessions/--tmp-proj--/session-stale");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(
            stale_dir.join("session.v3.jsonl.zstd"),
            zstd::stream::encode_all(
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"session-stale\",\"cwd\":\"/tmp/proj\",\"createdAt\":1,\"isSeeded\":false}}\n{{\"type\":\"turn/end\",\"seq\":2,\"time\":{}}}\n",
                    now - 48 * 3600 * 1000
                )
                .as_bytes(),
                3,
            )
            .unwrap(),
        )
        .unwrap();
        // 预过滤依据 mtime——把代际文件 mtime 拨回 48h 前
        let stale_gen = stale_dir.join("session.v3.jsonl.zstd");
        let old = filetime::FileTime::from_unix_time((now - 48 * 3600 * 1000) / 1000, 0);
        filetime::set_file_mtime(&stale_gen, old).unwrap();

        let host = fake_host();
        // 两次 scan 共用同一私有实例（缓存例外跨 scan 验证的前提）
        let scan = test_scan();
        TEST_DIGEST_CALLS.with(|c| c.set(0));
        let cards = scan_sessions(home.path(), &host, &scan);
        assert_eq!(cards.len(), 1, "仅窗内会话出卡");
        assert_eq!(cards[0].id, "session-fresh");
        // 机器证明（评审 Minor）：冷扫描只解析窗内 fresh 一次——超窗 stale 未被
        // 打开/解压（本线程计数=1 而非 2）
        TEST_DIGEST_CALLS.with(|c| {
            assert_eq!(
                c.get(),
                1,
                "冷扫描应只解析 fresh 一次，超窗文件不进 build_digest"
            );
        });
        // 缓存例外回归锁：超窗但曾解析过（缓存命中）→ 仍参与出卡。
        // 第一次 scan 预过滤跳过了 stale（缓存里没有），手动向同一实例注入
        // 一条（键=48h 前 mtime，与 peek 比对键一致），再扫应例外出卡
        scan.parse(&stale_gen, build_digest);
        TEST_DIGEST_CALLS.with(|c| c.set(0));
        let cards2 = scan_sessions(home.path(), &host, &scan);
        let ids: Vec<String> = cards2.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            cards2.len(),
            2,
            "缓存命中的超窗会话应例外出卡，实得 {:?}",
            ids
        );
        assert!(ids.iter().any(|i| i == "session-stale"));
        // 例外路径同样零新增解析：stale 走 peek 缓存命中，fresh 未变化
        TEST_DIGEST_CALLS.with(|c| {
            assert_eq!(
                c.get(),
                0,
                "第二次扫描应全缓存命中（含超窗例外），零 build_digest 调用"
            );
        });
    }

    #[test]
    fn take_recent_keeps_latest_and_truncates() {
        // 活跃度倒序 + LIMIT 截断（纯函数，对齐 zcode ORDER BY time_updated DESC LIMIT）
        let mk = |id: &str| Session {
            id: id.into(),
            agent_type: AgentType::Dsh,
            project_name: "p".into(),
            project_path: String::new(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread: false,
            unread_turned_green_at_ms: None,
        };
        let cards = vec![(100, mk("old")), (300, mk("new")), (200, mk("mid"))];
        let out = take_recent(cards, 2);
        assert_eq!(out.len(), 2, "LIMIT 截断");
        assert_eq!(out[0].id, "new", "活跃度最高者在前");
        assert_eq!(out[1].id, "mid");
    }

    #[test]
    fn digest_invalidates_when_log_appends() {
        // 追加一帧（size 变化）→ 缓存必须失效：completed → error，Finished → Waiting
        let home = make_home(
            "{\"type\":\"turn/start\",\"seq\":4,\"data\":{}}\n\
             {\"type\":\"turn/end\",\"seq\":6,\"data\":{\"reason\":{\"kind\":\"completed\"}}}\n",
        );
        let host = fake_host();
        assert_eq!(
            scan_sessions(home.path(), &host, &test_scan())[0].status,
            SessionStatus::Finished
        );
        let dir = home.path().join("sessions/--tmp-proj--/session-abc");
        let (_, gen) = log::generation_logs(&dir).pop().unwrap();
        let mut bytes = std::fs::read(&gen).unwrap();
        bytes.extend_from_slice(
            &zstd::stream::encode_all(
                b"{\"type\":\"turn/end\",\"seq\":9,\"data\":{\"reason\":{\"kind\":\"error\"}}}\n"
                    .as_slice(),
                3,
            )
            .unwrap(),
        );
        std::fs::write(&gen, &bytes).unwrap();
        let cards = scan_sessions(home.path(), &host, &test_scan());
        assert_eq!(
            cards[0].status,
            SessionStatus::Waiting,
            "追加 error 帧后应重扫"
        );
    }
}
