//! H8 codex APP 托管会话无头通道（Task 9）：**只做 codex 特有的事**——thread UUID 映射
//! （rollout 文件名唯一来源）、APP 在场 × 计划分派（queue 主 / exec resume 兜底）、命令形态
//! （`-C` 全局 flag 前置）、单写者锁改道、消费确认自建（rollout 追加侦测 + 队列副本交叉验证）、
//! 以及把 Task 6 底座串成单回合编排（并发名额 / 看门狗 / 取消 / kill 树 / 回执归一全在
//! [`super::runner`] / [`super::receipt`]，本模块**不重复实现**）。
//!
//! # 命令形态（spec §H8 + 本机 CLI 0.160.0 `--help` 实测）
//! - APP 开（thread 被打开/索引）：`codex queue --thread <UUID> --message "<正文> [mobile 花名]"`
//!   （`Usage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>`）；回执 =
//!   `Queued message <msg-id>` + exit 0 —— **只证明入队，不证明投递**（消费确认见下）。
//! - APP 关：`codex -C <项目> exec resume <UUID> "<正文> [mobile 花名]" --skip-git-repo-check`。
//!   **`-C`/`--cd` 是顶层全局 flag，必须置于子命令之前**（`codex exec resume --help` 的选项表
//!   里**没有** `-C` ⇒ 置于 `exec` 之后会 exit 2，Mac 实测同款）。本模块把 argv 构造成
//!   「`argv[0]` = 程序名 + `argv[1]` = `-C`」并**由单一构造器保证**（[`exec_resume_argv`]），
//!   测试钉死位置（`argv[1] == "-C"` 且 `-C` 的下标恒小于 `exec`）。
//!
//! # thread id = rollout 文件名里的 UUIDv7（**唯一**来源）
//! `~/.codex/sessions/<Y/M/D>/rollout-<ts>-<uuid>.jsonl`；`session_index.jsonl` 实测滞后
//! 13 天不作 id 源；**只用 UUID，禁用会话名**（Mac 普查 202 thread 中「hi」重名 ×5，
//! 且 `queue --thread` 自报收「Session UUID or exact session name」⇒ 名字面天然歧义）。
//! 读链路（[`crate::monitor::codex_thread_parser`] 的 `threads.rollout_path`）已给出 rollout
//! 路径，本模块只取 basename 解析（[`thread_id_of`]），**不另写扫描器**。
//!
//! # 消费确认自建（回执诚实性的核心）
//! `queue` 的 exit 0 只是「已入队」：本模块在**回合前**记目标 rollout 的字节长度基线，
//! 入队后**有界轮询** `[基线..EOF]` 的新增段（命中消息文本 = 已消费）；超时仍未命中时
//! 再查 `~/.codex/queue_1.sqlite` **副本**的 `queued_items`（活库不直查纪律）交叉验证，
//! 且**按入队回执 id 认领本条目**（只凭 thread 有行会变成「本条未消费」的过度断言）：
//! - 命中本条目 id ⇒ 「**已入队未消费**」+ 观测事实（等待 N s 无追加）+ **可能原因**
//!   （thread 未在 APP 打开；Mac 实测可滞留 19.2 分钟，Windows 抽验消费 ~55s——58s 预算
//!   只是刚过该量级，故**不把原因写成已证事实**）+ 建议（在 APP 打开该会话 / 改走 exec）；
//! - thread 有滞留行但**未命中本条目 id** ⇒ 如实「未能确认含本条」；
//! - 该 thread 已无滞留条目 ⇒ 「队列已取走但未观测到落盘」（不做因果猜测）；
//! - 副本不可读 ⇒ 「**无法确认**是否滞留」（绝不编结论）。
//!
//! # 版本门控双坐标（**已探的探、不可得的登记**）
//! CLI 坐标走 Task 6 门控机制（`queue --help` 子命令在场，结果按 exe mtime 缓存）——本机实测
//! `codex-cli 0.160.0`。**APP 内嵌 codex 版本不可得**（进程表拿不到 bundle id，要读
//! `Info.plist`/APP 装包；本机未装且未跑 ChatGPT.app ⇒ 连候选都没有），故**登记为代码内
//! 限制**而**不编造版本号**：CLI 探针通过 ≠ APP 端消费语义一致。该后果由**运行期证据**兜底
//! ——消费确认（rollout 追加 / 队列副本）看到的是 APP 真实行为，比版本号猜测更硬。
//!
//! # Windows spawn 形态（npm 垫片）
//! PATH 上的 `codex` 在 Windows 常是 npm 垫片（`codex.cmd`；本机实测）。CreateProcess 不认
//! 批处理，裸名直 spawn 必失败 ⇒ [`spawn_shape`] 对其走 `cmd /c <垫片>`（与仓库既有
//! `inject::approve` / `monitor::hooks` 的灰1 双路同口径）。
//!
//! **登记的完整风险（Task 9 复审，未实机验证）**：`cmd` 会**重解析整条命令行**——不只
//! `%VAR%`（未定义变量原样保留）：`"` 的配对规则、`&` `|` `^` `<` `>` 的重定向/串联语义都
//! 按 cmd 的口径解析，而 Rust 传参用的是 CRT 的 `\"` 转义风格（与 cmd 不同）⇒ **含这些
//! 字符的用户正文在垫片路径上可能被截断/改写/串联执行**。边界：仅当解析出的 CLI 是
//! `.cmd`/`.bat` 时走此路（[`spawn_shape`]）。
//! 缓解 = [`codex_in_path`] 的 `.exe` 优先——但该优先序**只在同一目录内生效**（PATH 顺序
//! 仍先于扩展名优先），故本机这种「PATH 上只有 npm 垫片」的安装**正在走垫片路径**。
//! 结论：本形态**登记为已知限制**，关闭路径 = 实机探测定案（Task 15 实机批次：用含
//! `"` `&` `%VAR%` 的载荷真跑一次，记录 cmd 的实际行为，再决定是否需要「经 `codex.js`
//! 反解真实 exe」或「拒绝含危险字符的正文」）——本任务不做未经证据的猜测式加固。
//!
//! # 测试纪律（宪法级）
//! 单测**绝不** spawn 真 codex（真实账号配额 + 真实 `~/.codex` 读写）：CLI 发现、rollout 定位、
//! 队列副本读取、APP 在场扫描的生产出口全部 `cfg(not(test))`（见 [`production_exe`] /
//! [`production_rollout_path`] / [`production_queue_holds`] / [`production_presence`]），
//! 端点用例据此**如实地**断言「版本门控不可达」而不会真的起回合；纯核用注入表/夹具驱动，
//! 编排经注入的 `make_runner` / 执行缝 / 消费探针驱动 Task 6 的脚本缝。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::gate::{ProbeCache, ProbeKey, ProbeSpec, ProbeVerdict};
use super::receipt::{Receipt, ReceiptStatus, Stage};
use super::runner::{GlobalSem, RunnerCfg};
// **只依赖底座**（Task 9 复审上提）：共享回合件定义在 [`super::turn`]，**不再从 zcode.rs 取**
// ——WB（Task 11）/ H11（Task 13）接入同规，禁止让通道之间互相依赖
use super::turn::{head_of, RunSeam};
pub use super::turn::{registry, TurnSlot};
// `TurnObs` 只在测试面用（脚本缝造结局；生产码只用推断字段）——测试构建才引入
#[cfg(test)]
use super::turn::TurnObs;
use crate::inject::normalize;
use crate::session::ProcessForm;

/// codex CLI 程序名词表项（`Plan::argv[0]` 的取值；真实 spawn 目标见 [`spawn_shape`]）
pub const CODEX_PROGRAM: &str = "codex";
/// 入队回执特征串（spec H8：`Queued message <msg-id>` + exit 0 = 只证入队）
pub const QUEUE_RECEIPT_MARKER: &str = "Queued message";
/// 单写者锁特征串（spec H8 / issue #47193：`-32600 already has an active writer`）
pub const WRITER_LOCK_MARKER: &str = "already has an active writer";
/// 单写者锁的 JSON-RPC 码（单独出现不足以定性——须与 writer 语义同现）
pub const WRITER_LOCK_RPC_CODE: &str = "-32600";
/// `exec resume` 的外置仓库开关（spec H8 命令形态逐字）
pub const SKIP_GIT_REPO_CHECK: &str = "--skip-git-repo-check";
/// 消费确认的有界轮询：次数 × 间隔（60s 预算；Mac 实测消费 ~20s / Win 抽验 ~55s）
pub const CONSUME_ATTEMPTS: usize = 30;
pub const CONSUME_INTERVAL_MS: u64 = 2_000;
/// 单次追加段读取上限（4MiB）：够覆盖「刚被消费的用户消息」，超出即**不判否**（见读缝）
pub const MAX_APPEND_READ: u64 = 4 * 1024 * 1024;

/// APP 在场性（H8 分派的第一判据）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPresence {
    /// APP 在场（thread 被打开/索引）⇒ `queue` 主路径
    Open,
    /// APP 不在场 ⇒ `exec resume` 兜底
    Closed,
}

/// 通道实际走向（回执封套 `channel` 与审计 channel 列同源）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Queue,
    ExecResume,
}

/// 投放计划（**纯函数产物**：argv 全量、thread id、最终载荷）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Queue {
        argv: Vec<String>,
        thread: String,
        message: String,
    },
    ExecResume {
        argv: Vec<String>,
        thread: String,
        message: String,
    },
}

impl Plan {
    /// 程序名词表项（`argv[0]`；真实 spawn 目标由 [`spawn_shape`] 决定）
    pub fn program(&self) -> &str {
        &self.argv()[0]
    }

    /// **投递载荷段**（不含 `argv[0]` 程序名）——`-C` 位置在此段内被钉死
    pub fn args(&self) -> &[String] {
        &self.argv()[1..]
    }

    /// 全量 argv（`argv[0]` = 程序名；`argv[1]` 起为载荷）
    pub fn argv(&self) -> &[String] {
        match self {
            Plan::Queue { argv, .. } | Plan::ExecResume { argv, .. } => argv,
        }
    }

    pub fn thread(&self) -> &str {
        match self {
            Plan::Queue { thread, .. } | Plan::ExecResume { thread, .. } => thread,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Plan::Queue { message, .. } | Plan::ExecResume { message, .. } => message,
        }
    }

    /// 走向（回执封套 channel / 审计 channel 列的来源）
    pub fn kind(&self) -> PlanKind {
        match self {
            Plan::Queue { .. } => PlanKind::Queue,
            Plan::ExecResume { .. } => PlanKind::ExecResume,
        }
    }
}

impl PlanKind {
    /// 路由词表单点（`Channel::Headless` 的 wire 名与审计 channel 列同源）
    pub fn headless_kind(self) -> crate::inject::routing::HeadlessKind {
        match self {
            PlanKind::Queue => crate::inject::routing::HeadlessKind::CodexQueue,
            PlanKind::ExecResume => crate::inject::routing::HeadlessKind::CodexExec,
        }
    }
}

/// APP 在场观测（探测定案：会话宿主形态 + 宿主存活 + APP 进程在场）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresenceObs {
    pub form: ProcessForm,
    pub host_alive: bool,
    pub app_running: bool,
}

/// MAM 会话 → codex thread（id + 目标 rollout 路径）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRef {
    pub id: String,
    pub rollout_path: Option<String>,
}

// ============================================================
// thread id 映射（rollout 文件名 UUID 唯一来源）
// ============================================================

/// ASCII 大小写不敏感子串定位（返回**字节**下标；命中处必为 ASCII ⇒ 切片边界安全）
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// 取串中首个 UUID 形态（8-4-4-4-12 十六进制段）。非 ASCII 字节天然不匹配 ⇒
/// 不会切在多字节字符中间
fn uuid_at(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let dash = |k: usize| matches!(k, 8 | 13 | 18 | 23);
    if b.len() < 36 {
        return None;
    }
    for i in 0..=b.len() - 36 {
        let w = &b[i..i + 36];
        let ok = w.iter().enumerate().all(|(k, c)| {
            if dash(k) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
        if ok {
            return Some(s[i..i + 36].to_string());
        }
    }
    None
}

/// 严格 UUID 形态（会话号回退用：只认整串，不做「包含」判定）
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && uuid_at(s).as_deref() == Some(s)
}

/// rollout 文件名里的 thread UUID（spec H8：**唯一** id 来源）。
/// 入参可为全路径（只看最后一段）；非 rollout 命名 → `None`（**绝不用会话名兜底**）
pub fn thread_id_of(name_or_path: &str) -> Option<String> {
    let name = name_or_path.rsplit(['/', '\\']).next().unwrap_or("");
    if !crate::monitor::codex_parser::is_rollout_file(Path::new(name)) {
        return None;
    }
    uuid_at(name)
}

/// MAM 会话 → thread 引用。**优先级**：rollout 文件名里的 UUID（spec 权威）→
/// 仅有 UUID 形态的会话号可回退（无 rollout 路径时）→ 否则 `None`（调用方如实拒绝）
pub fn resolve_thread(rollout_path: Option<&str>, session_id: &str) -> Option<ThreadRef> {
    let from_file = rollout_path.and_then(thread_id_of);
    let sid = session_id.trim();
    if let Some(f) = from_file.as_deref() {
        if !sid.is_empty() && f != sid {
            // 漂移不静默：文件名是 spec 权威，但两条链不一致值得留痕排查
            log::warn!(
                "codex: rollout 文件名 UUID({f}) 与 state 库会话号({sid}) 不一致——按文件名投递"
            );
        }
    }
    let id = from_file.or_else(|| is_uuid(sid).then(|| sid.to_string()))?;
    Some(ThreadRef {
        id,
        rollout_path: rollout_path.map(str::to_string),
    })
}

// ============================================================
// APP 在场 × 计划分派（纯核）
// ============================================================

/// APP 在场判定（探测定案）：**APP 形态** 且（会话宿主进程仍在 **或** APP 主进程在场）。
/// CLI 形态恒判「不在场」（H8 只服务 APP 托管会话；CLI TUI 保持 W3 终端注入）
pub fn presence_of(obs: &PresenceObs) -> AppPresence {
    if obs.form == ProcessForm::App && (obs.host_alive || obs.app_running) {
        AppPresence::Open
    } else {
        AppPresence::Closed
    }
}

/// Windows verbatim 本地盘符前缀（`\\?\C:\…`）→ 普通路径。UNC（`\\?\UNC\…`）与其他
/// 形态**原样保留**（只剥有实测依据的那一种，不做猜测式改写）。state 库的 `threads.cwd`
/// 列实测就是 verbatim 形态，直接塞进 `-C` 会让 CLI 拿到一个没人要的扩展长度路径
pub fn normalize_project_path(p: &str) -> String {
    const VERBATIM: &str = r"\\?\";
    if let Some(rest) = p.strip_prefix(VERBATIM) {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return rest.to_string();
        }
    }
    p.to_string()
}

/// 分派（**纯函数**）：APP 开 → queue；关 → exec resume 兜底。`text` = **最终载荷**
/// （调用方经 W4 单点 [`normalize::compose_injection`] 组装；本函数不二次签名）
pub fn dispatch(presence: AppPresence, thread: &str, project: &str, text: &str) -> Plan {
    match presence {
        AppPresence::Open => Plan::Queue {
            argv: queue_argv(thread, text),
            thread: thread.to_string(),
            message: text.to_string(),
        },
        AppPresence::Closed => Plan::ExecResume {
            argv: exec_resume_argv(project, thread, text),
            thread: thread.to_string(),
            message: text.to_string(),
        },
    }
}

/// `codex queue --thread <UUID> --message <text>`（本机 `codex queue --help` usage 逐字：
/// `Usage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>`）
fn queue_argv(thread: &str, message: &str) -> Vec<String> {
    vec![
        CODEX_PROGRAM.to_string(),
        "queue".to_string(),
        "--thread".to_string(),
        thread.to_string(),
        "--message".to_string(),
        message.to_string(),
    ]
}

/// `codex -C <dir> exec resume <UUID> <text> --skip-git-repo-check`
///
/// **`-C` 全局 flag 前置（唯一构造点）**：本机 `codex exec resume --help` 的选项表里
/// **没有** `-C`（只有 `-c/--config`）⇒ 它只能是顶层全局 flag，置于 `exec` 之后即 exit 2。
/// argv 的 `-C` 位置由本函数**按构造保证**，测试再钉一次下标顺序（[`super::tests`]）
fn exec_resume_argv(project: &str, thread: &str, message: &str) -> Vec<String> {
    vec![
        CODEX_PROGRAM.to_string(),
        "-C".to_string(),
        normalize_project_path(project),
        "exec".to_string(),
        "resume".to_string(),
        thread.to_string(),
        message.to_string(),
        SKIP_GIT_REPO_CHECK.to_string(),
    ]
}

// ============================================================
// 回执解析 / 退出分类（纯核）
// ============================================================

/// 入队回执解析（spec H8：`Queued message <msg-id>`；stdout/stderr **两路都找**——不猜
/// 走哪一路）。id 取标记后的**首个 token**；只有标记没有内容 ⇒ `None`（不编 id）
pub fn queued_message_id(stdout: &[String], stderr: &str) -> Option<String> {
    stdout
        .iter()
        .map(String::as_str)
        .chain(stderr.lines())
        .find_map(|line| {
            let l = line.trim();
            let idx = find_ascii_ci(l, QUEUE_RECEIPT_MARKER)?;
            let tail = l[idx + QUEUE_RECEIPT_MARKER.len()..].trim();
            tail.split_whitespace().next().map(str::to_string)
        })
}

/// 单写者锁证据（spec H8 / issue #47193）。判据：特征串命中，或 `-32600` 与 writer 语义
/// **同现**——裸错误码不定性（别的 JSON-RPC 失败不得被当成锁）
pub fn writer_lock_evidence(stdout_head: &str, stderr_head: &str) -> Option<String> {
    for hay in [stdout_head, stderr_head] {
        let lower = hay.to_ascii_lowercase();
        if lower.contains(WRITER_LOCK_MARKER) {
            return Some(WRITER_LOCK_MARKER.to_string());
        }
        if lower.contains(WRITER_LOCK_RPC_CODE) && lower.contains("writer") {
            return Some(format!("{WRITER_LOCK_RPC_CODE} + writer（单写者锁）"));
        }
    }
    None
}

/// `exec resume` 的末条回复 = stdout **最后一条非空行**（spec H8 定案）
pub fn last_stdout_line(stdout: &[String]) -> Option<String> {
    stdout
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

// ============================================================
// 消费确认（纯核 + 读缝）
// ============================================================

/// 消费针（rollout 是 JSONL：引号/反斜杠会被转义 ⇒ 原文 + JSON 转义变体；
/// 无需转义时不重复）
pub fn consume_needles(text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let mut out = vec![t.to_string()];
    let escaped = serde_json::to_string(t)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();
    if !escaped.is_empty() && escaped != t {
        out.push(escaped);
    }
    out
}

/// 目标 rollout 的当前字节长度（消费确认基线）
pub fn baseline_len(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// 追加命中（生产上限 [`MAX_APPEND_READ`]）
pub fn rollout_appended_hit(path: &str, baseline: u64, needles: &[String]) -> Option<bool> {
    rollout_appended_hit_with_cap(path, baseline, needles, MAX_APPEND_READ)
}

/// 追加段命中判定（读上限可注入）：
/// - 基线之后**没有新增** → `Some(false)`（历史同文本行不算消费——「hi」在 Mac 普查里重名 ×5）；
/// - 新增段**头 `cap` 字节**命中任一针 → `Some(true)`；
/// - 新增段超过 `cap` 且头段未命中 → **`None`（无法确认，不判否）**；读失败同样 `None`
pub fn rollout_appended_hit_with_cap(
    path: &str,
    baseline: u64,
    needles: &[String],
    cap: u64,
) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let len = std::fs::metadata(path).ok()?.len();
    if len <= baseline {
        return Some(false);
    }
    let region = len - baseline;
    let read_len = region.min(cap.max(1));
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(baseline)).ok()?;
    let mut buf = vec![0u8; read_len as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    let text = String::from_utf8_lossy(&buf[..filled]);
    if needles
        .iter()
        .any(|n| !n.is_empty() && text.contains(n.as_str()))
    {
        return Some(true);
    }
    if region > read_len {
        return None;
    }
    Some(false)
}

/// 单个队列库副本查询（`queued_items`）。
/// 表结构取自本机活库实证（`id/thread_id/payload_json/queue_order/created_at_ms/updated_at_ms`）；
/// **`id` 即入队回执里的 message-id**（同一条目），故按 `(thread_id, id)` 双键认领本条目：
/// 命中 ⇒ [`QueueEvidence::Ours`]；只有同 thread 别的行 ⇒ [`QueueEvidence::ThreadStalled`]。
/// 文件不在场 → `None`（**不凭空造库**）
pub fn queue_evidence_in_db(db_path: &Path, thread: &str, msg_id: &str) -> Option<QueueEvidence> {
    if !db_path.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open(db_path).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(500));
    let (rows, ours): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN id = ?2 THEN 1 ELSE 0 END), 0) \
             FROM queued_items WHERE thread_id = ?1",
            rusqlite::params![thread, msg_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    Some(if ours > 0 {
        QueueEvidence::Ours
    } else if rows > 0 {
        QueueEvidence::ThreadStalled {
            rows: rows as usize,
        }
    } else {
        QueueEvidence::ThreadClear
    })
}

/// **活库不直查纪律**（同 H12/WB 口径）：把队列库 **复制**到临时文件再查——APP 持 WAL
/// 写锁，直查会阻塞甚至被拒。`-wal`/`-shm` 一并复制（WAL 里才有最近的提交）。副本读完
/// 逐个文件删除（**不做递归删除**）
pub fn queue_evidence_via_replica(
    db_path: &Path,
    thread: &str,
    msg_id: &str,
) -> Option<QueueEvidence> {
    if !db_path.is_file() {
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 临时**文件名**级复制（不建目录 ⇒ 无需递归删除）；主库名带 `.sqlite`，
    // `-wal`/`-shm` 按 SQLite 约定贴在同名后缀上
    let base = std::env::temp_dir().join(format!(
        "mam-codex-queue-{}-{stamp}.sqlite",
        std::process::id()
    ));
    let mut copies: Vec<std::path::PathBuf> = Vec::new();
    let src = db_path.to_string_lossy().to_string();
    let dst = base.to_string_lossy().to_string();
    let mut ok = true;
    for suffix in ["", "-wal", "-shm"] {
        let from = format!("{src}{suffix}");
        if !Path::new(&from).is_file() {
            continue;
        }
        let to = std::path::PathBuf::from(format!("{dst}{suffix}"));
        if std::fs::copy(&from, &to).is_ok() {
            copies.push(to);
        } else if suffix.is_empty() {
            ok = false;
        }
    }
    let verdict = if ok {
        queue_evidence_in_db(&base, thread, msg_id)
    } else {
        None
    };
    for p in copies {
        let _ = std::fs::remove_file(p);
    }
    verdict
}

/// state 库只读取 rollout 路径（生产核心，夹具可测；列名 `threads.rollout_path` = 本机实证）
pub fn rollout_path_in_db(db_path: &Path, thread: &str) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(db_path)?;
    let p: String = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [thread],
            |r| r.get(0),
        )
        .ok()?;
    let p = p.trim().to_string();
    (!p.is_empty()).then_some(p)
}

// ============================================================
// CLI 发现 / spawn 形态（Windows npm 垫片）
// ============================================================

/// PATH 分段（纯核）：分隔符按平台取（Windows `;` / 其余 `:`），空段与引号壳滤除
pub fn path_dirs(path_env: &str, os: &str) -> Vec<String> {
    let sep = if os == "windows" { ';' } else { ':' };
    path_env
        .split(sep)
        .map(|d| d.trim().trim_matches('"'))
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect()
}

/// PATH 扫描（纯核）：**按 PATH 顺序**在每段目录内试 `.exe` → `.cmd` → `.bat`
/// （与 Windows 自身的解析顺序一致——PATH 顺序决定用哪个安装，目录内则真实可执行体优先：
/// 垫片要经 `cmd` 转一手，而 cmd 会重解析命令行）；POSIX 找裸名文件。
/// 找不到 → `None`（调用方如实拒绝，**绝不 spawn 不存在的程序**）
pub fn codex_in_path(path_env: &str, os: &str) -> Option<String> {
    let exts: &[&str] = if os == "windows" {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in path_dirs(path_env, os) {
        for ext in exts {
            let cand = Path::new(&dir).join(format!("codex{ext}"));
            if cand.is_file() {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// 是否 Windows 批处理垫片（`.cmd`/`.bat`；CreateProcess 不认批处理）
pub fn shim_needs_cmd(path: &str) -> bool {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| e == "cmd" || e == "bat")
}

/// spawn 形态（Windows 垫片经 cmd 转一手；其余直 spawn）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnShape {
    pub program: String,
    pub prefix: Vec<String>,
}

/// spawn 形态构造（唯一构造点）。**登记限制**：`cmd /c` 会重解析命令行——正文中的
/// `%VAR%` 会被展开（未定义变量原样保留）；这是 Windows npm 垫片的固有代价，
/// 故 [`codex_in_path`] 让 `.exe` 优先
pub fn spawn_shape(exe: &str, os: &str) -> SpawnShape {
    if os == "windows" && shim_needs_cmd(exe) {
        SpawnShape {
            program: "cmd".to_string(),
            prefix: vec!["/c".to_string(), exe.to_string()],
        }
    } else {
        SpawnShape {
            program: exe.to_string(),
            prefix: Vec::new(),
        }
    }
}

/// 回合配置工厂（端点侧闭包：经 [`super::runner_from_conn`] 取设置超时/全局名额，再按
/// [`SpawnShape`] 与计划 argv 组装 `RunnerCfg`；测试 = 记录型桩 + Task 6 脚本缝）
pub type MakeRunner<'a> = dyn Fn(&Plan) -> RunnerCfg + Send + Sync + 'a;

/// rollout 基线读缝（目标路径 → 当前字节长度）
pub type BaselineSeam = dyn Fn(&str) -> Option<u64> + Send + Sync;
/// rollout 追加命中读缝（路径 + 基线 + 针 → 三态结论）
pub type AppendedHitSeam = dyn Fn(&str, u64, &[String]) -> Option<bool> + Send + Sync;
/// 队列滞留证据（**按入队回执 id 认领本条目**——复审 Minor 2：只凭 thread 有行会把
/// 「本 thread 队列里恰好有别的条目」误读成「本条未消费」）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEvidence {
    /// 入队回执 id 命中 `queued_items.id` ⇒ **本条目**仍在队列
    Ours,
    /// 该 thread 仍有滞留条目，但**未命中本条目 id**（可能是别人的 / 更早的条目）
    ThreadStalled { rows: usize },
    /// 该 thread 在队列里已无滞留条目
    ThreadClear,
}

/// 队列副本读缝（thread + 入队回执 id → 三态证据；`None` = 副本不可读）
pub type QueueHoldsSeam = dyn Fn(&str, &str) -> Option<QueueEvidence> + Send + Sync;
/// 等待缝（生产 = `tokio::time::sleep`；测试 = 记录桩，零真实等待）
pub type WaitSeam = dyn Fn(Duration) -> super::turn::BoxFuture<()> + Send + Sync;

/// rollout 读缝（基线 + 追加命中；测试脚本桩，**零真实 ~/.codex**）
pub struct RolloutProbe {
    pub baseline: Box<BaselineSeam>,
    pub appended_hit: Box<AppendedHitSeam>,
}

impl RolloutProbe {
    /// 生产读缝（只读目标 rollout 的 `[基线..EOF]` 新段）
    pub fn production() -> Self {
        Self {
            baseline: Box::new(baseline_len),
            appended_hit: Box::new(rollout_appended_hit),
        }
    }
}

/// 编排依赖（消费确认轮询可注入：测试零真实等待零真实 rollout 零真实队列库）
pub struct TurnDeps {
    pub consume_attempts: usize,
    pub consume_interval: Duration,
    pub wait: Box<WaitSeam>,
    pub rollout: RolloutProbe,
    pub queue_holds: Box<QueueHoldsSeam>,
}

impl TurnDeps {
    /// 生产依赖（追加读走真 rollout 只读；队列交叉验证走 `queue_*.sqlite` 副本；等待走
    /// tokio 定时器）。**测试构建**：`production_queue_holds` 恒 `None`（不读真实 ~/.codex），
    /// 编排用例一律经注入缝驱动（见 tests 的 `deps_with`）
    pub fn production() -> Self {
        Self {
            consume_attempts: CONSUME_ATTEMPTS,
            consume_interval: Duration::from_millis(CONSUME_INTERVAL_MS),
            wait: Box::new(|d: Duration| Box::pin(tokio::time::sleep(d))),
            rollout: RolloutProbe::production(),
            queue_holds: Box::new(production_queue_holds),
        }
    }

    /// 消费等待预算（秒）——回执里如实报「等待了多少秒」，不写死数字
    pub fn consume_budget_secs(&self) -> u64 {
        (self.consume_attempts.max(1) as u128 * self.consume_interval.as_millis() / 1000) as u64
    }
}

/// 回合入参（会话号 = 串行锁/取消键；项目；目标 rollout）
pub struct TurnArgs<'a> {
    pub sid: &'a str,
    pub project: &'a str,
    pub rollout: Option<&'a str>,
}

/// 回执来源（可观测面：测试与审计据此分辨证据链）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptSource {
    /// **只证入队**（拿到了 `Queued message <id>`，但消费确认给不出「已消费」）——
    /// rollout 不可定位 / 不可读、或 60s 内未观测到追加且队列副本仍滞留
    Enqueue,
    /// rollout 追加命中（消费真源）
    RolloutAppend,
    /// `exec resume` stdout 尾行（末条回复）
    ExecStdout,
    /// 确认链断在回执本身（无有效入队回执 / exec 无输出）——如实不确认
    Unconfirmed,
    /// 非 0 退出 / 超时 / 取消：确认不参与判定
    NotApplicable,
}

/// 回合结局
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub receipt: Receipt,
    pub plan_used: PlanKind,
    pub diverted: bool,
    pub receipt_source: ReceiptSource,
}

/// 一次尝试的观测（回执 + 原始流供退出分类与入队回执解析消费）
struct Attempt {
    receipt: Receipt,
    stdout: Vec<String>,
    stderr: String,
}

/// 单次执行（**每回合 spawn 子进程**，裁决 8）：建 runner（端点侧经
/// [`super::runner_from_conn`]，超时/并发按设置）→ 逐尝试更新取消靶子 → 执行。
/// 取消靶子走 Task 8 建立的**进程级登记表**（按会话号索引、与通道无关）——取消端点只认
/// 这一份，codex 复用同一表，否则移动端取消钮对 codex 回合是哑的
async fn attempt(sid: &str, plan: &Plan, make_runner: &MakeRunner<'_>, run: &RunSeam) -> Attempt {
    let cfg = make_runner(plan);
    let handle = cfg.cancel_handle();
    registry().arm(sid, Arc::new(move || handle.cancel()));
    let obs = run(cfg).await;
    Attempt {
        receipt: obs.receipt,
        stdout: obs.stdout,
        stderr: obs.stderr,
    }
}

/// 回执原因前缀（改道如实注明用）
fn prefixed(note: Option<&str>, reason: &str) -> String {
    match note {
        Some(n) => format!("{n}{reason}"),
        None => reason.to_string(),
    }
}

/// 单回合编排（**唯一**消费 Task 6 底座的地方）。分派：
/// - [`Plan::Queue`] → 入队（短命进程；看门狗只覆盖**投递段**——turn 执行归 codex APP 自身）
///   + **消费确认自建**（[`consume_and_report`]）；
/// - [`Plan::ExecResume`] → 跑 `exec resume`：stdout 尾行 = 末条回复；**单写者锁**（APP 在场
///   时 exec 被拒，issue #47193）⇒ 自动改道 [`Plan::Queue`] 并如实注明（[`divert_note`]）。
///
/// 诚实纪律：入队回执（exit 0 + `Queued message <id>`）**绝不当成已送达**；确认不了就报
/// 「无法确认」；改道必须写在回执里。回执耗时 = 整回合墙钟（含消费等待）。
pub async fn run_turn(
    args: &TurnArgs<'_>,
    plan: &Plan,
    make_runner: &MakeRunner<'_>,
    deps: &TurnDeps,
    run: &RunSeam,
) -> TurnOutcome {
    let started = Instant::now();
    match plan {
        Plan::Queue { .. } => {
            run_queue_turn(args, plan, make_runner, deps, run, started, None).await
        }
        Plan::ExecResume { .. } => {
            let att = attempt(args.sid, plan, make_runner, run).await;
            let stderr_lines: Vec<String> = att.stderr.lines().map(str::to_string).collect();
            let evidence = writer_lock_evidence(&head_of(&att.stdout), &head_of(&stderr_lines));
            // 改道判据（Task 9 复审 Important）：**必须先是一次失败的尝试**
            // （`status == Failed`）。exit 0 的 exec 即使 stdout 里出现特征串（对话正文里
            // 引用这句话、或 CLI 把锁告警降级为警告后仍跑完），消息**已经送达**——再改道
            // queue 就是二次投递。故：
            // ① `status == Failed`（自然退出的失败）+ 锁证据在场才改道；
            // ② watchdog 超时（`stage == Timeout`）**不改道**——那是我们自己杀的，
            //    改道会把「我杀了它」谎报成「锁拒了它」；
            // ③ 取消（`status == Cancelled`）同理不改道（用户已叫停，不得二次投递）。
            let divertible = att.receipt.status == ReceiptStatus::Failed
                && att.receipt.stage != Some(Stage::Timeout)
                && evidence.is_some();
            if divertible {
                let qplan = dispatch(
                    AppPresence::Open,
                    plan.thread(),
                    args.project,
                    plan.message(),
                );
                let note = divert_note(&evidence.unwrap_or_default());
                let mut out =
                    run_queue_turn(args, &qplan, make_runner, deps, run, started, Some(&note))
                        .await;
                out.diverted = true;
                return out;
            }
            let (receipt, source) =
                exec_receipt(args.sid, &att, started.elapsed().as_millis() as u64);
            TurnOutcome {
                receipt,
                plan_used: PlanKind::ExecResume,
                diverted: false,
                receipt_source: source,
            }
        }
    }
}

/// 改道说明（如实注明：原路径被谁拒、改到哪里、未静默降级）
fn divert_note(evidence: &str) -> String {
    format!(
        "原计划 exec resume 被会话**单写者锁**拒绝（{evidence}；APP 在场时 exec 不可用，\
         issue #47193 跟踪）→ 已**自动改道 queue**（如实注明，未静默降级）；改道结果："
    )
}

/// `exec resume` 回执归一：非 0/超时/取消沿用 runner 结论；exit 0 → 末条回复 = stdout 尾行；
/// exit 0 且 stdout 无输出 → channel_error（**确认不了就不说成功**）
fn exec_receipt(sid: &str, att: &Attempt, total_ms: u64) -> (Receipt, ReceiptSource) {
    let r = att
        .receipt
        .clone()
        .with_session(sid)
        .with_duration_ms(total_ms);
    if r.status != ReceiptStatus::Ok {
        return (r, ReceiptSource::NotApplicable);
    }
    match last_stdout_line(&att.stdout) {
        Some(line) => {
            let mut ok = r;
            ok.last_assistant = Some(normalize::summarize(
                &line,
                super::receipt::LAST_ASSISTANT_CHARS,
            ));
            ok.reason = Some(
                "exec resume 正常退出：末条回复取自 stdout 尾行（回合由 codex CLI 自身执行）"
                    .to_string(),
            );
            (ok, ReceiptSource::ExecStdout)
        }
        None => (
            Receipt::failed(
                Stage::ChannelError,
                "exec resume 正常退出（exit 0）但 stdout 无任何输出——无法确认末条回复；\
                 请在会话内容中确认后再决定是否重发",
            )
            .with_session(sid)
            .with_duration_ms(total_ms),
            ReceiptSource::Unconfirmed,
        ),
    }
}

/// queue 路径全链（入队 + 消费确认）
#[allow(clippy::too_many_arguments)]
async fn run_queue_turn(
    args: &TurnArgs<'_>,
    plan: &Plan,
    make_runner: &MakeRunner<'_>,
    deps: &TurnDeps,
    run: &RunSeam,
    started: Instant,
    divert: Option<&str>,
) -> TurnOutcome {
    let sid = args.sid;
    let total_ms = || started.elapsed().as_millis() as u64;
    // 基线（消费确认的对照点）：必须在**入队之前**取，否则新追加段会被算进基线
    let baseline = args.rollout.and_then(|p| (deps.rollout.baseline)(p));
    let att = attempt(sid, plan, make_runner, run).await;
    let out = |receipt: Receipt, source: ReceiptSource| TurnOutcome {
        receipt,
        plan_used: PlanKind::Queue,
        diverted: divert.is_some(),
        receipt_source: source,
    };
    if att.receipt.status != ReceiptStatus::Ok {
        // 非 0 退出 / 超时 / 取消：沿用 runner 的如实结论（不改道、不重发）——
        // **但改道说明必须活到终局**（复审 Minor 1）：改道后的 queue 尝试若自己失败，
        // 回执仍须自报「本条是改道来的」，否则用户看到的是一次来历不明的失败
        let mut r = att.receipt.with_session(sid).with_duration_ms(total_ms());
        if let Some(note) = divert {
            let tail = r.reason.take().unwrap_or_default();
            r.reason = Some(format!("{note}{tail}"));
        }
        return out(r, ReceiptSource::NotApplicable);
    }
    let Some(msg_id) = queued_message_id(&att.stdout, &att.stderr) else {
        // exit 0 但没拿到入队回执 = 通道跑过了但没拿到有效回执（channel_error 语义）
        let reason = format!(
            "{}queue 进程正常退出（exit 0）但未拿到 `{QUEUE_RECEIPT_MARKER} <id>` 回执——\
             无法证明入队，按通道异常如实上报；stdout 头={}；stderr={}",
            prefixed(divert, ""),
            normalize::summarize(&head_of(&att.stdout), normalize::AUDIT_SUMMARY_CHARS),
            normalize::summarize(&att.stderr, normalize::AUDIT_SUMMARY_CHARS),
        );
        return out(
            Receipt::failed(Stage::ChannelError, &reason)
                .with_session(sid)
                .with_duration_ms(total_ms()),
            ReceiptSource::Unconfirmed,
        );
    };
    consume_and_report(args, plan, deps, sid, &msg_id, divert, baseline, total_ms()).await
}

/// 消费确认自建（入队回执之上再要一次**到达证据**）：
/// 1. 目标 rollout 不可定位/不可读 → 如实声明「消费确认不可用」（不假装确认）；
/// 2. 有界轮询 `[基线..EOF]` 追加段（命中消息文本 = 已消费）——期间移动端取消**送达**
///    （中止等待，回执如实说明消息仍在队列、无法撤回）；
/// 3. 仍未命中 → 查队列副本交叉验证（滞留 / 已无该条目 / 不可读，三态各自如实文案）。
#[allow(clippy::too_many_arguments)]
async fn consume_and_report(
    args: &TurnArgs<'_>,
    plan: &Plan,
    deps: &TurnDeps,
    sid: &str,
    msg_id: &str,
    divert: Option<&str>,
    baseline: Option<u64>,
    total_ms: u64,
) -> TurnOutcome {
    let queued = |tail: String| TurnOutcome {
        receipt: {
            let mut r = Receipt::ok(sid, total_ms);
            r.status = ReceiptStatus::Queued;
            r.reason = Some(prefixed(divert, &format!("{tail}（入队回执 id={msg_id}）")));
            r
        },
        plan_used: PlanKind::Queue,
        diverted: divert.is_some(),
        receipt_source: ReceiptSource::Enqueue,
    };
    let Some(rollout) = args.rollout else {
        return queued(
            "已入队；但目标 rollout 不可定位（该 thread 未在读链路登记 rollout 路径）→ \
             消费确认不可用——请在 APP 会话内确认是否已出现"
                .to_string(),
        );
    };
    let Some(base) = baseline else {
        return queued(
            "已入队；但目标 rollout 不可读（消费基线取不到）→ 消费确认不可用——\
             请在 APP 会话内确认是否已出现"
                .to_string(),
        );
    };
    // 消费等待期的取消靶子：送达（true）⇒ 中止等待；如实说明消息已入队、撤不回
    let cancelled = Arc::new(AtomicBool::new(false));
    registry().arm(sid, {
        let flag = cancelled.clone();
        Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
            true
        })
    });
    let needles = consume_needles(plan.message());
    let mut consumed = false;
    let mut unreadable = 0usize;
    for i in 0..deps.consume_attempts.max(1) {
        if cancelled.load(Ordering::SeqCst) {
            return cancelled_outcome(sid, msg_id, divert, total_ms);
        }
        if i > 0 {
            (deps.wait)(deps.consume_interval).await;
            if cancelled.load(Ordering::SeqCst) {
                return cancelled_outcome(sid, msg_id, divert, total_ms);
            }
        }
        match (deps.rollout.appended_hit)(rollout, base, &needles) {
            Some(true) => {
                consumed = true;
                break;
            }
            Some(false) => {}
            None => unreadable += 1,
        }
    }
    if consumed {
        let mut r = Receipt::ok(sid, total_ms);
        r.reason = Some(prefixed(
            divert,
            &format!(
                "APP 已消费该消息（目标 rollout 追加命中消息文本；入队回执 id={msg_id}）——\
                 回合执行与回复由 codex APP 自身完成，请在 APP 内或看板刷新后查看"
            ),
        ));
        return TurnOutcome {
            receipt: r,
            plan_used: PlanKind::Queue,
            diverted: divert.is_some(),
            receipt_source: ReceiptSource::RolloutAppend,
        };
    }
    // 交叉验证队列副本（**按入队回执 id 认领本条目**；三态各自如实，绝不编结论）
    let waited = deps.consume_budget_secs();
    let mut tail = match (deps.queue_holds)(plan.thread(), msg_id) {
        Some(QueueEvidence::Ours) => format!(
            "已入队未消费：等待 {waited}s 未观测到目标 rollout 追加，且入队回执 id={msg_id} 的条目\
             **仍在 codex 队列**（queue_1.sqlite 副本的 queued_items 命中该 id）——截至此刻本条\
             尚未被 APP 消费；**最可能的原因**是该 thread 未在 APP 打开（Mac 实测可滞留 19.2 分钟，\
             Windows 抽验消费约 55s，本回合等待 {waited}s 只是刚过该量级），也可能是 APP 忙碌或\
             落盘滞后——建议在 APP 打开该会话，或改走 exec resume"
        ),
        Some(QueueEvidence::ThreadStalled { rows }) => format!(
            "已入队未消费：等待 {waited}s 未观测到目标 rollout 追加；该 thread 队列里仍有 {rows} 条\
             滞留项，但**未能确认含本条**（副本里没有命中入队回执 id={msg_id} 的行）——建议在 APP \
             打开该会话确认，或改走 exec resume"
        ),
        Some(QueueEvidence::ThreadClear) => "已入队但**未观测到 rollout 追加**，且该 thread 在队列\
             副本里已无滞留条目——可能已被 APP 取走而落盘滞后，或目标 rollout 定位有误；请在 APP \
             会话内确认"
            .to_string(),
        None => "已入队但**未观测到 rollout 追加**；队列副本 queue_1.sqlite 不可读 → \
                 无法确认是否滞留；请在 APP 会话内确认"
            .to_string(),
    };
    if unreadable > 0 {
        tail.push_str(&format!(
            "（期间 {unreadable} 次追加段读取不完整/失败——未据此判否）"
        ));
    }
    queued(tail)
}

/// 消费等待被取消（移动端）：Cancelled + 如实说明「消息仍在队列、无法撤回」
fn cancelled_outcome(sid: &str, msg_id: &str, divert: Option<&str>, total_ms: u64) -> TurnOutcome {
    let r = Receipt::cancelled(&prefixed(
        divert,
        &format!(
            "已取消（移动端请求）——消费等待中止；消息**仍在 codex 队列中、无法撤回**\
             （入队回执 id={msg_id}）：请在 APP 打开该 thread 后查看，或自行在队列中清理"
        ),
    ))
    .with_session(sid)
    .with_duration_ms(total_ms);
    TurnOutcome {
        receipt: r,
        plan_used: PlanKind::Queue,
        diverted: divert.is_some(),
        receipt_source: ReceiptSource::NotApplicable,
    }
}

// ============================================================
// 生产只读缝（**测试构建恒空/恒否**：单测绝不 spawn 真 codex、绝不读真实 ~/.codex、
// 绝不扫真实进程表——与 zcode `production_roots` 同款宪法级纪律）
// ============================================================

/// codex CLI 发现：PATH 扫描（Windows 目录内 `.exe` 优先，其次 npm 垫片）。
/// **`cfg(test)` 恒 `None`**（见模块头「测试纪律」）⇒ 端点用例只能走「CLI 不可达」的
/// 如实失败路径，绝不会 spawn 真 codex 消耗用户配额
pub fn production_exe() -> Option<String> {
    #[cfg(test)]
    {
        // 测试构建：只有端点级用例装填的钩子能给出一条 CLI 路径（默认 None = 不可达）
        test_hooks::cli()
    }
    #[cfg(not(test))]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        let found = codex_in_path(&path, std::env::consts::OS);
        if found.is_none() {
            log::warn!("codex: PATH 上未找到 codex CLI");
        }
        found
    }
}

/// 目标 rollout 定位：state 库只读查 `threads.rollout_path`——**复用读链路既有的库发现**
/// （[`crate::monitor::codex_thread_parser::CodexThreadRoots`]），不另写扫描器。
/// **`cfg(test)` 恒 `None`**（测试构建不读真实 ~/.codex）
pub fn production_rollout_path(home: Option<&Path>, session_id: &str) -> Option<String> {
    #[cfg(test)]
    {
        let _ = (home, session_id);
        None
    }
    #[cfg(not(test))]
    {
        let home = home?;
        let roots = crate::monitor::codex_thread_parser::CodexThreadRoots::from_home(home);
        rollout_path_in_db(&roots.state_db, session_id)
    }
}

/// 队列滞留查询：`~/.codex/queue_*.sqlite` **副本**读（版本号取最大者，发现口径与
/// state/history 双库同源）。**`cfg(test)` 恒 `None`** = 「无法确认」（绝不读真实 ~/.codex）
pub fn production_queue_holds(thread: &str, msg_id: &str) -> Option<QueueEvidence> {
    #[cfg(test)]
    {
        let _ = (thread, msg_id);
        None
    }
    #[cfg(not(test))]
    {
        let home = dirs::home_dir()?;
        let db =
            crate::monitor::codex_thread_parser::latest_versioned(&home.join(".codex"), "queue_")?;
        queue_evidence_via_replica(&db, thread, msg_id)
    }
}

/// APP 在场观测：① 定向刷新会话宿主 pid 的 exe（**只问这一个 pid**）；② 未命中时全表扫
/// APP 主进程（复用读链路宿主判据 [`crate::monitor::host::tool_host_alive_in`]——它同时
/// 覆盖 `ChatGPT.app` 与旧 `Codex.app` 包名/`chatgpt.exe`）。
///
/// **`cfg(test)` 恒「不在场」**（不碰真实进程表）⇒ 端点用例走 `exec resume` 兜底分支的
/// 如实失败路径。
///
/// **登记限制（bundle id 精确识别）**：spec 要求按 bundle id（`com.openai.codex`）而非
/// 应用名识别，但进程表拿不到 bundle id（要读 `Info.plist`/装包元数据）；本实现复用 MAM
/// 读链路**同一份**宿主判据（`is_host_process(..., "codex")`）——即「APP 卡片从哪来，
/// 在场判定就从哪来」，不另造第二份口径；精确 bundle id 识别登记为后续项。
pub fn production_presence(form: ProcessForm, pid: u32) -> PresenceObs {
    #[cfg(test)]
    {
        let _ = pid;
        PresenceObs {
            form,
            host_alive: false,
            app_running: false,
        }
    }
    #[cfg(not(test))]
    {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let host_alive = if pid == 0 {
            false
        } else {
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
                true,
                ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
            );
            sys.process(sysinfo::Pid::from_u32(pid))
                .and_then(|p| p.exe())
                .map(|e| {
                    crate::monitor::host::is_host_process(
                        &e.to_string_lossy().to_lowercase(),
                        "codex",
                    )
                })
                .unwrap_or(false)
        };
        let app_running = host_alive || {
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
            );
            crate::monitor::host::tool_host_alive_in(&sys, "codex")
        };
        PresenceObs {
            form,
            host_alive,
            app_running,
        }
    }
}

/// 版本门控探针（codex 版；**消费 Task 6 的单点**：判定 [`gate::codex_verdict`]、缓存键
/// [`ProbeKey`]、缓存 [`ProbeCache`]、超时 [`gate::PROBE_TIMEOUT_MS`]、argv
/// [`gate::probe_argv`] 全部复用）。
///
/// # 为什么不直接调 [`super::gate::probe`]
/// Task 6 的 `probe` 以 `RunnerCfg::new(spec.exe())` 直 spawn，而 Windows 上的 codex 常是
/// npm 垫片（`.cmd`）——直 spawn 必失败（CreateProcess 不认批处理），门控会把整个 Windows
/// 通道判死。故本函数只把 **spawn 形态**换成 [`spawn_shape`]（`cmd /c <垫片>`），判定与缓存
/// 语义**一字未改**（也不改 Task 6 的 gate.rs 语义）。探针是 `queue --help`（**不触模型、
/// 不占无头回合名额**，自持 cap=1 名额）
pub async fn probe_cli(exe: &str, cache: &ProbeCache, os: &str) -> ProbeVerdict {
    #[cfg(test)]
    if let Some(injected) = test_hooks::probe() {
        let _ = (exe, os);
        return injected;
    }
    let spec = ProbeSpec::Codex {
        exe: exe.to_string(),
    };
    let key = ProbeKey::of(&spec);
    if let Some(hit) = cache.get(&key) {
        return hit;
    }
    if !key.exe_present {
        let v = ProbeVerdict::fail(format!("可执行文件不在场（不 spawn）：{exe}"));
        cache.put(key, v.clone());
        return v;
    }
    let shape = spawn_shape(exe, os);
    let mut cfg = RunnerCfg::new(&shape.program)
        .args(
            shape
                .prefix
                .iter()
                .cloned()
                .chain(super::gate::probe_argv(&spec)),
        )
        .timeout_ms(super::gate::PROBE_TIMEOUT_MS)
        .sem(GlobalSem::new(1));
    let receipt = cfg.run().await;
    let verdict = if receipt.stage == Some(Stage::Timeout) {
        ProbeVerdict::fail("探针干跑超时（15s）——codex 可能挂死")
    } else if receipt.stage == Some(Stage::Spawn) {
        ProbeVerdict::fail(format!("探针 spawn 失败：{exe}"))
    } else {
        super::gate::codex_verdict(
            &cfg.captured_stdout().join("\n"),
            &cfg.captured_stderr(),
            cfg.last_exit_code(),
        )
    };
    cache.put(key, verdict.clone());
    verdict
}

/// 探针缓存（进程级；键 = exe 在场性 + mtime：codex 升级即失效重探）
static PROBE_CACHE: std::sync::LazyLock<ProbeCache> = std::sync::LazyLock::new(ProbeCache::default);

pub fn probe_cache() -> &'static ProbeCache {
    &PROBE_CACHE
}

/// 执行缝出口（**通道自持**，端点不再借 `zcode::production_run_seam`）：
/// 生产 = 底座真 spawn 路径（[`super::turn::production_run_seam`]）；
/// `cfg(test)` = **脚本缝**（端点级用例注入结局，**绝不真 spawn**——真实账号配额纪律，
/// 见 [`test_hooks`]）
pub fn run_seam() -> Box<RunSeam> {
    #[cfg(test)]
    {
        test_hooks::run_seam()
    }
    #[cfg(not(test))]
    {
        super::turn::production_run_seam()
    }
}

/// **测试专用注入钩子**（仅 `cfg(test)`）：端点级分派链在测试构建里本来恒「CLI 不可达」，
/// 覆盖不到「真回合 → 单写者锁改道 → 回执封套与审计通道列」。故给端点用例三个可控面，
/// **生产构建里这些开关根本不存在**：
/// - `cli`：CLI 路径（默认 `None` = 不可达，保持原有诚实失败臂）；
/// - `probe_pass`：版本门控结论（默认 `None` = 走真探针；exe 缺席即如实拒发）；
/// - `script`：执行缝结局队列（默认空 = 任何尝试都返回**如实的 Spawn 失败**，绝不真 spawn）。
///
/// **并行纪律**（钩子是进程级全局态）：凡读写钩子的用例必须持 [`test_hooks::LOCK`]
/// 串行执行——与 Task 6 `HEADLESS_CAP_TEST_LOCK` 同款确定性方案，不靠重试启发式。
#[cfg(test)]
pub mod test_hooks {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 钩子用例串行锁（持锁期间其它端点用例不得断言 CLI 可达性）。
    /// **tokio 互斥量**（不是 std）：用例是 `#[tokio::test]`，守卫必然跨 await 持有——
    /// std 守卫跨 await 会被 `clippy::await_holding_lock` 正当地拦下
    pub(crate) static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    static CLI: std::sync::LazyLock<Mutex<Option<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    static PROBE: std::sync::LazyLock<Mutex<Option<ProbeVerdict>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    static SCRIPT: std::sync::LazyLock<Mutex<VecDeque<TurnObs>>> =
        std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));

    /// 装填钩子（**调用方必须已持 [`LOCK`]**）
    pub(crate) fn set(cli: Option<&str>, probe_pass: bool, script: Vec<TurnObs>) {
        *CLI.lock().unwrap_or_else(|e| e.into_inner()) = cli.map(str::to_string);
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(if probe_pass {
            ProbeVerdict::pass("测试注入：门控通过")
        } else {
            ProbeVerdict::fail("测试注入：门控拒发")
        });
        *SCRIPT.lock().unwrap_or_else(|e| e.into_inner()) = script.into();
    }

    /// 清空钩子（用例收尾；Drop 守卫调用）
    pub(crate) fn clear() {
        *CLI.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        SCRIPT.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub(crate) fn cli() -> Option<String> {
        CLI.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn probe() -> Option<ProbeVerdict> {
        PROBE.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 脚本缝：依次弹出结局；**脚本耗尽即如实的 Spawn 失败**（绝不回落到真 spawn）
    pub(crate) fn run_seam() -> Box<RunSeam> {
        Box::new(|_cfg: RunnerCfg| {
            let next = SCRIPT.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            Box::pin(async move {
                next.unwrap_or_else(|| TurnObs {
                    receipt: Receipt::failed(
                        Stage::Spawn,
                        "codex 测试构建：执行缝未注入脚本（绝不真 spawn 真 codex）",
                    ),
                    stdout: Vec::new(),
                    stderr: String::new(),
                    exit: None,
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const UUID: &str = "01a10735-1354-7d10-822a-f3bd9e041c12";
    const ROLLOUT: &str =
        "E:/t2/rollout-2026-10-04T21-58-01-01a10735-1354-7d10-822a-f3bd9e041c12.jsonl";

    // ===== 计划书 Step 1 原例（红灯测试） =====

    /// 计划书 Step 1 原例：探测定案——APP 开（thread 被打开）→ queue；关 → exec resume
    /// （**`-C` 前置！**）
    #[test]
    fn dispatch_by_app_presence() {
        let d = super::dispatch(super::AppPresence::Open, UUID, "E:/t2", "hi [mobile]");
        assert!(matches!(d, super::Plan::Queue { .. }));
        let d = super::dispatch(super::AppPresence::Closed, UUID, "E:/t2", "hi");
        assert!(matches!(d, super::Plan::ExecResume { argv, .. } if argv[1] == "-C"));
    }

    /// 计划书 Step 1 原例：thread id = rollout 文件名里的 UUIDv7（唯一来源）
    #[test]
    fn thread_uuid_from_rollout_filename() {
        assert_eq!(
            super::thread_id_of(
                "rollout-2026-10-04T21-58-01-01a10735-1354-7d10-822a-f3bd9e041c12.jsonl"
            )
            .as_deref(),
            Some(UUID)
        );
    }

    // ===== 命令形态（argv 逐字钉死 + `-C` 位置） =====

    /// queue 形态 = 本机 `codex queue --help` 实测 usage 逐字：
    /// `codex queue [OPTIONS] --thread <THREAD> --message <TEXT>`
    #[test]
    fn queue_argv_pins_the_probed_form() {
        let p = super::dispatch(AppPresence::Open, UUID, "E:/t2", "hi [mobile iPad]");
        assert_eq!(
            p.argv(),
            vec![
                CODEX_PROGRAM.to_string(),
                "queue".into(),
                "--thread".into(),
                UUID.into(),
                "--message".into(),
                "hi [mobile iPad]".into(),
            ],
            "queue argv 必须逐字对齐 CLI 实测 usage（顺序敏感）"
        );
        assert_eq!(p.thread(), UUID, "载荷里带的 thread 必须与 argv 同源");
        assert_eq!(p.kind(), PlanKind::Queue);
    }

    /// exec resume 形态逐字；**`-C` 位置由构造单点保证**（置于 `exec` 之后 = exit 2，
    /// 本机 `codex exec resume --help` 选项表里没有 `-C` ⇒ 它只能是顶层全局 flag）
    #[test]
    fn exec_resume_argv_pins_global_c_before_subcommand() {
        let p = super::dispatch(AppPresence::Closed, UUID, "E:/t2", "hi [mobile iPad]");
        assert_eq!(
            p.argv(),
            vec![
                CODEX_PROGRAM.to_string(),
                "-C".into(),
                "E:/t2".into(),
                "exec".into(),
                "resume".into(),
                UUID.into(),
                "hi [mobile iPad]".into(),
                SKIP_GIT_REPO_CHECK.into(),
            ],
            "exec resume argv 必须逐字对齐 spec H8（`-C` 前置 + 尾 flag）"
        );
        let argv = p.argv();
        let c = argv
            .iter()
            .position(|a| a == "-C")
            .expect("-C 必须在 argv 里");
        let sub = argv
            .iter()
            .position(|a| a == "exec")
            .expect("exec 必须在 argv 里");
        assert!(
            c < sub,
            "`-C` 下标（{c}）必须小于子命令 `exec` 下标（{sub}）——反了就是 exit 2"
        );
        assert_eq!(c, 1, "`-C` 恒在 argv[1]（程序名之后、一切子命令之前）");
        assert_eq!(argv[2], "E:/t2", "`-C` 的取值紧跟其后");
        assert_eq!(p.kind(), PlanKind::ExecResume);
    }

    /// 通道名映射单点（回执封套/Audit channel 列同源，勿在端点另抄）
    #[test]
    fn plan_kind_maps_to_wire_channel() {
        assert_eq!(
            PlanKind::Queue.headless_kind(),
            crate::inject::routing::HeadlessKind::CodexQueue
        );
        assert_eq!(
            PlanKind::ExecResume.headless_kind(),
            crate::inject::routing::HeadlessKind::CodexExec
        );
        assert_eq!(
            PlanKind::Queue.headless_kind().wire_name(),
            "headless_codex_queue"
        );
        assert_eq!(
            PlanKind::ExecResume.headless_kind().wire_name(),
            "headless_codex_exec"
        );
    }

    /// Windows verbatim 前缀（state DB 的 cwd 列实测形如 `\\?\C:\…`）不得进 `-C` 取值；
    /// UNC 形式与普通路径原样保留（不做猜测式改写）
    #[test]
    fn project_path_drops_verbatim_prefix() {
        assert_eq!(
            normalize_project_path(r"\\?\C:\Users\u\Test2"),
            r"C:\Users\u\Test2"
        );
        assert_eq!(
            normalize_project_path(r"\\?\UNC\srv\share\p"),
            r"\\?\UNC\srv\share\p",
            "UNC 形态不猜（只剥本地盘符 verbatim 前缀）"
        );
        assert_eq!(normalize_project_path("/tmp/p"), "/tmp/p");
        assert_eq!(normalize_project_path("E:/t2"), "E:/t2");
    }

    // ===== APP 在场判定 =====

    /// 在场判据（探测定案）：APP 形态 **且**（会话宿主仍在进程表 **或** APP 进程在场）；
    /// CLI 形态恒不在场（CLI TUI 会话不走 H8——W3 保持终端注入）
    #[test]
    fn presence_requires_app_form_and_evidence() {
        let obs = |form, host_alive, app_running| PresenceObs {
            form,
            host_alive,
            app_running,
        };
        assert_eq!(
            presence_of(&obs(ProcessForm::App, true, false)),
            AppPresence::Open,
            "会话宿主进程仍在 = APP 开着"
        );
        assert_eq!(
            presence_of(&obs(ProcessForm::App, false, true)),
            AppPresence::Open,
            "APP 主进程在场（pid=0 的未读/哨兵卡）也算开着"
        );
        assert_eq!(
            presence_of(&obs(ProcessForm::App, false, false)),
            AppPresence::Closed,
            "两条证据都没有 = 如实判关（走 exec resume 兜底）"
        );
        assert_eq!(
            presence_of(&obs(ProcessForm::Cli, true, true)),
            AppPresence::Closed,
            "CLI 形态不受 APP 在场影响"
        );
    }

    // ===== thread id 映射 =====

    /// id 只认 rollout 文件名里的 UUID：全路径也只看最后一段；无 UUID / 非 rollout 名 → None
    /// （**绝不拿会话名兜底**）
    #[test]
    fn thread_id_only_from_rollout_name() {
        assert_eq!(
            thread_id_of(ROLLOUT).as_deref(),
            Some(UUID),
            "全路径只看 basename"
        );
        assert_eq!(
            thread_id_of(&ROLLOUT.replace('/', "\\")).as_deref(),
            Some(UUID),
            "Windows 分隔符"
        );
        assert_eq!(thread_id_of("rollout-2026-10-04T21-58-01.jsonl"), None);
        assert_eq!(thread_id_of(UUID), None, "裸 UUID 不是 rollout 文件名");
        // 「hi」这类会话名/标题永不参与映射
        assert_eq!(thread_id_of("hi"), None);
        assert_eq!(thread_id_of("rollout-hi.jsonl"), None);
        assert_eq!(thread_id_of(""), None);
    }

    /// 会话 → thread 引用：rollout 文件名里的 UUID 优先（spec：文件名是唯一权威）；
    /// 定位不到 rollout 时**仅当**会话号本身是 UUID 形态才回退（否则拒绝，不按名字投递）
    #[test]
    fn resolve_thread_prefers_rollout_uuid_and_rejects_names() {
        let r = resolve_thread(Some(ROLLOUT), UUID).expect("文件名 UUID 必须解出");
        assert_eq!(r.id, UUID);
        assert_eq!(r.rollout_path.as_deref(), Some(ROLLOUT));
        // 文件名与 state DB id 漂移：文件名优先（spec 权威）
        let drifted = resolve_thread(Some(ROLLOUT), "01a99999-0000-7000-8000-000000000000")
            .expect("漂移时也解得出");
        assert_eq!(drifted.id, UUID, "rollout 文件名是 id 唯一权威");
        // 无 rollout 路径：UUID 形态的会话号可回退；非 UUID（会话名）一律拒绝
        let fallback = resolve_thread(None, UUID).expect("UUID 会话号可回退");
        assert_eq!(fallback.id, UUID);
        assert!(fallback.rollout_path.is_none());
        assert!(
            resolve_thread(None, "hi").is_none(),
            "会话名不得当 thread id（Mac 普查重名 ×5）"
        );
        assert!(resolve_thread(None, "sess_abc").is_none());
    }

    // ===== 回执解析 / 锁分类 =====

    /// 入队回执 = `Queued message <msg-id>`（stdout 或 stderr 两路都找——不猜走哪一路）
    #[test]
    fn queued_message_id_parses_receipt() {
        assert_eq!(
            queued_message_id(&["Queued message 0f1e2d3c-aaaa".to_string()], ""),
            Some("0f1e2d3c-aaaa".to_string())
        );
        assert_eq!(
            queued_message_id(&[], "Queued message from-stderr"),
            Some("from-stderr".to_string()),
            "回执行走 stderr 也算（不猜哪一路）"
        );
        assert_eq!(queued_message_id(&["nothing here".to_string()], ""), None);
        assert_eq!(
            queued_message_id(&["Queued message   ".to_string()], ""),
            None,
            "只有标记没有 id ≠ 有效回执（不编 id）"
        );
    }

    /// 单写者锁分类（spec H8 / issue #47193）：特征串命中即定性；只有 `-32600` 而
    /// 无 writer 语义 → **不定性**（别的 JSON-RPC 错误码不得被当成锁）
    #[test]
    fn writer_lock_needs_the_phrase_not_just_the_code() {
        assert!(writer_lock_evidence("", "Error: -32600 already has an active writer").is_some());
        assert!(writer_lock_evidence("ALREADY HAS AN ACTIVE WRITER", "").is_some());
        assert!(
            writer_lock_evidence("{\"code\":-32600,\"message\":\"writer busy\"}", "").is_some(),
            "码 + writer 语义同现才算"
        );
        assert!(
            writer_lock_evidence("-32600 invalid params", "").is_none(),
            "裸错误码不得定性为单写者锁"
        );
        assert!(writer_lock_evidence("all good", "").is_none());
    }

    /// exec resume 的末条回复 = stdout 最后一条非空行（spec H8）
    #[test]
    fn last_stdout_line_is_the_reply() {
        assert_eq!(
            last_stdout_line(&[
                "thinking".to_string(),
                String::new(),
                "最终回复".to_string()
            ]),
            Some("最终回复".to_string())
        );
        assert_eq!(last_stdout_line(&[]), None);
        assert_eq!(last_stdout_line(&["  ".to_string()]), None);
    }

    // ===== 消费确认（纯核） =====

    /// 针（needles）：原文 + JSON 转义变体（rollout 是 JSONL，引号/反斜杠会被转义）；
    /// 无需转义时**不重复**
    #[test]
    fn consume_needles_cover_json_escaped_variant() {
        assert_eq!(
            consume_needles("hi [mobile iPad]"),
            vec!["hi [mobile iPad]"]
        );
        let n = consume_needles("say \"hi\"\\ok");
        assert_eq!(n.len(), 2, "原文 + 转义变体：{n:?}");
        assert_eq!(n[0], "say \"hi\"\\ok");
        assert!(n[1].contains("\\\""), "转义变体须把引号转成 \\\"：{n:?}");
        assert!(consume_needles("").is_empty(), "空载荷不产生针");
        assert!(consume_needles("   ").is_empty());
    }

    /// rollout 追加命中：**只看基线之后的新增段**（同一文本的历史行不得算消费——
    /// 「hi」在 Mac 普查里重名 ×5）；基线未前进 = 无追加
    #[test]
    fn rollout_append_probe_only_counts_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rollout-x.jsonl");
        std::fs::write(&f, "{\"old\":\"hi [mobile iPad]\"}\n").unwrap();
        let path = f.to_string_lossy().to_string();
        let base = baseline_len(&path).expect("基线可读");
        // 基线之前的历史同文本 → 不算消费
        assert_eq!(
            rollout_appended_hit(&path, base, &consume_needles("hi [mobile iPad]")),
            Some(false),
            "历史行不得算消费"
        );
        // 追加一行（APP 消费后写 rollout 的形态）→ 命中
        let mut extra = std::fs::read_to_string(&f).unwrap();
        extra.push_str("{\"new\":\"hi [mobile iPad]\"}\n");
        std::fs::write(&f, extra).unwrap();
        assert_eq!(
            rollout_appended_hit(&path, base, &consume_needles("hi [mobile iPad]")),
            Some(true)
        );
        // 文件不存在 / 基线与当前同长 → 如实
        assert_eq!(baseline_len("Z:/mam-nonexistent-rollout.jsonl"), None);
        let now = baseline_len(&path).unwrap();
        assert_eq!(
            rollout_appended_hit(&path, now, &consume_needles("hi")),
            Some(false)
        );
    }

    /// 追加段超过读取上限时**不判否**（只读了头段，结论不可得 ⇒ None = 无法确认）
    #[test]
    fn oversized_append_is_unconfirmed_not_false() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rollout-big.jsonl");
        std::fs::write(&f, "x").unwrap();
        let path = f.to_string_lossy().to_string();
        let base = baseline_len(&path).unwrap();
        let mut s = String::new();
        for _ in 0..64 {
            s.push_str("0123456789abcdef\n");
        }
        std::fs::write(&f, format!("x{s}")).unwrap();
        // 上限 32 字节：区域远大于它，且针不在头段 → None（不判否）
        assert_eq!(
            rollout_appended_hit_with_cap(&path, base, &consume_needles("needle"), 32),
            None
        );
        // 上限够大 → 如实 false
        assert_eq!(
            rollout_appended_hit_with_cap(&path, base, &consume_needles("needle"), 1 << 20),
            Some(false)
        );
    }

    /// 队列副本查询：**按 (thread_id, id) 双键认领本条目**（复审 Minor 2：只凭 thread
    /// 有行会把「恰有别的条目」误读成「本条未消费」）；schema 取本机活库实证
    #[test]
    fn queue_evidence_claims_only_our_own_item() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue_1.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE queued_items (
                 id TEXT PRIMARY KEY NOT NULL,
                 thread_id TEXT NOT NULL,
                 payload_json TEXT NOT NULL,
                 queue_order INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO queued_items VALUES ('m-ours', ?1, '{}', 0, 1, 1)",
            [UUID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO queued_items VALUES ('m-theirs', ?1, '{}', 1, 2, 2)",
            [UUID],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            queue_evidence_in_db(&db, UUID, "m-ours"),
            Some(QueueEvidence::Ours),
            "回执 id 命中 ⇒ 本条目仍在队列"
        );
        assert_eq!(
            queue_evidence_in_db(&db, UUID, "m-unknown"),
            Some(QueueEvidence::ThreadStalled { rows: 2 }),
            "只有同 thread 别的行 ⇒ **未确认含本条**（不得说成本条滞留）"
        );
        assert_eq!(
            queue_evidence_in_db(&db, "01a99999-0000-7000-8000-000000000000", "m-ours"),
            Some(QueueEvidence::ThreadClear),
            "别的 thread 不参与本条判定"
        );
        assert_eq!(
            queue_evidence_in_db(&dir.path().join("nope.sqlite"), UUID, "m-ours"),
            None,
            "副本不可读 → None（不猜）"
        );
    }

    /// **活库不直查纪律**：队列副本读只碰副本，源库字节原样（本机实测 APP 持 WAL 写锁）；
    /// 副本读完即清（逐个文件删，不做递归删除）
    #[test]
    fn queue_replica_read_never_touches_the_live_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue_1.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE queued_items (
                 id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL, payload_json TEXT NOT NULL,
                 queue_order INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
             INSERT INTO queued_items VALUES ('m9', '01a10735-1354-7d10-822a-f3bd9e041c12', '{}', 0, 1, 1);",
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&db).unwrap();
        assert_eq!(
            queue_evidence_via_replica(&db, UUID, "m9"),
            Some(QueueEvidence::Ours),
            "副本里查到本条目"
        );
        assert_eq!(
            queue_evidence_via_replica(&db, UUID, "m-not-mine"),
            Some(QueueEvidence::ThreadStalled { rows: 1 })
        );
        assert_eq!(
            std::fs::read(&db).unwrap(),
            before,
            "源库字节必须原样（活库不直查：只读副本）"
        );
        assert_eq!(
            queue_evidence_via_replica(&dir.path().join("missing.sqlite"), UUID, "m9"),
            None,
            "源库不在场 → None（且不得凭空造出文件）"
        );
        assert!(!dir.path().join("missing.sqlite").exists(), "不得创建源库");
    }

    /// state 库取 rollout 路径（生产核心；列名 `threads.rollout_path` = 本机活库实证）
    #[test]
    fn rollout_path_query_reads_threads_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        let p = r"\\?\C:\Users\u\.codex\sessions\2026\10\04\rollout-2026-10-04T21-58-01-01a10735-1354-7d10-822a-f3bd9e041c12.jsonl";
        conn.execute(
            "INSERT INTO threads (id, rollout_path) VALUES (?1, ?2)",
            [UUID, p],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path) VALUES ('other', '')",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(rollout_path_in_db(&db, UUID).as_deref(), Some(p));
        assert_eq!(rollout_path_in_db(&db, "other"), None, "空路径 = 未登记");
        assert_eq!(rollout_path_in_db(&db, "absent"), None);
        assert_eq!(
            rollout_path_in_db(&dir.path().join("nope.sqlite"), UUID),
            None
        );
        // 与 id 映射合流：库里的路径 → 文件名 UUID
        let resolved = resolve_thread(rollout_path_in_db(&db, UUID).as_deref(), UUID).unwrap();
        assert_eq!(resolved.id, UUID);
    }

    // ===== CLI 发现 / spawn 形态 =====

    /// PATH 扫描（纯核）：**PATH 顺序优先**（与 Windows 自身解析一致——先命中的安装就是
    /// 用户实际会跑的安装），目录内 `.exe` 优先于 npm 垫片；POSIX 找裸名文件；
    /// miss → None（**绝不 spawn 不存在的程序**）
    #[test]
    fn codex_cli_discovery_prefers_real_exe() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(a.join("codex.cmd"), "@echo off").unwrap();
        std::fs::write(b.join("codex.exe"), "MZ").unwrap();
        let win = format!("{};{}", a.to_string_lossy(), b.to_string_lossy());
        let hit = codex_in_path(&win, "windows").expect("应命中");
        assert!(
            hit.ends_with("codex.cmd"),
            "PATH 顺序优先（先命中的目录即用户实际会跑的安装）：{hit}"
        );
        // 同一目录内 .exe 优先于垫片（垫片在 cmd 里会被重解析）
        std::fs::write(c.join("codex.cmd"), "@echo off").unwrap();
        std::fs::write(c.join("codex.exe"), "MZ").unwrap();
        let same_dir = codex_in_path(&c.to_string_lossy(), "windows").expect("同目录应命中");
        assert!(
            same_dir.ends_with("codex.exe"),
            "同目录内 .exe 优先：{same_dir}"
        );
        // 只有垫片时命中垫片（Windows npm 全局包形态）
        let only_shim = codex_in_path(&a.to_string_lossy(), "windows").expect("垫片也应命中");
        assert!(only_shim.ends_with("codex.cmd"));
        // POSIX：裸名文件（**Windows 上不可达**：绝对路径恒含盘符冒号 → 如实登记，
        // 不假装覆盖；分隔符规则本身由 path_dirs 的纯核用例钉住）
        #[cfg(unix)]
        {
            std::fs::write(a.join("codex"), "#!/bin/sh").unwrap();
            let posix = codex_in_path(&a.to_string_lossy(), "macos").expect("POSIX 裸名");
            assert!(posix.ends_with("codex") && !posix.ends_with(".cmd"));
        }
        #[cfg(windows)]
        assert!(
            codex_in_path(&a.to_string_lossy(), "macos").is_none(),
            "Windows 绝对路径含盘符冒号 ⇒ POSIX 分隔符分支在 Windows 上不可达"
        );
        // miss
        assert!(codex_in_path(&b.to_string_lossy(), "macos").is_none());
        assert!(codex_in_path("", "windows").is_none());
    }

    /// PATH 分段（纯核，平台可测）：分隔符按平台取；空段/引号壳滤除
    #[test]
    fn path_dirs_splits_by_platform_separator() {
        assert_eq!(
            path_dirs("/usr/bin:/bin", "macos"),
            vec!["/usr/bin".to_string(), "/bin".to_string()]
        );
        assert_eq!(
            path_dirs(r"C:\npm;D:\bin", "windows"),
            vec![r"C:\npm".to_string(), r"D:\bin".to_string()]
        );
        assert_eq!(
            path_dirs(r#""C:\with space";;C:\b"#, "windows"),
            vec![r"C:\with space".to_string(), r"C:\b".to_string()],
            "空段滤除 + 引号壳剥掉"
        );
        assert!(path_dirs("   ", "windows").is_empty());
    }

    /// spawn 形态：Windows 垫片 → `cmd /c <垫片>`（CreateProcess 不认批处理）；
    /// 其余（`.exe` / POSIX 裸名 / 非 Windows 的 .cmd 名）直 spawn
    #[test]
    fn spawn_shape_uses_cmd_only_for_windows_shims() {
        let direct = spawn_shape("C:/bin/codex.exe", "windows");
        assert_eq!(direct.program, "C:/bin/codex.exe");
        assert!(direct.prefix.is_empty());
        let shim = spawn_shape(r"C:\Users\u\AppData\Roaming\npm\codex.cmd", "windows");
        assert_eq!(shim.program, "cmd");
        assert_eq!(shim.prefix[0], "/c");
        assert!(shim.prefix[1].ends_with("codex.cmd"));
        assert!(shim_needs_cmd("codex.bat"));
        assert!(!shim_needs_cmd("codex.exe"));
        assert!(
            spawn_shape("codex.cmd", "macos").prefix.is_empty(),
            "非 Windows 不套 cmd（POSIX 上 .cmd 名只是文件名）"
        );
    }

    // ===== 编排（脚本缝：零真实进程 / 零真实等待 / 零真实 ~/.codex） =====

    /// 脚本执行缝：第 n 次调用返回第 n 个结局（越界 → exit 0 空输出）
    fn scripted_run(script: Vec<TurnObs>) -> (Box<RunSeam>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0usize));
        let seen = calls.clone();
        let script = Arc::new(script);
        let seam: Box<RunSeam> = Box::new(move |cfg: RunnerCfg| {
            let n = {
                let mut g = seen.lock().unwrap();
                let n = *g;
                *g += 1;
                n
            };
            let obs = script
                .get(n)
                .cloned()
                .unwrap_or_else(|| obs_exit(0, &[], ""));
            let _ = cfg;
            Box::pin(async move { obs })
        });
        (seam, calls)
    }

    /// 观测构造：非 0 退出 → Crash 回执（与 Task 6 runner 同口径），stdout/stderr 原样
    fn obs_exit(code: i32, stdout: &[&str], stderr: &str) -> TurnObs {
        let receipt = if code == 0 {
            Receipt::ok("", 7)
        } else {
            Receipt::failed(Stage::Crash, &format!("退出码 {code}；{stderr}"))
        };
        TurnObs {
            receipt,
            stdout: stdout.iter().map(|s| s.to_string()).collect(),
            stderr: stderr.to_string(),
            exit: Some(code),
        }
    }

    fn queued_obs(msg_id: &str) -> TurnObs {
        obs_exit(0, &[&format!("Queued message {msg_id}")], "")
    }

    /// 编排依赖（脚本消费探针：hits 逐轮出栈；queue_holds 固定结论）
    fn deps_with(hits: Vec<Option<bool>>, holds: Option<QueueEvidence>) -> TurnDeps {
        let q: Arc<Mutex<VecDeque<Option<bool>>>> = Arc::new(Mutex::new(hits.into()));
        TurnDeps {
            consume_attempts: 3,
            consume_interval: Duration::from_millis(1),
            wait: Box::new(|_| Box::pin(async {})),
            rollout: RolloutProbe {
                baseline: Box::new(|_| Some(100)),
                appended_hit: Box::new(move |_, _, _| {
                    q.lock().unwrap().pop_front().unwrap_or(Some(false))
                }),
            },
            queue_holds: Box::new(move |_, _| holds),
        }
    }

    /// 记录到的计划 argv 表（测试断言「实跑的是哪条计划、`-C` 位置」用）
    type RecordedArgvs = Arc<Mutex<Vec<Vec<String>>>>;

    /// 记录型 make_runner（把每个 plan 的 argv 记下来供断言）
    fn recording_runner() -> (RecordedArgvs, Box<MakeRunner<'static>>) {
        let seen: RecordedArgvs = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        (
            seen,
            Box::new(move |p: &Plan| {
                rec.lock().unwrap().push(p.argv().to_vec());
                RunnerCfg::for_test().session_id("sess")
            }),
        )
    }

    fn args_of<'a>(sid: &'a str, rollout: Option<&'a str>) -> TurnArgs<'a> {
        TurnArgs {
            sid,
            project: "E:/t2",
            rollout,
        }
    }

    /// queue 路径：入队回执 + rollout 追加命中 = **已消费**（Ok 且如实标注证据来源；
    /// 回合执行与回复归 APP 自身，故不编 lastAssistant）
    #[tokio::test]
    async fn queue_turn_confirms_consumption_via_rollout_append() {
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi [mobile iPad]");
        let (run, calls) = scripted_run(vec![queued_obs("m-1")]);
        let (argv_seen, make) = recording_runner();
        let deps = deps_with(vec![Some(false), Some(true)], Some(QueueEvidence::Ours));
        let out = run_turn(
            &args_of("sess_cq_confirm", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt_source, ReceiptSource::RolloutAppend);
        assert_eq!(out.plan_used, PlanKind::Queue);
        assert!(!out.diverted);
        assert!(out.receipt.last_assistant.is_none(), "不得编造末条回复");
        let reason = out.receipt.reason.unwrap_or_default();
        assert!(
            reason.contains("已消费") && reason.contains("APP"),
            "回执必须如实说清证据来源与归属：{reason}"
        );
        assert!(!reason.contains("未确认"), "{reason}");
        assert_eq!(*calls.lock().unwrap(), 1, "只投递一次（不重发）");
        assert_eq!(argv_seen.lock().unwrap().len(), 1);
        assert_eq!(argv_seen.lock().unwrap()[0][1], "queue");
    }

    /// queue 路径：60s 内未消费且队列副本查到滞留 → **如实报「已入队未消费」**+
    /// 可执行建议（**不得读成已送达，也不得读成失败**）
    #[tokio::test]
    async fn unconsumed_queue_item_is_reported_as_enqueued_not_delivered() {
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![queued_obs("m-2")]);
        let (_a, make) = recording_runner();
        let deps = deps_with(
            vec![Some(false), Some(false), Some(false)],
            Some(QueueEvidence::Ours),
        );
        let out = run_turn(
            &args_of("sess_cq_stall", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Queued,
            "入队未消费 = 排队态（既不是 ok 也不是 failed）：{:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt_source,
            ReceiptSource::Enqueue,
            "证据链止于入队回执（只证入队）"
        );
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(reason.contains("未消费"), "{reason}");
        assert!(reason.contains("APP 打开"), "要给可执行建议：{reason}");
        assert!(reason.contains("exec"), "要给出改道建议：{reason}");
        assert!(reason.contains("m-2"), "入队回执 id 必须在场：{reason}");
        assert!(
            reason.contains("未观测到目标 rollout 追加"),
            "必须写出**观测事实**（等待 N s 无追加），不得只给结论：{reason}"
        );
        assert!(
            reason.contains("最可能的原因"),
            "原因只能是**可能**（复审 Minor 3：58s 预算刚过 Windows 抽验量级，不得写成已证事实）：{reason}"
        );
        assert!(
            !reason.contains("未被 APP 打开；"),
            "不得把「thread 未打开」写成断言的同一性：{reason}"
        );
        assert!(
            out.receipt.stage.is_none(),
            "排队不是失败阶段：{:?}",
            out.receipt
        );
    }

    /// 队列副本三态**各自如实**（复审 Minor 2）：命中本条目 id / 只有同 thread 别的行
    /// （**未确认含本条**）/ 已无该 thread 条目 / 副本不可读——四种文案互不冒充
    #[tokio::test]
    async fn unconsumed_without_queue_evidence_stays_honest() {
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi");
        let (_a, make) = recording_runner();
        for (holds, needle) in [
            (None, "无法确认"),
            (Some(QueueEvidence::ThreadClear), "已无滞留条目"),
            (
                Some(QueueEvidence::ThreadStalled { rows: 3 }),
                "未能确认含本条",
            ),
            (Some(QueueEvidence::Ours), "命中该 id"),
        ] {
            let (run, _) = scripted_run(vec![queued_obs("m-3")]);
            let deps = deps_with(vec![Some(false)], holds);
            let out = run_turn(
                &args_of("sess_cq_noev", Some(ROLLOUT)),
                &plan,
                &make,
                &deps,
                &*run,
            )
            .await;
            assert_eq!(out.receipt.status, ReceiptStatus::Queued, "{holds:?}");
            let reason = out.receipt.reason.clone().unwrap_or_default();
            assert!(
                reason.contains(needle),
                "holds={holds:?} 文案须含「{needle}」：{reason}"
            );
            assert!(reason.contains("未观测到"), "{reason}");
            assert!(
                reason.contains("m-3"),
                "入队回执 id 必须在场（认领判据）：{reason}"
            );
        }
    }

    /// 目标 rollout 不可定位/不可读：仍如实报「已入队」，但**明确声明消费确认不可用**
    #[tokio::test]
    async fn queue_turn_without_rollout_declares_confirmation_unavailable() {
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![queued_obs("m-4")]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![Some(true)], Some(QueueEvidence::ThreadClear));
        let out = run_turn(&args_of("sess_cq_noroll", None), &plan, &make, &deps, &*run).await;
        assert_eq!(out.receipt.status, ReceiptStatus::Queued);
        assert_eq!(out.receipt_source, ReceiptSource::Enqueue);
        let reason = out.receipt.reason.unwrap_or_default();
        assert!(
            reason.contains("不可定位") && reason.contains("消费确认不可用"),
            "{reason}"
        );
    }

    /// exit 0 但没有 `Queued message <id>` → ChannelError（通道跑过了但没拿到有效回执）；
    /// 非 0 退出 → 沿用 runner 的 Crash（不冒充）
    #[tokio::test]
    async fn queue_without_receipt_line_is_channel_error() {
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi");
        let (_a, make) = recording_runner();
        let (run, _) = scripted_run(vec![obs_exit(0, &["some noise"], "")]);
        let deps = deps_with(vec![Some(true)], Some(QueueEvidence::ThreadClear));
        let out = run_turn(
            &args_of("sess_cq_norec", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert_eq!(out.receipt_source, ReceiptSource::Unconfirmed);
        let reason = out.receipt.reason.unwrap_or_default();
        assert!(
            reason.contains("Queued message"),
            "要说清缺哪条回执：{reason}"
        );

        let (run2, _) = scripted_run(vec![obs_exit(1, &[], "boom")]);
        let out2 = run_turn(
            &args_of("sess_cq_norec", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run2,
        )
        .await;
        assert_eq!(out2.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out2.receipt.stage, Some(Stage::Crash));
        assert_eq!(out2.receipt_source, ReceiptSource::NotApplicable);
    }

    /// 消费等待期间的移动端取消：**送达**（不是哑的）→ Cancelled 回执，且如实说明
    /// 「消息仍在 codex 队列里、无法撤回」。**独占会话号**（守卫 id 立规②：登记表按裸 id
    /// 全局占用，与同模块其它编排用例错开 → 不会互相顶掉取消靶子）
    #[tokio::test]
    async fn cancel_during_consumption_poll_is_delivered_and_honest() {
        const SID: &str = "sess_cq_cancel";
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![queued_obs("m-5")]);
        let (_a, make) = recording_runner();
        let mut deps = deps_with(
            vec![Some(false), Some(false), Some(false)],
            Some(QueueEvidence::Ours),
        );
        // 等待缝里按下取消（走**同一** 进程级登记表——取消端点只认这一份）
        deps.wait = Box::new(|_| {
            registry().request_cancel(SID);
            Box::pin(async {})
        });
        assert!(
            registry().begin(SID, TurnSlot::placeholder("codex", "hi".to_string())),
            "夹具须先占位（端点同款）"
        );
        let out = run_turn(&args_of(SID, Some(ROLLOUT)), &plan, &make, &deps, &*run).await;
        registry().end(SID);
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "{:?}",
            out.receipt
        );
        let reason = out.receipt.reason.unwrap_or_default();
        assert!(reason.contains("取消"), "{reason}");
        assert!(
            reason.contains("队列") && (reason.contains("无法撤回") || reason.contains("仍在")),
            "必须如实说明消息已入队无法撤回：{reason}"
        );
    }

    /// exec resume 成功：末条回复 = stdout 尾行（截断走 W5 单点）
    #[tokio::test]
    async fn exec_turn_uses_stdout_tail_as_last_reply() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![obs_exit(0, &["thinking…", "最终回复"], "")]);
        let (argv_seen, make) = recording_runner();
        let deps = deps_with(vec![], None);
        let out = run_turn(&args_of("sess_ce_ok", None), &plan, &make, &deps, &*run).await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt_source, ReceiptSource::ExecStdout);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("最终回复"));
        assert_eq!(out.plan_used, PlanKind::ExecResume);
        let argv = argv_seen.lock().unwrap()[0].clone();
        assert_eq!(argv[1], "-C", "实跑 argv 也必须 `-C` 前置：{argv:?}");
        assert_eq!(argv[2], "E:/t2");
    }

    /// exec resume：exit 0 但 stdout 无输出 → ChannelError（**确认不了就不说成功**）
    #[tokio::test]
    async fn exec_turn_with_empty_stdout_is_channel_error() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![obs_exit(0, &[], "")]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![], None);
        let out = run_turn(&args_of("sess_ce_empty", None), &plan, &make, &deps, &*run).await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert!(out.receipt.reason.unwrap_or_default().contains("stdout"));
    }

    /// **单写者锁改道**（spec H8 / issue #47193）：exec 被 `already has an active writer`
    /// 拒 → 自动改道 queue，**如实注明**（含原错误证据），并按 queue 语义继续确认消费
    #[tokio::test]
    async fn exec_writer_lock_diverts_to_queue_and_declares_it() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (run, calls) = scripted_run(vec![
            obs_exit(1, &[], "Error: -32600 already has an active writer"),
            queued_obs("m-6"),
        ]);
        let (argv_seen, make) = recording_runner();
        let deps = deps_with(vec![Some(true)], Some(QueueEvidence::ThreadClear));
        let out = run_turn(
            &args_of("sess_ce_lock", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert!(out.diverted, "必须标记改道");
        assert_eq!(out.plan_used, PlanKind::Queue, "实际通道 = queue");
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(*calls.lock().unwrap(), 2, "先 exec 后 queue 各一次");
        let argv = argv_seen.lock().unwrap().clone();
        assert_eq!(argv[0][1], "-C", "第一次是 exec resume：{argv:?}");
        assert_eq!(argv[1][1], "queue", "第二次是 queue 改道：{argv:?}");
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("单写者锁") && reason.contains("改道"),
            "改道必须如实注明：{reason}"
        );
        assert!(
            reason.contains(WRITER_LOCK_MARKER),
            "原错误证据必须在场：{reason}"
        );
    }

    /// 改道**不是**无条件：watchdog 超时（我们自己杀的）不触发改道——如实报超时
    #[tokio::test]
    async fn timeout_does_not_divert_to_queue() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let timeout_obs = TurnObs {
            receipt: Receipt::failed(Stage::Timeout, "watchdog 到点"),
            stdout: Vec::new(),
            stderr: "already has an active writer".to_string(),
            exit: None,
        };
        let (run, calls) = scripted_run(vec![timeout_obs]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![], None);
        let out = run_turn(
            &args_of("sess_ce_timeout", None),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert!(!out.diverted, "超时不得改道（进程是我们杀的）");
        assert_eq!(out.receipt.stage, Some(Stage::Timeout));
        assert_eq!(*calls.lock().unwrap(), 1, "不得二次投递");
    }

    /// **改道判据必须先是「失败」的尝试**（复审 Important）：exit 0 的 exec 即使 stdout
    /// 出现特征串（正文引用该句 / CLI 把锁告警降级为警告后仍跑完），消息**已经送达**——
    /// 若按特征串改道就是把同一条消息投递两次（exec 一次 + queue 一次）。
    #[tokio::test]
    async fn successful_exec_with_lock_phrase_does_not_divert() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        // exit 0 + 正文里恰好含特征串 + 末条回复正常
        let obs = obs_exit(
            0,
            &[
                "note: another client said \"already has an active writer\" but we proceeded",
                "最终回复",
            ],
            "",
        );
        let (run, calls) = scripted_run(vec![obs]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![Some(true)], Some(QueueEvidence::Ours));
        let out = run_turn(
            &args_of("sess_ce_ok_phrase", None),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert!(
            !out.diverted,
            "成功的 exec 绝不得改道（否则二次投递）：{:?}",
            out.receipt
        );
        assert_eq!(out.plan_used, PlanKind::ExecResume);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("最终回复"));
        assert_eq!(*calls.lock().unwrap(), 1, "只投递一次");
    }

    /// 取消（用户已叫停）同样不改道——不得在用户叫停后再投一次
    #[tokio::test]
    async fn cancelled_exec_with_lock_phrase_does_not_divert() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let obs = TurnObs {
            receipt: Receipt::cancelled("已取消（移动端请求，先到者生效）"),
            stdout: vec!["already has an active writer".to_string()],
            stderr: "-32600 writer busy".to_string(),
            exit: None,
        };
        let (run, calls) = scripted_run(vec![obs]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![], None);
        let out = run_turn(
            &args_of("sess_ce_cancel_phrase", None),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert!(!out.diverted, "取消不得触发改道");
        assert_eq!(out.receipt.status, ReceiptStatus::Cancelled);
        assert_eq!(*calls.lock().unwrap(), 1, "不得在用户叫停后再投一次");
    }

    /// **改道说明活到终局**（复审 Minor 1）：改道后的 queue 尝试自身失败时，回执仍须
    /// 自报「本条是改道来的」+ 原错误证据——否则用户看到一次来历不明的失败
    #[tokio::test]
    async fn divert_note_survives_a_failed_post_divert_queue_attempt() {
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (run, calls) = scripted_run(vec![
            obs_exit(1, &[], "Error: -32600 already has an active writer"),
            obs_exit(2, &[], "queue 子命令失败"),
        ]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![], None);
        let out = run_turn(
            &args_of("sess_ce_lock_then_fail", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert!(out.diverted);
        assert_eq!(out.plan_used, PlanKind::Queue, "实际走向仍是 queue");
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(
            out.receipt.stage,
            Some(Stage::Crash),
            "沿用 runner 的如实结论"
        );
        assert_eq!(*calls.lock().unwrap(), 2);
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("单写者锁") && reason.contains("改道"),
            "改道说明必须活到终局：{reason}"
        );
        assert!(
            reason.contains(WRITER_LOCK_MARKER),
            "原错误证据必须仍在场：{reason}"
        );
        assert!(
            reason.contains('2') || reason.contains("queue"),
            "改道后的失败证据也要在（不吞掉第二段原因）：{reason}"
        );
    }
}
