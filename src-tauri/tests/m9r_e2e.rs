#![cfg(windows)]
//! M9R–M9R 批次 Task 12 四例 + 评审修复批 F5 扩三例（共七例）实机 E2E 集成测试
//! （全部 `#[ignore]`，实机显式跑）。
//!
//! ## 前置条件（文档约定，测试头部备案）
//! - Windows 宿主（conhost 控制台拓扑，复用项目技能 `win-console-inject-probe`
//!   的起会话拓扑：conhost cmd /k 经 PowerShell `Start-Process`；探测基座
//!   `%USERPROFILE%\mam-probe-m6r\`）；F5 扩的 `e2e_wt_host_matrix` 另需本机
//!   已装 Windows Terminal（`where wt` 命中；不在场即前置 panic，见该例自检）；
//! - 本机已装四家 CLI 且版本与族规格表指纹一致：claude 2.1.251 / codex 0.154.0 /
//!   kimi 2.0.0 / opencode 1.18.31（`inject::families::family_for` 的 verified_with）；
//! - temp 探测目录可写（测试自建项目目录，run-id 唯一）；
//! - **硬杀测试进程（如 Ctrl-C）会残留探测终端需手动关闭**；evidence 目录随
//!   run 累积（体积小，历史 run 不自动清理）。
//!
//! ## 纪律（八条铁律的项目内裁剪）
//! - 只碰 temp 探测目录里**本测试新建**的会话，绝不触碰用户自己的终端/会话；
//! - 每例结束 `taskkill /T /F` 清场（进程树 + conhost 宿主），panic 路径由
//!   Drop 守卫兜底清场；
//! - 日志/证据追加式带 run-id，落 `%USERPROFILE%\mam-probe-m6r\evidence\m9r-e2e\`；
//!   临时 .ps1 一律 CRLF + UTF-8 BOM；
//! - 零接触真实 `~/.tuvis` 的写路径：服务器用内存库（`DeviceStore::memory`）；
//!   读路径（`read_session_messages` 确认轮询）只读真实 CLI 会话存储——这是
//!   A1 确认层「会话文件命中」语义所需，只读合规。
//!
//! ## 运行方式（实机显式跑）
//! ```text
//! cargo test --test m9r_e2e -- --ignored --nocapture --test-threads=1
//! ```
//! 常规门禁（`cargo test` 不带 --ignored）只验证编译，零新增运行时。
//! 慢用例（Test 2 opencode 背压）实测分钟级，耐心等待勿中途判定失败；
//! 单例失败先隔离重跑（`--test-threads=1` 或单测名过滤）再定因。
//!
//! ## E2E 实跑台账（2026-09-19 本机实跑记录；耗时以各 run.log 为准）
//! | 日期 | 用例 | 耗时 | 断言结果 | 证据目录 |
//! |---|---|---|---|---|
//! | 2026-09-19 | e2e_engine_matrix_short | 76.6s | 四家全过：背压旗标 claude/codex/kimi=false、opencode=true；四家 stamp 全命中（1~27ms） | evidence\m9r-e2e\engine-matrix-short-<run-id>\ |
//! | 2026-09-19 | e2e_engine_matrix_long | 147.1s（单例）；全套连跑 293.8s | claude 10k=2.1s（<15s ✓）；opencode 10k=106.3s/连跑 107.5s（≤460s 预算 ✓）；双 stamp 命中 | evidence\m9r-e2e\engine-matrix-long-<run-id>\ |
//! | 2026-09-19 | e2e_http_full_chain | 39.7s | 200 delivered + 目标会话 stamp 命中 + 审计 send/flush 两行（channel=real） | evidence\m9r-e2e\http-full-chain-<run-id>\ |
//! | 2026-09-19 | e2e_key_domain_and_enter | 19.0s | codex rollout 命中文本（VK 回车提交生效）+ 域外拒绝 Err 含「不支持的按键」 | evidence\m9r-e2e\key-domain-enter-<run-id>\ |
//! | 2026-09-19 | e2e_wt_host_matrix（F5） | 39.3s | WT×2000×claude/codex 全过：written=2000、背压旗标双 false（2000 恰不超阈值）；stamp 命中 2ms/9ms；TUI 定位 tui/cmd 双命中（wt -d 落目录经 cwd 标记匹配） | evidence\m9r-e2e\wt-host-matrix-20260919-162543-t5\ |
//! | 2026-09-19 | e2e_codex_long_10k（F5） | 21.6s | written=10000、背压=true（快消费者 10000>2000 语义）；注入耗时 2143ms（≤460s 预算 ✓）；stamp 命中 21ms | evidence\m9r-e2e\codex-long-10k-20260919-162641-t6\ |
//! | 2026-09-19 | e2e_cross_session_no_crosstalk（F5） | 41.9s（首跑 42.0s 同绿） | 双会话并行注入：A/B written=2000、背压双 false；A stamp 命中 0ms / B stamp 10ms；双向零串扰断言过（并行触发、CONSOLE_OP 内部串行） | evidence\m9r-e2e\cross-session-20260919-162801-t7\ |
//!
//! 实跑备注（同日活体实验定案，已内化为代码注释）：
//! - 四家 CLI 会话存储均**懒创建**（首条用户消息后才落盘）→ 流水线固定
//!   「注入 → 发现 sid → 轮询 stamp」；
//! - opencode 的真实 cwd 在 `session.directory`（正斜杠形态），`project.worktree`
//!   不可用（实测多为 '/'）；
//! - codex TUI 自报版本 0.155.1 与 npm `codex --version`（0.154.0）存在漂移，
//!   注入规格按 B 族口径实测有效。
//!
//! 失败史留痕（首跑失败 → 修复 → 复跑通过）：
//! - T1 首跑 opencode 发现超时 → 发现查询改 `session.directory` 匹配后复跑通过；
//! - T4 首跑 cmd 定位漏 -Parent → 修复后通过。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::inject::confirm::{stamp_hit_in_page, stamp_of};
use multi_agents_manager_lib::inject::e2e_support::{inject_key_spec, inject_text_spec};
use multi_agents_manager_lib::inject::families::{family_for, FamilySpec};
use multi_agents_manager_lib::remote::content::read_session_messages;

/// 确认轮询取数上限：与生产 `confirm::PROBE_MESSAGE_LIMIT`(20) 同口径
/// （该常量为 pub(crate)，集成测试侧以字面量对齐，注释锚定单一来源）。
const PROBE_LIMIT: usize = 20;

/// 会话 id 发现超时（会话存储懒创建 + 落盘异步，注入后轮询等待）。
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);
/// stamp 确认轮询窗——快消费者（claude/codex/kimi；提交秒级落盘）。
const STAMP_TIMEOUT_FAST: Duration = Duration::from_secs(30);
/// stamp 确认轮询窗——慢消费者（opencode，~70 事件/s + SQLite 落库）。
/// Test 1（150 字符）与 Test 2（10k 背压）统一取同一窗：注入返回后的 stamp
/// 落盘延迟由消费尾延迟主导、与注入长度弱相关，统一给足上界，不按用例分叉。
const STAMP_TIMEOUT_SLOW: Duration = Duration::from_secs(180);
/// 确认轮询步距（与生产 confirm::PROBE_INTERVAL_MS 同口径的测试侧常量）。
const POLL_GAP: Duration = Duration::from_millis(500);
/// WT 宿主目标进程发现窗（launch_cli 的 cmd 20s + TUI 45s 两段合并上限：
/// sysinfo cwd 匹配一次扫描同时覆盖两类候选，取最宽段）。
const WT_FIND_TIMEOUT: Duration = Duration::from_secs(45);

// ============================================================
// 证据与日志（追加式带 run-id；探测基座 mam-probe-m6r）
// ============================================================

/// 单例证据目录句柄：`%USERPROFILE%\mam-probe-m6r\evidence\m9r-e2e\<name>-<run-id>\`。
struct Ev {
    dir: PathBuf,
    log_path: PathBuf,
}

impl Ev {
    fn new(name: &str, run_id: &str) -> Self {
        let dir = dirs::home_dir()
            .expect("无法确定用户主目录")
            .join("mam-probe-m6r")
            .join("evidence")
            .join("m9r-e2e")
            .join(format!("{name}-{run_id}"));
        std::fs::create_dir_all(&dir).expect("建证据目录失败");
        let log_path = dir.join("run.log");
        Self { dir, log_path }
    }

    /// 追加一行日志（文件 + stderr 双通道，永不覆盖）。
    fn log(&self, msg: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let line = format!("[{ts}] {msg}");
        eprintln!("{line}");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// 写临时 .ps1（探测纪律：CRLF + UTF-8 BOM，ASCII 内容亦统一加 BOM）。
    fn write_ps(&self, name: &str, body: &str) -> PathBuf {
        let mut content = String::with_capacity(body.len() + 2);
        content.push('\u{feff}');
        content.push_str(body);
        let path = self.dir.join(name);
        std::fs::write(&path, content.replace('\n', "\r\n")).expect("写 .ps1 失败");
        path
    }
}

// ============================================================
// 探测会话起手 / 清场（conhost cmd /k <cli>，M6R 已证拓扑）
// ============================================================

/// 子进程枚举脚本：输出「name|pid|commandline」行（每直接子进程一行）。
const FIND_CHILDREN_PS: &str = "\
param([int]$Parent)
Get-CimInstance Win32_Process -Filter \"ParentProcessId=$Parent\" | ForEach-Object {
  \"{0}|{1}|{2}\" -f $_.Name, $_.ProcessId, $_.CommandLine
}";

/// 跑子进程枚举脚本（-File + -Parent 参数，规避 Rust→PowerShell 多层引号转义）。
fn run_ps_with_parent(finder: &Path, parent: u32) -> String {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(finder)
        .arg("-Parent")
        .arg(parent.to_string())
        .output()
        .expect("执行 powershell 失败");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 探测会话进程树（conhost → cmd → TUI 孙进程链；WT 拓扑见 [`launch_wt_cli`]）。
struct ProbeProc {
    /// 宿主清场补充 pid（cmd 树之外补杀的宿主）：conhost 拓扑 = `[conhost]`；
    /// WT 拓扑 = `[]`——杀 cmd 树后 ConPTY 客户端退出、标签页自关，WindowsTerminal
    /// 窗口进程属宿主所有（可能携带用户自己的其他标签页），**绝不代杀**
    /// （纪律：只碰本测试新建的进程树）。
    hosts: Vec<u32>,
    cmd: u32,
    /// 注入目标 pid = TUI 主进程（claude/codex/kimi/opencode 或其 node/bun
    /// 运行时），找不到 TUI 时回落 cmd.exe——同一控制台，M6R 同款兜底
    /// （TUI 进程号仅在 launch 时打日志留档，不入结构体字段——零死字段）。
    target: u32,
    /// 探测项目目录（temp 下，run-id 唯一）。
    proj: PathBuf,
    killed: bool,
}

impl Drop for ProbeProc {
    fn drop(&mut self) {
        self.kill();
    }
}

impl ProbeProc {
    /// 清场：taskkill /T /F 杀 cmd 进程树（含 TUI），再补杀宿主集合。幂等。
    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        for pid in std::iter::once(self.cmd).chain(self.hosts.iter().copied()) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

/// 起一个真实 CLI 探测会话：conhost cmd /k <cli>，WorkingDirectory = temp 探测目录。
/// 依次定位 cmd（conhost 直接子进程，≤20s）与 TUI 主进程（cmd 后代 ≤3 层，≤45s）。
fn launch_cli(cli: &str, tag: &str, ev: &Ev) -> ProbeProc {
    let proj = std::env::temp_dir().join(format!("mam-m9r-{tag}-{cli}"));
    let _ = std::fs::remove_dir_all(&proj); // run-id 唯一，清理仅防理论碰撞
    std::fs::create_dir_all(&proj).expect("建探测项目目录失败");

    // 已证拓扑：conhost.exe cmd /k（挂在探测目录）；-PassThru 取 conhost pid
    let script = format!(
        "Start-Process conhost.exe -ArgumentList 'cmd.exe','/k','{cli}' -WorkingDirectory '{}' -PassThru | Select-Object -ExpandProperty Id",
        proj.display()
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("启动 powershell 失败");
    let conhost: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| {
            // Start-Process 失败（目录不存在/权限等）时 stdout 非 pid——带上
            // powershell stderr 定位，避免裸「无法解析数字」哑 panic
            panic!(
                "conhost pid 解析失败（Start-Process 失败？stderr={}）",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        });
    ev.log(&format!(
        "launch {cli}: conhost={conhost} proj={}",
        proj.display()
    ));

    // ① 定位 cmd（conhost 直接子进程）
    let finder = ev.write_ps(&format!("children-{tag}.ps1"), FIND_CHILDREN_PS);
    let mut cmd_pid = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while cmd_pid.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1200));
        for line in run_ps_with_parent(&finder, conhost).lines() {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("cmd.exe") {
                cmd_pid = parts[1].parse().ok();
            }
        }
    }
    let cmd = cmd_pid.unwrap_or_else(|| panic!("{cli}: 20s 内未定位到 cmd.exe 子进程"));
    ev.log(&format!("launch {cli}: cmd={cmd}"));

    // ② 定位 TUI 主进程（cmd 后代 ≤3 层；npm shim 场景中间还有一层 cmd /c）
    let tui_names = [
        format!("{cli}.exe"),
        "node.exe".to_string(),
        "bun.exe".to_string(),
    ];
    let mut frontier = vec![cmd];
    let mut tui = None;
    'outer: for _depth in 0..3 {
        let mut next = Vec::new();
        for pid in &frontier {
            for line in run_ps_with_parent(&finder, *pid).lines() {
                let parts: Vec<&str> = line.splitn(3, '|').collect();
                if parts.len() < 2 {
                    continue;
                }
                let name = parts[0].to_ascii_lowercase();
                // cmd/conhost/powershell 是中间壳：入队继续向下找，不当 TUI
                if name == "cmd.exe" || name == "conhost.exe" || name == "powershell.exe" {
                    next.push(parts[1].parse::<u32>().unwrap_or(0));
                    continue;
                }
                if tui_names.contains(&name) {
                    tui = parts[1].parse().ok();
                    break 'outer;
                }
            }
        }
        frontier = next;
    }
    let target = tui.unwrap_or(cmd); // 同一控制台，cmd 兜底可注入（M6R 同款）
    ev.log(&format!("launch {cli}: tui={tui:?} target={target}"));
    ProbeProc {
        hosts: vec![conhost],
        cmd,
        target,
        proj,
        killed: false,
    }
}

/// WT 前置自检：`where wt` 探测 Windows Terminal（生产 `resume::windows_terminal_
/// path` 同款语义——该函数 crate 私有，测试侧同源复制，注释锚定单一来源；缓存
/// **首行完整路径**，避开应用执行别名停用/损坏场景，同评审 M1 口径）。不在场 →
/// panic 带明确中文提示（本例为实机显式跑场景，前置缺失即响亮失败可接受）。
fn wt_path_or_panic() -> String {
    use std::process::Stdio;
    let out = std::process::Command::new("where")
        .arg("wt")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("where wt 探测执行失败");
    let path = if out.status.success() {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    } else {
        None
    };
    path.unwrap_or_else(|| panic!("本机未装 Windows Terminal，无法验收 WT 宿主"))
}

/// WT 宿主下按「既有进程发现口径」定位探测进程（monitor::process 同款 sysinfo
/// 扫描——进程名 + cwd 匹配）。wt.exe 启动器移交 WindowsTerminal.exe 后即退，
/// conhost 拓扑的「从启动 pid 走父子树」不可用；且 cmd 的 CommandLine 是裸
/// `cmd /k <cli>`（不含目录），故以 **cwd 含探测标记** 锚定：`wt -d` 落目录经
/// 进程继承传导（cmd → TUI），凡 cwd 含标记者必属本探测树（标记含 run-id，
/// 跨运行/跨用户会话零碰撞）。返回 `(cmd pid, TUI pid)`：TUI 命中为注入目标；
/// cmd 全程只作清场根（WT 下杀 TUI 树不关标签页——/k 壳存活，必须杀 cmd 根）
/// 与 TUI 缺席时的回落目标。单次扫描同时收两类候选（sysinfo 进程表迭代序不定，
/// 不可命中 TUI 即早退，须整表扫完才下结论）。
fn find_wt_target(
    cli: &str,
    marker: &str,
    ev: &Ev,
    timeout: Duration,
) -> (Option<u32>, Option<u32>) {
    let tui_names = [
        format!("{cli}.exe"),
        "node.exe".to_string(),
        "bun.exe".to_string(),
    ];
    let t0 = Instant::now();
    loop {
        let system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_processes(
                sysinfo::ProcessRefreshKind::nothing()
                    .with_cmd(sysinfo::UpdateKind::Always)
                    .with_cwd(sysinfo::UpdateKind::Always)
                    .with_exe(sysinfo::UpdateKind::Always),
            ),
        );
        let mut cmd_pid = None;
        let mut tui_pid = None;
        for (pid, process) in system.processes() {
            let Some(cwd) = process.cwd() else {
                continue;
            };
            if !norm_path(&cwd.to_string_lossy()).contains(&norm_path(marker)) {
                continue;
            }
            let name = process.name().to_string_lossy().to_ascii_lowercase();
            if tui_names.contains(&name) && tui_pid.is_none() {
                tui_pid = Some(pid.as_u32());
            } else if name == "cmd.exe" && cmd_pid.is_none() {
                cmd_pid = Some(pid.as_u32());
            }
        }
        if tui_pid.is_some() || t0.elapsed() >= timeout {
            if let Some(t) = tui_pid {
                ev.log(&format!(
                    "wt target 命中：tui={t} cmd={cmd_pid:?}（{}ms）",
                    t0.elapsed().as_millis()
                ));
            } else {
                ev.log(&format!(
                    "wt target 发现超时（cmd 回落={cmd_pid:?}，marker={marker}）"
                ));
            }
            return (cmd_pid, tui_pid);
        }
        std::thread::sleep(Duration::from_millis(1200));
    }
}

/// 起 WT 宿主探测会话：`wt -d <proj> cmd /k <cli>`（Task 11 一键 resume 生产
/// 同款命令形态；Start-Process 另设 -WorkingDirectory，同 resume.rs 生产 spawner
/// 「-d + current_dir 双保险」语义）。目标定位走 [`find_wt_target`]（sysinfo
/// cwd 标记匹配），清场根 = cmd（杀 TUI 树不关 WT 标签页，见其注）。
fn launch_wt_cli(wt_path: &str, cli: &str, tag: &str, ev: &Ev) -> ProbeProc {
    let proj = std::env::temp_dir().join(format!("mam-m9r-{tag}-{cli}"));
    let _ = std::fs::remove_dir_all(&proj); // run-id 唯一，清理仅防理论碰撞
    std::fs::create_dir_all(&proj).expect("建探测项目目录失败");

    let script = format!(
        "Start-Process '{}' -ArgumentList '-d','{}','cmd.exe','/k','{cli}' -WorkingDirectory '{}' -PassThru | Select-Object -ExpandProperty Id",
        wt_path,
        proj.display(),
        proj.display()
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("启动 powershell 失败");
    let wt_pid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| {
            panic!(
                "wt 启动失败（Start-Process 未返回 pid，stderr={}）",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        });
    ev.log(&format!(
        "launch wt {cli}: wt_pid={wt_pid}（启动器移交 WindowsTerminal 后即退，仅留档）proj={}",
        proj.display()
    ));

    let marker = proj
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .expect("探测目录名");
    let (cmd, tui) = find_wt_target(cli, &marker, ev, WT_FIND_TIMEOUT);
    let cmd = cmd.unwrap_or_else(|| {
        panic!("{cli}: WT 宿主 {WT_FIND_TIMEOUT:?} 内未定位到探测进程（cwd 含标记 {marker}）")
    });
    let target = tui.unwrap_or(cmd);
    ev.log(&format!(
        "launch wt {cli}: cmd={cmd} tui={tui:?} target={target}"
    ));
    ProbeProc {
        hosts: Vec::new(), // WT 宿主进程绝不代杀（纪律见 ProbeProc.hosts 注）
        cmd,
        target,
        proj,
        killed: false,
    }
}

/// 首启信任/确认对话框预热（M6R E0 实证按键序列，经注入通道自身处理）：
/// claude: VT↓+Enter（+补一颗 Enter 防双弹窗）；codex: VK Enter ×2（E0 两张
/// after-trust 截图证明需两颗）；kimi/opencode: Enter。按键进控制台输入缓冲
/// 排队，TUI 未就绪时被后续读取消费——对启动时序天然容错。
/// 空输入行上的 Enter 是无害空提交，预热多按不污染会话正文。
fn warmup_trust(proc: &ProbeProc, cli: &str, spec: &FamilySpec, ev: &Ev) {
    let pid = proc.target;
    let keys: Vec<&str> = match cli {
        "claude" => vec!["down", "enter", "enter"],
        "codex" => vec!["enter", "enter"],
        _ => vec!["enter"],
    };
    for k in keys {
        match inject_key_spec(pid, k, spec) {
            Ok(()) => ev.log(&format!("warmup {cli}: key={k} ok")),
            Err(e) => ev.log(&format!("warmup {cli}: key={k} err={e}")), // 预热 best-effort
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    std::thread::sleep(Duration::from_millis(2500)); // 弹窗关闭沉降
}

// ============================================================
// 注入文本构造与确认轮询（复用 read_session_messages + stamp 语义）
// ============================================================

/// 构造 E2E 注入文本：`p` 填充 + 尾部唯一标记（run-id 随行）。
/// 尾部标记保证 `stamp_of` 的 24 字符尾戳具有区分度（尾戳=全量送达判据）。
fn e2e_text(tool: &str, run_id: &str, total_chars: usize) -> String {
    let tail = format!("M9R-E2E-{tool}-{run_id}-END");
    let pad = total_chars.saturating_sub(tail.chars().count());
    format!("{}{}", "p".repeat(pad), tail)
}

/// 确认轮询（复用生产读路径）：`read_session_messages`（八工具统一出口，只读
/// 真实 CLI 会话存储）+ `stamp_hit_in_page`（user 侧过滤 + 含戳）——与
/// `confirm::session_stamp_hit` 同语义。命中 true / 超时 false。
fn poll_stamp(ev: &Ev, tool: &str, sid: &str, stamp: &str, timeout: Duration, tag: &str) -> bool {
    let t0 = Instant::now();
    loop {
        match read_session_messages(tool, sid, PROBE_LIMIT) {
            Ok(pg) => {
                if stamp_hit_in_page(&pg, stamp) {
                    ev.log(&format!(
                        "{tag}: stamp HIT（耗时 {}ms）",
                        t0.elapsed().as_millis()
                    ));
                    return true;
                }
            }
            Err(e) => ev.log(&format!("{tag}: read err={e}")),
        }
        if t0.elapsed() >= timeout {
            ev.log(&format!(
                "{tag}: stamp 超时未命中（{}ms）",
                timeout.as_millis()
            ));
            return false;
        }
        std::thread::sleep(POLL_GAP);
    }
}

/// 首行 JSON 取字段（会话 id 发现用）。
fn first_line_field(path: &Path, field: &str) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let head = &data[..data.len().min(256 * 1024)];
    let text = String::from_utf8_lossy(head);
    let line = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get(field).and_then(|x| x.as_str()).map(str::to_string)
}

/// 路径归一（跨工具 workDir/目录名匹配）：小写 + 斜杠统一。
fn norm_path(s: &str) -> String {
    s.replace('/', "\\").to_ascii_lowercase()
}

/// 会话 id 发现轮询：工具派发，直至超时（会话文件/索引落盘是异步的）。
fn discover_session_id(
    ev: &Ev,
    tool: &str,
    marker: &str,
    timeout: Duration,
    tag: &str,
) -> Option<String> {
    let t0 = Instant::now();
    loop {
        let found = match tool {
            "claude" => discover_claude_sid(marker),
            "codex" => discover_codex_sid(marker, ev),
            "kimi" => discover_kimi_sid(marker),
            "opencode" => discover_opencode_sid(marker, ev),
            _ => None,
        };
        if let Some(sid) = found {
            ev.log(&format!(
                "{tag}: sid={sid}（{}ms）",
                t0.elapsed().as_millis()
            ));
            return Some(sid);
        }
        if t0.elapsed() >= timeout {
            ev.log(&format!("{tag}: 会话 id 发现超时（{tool}）"));
            return None;
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// claude：`~/.claude/projects/` 下目录名含探测标记的项目目录 → 最新 .jsonl
/// → 首行 sessionId（目录名编解码规则即 Claude Code 自身的逐字符 munging，
/// 探测标记全为字母数字与 '-'，munging 后原样保留，contains 直配）。
fn discover_claude_sid(marker: &str) -> Option<String> {
    let projects = dirs::home_dir()?.join(".claude").join("projects");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in std::fs::read_dir(&projects).ok()?.flatten() {
        if !dir.file_name().to_string_lossy().contains(marker) {
            continue;
        }
        for f in std::fs::read_dir(dir.path()).ok()?.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(mt) = f.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let replace = match &best {
                None => true,
                Some((t, _)) => mt > *t,
            };
            if replace {
                best = Some((mt, p));
            }
        }
    }
    let (_, path) = best?;
    first_line_field(&path, "sessionId")
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()))
}

/// codex：今日 `~/.codex/sessions/<Y>/<M>/<D>/rollout-*.jsonl` 按 mtime 倒序，
/// 首行 session_meta 的 cwd 含探测标记 → 取 payload.id（历史会话是大库，
/// 只扫今日目录守扫描预算；探测标记唯一性保证不撞用户真实会话）。
fn discover_codex_sid(marker: &str, ev: &Ev) -> Option<String> {
    let day = dirs::home_dir()?
        .join(".codex")
        .join("sessions")
        .join(chrono::Local::now().format("%Y/%m/%d").to_string());
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for f in std::fs::read_dir(&day).ok()?.flatten() {
        let p = f.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with("rollout") && name.ends_with(".jsonl") {
            if let Ok(mt) = f.metadata().and_then(|m| m.modified()) {
                files.push((mt, p));
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // 最新在前
    for (_, path) in files {
        // 首行 session_meta：payload.id 为会话 id，payload.cwd 含探测标记
        // 读失败（竞态写半行/锁冲突等）→ 跳过该文件继续下一轮，不中断整轮发现
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let head = String::from_utf8_lossy(&data[..data.len().min(256 * 1024)]).to_string();
        let Some(line) = head.lines().next() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let cwd = v
            .pointer("/payload/cwd")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let id = v
            .pointer("/payload/id")
            .and_then(|i| i.as_str())
            .unwrap_or("");
        if !id.is_empty() && (norm_path(cwd).contains(&norm_path(marker)) || head.contains(marker))
        {
            ev.log(&format!("codex rollout 命中：{}", path.display()));
            return Some(id.to_string());
        }
    }
    None
}

/// kimi：`session_index.jsonl`（KIMI_CODE_HOME 优先，回退 ~/.kimi-code / ~/.kimi，
/// 与 resolve_data_root 同源规则）逐行 {sessionId, sessionDir, workDir}，
/// workDir 含探测标记 → 取该行 sessionId（索引追加序，取最后命中）。
fn discover_kimi_sid(marker: &str) -> Option<String> {
    let env_home = std::env::var("KIMI_CODE_HOME")
        .ok()
        .filter(|h| !h.is_empty());
    let home = match env_home {
        Some(h) => PathBuf::from(h),
        None => {
            let user = dirs::home_dir()?;
            let primary = user.join(".kimi-code");
            if primary.join("sessions").exists() {
                primary
            } else {
                user.join(".kimi")
            }
        }
    };
    let text = std::fs::read_to_string(home.join("session_index.jsonl")).ok()?;
    let mut hit = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let work = v.get("workDir").and_then(|w| w.as_str()).unwrap_or("");
        let sid = v.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
        if !sid.is_empty() && norm_path(work).contains(&norm_path(marker)) {
            hit = Some(sid.to_string());
        }
    }
    hit
}

/// opencode：拷贝 `~/.local/share/opencode/opencode.db`（+wal+shm）副本后查询
/// （WAL 活库不可直查，探测纪律；副本本身落在证据目录留档）。匹配列是
/// `session.directory`（真实 cwd；project.worktree 是另一维度，实测多为 '/'，
/// 不可用——2026-09-19 活体实验教训），路径为正斜杠形态，经 norm_path 归一
/// 后 contains 探测标记，按 time_updated 倒序取最新命中。
fn discover_opencode_sid(marker: &str, ev: &Ev) -> Option<String> {
    let db = dirs::home_dir()?
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    let copy = ev.dir.join("opencode.db");
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", db.display()));
        let dst = PathBuf::from(format!("{}{suffix}", copy.display()));
        if std::fs::copy(&src, &dst).is_err() && suffix.is_empty() {
            return None; // 主库缺失
        }
    }
    let conn = rusqlite::Connection::open(&copy).ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, directory FROM session \
             ORDER BY time_updated DESC LIMIT 100",
        )
        .ok()?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    rows.into_iter()
        .find(|(_, directory)| norm_path(directory).contains(&norm_path(marker)))
        .map(|(id, _)| id)
}

// ============================================================
// 单例装配：起会话 → 预热（共用前摇）；sid 发现在注入之后
// ============================================================

/// 起会话 + 等待 TUI 就绪 + 信任预热。返回进程树。
///
/// **sid 发现在注入之后**（2026-09-19 活体实验定案）：四家 CLI 的会话存储均为
/// **懒创建**——claude 项目目录/codex rollout 在首条用户消息提交后才落盘
/// （启动后 25s 无对话框确认无文件；信任框确认 + 空回车也不会创建），故
/// 「先注入、后发现 sid、再轮询 stamp」是唯一可行顺序。
fn boot_probe_session(cli: &str, tag: &str, ev: &Ev) -> ProbeProc {
    let spec = family_for(cli).unwrap_or_else(|| panic!("{cli} 族规格缺失（families 表）"));
    let proc = launch_cli(cli, tag, ev);
    // TUI 冷启动绘制 + 信任对话框弹出窗（ffi_hop 前摇 + M6R E0 经验值）
    std::thread::sleep(Duration::from_secs(8));
    warmup_trust(&proc, cli, &spec, ev);
    proc
}

/// WT 宿主版起会话（节奏与 [`boot_probe_session`] 全同）：launch_wt_cli + TUI
/// 冷启动沉降 + 信任预热。WT 与 conhost 差异仅在宿主窗口层，ConPTY 输入面同构，
/// 预热按键序列/沉降窗复用同一套经验值。
fn boot_wt_session(wt_path: &str, cli: &str, tag: &str, ev: &Ev) -> ProbeProc {
    let spec = family_for(cli).unwrap_or_else(|| panic!("{cli} 族规格缺失（families 表）"));
    let proc = launch_wt_cli(wt_path, cli, tag, ev);
    std::thread::sleep(Duration::from_secs(8));
    warmup_trust(&proc, cli, &spec, ev);
    proc
}

/// 探测项目目录的唯一标记（会话存储匹配锚点：run-id 随行，跨运行不撞）。
fn proj_marker(proc: &ProbeProc) -> String {
    proc.proj
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .expect("探测目录名")
}

/// 等待 claude 回合结束（Test 3 专用）：预热消息会触发一次真实模型回合，
/// 轮询会话 JSONL 体积，连续 `IDLE_STABLE_SECS`(20s) 无增长即判「输入框已交回」。
/// **误判模式披露**：回合中途出现 >20s 的静默间隙会被误判为结束——失败形态是
/// 后续 POST 的 delivered 断言**响亮失败**（确认层超时回执），非静默污染，
/// 重跑即愈。判不齐时（模型流式间隔更长）到时放行并告警，兜底同上。
fn wait_claude_idle(ev: &Ev, marker: &str, max_wait: Duration, tag: &str) {
    const IDLE_STABLE_SECS: u64 = 20;
    let projects = dirs::home_dir()
        .expect("home")
        .join(".claude")
        .join("projects");
    // 找到本项目目录下 **mtime 最新** 的 jsonl（与 discover_claude_sid 同一口径，
    // 防 claude sidechain/子代理等旁路文件被误选为监听对象）
    let mut jsonl: Option<(std::time::SystemTime, PathBuf)> = None;
    for _ in 0..10 {
        for dir in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
            if !dir.file_name().to_string_lossy().contains(marker) {
                continue;
            }
            for f in std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(mt) = f.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                let replace = match &jsonl {
                    None => true,
                    Some((t, _)) => mt > *t,
                };
                if replace {
                    jsonl = Some((mt, p));
                }
            }
        }
        if jsonl.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    let Some((_, path)) = jsonl else {
        ev.log(&format!("{tag}: wait_claude_idle 未找到会话文件（放行）"));
        return;
    };
    let t0 = Instant::now();
    let mut last = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let cur = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(last);
        if cur != last {
            last = cur;
            since = Instant::now();
        } else if since.elapsed().as_secs() >= IDLE_STABLE_SECS {
            ev.log(&format!(
                "{tag}: claude 回合判定结束（文件稳定 {IDLE_STABLE_SECS}s，{}ms）",
                t0.elapsed().as_millis()
            ));
            return;
        }
        if t0.elapsed() >= max_wait {
            ev.log(&format!("{tag}: wait_claude_idle 到时放行（{max_wait:?}）"));
            return;
        }
    }
}

// ============================================================
// Test 1 —— 引擎矩阵短消息：四家 CLI 背压旗标 + 全量 stamp 命中
// ============================================================

#[test]
#[ignore = "实机显式跑：四家 CLI 真会话（conhost 探测拓扑 + 真会话存储确认）"]
fn e2e_engine_matrix_short() {
    let run_id = format!("{}-t1", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("engine-matrix-short", &run_id);
    ev.log(&format!("=== e2e_engine_matrix_short run={run_id} ==="));
    // (tool, 期望背压旗标)：claude/kimi/codex 快消费者 200 字符内不背压；
    // opencode 慢消费者任意长度背压（families::use_backpressure / R2-1）
    let cases = [
        ("claude", false),
        ("codex", false),
        ("kimi", false),
        ("opencode", true),
    ];
    let mut failures: Vec<String> = Vec::new();
    for (tool, want_bp) in cases {
        let tag = format!("short-{tool}-{run_id}");
        let r: Result<(), String> = (|| {
            let mut proc = boot_probe_session(tool, &tag, &ev);
            // 150 字符与计划的 200 字符语义等价：快家族背压阈值 >2000（任意
            // ≤2000 长度不背压）、慢家族恒背压——长度在本档内断言等价
            let text = e2e_text(tool, &run_id, 150);
            assert!(text.chars().count() < 200, "短文用例必须 <200 字符");
            let spec = family_for(tool).unwrap();
            let stamp = stamp_of(&text).to_string();
            ev.log(&format!("{tag}: 注入 {} 字符", text.chars().count()));

            let stats = inject_text_spec(proc.target, &text, &spec)
                .map_err(|e| format!("{tool} inject_text_spec 失败: {e}"))?;
            ev.log(&format!("{tag}: stats={stats:?}"));
            // 背压旗标按族规格断言：快消费者=false，opencode 慢消费者=true
            if stats.backpressure != want_bp {
                return Err(format!(
                    "{tool} 背压旗标不符：got={} want={want_bp}（stats={stats:?}）",
                    stats.backpressure
                ));
            }
            if stats.written != text.chars().count() {
                return Err(format!("{tool} written={} ≠ 全量", stats.written));
            }
            // sid 发现在注入之后（会话存储懒创建），随后确认层轮询 stamp
            // （opencode 慢消费者统一走 STAMP_TIMEOUT_SLOW 长窗）
            let marker = proj_marker(&proc);
            let sid = discover_session_id(&ev, tool, &marker, DISCOVER_TIMEOUT, &tag).ok_or_else(
                || format!("{tool} 注入后未发现会话 id（marker={marker}，{DISCOVER_TIMEOUT:?}）"),
            )?;
            let timeout = if tool == "opencode" {
                STAMP_TIMEOUT_SLOW
            } else {
                STAMP_TIMEOUT_FAST
            };
            let hit = poll_stamp(&ev, tool, &sid, &stamp, timeout, &tag);
            proc.kill();
            if !hit {
                return Err(format!("{tool} stamp 未命中（sid={sid}）"));
            }
            Ok(())
        })();
        if let Err(e) = r {
            ev.log(&format!("{tag}: FAIL {e}"));
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "四家引擎矩阵短消息存在失败：{failures:#?}"
    );
}

// ============================================================
// Test 2 —— 长文预算：claude 10k 快路径 <15s；opencode 10k 背压 ≤460s
// ============================================================

#[test]
#[ignore = "实机显式跑：长文 10k 真会话（opencode 背压实测分钟级，耐心等待）"]
fn e2e_engine_matrix_long() {
    let run_id = format!("{}-t2", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("engine-matrix-long", &run_id);
    ev.log(&format!("=== e2e_engine_matrix_long run={run_id} ==="));
    let mut failures: Vec<String> = Vec::new();

    // ① claude 10000 字符（快消费者长文 >2000 切背压排水，仍须 <15s）
    {
        let tag = format!("long-claude-{run_id}");
        let mut proc = boot_probe_session("claude", &tag, &ev);
        let text = e2e_text("claude", &run_id, 10_000);
        assert_eq!(text.chars().count(), 10_000);
        let spec = family_for("claude").unwrap();
        let stamp = stamp_of(&text).to_string();
        let t0 = Instant::now();
        let r = inject_text_spec(proc.target, &text, &spec);
        let elapsed = t0.elapsed();
        ev.log(&format!(
            "{tag}: elapsed={}ms r={:?}",
            elapsed.as_millis(),
            r.as_ref().map(|s| format!("{s:?}"))
        ));
        match r {
            Ok(stats) => {
                if elapsed.as_millis() >= 15_000 {
                    failures.push(format!(
                        "claude 10k 耗时 {}ms ≥ 15s 预算",
                        elapsed.as_millis()
                    ));
                }
                if !stats.backpressure {
                    failures.push("claude 10k 应走背压（>2000 长文阈值）".to_string());
                }
            }
            Err(e) => failures.push(format!("claude 10k 注入失败: {e}")),
        }
        // sid 发现在注入之后（会话存储懒创建），随后确认层轮询 stamp
        let marker = proj_marker(&proc);
        let sid = discover_session_id(&ev, "claude", &marker, DISCOVER_TIMEOUT, &tag)
            .unwrap_or_else(|| panic!("claude 10k 注入后未发现会话 id（marker={marker}）"));
        let hit = poll_stamp(&ev, "claude", &sid, &stamp, STAMP_TIMEOUT_FAST, &tag);
        proc.kill();
        if !hit {
            failures.push(format!("claude 10k stamp 未命中（sid={sid}）"));
        }
    }

    // ② opencode 10000 字符（慢消费者背压路径；总耗时 ≤ 460s 预算 =
    //    families::inject_budget_ms(opencode, 10000) = 10s + 10000×45ms）
    {
        let tag = format!("long-opencode-{run_id}");
        let mut proc = boot_probe_session("opencode", &tag, &ev);
        let text = e2e_text("opencode", &run_id, 10_000);
        let spec = family_for("opencode").unwrap();
        let budget = multi_agents_manager_lib::inject::families::inject_budget_ms(&spec, 10_000);
        let stamp = stamp_of(&text).to_string();
        let t0 = Instant::now();
        let r = inject_text_spec(proc.target, &text, &spec);
        let elapsed = t0.elapsed();
        ev.log(&format!(
            "{tag}: elapsed={}ms r={:?}",
            elapsed.as_millis(),
            r.as_ref().map(|s| format!("{s:?}"))
        ));
        match r {
            Ok(stats) => {
                if elapsed.as_millis() as u64 > budget {
                    failures.push(format!(
                        "opencode 10k 耗时 {}ms > 背压总预算 {budget}ms",
                        elapsed.as_millis()
                    ));
                }
                if !stats.backpressure {
                    failures.push("opencode 10k 应走背压（慢消费者恒节流）".to_string());
                }
            }
            Err(e) => failures.push(format!("opencode 10k 注入失败: {e}")),
        }
        // sid 发现在注入之后（会话存储懒创建），随后确认层轮询 stamp
        let marker = proj_marker(&proc);
        let sid = discover_session_id(&ev, "opencode", &marker, DISCOVER_TIMEOUT, &tag)
            .unwrap_or_else(|| panic!("opencode 10k 注入后未发现会话 id（marker={marker}）"));
        let hit = poll_stamp(&ev, "opencode", &sid, &stamp, STAMP_TIMEOUT_SLOW, &tag);
        proc.kill();
        if !hit {
            failures.push(format!("opencode 10k stamp 未命中（sid={sid}）"));
        }
    }

    assert!(failures.is_empty(), "长文预算矩阵存在失败：{failures:#?}");
}

// ============================================================
// Test 3 —— HTTP 全链闭环：真 HTTP → 路由 → 队列 → 引擎 → 确认 → 审计
// ============================================================

#[tokio::test]
#[ignore = "实机显式跑：真 claude TUI 会话 + 真 HTTP 服务器全链闭环"]
async fn e2e_http_full_chain() {
    use multi_agents_manager_lib::inject::normalize::compose_injection;
    use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
    use multi_agents_manager_lib::remote::pin::PinRateLimiter;
    use multi_agents_manager_lib::remote::server::{router, RemoteState, SseRegistry};

    let run_id = format!("{}-t3", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("http-full-chain", &run_id);
    ev.log(&format!("=== e2e_http_full_chain run={run_id} ==="));

    // ① 真实 claude TUI 探测会话（temp 项目目录；引擎注入后 claude 立即将
    //    用户消息写入真实 JSONL，真 confirm_probe 可命中——不伪造确认层）。
    //    会话 JSONL 懒创建（首条用户消息后才落盘）→ 先直发一条预热消息创建
    //    会话拿到 sid，并等模型回合结束（输入框交回），再走 HTTP 链投递正主。
    let mut proc = boot_probe_session("claude", &format!("http-chain-{run_id}"), &ev);
    let marker = proj_marker(&proc);
    let spec = family_for("claude").unwrap();
    let warm = format!("M9R-E2E3 warm-up; run={run_id}; reply briefly then idle");
    let warm_stats = inject_text_spec(proc.target, &warm, &spec)
        .expect("Test 3 预热消息注入失败（会话创建前置）");
    ev.log(&format!("http-chain: 预热注入 stats={warm_stats:?}"));
    let sid = discover_session_id(&ev, "claude", &marker, DISCOVER_TIMEOUT, "http-chain")
        .expect("Test 3 预热后未发现 claude 会话 id");
    wait_claude_idle(&ev, &marker, Duration::from_secs(240), "http-chain");
    let proj = proc.proj.display().to_string();

    // ② 假 session_source 指向该真实会话（pid/agent_type/project_path 取自
    //    真实会话；status=Waiting 触发直发路径）；其余缝按生产装配/测试先例
    let session = multi_agents_manager_lib::session::Session {
        id: sid.clone(),
        agent_type: multi_agents_manager_lib::session::AgentType::Claude,
        project_name: "mam-e2e-http".into(),
        project_path: proj.clone(),
        title: None,
        git_branch: None,
        github_url: None,
        status: multi_agents_manager_lib::session::SessionStatus::Waiting,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: chrono::Utc::now().to_rfc3339(),
        pid: proc.target,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: multi_agents_manager_lib::session::ProcessForm::Cli,
        jump_supported: false,
        unread: false,
    };
    let sessions = vec![session];
    let total = sessions.len();
    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        // L13 靶向证据缝：空证据 = 无候选 → 靶向无歧义（本套件不构造同 cwd 多实例）
        target_evidence: Box::new(|_, _| {
            multi_agents_manager_lib::window::tty_map::TargetEvidence::default()
        }),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(
            move || multi_agents_manager_lib::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: total,
                waiting_count: 0,
            },
        ),
        store: DeviceStore::memory(), // 内存库——零接触真实 ~/.tuvis/tuvis.db
        injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        // C6：create 缝束（任务簿+发现/pid/工具探测/步距缝）——本 E2E 不触
        // /session-create，注测试 stub（零真实 FS 零真实睡眠）
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        // 历史会话区缝（spec 2026-09-20-mobile-archive-history §6.1）：E2E 不触归档
        // 路径，注空桩。本文件 #![cfg(windows)]——macOS 开发机上编译为空，漏补会在
        // Windows 测试构建上 E0063 missing fields（评审 Critical，2026-09-20）
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        // 真 confirm_probe（生产同源装配：读路径 + 尾戳 user 侧命中）
        confirm_probe: Arc::new(|tool: &str, s: &str, stamp: &str| -> bool {
            read_session_messages(tool, s, PROBE_LIMIT)
                .map(|pg| stamp_hit_in_page(&pg, stamp))
                .unwrap_or(false)
        }),
        host_source: Box::new(|| serde_json::Value::Null),
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
        // 全局 PIN 限速桶（#119 的 remote-gate-zero-exemption 新增；取值照抄其余各处惯例）
        global_pin_limiter: std::sync::Mutex::new(PinRateLimiter::global()),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        // 配对限速的「通道来源」缝（#119 取代了旧的 `tunnel_hosts_source`）。
        // **类型形状与旧字段不同，不是纯改名**：旧的 `Fn() -> Option<Vec<String>>`，
        // 新的 `Fn() -> Vec<RateBucketChannel>`（**没有 Option**、元素类型也不同）。
        // 空表 = 无任何已声明通道 ⇒ 取权威头一律 fail-closed；取值写法照抄其余各处惯例。
        rate_bucket_channels_source: Box::new(Vec::new),
        via_hosts_source: Box::new(|| None),
        home_source: Box::new(|| None),
        // ⚠️ **改 `RemoteState` 的字段必须同步本文件**：本文件首行是 `#![cfg(windows)]`，
        // 在 Linux 上整个文件编译成**空测试二进制** —— 本机 `cargo test` 与 CI 的所有
        // Linux 步骤**都碰不到它**，唯一会编译它的是 CI 的 **Windows 交叉门禁**
        // （`cargo check/clippy --target x86_64-pc-windows-gnu --all-targets`）。
        // 2026-10-07 就是在这里栽的：`tunnel_hosts_source` 退役 + `global_pin_limiter`
        // 新增，本文件没跟着改，而该门禁对 #119 **从未执行过** ⇒ 静默潜伏到 Windows
        // 交叉门禁第一次真跑（E0560）。改字段时请连同本文件一起 grep。
        // C7：配对计数缝——本 E2E 不触配对打标/提示路径，注空表桩（零真实进程扫描）
        pairing_counter: Box::new(Vec::new),
        // 丁T3：对话框在场探针——实机 E2E 装配**生产同源实现**（真屏读，非假体）；
        // 本用例不触控制类注入守卫，此处只为构造完整性
        dialog_probe: Arc::new(|_sid: &str, pid: u32| {
            multi_agents_manager_lib::inject::dialog::probe_screen_dialog(pid)
        }),
        // 丁T5：问答阶段机的屏读**能力**缝——实机 E2E 走真实屏读（这条测试本来就
        // 需要一个真 conhost 目标进程；非 Windows 上 `read_screen_window` 不存在，
        // 故与生产装配同构：`#[cfg(windows)]` 直调，其它平台恒 None）。
        screen_probe: {
            #[cfg(windows)]
            {
                Arc::new(|_sid: &str, pid: u32| {
                    multi_agents_manager_lib::inject::e2e_support::read_screen_lines(pid)
                })
            }
            #[cfg(not(windows))]
            {
                Arc::new(|_sid: &str, _pid: u32| None)
            }
        },
    });

    // 配对设备（server.rs 既有 persist_named_device 先例）：cookie 直指内存库设备行
    state
        .store
        .with(|c| {
            persist_device(
                c,
                &NewDevice {
                    id: "e2e-dev".into(),
                    name: "E2E手机".into(),
                    ua: "ua-e2e".into(),
                    origin_ip: "ip-e2e".into(),
                    via: String::new(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
        })
        .expect("预置配对设备失败");

    // ③ 真 HTTP 服务器（127.0.0.1 随机端口；ConnectInfo 由
    //    into_make_service_with_connect_info 注入——gate 本机豁免判定数据源）
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
    ev.log(&format!("HTTP 服务器就绪 127.0.0.1:{port}"));

    // ④ POST /m/api/v1/session-send（配对 cookie 先例 + 短文本）
    let text = format!(
        "M9R-E2E3 HTTP full-chain probe; run={run_id}; TAIL-{:04}",
        std::process::id() % 10000
    );
    let body = serde_json::json!({ "sessionId": sid, "text": text });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("reqwest client 构建失败");
    let resp = client
        .post(format!("http://127.0.0.1:{port}/m/api/v1/session-send"))
        .header("cookie", "mam_device=e2e-dev")
        .json(&body)
        .send()
        .await
        .expect("session-send 请求失败");
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    ev.log(&format!("session-send 响应：{status} {payload}"));
    assert_eq!(status, 200, "session-send 应 200：{payload}");
    assert_eq!(
        payload["status"], "delivered",
        "可输入态直发应 delivered：{payload}"
    );

    // ⑤ 目标会话文件命中 stamp（确认层已在服务端命中过——此处独立复核落盘）。
    //    poll_stamp 为同步轮询：此处服务端投递已完成（响应已回），同步阻塞
    //    轮询只读文件，不与运行时任务争抢（安全）
    // 丁T3 裁2：compose 产出 `{正文} [mobile E2E手机]`（签名后置）；stamp_of 内部
    // 剥尾签名后取正文尾 24 字符——本处直接喂 composed 即验证了那条适配在真实
    // 会话文件上成立（假命中防护见 inject::confirm 的
    // stamp_never_false_hits_across_same_device_messages）
    let composed = compose_injection("E2E手机", &text).expect("E2E 花名在白名单内（裁决 24b）");
    let stamp = stamp_of(&composed).to_string();
    let hit = poll_stamp(
        &ev,
        "claude",
        &sid,
        &stamp,
        STAMP_TIMEOUT_FAST,
        "http-chain",
    );
    proc.kill();
    server.abort();
    assert!(hit, "目标 claude 会话文件应命中 stamp（sid={sid}）");

    // ⑥ 审计 send 与 flush 两行落库（内存库 recent_conn；settle 落 flush、
    //    端点落 send，channel = injector.name() = "real"）
    let audits = state
        .store
        .with(|c| multi_agents_manager_lib::database::dao::write_audit::recent_conn(c, 10));
    for a in &audits {
        ev.log(&format!(
            "audit: action={} result={} channel={} session={}",
            a.action, a.result, a.channel, a.session_id
        ));
    }
    assert!(
        audits
            .iter()
            .any(|a| a.action == "send" && a.result == "ok" && a.channel == "real"),
        "应存在 action=send result=ok channel=real 的审计行：{audits:?}"
    );
    assert!(
        audits
            .iter()
            .any(|a| a.action == "flush" && a.result == "ok" && a.session_id == sid),
        "应存在直发落账 action=flush result=ok 的审计行：{audits:?}"
    );
}

// ============================================================
// Test 4 —— 键域与回车：codex 短文本 + VK 回车提交生效 + 域外拒绝
// ============================================================

#[test]
#[ignore = "实机显式跑：codex 真会话（B 族 VK 回车提交 + 键域校验）"]
fn e2e_key_domain_and_enter() {
    let run_id = format!("{}-t4", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("key-domain-enter", &run_id);
    ev.log(&format!("=== e2e_key_domain_and_enter run={run_id} ==="));
    let mut proc = boot_probe_session("codex", &format!("key-domain-{run_id}"), &ev);
    let spec = family_for("codex").unwrap();

    // ① 短文本 + VK 回车（inject_text_spec 自带提交回车）：codex rollout 命中文本
    let text = "echo M9R-E2E-KEY-OK";
    let stamp = stamp_of(text).to_string();
    let r = inject_text_spec(proc.target, text, &spec);
    ev.log(&format!("key-domain: 注入 r={r:?}"));
    assert!(r.is_ok(), "codex 短文本注入应成功：{r:?}");
    // sid 发现在注入之后（rollout 懒创建）
    let marker = proj_marker(&proc);
    let sid = discover_session_id(&ev, "codex", &marker, DISCOVER_TIMEOUT, "key-domain")
        .expect("codex 注入后未发现 rollout 会话 id");
    let hit = poll_stamp(&ev, "codex", &sid, &stamp, STAMP_TIMEOUT_FAST, "key-domain");
    assert!(hit, "codex rollout JSONL 应命中文本（sid={sid}）");

    // ② 域外拒绝：bad! 不在键域（enter/esc/tab/单字符字母数字/方向键），
    //    在取锁/附加之前快速失败（P2-2），不回退文本+回车
    let bad = inject_key_spec(proc.target, "bad!", &spec);
    ev.log(&format!("key-domain: 域外 r={bad:?}"));
    let err = bad.expect_err("域外键应被拒绝");
    assert!(err.contains("不支持的按键"), "域外拒绝文案不符：{err}");

    // 单一清场点：置于全部断言之后；域校验先行（P2-2）使②的断言与进程
    // 存活性无关（域外键不触任何控制台附加，活/死 pid 结果一致）
    proc.kill();
}

// ============================================================
// Test 5（F5）—— WT 宿主矩阵：Windows Terminal × 2000 字符 × claude/codex
// ============================================================

#[test]
#[ignore = "实机显式跑：真 Windows Terminal 开窗 + claude/codex 真会话（--test-threads=1）"]
fn e2e_wt_host_matrix() {
    let run_id = format!("{}-t5", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("wt-host-matrix", &run_id);
    ev.log(&format!("=== e2e_wt_host_matrix run={run_id} ==="));
    // 前置自检：wt 不在场即 panic 明确中文提示（实机显式跑场景可接受）
    let wt_path = wt_path_or_panic();
    ev.log(&format!("wt 探测命中：{wt_path}"));

    // (tool, 期望背压旗标)：2000 恰不超 LONG_MSG_CHARS(2000) 阈值 → 快消费者
    // 两家均不背压（families::use_backpressure 语义：chars > 阈值才切）
    let cases = [("claude", false), ("codex", false)];
    let mut failures: Vec<String> = Vec::new();
    for (tool, want_bp) in cases {
        let tag = format!("wt-{tool}-{run_id}");
        let r: Result<(), String> = (|| {
            let mut proc = boot_wt_session(&wt_path, tool, &tag, &ev);
            let text = e2e_text(tool, &run_id, 2_000);
            assert_eq!(text.chars().count(), 2_000, "WT 矩阵固定 2000 字符档");
            let spec = family_for(tool).unwrap();
            let stamp = stamp_of(&text).to_string();
            let stats = inject_text_spec(proc.target, &text, &spec)
                .map_err(|e| format!("wt×{tool} inject_text_spec 失败: {e}"))?;
            ev.log(&format!("{tag}: stats={stats:?}"));
            if stats.backpressure != want_bp {
                return Err(format!(
                    "wt×{tool} 背压旗标不符：got={} want={want_bp}（2000 恰不超阈值）",
                    stats.backpressure
                ));
            }
            if stats.written != text.chars().count() {
                return Err(format!("wt×{tool} written={} ≠ 全量 2000", stats.written));
            }
            // sid 发现在注入之后（会话存储懒创建），随后确认层轮询 stamp
            let marker = proj_marker(&proc);
            let sid = discover_session_id(&ev, tool, &marker, DISCOVER_TIMEOUT, &tag).ok_or_else(
                || {
                    format!(
                        "wt×{tool} 注入后未发现会话 id（marker={marker}，{DISCOVER_TIMEOUT:?}）"
                    )
                },
            )?;
            let hit = poll_stamp(&ev, tool, &sid, &stamp, STAMP_TIMEOUT_FAST, &tag);
            proc.kill();
            if !hit {
                return Err(format!("wt×{tool} stamp 未命中（sid={sid}）"));
            }
            Ok(())
        })();
        if let Err(e) = r {
            ev.log(&format!("{tag}: FAIL {e}"));
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "WT 宿主矩阵存在失败：{failures:#?}");
}

// ============================================================
// Test 6（F5）—— codex 万字符：快消费者 10k 切背压 + 460s 总预算
// ============================================================

#[test]
#[ignore = "实机显式跑：codex 10000 字符长文（背压注入可达分钟级，耐心等待勿中途判定失败）"]
fn e2e_codex_long_10k() {
    let run_id = format!("{}-t6", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("codex-long-10k", &run_id);
    ev.log(&format!("=== e2e_codex_long_10k run={run_id} ==="));
    // conhost 拓扑复用（宿主维度已由 Test 1/4 与本批 WT 例覆盖，本例只验长度维度）
    let tag = format!("codex-10k-{run_id}");
    let mut proc = boot_probe_session("codex", &tag, &ev);
    let text = e2e_text("codex", &run_id, 10_000);
    assert_eq!(text.chars().count(), 10_000);
    let spec = family_for("codex").unwrap();
    // 快消费者 10000 > LONG_MSG_CHARS(2000) → 按 families::use_backpressure 语义
    // 切背压=true；总预算 = inject_budget_ms = 10s + 10000×45ms = 460s
    let budget = multi_agents_manager_lib::inject::families::inject_budget_ms(&spec, 10_000);
    let stamp = stamp_of(&text).to_string();
    let t0 = Instant::now();
    let r = inject_text_spec(proc.target, &text, &spec);
    let elapsed = t0.elapsed();
    ev.log(&format!(
        "{tag}: elapsed={}ms r={:?}",
        elapsed.as_millis(),
        r.as_ref().map(|s| format!("{s:?}"))
    ));
    let mut failures: Vec<String> = Vec::new();
    match r {
        Ok(stats) => {
            if !stats.backpressure {
                failures.push(
                    "codex 10k 应走背压（快消费者 10000>2000，use_backpressure 语义）".to_string(),
                );
            }
            if elapsed.as_millis() as u64 > budget {
                failures.push(format!(
                    "codex 10k 耗时 {}ms > 背压总预算 {budget}ms",
                    elapsed.as_millis()
                ));
            }
            if stats.written != text.chars().count() {
                failures.push(format!("codex 10k written={} ≠ 全量 10000", stats.written));
            }
        }
        Err(e) => failures.push(format!("codex 10k 注入失败: {e}")),
    }
    // sid 发现在注入之后（rollout 懒创建），确认层轮询统一慢窗（见常量注）
    let marker = proj_marker(&proc);
    let sid = discover_session_id(&ev, "codex", &marker, DISCOVER_TIMEOUT, &tag)
        .unwrap_or_else(|| panic!("codex 10k 注入后未发现会话 id（marker={marker}）"));
    let hit = poll_stamp(&ev, "codex", &sid, &stamp, STAMP_TIMEOUT_SLOW, &tag);
    // 单一清场点：置于全部断言之后（panic 路径由 Drop 守卫兜底）
    proc.kill();
    if !hit {
        failures.push(format!("codex 10k stamp 未命中（sid={sid}）"));
    }
    assert!(failures.is_empty(), "codex 10k 长文存在失败：{failures:#?}");
}

// ============================================================
// Test 7（F5）—— 跨会话并发互扰：两会话并行注入各自命中、零串扰
// ============================================================

#[test]
#[ignore = "实机显式跑：claude+codex 双会话并行注入（各自 stamp 各自命中 + 对方零串扰）"]
fn e2e_cross_session_no_crosstalk() {
    let run_id = format!("{}-t7", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let ev = Ev::new("cross-session", &run_id);
    ev.log(&format!(
        "=== e2e_cross_session_no_crosstalk run={run_id} ==="
    ));

    // ① 两会话独立起手（各自独立 temp 目录与 run-id 标记）
    let mut proc_a = boot_probe_session("claude", &format!("cross-a-{run_id}"), &ev);
    let mut proc_b = boot_probe_session("codex", &format!("cross-b-{run_id}"), &ev);

    // ② 各自 2000 字符（恰不超快家族长文阈值）+ 专属尾戳（XA/XB 区分双方，
    //    尾戳即全量送达判据，也是串扰检测的异戳探针）
    let tail_a = format!("M9R-E2E-XA-{run_id}-END");
    let tail_b = format!("M9R-E2E-XB-{run_id}-END");
    let text_a = format!("{}{tail_a}", "a".repeat(2_000 - tail_a.chars().count()));
    let text_b = format!("{}{tail_b}", "b".repeat(2_000 - tail_b.chars().count()));
    assert_eq!(text_a.chars().count(), 2_000);
    assert_eq!(text_b.chars().count(), 2_000);
    let stamp_a = stamp_of(&text_a).to_string();
    let stamp_b = stamp_of(&text_b).to_string();

    // ③ 两线程同时发起 inject_text_spec。引擎 CONSOLE_OP 进程级串行为既定行为
    //    （控制台附加态进程全局唯一，B 在锁上排队等 A 的临界区放出）——本例
    //    验收点不是并行度，而是「并行触发、内部串行」之下：各自 stamp 各自
    //    命中 + 对方会话存储零串扰。
    let spec_a = family_for("claude").unwrap();
    let spec_b = family_for("codex").unwrap();
    let pid_a = proc_a.target;
    let pid_b = proc_b.target;
    let t_a = std::thread::spawn(move || inject_text_spec(pid_a, &text_a, &spec_a));
    let t_b = std::thread::spawn(move || inject_text_spec(pid_b, &text_b, &spec_b));
    let stats_a = t_a
        .join()
        .expect("A（claude）注入线程 panic")
        .expect("A 注入失败");
    let stats_b = t_b
        .join()
        .expect("B（codex）注入线程 panic")
        .expect("B 注入失败");
    ev.log(&format!("cross: A stats={stats_a:?} B stats={stats_b:?}"));
    assert!(!stats_a.backpressure, "A 2000 字符恰不超阈值不应背压");
    assert!(!stats_b.backpressure, "B 2000 字符恰不超阈值不应背压");
    assert_eq!(stats_a.written, 2_000, "A 应全量送达");
    assert_eq!(stats_b.written, 2_000, "B 应全量送达");

    // ④ sid 各自发现（两会话存储独立懒创建），随后各自确认层轮询
    let marker_a = proj_marker(&proc_a);
    let marker_b = proj_marker(&proc_b);
    let sid_a = discover_session_id(&ev, "claude", &marker_a, DISCOVER_TIMEOUT, "cross-a")
        .expect("A（claude）注入后未发现会话 id");
    let sid_b = discover_session_id(&ev, "codex", &marker_b, DISCOVER_TIMEOUT, "cross-b")
        .expect("B（codex）注入后未发现会话 id");
    let hit_a = poll_stamp(
        &ev,
        "claude",
        &sid_a,
        &stamp_a,
        STAMP_TIMEOUT_FAST,
        "cross-a",
    );
    let hit_b = poll_stamp(
        &ev,
        "codex",
        &sid_b,
        &stamp_b,
        STAMP_TIMEOUT_FAST,
        "cross-b",
    );

    // ⑤ 零串扰双向断言（在各自命中之后读——页面已有本方消息，「异戳不在此页」
    //    才是有效的零证据）
    let a_page = read_session_messages("claude", &sid_a, PROBE_LIMIT).expect("读 A 会话存储失败");
    let b_page = read_session_messages("codex", &sid_b, PROBE_LIMIT).expect("读 B 会话存储失败");
    let a_has_b = stamp_hit_in_page(&a_page, &stamp_b);
    let b_has_a = stamp_hit_in_page(&b_page, &stamp_a);

    // 单一清场点：置于全部断言之后（panic 路径由 Drop 守卫兜底）
    proc_a.kill();
    proc_b.kill();

    assert!(hit_a, "A stamp 应命中 A 会话存储（sid={sid_a}）");
    assert!(hit_b, "B stamp 应命中 B 会话存储（sid={sid_b}）");
    assert!(!a_has_b, "A（claude）会话存储出现 B 的 stamp——串扰！");
    assert!(!b_has_a, "B（codex）会话存储出现 A 的 stamp——串扰！");
}
