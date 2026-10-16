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
//! - APP 关：`codex -C <项目> exec resume -c approval_policy=<档> <UUID> "<正文> [mobile 花名]"
//!   --skip-git-repo-check`（H5 策略旗子见 [`exec_resume_argv`] 的实测说明）。
//!   **`-C`/`--cd` 是顶层全局 flag，必须置于子命令之前**（`codex exec resume --help` 的选项表
//!   里**没有** `-C` ⇒ 置于 `exec` 之后会 exit 2，Mac 实测同款），而 `-c/--config` 两处都在场
//!   ⇒ 策略旗子放在子命令 `resume` **之后**。本模块把 argv 构造成
//!   「`argv[0]` = 程序名 + `argv[1]` = `-C`」并**由单一构造器保证**（[`exec_resume_argv`]），
//!   测试钉死位置（`argv[1] == "-C"` 且 `-C` 的下标恒小于 `exec`、`-c` 恒大于 `resume`）。
//!
//! # ⚠️ 登记：`-c approval_policy=…` 是**行为面改动**，语义未实机验证（归 H11/M11 实机批次）
//! Task 10（H5）给 exec 兜底路注入 `-c approval_policy=on-request` —— **本机只取证了旗子的
//! 形态**（`--help` 解析 + 退出码，见 [`exec_resume_argv`]），**没取证它的行为语义**：非交互
//! `exec` 面在模型请求批准时到底如何收场（即刻拒绝 / 自动批准 / 挂起不返回）目前只有 spec H5
//! 的「turn 不阻塞」说法，无本机证据。`codex --help` 另有 `--approve-for-me`（把批准交给自动
//! 审查）这一**候选形态**——本批不猜、不改。
//! **义务归属**：codex/kimi/opencode 的策略**语义**随 **H11 / M11 实机探测**关账（spec §H5
//! 「策略驱动」条 + 计划 Task 13 Step 5 USER-ASSIST 实机步与 Task 15 的 M11 验收行）——届时按
//! 实测修订默认档、旗子形态与回执文案（含「需批准/被拒事件回流回执」是否真的成立）。
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
//! 再查 `~/.codex/queue_1.sqlite` **副本**的 `queued_items` 交叉验证（**codex 特例**：APP 持
//! WAL 写锁，故走副本——**裁决 20 豁免项**；WB 已改只读直连，两者不是同一条纪律），
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
//! **cmd 重解析面（三面收口；Windows 垫片真机行为仍未实机验证）**：`cmd` 会**重解析整条
//! 命令行**——不只 `%VAR%`（未定义变量原样保留）：`"` 的配对规则、`&` `|` `^` `<` `>`
//! 的重定向/串联语义都按 cmd 的口径解析，而 Rust 传参用的是 CRT 的 `\"` 转义风格（与
//! cmd 不同）⇒ **含这些字符的命令行分段在垫片路径上可能被截断/改写/串联执行**。边界：
//! 仅当解析出的 CLI 是 `.cmd`/`.bat` 时走此路（[`spawn_shape`]）——`.exe` 直连不过 cmd。
//! 缓解 = [`codex_in_path`] 的 `.exe` 优先——但该优先序**只在同一目录内生效**（PATH 顺序
//! 仍先于扩展名优先），故本机这种「PATH 上只有 npm 垫片」的安装**正在走垫片路径**。
//!
//! 逐面收口（**每面一个判据单点；都是投递前拒绝、零字节投递、文案同源**）：
//! - **正文面（R3 复审升级）**：原先只「登记为已知限制」，R3 升级为**投递前拒绝**——
//!   [`super::turn::cmd_shim_body_refusal`]（正文 + 已解析形态 ⇒ 放行/拒绝，通道无关单点）
//!   在两处分派点（本通道 + `cli_three` 的 kimi/opencode 臂）拒绝含元字符的正文，回执档 =
//!   `Stage::Refused`（未起跑、零字节投递，文案风格同斜杠命令拒绝）。`.exe` 直装不受影响。
//! - **花名面（裁决 24b，收尾轮）**：R3 当时只判**用户正文**——我们追加的
//!   `[mobile <花名>]` 签名不在判据内，故花名含 `&`/`%` 时该签名仍可能被 cmd 重解析（当时
//!   **如实登记为残留**）。收尾轮按当时写下的收口方向落地：花名走**白名单**
//!   [`super::turn::device_name_refusal`]（Unicode 字母数字 + 空格 `-` `_` `.` `·`），判据
//!   落在**共享组装单点** [`crate::inject::normalize::compose_injection`]（拼签名前 fail
//!   closed，回执档同样 `refused`）+ 设备名注册点（`remote::api::persist_and_cookie`，
//!   拒登记）。本通道的签名由 compose 单点产出，无第二条拼签名路径。
//! - **项目路径面（评审 I8，本轮）**：exec 兜底路的 [`exec_resume_argv`] 把
//!   `-C <项目路径>`（会话 `project_path`，用户可控）放进 argv，而正文面/花名面**都只判
//!   自己那一段** ⇒ 项目路径含 `&`/`%`/`^`/`"` 时整条命令行照样被 cmd 重解析（与两面
//!   **同族**的敞口）。判据 = [`argv_shim_refusal`]（**逐段**委托正文面同一个函数 ⇒ 同源
//!   字符表、**逐字相同**的文案），执行臂 = [`attempt`]——**spawn 前唯一咽喉**（exec 臂 /
//!   queue 臂 / exec→queue 改道臂全经此处），命中即 `Receipt::failed(Stage::Refused, …)`：
//!   在 `make_runner` **之前**返回 ⇒ 零回合配置、零进程、零并发名额占用。argv 全由
//!   [`dispatch`]（[`Plan::argv`]）产出，无第二条拼 argv 路径。
//!
//! **如实登记的边界（都不是「未收口」，逐条写清免得后人误读成敞口）**：
//! ① 项目路径面在**回合任务内**判：端点分派点的 `shape` 只与 `plan` 同场于该分派点，
//!    单文件内以**同一个** [`spawn_shape`] + **同一个** [`production_exe`] 复算形态
//!    （不是第二份「是不是垫片」判据），故它**晚于同请求内的版本探针一步**——探针不投递
//!    任何载荷分段、不占回合名额，「零字节投递」与回执档（`refused`）都不变；
//! ② `shape.prefix`（`cmd /c <垫片路径>`：垫片安装路径来自 PATH 解析、**不是用户数据**）
//!    不在本判据面内——本面只收口「进 argv 的载荷分段」；
//! ③ Windows npm 垫片的**真机行为**仍未实机验证（义务归 H11/M11 实机批次）：本面是「按
//!    cmd 解析规则**前移到投递前拒绝**」，不是对 cmd 行为的实机取证。
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
// H5 权限档参数面（Task 10）：exec 兜底路的策略旗子从它取——**单一来源**，勿另写字面
use super::PermissionSpec;
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
///
/// **H5 权限档（Task 10）**：只有 exec 兜底路取档（[`PermissionSpec::codex_exec_default`]，
/// 默认 `on-request`）——`-c approval_policy=…` 在 spawn 时给定，turn 不因审批阻塞。
/// **queue 路没有审批面**（H8 定案）：入队只是短命进程，turn 执行与审批归 codex APP 自身，
/// 故 Queue 臂**不取**任何权限档（也不得顺手加旗子——测试 `queue_plan_carries_no_permission_flag`
/// 钉死）。
pub fn dispatch(presence: AppPresence, thread: &str, project: &str, text: &str) -> Plan {
    match presence {
        AppPresence::Open => Plan::Queue {
            argv: queue_argv(thread, text),
            thread: thread.to_string(),
            message: text.to_string(),
        },
        AppPresence::Closed => Plan::ExecResume {
            argv: exec_resume_argv(project, thread, text, PermissionSpec::codex_exec_default()),
            thread: thread.to_string(),
            message: text.to_string(),
        },
    }
}

/// `codex queue --thread <UUID> --message <text>`（本机 `codex queue --help` usage 逐字：
/// `Usage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>`）
///
/// **H5 边界落码（H8 定案）：queue 通道没有审批面。** 本命令只是把消息**入队**的短命
/// 进程；turn 执行（含工具审批）归 codex APP 自身、按 APP 自己的配置走 ⇒ MAM 侧**没有**
/// 可注入的权限档旗子。故本函数一个字都不加（由
/// `queue_plan_carries_no_permission_flag` 钉死）——需要策略驱动的场合
/// 走 `exec resume`（见 [`exec_resume_argv`]）。
fn queue_argv(thread: &str, message: &str) -> Vec<String> {
    vec![
        CODEX_PROGRAM.to_string(),
        "queue".to_string(),
        "--thread".to_string(),
        thread.to_string(),
        CODEX_MESSAGE_FLAG.to_string(),
        message.to_string(),
    ]
}

/// `codex -C <dir> exec resume -c approval_policy=<档> <UUID> <text> --skip-git-repo-check`
///
/// **`-C` 全局 flag 前置（唯一构造点）**：本机 `codex exec resume --help` 的选项表里
/// **没有** `-C`（只有 `-c/--config`）⇒ 它只能是顶层全局 flag，置于 `exec` 之后即 exit 2。
/// argv 的 `-C` 位置由本函数**按构造保证**，测试再钉一次下标顺序（[`super::tests`]）
///
/// **H5 策略旗子位置（Task 10 实测）**：`-c/--config` 在**顶层与 `exec resume` 两处**都在场
/// ——`codex exec resume -c approval_policy=on-request --help` ⇒ exit 0；对照
/// `codex exec resume -C E:/t2 --help` ⇒ exit 2 `unexpected argument '-C' found`
/// （`-C` 是全局-only 的证明）。故 `-c` 放在子命令 `resume` **之后**（照 usage
/// `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]` 的选项前置序），与 `-C` 的全局前置
/// 方向相反——**勿挪位**，测试钉死下标。
/// 档位来自 [`PermissionSpec`]（[`PermissionSpec::codex_exec_default`] = `on-request`）。
///
/// **⚠️ 登记（行为面未实机验证 → 归 H11/M11）**：`-c approval_policy=<档>` 的**语义**
/// （非交互 exec 面在模型需批准时如何收场、`--approve-for-me` 是否更合适）**无本机证据**，
/// 本批只取证旗子形态；义务随 **H11/M11 实机探测**关账——完整指针见模块头「⚠️ 登记」段。
fn exec_resume_argv(
    project: &str,
    thread: &str,
    message: &str,
    perm: PermissionSpec,
) -> Vec<String> {
    let mut argv = vec![
        CODEX_PROGRAM.to_string(),
        CODEX_PROJECT_FLAG.to_string(),
        normalize_project_path(project),
        "exec".to_string(),
        "resume".to_string(),
    ];
    // H5 策略旗子（`-c approval_policy=<档>`）：档由 spec 单点产生
    argv.extend(perm.flags().into_iter().map(str::to_string));
    argv.push(thread.to_string());
    argv.push(message.to_string());
    argv.push(SKIP_GIT_REPO_CHECK.to_string());
    argv
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

/// 单写者锁特征串命中（**大小写口径的唯一单点**，评审 M3）：ASCII 大小写不敏感。
///
/// 为什么必须单点：同一句锁告警在两处判据上出现过（本模块的 stdout/stderr 头扫描、
/// `codex_app::classify` 的错误帧 message）——**一处 lowercase 再 contains、另一处原样
/// 大小写敏感**，等于同一句告警换个大小写就一边判锁、一边判普通 RPC 错误（口径漂移）。
/// 上游服务端原文的大小写不受我们控制（版本间也会漂），故判据必须宽容，且只有一份。
pub fn hits_writer_lock_marker(text: &str) -> bool {
    find_ascii_ci(text, WRITER_LOCK_MARKER).is_some()
}

/// 单写者锁证据（spec H8 / issue #47193）。判据：特征串命中，或 `-32600` 与 writer 语义
/// **同现**——裸错误码不定性（别的 JSON-RPC 失败不得被当成锁）。两分支都走
/// [`find_ascii_ci`]（同一大小写口径）。
pub fn writer_lock_evidence(stdout_head: &str, stderr_head: &str) -> Option<String> {
    for hay in [stdout_head, stderr_head] {
        if hits_writer_lock_marker(hay) {
            return Some(WRITER_LOCK_MARKER.to_string());
        }
        if find_ascii_ci(hay, WRITER_LOCK_RPC_CODE).is_some()
            && find_ascii_ci(hay, "writer").is_some()
        {
            return Some(format!(
                "CODEX_WRITER_LOCK_EVIDENCE: {WRITER_LOCK_RPC_CODE} + writer（单写者锁）"
            ));
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

/// **codex 特例：APP 持 WAL 写锁故走副本（裁决 20 豁免项；WB 已改只读直连）**：把队列库
/// **复制**到临时文件再查——APP 持 WAL 写锁，直查会阻塞甚至被拒。`-wal`/`-shm` 一并复制
/// （WAL 里才有最近的提交）。副本读完逐个文件删除（**不做递归删除**）。
/// 注意与「只读直连」的区别：项目读侧纪律是**只读连接直查活库**（裁决 20，WB/zcode/opencode
/// 同规），codex **队列库**是**唯一豁免**——写锁在 APP 手里，只读连接也拿不到一致视图。
/// 别把本函数「统一」成只读直连（那会在这条通道上引入阻塞/被拒）。
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

/// PATH 分段 / `cmd /c` 垫片包装 / spawn 形态（**Task 13 上提到 [`super::turn`]**——
/// H11 三家 CLI 共用同一套；此处 `pub use` + 薄委托保持 codex 既有调用面与测试不变，
/// **零行为变化**）
pub use super::turn::{path_dirs, shim_needs_cmd, spawn_shape, SpawnShape};

/// PATH 扫描（**codex 专用入口**：委托给泛化的 [`super::turn::cli_in_path`]）
pub fn codex_in_path(path_env: &str, os: &str) -> Option<String> {
    super::turn::cli_in_path("codex", path_env, os)
}

// ------------------------------------------------------------
// 进 argv 的载荷分段 × cmd 垫片拒绝（**项目路径面收口**，评审 I8）
// ------------------------------------------------------------

/// **进 argv 的载荷分段 + 已解析 spawn 形态 ⇒ 放行 / 拒绝**（单一决策点；`Some(reason)` =
/// 拒发）。**逐段委托** [`super::turn::cmd_shim_body_refusal`]——同源字符表
/// （[`super::turn::CMD_SHIM_METACHARS`]）、同源形态门（[`SpawnShape::via_cmd_shim`]）、
/// **逐字相同**的文案（不另抄一份字符表，也不另写第二套措辞）。
///
/// # 为什么需要（评审 I8：`-C <项目路径>` 曾绕过既有拒绝面）
/// 正文面（R3）与花名面（裁决 24b）**都只判自己那一段**，而 `cmd /c` 重解析的是**整条
/// 命令行**：exec 兜底路的 [`exec_resume_argv`] 把 `-C` 的取值 = 会话项目路径
/// （`~/.codex` state 库 `threads.cwd` ⇒ **用户可控**）拼进 argv，含 `&` / `%` / `^` / `"`
/// 时与正文/花名**同族**地被截断/改写/串联执行。本函数即该面的收口处。
///
/// # 判据面（`argv[0]` **之外**逐段全判）
/// `argv[0]` **不判**：它是 [`CODEX_PROGRAM`] 程序名词表项（我们自己给的字面量，不是用户
/// 数据）；其余分段**全判** ⇒ `-C` 的取值（本条目正主）、thread UUID、`-c` 策略旗子、
/// 最终载荷都在判据面内，将来 argv 增段也自动落进来（用例从**真组装器**取 argv，形态一改
/// 就一起红）。
///
/// # 文案（**同一个函数的原话**，不另起措辞——按「同款文案」的字面要求）
/// 命中时返回的就是正文面那句原话：字符点名 +「cmd 会重解析整条命令行」的原因 +
/// 「本条**未投递**（零字节投递）」+ 补救建议四段全在。**如实登记**：该句主语写的是
/// 「正文」（它是正文面的单点文案）——本面按「别另写一套措辞」原样复用，故项目路径命中时
/// 读到的也是这句话；要按分段改名须把文案参数化（在 [`super::turn`] 内，属另一文件）。
///
/// # 纯核
/// 入参 = 形态 + argv，零 I/O、零进程 ⇒ 可脱离真实进程单测（见 tests 的
/// `argv_shim_refusal_*`：形态取自 [`spawn_shape`] 的真实产物、argv 取自 [`dispatch`]
/// 的真实产物，不手抄字面量）。
///
/// # 边界（见模块头「如实登记的边界」）
/// `shape.prefix`（垫片安装路径，来自 PATH 解析、不是用户数据）不在判据面内。
/// 项目路径旗子（`-C`，**单一来源**：argv 构造器与本函数的「段身份」判据同用一份，
/// 免得两处各写一个字面量而漂移）
pub const CODEX_PROJECT_FLAG: &str = "-C";

/// 正文旗子（`--message`，queue 形态；**单一来源**：[`queue_argv`] 构造器与
/// [`segment_subject`] 的正文段判据同用一份——同 [`CODEX_PROJECT_FLAG`] 的纪律）
pub const CODEX_MESSAGE_FLAG: &str = "--message";

/// argv 分段的**身份判据**（评审 N2：主语标注的原料，纯函数）：按旗子的**真实位置**推，
/// 不按「前一段恰为 [`CODEX_PROJECT_FLAG`] 字面量」推——正文可以是任意字面量（含恰为
/// `"-C"`），旧的「前一段」启发式会把正文 `"-C"` 的后邻段（如 `--skip-git-repo-check`）
/// 误标成「项目路径（-C 取值）」。
///
/// - **项目路径段** = `-C` 旗子的取值：真旗子恒在 **argv[1]**（exec 形态全局前置，由
///   [`exec_resume_argv`] 唯一构造器钉死、测试钉下标）⇒ 取值恒为 argv[2] 的**位置契约**。
///   正文段（queue 的 [`CODEX_MESSAGE_FLAG`] 取值 / exec 的 PROMPT 位置参数）都到不了
///   argv[1]，位置即把「正文伪造 `-C` 旗子」的面关死——等价于「显式定位正文段后排除」，
///   且更简单（无需在判据里枚举正文段的全部形态）。
/// - **正文段及其余分段**：主语按既有口径走兜底面 [`super::turn::CmdShimSubject::ArgvSegment`]
///   ——正文面已有专门之门与「正文」文案（`cmd_shim_body_refusal`，I8 承诺逐字未变），
///   本判据所在的 [`argv_shim_refusal`] 是**纵深防御**，不是第二判据（既有用例语义不动）。
fn segment_subject(argv: &[String], i: usize) -> super::turn::CmdShimSubject {
    if i == 2 && argv[1].as_str() == CODEX_PROJECT_FLAG {
        return super::turn::CmdShimSubject::ProjectPath;
    }
    super::turn::CmdShimSubject::ArgvSegment
}

pub fn argv_shim_refusal(shape: &SpawnShape, argv: &[String]) -> Option<String> {
    // 逐段判，**按段身份出文案**（评审 I8 文案诚实化）：`-C` 的取值是**项目路径**，
    // 其余载荷分段是泛指的「命令行分段」。旧写法一律按「正文」出文案——项目路径命中时
    // 用户会去改正文，而真凶在项目路径上，改多少遍都没用（错报对象 = 不诚实）。
    // 段身份判据 = [`segment_subject`]（评审 N2：位置契约，不被正文 "-C" 伪造）。
    let mut i = 1;
    while i < argv.len() {
        if let Some(reason) =
            super::turn::cmd_shim_refusal(segment_subject(argv, i), &argv[i], shape, true)
        {
            return Some(reason);
        }
        i += 1;
    }
    None
}

/// 本回合 spawn 形态（**复算，不是第二判据**）：同一个 CLI 发现出口 [`production_exe`] +
/// 同一个构造点 [`spawn_shape`]。生产下与端点分派点（`remote::api` 的 `5a` 步）解析出的
/// 形态逐字段一致（同 PATH、同 `std::env::consts::OS`）——「是不是垫片」的判据仍只有
/// [`SpawnShape::via_cmd_shim`] 一份。
///
/// `None` = CLI 不可达 ⇒ **不判**（端点侧对不可达早按 `Stage::Spawn` 如实拒绝，本函数的
/// 调用点在生产上到不了；此处 fail-open 不漏投递：没有 exe 就没有 spawn）。
///
/// `cfg(test)` 面可经 [`test_hooks::shape`] 注入形态：`spawn_shape` 只在 `windows` +
/// `.cmd`/`.bat` 上产出垫片形态，注入面让「垫片形态的**执行臂**」（不只是纯判据）在任何
/// 平台都能被用例钉住。
fn current_spawn_shape() -> Option<SpawnShape> {
    #[cfg(test)]
    if let Some(s) = test_hooks::shape() {
        return Some(s);
    }
    production_exe().map(|exe| spawn_shape(&exe, std::env::consts::OS))
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
///
/// **⓪ 进 argv 载荷分段的 cmd 垫片拒绝（评审 I8；判据 = [`argv_shim_refusal`]）**：本函数
/// 是**唯一的 spawn 前咽喉**（exec 臂、queue 臂、exec→queue 改道臂全经此处），故判据不会
/// 漏掉任何一条实际投递路径。命中即 `Stage::Refused`（与正文面/花名面**同档同文案**：
/// 未起跑、零字节投递），且在 `make_runner` **之前**返回 ⇒ 零回合配置、零进程、零名额。
async fn attempt(sid: &str, plan: &Plan, make_runner: &MakeRunner<'_>, run: &RunSeam) -> Attempt {
    if let Some(shape) = current_spawn_shape() {
        if let Some(reason) = argv_shim_refusal(&shape, plan.argv()) {
            return Attempt {
                receipt: Receipt::failed(Stage::Refused, &reason).with_session(sid),
                stdout: Vec::new(),
                stderr: String::new(),
            };
        }
    }
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
        "CODEX_DIVERT_WRITER_LOCK: 原计划 exec resume 被会话**单写者锁**拒绝（{evidence}；APP 在场时 exec 不可用，\
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
                "CODEX_EXEC_NO_OUTPUT: exec resume 正常退出（exit 0）但 stdout 无任何输出——无法确认末条回复；\
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
        let reason = prefixed(
            divert,
            &format!(
                "CODEX_QUEUE_NO_RECEIPT: queue 进程正常退出（exit 0）但未拿到 `{QUEUE_RECEIPT_MARKER} <id>` 回执——\
             无法证明入队，按通道异常如实上报；stdout 头={}；stderr={}",
                normalize::summarize(&head_of(&att.stdout), normalize::AUDIT_SUMMARY_CHARS),
                normalize::summarize(&att.stderr, normalize::AUDIT_SUMMARY_CHARS),
            ),
        );
        return out(
            Receipt::failed(Stage::ChannelError, &reason)
                .with_session(sid)
                .with_duration_ms(total_ms()),
            ReceiptSource::Unconfirmed,
        );
    };
    consume_and_report(args, plan, deps, sid, &msg_id, divert, baseline, started).await
}

/// 消费确认自建（入队回执之上再要一次**到达证据**）：
/// 1. 目标 rollout 不可定位/不可读 → 如实声明「消费确认不可用」（不假装确认）；
/// 2. 有界轮询 `[基线..EOF]` 追加段（命中消息文本 = 已消费）——期间移动端取消**送达**
///    （中止等待，回执如实说明消息仍在队列、无法撤回）；
/// 3. 仍未命中 → 查队列副本交叉验证（滞留 / 已无该条目 / 不可读，三态各自如实文案）。
///
/// **耗时口径（评审 I4）**：本函数收 `started`（回合起点）而**不是**一个已求值的 `u64`
/// ——旧实现由调用方在 `await` 前求值，消费等待（最多 60s）全部漏计，与模块头
/// 「回执耗时 = 整回合墙钟（**含消费等待**）」不符，且审计行同步少报。现在每个终态出口
/// 各自现求 `started.elapsed()`（exec 路径 `exec_receipt` 早就是这写法，此处照搬）。
#[allow(clippy::too_many_arguments)]
async fn consume_and_report(
    args: &TurnArgs<'_>,
    plan: &Plan,
    deps: &TurnDeps,
    sid: &str,
    msg_id: &str,
    divert: Option<&str>,
    baseline: Option<u64>,
    started: Instant,
) -> TurnOutcome {
    let total_ms = || started.elapsed().as_millis() as u64;
    let queued = |tail: String| TurnOutcome {
        receipt: {
            let mut r = Receipt::ok(sid, total_ms());
            r.status = ReceiptStatus::Queued;
            r.reason = Some(prefixed(
                divert,
                &format!("CODEX_ENQUEUE_RECEIPT_ID: {tail}（入队回执 id={msg_id}）"),
            ));
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
    // 消费等待期的取消靶子：送达（true）⇒ 中止等待；如实说明消息已入队、撤不回。
    // 槽位通道同步（Task 6b review M-3）：改道腿（exec 计划 → 锁拒 → 改投 queue）
    // 落定进入消费等待——取消端点按槽位通道词分叉，不同步则改道腿的取消拿不到
    // 「queue 滞留」的撤回缺失明示（裸 cancelled:true）。计划即 queue 的腿同步为
    // 同值（幂等无副作用）
    registry().update_channel(
        sid,
        crate::inject::routing::HeadlessKind::CodexQueue.wire_name(),
    );
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
            return cancelled_outcome(sid, msg_id, divert, total_ms());
        }
        if i > 0 {
            (deps.wait)(deps.consume_interval).await;
            if cancelled.load(Ordering::SeqCst) {
                return cancelled_outcome(sid, msg_id, divert, total_ms());
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
        let mut r = Receipt::ok(sid, total_ms());
        r.reason = Some(prefixed(
            divert,
            &format!(
                "CODEX_CONSUMED_BY_APP: APP 已消费该消息（目标 rollout 追加命中消息文本；入队回执 id={msg_id}）——\
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
            "CODEX_QUEUED_NOT_CONSUMED: 已入队未消费：等待 {waited}s 未观测到目标 rollout 追加，且入队回执 id={msg_id} 的条目\
             **仍在 codex 队列**（queue_1.sqlite 副本的 queued_items 命中该 id）——截至此刻本条\
             尚未被 APP 消费；**最可能的原因**是该 thread 未在 APP 打开（Mac 实测可滞留 19.2 分钟，\
             Windows 抽验消费约 55s，本回合等待 {waited}s 只是刚过该量级），也可能是 APP 忙碌或\
             落盘滞后——建议在 APP 打开该会话，或改走 exec resume"
        ),
        Some(QueueEvidence::ThreadStalled { rows }) => format!(
            "CODEX_QUEUED_STALLED_UNVERIFIED: 已入队未消费：等待 {waited}s 未观测到目标 rollout 追加；该 thread 队列里仍有 {rows} 条\
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
            "CODEX_APPEND_READ_INCOMPLETE: （期间 {unreadable} 次追加段读取不完整/失败——未据此判否）"
        ));
    }
    queued(tail)
}

/// 消费等待被取消（移动端）：Cancelled + 如实说明「消息仍在队列、无法撤回」
fn cancelled_outcome(sid: &str, msg_id: &str, divert: Option<&str>, total_ms: u64) -> TurnOutcome {
    let r = Receipt::cancelled(&prefixed(
        divert,
        &format!(
            "CODEX_CANCELLED_QUEUED_UNRECALLABLE: 已取消（移动端请求）——消费等待中止；消息**仍在 codex 队列中、无法撤回**\
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

/// 队列滞留查询：`~/.codex/queue_*.sqlite` **副本**读（**codex 特例**：APP 持 WAL 写锁故走
/// 副本，**裁决 20 豁免项**——见 [`queue_evidence_via_replica`]；版本号取最大者，发现口径与
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
        let v = ProbeVerdict::fail(format!(
            "PROBE_EXE_MISSING: 可执行文件不在场（不 spawn）：{exe}"
        ));
        cache.put(key, v.clone());
        return v;
    }
    // 返工 M-1：真探针分支起止计时——spawn 尝试/超时/正常三路的实测墙钟都计入
    // verdict（与 gate.rs 同机制），api.rs:2155 审计行据此出真实耗时（旧态恒「· 0ms」）
    let started = Instant::now();
    let verdict = {
        #[cfg(test)]
        {
            // 缝（返工 M-1）：测试注入执行体替代真 spawn——缓存/exe 在场性检查仍走真路径
            if let Some(round) = test_hooks::probe_round() {
                round()
            } else {
                probe_cli_round(exe, &spec, os).await
            }
        }
        #[cfg(not(test))]
        {
            probe_cli_round(exe, &spec, os).await
        }
    }
    .with_duration_ms(started.elapsed().as_millis() as u64);
    cache.put(key, verdict.clone());
    verdict
}

/// 真探针执行体（从 [`probe_cli`] 分离，仅为让 M-1 的计时不被缓存/exe 检查稀释）：
/// spawn `queue --help`（Windows npm 垫片经 [`spawn_shape`] 转一手）+ 判定。
async fn probe_cli_round(exe: &str, spec: &ProbeSpec, os: &str) -> ProbeVerdict {
    let shape = spawn_shape(exe, os);
    let mut cfg = RunnerCfg::new(&shape.program)
        .args(
            shape
                .prefix
                .iter()
                .cloned()
                .chain(super::gate::probe_argv(spec)),
        )
        .timeout_ms(super::gate::PROBE_TIMEOUT_MS)
        .sem(GlobalSem::new(1));
    let receipt = cfg.run().await;
    probe_cli_verdict(&receipt, &cfg, exe)
}

/// 探针判定（纯函数，返工 M-2 缝）：Timeout → **复用 [`gate::timeout_zero_output_hint`]**
/// 的单一文案源（秒数从常量生成——旧写死「15s/可能挂死」字面与阈值漂移且误导归因）；
/// Spawn → spawn 失败；其余 → [`gate::codex_verdict`]。
fn probe_cli_verdict(receipt: &Receipt, cfg: &RunnerCfg, exe: &str) -> ProbeVerdict {
    if receipt.stage == Some(Stage::Timeout) {
        return ProbeVerdict::fail(super::gate::timeout_zero_output_hint());
    }
    if receipt.stage == Some(Stage::Spawn) {
        return ProbeVerdict::fail(format!("PROBE_SPAWN_FAIL: 探针 spawn 失败：{exe}"));
    }
    super::gate::codex_verdict(
        &cfg.captured_stdout().join("\n"),
        &cfg.captured_stderr(),
        cfg.last_exit_code(),
    )
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

    /// 真探针执行体缝的句柄类型（类型别名：clippy::type_complexity）
    pub(crate) type ProbeRoundFn = std::sync::Arc<dyn Fn() -> ProbeVerdict + Send + Sync>;

    /// 钩子用例串行锁（持锁期间其它端点用例不得断言 CLI 可达性）。
    /// **tokio 互斥量**（不是 std）：用例是 `#[tokio::test]`，守卫必然跨 await 持有——
    /// std 守卫跨 await 会被 `clippy::await_holding_lock` 正当地拦下
    pub(crate) static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    static CLI: std::sync::LazyLock<Mutex<Option<String>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    static PROBE: std::sync::LazyLock<Mutex<Option<ProbeVerdict>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    /// 真探针分支的执行体缝（返工 M-1）：`Some` = 替代真 spawn 的注入执行体
    /// （同步闭包，返回即 verdict；`Arc` 便于按句柄克隆取出）。默认 None = 真 spawn。
    static PROBE_ROUND: std::sync::LazyLock<Mutex<Option<ProbeRoundFn>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));
    static SCRIPT: std::sync::LazyLock<Mutex<VecDeque<TurnObs>>> =
        std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));
    /// spawn 形态缝（**评审 I8**）：默认 None = 按 [`production_exe`] + OS 真算。`spawn_shape`
    /// 只在 `windows` + `.cmd`/`.bat` 上产出垫片形态，此缝让「垫片形态的**执行臂**」在任何
    /// 平台都能被用例钉住（纯判据面本来就不依赖平台）。
    static SHAPE: std::sync::LazyLock<Mutex<Option<SpawnShape>>> =
        std::sync::LazyLock::new(|| Mutex::new(None));

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
        *PROBE_ROUND.lock().unwrap_or_else(|e| e.into_inner()) = None;
        SCRIPT.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *SHAPE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 装填 spawn 形态缝（**调用方必须已持 [`LOCK`]**；None = 恢复按 CLI 发现 + OS 真算）
    pub(crate) fn set_shape(shape: Option<SpawnShape>) {
        *SHAPE.lock().unwrap_or_else(|e| e.into_inner()) = shape;
    }

    /// 取 spawn 形态缝（None = 无注入，真算）
    pub(crate) fn shape() -> Option<SpawnShape> {
        SHAPE.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 装填真探针执行体缝（**调用方必须已持 [`LOCK`]**；None = 清除，恢复真 spawn）
    pub(crate) fn set_probe_round(f: Option<ProbeRoundFn>) {
        *PROBE_ROUND.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// 取真探针执行体缝（None = 无注入，走真 spawn）
    pub(crate) fn probe_round() -> Option<ProbeRoundFn> {
        PROBE_ROUND
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
    // H5 档位枚举（Task 10）：仅测试面直取（生产面只用 `PermissionSpec::codex_exec_default`）
    use crate::inject::headless::CodexApprovalPolicy;
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
    /// 本机 `codex exec resume --help` 选项表里没有 `-C` ⇒ 它只能是顶层全局 flag）。
    /// **H5 策略旗子（Task 10）**：`-c approval_policy=on-request` 紧接 `resume`——
    /// 本机 `codex exec resume --help` 的选项表**列有** `-c, --config <key=value>`，
    /// 且实测 `codex exec resume -c approval_policy=on-request --help` 退出码 0
    /// （对照：`codex exec resume -C E:/t2 --help` ⇒ exit 2 `unexpected argument '-C' found`）
    /// ⇒ 子命令后接受 `-c`，位置照 usage `[OPTIONS] [SESSION_ID] [PROMPT]` 前置。
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
                "-c".into(),
                "approval_policy=on-request".into(),
                UUID.into(),
                "hi [mobile iPad]".into(),
                SKIP_GIT_REPO_CHECK.into(),
            ],
            "exec resume argv 必须逐字对齐 spec H8 + H5 策略旗子（`-C` 全局前置、`-c` 子命令内）"
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
        // H5 策略旗子：子命令**之内**（`resume` 之后）——与 `-C` 的全局前置相反，勿挪
        let resume_idx = argv
            .iter()
            .position(|a| a == "resume")
            .expect("resume 必须在 argv 里");
        let policy = argv
            .iter()
            .position(|a| a == "-c")
            .expect("H5 策略旗子必须在");
        assert!(
            policy > resume_idx,
            "`-c` 下标（{policy}）必须大于子命令 `resume` 下标（{resume_idx}）——它是子命令内选项"
        );
        assert_eq!(
            argv[policy + 1],
            "approval_policy=on-request",
            "策略旗子必须带默认档 on-request"
        );
        assert_eq!(p.kind(), PlanKind::ExecResume);
    }

    /// H5 边界落码（H8 定案）：**queue 通道没有审批面**——`queue` 只是把消息入队的短命
    /// 进程，turn 执行（含工具审批）归 codex APP 自身、按 APP 自己的配置走 ⇒ MAM 侧没有
    /// 可注入的权限档旗子。本测钉死这条边界（防「顺手也给 queue 加一个 -c/--mode」）。
    #[test]
    fn queue_plan_carries_no_permission_flag() {
        let p = super::dispatch(AppPresence::Open, UUID, "E:/t2", "hi [mobile iPad]");
        let argv = p.argv();
        assert!(
            !argv.iter().any(|a| matches!(a.as_str(), "-c" | "--mode")),
            "queue 通道无审批面（H8 定案）——不得注入权限档旗子: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("approval_policy")),
            "queue 不得携带审批策略: {argv:?}"
        );
    }

    /// H5 策略档**参数化**：exec resume 的旗子随 [`PermissionSpec`] 变（本轮只 spawn 选档 +
    /// 展示；改档重试属后续批次）；**默认档恒 on-request**（dispatch 用的那条）
    #[test]
    fn exec_resume_policy_flag_follows_the_spec() {
        let alt = super::exec_resume_argv(
            "E:/t2",
            UUID,
            "hi",
            PermissionSpec::CodexExec(CodexApprovalPolicy::Never),
        );
        assert!(
            alt.windows(2).any(|w| w == ["-c", "approval_policy=never"]),
            "换档必须换旗子（策略驱动）: {alt:?}"
        );
        let d = super::dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        assert!(
            d.argv()
                .windows(2)
                .any(|w| w == ["-c", "approval_policy=on-request"]),
            "分派默认档必须 on-request: {:?}",
            d.argv()
        );
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

    // ===== 项目路径面：进 argv 的载荷分段 × cmd 垫片（评审 I8，纯核） =====

    /// 垫片形态的真实产物（[`spawn_shape`] 单点）——不手抄「垫片」字面量，形态判据只有一份
    fn shim_fixture() -> SpawnShape {
        let s = spawn_shape(r"C:\Users\u\AppData\Roaming\npm\codex.cmd", "windows");
        assert!(
            s.via_cmd_shim(),
            "夹具前提：windows + .cmd 必须产出垫片形态"
        );
        s
    }

    /// 直连 exe 的真实产物（对照：不过 cmd，CRT 与 CreateProcess 同规则 ⇒ 无重解析面）
    fn direct_fixture() -> SpawnShape {
        let s = spawn_shape("C:/bin/codex.exe", "windows");
        assert!(!s.via_cmd_shim(), "夹具前提：.exe 必须是直 spawn 形态");
        s
    }

    /// **评审 I8**：垫片形态 + `-C` 取值含元字符 ⇒ 拒。argv 取自**真组装器**
    /// [`dispatch`]（不手抄字面量——将来 argv 形态一变，`-C` 取值断言先红）；
    /// 文案与正文面**逐字相同**（同源判定：同一个函数、同一份字符表）。
    #[test]
    fn shim_shape_refuses_metachar_project_path_in_the_real_argv() {
        let dirty = r"C:\work\A&B%TEMP%";
        let plan = dispatch(AppPresence::Closed, UUID, dirty, "hi [mobile iPad]");
        let argv = plan.argv();
        assert_eq!(argv[2], dirty, "前提：`-C` 的取值就是项目路径（真组装器）");
        let shim = shim_fixture();
        let reason = argv_shim_refusal(&shim, argv).expect("垫片 + 含元字符的 -C 必须拒");
        assert_eq!(
            Some(&reason),
            crate::inject::headless::turn::cmd_shim_refusal(
                crate::inject::headless::turn::CmdShimSubject::ProjectPath,
                dirty,
                &shim,
                true,
            )
            .as_ref(),
            "项目路径面的判定与文案必须与判据单点同源（不另写第二套措辞）"
        );
        // **文案诚实化（评审 I8 追补）**：必须点名**项目路径**这一段——说「正文含…」
        // 是错报对象（用户会去改正文，而真凶在项目路径上，改多少遍都没用）
        assert!(
            reason.contains("项目路径（-C 取值）含 cmd 元字符"),
            "必须点名实际命中的那一段：{reason}"
        );
        assert!(
            !reason.contains("正文含 cmd 元字符"),
            "不得把项目路径的命中说成正文命中（错报对象）：{reason}"
        );
        for c in ['&', '%'] {
            assert!(
                reason.contains(c),
                "拒绝原因必须点名具体元字符 {c}：{reason}"
            );
        }
        assert!(reason.contains("cmd"), "必须说清是 cmd 重解析面：{reason}");
        assert!(reason.contains("未投递"), "必须声明零字节投递：{reason}");
    }

    /// 对照：**非垫片形态**（直连 exe）下同一条含元字符的项目路径必须**照旧放行**
    /// ——CRT 引号/转义规则与 CreateProcess 一致，没有 cmd 重解析面，不得误拒。
    #[test]
    fn direct_exe_shape_allows_the_same_metachar_project_path() {
        let dirty = r"C:\work\A&B%TEMP%";
        let plan = dispatch(AppPresence::Closed, UUID, dirty, "hi [mobile iPad]");
        assert_eq!(
            argv_shim_refusal(&direct_fixture(), plan.argv()),
            None,
            "直连 exe 不过 cmd：含 `&`/`%` 的项目路径必须照旧放行"
        );
        assert_eq!(
            argv_shim_refusal(&spawn_shape("codex", "macos"), plan.argv()),
            None,
            "非 Windows 形态同样不过 cmd"
        );
    }

    /// 垫片形态 + **干净路径** ⇒ 放行；且判据面是**全部载荷分段**（不只 `-C`）：
    /// queue 形态的 `--message` 取值同样在面内（正文面的**纵深防御**，不是第二判据——
    /// 判定委托同一个函数）。
    #[test]
    fn shim_shape_allows_clean_plan_but_judges_every_payload_segment() {
        let shim = shim_fixture();
        let clean = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi [mobile iPad]");
        assert_eq!(
            argv_shim_refusal(&shim, clean.argv()),
            None,
            "干净项目路径 + 干净载荷在垫片形态下必须放行"
        );
        let queue = dispatch(AppPresence::Open, UUID, "E:/t2", "hi & whoami %PATH%");
        assert!(
            argv_shim_refusal(&shim, queue.argv()).is_some(),
            "判据面 = argv[1..] 全段：queue 的 --message 取值也在内（纵深防御）"
        );
        assert_eq!(
            argv_shim_refusal(&shim, queue.argv()),
            crate::inject::headless::turn::cmd_shim_refusal(
                crate::inject::headless::turn::CmdShimSubject::ArgvSegment,
                "hi & whoami %PATH%",
                &shim,
                true,
            ),
            "逐段委托 ⇒ 与判据单点同源同文案（这一段不是 `-C` 取值 ⇒ 泛指「命令行分段」）"
        );
    }

    /// **评审 N2**：主语判据不得被「正文恰为字面量 `"-C"`」伪造——旧的「前一段是不是
    /// `-C`」启发式会把正文 `"-C"` 的后邻段（`--skip-git-repo-check`）误标成
    /// 「项目路径（-C 取值）」。argv 取自**真组装器** [`dispatch`]；主语判据的原料
    /// [`segment_subject`] 是纯函数，直接钉死（真实 argv 里后邻段无元字符 ⇒ 误标无可见
    /// 文案，红绿面钉在判据本身）。行为级对照：正文 `"-C"` 自身无元字符 ⇒ 本场景整体
    /// 放行（按真实判据断言，不造不存在的拒绝）。
    #[test]
    fn argv_subject_not_spoofed_by_literal_dash_c_body() {
        use crate::inject::headless::turn::CmdShimSubject;
        // exec 形态：正文（PROMPT 位置参数）恰为字面量 "-C"，项目路径干净
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "-C");
        let argv = plan.argv();
        let skip_at = argv
            .iter()
            .position(|a| a == SKIP_GIT_REPO_CHECK)
            .expect("真组装器：收尾旗子必在");
        assert_eq!(
            argv[skip_at - 1],
            "-C",
            "夹具前提：正文 '-C' 紧邻收尾旗子（正是旧启发式的误标对象）"
        );
        assert_eq!(
            segment_subject(argv, skip_at),
            CmdShimSubject::ArgvSegment,
            "正文 '-C' 不是旗子：其后邻段不得被误标为项目路径（评审 N2 主红面）"
        );
        assert_eq!(
            segment_subject(argv, 2),
            CmdShimSubject::ProjectPath,
            "位置契约不漏判：argv[1] 是构造器钉死的 '-C' 旗子 ⇒ argv[2] 仍正确标项目路径"
        );
        // 行为级对照：本场景（正文 "-C" 无元字符、项目路径干净）整体放行
        assert_eq!(
            argv_shim_refusal(&shim_fixture(), argv),
            None,
            "正文 '-C' 自身无元字符 ⇒ 无拒绝面，照实放行（不造不存在的拒绝）"
        );
        // queue 形态：正文段（`--message` 取值）主语照旧走兜底面（正文门另有专门文案）
        let queue = dispatch(AppPresence::Open, UUID, "E:/t2", "-C");
        let qargv = queue.argv();
        let msg_at = qargv
            .iter()
            .position(|a| a == CODEX_MESSAGE_FLAG)
            .expect("真组装器：--message 必在")
            + 1;
        assert_eq!(qargv[msg_at], "-C", "夹具前提：queue 正文也是 '-C'");
        assert_eq!(
            segment_subject(qargv, msg_at),
            CmdShimSubject::ArgvSegment,
            "正文段（--message 取值）照旧走兜底面（正文面已有专门之门，此处不另写文案）"
        );
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

    /// **评审 M3：大小写口径的唯一单点**——`hits_writer_lock_marker` 是两处判据
    /// （本模块的 stdout/stderr 头扫描、`codex_app::classify` 的错误帧 message）共用的
    /// **同一份**比对。上游服务端原文的大小写不受我们控制（版本间也会漂），判据必须宽容；
    /// 两处各写一份就必然漂移（旧 codex_app 口径是大小写敏感）。
    ///
    /// 变异验证：把 `hits_writer_lock_marker` 改回 `text.contains(WRITER_LOCK_MARKER)`
    /// （即 codex_app 的旧口径）→ 本用例的大小写变体三条红。
    #[test]
    fn writer_lock_marker_matching_is_case_insensitive_single_point() {
        assert!(hits_writer_lock_marker(WRITER_LOCK_MARKER));
        assert!(hits_writer_lock_marker(
            &WRITER_LOCK_MARKER.to_ascii_uppercase()
        ));
        assert!(hits_writer_lock_marker(
            "Thread x Already Has An Active Writer"
        ));
        assert!(
            !hits_writer_lock_marker("already has an active reader"),
            "近似串不得命中（判据不得放宽成「含 writer」）"
        );
        // 头扫描与它同口径（同一函数，不是两份实现）
        assert!(writer_lock_evidence("", "ALREADY HAS AN ACTIVE WRITER").is_some());
        assert!(writer_lock_evidence("Already Has An Active Writer", "").is_some());
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

    /// **codex 特例：APP 持 WAL 写锁故走副本（裁决 20 豁免项；WB 已改只读直连）**：队列副本
    /// 读只碰副本，源库字节原样（本机实测 APP 持 WAL 写锁）；副本读完即清（逐个文件删，
    /// 不做递归删除）
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
            "源库字节必须原样（codex 特例：只读副本，不直查活库）"
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

    /// spawn 形态缝收尾守卫（panic 也清——形态缝是进程级全局态，评审 I8 的执行臂用例用）
    struct ShapeHookClear;
    impl Drop for ShapeHookClear {
        fn drop(&mut self) {
            test_hooks::set_shape(None);
        }
    }

    /// **评审 I4**：queue 回执的 `durationMs` 必须**含消费等待**——模块头声明的口径是
    /// 「整回合墙钟（含消费等待）」，旧实现由 `run_queue_turn` 在 `await` 前把
    /// `total_ms()` 求值成一个冻结的 `u64` 传进 `consume_and_report`，于是消费等待
    /// （生产最多 60s）全部漏计，审计行 `duration_ms` 同步少报。
    ///
    /// 本用例把等待缝换成**真睡觉 + 记账**，覆盖全部出口：已消费 / 四个未消费态 /
    /// 取消。变异验证：把调用点改回 `total_ms()`（await 前求值）→ 全部断言红。
    #[tokio::test]
    async fn queue_receipt_durations_include_the_consume_wait() {
        const WAIT_MS: u64 = 25;
        // 真睡觉的消费等待缝（每次 wait 记一笔并真睡 WAIT_MS）
        fn sleeping_deps(
            hits: Vec<Option<bool>>,
            holds: Option<QueueEvidence>,
        ) -> (TurnDeps, Arc<Mutex<u64>>) {
            let mut d = deps_with(hits, holds);
            let slept: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
            let acc = slept.clone();
            d.wait = Box::new(move |_| {
                let acc = acc.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(WAIT_MS)).await;
                    *acc.lock().unwrap_or_else(|e| e.into_inner()) += WAIT_MS;
                })
            });
            (d, slept)
        }
        let plan = dispatch(AppPresence::Open, UUID, "E:/t2", "hi [mobile iPad]");
        let slept_of = |s: &Arc<Mutex<u64>>| *s.lock().unwrap_or_else(|e| e.into_inner());

        // ① 已消费出口（第 2 轮追加命中——第 1 轮后真等过一次）
        let (run, _) = scripted_run(vec![queued_obs("m-1")]);
        let (_, make) = recording_runner();
        let (deps, slept) = sleeping_deps(vec![Some(false), Some(true)], Some(QueueEvidence::Ours));
        let out = run_turn(
            &args_of("sess_i4_consumed", Some(ROLLOUT)),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert!(slept_of(&slept) > 0, "本用例必须真的等过（否则断言空转）");
        assert!(
            out.receipt.duration_ms >= slept_of(&slept),
            "已消费出口：耗时 {}ms 必须 ≥ 消费等待 {}ms",
            out.receipt.duration_ms,
            slept_of(&slept)
        );

        // ② 四个未消费出口（三态 + 副本不可读）：全程等待都发生在回执构造之前
        for (holds, label) in [
            (Some(QueueEvidence::Ours), "队列仍滞留本条目"),
            (
                Some(QueueEvidence::ThreadStalled { rows: 3 }),
                "thread 有滞留",
            ),
            (Some(QueueEvidence::ThreadClear), "thread 无滞留"),
            (None, "副本不可读"),
        ] {
            let (run, _) = scripted_run(vec![queued_obs("m-2")]);
            let (_, make) = recording_runner();
            let (deps, slept) = sleeping_deps(vec![Some(false), Some(false), Some(false)], holds);
            let out = run_turn(
                &args_of("sess_i4_queued", Some(ROLLOUT)),
                &plan,
                &make,
                &deps,
                &*run,
            )
            .await;
            assert_eq!(out.receipt.status, ReceiptStatus::Queued, "{label}");
            assert!(slept_of(&slept) > 0, "{label}：必须真的等过");
            assert!(
                out.receipt.duration_ms >= slept_of(&slept),
                "{label}：耗时 {}ms 必须 ≥ 消费等待 {}ms",
                out.receipt.duration_ms,
                slept_of(&slept)
            );
        }

        // ③ 取消出口（等待途中移动端叫停）：回执耗时同样必须含已发生的等待
        let sid = "sess_i4_cancelled";
        // 槽位按生产形态先占住（api.rs 分派臂先 begin，取消端点才找得到靶子）
        assert!(registry().begin(sid, TurnSlot::placeholder("codex", "hi".to_string())));
        let (run, _) = scripted_run(vec![queued_obs("m-3")]);
        let (_, make) = recording_runner();
        let mut deps = deps_with(vec![Some(false), Some(false)], Some(QueueEvidence::Ours));
        let slept: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let acc = slept.clone();
        deps.wait = Box::new(move |_| {
            let acc = acc.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(WAIT_MS)).await;
                *acc.lock().unwrap_or_else(|e| e.into_inner()) += WAIT_MS;
                // 等待窗内送达取消（消费等待期已武装靶子 → 闩 + 中止）
                let _ = registry().request_cancel(sid);
            })
        });
        let out = run_turn(&args_of(sid, Some(ROLLOUT)), &plan, &make, &deps, &*run).await;
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "{:?}",
            out.receipt
        );
        assert!(slept_of(&slept) > 0);
        assert!(
            out.receipt.duration_ms >= slept_of(&slept),
            "取消出口：耗时 {}ms 必须 ≥ 消费等待 {}ms",
            out.receipt.duration_ms,
            slept_of(&slept)
        );
        registry().end(sid);
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

    /// Task 6b review M-3：exec 计划被单写者锁拒 → 改道 queue 落定进入消费等待 ⇒
    /// 槽位通道同步为 queue（取消端点按槽位通道词分叉——不同步则改道腿的取消拿
    /// 不到「queue 滞留」撤回缺失明示）。占位形态模拟 api.rs 分派臂（计划通道 =
    /// exec），断言在消费等待入口即已翻转（等待期内取消才看得到）
    #[tokio::test]
    async fn divert_to_queue_updates_slot_channel_for_cancel_branching() {
        use crate::inject::routing::HeadlessKind;
        let sid = "sess_div_ch_6b";
        assert!(registry().begin(
            sid,
            TurnSlot::placeholder("codex", "hi".to_string())
                .with_channel(HeadlessKind::CodexExec.wire_name()),
        ));
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (run, _) = scripted_run(vec![
            // ① exec resume 被单写者锁拒（非 0 退出 + 锁证据 → 改道判据成立）
            obs_exit(1, &[], "Error: -32600 already has an active writer"),
            // ② 改道 queue：入队回执（exit 0 + Queued message）
            queued_obs("m-div-ch"),
        ]);
        let (_a, make) = recording_runner();
        let deps = deps_with(vec![Some(false)], Some(QueueEvidence::Ours));
        let out = run_turn(&args_of(sid, Some(ROLLOUT)), &plan, &make, &deps, &*run).await;
        assert!(
            out.diverted,
            "先证明确实走了改道腿：{:?}",
            out.receipt.reason
        );
        let snap = registry()
            .cancel_snapshot(sid)
            .expect("槽位仍在（注销归 api 分派臂，内核不注销）");
        assert_eq!(
            snap.channel,
            HeadlessKind::CodexQueue.wire_name(),
            "改道落定进入消费等待 → 槽位通道同步（取消分叉判据）"
        );
        registry().end(sid);
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

    /// **评审 I8（执行臂）**：垫片形态 + `-C` 取值含元字符 ⇒ **投递前拒绝**——回执档
    /// `Stage::Refused`（与正文面/花名面同档同文案）、`make_runner` 与执行缝**都没被调用**
    /// （零回合配置、零进程、零名额、零字节投递），也不得被误当成「可改道的失败」。
    ///
    /// 形态经 [`test_hooks::set_shape`] 注入：`spawn_shape` 只在 `windows` + `.cmd`/`.bat`
    /// 上产出垫片形态，注入面让**执行臂**在任何平台都跑得到；argv 取自**真组装器**
    /// [`dispatch`]，文案与正文面**逐字**比对（同源判定的字面证据）。
    #[tokio::test]
    async fn shim_metachar_project_path_is_refused_before_any_runner_is_built() {
        let _serial = test_hooks::LOCK.lock().await;
        let shim = shim_fixture();
        test_hooks::set_shape(Some(shim.clone()));
        let _clear = ShapeHookClear; // panic 也清形态缝
        const SID: &str = "sess_i8_shim_refuse";
        let dirty = r"C:\work\A&B%TEMP%";
        let plan = dispatch(AppPresence::Closed, UUID, dirty, "hi");
        assert_eq!(plan.argv()[2], dirty, "前提：`-C` 的取值就是项目路径");
        let (seen, make) = recording_runner();
        let (run, calls) = scripted_run(vec![queued_obs("m-must-not-run")]);
        let deps = deps_with(vec![Some(true)], None);
        let out = run_turn(&args_of(SID, Some(ROLLOUT)), &plan, &make, &deps, &*run).await;
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Failed,
            "{:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt.stage,
            Some(Stage::Refused),
            "投递前拒绝档必须是 refused（未起跑、零字节投递）：{:?}",
            out.receipt
        );
        assert_eq!(out.receipt.session_id, SID, "回执必须带会话号");
        assert_eq!(out.plan_used, PlanKind::ExecResume);
        assert_eq!(out.receipt_source, ReceiptSource::NotApplicable);
        assert!(!out.diverted, "零尝试的拒绝不是「可改道的失败」");
        assert_eq!(
            out.receipt.reason,
            crate::inject::headless::turn::cmd_shim_refusal(
                crate::inject::headless::turn::CmdShimSubject::ProjectPath,
                dirty,
                &shim,
                true,
            ),
            "项目路径面的文案必须与判据单点**同源同句**（不另写一套措辞）"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "拒绝臂必须零投递（执行缝未被调用）"
        );
        let recorded = seen.lock().unwrap().len();
        assert_eq!(recorded, 0, "拒绝臂必须零回合配置（make_runner 未被调用）");
    }

    /// 对照（执行臂）：同一垫片形态 + **干净项目路径** ⇒ 照旧放行并**真的跑到**执行缝
    /// （防「垫片形态一律拒」式的误拒）。
    #[tokio::test]
    async fn shim_clean_project_path_still_reaches_the_runner() {
        let _serial = test_hooks::LOCK.lock().await;
        test_hooks::set_shape(Some(shim_fixture()));
        let _clear = ShapeHookClear;
        let plan = dispatch(AppPresence::Closed, UUID, "E:/t2", "hi");
        let (seen, make) = recording_runner();
        let (run, calls) = scripted_run(vec![obs_exit(0, &["最终回复"], "")]);
        let deps = deps_with(vec![], None);
        let out = run_turn(
            &args_of("sess_i8_shim_pass", None),
            &plan,
            &make,
            &deps,
            &*run,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt_source, ReceiptSource::ExecStdout);
        assert_eq!(*calls.lock().unwrap(), 1, "干净路径必须照旧投递一次");
        let argv = seen.lock().unwrap()[0].clone();
        assert_eq!(argv[2], "E:/t2", "实跑 argv 的 `-C` 取值：{argv:?}");
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

    // ============================================================
    // Task 2 返工（spec 审 M-1/M-2）：probe_cli duration 回填 + 超时文案单一源
    // ============================================================

    /// **M-1**：真探针分支起止计时——结论携带实测墙钟（≥ 注入执行体的 20ms 行程），
    /// 且 verdict 本体不被计时改写——api.rs:2155 审计行的数据源就是它（旧态恒「· 0ms」）。
    /// 缝：`test_hooks::probe_round()` 替代 spawn 执行体（缓存/exe 在场性检查仍走真路径）。
    #[tokio::test]
    async fn probe_cli_real_branch_carries_wallclock_duration() {
        let _guard = test_hooks::LOCK.lock().await;
        test_hooks::clear();
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("codex-probe-stub.exe");
        std::fs::write(&exe, "stub").unwrap();
        let exe_s = exe.to_string_lossy().to_string();
        test_hooks::set_probe_round(Some(std::sync::Arc::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            ProbeVerdict::fail("round 注入：模拟真探针行程")
        })));
        let v = probe_cli(&exe_s, &ProbeCache::default(), "windows").await;
        test_hooks::set_probe_round(None);
        assert!(!v.is_pass());
        assert!(
            v.duration_ms() >= 20,
            "真分支结论必须携带实测墙钟: {}",
            v.duration_ms()
        );
        assert_eq!(
            v.hint(),
            Some("round 注入：模拟真探针行程"),
            "verdict 本体不得被计时回填改写"
        );
    }

    /// **M-2**：超时 hint 复用 gate 的单一文案源（秒数从常量生成）——
    /// 旧字面「15s」与「可能挂死」误导归因必须退场。
    #[test]
    fn probe_timeout_hint_drops_stale_15s_and_hang_blame() {
        let receipt = Receipt::failed(Stage::Timeout, "watchdog 到点");
        let cfg = RunnerCfg::for_test();
        let v = probe_cli_verdict(&receipt, &cfg, "codex");
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(!hint.contains("15s"), "旧写死秒数不得在场: {hint}");
        assert!(!hint.contains("挂死"), "误导归因必须退场: {hint}");
        assert!(hint.contains("网络/模型服务"), "{hint}");
        assert!(
            hint.contains(&format!("{}s", super::super::gate::PROBE_TIMEOUT_MS / 1000)),
            "秒数从常量生成（阈值改文案跟着走）: {hint}"
        );
    }
}
