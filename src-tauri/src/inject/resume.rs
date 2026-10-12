//! R5 一键 resume 窗口（M6R–M9R 批次 Task 11）：
//! 手机或桌面点一下 → 本机自动打开终端 + 进入项目目录 + 恢复会话 + 置前聚焦，
//! 用户零额外操作。终端选择：Windows 优先 Windows Terminal、未装回退 conhost；
//! macOS **Terminal.app 优先、iTerm2 次选**（F1 治理 2026-09-20：iTerm2 3.7.2+
//! macOS26 带命令 create window 恒死窗假成功——mac-reverify-b9a501c §四-A/R1②，
//! 上游升级复测后再翻转；osascript **等待执行 + 失败回执**（M2 修法）+ **出手后
//! 效果回查**（F1②：2–3s 内按注入命令特征查子进程，未命中转次选，双败 failed
//! 回执 + 审计 failed——「死窗」纳入账实一致），见 [`open_macos_with`] /
//! [`resume_effect_in_snapshot`] / [`classify_resume_error`] / [`spawn_terminal`]）。
//!
//! ## 结构（构造与执行分离，engine.rs 同款纪律；评审 I2：macOS 同样入缝）
//! - [`resume_command`]：命令表纯函数（Step 1 实测取证，**未查证/不存在的工具绝不入表**
//!   ——前端按钮禁用 + 原因「该工具 resume 命令待查证」）；
//! - [`SpawnSpec`]：spawn 计划载荷（枚举两变体——Windows CreateProcess / macOS
//!   AppleScript），构造与执行分离，跨平台 pub 可测不真 spawn；
//! - [`build_spawn_command_windows`]：Windows 变体纯构造（wt / conhost 两形态）；
//! - [`iterm_open_window_script`] / [`terminal_open_script`]：macOS AppleScript 纯构造
//!   （构造只是字符串拼接，**Windows 上即可测**，实机开窗验证归 Mac 回传清单）；
//! - [`open_macos_with`]：macOS 双通道缝出手顺序（iTerm2 优先、Terminal.app 次选），
//!   跨平台可测；
//! - [`open_session_terminal_with`]：核心入口（**接受 spawner 缝**，远端端点测试注入
//!   记录型假 spawner，零真开窗——两平台皆然）；[`open_session_terminal`] =
//!   生产装配（真 spawn / 真 AppleScript）。
//!
//! ## cwd 落点机制（评审 I1 修正）
//! conhost 回退无 `-d` 等价参数，cwd 继承靠生产 spawner 的 `Command::current_dir`：
//! conhost → cmd 逐级继承进程工作目录，落在项目目录（免 `cd /d` 复合串的引号地狱）；
//! wt 分支另带 `-d <cwd>`（双保险），`current_dir` 同设无害。codex resume 默认按
//! cwd 过滤候选（`--all` 才解除）——spawn 落在项目目录即满足该工作区上下文。
//!
//! ## 审计
//! 远端端点（POST /m/api/v1/session-open）在 spawn 出手时写审计 action=`open`
//! （W5 词表 send|queue|flush|jump|retract|approve|reject|fail|key|open——Task 7
//! 的「open 预留：Task 11」标注兑现）。桌面 Tauri 命令路径不写审计：audit_write
//! 需要设备身份（device_id/device_name），本机点击无设备身份，审计口径 = 远端设备
//! 动作留痕（与 session-send 等既有写端点一致）。
//!
//! ## 注入面说明（安全）
//! resume 命令里的会话 id 一律取自**本机会话快照命中项**（远端只送 session_id 用于
//! 查找，未命中即 404 no_session），不回显请求原文——`cmd /k` 对 `&`/引号等元字符
//! 的解析面不接触远端输入。

use crate::session::Session;

/// spawner 缝类型（对齐 remote::server::ViaHostsSource 的 type_complexity 收敛别名）：
/// 消费 [`SpawnSpec`] 完成「开终端窗口」副作用。生产 = [`spawn_terminal`]（真开窗）；
/// 测试 = 记录型假体（零真开窗）。
pub type SpawnFn = dyn Fn(&SpawnSpec) -> Result<(), String> + Send + Sync;

/// 终端 spawn 计划载荷（枚举两变体；评审 I2：macOS 同样走缝，「测试零真开窗」
/// 跨平台为真，且 AppleScript 构造获得 Windows 可测性——对齐 R6 精神）。
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnSpec {
    /// Windows：CreateProcess 开新窗/新标签
    Windows {
        /// 可执行程序（wt 探测命中的完整路径 / "conhost.exe"）
        program: String,
        /// 参数向量（resume 命令整体作为一个参数由 CreateProcess 引号传递，不再二次分词）
        args: Vec<String>,
        /// 进程工作目录（生产 spawner 经 `Command::current_dir` 消费——conhost 回退
        /// 无 `-d` 等价物，cwd 靠继承落在项目目录；wt 分支同设无害）
        cwd: String,
        /// 新建会话场景专用环境变量（C3）：create 显式设 `DISABLE_AUTOUPDATER=1`
        /// （spec §4.2 环境红线，防工具自更新打断起窗）；resume 场景恒空（零行为
        /// 变更）。macOS（MacosApplescript 变体）走 AppleScript 脚本内联 env，
        /// 归 Mac 后补批。
        env: Vec<(String, String)>,
        /// 新建会话场景专用环境**剥离**前缀（C8 实机定案）：命中前缀的继承变量逐一
        /// `env_remove`——create 会话是**独立一等会话**，不得继承启动者（兔维斯 宿主/
        /// E2E 测试进程）的 Claude 会话管道变量：冒烟实证 `CLAUDE_CODE_CHILD_SESSION`
        /// 被继承后，起窗的 claude 自认子会话（跳过信任框 + **关闭 transcript 落盘**
        /// → 会话文件永不物化 +「会话卡上板」全链失真）。resume 场景恒空（零行为
        /// 变更）。ANTHROPIC_* 不在剥离面（可能是用户真实配置）。
        env_rm_prefixes: Vec<String>,
        /// CREATE_NEW_CONSOLE 标记：conhost 回退必须自带（0x10）才开新控制台窗；
        /// wt 自开新标签无此需求
        new_console: bool,
    },
    /// macOS：AppleScript 脚本整体（生产 = osascript 执行）
    MacosApplescript {
        /// 完整脚本（[`iterm_open_window_script`] / [`terminal_open_script`] 构造）
        script: String,
    },
}

/// resume 命令表（Step 1 实测取证：Windows 本机 `--help` 实跑，2026-09-19）。
/// **未查证/不存在的工具绝不入表**（严格档同理），各条注明查证版本与 --help 原文：
/// - claude 2.1.251：`-r, --resume [value]  Resume a conversation by session ID, or
///   open interactive picker with optional search term`；
/// - codex-cli 0.154.0：子命令 `codex resume [OPTIONS] [SESSION_ID] [PROMPT]`
///   （"Resume a previous interactive session"）。**工作区上下文限定**：resume 默认按
///   cwd 过滤候选（`--all` 才解除），本实现 spawn 以 current_dir 落在项目目录即满足；
///   zcode 的 D6 在册工作区限定同口径——但 zcode CLI 本机未安装，如实记 None 不入表；
/// - kimi 2.0.0：`-S, --session [id]  Resume a session. With ID: resume that session.
///   Without ID: interactively pick.`；
/// - opencode 1.18.31：`-s, --session  session id to continue`（SQLite 类工具，
///   扫描契约豁免 L2/L3，与本表无关）；
/// - 未安装/不适用（Step 1 如实记录，均 None）：zcode（**包内 CLI 0.16.9 存在**
///   （--resume/-c 可用，路径=应用包 Resources，不在 PATH——`where` 探测不到），
///   但 zcode 的 resume 窗口语义待设计（桌面深链 vs 终端无头）——见
///   docs/release-notes/2026-09-19-zcode-APP形态首触调查与互通矩阵.md，F4①），
///   openclaw（CLI 未安装）、workbuddy（桌面 APP 形态无 CLI）、dsh（web 宿主）。
const RESUME_TABLE: &[(&str, &str)] = &[
    ("claude", "claude --resume {id}"),
    ("codex", "codex resume {id}"),
    ("kimi", "kimi --session {id}"),
    ("opencode", "opencode --session {id}"),
];

/// 按工具查 resume 命令（`{id}` 占位替换为会话 id）。未入表工具 → None：
/// 端点映射 404 no_resume_command，前端按钮禁用 + 原因「该工具 resume 命令待查证」。
pub fn resume_command(tool: &str, session_id: &str) -> Option<String> {
    RESUME_TABLE
        .iter()
        .find(|(t, _)| *t == tool)
        .map(|(_, tpl)| tpl.replace("{id}", session_id))
}

/// 未信任预检提醒文案（T3 决策 5，投递面按发起侧：远程 → resume 回执
/// `trustPromptExpected` / 会话页提示条；桌面 → 桌面侧。文案两处同源）。
pub const TRUST_PROMPT_REMINDER: &str =
    "重开的会话所在目录未做信任确认，请在主机终端应答信任提示，否则会话将挂起";

/// T3 重开 cwd 信任归一 + 未信任预检（claude 专属；**只读** `~/.claude.json`
/// （经 `monitor::claude_config`），兔维斯 永不写该文件）。原地改写
/// `session.project_path`：命中已信任条款则复用其**精确 casing**（claude 按 cwd
/// 精确字符串查信任、键存储对盘符大小写脆弱——实证 `E:`=false / `e:`=true 双条
/// 并存，照抄记录 cwd 会命中 false 条款弹信任 TUI，手机注入答不了 → 重开挂起）；
/// 命中全 false → cwd 原样、返回 true（**预检提醒触发条件**）；未命中 / 配置
/// 不可读 / 非 claude / 无 home / 空白 cwd → 保守 no-op 返回 false（全新目录的
/// 首次信任属正常流程，不提醒）。
///
/// home 注入缝：远程端点经 `RemoteState.home_source`（测试注入 tempdir，零接触
/// 真实主目录）；桌面命令 = `dirs::home_dir()`。归一在命令处理器层做（此处 cwd
/// 尚未进 spawn 构造，归一产物同时是 spawn cwd 与提醒判定的唯一来源）。
pub fn normalize_cwd_for_trust(session: &mut Session, home: Option<&std::path::Path>) -> bool {
    if session.agent_type.tool_id() != "claude" {
        return false;
    }
    let cwd = session.project_path.trim();
    if cwd.is_empty() {
        return false; // no_cwd 哨兵面不碰（核心照原样返回哨兵）
    }
    let Some(home) = home else {
        return false;
    };
    // 择优复用真条款 casing（一次读盘）；未复用再判未信任提醒（第二次读盘只在
    // 全 false / 未命中路径——重开是低频用户动作，可忽略）
    if let Some(trusted) =
        crate::monitor::claude_config::trusted_casing_for(home, std::path::Path::new(cwd))
    {
        session.project_path = trusted;
        return false;
    }
    crate::monitor::claude_config::is_trusted(home, std::path::Path::new(cwd)) == Some(false)
}

/// Windows spawn 计划纯构造（**不真 spawn**，跨平台可测；评审 M1：入参为 wt 完整
/// 路径 Option 而非在场布尔——路径进 spec 由生产 spawner 直 spawn，绕开应用执行
/// 别名（App Execution Alias）停用/损坏场景）。两分支 `cwd` 字段同设——生产
/// spawner 经 current_dir 消费（见 [`spawn_terminal`]）：
/// - 有 wt：`<wt 路径> -d <cwd> cmd /k <resume>`（wt 自开新标签并落在 cwd）；
/// - 无 wt：`conhost.exe cmd /k <resume>` + CREATE_NEW_CONSOLE（全新控制台窗）。
///
/// C3 起末参亦承接**新建会话的裸工具名**（create 复用本构造后覆写 env，见
/// [`build_create_spawn_spec`]）——`<resume>` 占位读作「resume 命令或裸工具名」。
pub fn build_spawn_command_windows(wt: Option<&str>, cwd: &str, resume: &str) -> SpawnSpec {
    match wt {
        Some(path) => SpawnSpec::Windows {
            program: path.to_string(),
            args: vec![
                "-d".to_string(),
                cwd.to_string(),
                "cmd".to_string(),
                "/k".to_string(),
                resume.to_string(),
            ],
            cwd: cwd.to_string(),
            env: Vec::new(),
            env_rm_prefixes: Vec::new(),
            new_console: false,
        },
        None => SpawnSpec::Windows {
            program: "conhost.exe".to_string(),
            args: vec!["cmd".to_string(), "/k".to_string(), resume.to_string()],
            cwd: cwd.to_string(),
            env: Vec::new(),
            env_rm_prefixes: Vec::new(),
            new_console: true,
        },
    }
}

/// 新建会话起窗命令表（`{tool}` 占位工具 id）。
///
/// **与 [`RESUME_TABLE`] 的关键差别**：opencode 在 2.x 下必须加 `--standalone`。
/// 依据（复验定案 §2 端口竞争陷阱，Mac C 组实证 + Win 同参实测）：
/// 2.x 默认形态 = 前台 TUI + **独立后台服务**；当已有服务占用默认端口时，裸
/// `opencode` 不出 TUI，而进 `Starting background server...` 静默重试环——Phase C
/// create E2E opencode 腿 17:23/17:28 失败（find_tui_pid 30s 超时）的根因即此
/// （用户常驻服务在场）。`--standalone` = 私有 server，TTY 下必出 TUI、不依赖
/// 服务/端口。resume 侧不受影响（复验定案 §7，2026-10-03 **常驻服务在场**条件
/// 取证：裸 `--session` TUI 正常载入目标会话，免疫端口陷阱——与 create 裸命令
/// 行为不一致属实证事实，机制未探明不做超证据结论）。
///
/// 注：`opencode session list` 在 2.x 是「current project」作用域（D0-3），
/// 与发现层无关（发现层走 db 直读）。
const CREATE_COMMAND_TABLE: &[(&str, &str)] = &[("opencode", "opencode --standalone")];

/// 按工具查新建会话的起窗命令（未入表 → 裸 `{tool}`）。目前仅 opencode 需特化。
pub fn create_command(tool: &str) -> String {
    CREATE_COMMAND_TABLE
        .iter()
        .find(|(t, _)| *t == tool)
        .map(|(_, cmd)| (*cmd).to_string())
        .unwrap_or_else(|| tool.to_string())
}

/// 新建会话起窗规格：工具命令 + DISABLE_AUTOUPDATER=1（spec §4.2 环境红线；
/// S2 实测 conhost/WT 双宿主 4/4 透传，形态A=spawn 显式设 env）。C6 起**跨平台**：
/// 纯载荷构造与 [`build_spawn_command_windows`] 同口径（wt=None → conhost 载荷），
/// 端点假缝不真 spawn、非 Windows 构建可编译可测；macOS 真 spawn 变体归 Mac 后补批
/// （脚本内联 `env K=V ` 前缀，Mac 探测 M2 实证）。
/// 起窗命令经 [`create_command`] 取（opencode 走 `--standalone`，见该表 doc）。
/// create 起窗的会话上下文剥离前缀（C8 冒烟实机定案；`env_rm_prefixes` 字段 doc
/// 有根因全文）：命中前缀的继承变量在 spawn 时逐一 `env_remove`。只剥 **Claude
/// 会话管道**变量（子会话标记/入口/SSE/effort 等——都是「本次会话」的上下文，
/// 对新建会话是污染源）；**ANTHROPIC_* 不剥**（可能是用户真实配置）。
pub const CREATE_ENV_RM_PREFIXES: &[&str] =
    &["CLAUDECODE", "CLAUDE_CODE_", "CLAUDE_PID", "CLAUDE_EFFORT"];

pub fn build_create_spawn_spec(wt: Option<&str>, cwd: &str, tool: &str) -> SpawnSpec {
    let mut spec = build_spawn_command_windows(wt, cwd, &create_command(tool));
    if let SpawnSpec::Windows {
        env,
        env_rm_prefixes,
        ..
    } = &mut spec
    {
        *env = vec![("DISABLE_AUTOUPDATER".to_string(), "1".to_string())];
        *env_rm_prefixes = CREATE_ENV_RM_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    spec
}

/// `where wt` 探测 Windows Terminal（进程级缓存 OnceLock，approve.rs VERSION_CACHE
/// 先例：首调一次探测，结果含 None 一并入缓存，不重复刷进程）。缓存**首行完整
/// 路径**（评审 M1：生产直 spawn 该路径——`wt` 应用执行别名可能被停用/损坏，
/// where 命中的真实 exe 不受影响）。`where.exe` 是 System32 上的真实可执行文件
/// （非 cmd 内建），裸名直 spawn 即可。
#[cfg(windows)]
pub(crate) fn windows_terminal_path() -> Option<String> {
    static WT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
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
            })
            .filter(|l| !l.is_empty())
    })
    .clone()
}

/// 非 Windows 平台无 wt 语义（构造层恒 None；该平台走 AppleScript 分支）。
#[cfg(not(windows))]
pub(crate) fn windows_terminal_path() -> Option<String> {
    None
}

/// `where <工具>` 输出选行（纯函数可测，P1-1 安全修复 2026-10-03）：取**绝对
/// 路径形态（`X:\` 盘符起）且扩展名属 cmd 可执行类**（.exe/.cmd/.bat/.com）的
/// 首行。两道过滤各有实因：① where 从其自身 cwd 起搜，cwd 命中行是裸文件名
/// （调用侧已把 where 的 cwd 固定到 SystemRoot，仍只认绝对路径行双保险）；
/// ② npm 目录同前缀多形态并存（无扩展 sh 脚本 / .cmd / .ps1）——cmd 对
/// path-qualified 无扩展名不可执行、对 .ps1 不认，跳过这两类行。
/// 非 Windows 生产面不消费（spawn 臂 cfg 门控；测试两平台都跑）——allow(dead_code)
/// 防 Linux 构建告警（run_osascript_wait 同款先例）。
#[cfg_attr(not(windows), allow(dead_code))]
fn pick_where_hit(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| {
            let lower = l.to_ascii_lowercase();
            let exec_ext = [".exe", ".cmd", ".bat", ".com"]
                .iter()
                .any(|e| lower.ends_with(e));
            let b = l.as_bytes();
            let abs_drive = b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && (b[2] == b'\\' || b[2] == b'/');
            exec_ext && abs_drive
        })
        .map(String::from)
}

/// 工具名 → 安装绝对路径（`where` 解析；仅 Windows，P1-1）。**每次现查不缓存**：
/// 工具安装/迁移后立即生效（对照 [`windows_terminal_path`] 的 OnceLock 先例——
/// wt 位置稳定可缓存，工具 bin 会动）。where 的 cwd 固定 SystemRoot：where 从
/// 自身 cwd 起搜，不固定会把 兔维斯 进程 cwd 下的同名可执行体当首命中。
#[cfg(windows)]
fn resolve_tool_path(name: &str) -> Option<String> {
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    std::process::Command::new("where")
        .arg(name)
        .current_dir(sysroot)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| pick_where_hit(&String::from_utf8_lossy(&o.stdout)))
}

/// `/k` 载荷安全加固（纯函数，resolver 缝注入可测，P1-1）。威胁：`cmd /k claude`
/// 由 cmd 解析 `claude` 时**当前目录优先于 PATH**，而 spawn 的 current_dir 恰是
/// 用户项目目录——外部 clone 的仓库带恶意 `claude.cmd` 即可在该目录起会话时执行
/// 任意代码。修法（在 [`spawn_terminal`] 生产执行层做，纯构造层不变——缝测试保
/// 持机器无关）：定位 args 中 `/k`（rposition，wt 前缀链唯一），其后载荷分词——
/// 首 token 为裸名 → resolver（生产 = [`resolve_tool_path`]）解析出的**绝对路径**
/// 替换并**分体传参**（路径含空格时由 Command 逐参引号包裹；单字符串内嵌引号会
/// 经 MSVCRT 反斜杠转义，cmd 不认）。首 token 已是路径形态 / resolver 未命中 /
/// 无 `/k` → 原样返回（配合 spawn 侧 NoDefaultCurrentDirectoryInExePath 兜底）。
/// 非 Windows 生产面不消费（同 [`pick_where_hit`] 的 cfg_attr 先例）。
#[cfg_attr(not(windows), allow(dead_code))]
fn harden_cmd_payload(args: &[String], resolver: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let Some(k) = args.iter().rposition(|a| a.eq_ignore_ascii_case("/k")) else {
        return args.to_vec();
    };
    let Some(payload) = args.get(k + 1) else {
        return args.to_vec();
    };
    let toks: Vec<&str> = payload.split_whitespace().collect();
    let Some(first) = toks.first().copied() else {
        return args.to_vec();
    };
    let bare_name = !first.contains('\\') && !first.contains('/') && !first.contains(':');
    if !bare_name {
        return args.to_vec();
    }
    match resolver(first) {
        Some(abs) => {
            let mut out = args[..=k].to_vec();
            out.push(abs);
            out.extend(toks[1..].iter().map(|s| s.to_string()));
            out
        }
        None => args.to_vec(),
    }
}

/// cmd 元字符判据（P2-1，评审二轮 2026-10-03）：token 含空白或任一 cmd 特殊
/// 字符时需要引号包裹（引号内这些字符均为字面量）。`%` 例外——引号内
/// `%VAR%` 仍会被 cmd 展开，与 `"`（路径中本不可合法出现，防御性拒）同归
/// 「不可安全表示」，由 [`cmd_payload_raw`] fail-closed。
fn needs_cmd_quotes(token: &str) -> bool {
    token.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '&' | '('
                    | ')'
                    | '{'
                    | '}'
                    | '^'
                    | '='
                    | ';'
                    | '!'
                    | '\''
                    | '+'
                    | ','
                    | '`'
                    | '~'
                    | '<'
                    | '>'
                    | '|'
            )
    })
}

/// `/k` 载荷 tokens → 单条 raw 命令行片段（P2-1，spawn_terminal 消费）——经
/// `raw_arg` 原样进 lpCommandLine，不经 std 的 MSVCRT 引号转义（分体传参通道
/// 对「无空格含元字符」路径不设防，实机复现 `Program Files (x86)` 类路径被
/// cmd 拆解）。两分支（Win11 26200 raw_arg 实证 + 四家 E2E 矩阵定案）：
/// - **全 token 干净**（无一需引号）→ **裸 join、不加任何引号**：与分体传参
///   命令行形态完全等价——实测红线：无条件外层包裹（恰两引号载荷）经 wt
///   中转后 codex 腿注入失效（create E2E 矩阵两轮复现；剥引号规则在 wt
///   argv 重组链下行为漂移，勿再引入裸串等价形态之外的引号）；
/// - **任一 token 需引号**（元字符/空白）→ 该 token 引号包裹 + **外层整体
///   再包一层**：cmd 对「载荷以引号开头且恰两引号」必剥（形态 A/E 实证，
///   剥后元字符裸奔），外层包裹使总引号 ≥4 不剥、元字符成字面量（形态 F
///   多 token 实证 + 元字符目录实机锚通过）。
///
/// 任一 token 含 `"` 或 `%` → None：不可安全表示（引号内 %VAR% 仍展开），调用
/// 方回退分体传参（NoDefaultCurrentDirectoryInExePath 仍防 cwd 劫持）。
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_payload_raw(tokens: &[String]) -> Option<String> {
    let mut quoted: Vec<String> = Vec::with_capacity(tokens.len());
    let mut any_needs_quotes = false;
    for t in tokens {
        if t.contains('"') || t.contains('%') {
            return None;
        }
        if needs_cmd_quotes(t) {
            any_needs_quotes = true;
            quoted.push(format!("\"{t}\""));
        } else {
            quoted.push(t.clone());
        }
    }
    if any_needs_quotes {
        Some(format!("\"{}\"", quoted.join(" ")))
    } else {
        Some(quoted.join(" "))
    }
}

/// shell 单引号安全包装（macOS cd 参数用）：`'…'` 形态，内嵌单引号按 POSIX `'\''`
/// 序列转义——目录名带撇号/空格时裸拼会截断 cd 参数。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 组装 macOS 命令载荷：`cd '<cwd>' && <resume>`（cwd 经 [`shell_single_quote`]，
/// 整条再经 applescript_escape 进 AppleScript 双引号字符串字面量）。
fn macos_command_payload(cwd: &str, resume: &str) -> String {
    crate::inject::engine::applescript_escape(&format!(
        "cd {} && {}",
        shell_single_quote(cwd),
        resume
    ))
}

/// 构造 iTerm2 新开窗脚本（macOS 优先通道）：`create window with default profile
/// command "cd '<cwd>' && <resume>"` + activate 置前聚焦。载荷转义复用 engine.rs
/// [`crate::inject::engine::applescript_escape`]（双引号形态：反斜杠/双引号/裸换行；
/// shell 单引号在 AppleScript 双引号字符串里是字面量，cwd 的单引号包裹安全穿过）。
pub fn iterm_open_window_script(cwd: &str, resume: &str) -> String {
    format!(
        r#"tell application "iTerm2"
	activate
	create window with default profile command "{payload}"
	return "opened"
end tell
"#,
        payload = macos_command_payload(cwd, resume)
    )
}

/// 构造 Terminal.app 执行脚本（macOS 次选通道）：`do script "cd '<cwd>' && <resume>"`
/// （do script 自带回车）+ activate 置前聚焦。转义同 [`iterm_open_window_script`]。
pub fn terminal_open_script(cwd: &str, resume: &str) -> String {
    format!(
        r#"tell application "Terminal"
	do script "{payload}"
	activate
	return "opened"
end tell
"#,
        payload = macos_command_payload(cwd, resume)
    )
}

// ==== M2（Mac 验收 D-5 根因①处置）：macOS spawn 回执化 ====
// 实机教训（mac-acceptance-report-55f37e7 §四-B）：兔维斯 dev 二进制（无 bundle id）缺
// TCC 自动化授权时，osascript 以 -1743 失败，旧路径发射后不管 → stderr 被吞、audit
// 照记 open ok——账实背离。现 macOS 出手等待完成（wait_timeout 10s）+ stderr/退出码
// 捕获，失败经错误分类给可行动回执（200 failed + 审计 failed，端点既有 Err 臂承接）。

/// osascript 等待上限（秒）：osascript 下发脚本即返回，正常 <1s；10s 给 TCC 首次
/// 授权弹窗与冷启动留裕量，超时视为失败（kill + wait 收尸防僵尸，approve.rs 同口径）
const OSASCRIPT_WAIT_SECS: u64 = 10;

/// stderr 摘要截断上限（失败回执与审计 result 双消费防膨胀；normalize::summarize
/// 按 chars 计，中文多字节安全）
const RESUME_STDERR_SUMMARY_CHARS: usize = 120;

/// TCC -1743（errAEEventNotHandled / 自动化授权拒绝）的中文指引文案（用户裁决口径，
/// 固定长度有界——审计 result 直接承载）
pub const MACOS_TCC_GUIDANCE: &str =
    "macOS 自动化授权缺失：系统设置 > 隐私与安全性 > 自动化 中允许控制 Terminal/iTerm2 后重试（开发版首次需手动授权一次）";

/// osascript 失败分类（**纯函数跨平台可测**，M2 裁决：错误分类与执行层分离——
/// Windows 上即可单测，真实 osascript 路径只做编译验证）：
/// - stderr 含 `-1743` 字样或 osascript 标准错误「Not authorized to send Apple
///   events」（大小写不敏感）→ [`MACOS_TCC_GUIDANCE`] 指引文案；
/// - 其他 → stderr 摘要（[`RESUME_STDERR_SUMMARY_CHARS`] 截断防膨胀）；
/// - 空 stderr → 有界兜底文案（回执不悬空）。
pub fn classify_resume_error(stderr: &str) -> String {
    let tcc_hit = stderr.contains("-1743")
        || stderr
            .to_ascii_lowercase()
            .contains("not authorized to send apple events");
    if tcc_hit {
        MACOS_TCC_GUIDANCE.to_string()
    } else {
        let s = stderr.trim();
        if s.is_empty() {
            "osascript 执行失败（无 stderr 输出）".to_string()
        } else {
            crate::inject::normalize::summarize(s, RESUME_STDERR_SUMMARY_CHARS)
        }
    }
}

/// resume 专用 osascript 等待执行（macOS 生产路径；区别于聚焦层
/// window::applescript::execute_applescript——后者服务窗口聚焦/注入路径维持原语义
/// 不动，M2 只收窄 resume 出手）：等待子进程完成（wait_timeout 10s）+ stderr/退出码
/// 捕获，失败 Err 携带分类产物 → open_macos_with 双通道合并 → 端点既有 Err 臂 →
/// 200 failed 回执 + 审计 failed。
///
/// 管道口径（approve.rs probe_cli_version 同源论证）：osascript 输出远小于管道缓冲
/// （成功仅 `opened` 一行），先等后读无死锁风险；披露：输出超 ~64KB 管道缓冲的极端
/// 脚本会在 wait 时被写满阻塞而假性超时——结果有界（10s 杀掉）非死锁。stdin 置
/// null 防 osascript 等 stdin 白耗超时窗。不读 stdout 判「not found」：resume 脚本
/// `return "opened"`，无聚焦层的 tab 查找语义。
///
/// 函数体全为跨平台 API（Command/Stdio/wait_timeout），故**不 cfg 隔离**——Windows
/// 构建持续编译验证本函数（M2 裁决「cfg macos 构造层只做编译验证」的可执行化）；
/// 仅调用点（[`spawn_terminal`] 的 macOS 臂）cfg 分派，运行时 macOS 独占触达，
/// 非 macOS 构建下它不被调用故显式 allow(dead_code)。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn run_osascript_wait(script: &str) -> Result<(), String> {
    use std::io::Read;
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("osascript 启动失败：{e}"))?;
    match child.wait_timeout(std::time::Duration::from_secs(OSASCRIPT_WAIT_SECS)) {
        // 正常退出：先收退出码分诊成败，再排空 stderr（子进程已退出，缓冲安全读）
        Ok(Some(status)) => {
            let mut raw = Vec::new();
            if let Some(pipe) = child.stderr.as_mut() {
                let _ = pipe.read_to_end(&mut raw);
            }
            let stderr = String::from_utf8_lossy(&raw);
            if status.success() {
                Ok(())
            } else if stderr.trim().is_empty() {
                // 退出码捕获：无 stderr 时它是唯一线索，直接进回执
                Err(format!(
                    "osascript 执行失败（退出码 {}）",
                    status.code().unwrap_or(-1)
                ))
            } else {
                Err(classify_resume_error(&stderr))
            }
        }
        // 超时：杀 + 收尸（防僵尸），失败回执（与 spawn Err 同管道走端点 failed）
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!(
                "osascript 执行超时（{OSASCRIPT_WAIT_SECS}s 上限，已终止）——请重试；若持续失败检查 macOS 自动化授权"
            ))
        }
        Err(e) => Err(format!("osascript 等待失败：{e}")),
    }
}

/// Windows 出手链（跨平台纯逻辑，wt 路径由调用方注入便于测试降级链）：wt 在场
/// 先试；spawn 失败（应用执行别名停用/损坏等场景）**降级重试一次 conhost** 再报
/// failed（两次错误合并披露，cwd 经 current_dir 继承不丢）。
fn open_windows_with(
    wt: Option<&str>,
    cwd: &str,
    resume: &str,
    spawner: &SpawnFn,
) -> Result<(), String> {
    let mut wt_err: Option<String> = None;
    if let Some(wt) = wt {
        let spec = build_spawn_command_windows(Some(wt), cwd, resume);
        match spawner(&spec) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("wt spawn 失败，降级重试 conhost：{e}");
                wt_err = Some(e);
            }
        }
    }
    let spec = build_spawn_command_windows(None, cwd, resume);
    match spawner(&spec) {
        Ok(()) => Ok(()),
        Err(e) => Err(match wt_err {
            Some(w) => format!("{w}；{e}"),
            None => e,
        }),
    }
}

/// 效果回查缝类型（F1②，mac-reverify-b9a501c §四-A）：出手成功后确认目标会话
/// 真建立。入参 = resume 命令（会话 id 唯一，特征查子进程）；返回 false = 该通道
/// 「死窗/会话未建立」。生产 = [`macos_effect_probe`]（进程表轮询）；测试 = 假体。
pub type EffectCheckFn = dyn Fn(&str) -> bool + Send + Sync;

/// 效果回查轮询窗（秒）与采样间隔（毫秒）：出手后 2–3s 内确认（Mac 复验报告
/// §四-A 建议 ①）——Terminal `do script` → shell → 工具进程通常 <1.5s；3s 窗
/// 给慢机留裕量。双通道最坏 ≈ 2×(osascript 10s + 3s)，仅失败路径触达。
const EFFECT_CHECK_SECS: u64 = 3;
const EFFECT_CHECK_POLL_MS: u64 = 500;

/// 回查判定纯函数（跨平台可测）：进程命令行快照中存在含 resume 命令特征的条目
/// → 该通道真建立了会话。resume 命令含会话 id（UUID 级唯一），npm shim 类工具
/// （claude → `node …/claude --resume <id>`）命令行保留原参数，子串包含即命中。
/// 空特征防御性返回 false（不把空串当通配）。
///
/// 无假阳性来源说明：出手（osascript）经 [`run_osascript_wait`]**等待退出后**才
/// 回查——osascript 进程（命令行内嵌脚本全文含 resume 命令）在回查时已不存在，
/// 不会自命中。
pub fn resume_effect_in_snapshot(resume_cmd: &str, snapshot: &[String]) -> bool {
    let sig = resume_cmd.trim();
    !sig.is_empty() && snapshot.iter().any(|cl| cl.contains(sig))
}

/// 生产效果回查（macOS 运行时消费；跨平台 API 编译验证，Windows 构建不触达）：
/// 轮询 [`EFFECT_CHECK_SECS`] 秒 × [`EFFECT_CHECK_POLL_MS`]，每次全量刷新进程表
/// 采命令行快照交 [`resume_effect_in_snapshot`] 判定（sysinfo 0.32：
/// refresh_processes 增量关 + 全量刷；`cmd()` 为 OsStr 连接成串）。
fn macos_effect_probe(resume_cmd: &str) -> bool {
    let mut sys = sysinfo::System::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(EFFECT_CHECK_SECS);
    let mut self_checked = false;
    loop {
        let snapshot = refresh_cmd_snapshot(&mut sys);
        // 数据源自检（每轮探针只报一次）：自身进程 cmd 为空 = 刷新配置失效
        // （回归观测点——两参 refresh 默认 Kind 不刷 cmd 即此形态，2026-09-20 Mac 实测）
        if !self_checked {
            self_checked = true;
            if let Some(me) = sys.process(sysinfo::Pid::from_u32(std::process::id())) {
                if me.cmd().is_empty() {
                    log::error!(
                        "macos_effect_probe 自检：自身进程 cmd() 为空——进程刷新配置异常，效果回查将恒假阴性"
                    );
                }
            }
        }
        if resume_effect_in_snapshot(resume_cmd, &snapshot) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(EFFECT_CHECK_POLL_MS));
    }
}

/// 进程表全量刷新并返回命令行快照。**必须用 specifics 显式刷 cmd**：sysinfo 0.32
/// 两参 `refresh_processes` 的默认 `ProcessRefreshKind` 不含 cmd 字段——`cmd()`
/// 全量返回空（Mac 实测 2026-09-20：662 进程 empty_cmd=662），效果回查恒假阴性。
/// 与主扫描（adapter/mod.rs）同款口径。
fn refresh_cmd_snapshot(sys: &mut sysinfo::System) -> Vec<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .map(|p| {
            p.cmd()
                .iter()
                .map(|a| a.to_string_lossy().replace('\\', "/"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// macOS 双通道缝出手（F1 治理后：**Terminal.app 优先、iTerm2 次选**——mac-reverify
/// b9a501c §四-A/R1②：iTerm2 3.7.2+macOS26 带命令 create window **恒产 0-tab 死窗**
/// 且 osascript 假成功（exit 0/stderr 空），在 osascript 契约内不可辨；而 Terminal
/// `do script` 同环境实测全通；R1③：上游升级复测后再翻转回 iTerm2 优先）。
/// 每通道 = 组装 [`SpawnSpec::MacosApplescript`] 交 spawner，Ok 后过
/// [`EffectCheckFn`] 效果回查：命中即收口；未命中（死窗）按**该通道失败**处理转
/// 次选；双通道皆败（spawn Err 或回查未命中）合并中文错误（通道名标注进账——
/// 端点 Err 臂 → 200 failed 回执 + 审计 `open failed:*`，把「死窗」纳入账实一致）。
fn open_macos_with(
    cwd: &str,
    resume: &str,
    spawner: &SpawnFn,
    effect_check: &EffectCheckFn,
) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    for (name, script) in [
        ("Terminal.app", terminal_open_script(cwd, resume)),
        ("iTerm2", iterm_open_window_script(cwd, resume)),
    ] {
        match spawner(&SpawnSpec::MacosApplescript { script }) {
            Ok(()) => {
                if effect_check(resume) {
                    return Ok(());
                }
                log::warn!("resume {name} 出手后回查未命中（死窗/会话未建立），转次选通道");
                errs.push(format!("{name} 出手后回查未命中（死窗/会话未建立）"));
            }
            Err(e) => errs.push(format!("{name}: {e}")),
        }
    }
    Err(format!("全部 resume 通道失败：{}", errs.join("；")))
}

/// 生产 spawner（RemoteState 生产装配 / Tauri 命令共用），按变体 cfg 分派：
/// - Windows 变体：CreateProcess **fire-and-forget（不 wait）**——wt/conhost 承载的
///   `cmd /k` 是常驻交互 shell，等它退出只会空耗超时窗（M2 裁决：与 macOS 的不对称
///   以此注释存证）；spawn 失败已由 `Command::spawn` Err → failed 回执覆盖。
///   `current_dir` 消费 cwd（评审 I1——conhost 回退靠进程工作目录继承落在项目目录）；
///   conhost 路径加 CREATE_NEW_CONSOLE(0x10)。
/// - macOS 变体：**等待完成**（[`run_osascript_wait`]：wait_timeout 10s + stderr/
///   退出码捕获）——osascript 下发脚本即返回（正常 <1s），等待代价可忽略，而失败
///   回执杜绝「audit open ok 但无窗」的账实背离（M2：Mac 验收 D-5 唯一实机 FAIL
///   根因①）。
pub fn spawn_terminal(spec: &SpawnSpec) -> Result<(), String> {
    match spec {
        #[cfg(windows)]
        SpawnSpec::Windows {
            program,
            args,
            cwd,
            env,
            env_rm_prefixes,
            new_console,
        } => {
            use std::os::windows::process::CommandExt;
            // P1-1 安全加固（评审 2026-10-03，在生产执行层做——纯构造层与缝测试
            // 保持机器无关）：`cmd /k <裸工具名>` 的 cmd 解析**当前目录优先于
            // PATH**，而 spawn 的 current_dir 恰是用户项目目录——外部仓库携带同名
            // 恶意 .cmd/.exe 即可劫持起窗载荷。双层防御：
            // ① 裸名 → `where` 绝对路径替换 + 分体传参（见 harden_cmd_payload doc）；
            // ② NoDefaultCurrentDirectoryInExePath=1（cmd/CreateProcess 均认）——
            //    解析失败的裸名也不再落回当前目录，残余路径只剩 PATH。
            //    **会话级副作用（评审二轮 P2-3 如实登记）**：该变量随 cmd 终端存续
            //    全程生效并被子孙进程继承——用户在这个终端窗口里裸名调用项目目录
            //    内的可执行体（如 node_modules/.bin 工具）将不再命中，需 .\ 前缀
            //    （与 PowerShell 默认行为一致的安全取舍，换取 cwd 劫持面归零）
            let args = harden_cmd_payload(args, resolve_tool_path);
            let mut cmd = std::process::Command::new(program);
            // P2-1（评审二轮 2026-10-03）：`/k` 载荷改走 **raw_arg**——分体传参时
            // Rust 只对含空格 token 加引号，无空格但含 cmd 元字符的路径（如
            // `...\Program Files (x86)\tool.cmd` 的括号）裸奔在命令行上会被 cmd
            // 拆解（实机复现）。raw_arg 绕开 MSVCRT 转义、由 [`cmd_payload_raw`]
            // 按需引号（干净载荷=裸串与分体等价；元字符载荷=外层包裹）；不可安全
            // 表示（token 含 %/"）→ fail-closed 回分体传参（env 兜底仍防 cwd
            // 劫持，工具起不来是诚实失败）
            match args
                .iter()
                .rposition(|a| a.eq_ignore_ascii_case("/k"))
                .filter(|&k| k + 1 < args.len())
            {
                Some(k) => match cmd_payload_raw(&args[k + 1..]) {
                    Some(payload) => {
                        cmd.args(&args[..=k]);
                        cmd.raw_arg(&payload);
                    }
                    None => {
                        cmd.args(&args);
                    }
                },
                None => {
                    cmd.args(&args);
                }
            }
            cmd.env("NoDefaultCurrentDirectoryInExePath", "1");
            // 新建会话环境红线（C3，spec §4.2）：DISABLE_AUTOUPDATER=1 等显式注入
            // 子进程环境；resume 规格 env 恒空（空迭代器 no-op，零行为变更）
            cmd.envs(env.iter().map(|(k, v)| (k, v)));
            // 会话上下文剥离（C8 实机定案，env_rm_prefixes 字段 doc）：命中前缀的
            // 继承变量逐一 env_remove——create 规格借此切断启动者的 Claude 会话
            // 管道（CLAUDE_CODE_CHILD_SESSION 等）；resume 恒空 no-op
            if !env_rm_prefixes.is_empty() {
                for (k, _) in std::env::vars_os() {
                    let key = k.to_string_lossy().to_string();
                    if env_rm_prefixes.iter().any(|p| key.starts_with(p.as_str())) {
                        cmd.env_remove(&key);
                    }
                }
            }
            // conhost 回退的 cwd 继承点（wt 同设无害：-d 已双保险）
            if !cwd.is_empty() {
                cmd.current_dir(cwd);
            }
            if *new_console {
                // CREATE_NEW_CONSOLE：为 conhost 回退开全新控制台窗
                cmd.creation_flags(0x0000_0010);
            }
            // 统一出口（I3）：本条要的是「新控制台自己的标准句柄」，走
            // [`crate::window::detached::ConsoleMode::OwnConsole`]——**不碰 stdio、也不碰
            // creation_flags**（J2 复评：此前一律置 NUL，会让窗内 cmd /k 的 stdin 指向
            // NUL 设备而可能开出死窗；上面 CREATE_NEW_CONSOLE 的起窗语义亦必须原样保留）。
            // 真机在 explorer 派发点上复现过 os error 6，本处改动前同为「三路全继承 +
            // spawn()」；本处是否真会失败**未在 Windows 真机验证**（属同形加固，不是
            // 已验证的第二处缺陷），机制与定案实验见该模块文档
            crate::window::detached::spawn_detached(
                &mut cmd,
                crate::window::detached::ConsoleMode::OwnConsole,
            )
            .map(|_| ())
            .map_err(|e| format!("终端启动失败（{program}）：{e}"))
        }
        #[cfg(not(windows))]
        SpawnSpec::Windows { program, .. } => Err(format!("当前平台不支持终端 spawn（{program}）")),
        #[cfg(target_os = "macos")]
        SpawnSpec::MacosApplescript { script } => run_osascript_wait(script),
        #[cfg(not(target_os = "macos"))]
        SpawnSpec::MacosApplescript { .. } => {
            Err("AppleScript 执行层仅在 macOS 构建装配".to_string())
        }
    }
}

/// 核心入口（spawner 缝版本）：命令表解析 → cwd 预检 → 按平台组装 spec 交缝出手。
/// 错误契约：哨兵串 `"no_resume_command"` / `"no_cwd"`（远端端点映射 404；
/// 前端按钮同因禁用），其余为人类可读失败文案。
///
/// Windows 出手序（评审 M1）：wt 探测命中 → 先试 wt；spawn 失败（应用执行别名
/// 停用/损坏等场景）**降级重试一次 conhost** 再报 failed（两次错误合并披露，
/// cwd 经 current_dir 继承不丢）。
pub fn open_session_terminal_with(session: &Session, spawner: &SpawnFn) -> Result<(), String> {
    let tool = session.agent_type.tool_id();
    // 未查证/未入表工具：出手前拦截（spawner 不被调用）
    let Some(resume) = resume_command(tool, &session.id) else {
        return Err("no_resume_command".to_string());
    };
    let cwd = session.project_path.trim();
    if cwd.is_empty() {
        return Err("no_cwd".to_string());
    }
    match std::env::consts::OS {
        // Windows：wt 在场先试、败降级 conhost 一次（open_windows_with 跨平台可测）
        "windows" => open_windows_with(windows_terminal_path().as_deref(), cwd, &resume, spawner),
        // macOS：Terminal.app 优先、iTerm2 次选（F1 治理，R1②/R1③）+ 出手后
        // 效果回查（死窗→转次选；双败 failed 回执+审计）。open_macos_with 跨平台
        // 可测（回查缝注入假体）；实机验证归 Mac 回传清单
        "macos" => open_macos_with(cwd, &resume, spawner, &macos_effect_probe),
        _ => Err("当前平台不支持一键恢复会话".to_string()),
    }
}

/// 生产装配（Tauri 命令路径）：真 spawn 终端。
pub fn open_session_terminal(session: &Session) -> Result<(), String> {
    open_session_terminal_with(session, &spawn_terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小会话夹具（字段形状对齐 remote/server.rs inj_sess 先例）
    fn sess(agent_type: crate::session::AgentType, cwd: &str) -> Session {
        Session {
            id: "abc".into(),
            agent_type,
            project_name: "proj".into(),
            project_path: cwd.into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: crate::session::SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-19T00:00:00Z".into(),
            pid: 7,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// T3 归一（实证形态，终审发现 B 现场）：会话记录 cwd = 反斜杠 + 大写盘符，
    /// `~/.claude.json` 双 casing 条款一真一假并存 → 原地改写复用**真条款精确
    /// casing** 且不提醒；命中全 false → cwd 原样 + 提醒触发（tempdir 假 home，
    /// 零接触真实 ~/.claude.json）
    #[test]
    fn normalize_cwd_for_trust_reuses_trusted_casing_and_flags_untrusted() {
        // 双 casing 一真一假（实证的挂起形态）→ 改写为真条款 casing、无提醒
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"projects":{"E:/LLMproject/Test2":{"hasTrustDialogAccepted":false},"e:/LLMproject/Test2":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        let mut s = sess(crate::session::AgentType::Claude, r"E:\LLMproject\Test2");
        assert!(
            !normalize_cwd_for_trust(&mut s, Some(home.path())),
            "复用真条款后不再弹窗，不提醒"
        );
        assert_eq!(
            s.project_path, "e:/LLMproject/Test2",
            "spawn cwd 必须是真条款逐字 casing"
        );

        // 全 false：cwd 原样（无可复用 casing）+ 提醒触发
        let home2 = tempfile::tempdir().unwrap();
        std::fs::write(
            home2.path().join(".claude.json"),
            r#"{"projects":{"/tmp/proj-u":{"hasTrustDialogAccepted":false}}}"#,
        )
        .unwrap();
        let mut s2 = sess(crate::session::AgentType::Claude, "/tmp/proj-u");
        assert!(
            normalize_cwd_for_trust(&mut s2, Some(home2.path())),
            "Some(false)=提醒触发"
        );
        assert_eq!(s2.project_path, "/tmp/proj-u", "全 false 不得改写 cwd");
    }

    /// T3 归一 no-op 面（四路保守降级）：非 claude 工具不消费 claude 信任库 /
    /// 无 home / 全新目录（无条目，首次信任属正常流程）/ 空白 cwd——一律不改写
    /// 不提醒
    #[test]
    fn normalize_cwd_for_trust_noop_surfaces() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"projects":{"/tmp/proj":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        // 非 claude：claude 的信任库对其他工具无语义
        let mut codex = sess(crate::session::AgentType::Codex, "/tmp/proj");
        assert!(!normalize_cwd_for_trust(&mut codex, Some(home.path())));
        assert_eq!(codex.project_path, "/tmp/proj");
        // 无 home（解析失败）：保守 no-op
        let mut no_home = sess(crate::session::AgentType::Claude, "/tmp/proj");
        assert!(!normalize_cwd_for_trust(&mut no_home, None));
        assert_eq!(no_home.project_path, "/tmp/proj");
        // 全新目录（无条目）：不提醒、cwd 原样
        let mut fresh = sess(crate::session::AgentType::Claude, "/tmp/brand-new");
        assert!(!normalize_cwd_for_trust(&mut fresh, Some(home.path())));
        assert_eq!(fresh.project_path, "/tmp/brand-new");
        // 空白 cwd（no_cwd 哨兵面）：不碰
        let mut blank = sess(crate::session::AgentType::Claude, "   ");
        assert!(!normalize_cwd_for_trust(&mut blank, Some(home.path())));
        assert_eq!(blank.project_path, "   ");
    }

    /// Step 2 失败测试 ①：命令表按 Step 1 实测断言——四工具命中，未知/未查证工具
    /// 一律 None（绝不入表）
    #[test]
    fn resume_command_table() {
        assert_eq!(
            resume_command("claude", "abc").as_deref(),
            Some("claude --resume abc")
        );
        assert_eq!(
            resume_command("codex", "abc").as_deref(),
            Some("codex resume abc"),
            "codex 按 Step 1 实测（resume 子命令）"
        );
        assert_eq!(
            resume_command("kimi", "abc").as_deref(),
            Some("kimi --session abc")
        );
        assert_eq!(
            resume_command("opencode", "abc").as_deref(),
            Some("opencode --session abc")
        );
        // 未安装 / 不适用（Step 1 如实记录）+ 未知工具：None → 按钮禁用 + 待查证
        for t in ["zcode", "openclaw", "workbuddy", "dsh", "unknown-tool", ""] {
            assert_eq!(resume_command(t, "abc"), None, "{t} 不得有 resume 映射");
        }
    }

    /// Step 2 失败测试 ②：Windows spawn 计划纯构造——有 wt（完整路径）→
    /// `<路径> -d cwd cmd /k`；无 wt → conhost + CREATE_NEW_CONSOLE（纯构造断言，
    /// 不真 spawn）。cwd 字段两分支同设（评审 I1：conhost 的 cwd 靠 current_dir
    /// 继承，生产 spawner 消费点见 spawn_terminal 的 cwd 臂注释）。
    /// 函数与测试同名：测试体内用 `super::` 显式路径防 glob 导入被本测试名遮蔽
    #[test]
    fn build_spawn_command_windows() {
        let wt = super::build_spawn_command_windows(
            Some(r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\wt.exe"),
            r"E:\proj",
            "claude --resume abc",
        );
        let SpawnSpec::Windows {
            program,
            args,
            cwd,
            new_console,
            ..
        } = wt
        else {
            panic!("wt 在场必须构造 Windows 变体");
        };
        assert_eq!(
            program, r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\wt.exe",
            "评审 M1：缓存 wt 完整路径并进 spec（绕开应用执行别名停用场景）"
        );
        assert_eq!(
            args,
            vec![
                "-d".to_string(),
                r"E:\proj".to_string(),
                "cmd".to_string(),
                "/k".to_string(),
                "claude --resume abc".to_string(),
            ],
            "有 wt → <路径> -d <cwd> cmd /k <resume>"
        );
        assert_eq!(cwd, r"E:\proj", "cwd 字段两分支同设（current_dir 消费）");
        assert!(!new_console, "wt 自开新标签，无需 CREATE_NEW_CONSOLE");

        let conhost = super::build_spawn_command_windows(None, r"E:\proj", "claude --resume abc");
        let SpawnSpec::Windows {
            program,
            args,
            cwd,
            new_console,
            ..
        } = conhost
        else {
            panic!("无 wt 必须构造 Windows 变体（conhost）");
        };
        assert_eq!(program, "conhost.exe");
        assert_eq!(
            args,
            vec![
                "cmd".to_string(),
                "/k".to_string(),
                "claude --resume abc".to_string(),
            ],
            "无 wt → conhost.exe cmd /k <resume>"
        );
        assert_eq!(cwd, r"E:\proj", "评审 I1：conhost 回退不得丢 cwd");
        assert!(new_console, "conhost 必须 CREATE_NEW_CONSOLE 开新窗");
    }

    /// C3：新建会话起窗规格——裸工具命令 + DISABLE_AUTOUPDATER=1（spec §4.2 环境
    /// 红线）；C8 实机补充：env_rm_prefixes 携带会话上下文剥离面；resume 既有规格
    /// env/env_rm_prefixes 恒空（零回归锚，既有 resume 行为不变）
    #[cfg(windows)]
    #[test]
    fn create_spawn_spec_carries_env_and_bare_command() {
        let s = build_create_spawn_spec(None, r"C:\proj", "claude");
        match s {
            SpawnSpec::Windows {
                program,
                args,
                env,
                env_rm_prefixes,
                ..
            } => {
                assert_eq!(program, "conhost.exe");
                assert_eq!(args, vec!["cmd", "/k", "claude"]);
                assert!(env
                    .iter()
                    .any(|(k, v)| k == "DISABLE_AUTOUPDATER" && v == "1"));
                // 会话上下文剥离面（C8 冒烟定案）：CLAUDECODE 与 CLAUDE_CODE_ 必在
                assert!(env_rm_prefixes.contains(&"CLAUDECODE".to_string()));
                assert!(env_rm_prefixes.contains(&"CLAUDE_CODE_".to_string()));
            }
            _ => panic!("Windows 平台必须是 Windows 变体"),
        }
        let w = build_create_spawn_spec(Some(r"C:\wt\wt.exe"), r"C:\proj", "codex");
        match w {
            SpawnSpec::Windows { args, env, .. } => {
                assert_eq!(args, vec!["-d", r"C:\proj", "cmd", "/k", "codex"]);
                assert!(env
                    .iter()
                    .any(|(k, v)| k == "DISABLE_AUTOUPDATER" && v == "1"));
            }
            _ => panic!(),
        }
        // resume 既有规格 env / env_rm_prefixes 恒空（零回归锚，wt/conhost 双分支都
        // 锁）；super:: 显式路径防被同名测试遮蔽
        match super::build_spawn_command_windows(None, r"C:\p", "claude --resume x") {
            SpawnSpec::Windows {
                env,
                env_rm_prefixes,
                ..
            } => {
                assert!(env.is_empty());
                assert!(env_rm_prefixes.is_empty());
            }
            _ => panic!(),
        }
        match super::build_spawn_command_windows(
            Some(r"C:\wt\wt.exe"),
            r"C:\p",
            "claude --resume x",
        ) {
            SpawnSpec::Windows {
                env,
                env_rm_prefixes,
                ..
            } => {
                assert!(env.is_empty());
                assert!(env_rm_prefixes.is_empty());
            }
            _ => panic!(),
        }
    }

    /// D2：新建会话起窗命令表——opencode 2.x 必须带 `--standalone`（端口竞争陷阱，
    /// 复验定案 §2）；其余工具保持裸命令（零回归）
    #[test]
    fn create_command_table_pins_standalone_for_opencode() {
        assert_eq!(create_command("opencode"), "opencode --standalone");
        // 未入表工具 = 裸工具名（零回归）
        assert_eq!(create_command("claude"), "claude");
        assert_eq!(create_command("codex"), "codex");
        assert_eq!(create_command("kimi"), "kimi");
        // 起窗载荷确实带上（跨平台构造层，conhost 分支）
        match build_create_spawn_spec(None, "/tmp/p", "opencode") {
            SpawnSpec::Windows { args, .. } => {
                assert_eq!(args, vec!["cmd", "/k", "opencode --standalone"]);
            }
            _ => panic!("conhost 分支"),
        }
    }

    /// resume 表的 opencode 命令**不带** `--standalone`（复用既有服务是期望行为；
    /// `--session` 回放已两次实机验证）——防与 create 表混用
    #[test]
    fn resume_table_opencode_stays_bare() {
        assert_eq!(
            resume_command("opencode", "abc").as_deref(),
            Some("opencode --session abc")
        );
    }

    /// P1-1：`where` 输出选行——绝对路径 ∧ cmd 可执行扩展名双过滤
    #[test]
    fn pick_where_hit_filters_form_and_extension() {
        // 正序命中：绝对路径 .cmd 首行即取
        assert_eq!(
            pick_where_hit("C:\\npm\\claude.cmd\nC:\\npm\\claude.ps1\n"),
            Some(r"C:\npm\claude.cmd".to_string())
        );
        // 裸文件名行（where 从其 cwd 命中的形态）与无扩展 sh 脚本行跳过，
        // 后续绝对 .exe 行可取
        assert_eq!(
            pick_where_hit("claude\nC:\\npm\\claude\nC:\\npm\\claude.exe\n"),
            Some(r"C:\npm\claude.exe".to_string())
        );
        // 全部不可执行形态（.ps1 / 无扩展）→ None
        assert_eq!(
            pick_where_hit("C:\\npm\\claude.ps1\nC:\\npm\\claude\n"),
            None
        );
        // 相对路径行不认（双保险：调用侧已固定 where 的 cwd）
        assert_eq!(pick_where_hit("npm\\claude.cmd\n"), None);
        assert_eq!(pick_where_hit(""), None);
    }

    /// P1-1：`/k` 载荷加固——裸名解析为绝对路径并分体传参（含空格路径由
    /// Command 逐参引号，不经 MSVCRT 内嵌引号转义）
    #[test]
    fn harden_cmd_payload_absolutizes_bare_name_and_splats() {
        let args: Vec<String> = vec!["cmd".into(), "/k".into(), "claude --resume abc".into()];
        let out = harden_cmd_payload(&args, |n| {
            (n == "claude").then(|| r"C:\npm dir\claude.cmd".to_string())
        });
        assert_eq!(
            out,
            vec![
                "cmd".to_string(),
                "/k".to_string(),
                r"C:\npm dir\claude.cmd".to_string(),
                "--resume".to_string(),
                "abc".to_string(),
            ],
            "裸名 → 绝对路径替换 + 载荷分体传参"
        );
        // wt 前缀链：/k 定位不受前缀参数影响
        let wt: Vec<String> = vec![
            "-d".into(),
            r"E:\proj".into(),
            "cmd".into(),
            "/k".into(),
            "opencode --standalone".into(),
        ];
        let out = harden_cmd_payload(&wt, |n| {
            (n == "opencode").then(|| r"C:\bin\opencode.exe".to_string())
        });
        assert_eq!(
            out,
            vec![
                "-d".to_string(),
                r"E:\proj".to_string(),
                "cmd".to_string(),
                "/k".to_string(),
                r"C:\bin\opencode.exe".to_string(),
                "--standalone".to_string(),
            ]
        );
    }

    /// P1-1：`/k` 载荷加固的原样返回支——已路径形态 / resolver 未命中 / 无 `/k`
    #[test]
    fn harden_cmd_payload_leaves_path_forms_unresolved_and_no_k() {
        // 首 token 已是路径形态（盘符）→ 不改（防二次解析）
        let path_form: Vec<String> = vec![
            "cmd".into(),
            "/k".into(),
            r"C:\x\claude.cmd --resume a".into(),
        ];
        assert_eq!(
            harden_cmd_payload(&path_form, |_| Some(r"C:\evil\x.cmd".into())),
            path_form
        );
        // resolver 未命中（工具不可解析）→ 原样（残余防线 = 环境变量）
        let bare: Vec<String> = vec!["cmd".into(), "/k".into(), "claude --resume a".into()];
        assert_eq!(harden_cmd_payload(&bare, |_| None), bare);
        // 无 /k / /k 后无载荷 → 原样
        let no_k: Vec<String> = vec!["-d".into(), r"E:\p".into(), "cmd".into()];
        assert_eq!(
            harden_cmd_payload(&no_k, |_| Some(r"C:\x.exe".into())),
            no_k
        );
        let k_tail: Vec<String> = vec!["cmd".into(), "/k".into()];
        assert_eq!(
            harden_cmd_payload(&k_tail, |_| Some(r"C:\x.exe".into())),
            k_tail
        );
    }

    /// P1-1 真机锚（仅 Windows）：`where cmd` 必解析出 System32 绝对路径——
    /// resolve_tool_path 的 IO 链路（cwd 固定 SystemRoot + pick_where_hit）走通
    #[cfg(windows)]
    #[test]
    fn resolve_tool_path_finds_cmd_on_windows() {
        let hit = resolve_tool_path("cmd").expect("cmd 必在 PATH（System32）");
        assert!(
            hit.to_ascii_lowercase().ends_with("cmd.exe"),
            "解析产物应为 cmd.exe 绝对路径：{hit}"
        );
    }

    /// P2-1：raw 载荷构造——干净载荷裸串（与分体传参等价，wt 中转安全红线）；
    /// 元字符 token 内层引号 + 外层整体包裹（≥4 引号防 cmd 剥）；含 %/" fail-closed
    #[test]
    fn cmd_payload_raw_quotes_metacharacters() {
        let toks: Vec<String> = vec![
            r"C:\Program Files (x86)\tool&co\claude.cmd".into(),
            "--resume".into(),
            "abc".into(),
        ];
        assert_eq!(
            cmd_payload_raw(&toks).as_deref(),
            Some(r#"""C:\Program Files (x86)\tool&co\claude.cmd" --resume abc""#),
            "任一 token 需引号 → 内层引号 + 外层整体包裹（恰两引号会被 cmd 剥、剥后元字符裸奔）"
        );
        // 干净 token：裸 join 零引号——与分体传参命令行等价（无条件外层包裹经 wt
        // 中转会让 codex 注入失效，E2E 矩阵两轮实证，勿回退）
        let clean: Vec<String> = vec![r"C:\npm\claude.cmd".into(), "--resume".into()];
        assert_eq!(
            cmd_payload_raw(&clean).as_deref(),
            Some(r"C:\npm\claude.cmd --resume")
        );
        // % / " 不可安全表示（引号内 %VAR% 仍展开）→ None
        assert_eq!(cmd_payload_raw(&[r"C:\a%b\tool.cmd".into()]), None);
        assert_eq!(cmd_payload_raw(&[r#"C:\a"b\tool.cmd"#.into()]), None);
        // 空载荷 → 空串（调用方按无载荷分支不会走到，防御形态）
        assert_eq!(cmd_payload_raw(&[]), Some(String::new()));
    }

    /// P2-1 真机锚（仅 Windows）：元字符目录下的 .cmd 经生产同构 raw 载荷真跑通
    /// ——`paren(x)&co` 目录名同时压括号与 &（分体传参形态被 cmd 拆解、实机
    /// 复现的失效面）。脚本自证：向自身目录写 out.txt。CREATE_NO_WINDOW 免弹窗。
    #[cfg(windows)]
    #[test]
    fn raw_arg_payload_runs_cmd_in_metacharacter_dir() {
        use std::os::windows::process::CommandExt;
        let base = std::env::temp_dir().join(format!(
            "mam-p21-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = base.join("paren(x)&co");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hi.cmd");
        std::fs::write(&script, "@echo off\r\necho ok> \"%~dp0out.txt\"\r\n").unwrap();
        // 与 spawn_terminal 同构造：cmd + /c（同 /k 的引号解析规则）+ raw 载荷
        let payload = cmd_payload_raw(&[script.to_string_lossy().to_string()])
            .expect("元字符路径必须可构造 raw 载荷");
        let out = std::process::Command::new("cmd")
            .arg("/c")
            .raw_arg(&payload)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .expect("spawn cmd");
        assert!(
            out.status.success(),
            "cmd 须成功：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let written = std::fs::read_to_string(dir.join("out.txt")).unwrap_or_default();
        assert!(
            written.contains("ok"),
            "脚本须真被执行（out.txt=ok）：{written}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
    /// macOS 构造层（跨平台可测）：cd '<cwd>' && <resume> 进脚本 + activate 置前；
    /// 转义路径——反斜杠/双引号经 applescript_escape，内嵌单引号经 POSIX '\'' 转义
    #[test]
    fn macos_scripts_carry_cd_focus_and_escaping() {
        let s = iterm_open_window_script("/tmp/proj", "claude --resume abc");
        assert!(s.contains("create window with default profile command"));
        assert!(s.contains(r#"cd '/tmp/proj' && claude --resume abc"#));
        assert!(s.contains("activate"), "完成后置前聚焦");

        let t = terminal_open_script("/tmp/proj", "claude --resume abc");
        assert!(t.contains(r#"do script "cd '/tmp/proj' && claude --resume abc""#));
        assert!(t.contains("activate"));

        // 双引号/反斜杠经 applescript_escape（C:\wei"rd → C:\\wei\"rd），字面量不破
        let e = terminal_open_script(r#"C:\wei"rd"#, "claude --resume abc");
        assert!(e.contains(r#"cd 'C:\\wei\"rd'"#));
        // 内嵌单引号：先 shell '\'' 转义、后 applescript_escape 把该反斜杠再翻倍
        // （\\）——AppleScript 字面量解一转义后 shell 收到的仍是 `'\''`，语义正确
        let q = iterm_open_window_script("/tmp/it's", "claude --resume abc");
        assert!(q.contains(r#"cd '/tmp/it'\\''s'"#));
    }

    /// 核心错误契约：无映射 / 无 cwd 哨兵在出手前返回（spawner 不得被调用）
    #[test]
    fn core_open_sentinels_before_spawn() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let spawner = {
            let calls = calls.clone();
            move |_spec: &SpawnSpec| {
                *calls.lock().unwrap() += 1;
                Ok(())
            }
        };
        // 无映射工具（workbuddy 未入表）
        let no_map = sess(crate::session::AgentType::WorkBuddy, "/tmp/p");
        assert_eq!(
            open_session_terminal_with(&no_map, &spawner).unwrap_err(),
            "no_resume_command"
        );
        // 无 cwd（空白按无目录处理）
        let no_cwd = sess(crate::session::AgentType::Claude, "   ");
        assert_eq!(
            open_session_terminal_with(&no_cwd, &spawner).unwrap_err(),
            "no_cwd"
        );
        assert_eq!(*calls.lock().unwrap(), 0, "哨兵错误必须在出手前返回");
    }

    /// 核心出手路径（Windows）：Ok 且 spawner 恰被调用一次，携带命令表产物
    /// （探测 wt 真跑一次 `where`，OnceLock 缓存——approve.rs 实测探测先例同口径）
    #[cfg(windows)]
    #[test]
    fn core_open_dispatches_spawn_on_windows() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Ok(())
            }
        };
        let s = sess(crate::session::AgentType::Claude, "/tmp/proj");
        assert_eq!(open_session_terminal_with(&s, &spawner), Ok(()));
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "spawner 恰被调用一次");
        assert!(
            recorded
                .iter()
                .any(|spec| matches!(spec, SpawnSpec::Windows { args, .. } if args.iter().any(|a| a == "claude --resume abc"))),
            "spawn 计划必须携带命令表产物：{recorded:?}"
        );
    }

    /// 评审 M1 降级链（驱动 open_windows_with 真链）：wt 出手失败 → 降级重试一次
    /// conhost，两次皆败错误合并；wt 不在场 → 只有 conhost 一次出手
    #[test]
    fn windows_wt_failure_falls_back_to_conhost_once() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                let is_wt = matches!(spec, SpawnSpec::Windows { program, .. } if program.ends_with("wt.exe"));
                let cwd = match spec {
                    SpawnSpec::Windows { cwd, .. } => cwd.clone(),
                    _ => String::new(),
                };
                calls.lock().unwrap().push(spec.clone());
                if is_wt {
                    // cwd 经参数直传（current_dir 在生产 spawner 消费）——降级链
                    // 两次出手必须携带同一 cwd
                    assert_eq!(cwd, "/tmp/proj", "降级链不得丢 cwd");
                    Err("wt 启动失败（模拟别名停用）".to_string())
                } else {
                    Ok(())
                }
            }
        };
        // wt 败 → conhost 接力成功：恰两次出手，Ok
        assert_eq!(
            open_windows_with(
                Some("C:\\fake\\wt.exe"),
                "/tmp/proj",
                "claude --resume abc",
                &spawner
            ),
            Ok(()),
            "wt 失败必须降级 conhost 重试"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "降级链恰两次出手（wt → conhost）");
        assert!(
            matches!(&recorded[0], SpawnSpec::Windows { program, .. } if program.ends_with("wt.exe")),
            "首次出手是 wt"
        );
        assert!(
            matches!(&recorded[1], SpawnSpec::Windows { program, new_console, cwd, .. }
                if program == "conhost.exe" && *new_console && cwd == "/tmp/proj"),
            "降级出手是 conhost + CREATE_NEW_CONSOLE + 原 cwd"
        );

        // wt 不在场：只有 conhost 一次出手（链路入口直接 None）
        let calls2 = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let spawner2 = {
            let calls2 = calls2.clone();
            move |spec: &SpawnSpec| {
                assert!(
                    matches!(spec, SpawnSpec::Windows { program, .. } if program == "conhost.exe"),
                    "无 wt 只应出手 conhost"
                );
                *calls2.lock().unwrap() += 1;
                Ok(())
            }
        };
        assert_eq!(
            open_windows_with(None, "/tmp/proj", "claude --resume abc", &spawner2),
            Ok(())
        );
        assert_eq!(*calls2.lock().unwrap(), 1, "无 wt 恰一次出手");
    }

    /// F1 治理后 macOS 双通道入缝（跨平台可测）：**Terminal.app 优先**（R1②——
    /// iTerm2 带命令 create window 恒死窗假成功）；首通道败（spawn Err 或回查未
    /// 命中死窗）→ iTerm2 次选；spec 恒 MacosApplescript 变体且携带
    /// cd '<cwd>' && <resume> 载荷
    #[test]
    fn macos_dual_channel_dispatches_via_seam() {
        // ① 首通道（Terminal.app）成功 + 回查命中：恰一次出手，script 为 do script 形态
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Ok(())
            }
        };
        assert_eq!(
            open_macos_with("/tmp/proj", "claude --resume abc", &spawner, &|_| true),
            Ok(())
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "首通道成功恰一次出手");
        let SpawnSpec::MacosApplescript { script } = &recorded[0] else {
            panic!("macOS 缝必须收 AppleScript 变体");
        };
        assert!(script.contains("cd '/tmp/proj' && claude --resume abc"));
        assert!(script.contains("activate"));
        assert!(
            script.contains(r#"do script "cd '/tmp/proj'"#),
            "首通道是 Terminal.app 形态（F1：Terminal 优先）"
        );

        // ② 首通道 spawn 失败：降级 iTerm2 次选（第二次出手 create window 形态）
        let calls2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner2 = {
            let calls2 = calls2.clone();
            move |spec: &SpawnSpec| {
                calls2.lock().unwrap().push(spec.clone());
                Err("osascript 失败".to_string())
            }
        };
        let _ = open_macos_with("/tmp/proj", "claude --resume abc", &spawner2, &|_| true);
        let recorded2 = calls2.lock().unwrap().clone();
        assert_eq!(recorded2.len(), 2, "首通道败必须降级第二通道");
        let SpawnSpec::MacosApplescript { script: second } = &recorded2[1] else {
            panic!("第二通道同为 AppleScript 变体");
        };
        assert!(
            second.contains("create window with default profile command"),
            "次选是 iTerm2 形态"
        );
    }

    /// F1② 核心序列：Terminal 出手 Ok 但**回查未命中（死窗）**→ 自动转 iTerm2
    /// 次选；次选回查命中 → Ok。回查缝逐通道各被调一次（传 resume 命令）
    #[test]
    fn macos_terminal_dead_window_check_miss_falls_to_iterm2() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let checks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Ok(())
            }
        };
        let checks_ref = checks.clone();
        let check = move |resume: &str| {
            // 首通道（Terminal）回查未命中，次选命中——按出手次数区分
            let n = checks_ref.lock().unwrap().len();
            checks_ref.lock().unwrap().push(resume.to_string());
            n >= 1
        };
        assert_eq!(
            open_macos_with("/tmp/proj", "claude --resume abc", &spawner, &check),
            Ok(()),
            "Terminal 死窗必须自动转 iTerm2 次选"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "死窗转次选恰两次出手");
        assert!(
            matches!(&recorded[0], SpawnSpec::MacosApplescript { script } if script.contains("do script")),
            "首通道 Terminal.app"
        );
        assert!(
            matches!(&recorded[1], SpawnSpec::MacosApplescript { script } if script.contains("create window")),
            "次选 iTerm2"
        );
        let checked = checks.lock().unwrap().clone();
        assert_eq!(checked.len(), 2, "每通道各回查一次");
        assert_eq!(checked[0], "claude --resume abc", "回查收 resume 命令特征");
    }

    /// F1② 双通道皆败（spawn Err / 回查未命中两种失败混合）→ Err 合并且**逐通道
    /// 标注**——端点 Err 臂 → 200 failed 回执 + 审计 open failed:*（账实一致，
    /// 「死窗」入账；端点契约面由 server.rs session_open_endpoint_spawn_failure
    /// 系列测试锁定）
    #[test]
    fn macos_both_channels_fail_merged_with_channel_labels() {
        // 回查未命中形态（两通道双双死窗：恒 false 无状态假体，dyn Fn 要求 Fn）
        let check = |_: &str| false;
        let spawner = |spec: &SpawnSpec| {
            let _ = spec;
            Ok(())
        };
        let err =
            open_macos_with("/tmp/proj", "claude --resume abc", &spawner, &check).unwrap_err();
        assert!(err.contains("Terminal.app"), "错误必须标注首通道：{err}");
        assert!(err.contains("iTerm2"), "错误必须标注次选通道：{err}");
        assert!(
            err.contains("回查未命中"),
            "死窗失败必须点明回查未命中：{err}"
        );

        // 纯 spawn Err 形态（既有语义，通道名标注）
        let spawner_err = |_: &SpawnSpec| Err("osascript 失败".to_string());
        let err2 = open_macos_with("/tmp/proj", "claude --resume abc", &spawner_err, &|_| true)
            .unwrap_err();
        assert!(err2.contains("Terminal.app: osascript 失败"), "{err2}");
        assert!(err2.contains("iTerm2: osascript 失败"), "{err2}");
    }

    /// F1② 回查判定纯函数：快照含特征 → 命中；不含 → 未命中；空特征防御 false
    #[test]
    fn resume_effect_in_snapshot_pure() {
        let snap = vec![
            "zsh -c cd '/tmp/p' && claude --resume abc".to_string(),
            "node /usr/local/bin/claude --resume abc".to_string(),
        ];
        assert!(resume_effect_in_snapshot("claude --resume abc", &snap));
        assert!(!resume_effect_in_snapshot("codex resume abc", &snap));
        assert!(!resume_effect_in_snapshot("   ", &snap), "空特征不得通配");
        assert!(!resume_effect_in_snapshot("claude --resume abc", &[]));
    }

    /// M2：osascript 错误分类（纯函数跨平台可测）——TCC -1743 /「Not authorized to
    /// send Apple events」→ 中文授权指引（用户裁决口径原文）
    #[test]
    fn classify_resume_error_maps_tcc_to_guidance() {
        // Mac 验收 D-5 实机 stderr 形态（osascript 标准错误原文，§四-B）
        let real =
            "script: execution error: Not authorized to send Apple events to Terminal (-1743).";
        assert_eq!(classify_resume_error(real), MACOS_TCC_GUIDANCE);
        // 仅 -1743 字样（退出码形态兜底路径）同样命中
        assert_eq!(
            classify_resume_error("execution error: (-1743)"),
            MACOS_TCC_GUIDANCE
        );
        // 大小写不敏感（osascript 输出口径漂移容错）
        assert_eq!(
            classify_resume_error("not authorized to send apple events"),
            MACOS_TCC_GUIDANCE
        );
    }

    /// M2：osascript 错误分类——非 TCC 错误保持 stderr 摘要（截断防膨胀）；空
    /// stderr 有界兜底（回执不悬空）
    #[test]
    fn classify_resume_error_other_keeps_summary() {
        // 非 TCC 错误保留原始摘要（-1728 找不到应用），不得误报授权指引
        let out =
            classify_resume_error("execution error: Can't get application \"iTerm2\". (-1728)");
        assert!(out.contains("-1728"), "摘要保留原始错误：{out}");
        assert!(
            !out.contains("自动化授权"),
            "非 TCC 错误不得误报授权指引：{out}"
        );
        // 超长 stderr 截断（+1 为截断省略号）——回执与审计双消费均有界
        let long = format!("execution error: {}", "x".repeat(500));
        let out = classify_resume_error(&long);
        assert!(
            out.chars().count() <= RESUME_STDERR_SUMMARY_CHARS + 1,
            "stderr 摘要必须截断：{} chars",
            out.chars().count()
        );
        // 空/纯空白 stderr → 有界兜底文案
        assert_eq!(
            classify_resume_error("   "),
            "osascript 执行失败（无 stderr 输出）"
        );
    }
}

#[cfg(test)]
mod cmd_snapshot_tests {
    use super::refresh_cmd_snapshot;

    /// sysinfo 两参 refresh 回归锁（Mac 实测 2026-09-20：默认 Kind 不刷 cmd，
    /// 全量进程 cmd() 为空 → resume 效果回查恒假阴性）：specifics 刷新后
    /// 自身进程命令行必须非空。跨平台常跑（Windows 门禁即锁）。
    #[test]
    fn cmd_snapshot_own_process_cmd_nonempty() {
        let mut sys = sysinfo::System::new();
        let _ = refresh_cmd_snapshot(&mut sys);
        let me = sys.process(sysinfo::Pid::from_u32(std::process::id()));
        assert!(
            me.map(|p| !p.cmd().is_empty()).unwrap_or(false),
            "自身进程 cmd() 为空——进程刷新配置回归（两参 refresh 默认 Kind 不含 cmd）"
        );
    }
}
