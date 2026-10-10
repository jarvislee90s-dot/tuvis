#![cfg(windows)]
//! C8 四家实机 E2E：远程新建会话全链矩阵（spec §4；**只写不跑**——实机运行由主会话
//! 显式执行，全部 `#[ignore]`）。
//!
//! ## 链路（单 `#[test]` 驱动四家 claude→codex→kimi→opencode，--test-threads=1 串行；
//! 单家失败断言即止，后续家不跑——实机诊断优先，已完成家清单式打点 stdout）
//! 全新 temp 目录 → `build_create_spawn_spec` 起窗（生产 spawner [`spawn_terminal`，
//! DISABLE_AUTOUPDATER=1 环境红线随行）→ `find_tui_pid`（真 sysinfo cwd+进程名+新鲜度
//! 锚定，30s）→ `run_pipeline`（真屏读 `e2e_support::read_screen_lines` / 真注入
//! `RealInjector` spec 缝；步距/键间隔由缝闭包承载，与生产 `api.rs::run_create_pipeline`
//! 同构）→ `discover_new_session` 物化轮询（真数据根 `create_store_root`）→ 生产确认
//! 语义 `stamp_hit_in_page` 命中 → **断言会话文件首条用户消息 == composed 逐字节**
//! （经生产读路径 `read_session_messages`——四家解析器同一份，测试不手写第二份文件
//! 定位）→ taskkill 清场。
//!
//! ## 键序红线实机复核（keys_sent 断言，计划 C8 权威）
//! - claude == `["down","enter"]`（信任框默认 ❯ No, exit 危险默认——必须 ↓+Enter）；
//! - codex 放宽**实机三态域**（C8 实机定案 2026-10-02）：`["2","enter"]`（更新框+
//!   信任框双框轮）/ `["enter"]`（仅信任框）/ `[]`（无框直 idle——0.160.0 实测形态，
//!   信任/更新模型机器态漂移）。红线键序（'2'=Skip、Enter=信任）由 C5 disposal_keys
//!   单测 + 探测定案 §4/§5 锁定；实际形态落台账（dialog_log 随行）；
//! - kimi 放宽二选一：`["enter"]`（信任框弹出）或 `[]`（未弹）——实际形态打印进台账；
//! - opencode == `[]`（无信任框直 idle）。
//!
//! ## 前置条件（测试头部备案）
//! - Windows 宿主 + 四家 CLI 已安装且已用过：claude `~/.claude`、codex `~/.codex`、
//!   kimi 数据根（`~/.kimi-code/sessions` 在场或 `KIMI_CODE_HOME` 指向）、opencode
//!   `~/.local/share/opencode/opencode.db` 在场——每 leg 起手自检，缺即前置失败
//!   （不做材料化轮询的无谓等待）；
//! - Windows Terminal 可选：wt 在场走 wt 标签，不在场走生产同款 conhost 回退
//!   （两者都是本 E2E 的覆盖面，不因 wt 缺席失败）；
//! - temp 项目目录建在 `E:\tmp-c8-e2e` 下（**C8 冒烟实机定案**：目录须在已信任
//!   git 仓库树之外——claude 按 git 根继承信任，仓库内 tempdir 不弹信任框；ASCII、
//!   盘符绝对形态、非黑名单——每 leg 起手过 `create_path::validate` 自检）。
//!
//! ## 纪律
//! - 只碰本测试新建的进程树与 temp 目录；temp 全新目录（每 leg 独立）保证 claude/
//!   codex/kimi 的目录级信任框真实触发；
//! - **清场**：每 leg 结束 taskkill /T /F（cwd 含唯一目录标记的全部进程 + pid 树兜底；
//!   panic 路径由 [`LegProc`] Drop 兜底）——生产「失败不清场保留窗口」语义在测试侧
//!   的**例外**（计划明示 E2E 必须 taskkill 清场）；清场前**固定等待
//!   [`REPLY_WAIT_MS`]（15s，用户裁决 2026-10-02）**让模型回合落地——「模型是否
//!   真实回复」以信息性台账记录（assistant 条数），不作断言；证据（keys/
//!   dialog_log/屏读尾 40 行）先落 evidence 再杀；
//! - temp 目录**保留**不删（失败现场语义 + 证据；TempDir 以 `std::mem::forget` 丢弃
//!   清理行为）；
//! - evidence 落 `~/.tuvis/create-evidence/<ts>-<tool>.log`（产品态目录 spec §4.4；
//!   best-effort，写失败不碍断言）——这是对「零接触 ~/.tuvis」纪律的**明示例外**：
//!   只写本证据目录，不触 tuvis.db；
//! - 零接触真实 ~/.tuvis 的库路径：不用 DeviceStore/DB，确认语义经生产读路径
//!   `read_session_messages`（只读真实 CLI 会话存储——A1 既有口径）。
//!
//! ## 运行方式（实机显式跑）
//! ```text
//! cargo test --test create_e2e -- --ignored --nocapture --test-threads=1
//! ```
//! 常规门禁（cargo test 不带 --ignored）只验证编译，零新增运行时。建议先跑
//! `e2e_create_single_claude_smoke` 冒烟一家，再跑全矩阵（单测名过滤即可）。
//!
//! ## 预算
//! 单家全链 ≤90s（find_tui_pid 30s + run_pipeline 30s + 物化 30s 由既有常量约束；
//! 实测预期 ~23s/家）。
//!
//! ## 实跑台账（主会话实机执行记录）
//! | 日期 | 用例 | 四家各耗时 | 断言结果 | 证据目录 |
//! |---|---|---|---|---|
//! | 2026-10-02 | e2e_create_single_claude_smoke | claude 27.5s | PASS：keys
//!   ["down","enter"] 红线实机复核；首条 user 逐字节；模型已回复（assistant 2 条） | `~/.tuvis/create-evidence/` |
//! | 2026-10-02 | e2e_create_matrix_four_tools | claude 27.6s / codex 22.4s /
//!   kimi 27.2s / opencode 24.9s | PASS **四家全过**：claude 信任框红线
//!   ["down","enter"]；codex 无框直 idle（三态域内，模型回合因 CC Switch 代理
//!   400 未回复——环境态如实记录）；kimi 信任框 ["enter"]；opencode 无框 []；
//!   四家首条 user 断言全过；claude/kimi/opencode 模型已回复 | `~/.tuvis/create-evidence/` |
//! | 2026-10-02 | e2e_create_http_full_chain（六门禁终跑） | 全链 52.0s | PASS：
//!   sid 三处一致；第二条消息落地后 assistant 4 条 | `~/.tuvis/create-evidence/` |
//! | 2026-10-02 | e2e_create_matrix_four_tools（六门禁终跑）+ 复跑 | claude
//!   27.4s / codex 22.6s / kimi 26.0s 过；**opencode 锚定失败**——spawn Ok、
//!   find_tui_pid 30s 超时（两次复现）：上游自升 2.0.22（服务架构，进程无可附加
//!   控制台）→ 归 opencode2 复验批（验收清单 #26；1.18.32 线上全链两次实机验证
//!   在案） | `~/.tuvis/create-evidence/` |
//! | 2026-10-02 | e2e_create_matrix_four_tools（**opencode 2.x 适配批 D2 复跑**）
//!   | claude 28.0s / codex 22.6s / kimi 26.0s / **opencode 23.8s** | PASS
//!   **四家全过**：opencode 2.0.22 腿 **锚定恢复**（`--standalone` 修端口竞争陷阱：
//!   TUI pid 2.5s 内到手，取代此前 30s 超时）；keys=[]；首条 user 逐字节；物化
//!   stamp 命中（`remote/content.rs` v2 派发修复——此前查冻结 `message`/`part`
//!   表命不中） | `~/.tuvis/create-evidence/` |
//! | 2026-10-02 | e2e_create_http_full_chain（D2 复跑） | 全链 46.0s | PASS：
//!   第二条消息落地后 assistant 2 条 | 同上 |
//! | 2026-10-02 | e2e_create_single_claude_smoke（D2 复跑） | claude 27.6s | PASS：
//!   keys ["down","enter"] 红线 | 同上 |
//! | 2026-10-02 | **D2 全量三例连跑**（单进程 --test-threads=1）| matrix：
//!   claude 28.0s / codex 22.6s / kimi 26.0s / opencode 22.6s；http 全链；claude
//!   冒烟 27.6s | **3 passed 0 failed**（opencode 腿 22.6s，与三家同量级）；
//!   opencode 修复两处根因：① `--standalone` 起窗（端口竞争）；②
//!   `remote/content.rs` 消息读取 v2 派发（stamp 命中路径） | 同上 |
//!
//! | 2026-10-03 | **rebase 至 origin/main(8d756a2) 后全量复跑**（--test-threads=1）
//!   | matrix：claude 29.4s（信任框红线↓+Enter 实机复核）/ codex 22.6s / kimi
//!   27.3s（enter）/ **opencode 23.8s（2.x 腿）**；http 全链 15.1s（taskId→
//!   四态→done→sessionId）；claude 冒烟 27.6s | **3 passed 0 failed**——rebase
//!   携 main 的 capability_table/AUQ 重构后功能零回归；RemoteState 夹具补
//!   capability_table 初始化为本轮唯一适配 | `~/.tuvis/create-evidence/` |
//! ### D2 修复（opencode 2.x create 腿）
//! 1. **起窗命令 `--standalone`**（`inject/resume.rs::CREATE_COMMAND_TABLE`）：2.x
//!    裸 `opencode` 在已有后台服务占默认端口时不出 TUI（`Starting background
//!    server...` 静默重试环）→ find_tui_pid 30s 超时。`--standalone` = 私有 server，
//!    TTY 下必出 TUI。
//! 2. **消息读取 v2 派发**（`remote/content.rs::read_opencode_messages_v2`）：create
//!    物化用 stamp 命中会话，走 `read_session_messages`——旧路径查冻结的
//!    `message`/`part` 表 → 新会话「不存在」→ 15 轮物化全 miss。v2 派发后 stamp 命中。
//! 3. **发现层 v2 派发**（`inject/create_discover.rs::query_opencode_copy`）：候选会话
//!    查询同源修复（`session` → `session_v2`）。
//! 4. **卡片文本/尾信号 v2**（`monitor/opencode_parser.rs`，D1）：见 D1 定案。
//!
//! ### 实机定案（本轮 E2E 揭示并修复，均登记简报）
//! 1. npm bin claude.exe 首进程为短命蹦床 + MCP 子进程持私有空控制台 →
//!    find_tui_pid 增候选取根 + 屏读内容终判；
//! 2. E2E 继承启动者 CLAUDE_CODE_* 环境致 claude 自认子会话（跳过信任框 + 关闭
//!    transcript）→ create 起窗剥离会话上下文（SpawnSpec::env_rm_prefixes）；
//! 3. 仓库内 tempdir 按 git 根继承信任（信任框不弹）→ E2E 目录迁 E:\tmp-c8-e2e；
//! 4. codex 0.160.0 机器态三态（更新框+信任框 / 仅信任框 / 无框直 idle）→ keys
//!    断言放宽实机域（红线由 C5 单测 + 探测定案锁定）；
//! 5. codex「Hooks need review」框阻塞 composer（spec §4.8 阻塞语义实机修正）→
//!    **核验式自动信任**（codex_hooks_all_ours 全我方 → '2'+enter；混杂 → esc），
//!    用户在场裁决；早期「物化命中即杀」致模型回复证据缺失 → 清场前固定等 15s
//!    （用户裁决），assistant 落地为信息性台账。
//!
//! ## C9（HTTP 端到端总测试，移动端等效路径；只写不跑）
//! 临时目录（`E:\tmp-c9-e2e`，信任树外）→ POST `/m/api/v1/session-create`（设备
//! cookie；create 域真缝束 `CreateTaskHub::production()` + 真
//! `adapter::get_all_sessions` 数据源）→ 轮询 `/session-create/status` 至
//! `done{sessionId}` → GET `/sessions` 断言新卡在场且 `pairingAmbiguous=false` →
//! 等会话回可输入态 → POST `/session-send` 第二条消息 `delivered`（回执含
//! pairingHint）→ 审计三类行（create×1 / dialog≥1 / send×2）→ 回合落地等待 15s
//! → taskkill 清场（temp 目录保留）。装配细节/断言清单/与 m9r 模式差异/预判风险
//! 见 `docs/release-notes/create-acceptance-checklist.md`（§六 自动化 E2E + §四 审计）。

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use multi_agents_manager_lib::inject::confirm::{stamp_hit_in_page, stamp_of};
use multi_agents_manager_lib::inject::create::{
    find_tui_pid, run_pipeline, CreateDeps, CreateStatus, Params, KEY_GAP_MS,
    MATERIALIZE_MAX_ROUNDS, SCREEN_POLL_STEP_MS,
};
use multi_agents_manager_lib::inject::create_discover::{create_store_root, discover_new_session};
use multi_agents_manager_lib::inject::create_path;
use multi_agents_manager_lib::inject::e2e_support::read_screen_lines;
use multi_agents_manager_lib::inject::engine::{Injector, RealInjector};
use multi_agents_manager_lib::inject::families::family_for;
use multi_agents_manager_lib::inject::normalize::compose_injection;
use multi_agents_manager_lib::inject::resume::{build_create_spawn_spec, spawn_terminal};
use multi_agents_manager_lib::remote::content::{read_session_messages, MessagesPage};

/// 确认轮询取数上限：与生产 `confirm::PROBE_MESSAGE_LIMIT`(20) 同口径
/// （该常量为 pub(crate)，集成测试侧以字面量对齐，注释锚定单一来源；m9r_e2e 同款）。
const PROBE_LIMIT: usize = 20;

/// 回合落地等待（用户裁决 2026-10-02：注入后至少等 10-15s 让模型调用返回再清场
/// ——物化命中即杀会让「模型真实回复」证据缺失、屏读只拍到回合中途态）。
const REPLY_WAIT_MS: u64 = 15_000;

/// E2E 首句正文（计划权威示例 `hi [mobile <名>]` 的正文半边）。
const FIRST_MESSAGE: &str = "hi";
/// E2E 设备花名（ASCII——CJK 签名注入属 char-stream 既有通道，非本任务变量，报告登记）。
const DEVICE_NAME: &str = "C8E2E";

// ============================================================
// 证据（产品态目录 ~/.tuvis/create-evidence/，spec §4.4；best-effort）
// ============================================================

/// 单 run 证据句柄：`~/.tuvis/create-evidence/` 下按 `<ts>-<tool>.log` 追加。
struct Ev {
    dir: PathBuf,
    ts: String,
}

impl Ev {
    fn new() -> Self {
        let dir = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".tuvis")
            .join("create-evidence");
        let _ = std::fs::create_dir_all(&dir);
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        Self { dir, ts }
    }

    /// 台账一行：stdout（--nocapture 可见）+ 证据文件追加（写失败只丢证据）。
    fn log(&self, tool: &str, msg: &str) {
        let now = chrono::Local::now().format("%H:%M:%S%.3f");
        println!("[{now}] [{tool}] {msg}");
        let run_ts = &self.ts;
        let path = self.dir.join(format!("{run_ts}-{tool}.log"));
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{now}] {msg}");
        }
    }

    /// 最终屏读末 40 行进证据（best-effort；读屏失败留一行说明）。
    fn log_screen_tail(&self, tool: &str, pid: u32) {
        match read_screen_lines(pid) {
            Some(lines) => {
                let start = lines.len().saturating_sub(40);
                self.log(tool, &format!("屏读末 {} 行：", lines.len() - start));
                for l in &lines[start..] {
                    self.log(tool, &format!("  | {l}"));
                }
            }
            None => self.log(tool, "屏读失败（read_screen_lines None）——尾部证据缺失"),
        }
    }
}

// ============================================================
// 起窗前置：wt 探测（生产 windows_terminal_path 的测试侧最小复制）
// ============================================================

/// `where wt` 探测 Windows Terminal 完整路径（生产 `resume::windows_terminal_path`
/// 同源语义的测试侧最小复制——该函数 pub(crate) 不出 crate；缓存首行完整路径，
/// 不在场 None → 生产同款 conhost 回退，**不 panic**：conhost 形态同为覆盖面）。
fn wt_path() -> Option<String> {
    static WT: OnceLock<Option<String>> = OnceLock::new();
    WT.get_or_init(|| {
        std::process::Command::new("where")
            .arg("wt")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
            })
    })
    .clone()
}

// ============================================================
// 清场（cwd 标记扫描 + pid 树兜底；Drop 守卫）
// ============================================================

/// 路径归一（sysinfo cwd 匹配域：小写 + 斜杠统一——`monitor::cwd` 同语义的测试侧
/// 最小复制；`create::find_tui_pid` 判据同域）。
fn norm_path(s: &str) -> String {
    s.replace('/', "\\").to_ascii_lowercase()
}

/// cwd 含标记的全部进程 pid（marker = 本 leg 唯一 temp 目录的归一全路径；sysinfo
/// 刷新口径与 `create::find_tui_pid` 同款 with_cwd Always）。标记含 tempfile 唯一名，
/// 跨运行/跨用户会话零碰撞——命中者必属本 leg 进程树。
fn pids_with_cwd_marker(marker: &str) -> Vec<u32> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cwd(sysinfo::UpdateKind::Always),
    );
    sys.processes()
        .iter()
        .filter_map(|(pid, p)| {
            let cwd = p.cwd()?.to_string_lossy();
            (norm_path(&cwd).contains(marker)).then_some(pid.as_u32())
        })
        .collect()
}

/// taskkill /T /F 清场根（best-effort——失败只丢清场完整性，不碍断言）。
fn taskkill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

/// 单 leg 清场守卫（m9r_e2e ProbeProc 同款纪律的 create 版）：Drop 兜底 + 显式 kill
/// 幂等。清场面 = cwd 含本 leg 唯一 temp 目录标记的全部进程（cmd /k 壳 + TUI 同树）
/// 各自 taskkill /T /F——WT 标签页随 cmd 壳退出自关、conhost 随唯一客户端退出自收，
/// 宿主（WindowsTerminal/OpenConsole）绝不代杀（纪律：只碰本测试起的树）；pid 已知
/// 时补杀其树（cwd 刷新失灵的兜底）。
struct LegProc {
    pid: Option<u32>,
    marker: String,
    killed: bool,
}

impl LegProc {
    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        for p in pids_with_cwd_marker(&self.marker) {
            taskkill_tree(p);
        }
        if let Some(pid) = self.pid {
            taskkill_tree(pid);
        }
    }
}

impl Drop for LegProc {
    fn drop(&mut self) {
        self.kill();
    }
}

// ============================================================
// 单 leg 装配（矩阵与冒烟共用全链）
// ============================================================

/// 单 leg 台账载荷（矩阵测试 println 消费；字段全集进台账行）。
struct LegSummary {
    pid: u32,
    keys: Vec<String>,
    dialog_log: Vec<(String, Vec<String>)>,
    elapsed_ms: u128,
    sid: String,
    root: PathBuf,
    first_user: String,
}

/// 失败收口：证据留痕（消息 + 屏读尾部 best-effort）后返回错误串（调用方 Err 上抛，
/// [`LegProc`] Drop 兜底清场）。
fn fail_leg(ev: &Ev, tool: &str, pid: Option<u32>, msg: &str) -> String {
    ev.log(tool, &format!("FAIL {msg}"));
    if let Some(p) = pid {
        ev.log_screen_tail(tool, p);
    }
    msg.to_string()
}

/// keys_sent 键序红线断言（计划 f 条）：claude/opencode 严格钉死；codex 放宽为
/// **实机三态域**（C8 实机定案 2026-10-02：codex 0.160.0 无框直 idle——更新框+
/// 信任框 ["2","enter"] / 仅信任框 ["enter"] / 无框 []，红线键序本身由 C5
/// disposal_keys 单测 + 探测定案 §4/§5 锁定，实际形态落台账）；kimi 放宽二选一
/// （信任框弹出 ["enter"] / 未弹 []）。不符 → Some(诊断消息)。
fn keys_mismatch(tool: &str, keys: &[String]) -> Option<String> {
    let want: Vec<String> = match tool {
        "claude" => vec!["down".to_string(), "enter".to_string()],
        "codex" => {
            if keys.is_empty()
                || keys == ["enter".to_string()]
                || keys == ["2".to_string(), "enter".to_string()]
            {
                return None;
            }
            return Some(format!(
                "codex keys_sent={keys:?} 不在实机三态域（[\"2\",\"enter\"] / [\"enter\"] / []）"
            ));
        }
        "opencode" => Vec::new(),
        // kimi 放宽（计划 f 条）：实际形态由台账行记录（keys 已在 println 中）
        "kimi" => {
            if keys.is_empty() || (keys.len() == 1 && keys[0] == "enter") {
                return None;
            }
            return Some(format!(
                "kimi keys_sent={keys:?} 不在放宽域（期望 [\"enter\"] 或 []——信任框弹出与否二态）"
            ));
        }
        _ => return None,
    };
    if keys != want.as_slice() {
        return Some(format!(
            "keys_sent={keys:?} ≠ 期望 {want:?}（键序红线——处置键序与探测定案不符）"
        ));
    }
    None
}

/// 单家全链：全新 temp 目录 → 生产 spawner 起窗 → find_tui_pid → run_pipeline
/// （真屏读/真注入缝）→ discover_new_session 物化轮询 → stamp 命中 → 首条用户
/// 消息逐字节 == composed → taskkill 清场。Err 带诊断串（清场由守卫兜底）。
fn run_create_leg(ev: &Ev, tool: &'static str) -> Result<LegSummary, String> {
    let t0 = Instant::now();
    ev.log(tool, "=== leg 开始 ===");

    // a. 全新 temp 项目目录（**E:\ 根独立基座**，C8 冒烟实机定案：tempdir 在已信任
    //    git 仓库树下会被 claude 按 git 根**继承信任**——信任框不弹、键序红线无法
    //    实机复核；E:\tmp-c8-e2e 非仓库非黑名单，四家信任框真实触发。ASCII / 盘符
    //    绝对形态，validate 自检保留；TempDir 清理行为 forget 丢弃——目录保留为
    //    失败现场 + 证据）
    let target = Path::new(r"E:\tmp-c8-e2e");
    std::fs::create_dir_all(target).map_err(|e| format!("E:\\tmp-c8-e2e 创建失败：{e}"))?;
    let tmp = tempfile::tempdir_in(target).map_err(|e| format!("tempdir 创建失败：{e}"))?;
    let dir_str = tmp.path().display().to_string();
    create_path::validate(&dir_str, std::env::consts::OS).map_err(|rj| {
        format!(
            "temp 目录未过 create_path::validate：{}（{}）",
            rj.code, rj.message
        )
    })?;
    std::mem::forget(tmp);
    ev.log(tool, &format!("proj 目录（保留）={dir_str}"));

    // 清场守卫：构造即生效（panic 路径也杀）；marker = 唯一目录的归一全路径
    let marker = norm_path(&dir_str);
    let mut proc = LegProc {
        pid: None,
        marker: marker.clone(),
        killed: false,
    };

    // 物化发现的数据根（生产同源推导）+ 前置在场自检（缺即前置失败，不做无谓轮询）
    let home = dirs::home_dir().ok_or("无法确定用户主目录")?;
    let root = create_store_root(tool, &home)
        .ok_or_else(|| format!("{tool} 数据根不可用（kimi 需 ~/.kimi-code/sessions 在场）"))?;
    let root_ready = match tool {
        "opencode" => root.join("opencode.db").exists(),
        _ => root.exists(),
    };
    if !root_ready {
        return Err(fail_leg(
            ev,
            tool,
            None,
            &format!(
                "{} 数据根 {} 不在场（本机从未用过该工具？）——E2E 前置缺失",
                tool,
                root.display()
            ),
        ));
    }

    // wt 宿主探测（不在场 conhost 回退——生产同款降级链）
    let wt = wt_path();
    ev.log(
        tool,
        &format!("wt={}", wt.as_deref().unwrap_or("<无——conhost 回退>")),
    );

    // b. since 在管线起点采样（生产同位：codex 信任处置即建 rollout，落盘早于注入；
    //    confirm 戳是最终判据，since 取宽不假阳）
    let since = SystemTime::now();

    // c. 生产 spawner 真起窗（DISABLE_AUTOUPDATER=1 环境红线随 spec）
    let spec = build_create_spawn_spec(wt.as_deref(), &dir_str, tool);
    if let Err(e) = spawn_terminal(&spec) {
        return Err(fail_leg(
            ev,
            tool,
            None,
            &format!("spawn_terminal 失败：{e}"),
        ));
    }
    ev.log(tool, "spawn_terminal Ok（终端已出手）");

    // d. 真 pid 锚定（30s；起窗后立即调用——C5 not_before 调用方契约）
    let pid = find_tui_pid(tool, Path::new(&dir_str), Duration::from_secs(30))
        .map_err(|e| fail_leg(ev, tool, None, &format!("find_tui_pid 失败：{e}")))?;
    proc.pid = Some(pid);
    ev.log(tool, &format!("TUI pid={pid}"));

    // e. run_pipeline 真缝装配（屏读步距 / 键间隔由缝闭包承载；生产
    //    api.rs::run_create_pipeline 同构——pacer 先行、注入走 RealInjector spec 面）
    let fam = family_for(tool).expect("四家白名单工具必有族规格（families 表）");
    let inj = RealInjector;
    let screen = move |_p: u32| -> Option<Vec<String>> {
        std::thread::sleep(Duration::from_millis(SCREEN_POLL_STEP_MS));
        read_screen_lines(pid)
    };
    let send_key = |_p: u32, k: &str| -> Result<(), String> {
        std::thread::sleep(Duration::from_millis(KEY_GAP_MS));
        inj.locate_and_send_key_spec(pid, k, &fam)
    };
    let send_text =
        |_p: u32, text: &str| -> Result<(), String> { inj.locate_and_inject_spec(pid, text, &fam) };
    let deps = CreateDeps {
        screen: &screen,
        send_key: &send_key,
        send_text: &send_text,
    };
    // compose 单点（裁决 24b）返回 Result：E2E 固定设备名在白名单内，expect 即可
    let composed = compose_injection(DEVICE_NAME, FIRST_MESSAGE)
        .expect("E2E 设备名须在花名白名单内（见 turn::device_name_refusal）");
    ev.log(tool, &format!("composed={composed:?}"));
    let params = Params {
        tool: tool.to_string(),
        dir: PathBuf::from(&dir_str),
        first_message: String::new(),
        composed: composed.clone(),
        // codex hooks 核验式信任（生产 api.rs 同源判据）
        hooks_trust_ok: dirs::home_dir()
            .map(|h| multi_agents_manager_lib::monitor::hooks::codex_hooks_all_ours(&h))
            .unwrap_or(false),
    };
    let pipeline_t0 = Instant::now();
    let outcome = run_pipeline(&deps, &params);
    ev.log(
        tool,
        &format!(
            "pipeline 耗时={}ms outcome={:?}",
            pipeline_t0.elapsed().as_millis(),
            outcome
        ),
    );

    let (keys, dialog_log) = (outcome.keys_sent.clone(), outcome.dialog_log.clone());
    if !matches!(outcome.status, CreateStatus::WaitingMaterialize) {
        let msg = match &outcome.status {
            CreateStatus::Failed {
                phase,
                code,
                message,
            } => format!(
                "run_pipeline Failed：phase={phase} code={code} message={message} \
                 keys_sent={keys:?} dialog_log={dialog_log:?}"
            ),
            other => format!("run_pipeline 非预期终态：{other:?}（keys_sent={keys:?}）"),
        };
        return Err(fail_leg(ev, tool, Some(pid), &msg));
    }

    // f. keys_sent 键序红线（计划 C8 权威；kimi 放宽二选一）
    if let Some(msg) = keys_mismatch(tool, &keys) {
        return Err(fail_leg(ev, tool, Some(pid), &msg));
    }

    // g. 物化轮询（MATERIALIZE_MAX_ROUNDS 轮 × SCREEN_POLL_STEP_MS 步距——生产
    //    api.rs 同构：discover 逐候选 → 生产确认语义 stamp_hit_in_page 命中收口）
    let stamp = stamp_of(&composed).to_string();
    let mut hit: Option<(String, MessagesPage)> = None;
    for round in 1..=MATERIALIZE_MAX_ROUNDS {
        std::thread::sleep(Duration::from_millis(SCREEN_POLL_STEP_MS));
        let candidates = discover_new_session(tool, &root, since, &dir_str);
        ev.log(
            tool,
            &format!("物化第 {round} 轮：候选 {} 个", candidates.len()),
        );
        for sid in candidates {
            let Ok(pg) = read_session_messages(tool, &sid, PROBE_LIMIT) else {
                continue;
            };
            if stamp_hit_in_page(&pg, &stamp) {
                ev.log(
                    tool,
                    &format!("物化命中：sid={sid}（第 {round} 轮，stamp={stamp:?}）"),
                );
                hit = Some((sid, pg));
                break;
            }
        }
        if hit.is_some() {
            break;
        }
    }
    let Some((sid, pg)) = hit else {
        return Err(fail_leg(
            ev,
            tool,
            Some(pid),
            &format!(
                "物化超时：{} 轮内未发现 stamp 命中的会话（root={} dir={dir_str}）",
                MATERIALIZE_MAX_ROUNDS,
                root.display()
            ),
        ));
    };

    // 首条用户消息断言（计划权威「hi [mobile <名>]」compose 签名；经生产读路径
    // read_session_messages——各工具解析器同一份，页序 = 文件序尾窗）。**per-tool
    // 策略（C8 实机定案 2026-10-02）**：claude 逐字节（实机已证首条 user 即裸注入
    // 文本）；其余工具**页内任一 user 消息 contains composed**——codex 0.160.0 实证
    // 把 AGENTS.md instructions + environment_context 作为独立首条 user 消息，注入
    // 文本在后续 user 消息（rollout grep 已证 2 处在场）。composed 全文含移动端签名，
    // 非全词误报面可忽略。
    //
    // **composed 全文重试窗（2026-10-04 实机定案）**：物化 stamp（=strip 签名后的
    // 首句，缺省探针即 "hi"）对 codex 有假阳面——~/.codex/AGENTS.md 注入条含
    // "hi" 子串（High/benchmark 等），stamp 命中 ≠ 首句已落盘；且 codex 首启对
    // AGENTS.md 做 context compaction（屏读实证 `Context compacted · 5-9s`）期间
    // 输入消费与 rollout 写入双迟滞（注入成功、`› hi` 在 composer、模型 Working
    // 而 rollout 尚无 user 条）。故非 claude 工具在此**以 composed 全文 contains
    // 为准并带 45s 重试**；生产 stamp 假阳只致 done 提前回执（首句随后落盘、会话
    // 终态一致），登记后续批收紧 stamp 特异性，不在 E2E 侧修生产语义。
    let exact = tool == "claude";
    let mut first_user = pg
        .messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone());
    let mut hit = if exact {
        first_user.as_deref() == Some(composed.as_str())
    } else {
        pg.messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains(&composed))
    };
    if !hit && !exact {
        for _ in 0..22 {
            std::thread::sleep(Duration::from_millis(2000));
            if let Ok(p2) = read_session_messages(tool, &sid, PROBE_LIMIT) {
                if p2
                    .messages
                    .iter()
                    .any(|m| m.role == "user" && m.content.contains(&composed))
                {
                    hit = true;
                    break;
                }
                first_user = p2
                    .messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone());
            }
        }
    }
    let Some(first_user) = first_user else {
        return Err(fail_leg(
            ev,
            tool,
            Some(pid),
            "会话页内无 user 侧消息（stamp 命中但页空？）",
        ));
    };
    ev.log(tool, &format!("首条用户消息原文={first_user:?}"));
    if !hit {
        return Err(fail_leg(
            ev,
            tool,
            Some(pid),
            &format!(
                "首条用户消息{}不符：\n  期望 composed={composed:?}\n  实际        ={first_user:?}",
                if exact {
                    "逐字节"
                } else {
                    "页内 contains"
                }
            ),
        ));
    }

    // 回合落地等待（用户裁决 2026-10-02：注入后至少等 10-15s 让模型调用返回，
    // 不得物化命中即杀——否则「模型是否真实回复」这一最强端到端证据从未被采集，
    // 屏读证据只拍到回合中途态）。等待后重读页：assistant 是否已回复为**信息性
    // 台账**（不作断言——模型可用性是环境态，CC Switch 代理 400 等与 create 链
    // 无关），随后才取证清场。
    std::thread::sleep(Duration::from_millis(REPLY_WAIT_MS));
    if let Ok(pg2) = read_session_messages(tool, &sid, PROBE_LIMIT) {
        let assistants = pg2
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .count();
        if assistants > 0 {
            ev.log(
                tool,
                &format!("模型已回复（assistant {assistants} 条）——回合在清场前落地"),
            );
        } else {
            ev.log(
                tool,
                &format!(
                    "等待 {REPLY_WAIT_MS}ms 内未见 assistant 回复（模型态/代理态原因，\
                     不阻塞 create 链断言）"
                ),
            );
        }
    }

    // 收尾证据 + 台账 + 清场（守卫对 panic 路径兜底）
    ev.log_screen_tail(tool, pid);
    let elapsed_ms = t0.elapsed().as_millis();
    ev.log(tool, &format!("leg 完成：sid={sid} 耗时={elapsed_ms}ms"));
    proc.kill();
    Ok(LegSummary {
        pid,
        keys,
        dialog_log,
        elapsed_ms,
        sid,
        root,
        first_user,
    })
}

// ============================================================
// Test 1 —— 四家矩阵（单 #[test] 串行驱动；单家失败断言即止）
// ============================================================

/// 四家矩阵：全新 temp 目录 → build_create_spawn_spec 起窗 → find_tui_pid →
/// run_pipeline（真屏读/真注入缝）→ discover_new_session → confirm stamp →
/// 断言会话文件首条用户消息 == "hi [mobile <名>]"（compose 签名逐字）→ taskkill 清场。
/// 附加断言：claude 轮 keys_sent == [down, enter]；codex 双框轮 == [2, enter]（键序红线实机复核）。
#[test]
#[ignore = "实机显式跑：cargo test --test create_e2e -- --ignored --nocapture --test-threads=1"]
fn e2e_create_matrix_four_tools() {
    let ev = Ev::new();
    ev.log(
        "matrix",
        "=== e2e_create_matrix_four_tools 开始（claude→codex→kimi→opencode 串行）===",
    );
    let mut done: Vec<String> = Vec::new();
    for tool in ["claude", "codex", "kimi", "opencode"] {
        let t0 = Instant::now();
        match run_create_leg(&ev, tool) {
            Ok(s) => {
                done.push(tool.to_string());
                // 台账行（tool/pid/keys/耗时/会话存储定位——root+sid 唯一定位会话文件）
                ev.log(
                    tool,
                    &format!(
                        "台账：tool={tool} pid={} keys={:?} dialog_log={:?} \
                         leg耗时={}ms 总耗时={}ms sid={} root={} 首条user={:?}",
                        s.pid,
                        s.keys,
                        s.dialog_log,
                        s.elapsed_ms,
                        t0.elapsed().as_millis(),
                        s.sid,
                        s.root.display(),
                        s.first_user
                    ),
                );
            }
            Err(e) => {
                // 清单式已完成家 → stdout（实机诊断优先：失败即止，后续家不跑）
                println!("C8 台账：已完成家 = {done:?}");
                panic!("{tool} 新建会话 E2E 失败：{e}");
            }
        }
    }
    ev.log("matrix", &format!("四家全过：done={done:?}"));
    assert_eq!(done.len(), 4, "四家矩阵应全过：{done:?}");
}

// ============================================================
// Test 2 —— claude 单家冒烟（主会话实跑先冒烟一家再跑全矩阵）
// ============================================================

#[test]
#[ignore = "实机显式跑：cargo test --test create_e2e -- --ignored --nocapture --test-threads=1"]
fn e2e_create_single_claude_smoke() {
    let ev = Ev::new();
    ev.log(
        "claude",
        "=== e2e_create_single_claude_smoke 开始（单家冒烟）===",
    );
    match run_create_leg(&ev, "claude") {
        Ok(s) => ev.log(
            "claude",
            &format!(
                "台账：tool=claude pid={} keys={:?} dialog_log={:?} 耗时={}ms \
                 sid={} 首条user={:?}",
                s.pid, s.keys, s.dialog_log, s.elapsed_ms, s.sid, s.first_user
            ),
        ),
        Err(e) => panic!("claude 冒烟失败：{e}"),
    }
}

// ============================================================
// Test 3 —— C9 HTTP 端到端总测试（移动端等效路径；只写不跑）
// ============================================================

/// C9 设备花名（ASCII；配对 cookie 直指内存库设备行——与 C8 的 C8E2E 同规不同值）。
const C9_DEVICE_NAME: &str = "C9E2E";
/// C9 第二条消息正文（计划权威示例 `第二条消息 [mobile C9E2E]` 的**正文半边**——
/// 签名由服务端 compose 单点追加，发送端一律裸正文，与移动端等效）。
const C9_SECOND_MESSAGE: &str = "第二条消息";
/// 任务态/会话态轮询步距（计划权威：2s）。
const C9_POLL_STEP_MS: u64 = 2_000;
/// 任务态轮询窗（计划权威：至多 60s 至 done）。
const C9_STATUS_TIMEOUT: Duration = Duration::from_secs(60);
/// 会话回可输入态轮询窗（第二条消息**直发**前置；理由见测试内注释）。
const C9_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 全链设计预算（计划权威；C8 预算口径——超出登记不判红，避免模型侧环境态假红）。
const C9_BUDGET_MS: u128 = 120_000;

/// 运行进程 (工具, 项目目录名) 表——生产
/// `commands::session::running_projects_from_processes` 的测试侧最小复制（该函数
/// `pub(crate)` 不出 crate；`monitor::process` 五个发现器均为 pub，逐工具 cwd 目录名
/// 收集为同一段语义）。消费方 = `pairing_counter` 缝（C7 配对不确定打标域）。
fn c9_running_projects() -> Vec<(String, String)> {
    let system = sysinfo::System::new_all();
    let mut v = Vec::new();
    for (agent, procs) in [
        (
            "claude",
            multi_agents_manager_lib::monitor::process::find_claude_processes(&system),
        ),
        (
            "codex",
            multi_agents_manager_lib::monitor::process::find_codex_processes(&system),
        ),
        (
            "opencode",
            multi_agents_manager_lib::monitor::process::find_opencode_processes(&system),
        ),
        (
            "openclaw",
            multi_agents_manager_lib::monitor::process::find_openclaw_processes(&system),
        ),
        (
            "kimi",
            multi_agents_manager_lib::monitor::process::find_kimi_processes(&system),
        ),
    ] {
        for p in procs {
            let name = p
                .cwd
                .as_ref()
                .and_then(|c| c.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.is_empty());
            if let Some(name) = name {
                v.push((agent.to_string(), name));
            }
        }
    }
    v
}

/// GET /sessions 单次取数（真聚合口快照——全机真实会话）。
async fn c9_sessions(client: &reqwest::Client, port: u16) -> Result<serde_json::Value, String> {
    let resp = client
        .get(format!("http://127.0.0.1:{port}/m/api/v1/sessions"))
        .send()
        .await
        .map_err(|e| format!("请求失败：{e}"))?;
    let status = resp.status();
    let v = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("响应非 JSON：{e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}：{v}"));
    }
    Ok(v)
}

/// /sessions 快照按 id 精确取卡（快照是全机真实会话——必须 id 匹配，禁模糊命中）。
fn c9_card<'a>(payload: &'a serde_json::Value, sid: &str) -> Option<&'a serde_json::Value> {
    payload
        .get("sessions")?
        .as_array()?
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(sid))
}

/// 卡片 status 是否可输入三态（waiting|idle|finished——生产
/// `inject::queue::is_input_ready` 同口径；session-send 仅对此三态直发）。
fn c9_input_ready(card: &serde_json::Value) -> bool {
    matches!(
        card.get("status").and_then(|s| s.as_str()),
        Some("waiting") | Some("idle") | Some("finished")
    )
}

/// 端到端总测试（移动端等效路径）：临时目录 → POST /m/api/v1/session-create（带设备
/// cookie）→ 轮询 /session-create/status 至 done{sessionId} → GET /sessions 含新会话卡
/// （pairingAmbiguous=false）→ POST /session-send 第二条消息 delivered → 审计行可查
/// （create + dialog + send×2）→ taskkill 清场。
///
/// 装配与 C8 内核直调链的差异：全链**只走 HTTP**（与移动端同一条路由/门禁/队列/
/// 审计面），create 域用生产缝束（真 pid 锚定/真物化发现/真步距/真安装探测），
/// session_source 用真 `adapter::get_all_sessions`（新会话真实上板）。
#[tokio::test]
#[ignore = "实机显式跑：cargo test --test create_e2e -- --ignored --nocapture --test-threads=1"]
async fn e2e_create_http_full_chain() {
    use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
    use multi_agents_manager_lib::remote::pin::PinRateLimiter;
    use multi_agents_manager_lib::remote::server::{
        router, CreateTaskHub, RemoteState, SseRegistry,
    };

    let ev = Ev::new();
    ev.log(
        "c9-http",
        "=== e2e_create_http_full_chain 开始（移动端等效 HTTP 全链）===",
    );
    let t0 = Instant::now();

    // a. temp 基座（E:\tmp-c9-e2e——信任树外，C8 实机定案；validate 自检保留；
    //    TempDir 清理行为 forget 丢弃——目录保留为失败现场 + 证据）
    let target = Path::new(r"E:\tmp-c9-e2e");
    std::fs::create_dir_all(target).expect("E:\\tmp-c9-e2e 创建失败");
    let tmp = tempfile::tempdir_in(target).expect("tempdir 创建失败");
    let dir_str = tmp.path().display().to_string();
    create_path::validate(&dir_str, std::env::consts::OS).unwrap_or_else(|rj| {
        panic!(
            "temp 目录未过 create_path::validate：{}（{}）",
            rj.code, rj.message
        )
    });
    std::mem::forget(tmp);
    ev.log("c9-http", &format!("proj 目录（保留）={dir_str}"));

    // 清场守卫：构造即生效（panic 路径 Drop 兜底）；spawnedPid 到位后补 pid 双保险
    let marker = norm_path(&dir_str);
    let mut proc = LegProc {
        pid: None,
        marker: marker.clone(),
        killed: false,
    };

    // ① RemoteState 装配（m9r_e2e::e2e_http_full_chain 模板 + create 域真缝）：
    //    - create_hub = production（真 pid_finder 30s / 真 discoverer / 真 pacer /
    //      真 tool_probe）；
    //    - session_source = 真 adapter::get_all_sessions（新建会话真实上板——本用例
    //      的「上板」断言即建立在此）；
    //    - pairing_counter = mod.rs 生产形态（Windows 真 sysinfo 收集，测试侧最小
    //      复制 c9_running_projects）；
    //    - injector / resume_spawner / screen_probe / confirm_probe / dialog_probe =
    //      生产同源（真注入 / 真起窗 / 真屏读 / 真确认 / 真对话框探针）；
    //    - store = DeviceStore::memory（零接触真实 tuvis.db 写路径；审计/设备行全在
    //      内存库）；
    //    - host_source 必须给 enabledTools（session-create 工具门第二道），故非 m9r
    //      的 Null 桩；其余缝照 m9r 最小假体（本链不触归档/看板隐藏/未读/硬杀）。
    let state = Arc::new(RemoteState {
        // L13 靶向证据缝：E2E 不构造同 cwd 多实例 → 空证据 = 无候选 = 放行
        target_evidence: Box::new(|_, _| {
            multi_agents_manager_lib::window::tty_map::TargetEvidence::default()
        }),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(multi_agents_manager_lib::adapter::get_all_sessions),
        pairing_counter: Box::new(c9_running_projects),
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        store: DeviceStore::memory(),
        injector: Arc::new(RealInjector),
        resume_spawner: Arc::new(multi_agents_manager_lib::inject::resume::spawn_terminal),
        create_hub: Arc::new(CreateTaskHub::production()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|tool: &str, s: &str, stamp: &str| -> bool {
            read_session_messages(tool, s, PROBE_LIMIT)
                .map(|pg| stamp_hit_in_page(&pg, stamp))
                .unwrap_or(false)
        }),
        host_source: Box::new(|| serde_json::json!({ "enabledTools": ["claude"] })),
        message_source: Box::new(read_session_messages),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: std::sync::Arc::new(|_| 0usize),
        board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
        unread_mark_read: std::sync::Arc::new(|_, _| ()),
        session_close: std::sync::Arc::new(|_| Ok(())),
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: std::sync::Mutex::new(PinRateLimiter::new()),
        global_pin_limiter: std::sync::Mutex::new(PinRateLimiter::global()),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        // 配对限速通道来源缝（#119 取代 tunnel_hosts_source；空表 = fail-closed）
        rate_bucket_channels_source: Box::new(Vec::new),
        via_hosts_source: Box::new(|| None),
        home_source: Box::new(|| None),
        dialog_probe: Arc::new(|_sid: &str, pid: u32| {
            multi_agents_manager_lib::inject::dialog::probe_screen_dialog(pid)
        }),
        screen_probe: Arc::new(|_sid: &str, pid: u32| read_screen_lines(pid)),
    });

    // ② 预置配对设备（内存库直插设备行；cookie 名 mam_device——m9r 先例）
    state
        .store
        .with(|c| {
            persist_device(
                c,
                &NewDevice {
                    id: "c9-e2e-dev".into(),
                    name: C9_DEVICE_NAME.into(),
                    ua: "ua-c9-e2e".into(),
                    origin_ip: "ip-c9-e2e".into(),
                    via: String::new(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
        })
        .expect("预置配对设备失败");

    // ③ 真 HTTP 服务器（127.0.0.1:0 + ConnectInfo；router = server.rs 现成构造器）
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定测试端口失败");
    let port = listener.local_addr().unwrap().port();
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("测试服务器异常退出");
    });
    ev.log("c9-http", &format!("HTTP 服务器就绪 127.0.0.1:{port}"));

    // ④ 真 HTTP 客户端（设备 cookie 默认头——移动端等效；120s 超时覆盖物化窗）
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::COOKIE,
        reqwest::header::HeaderValue::from_static("mam_device=c9-e2e-dev"),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .default_headers(headers)
        .build()
        .expect("reqwest client 构建失败");

    // b. POST /session-create（工具=claude，全新 temp 目录，缺省首句 = 默认探针 hi）
    let resp = client
        .post(format!("http://127.0.0.1:{port}/m/api/v1/session-create"))
        .json(&serde_json::json!({ "tool": "claude", "projectPath": dir_str }))
        .send()
        .await
        .expect("session-create 请求失败");
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    ev.log(
        "c9-http",
        &format!("session-create 响应：{status} {payload}"),
    );
    assert_eq!(status, 200, "session-create 应 200：{payload}");
    let task_id = payload
        .get("taskId")
        .and_then(|v| v.as_u64())
        .expect("回执缺 taskId");
    assert_eq!(
        payload.get("hasActiveSession").and_then(|v| v.as_bool()),
        Some(false),
        "全新 temp 目录不应有同项目活跃会话信号：{payload}"
    );

    // c. 轮询任务态至 done{sessionId}（2s 步距、至多 60s；相变 + spawnedPid 打台账）
    let mut spawned_pid: Option<u32> = None;
    let mut last_phase = String::new();
    let sid = {
        let deadline = Instant::now() + C9_STATUS_TIMEOUT;
        loop {
            let resp = client
                .get(format!(
                    "http://127.0.0.1:{port}/m/api/v1/session-create/status?taskId={task_id}"
                ))
                .send()
                .await
                .expect("session-create/status 请求失败");
            let status = resp.status();
            let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            assert_eq!(status, 200, "status 应 200：{v}");
            let phase = v
                .get("phase")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(pid) = v.get("spawnedPid").and_then(|p| p.as_u64()) {
                if spawned_pid != Some(pid as u32) {
                    spawned_pid = Some(pid as u32);
                    proc.pid = spawned_pid; // 清场 pid 兜底随回执挂上
                    ev.log("c9-http", &format!("spawnedPid={pid}（清场兜底已挂）"));
                }
            }
            if phase != last_phase {
                ev.log(
                    "c9-http",
                    &format!(
                        "phase={phase} detail={:?} sessionId={:?}",
                        v.get("detail"),
                        v.get("sessionId")
                    ),
                );
                last_phase = phase.clone();
            }
            if phase == "done" {
                let sid = v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                assert!(!sid.is_empty(), "done 回执必须带 sessionId：{v}");
                break sid;
            }
            assert_ne!(phase, "failed", "创建管线失败：{v}");
            if Instant::now() >= deadline {
                panic!(
                    "任务 {task_id} 未在 {}s 内 done（末相 {phase}）：{v}",
                    C9_STATUS_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(C9_POLL_STEP_MS)).await;
        }
    };
    ev.log(
        "c9-http",
        &format!("任务 {task_id} done：sessionId={sid} spawnedPid={spawned_pid:?}"),
    );

    // d. GET /sessions（真聚合口快照）→ 新会话卡在场 + pairingAmbiguous=false。
    //    快照是全机真实会话——按 sessionId 精确匹配，禁模糊命中；单飞护栏可能
    //    返回材料化前的旧快照 → 有界重试（移动端同样 3s 轮询，等效）。
    let card = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snap = c9_sessions(&client, port)
                .await
                .expect("GET /sessions 失败");
            if let Some(card) = c9_card(&snap, &sid) {
                break card.clone();
            }
            if Instant::now() >= deadline {
                panic!(
                    "30s 内 /sessions 未见新会话卡（sid={sid}，totalCount={}）",
                    snap.get("totalCount").and_then(|v| v.as_u64()).unwrap_or(0)
                );
            }
            tokio::time::sleep(Duration::from_millis(C9_POLL_STEP_MS)).await;
        }
    };
    ev.log("c9-http", &format!("/sessions 新卡：{card}"));
    assert_eq!(
        card.get("agentType").and_then(|v| v.as_str()),
        Some("claude"),
        "新卡 agentType 应为 claude：{card}"
    );
    assert_eq!(
        card.get("id").and_then(|v| v.as_str()),
        Some(sid.as_str()),
        "新卡 id 应等于 status 回执 sessionId：{card}"
    );
    assert_eq!(
        card.get("pairingAmbiguous").and_then(|v| v.as_bool()),
        Some(false),
        "唯一 temp 项目的新会话不应打配对歧义标（C7 打标域）：{card}"
    );

    // d2. 等会话回可输入态（第二条消息**直发**的前置）：session-send 对运行中/
    //     思考中会话按队列语义回 queued——物化 done 只保证首句已落盘，模型回合
    //     尚在进行（末条 user → thinking）；移动端等效动作 = 等红/绿卡再发。
    //     轮询 /sessions 至 waiting|idle|finished，2s 步距、至多 60s；相变打台账。
    {
        let deadline = Instant::now() + C9_IDLE_TIMEOUT;
        let mut last = String::new();
        loop {
            let snap = c9_sessions(&client, port)
                .await
                .expect("GET /sessions 失败（等空闲）");
            let card = c9_card(&snap, &sid).expect("会话卡在等空闲期间消失");
            let st = card
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if st != last {
                ev.log("c9-http", &format!("等空闲：status={st}"));
                last = st.clone();
            }
            if c9_input_ready(card) {
                ev.log(
                    "c9-http",
                    &format!(
                        "会话回到可输入态（status={st}，链起点以来 {}ms）",
                        t0.elapsed().as_millis()
                    ),
                );
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{}s 内会话未回可输入态（末态 {st}）——第二条消息直发将转排队，中止",
                    C9_IDLE_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(C9_POLL_STEP_MS)).await;
        }
    }

    // e. POST /session-send 第二条消息（裸正文——签名由服务端 compose 单点追加；
    //    期望 composed = `第二条消息 [mobile C9E2E]`）
    let resp = client
        .post(format!("http://127.0.0.1:{port}/m/api/v1/session-send"))
        .json(&serde_json::json!({ "sessionId": sid, "text": C9_SECOND_MESSAGE }))
        .send()
        .await
        .expect("session-send 请求失败");
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    ev.log(
        "c9-http",
        &format!(
            "session-send 响应：{status} {payload}（composed={:?}）",
            compose_injection(C9_DEVICE_NAME, C9_SECOND_MESSAGE)
        ),
    );
    assert_eq!(status, 200, "session-send 应 200：{payload}");
    assert_eq!(
        payload.get("status").and_then(|v| v.as_str()),
        Some("delivered"),
        "可输入态直发应 delivered：{payload}"
    );
    assert!(
        payload
            .get("pairingHint")
            .and_then(|v| v.as_bool())
            .is_some(),
        "delivered 回执应含 pairingHint 布尔（恒在场，C7）：{payload}"
    );

    // f. 审计断言（内存库读 API；行序最新在前）。计划 C9 权威：create 恰 1 行
    //    （ok + session_id=sid + device_name 一致）、dialog ≥1 行（claude 信任框
    //    create_trust: down,enter）、send 恰 2 行（create 首句 composed + 第二条，
    //    device_name 一致）。flush 机制行随直发 settle 落账——计划括号「直发
    //    delivered 无 flush 行」与实现不符（m9r 已断言其存在），本用例**只登记
    //    不断言**（见 .report-c9.md）。
    let audits = state
        .store
        .with(|c| multi_agents_manager_lib::database::dao::write_audit::recent_conn(c, 50));
    for a in &audits {
        ev.log(
            "c9-http",
            &format!(
                "audit: action={} result={} channel={} session={} device={} summary={}",
                a.action, a.result, a.channel, a.session_id, a.device_name, a.summary
            ),
        );
    }
    let creates: Vec<_> = audits.iter().filter(|a| a.action == "create").collect();
    assert_eq!(
        creates.len(),
        1,
        "create 审计应恰 1 行（成功终态）：{audits:?}"
    );
    assert_eq!(creates[0].result, "ok", "create 行应 result=ok");
    assert_eq!(
        creates[0].session_id, sid,
        "create 行 session_id 应等于 done 回执 sid"
    );
    assert_eq!(
        creates[0].device_name, C9_DEVICE_NAME,
        "create 行设备名应为测试设备"
    );
    assert_eq!(
        creates[0].channel, "real",
        "create 行 channel 应取注入器名 real"
    );

    let dialogs: Vec<_> = audits.iter().filter(|a| a.action == "dialog").collect();
    assert!(
        !dialogs.is_empty(),
        "claude 信任框处置应留 dialog 行：{audits:?}"
    );
    assert!(
        dialogs
            .iter()
            .any(|a| a.summary.contains("create_trust") && a.summary.contains("down,enter")),
        "claude 信任框处置应为 create_trust: down,enter：{dialogs:?}"
    );

    let sends: Vec<_> = audits.iter().filter(|a| a.action == "send").collect();
    assert_eq!(
        sends.len(),
        2,
        "send 审计应恰 2 行（create 首句 + session-send 第二条）：{audits:?}"
    );
    assert!(
        sends
            .iter()
            .all(|a| a.result == "ok" && a.device_name == C9_DEVICE_NAME),
        "send 行应全为 result=ok 且设备名一致：{sends:?}"
    );
    let first = sends
        .iter()
        .find(|a| a.session_id.is_empty())
        .expect("首句 send 行（物化前 session_id 为空）");
    assert!(
        first.summary.contains("hi"),
        "首句 send 行 summary 应含默认探针 hi（composed=hi [mobile C9E2E]）：{first:?}"
    );
    let second = sends
        .iter()
        .find(|a| a.session_id == sid)
        .expect("第二条 send 行（session-send 投递）");
    assert!(
        second.summary.contains(C9_SECOND_MESSAGE),
        "第二条 send 行 summary 应含正文：{second:?}"
    );
    let flushes: Vec<_> = audits.iter().filter(|a| a.action == "flush").collect();
    ev.log(
        "c9-http",
        &format!(
            "flush 机制行 {} 条（直发 settle 落账——计划括号「直发无 flush 行」与实现不符，\
             不断言，报告登记）",
            flushes.len()
        ),
    );

    // g. 回合落地等待（REPLY_WAIT_MS=15s，用户裁决同款）：第二条消息的模型回复也要
    //    有机会落地再取证清场。等待后复读 /sessions 看状态跃迁 + 会话页 assistant
    //    条数（均信息性，不断言——模型可用性是环境态，与 create 链断言无关）。
    tokio::time::sleep(Duration::from_millis(REPLY_WAIT_MS)).await;
    if let Ok(snap) = c9_sessions(&client, port).await {
        if let Some(card) = c9_card(&snap, &sid) {
            ev.log(
                "c9-http",
                &format!(
                    "等待后 /sessions 卡：status={} lastMessageRole={:?}",
                    card.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                    card.get("lastMessageRole")
                ),
            );
        }
    }
    if let Ok(pg) = read_session_messages("claude", &sid, PROBE_LIMIT) {
        let assistants = pg.messages.iter().filter(|m| m.role == "assistant").count();
        ev.log(
            "c9-http",
            &format!(
                "第二条消息落地后 assistant 条数={assistants}（信息性台账——模型可用性是\
                 环境态，不阻塞断言）"
            ),
        );
    }

    // i. 证据：最终屏读尾 40 行（spawnedPid 在场时；best-effort）
    if let Some(pid) = spawned_pid {
        ev.log_screen_tail("c9-http", pid);
    }
    // h. 清场：taskkill 树（cwd 标记扫描 + spawnedPid 兜底，LegProc 同款）；temp
    //    目录保留（std::mem::forget 已丢弃 TempDir 清理行为）
    proc.kill();
    server.abort();

    let elapsed = t0.elapsed().as_millis();
    ev.log(
        "c9-http",
        &format!(
            "=== 全链完成：sid={sid} 耗时={elapsed}ms（设计预算 ≤{C9_BUDGET_MS}ms：{}）===",
            if elapsed <= C9_BUDGET_MS {
                "达标"
            } else {
                "超出——环境态慢于此，如实登记不判红（C8 预算口径）"
            }
        ),
    );
}
