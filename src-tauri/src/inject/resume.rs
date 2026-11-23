//! R5 一键 resume 窗口（M6R–M9R 批次 Task 11）：
//! 手机或桌面点一下 → 本机自动打开终端 + 进入项目目录 + 恢复会话 + 置前聚焦，
//! 用户零额外操作。终端选择：Windows 优先 Windows Terminal、未装回退 conhost；
//! macOS **Terminal.app 优先、iTerm2 次选**（F1 治理 2026-09-20：iTerm2 3.7.2+
//! macOS26 带命令 create window 恒死窗假成功——mac-reverify-b9a501c §四-A/R1②，
//! 上游升级复测后再翻转；osascript **等待执行 + 失败回执**（M2 修法）+ **出手后
//! 效果回查**（F1②：2–3s 内按注入命令特征查子进程，未命中转次选，双败 failed
//! 回执 + 审计 failed——「死窗」纳入账实一致；**G5 收口：opencode 恢复链在交付
//! 成功后回查未命中不再转次选**——pending 在场禁 fallback，绝不二窗；legacy
//! fallback 仅非 opencode 保留，见 [`open_macos_with`]）。
//! **G2 收口：恢复服务密码不入命令文本**——显式连接形态经受控本地启动入口
//! （0600 env 文件 source 即删，[`EnvSecret`] / [`materialize_env_secret`]）交付
//! 子进程环境；起窗前三核对（端点回环 / 登记进程身份 / 目标会话规范目录）失败
//! 零起窗，见 [`prepare_opencode_resume_with_sources`] / restore_pending ④。
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
pub type SpawnFn = ScopedSpawnFn<'static>;
type ScopedSpawnFn<'a> = dyn Fn(&SpawnSpec) -> Result<(), String> + Send + Sync + 'a;

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
        /// I4 聚焦段脚本（Some = 起窗交付成功后 best-effort 执行的聚焦段——
        /// [`terminal_focus_script`] 构造；聚焦失败只降级日志、**永不**改变交付
        /// 判定）。None = 该通道无独立聚焦段（iTerm2：activate 先于 create window
        /// 是其交付语义一部分，且末通道无 fallback 误触发面，保持原样零行为变更）。
        /// **只在生产 spawner 消费**——测试注入的记录型假体不执行（零真实 osascript）。
        focus_script: Option<String>,
        /// G2 受控启动入口秘密（Some = 交付时刻由生产 spawner 落盘 0600 env 文件，
        /// 载荷 `source` 后即删；None = 无凭据面，照常出手）。**只在生产 spawner
        /// 消费**——测试注入的记录型假体不消费（密码零落盘，测试零复制真实凭据）。
        env_secret: Option<EnvSecret>,
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
/// - opencode：`--session <id>` 语法查证版本演进——1.18.31（`-s, --session  session
///   id to continue`，2026-09-19 Step 1 实测）→ **2.2.5（M7 LOW-1 复验：隔离环境
///   真实 2.2.5 二进制 `--session <已关会话>` 实跑载入成功，证据
///   `task-M7-report.md` §语法复验实录）**。表内载荷只是模板：M7 起起窗分发层对
///   opencode 先过二进制解析+版本门（见模块尾部 M7 段），实际载荷 = 规范二进制
///   绝对路径形态，裸 PATH 名不再出手（模板保留供审计摘要与禁用判定）。
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

// ==== M7（B1 工程修复，2026-11-16 复审裁决）：opencode 恢复链二进制解析与版本门 ====
// N6 E1 精化事实：resume 载荷恒为 PATH 相对名 `opencode`，macOS 起窗经 Terminal.app
// 登录 shell 解析 PATH 命中 **Homebrew 1.19.4**（≠ 规范 2.2.5）——起窗二进制版本错配
// 使「可跑」也不能当 2.2.5 恢复链证据。修法三件套：
// ① 解析规范二进制（优先级见 [`resolve_opencode_binary_in`] doc，存在性/可执行性
//    不满足一律结构化诚实失败，**绝不静默回落 PATH**——Homebrew 错配即死法）；
// ② 起窗前对解析产物跑 `--version`（有界超时 [`OPENCODE_VERSION_PROBE_TIMEOUT_SECS`]），
//    与被恢复会话登记版本 [`OPENCODE_ANCHORED_VERSION`] 不符 → 拒绝恢复 + 可读原因；
// ③ 载荷改为解析产物的**绝对路径**形态（裸 PATH 相对名退役），macOS 脚本载荷对路径
//    单引号包裹，效果回查特征改用 argv 直拼形态（见 [`OpenCodeResumePayload`] doc）。
// 其余工具（claude/codex/kimi）零行为变更：不进本门，载荷与回查特征同旧。

/// opencode 恢复链登记版本（M7 版本门的比较基准）。依据：归属语义证据登记表
/// `services/opencode_native/attribution_evidence.rs`（唯一登记构建 v2.2.5，
/// H02 锚 SHA `abe4d885…`）——恢复链与原生语义同源锚定，低版本二进制起窗会让
/// 恢复语义失效（N6 实证 1.19.4 错配）。升级 = 改本常量 + attribution_evidence
/// 登记复验 + 本轮同款隔离语法复验，三者缺一不可。
pub const OPENCODE_ANCHORED_VERSION: &str = "2.2.5";

/// 显式配置键（与 `remote/mod.rs` opencode_binary 同键同义——全库一个覆盖点；
/// 桥 reload 与 resume 解析共用同一指认）。
const OPENCODE_BINARY_ENV: &str = "OPENCODE_BINARY";

/// `--version` 探测有界超时（秒）。approve.rs probe_cli_version 3s 先例 + 226MB
/// 二进制冷启动裕量；超时即杀 + 收尸（防僵尸）+ 结构化失败（fail-closed，不跳过门）。
const OPENCODE_VERSION_PROBE_TIMEOUT_SECS: u64 = 5;

/// 结构化错误码（远端端点 200 failed + 审计 `open failed:<code>:…` 承载；非哨兵，
/// 不进 404 分诊——拒绝发生在会话命中之后，工具身份在场，审计必须留痕）。
pub(crate) const OPENCODE_BINARY_MISSING: &str = "OPENCODE_BINARY_MISSING";
pub(crate) const OPENCODE_BINARY_NOT_EXECUTABLE: &str = "OPENCODE_BINARY_NOT_EXECUTABLE";
pub(crate) const OPENCODE_VERSION_PROBE_FAILED: &str = "OPENCODE_VERSION_PROBE_FAILED";
pub(crate) const OPENCODE_VERSION_MISMATCH: &str = "OPENCODE_VERSION_MISMATCH";

/// G2 起窗前三核对失败哨兵（端点非回环）：出手前拦截，spawner 零出手、pending
/// 零插入；端点契约映射 404（校验失败，非出手后失败）。
pub(crate) const OC_RESUME_ENDPOINT_NOT_LOOPBACK: &str = "no_loopback_endpoint";
/// G2 起窗前三核对失败哨兵（登记进程身份不可核验：登记 pid 已死/进程事实不可读）。
pub(crate) const OC_RESUME_REGISTRATION_UNVERIFIED: &str = "registration_process_unverified";
/// G2 起窗前三核对失败哨兵（目标会话目录不可解析：project_path realpath 失败/非目录）。
pub(crate) const OC_RESUME_DIRECTORY_UNRESOLVED: &str = "session_directory_unresolved";

/// HC 受控入口目录校验失败哨兵（H3）：目录符号链接/他者归属/含组他者写位 ⇒ 拒绝
/// 落盘零起窗（异常目录可能是他者栽赃面，拒绝而非静默修补）。
pub(crate) const ENV_SECRET_DIR_UNSAFE: &str = "resume_env_dir_unsafe";
/// HC 受控入口路径形状哨兵（H3）：文件名越出 `resume-env-*.sh` 白名单形状 / 路径
/// 含 `..` 逃逸分量 ⇒ 拒绝落盘零起窗。
pub(crate) const ENV_SECRET_PATH_UNSAFE: &str = "resume_env_path_unsafe";
/// HC 受控入口独占创建冲突哨兵（H3）：同名文件已存在（他者遗留/栽赃/符号链接）
/// ⇒ 拒绝覆盖、放弃本次交付（token 含纳秒+pid+进程计数器，实际碰撞面可忽略；
/// 冲突几乎必然异常，fail-closed 不换名绕过）。
pub(crate) const ENV_SECRET_COLLISION: &str = "resume_env_collision";

/// 恢复链校验失败哨兵的前缀形态识别 + 裸码归一（GB fix round 1，GR1 GB-1）。
///
/// 预备链（[`prepare_opencode_resume_with_sources`]）/目录核对（restore_pending ④）
/// 产出的 Err = `"哨兵: 中文可读文案…"` **带前缀形态**；消费端（remote/api.rs 404
/// 免审计臂、commands/session.rs 桌面 toast 转译臂）原先用**裸哨兵 `==` 精确等值**
/// 匹配——对带前缀形态恒 miss = 死码（404 臂永不命中 → 落通用 Failed 臂＝写审计
/// +回 200，违背「校验失败不写审计」；toast 直展带前缀原文）。改前缀匹配，并归一
/// 出裸哨兵码供回执体复用（客户端契约面与旧裸哨兵同形）。旧裸哨兵
/// `no_resume_command` / `no_cwd` 一并纳入（两形态都认，防上游形态漂移）。
/// 非 `Some` = 非校验失败（出手后失败，走 200 failed + 审计既有契约）。
pub(crate) fn resume_validation_error_code(e: &str) -> Option<&'static str> {
    const VALIDATION_SENTINELS: [&str; 5] = [
        "no_resume_command",
        "no_cwd",
        OC_RESUME_ENDPOINT_NOT_LOOPBACK,
        OC_RESUME_REGISTRATION_UNVERIFIED,
        OC_RESUME_DIRECTORY_UNRESOLVED,
    ];
    VALIDATION_SENTINELS
        .into_iter()
        .find(|s| e == *s || e.starts_with(&format!("{s}: ")))
}

/// 受控启动入口的 env 秘密（G2）：恢复服务密码**不再进命令文本/osascript 参数**，
/// 改经 0600 env 文件（用户缓存目录 + 随机文件名）交给子进程环境。
/// * [`Self::path`] 在预备链（纯构造）生成并嵌进 macOS 载荷（`source '<path>' &&
///   rm -f '<path>' && …`——source 后即删）；
/// * 文件本体在**交付时刻**由生产 spawner（[`spawn_terminal`]）落盘（0600）——
///   预备/测试路径零落盘（测试注入路径即可断言载荷形态，真实密码零复制）；
/// * Debug 一律脱敏：密码不得随日志/调试输出外泄。pub 仅为承载 [`SpawnSpec`]
///   公开变体字段（private_interfaces），消费面仍在 crate 内。
#[derive(Clone, PartialEq)]
pub struct EnvSecret {
    pub path: std::path::PathBuf,
    pub password: String,
}

impl std::fmt::Debug for EnvSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvSecret")
            .field("path", &self.path)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// env 文件内容（交付时刻落盘的正文）：POSIX `export` + 单引号转义。zsh/bash/
/// fish(≥3.1 内建 export)/csh 的 `source` 语义均消费 export 行；密码按
/// [`shell_single_quote`] 转义（`'` → `'\''`，与既有载荷同源零回退）。
pub(crate) fn render_env_secret_content(password: &str) -> String {
    format!(
        "export OPENCODE_PASSWORD={}\n",
        shell_single_quote(password)
    )
}

/// 用户缓存目录下的受控 env 文件目录（0700）：`<cache>/tuvis/resume-env/`。
/// 生产路径（macOS = ~/Library/Caches/tuvis/resume-env/）；HOME 重定向（候选/
/// 测试隔离轮）天然落到隔离根，零接触真实缓存。
pub(crate) fn resume_env_dir() -> Result<std::path::PathBuf, String> {
    let cache = dirs::cache_dir().ok_or_else(|| {
        "G2 受控启动入口：用户缓存目录不可解析（cache_dir 缺失），拒绝构造恢复载荷".to_string()
    })?;
    Ok(cache.join("tuvis").join("resume-env"))
}

/// 唯一 token 核（纳秒时钟 + pid + 进程级计数器；无 rand 依赖，同进程内 seq 单调，
/// 碰撞面可忽略）。I3 请求唯一标识与 env 文件名 token **同族同源**（同一生成核，
/// 不建第二套标识）：env 文件名只要求同目录唯一（内容 0600），请求 token 要求
/// 同会话跨请求可区分——两者均由本核承载。
pub(crate) fn unique_request_token() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{:x}", nanos, std::process::id(), seq)
}

/// 随机文件名 token（[`unique_request_token`] 同核加文件名前后缀；sweep 前缀
/// `resume-env-` / 后缀 `.sh` 识别面保持）。
fn env_secret_token() -> String {
    format!("resume-env-{}.sh", unique_request_token())
}

/// 残留清扫（G2 生命周期申报面收敛）：删除目录内超过 24h 的 resume-env-*.sh 残留
/// （崩溃/窗口未跑 rm 的残留，brief 允许 ≤1 个且 0600；清扫令累积上界归零）。
/// 全路径 best-effort：清扫失败不影响恢复链（残留仍有 0600 保护）。
/// HC 起提升 pub(crate)：交付未知生命周期的 24h 兜底清扫由 restore_pending
/// 结算测试同源消费（同一把扫帚，不建第二套）。
pub(crate) fn sweep_stale_resume_env_files(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("resume-env-") || !name.ends_with(".sh") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age.as_secs() > 24 * 3600)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 受控启动入口秘密生成（生产装配，预备链消费）：**只生成路径，不落盘**——
/// 文件本体在交付时刻由 [`spawn_terminal`] 落盘（见 [`materialize_env_secret`]），
/// 预备失败/测试路径零密码落盘。落盘目录不可解析 → 结构化失败（零起窗）。
fn env_secret_writer_production(password: &str) -> Result<EnvSecret, String> {
    let dir = resume_env_dir()?;
    Ok(EnvSecret {
        path: dir.join(env_secret_token()),
        password: password.to_string(),
    })
}

/// 交付时刻落盘（生产 spawner 消费）：HC 全生命周期加固版（目录校验 + 独占创建
/// 0600 + 写失败清理），生产 writer 特化。失败 = 结构化 Err（osascript 不出手
/// ——宁可不恢复也不把密码泄进命令文本回退）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn materialize_env_secret(secret: &EnvSecret) -> Result<(), String> {
    materialize_env_secret_with_writer(secret, &write_env_secret_content)
}

/// 生产内容写手（[`materialize_env_secret`] 特化）：write_all + flush。写入失败
/// 由调用方清理半成品（凭据正文零落盘面）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn write_env_secret_content(file: &mut std::fs::File, content: &str) -> Result<(), String> {
    use std::io::Write;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    file.flush().map_err(|e| format!("flush: {e}"))
}

/// 当前用户 uid（HC 目录归属校验视角源；Windows 构建 0 不参与校验）。
#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// 受控 env 目录安全校验（HC H3）：目录非符号链接、是目录、归属当前用户、权限
/// 不含组/他者写位（裁定口径：写位归零面；读/执行位不构成凭据暴露面）。任何异常
/// = 结构化拒绝——不静默修补既有目录（异常可能是他者栽赃面，修补=掩盖证据）。
#[cfg(unix)]
fn validate_resume_env_dir(dir: &std::path::Path, current_uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(dir).map_err(|e| {
        format!(
            "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口目录不可读（{}）：{e}",
            dir.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口目录是符号链接（{}）——拒绝落盘",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口路径不是目录（{}）",
            dir.display()
        ));
    }
    if meta.uid() != current_uid {
        return Err(format!(
            "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口目录归属他者（uid {} ≠ 当前 {}，{}）——拒绝落盘",
            meta.uid(),
            current_uid,
            dir.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口目录权限过宽（{:o}，含组/他者写位，{}）——拒绝落盘",
            mode,
            dir.display()
        ));
    }
    Ok(())
}

/// 路径形状白名单校验（HC H3）：文件名必须 `resume-env-*.sh`（与
/// [`env_secret_token`] 同形）且全路径不得含 `..` 分量（逃逸面归零）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn validate_env_secret_path(path: &std::path::Path) -> Result<(), String> {
    let name_ok = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("resume-env-") && n.ends_with(".sh"))
        .unwrap_or(false);
    if !name_ok {
        return Err(format!(
            "{ENV_SECRET_PATH_UNSAFE}: 受控启动入口文件名越出白名单形状（resume-env-*.sh）：{}",
            path.display()
        ));
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "{ENV_SECRET_PATH_UNSAFE}: 受控启动入口路径含 `..` 逃逸分量：{}",
            path.display()
        ));
    }
    Ok(())
}

/// 受控入口全生命周期加固版落盘（HC H3 校验/独占创建/写失败清理核心入口）：
/// ① 路径形状白名单（文件名 + `..` 逃逸）→ ② 目录安全（存在即严格校验拒绝，
/// 缺失才自建收紧 0700 并回读校验）→ ③ 独占创建（create-new 语义 O_EXCL 等价，
/// 0600 一次性设置；同名已存在 = 结构化拒绝**不覆盖**他者文件——token 含纳秒、
/// pid、进程计数器，冲突几乎必然是残留/栽赃，fail-closed 不换名绕过：换名会让
/// 交付在异常目录里继续，恰恰放大暴露面）→ ④ 权限核验（umask 只减不加，0600
/// 归一并回读断言；异常即清理拒绝）→ ⑤ 校验全部成立后才写内容（writer 注入缝，
/// 失败即清理半成品——凭据正文零落盘面）。GB [`materialize_env_secret`] 为其
/// 生产 writer 特化。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn materialize_env_secret_with_writer(
    secret: &EnvSecret,
    writer: &dyn Fn(&mut std::fs::File, &str) -> Result<(), String>,
) -> Result<(), String> {
    // ① 路径形状白名单
    validate_env_secret_path(&secret.path)?;
    let parent = secret.path.parent().ok_or_else(|| {
        format!(
            "{ENV_SECRET_PATH_UNSAFE}: 受控启动入口路径无父目录（{}）",
            secret.path.display()
        )
    })?;
    // ② 目录安全：存在即校验（异常拒绝而非静默修补）；缺失才创建 + 收紧 + 回读
    match std::fs::symlink_metadata(parent) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口目录是符号链接（{}）——拒绝落盘",
                    parent.display()
                ));
            }
            #[cfg(unix)]
            {
                validate_resume_env_dir(parent, current_uid())?;
            }
            #[cfg(not(unix))]
            {
                let _ = meta;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("受控启动入口目录创建失败（{}）：{e}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                    |e| {
                        format!(
                            "受控启动入口目录收紧 0700 失败（{}）：{e}",
                            parent.display()
                        )
                    },
                )?;
                validate_resume_env_dir(parent, current_uid())?;
            }
        }
        Err(e) => {
            return Err(format!(
                "受控启动入口目录状态不可读（{}）：{e}",
                parent.display()
            ));
        }
    }
    sweep_stale_resume_env_files(parent);
    // ③ 独占创建（create-new 语义）：O_EXCL 等价 + 0600 一次性设置。Unix 上
    //    open(2) 的 mode 受 umask 只减不加约束（0o600 无组/他者写位，任何常态
    //    umask 都不会放宽它）；AlreadyExists（含文件位被 symlink 占据——O_EXCL
    //    不跟随）一律按冲突拒绝。
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&secret.path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "{ENV_SECRET_COLLISION}: 受控启动入口 env 文件已存在（{}）——拒绝覆盖\
                         他者文件，放弃本次交付（重试会生成新随机名）",
                        secret.path.display()
                    )
                } else {
                    format!(
                        "受控启动入口 env 文件创建失败（{}）：{e}",
                        secret.path.display()
                    )
                }
            })?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&secret.path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "{ENV_SECRET_COLLISION}: 受控启动入口 env 文件已存在（{}）——拒绝覆盖\
                     他者文件，放弃本次交付（重试会生成新随机名）",
                    secret.path.display()
                )
            } else {
                format!(
                    "受控启动入口 env 文件创建失败（{}）：{e}",
                    secret.path.display()
                )
            }
        })?;
    // ④ 权限/形态核验（防 umask 异常与竞态替换；异常即清理拒绝）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&secret.path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&secret.path);
            return Err(format!(
                "受控启动入口 env 文件收紧 0600 失败（{}，半成品已清理）：{e}",
                secret.path.display()
            ));
        }
        let meta = match std::fs::symlink_metadata(&secret.path) {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_file(&secret.path);
                return Err(format!(
                    "受控启动入口 env 文件回读失败（{}，半成品已清理）：{e}",
                    secret.path.display()
                ));
            }
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            let _ = std::fs::remove_file(&secret.path);
            return Err(format!(
                "{ENV_SECRET_PATH_UNSAFE}: 受控启动入口 env 文件形态异常（非普通文件，{}，\
                 半成品已清理）——拒绝落盘",
                secret.path.display()
            ));
        }
        if meta.permissions().mode() & 0o777 != 0o600 {
            let _ = std::fs::remove_file(&secret.path);
            return Err(format!(
                "{ENV_SECRET_DIR_UNSAFE}: 受控启动入口 env 文件权限未归零 0600（{:o}，{}，\
                 半成品已清理）——拒绝落盘",
                meta.permissions().mode() & 0o777,
                secret.path.display()
            ));
        }
    }
    // ⑤ 校验全部成立后才写内容；写入失败即清理半成品
    let content = render_env_secret_content(&secret.password);
    if let Err(e) = writer(&mut file, &content) {
        let _ = std::fs::remove_file(&secret.path);
        return Err(format!(
            "受控启动入口 env 文件写入失败，已清理半成品（{}）：{e}",
            secret.path.display()
        ));
    }
    Ok(())
}

/// osascript 交付结果前缀（[`run_osascript_wait`] 产出与 HC 处置分类共享同一
/// 常量——防两处文案漂移导致分类失明）。
const OSASCRIPT_TIMEOUT_PREFIX: &str = "osascript 执行超时";
const OSASCRIPT_WAITFAIL_PREFIX: &str = "osascript 等待失败";

/// I4 聚焦段失败前缀（与 `OSASCRIPT_*` 前缀**同族同源**登记——同一 &'static str
/// 常量族、防生产臂与日志分类两处文案漂移）。聚焦失败是交付成立后的锦上添花损失：
/// 结构化标记只进日志，**永不**参与交付分类（[`is_delivery_unknown_error`] 恒否）
/// 与备用终端决策。
pub(crate) const RESUME_FOCUS_FAIL_PREFIX: &str = "resume 聚焦失败";

/// 聚焦失败结构化标记（纯函数跨平台可测）：`{RESUME_FOCUS_FAIL_PREFIX}（窗口已
/// 交付，忽略）：{e}`——生产臂日志与测试断言共享同一构造（防文案漂移）。
pub(crate) fn focus_failure_marker(e: &str) -> String {
    format!("{RESUME_FOCUS_FAIL_PREFIX}（窗口已交付，忽略）：{e}")
}

/// 交付处置分类（HC H3 交付未知生命周期）：
/// * [`Self::PayloadOwnsDeletion`]：交付 Ok——窗口**异步** source 中，载荷自带
///   `rm -f` source 后即删，交付方不得提前删（提前删会让慢窗口 source 失败）；
/// * [`Self::DeleteNow`]：确定性失败（osascript 没能送达脚本：启动失败/执行失败/
///   TCC 拒绝）——请求未达 Terminal，rm 不会跑，立即删除；
/// * [`Self::RetainForSettle`]：交付未知（超时/等待失败——Apple 事件可能已送达，
///   窗口可能稍后才 source）：文件保留（0600）交观察窗结算钩子删除（restore_pending
///   确认核销/显式放弃即删），24h 清扫兜底（最坏留存 24h）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvSecretDeliveryDisposition {
    PayloadOwnsDeletion,
    DeleteNow,
    RetainForSettle,
}

/// 交付未知分类（HD H4 释放规则共用判据，与 [`disposition_for_osascript_result`]
/// 同源常量——防两处文案漂移导致分类失明）：Err 文案携带超时/等待失败前缀 ⇒
/// Apple 事件可能已送达（交付结果未知）；其余 Err = 确定性失败（请求未达终端）。
pub(crate) fn is_delivery_unknown_error(e: &str) -> bool {
    e.starts_with(OSASCRIPT_TIMEOUT_PREFIX) || e.starts_with(OSASCRIPT_WAITFAIL_PREFIX)
}

fn disposition_for_osascript_result(r: &Result<(), String>) -> EnvSecretDeliveryDisposition {
    match r {
        Ok(()) => EnvSecretDeliveryDisposition::PayloadOwnsDeletion,
        Err(e) if is_delivery_unknown_error(e) => EnvSecretDeliveryDisposition::RetainForSettle,
        Err(_) => EnvSecretDeliveryDisposition::DeleteNow,
    }
}

/// macOS 交付缝（HC H3）：落盘（校验/独占创建，Err = 零起窗短路）→ osascript
/// runner → 按处置分类清理/留存 env 文件。生产 runner = [`run_osascript_wait`]；
/// 测试 = 记录型假体（零真实 osascript——校验失败在 runner 出手前短路，即「零起窗」
/// 的可测形态）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn deliver_macos_with_env_secret(
    script: &str,
    env_secret: Option<&EnvSecret>,
    runner: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(secret) = env_secret {
        materialize_env_secret(secret)?;
    }
    let r = runner(script);
    if let Some(secret) = env_secret {
        match disposition_for_osascript_result(&r) {
            EnvSecretDeliveryDisposition::PayloadOwnsDeletion => {}
            EnvSecretDeliveryDisposition::DeleteNow => {
                let _ = std::fs::remove_file(&secret.path);
            }
            EnvSecretDeliveryDisposition::RetainForSettle => {
                // 交付未知：文件保留（0600），观察窗结算钩子（restore_pending
                // 确认核销/显式放弃）负责删除；24h 清扫兜底——不在此处冒进删
                //（窗口可能迟 source，提前删会让它 source 失败开死窗）
            }
        }
    }
    r
}

/// I4 交付/聚焦分离缝（macOS 生产臂两段信号执行；跨平台可测——runner 全注入，
/// 零真实 osascript）：
/// * **起窗段**（`script`）= 交付信号——经 [`deliver_macos_with_env_secret`] 全链
///   （env 文件落盘/处置 + runner），其结果即交付判定：Ok = 窗口已起（已交付），
///   Err = 明确未交付（原样上抛，下游才允许转备用终端）；
/// * **聚焦段**（`focus_script`）= 锦上添花——仅起窗段 Ok 后 best-effort 执行；
///   失败只记 [`focus_failure_marker`] 结构化日志（窗口已交付，忽略），**永不**
///   回改交付判定、永不触发备用终端。聚焦段零出手面：起窗段 Err 时（交付未成立，
///   聚焦无意义）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn deliver_macos_with_focus(
    script: &str,
    focus_script: Option<&str>,
    env_secret: Option<&EnvSecret>,
    runner: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let delivery = deliver_macos_with_env_secret(script, env_secret, runner);
    if delivery.is_ok() {
        if let Some(focus) = focus_script {
            if let Err(e) = runner(focus) {
                log::warn!("{}", focus_failure_marker(&e));
            }
        }
    }
    delivery
}

/// 各平台的 opencode 可执行文件名候选（Windows 官方安装落 `opencode.exe`，
/// 兼容无扩展名形态；unix 恒 `opencode`）。
fn opencode_bin_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["opencode.exe", "opencode"]
    } else {
        &["opencode"]
    }
}

/// 可执行文件判定：存在 + 是普通文件 + unix 三位执行位任一（Windows：存在即可，
/// CreateProcess 语义）。目录/缺失一律 false。
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 单目录探测结果（规范安装位/显式配置需要区分「缺失」与「存在但不可执行」——
/// 后者是安装面破损，必须诚实上报而不是跳过）。
enum DirHit {
    Miss,
    NotExecutable(std::path::PathBuf),
    Hit(std::path::PathBuf),
}

/// 按 [`opencode_bin_names`] 在目录内找可执行命中（首个存在且可执行的文件名）；
/// 全部不存在 → Miss；存在但均不可执行 → NotExecutable（报首个存在者）。
fn probe_dir(dir: &std::path::Path) -> DirHit {
    let mut not_exec: Option<std::path::PathBuf> = None;
    for name in opencode_bin_names() {
        let p = dir.join(name);
        if is_executable_file(&p) {
            return DirHit::Hit(p);
        }
        if p.exists() && not_exec.is_none() {
            not_exec = Some(p);
        }
    }
    match not_exec {
        Some(p) => DirHit::NotExecutable(p),
        None => DirHit::Miss,
    }
}

/// opencode 起窗载荷（M7）：同一解析产物的两种形态——
/// * [`Self::macos_payload`]：AppleScript/shell 载荷，二进制路径经
///   [`shell_single_quote`] 包裹（空格/单引号路径安全；单引号按 POSIX `'\''` 转义）；
/// * [`Self::windows_payload`]：Windows `cmd /k` 载荷（路径直拼，引号交给既有
///   `cmd_payload_raw` 机制）**兼作 macOS 效果回查特征**——起窗后进程 argv 直拼串
///   （shell 已消费引号，argv[0] = 字面路径），特征必须是不带引号形态才能
///   `resume_effect_in_snapshot` 子串命中（带引号特征恒 miss → 死窗假阴性）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenCodeResumePayload {
    pub macos_payload: String,
    pub windows_payload: String,
}

/// 载荷构造纯函数（跨平台可测）：`<bin> --session <id>` 直拼 + macOS 单引号形态。
/// 会话 id 沿用既有信任口径：一律取自本机会话快照/登记行命中项（见模块 doc
/// 注入面说明），不回显远端输入。
pub(crate) fn build_opencode_resume_payload(bin: &str, session_id: &str) -> OpenCodeResumePayload {
    OpenCodeResumePayload {
        macos_payload: format!("{} --session {}", shell_single_quote(bin), session_id),
        windows_payload: format!("{bin} --session {session_id}"),
    }
}

/// F4（B1 恢复 · Ruling 4）显式连接形态载荷：目标 = **已有外部 serve**（登记发现
/// 分支在场）——新 TUI 用 `--server <endpoint>` 连接同一 serve（不自托管、零新起
/// 实名 serve，全局实名 serve 计数不变）。OPENCODE_PASSWORD **不经命令文本**（G2
/// 收口：命令文本/osascript 参数/argv/日志/回执五媒介零凭据明文）——改经受控本地
/// 启动入口：0600 env 文件（[`EnvSecret`]）+ 载荷内 `source` 即删。显式形态客户端
/// 凭据证据面不变（discovery.rs `discover_explicit_client`：argv `--server` + 进程
/// 环境 `OPENCODE_PASSWORD`，两者必须同源齐备——source 把 OPENCODE_PASSWORD 注入
/// 目标 shell 环境，子进程 env 照常命中）。
///
/// * [`Self::windows_payload`]（兼作效果回查特征）：`<bin> --server <endpoint>
///   --session <id>` 直拼——本来就不含凭据（Windows 通道 env_secret 不消费，
///   显式恢复的密码注入面自 F4 起即 macOS 专属）；
/// * [`Self::macos_payload`]：`source '<envfile>' && rm -f '<envfile>' && env
///   [HOME='<home>'] [XDG_STATE_HOME='<state>'] [XDG_CONFIG_HOME='<config>']
///   '<bin>' --server '<endpoint>' --session <id>`——Terminal.app `do script` 的
///   shell **不继承兔维斯 进程环境**（独立 GUI 应用），目标环境的 HOME/XDG 必须显式
///   随载荷携带（插件/引导登记落到候选数据根）；endpoint/HOME/XDG/路径全部经
///   [`shell_single_quote`] 单引号包裹，POSIX 转义零回退。
///
/// env 文件生命周期（G2 申报面）：`source` 成功后 `rm -f` 即删（密码入 shell 环境
/// 后文件即刻消失）；source 失败（文件损坏）→ `&&` 链短路，rm 不执行、bin 不起
/// （fail-closed，文件残留 ≤1 个且 0600）；窗口在 rm 前被用户关闭 → 同形态残留。
/// 交付时刻才落盘（[`materialize_env_secret`]），预备/构造路径零落盘。
pub(crate) fn build_opencode_explicit_resume_payload(
    bin: &str,
    session_id: &str,
    endpoint: &str,
    secret: &EnvSecret,
    home: Option<&std::path::Path>,
    xdg_state_home: Option<&std::path::Path>,
    xdg_config_home: Option<&std::path::Path>,
) -> OpenCodeResumePayload {
    let mut env_pairs: Vec<String> = Vec::new();
    if let Some(home) = home {
        env_pairs.push(format!(
            "HOME={}",
            shell_single_quote(&home.to_string_lossy())
        ));
    }
    if let Some(state) = xdg_state_home {
        env_pairs.push(format!(
            "XDG_STATE_HOME={}",
            shell_single_quote(&state.to_string_lossy())
        ));
    }
    if let Some(config) = xdg_config_home {
        env_pairs.push(format!(
            "XDG_CONFIG_HOME={}",
            shell_single_quote(&config.to_string_lossy())
        ));
    }
    let env_prefix = if env_pairs.is_empty() {
        String::new()
    } else {
        format!("env {} ", env_pairs.join(" "))
    };
    OpenCodeResumePayload {
        macos_payload: format!(
            "source {} && rm -f {} && {}{} --server {} --session {}",
            shell_single_quote(&secret.path.to_string_lossy()),
            shell_single_quote(&secret.path.to_string_lossy()),
            env_prefix,
            shell_single_quote(bin),
            shell_single_quote(endpoint),
            session_id
        ),
        windows_payload: format!("{bin} --server {endpoint} --session {session_id}"),
    }
}

/// 二进制解析（注入式：home/PATH/显式配置三参全注入，测试零接触真实环境；
/// 生产装配 [`prepare_opencode_resume`] 读进程环境）。
///
/// 优先级（brief 定案「自查并论证」）：
/// ① **规范安装位** `~/.opencode/bin/opencode`——与登记/托管服务同源（N6 开轮实测
///    真实 `~/.opencode/bin/opencode` SHA = H02 锚 `abe4d885…`），是版本门的天然
///    锚面；**存在即权威候选**：存在但不可执行 = 安装面破损，结构化失败且**不回落
///    PATH**（静默回落 Homebrew 1.19.4 正是 N6 E1 要堵的死法）；
/// ② **显式配置** `OPENCODE_BINARY`（优先级 1 缺席时救援非标安装）；配置了但缺失/
///    不可执行 = 配置错误，结构化失败且**不静默回落 PATH**（掩盖用户指认面）；
/// ③ **PATH 回退**（前两者全缺席）：逐目录首个可执行命中（shell 解析语义同构，
///    不可执行条目跳过与 shell 一致）；无论命中与否，都要过版本门——PATH 只可能是
///    任意来源的二进制（Homebrew/npm/手工），版本门是它的唯一准入凭据。
///
/// 三路全缺席 → [`OPENCODE_BINARY_MISSING`]（错误正文披露搜索面）。
pub(crate) fn resolve_opencode_binary_in(
    home: Option<&std::path::Path>,
    path_env: Option<&str>,
    env_override: Option<&std::ffi::OsStr>,
) -> Result<String, String> {
    // ① 规范安装位
    if let Some(home) = home {
        match probe_dir(&home.join(".opencode").join("bin")) {
            DirHit::Hit(p) => return finalize_candidate(p),
            DirHit::NotExecutable(p) => {
                return Err(format!(
                    "{OPENCODE_BINARY_NOT_EXECUTABLE}: 规范安装位的 opencode 不可执行：{}（权威候选存在但缺执行位——请修复权限；为防版本错配不回落 PATH）",
                    p.display()
                ));
            }
            DirHit::Miss => {}
        }
    }
    // ② 显式配置
    if let Some(raw) = env_override {
        let p = std::path::PathBuf::from(raw);
        return if is_executable_file(&p) {
            finalize_candidate(p)
        } else if p.exists() {
            Err(format!(
                "{OPENCODE_BINARY_NOT_EXECUTABLE}: {OPENCODE_BINARY_ENV}={} 指向的文件不可执行（请修复权限或改指可执行文件）",
                p.display()
            ))
        } else {
            Err(format!(
                "{OPENCODE_BINARY_MISSING}: 显式配置 {OPENCODE_BINARY_ENV}={} 指向的文件不存在（请修正指认；为防版本错配不静默改走 PATH）",
                p.display()
            ))
        };
    }
    // ③ PATH 回退（分隔符：windows ';' 其余 ':'；空段跳过与 shell 一致）
    let sep = if cfg!(windows) { ';' } else { ':' };
    let path_raw = path_env.unwrap_or("");
    let mut searched = 0usize;
    for dir in path_raw.split(sep).map(str::trim).filter(|d| !d.is_empty()) {
        searched += 1;
        if let DirHit::Hit(p) = probe_dir(std::path::Path::new(dir)) {
            return finalize_candidate(p);
        }
    }
    Err(format!(
        "{OPENCODE_BINARY_MISSING}: 未找到 opencode 可执行文件（搜索面：规范安装位 ~/.opencode/bin、{OPENCODE_BINARY_ENV} 显式配置、PATH 共 {searched} 个目录）——请安装 opencode 或设置 {OPENCODE_BINARY_ENV}"
    ))
}

/// 候选定案前校验：必须绝对路径（载荷以绝对路径起窗，cwd 是项目目录——相对路径
/// 会在起窗时按项目目录解析，指认漂移）。
fn finalize_candidate(p: std::path::PathBuf) -> Result<String, String> {
    if !p.is_absolute() {
        return Err(format!(
            "{OPENCODE_BINARY_MISSING}: 解析到的 opencode 路径不是绝对路径：{}（PATH 项与 {OPENCODE_BINARY_ENV} 必须为绝对路径）",
            p.display()
        ));
    }
    Ok(p.to_string_lossy().into_owned())
}

/// 版本输出解析（纯函数）：stdout 首行首个含 ASCII 数字 token，剥 v/V 前缀后必须
/// 以数字开头（`opencode v2.2.5` → `2.2.5`；`dev` 类无数字/`dev2` 剥后非数字开头
/// → None，fail-closed）。approve.rs probe_cli_version 同款 token 法 + M7 收紧。
pub(crate) fn parse_version_output(first_line: &str) -> Option<String> {
    first_line
        .split_whitespace()
        .find(|t| t.chars().any(|c| c.is_ascii_digit()))
        .map(|t| t.trim_start_matches(['v', 'V']).to_string())
        .filter(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// `--version` 探测（有界 [`OPENCODE_VERSION_PROBE_TIMEOUT_SECS`]）：
/// spawn 失败 / 超时（kill+收尸）/ 非零退出 / 输出不可解析 → 一律
/// [`OPENCODE_VERSION_PROBE_FAILED`] 结构化失败（fail-closed——探测不到 = 门不过，
/// 绝不无凭据放行）。stdout 管道在拿到退出态后才读（`--version` 输出远小于管道
/// 缓冲，先等后读无死锁风险；超长输出的极端披露与 approve.rs probe_cli_version
/// 同源：wait 时被写满会假性超时，结果有界非死锁）。
pub(crate) fn probe_opencode_version(bin: &str) -> Result<String, String> {
    use std::io::Read;
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("{OPENCODE_VERSION_PROBE_FAILED}: 无法启动 --version 探测（{bin}）：{e}")
        })?;
    let wait = child.wait_timeout(std::time::Duration::from_secs(
        OPENCODE_VERSION_PROBE_TIMEOUT_SECS,
    ));
    match wait {
        Ok(Some(status)) => {
            let mut raw = Vec::new();
            if let Some(pipe) = child.stdout.as_mut() {
                let _ = pipe.read_to_end(&mut raw);
            }
            let stdout = String::from_utf8_lossy(&raw);
            if !status.success() {
                return Err(format!(
                    "{OPENCODE_VERSION_PROBE_FAILED}: --version 非零退出（码 {}，{bin}）{}",
                    status.code().unwrap_or(-1),
                    crate::inject::normalize::summarize(stdout.trim(), RESUME_STDERR_SUMMARY_CHARS)
                ));
            }
            let first_line = stdout.lines().next().unwrap_or("");
            parse_version_output(first_line).ok_or_else(|| {
                format!(
                    "{OPENCODE_VERSION_PROBE_FAILED}: --version 输出不可解析（{bin}）：{}",
                    crate::inject::normalize::summarize(first_line, RESUME_STDERR_SUMMARY_CHARS)
                )
            })
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!(
                "{OPENCODE_VERSION_PROBE_FAILED}: --version 探测超时（{OPENCODE_VERSION_PROBE_TIMEOUT_SECS}s 上限，已终止；{bin}）"
            ))
        }
        Err(e) => Err(format!(
            "{OPENCODE_VERSION_PROBE_FAILED}: --version 探测等待失败（{bin}）：{e}"
        )),
    }
}

/// opencode 起窗预备门（注入式核心，M7 形态签名保留）：解析 → `--version` 探测 →
/// 版本比对 → 载荷。生产走 [`prepare_opencode_resume`]（进程环境 + XDG 全量）；
/// 本包装（xdg=None）仅测试面继续使用（M7 注入式用例签名零改动）。
///
/// F4（Ruling 4）载荷形态分派：解析/版本门通过后读**登记发现分支**（discovery
/// `state_file_for(home, xdg_state_home)` 同一路径）——登记在场且形状合法 ⇒
/// **显式连接形态**（`--server <登记端点>`，连接已有外部 serve，不自托管）；
/// 登记缺失/不可读 ⇒ 自托管形态（M7 原载荷，零回退）。
#[cfg(test)]
pub(crate) fn prepare_opencode_resume_with(
    home: Option<&std::path::Path>,
    path_env: Option<&str>,
    env_override: Option<&std::ffi::OsStr>,
    session_id: &str,
) -> Result<PreparedResume, String> {
    prepare_opencode_resume_with_sources(
        home,
        None,
        None,
        path_env,
        env_override,
        session_id,
        &resume_sources_production(),
    )
}

/// 预备链依赖束（G2 起 `read_state_file` 之外新增登记进程事实 + 受控启动入口两缝，
/// 收敛为结构体参数）：全缝显式注入，测试零接触真实进程表/真实缓存目录。
pub(crate) struct ResumeSources<'a> {
    /// 登记文件读取（Err = 登记缺失/不可读 ⇒ 自托管形态，fail-closed 不猜端点）。
    pub read_state_file: &'a dyn Fn(&std::path::Path) -> Result<Vec<u8>, String>,
    /// 进程事实（登记 pid 存活性核验；生产 =
    /// `monitor::opencode_observation::production_process_fact`）。
    pub process_fact: &'a dyn Fn(u32) -> Option<i64>,
    /// 受控启动入口秘密生成（生产 = [`env_secret_writer_production`]：只出路径
    /// 零落盘；测试注入 tempdir 路径 + 假凭据，真实密码零复制）。
    pub env_secret_writer: &'a dyn Fn(&str) -> Result<EnvSecret, String>,
}

/// 生产依赖束（预备链装配：真实登记读取 + 真实进程事实 + 生产受控入口）。
pub(crate) fn resume_sources_production() -> ResumeSources<'static> {
    ResumeSources {
        read_state_file: &read_state_file_real,
        process_fact: &|pid| crate::monitor::opencode_observation::production_process_fact(pid),
        env_secret_writer: &env_secret_writer_production,
    }
}

/// 预备链产物（G2 构造/执行分离补全）：载荷两形态 + 受控启动入口秘密（自托管
/// 形态 = None）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedResume {
    pub payload: OpenCodeResumePayload,
    pub env_secret: Option<EnvSecret>,
}

/// 全缝注入版（F4 + G2 三核对前置）：xdg 两参 + 登记读取/进程事实/受控入口三缝
/// 全部显式，测试零接触真实环境。`read_state_file` 的 Err = 登记缺失/不可读 ⇒
/// 自托管形态（不猜端口，与 discovery 登记发现分支的 fail-closed 同义）。
///
/// G2 起窗前三核对（登记显式形态路径，**失败 ⇒ 结构化 Err 零起窗**——不回落
/// 自托管：回落 = 开自托管新窗，恰恰构成「起窗」）：
/// ① 端点必须回环（127.0.0.1/::1/localhost；discovery `is_loopback` 同一判据）；
/// ② 登记进程身份核验：登记 pid 的进程事实可读（pid 活）。登记文件无 lstart
///    字段，「lstart 与登记一致」无锚可比——按主会话裁定的等价分支「登记发现
///    分支命中」实现：形状合法 + 端点回环 + pid 存活 + 进程事实可读，恰为
///    discovery `discover` 全链的前置（同 fail-closed 口径）；
/// ③ 目标会话目录（project_path realpath 化 + is_dir）在恢复流转层前置
///    （restore_pending ④，两形态通用），见该模块。
pub(crate) fn prepare_opencode_resume_with_sources(
    home: Option<&std::path::Path>,
    xdg_state_home: Option<&std::path::Path>,
    xdg_config_home: Option<&std::path::Path>,
    path_env: Option<&str>,
    env_override: Option<&std::ffi::OsStr>,
    session_id: &str,
    sources: &ResumeSources<'_>,
) -> Result<PreparedResume, String> {
    let bin = verified_opencode_resume_binary(home, path_env, env_override)?;
    // 登记发现分支（F4 Ruling 4）：登记在场 → 显式连接已有 serve；缺失 → 自托管。
    if let Some(home) = home {
        let state_file =
            crate::services::opencode_native::discovery::state_file_for(home, xdg_state_home);
        if let Ok(bytes) = (sources.read_state_file)(&state_file) {
            if let Some(reg) =
                crate::services::opencode_native::discovery::parse_registration_summary(&bytes)
            {
                // G2 ①：端点必须回环（非回环登记 = 端点被改写/恶意登记，拒绝起窗）
                let url = reqwest::Url::parse(&reg.endpoint).map_err(|_| {
                    format!(
                        "{OC_RESUME_ENDPOINT_NOT_LOOPBACK}: 登记端点不可解析（{reg_endpoint}），拒绝起窗",
                        reg_endpoint = reg.endpoint
                    )
                })?;
                if !crate::services::opencode_native::discovery::is_loopback(&url) {
                    return Err(format!(
                        "{OC_RESUME_ENDPOINT_NOT_LOOPBACK}: 登记端点非回环（{reg_endpoint}）——\
                         恢复链只允许连接本机回环 serve，拒绝起窗；请核对 opencode 登记文件",
                        reg_endpoint = reg.endpoint
                    ));
                }
                // G2 ②：登记进程身份核验（登记 pid 死/进程事实不可读 ⇒ 旧代际残迹，
                // 拒绝起窗——连接死 serve 只会开死窗）
                if (sources.process_fact)(reg.pid).is_none() {
                    return Err(format!(
                        "{OC_RESUME_REGISTRATION_UNVERIFIED}: 登记进程（pid {}）已不在运行或\
                         启动事实不可读——登记为旧代际残迹，拒绝起窗；请重启 opencode 服务后重试",
                        reg.pid
                    ));
                }
                // G2 密码脱离命令文本：受控启动入口（env 文件路径，交付时刻落盘）
                let secret = (sources.env_secret_writer)(&reg.password)?;
                let payload = build_opencode_explicit_resume_payload(
                    &bin,
                    session_id,
                    &reg.endpoint,
                    &secret,
                    Some(home),
                    xdg_state_home,
                    xdg_config_home,
                );
                return Ok(PreparedResume {
                    payload,
                    env_secret: Some(secret),
                });
            }
        }
    }
    Ok(PreparedResume {
        payload: build_opencode_resume_payload(&bin, session_id),
        env_secret: None,
    })
}

fn verified_opencode_resume_binary(
    home: Option<&std::path::Path>,
    path_env: Option<&str>,
    env_override: Option<&std::ffi::OsStr>,
) -> Result<String, String> {
    let bin = resolve_opencode_binary_in(home, path_env, env_override)?;
    let found = probe_opencode_version(&bin)?;
    if found != OPENCODE_ANCHORED_VERSION {
        return Err(format!(
            "{OPENCODE_VERSION_MISMATCH}: 解析到的 opencode 版本为 {found}，恢复链登记版本为 {OPENCODE_ANCHORED_VERSION}（二进制：{bin}）——版本错配会让恢复链语义失效，已拒绝恢复；请安装规范版本或用 {OPENCODE_BINARY_ENV} 指认登记构建"
        ));
    }
    Ok(bin)
}

/// 创建后的起窗绑定原服务描述符，禁止在构造载荷时重新选择登记或回落自托管。
pub(crate) fn prepare_created_opencode_terminal(
    session: &Session,
    service: &crate::services::opencode_native::types::ServiceDescriptor,
    _directory: &super::create_path::CreateDirectory,
    _registration_file: &std::path::Path,
    _registration_digest: &str,
) -> Result<PreparedSpawn, String> {
    let home = dirs::home_dir();
    let path_env = std::env::var("PATH").ok();
    let env_override = std::env::var_os(OPENCODE_BINARY_ENV);
    let xdg_state = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from);
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    let bin = verified_opencode_resume_binary(
        home.as_deref(),
        path_env.as_deref(),
        env_override.as_deref(),
    )?;
    let secret = env_secret_writer_production(&service.password)?;
    let payload = build_opencode_explicit_resume_payload(
        &bin,
        &session.id,
        service.endpoint.as_str().trim_end_matches('/'),
        &secret,
        home.as_deref(),
        xdg_state.as_deref(),
        xdg_config.as_deref(),
    );
    #[cfg(target_os = "macos")]
    let macos_payload = {
        let started = chrono::DateTime::from_timestamp_millis(service.runtime.started_at_ms)
            .ok_or_else(|| "无法核实 OpenCode 服务的启动时刻".to_owned())?;
        let started = started.format("%a %b %e %T %Y").to_string();
        let started = started.split_whitespace().collect::<Vec<_>>().join(" ");
        // 终端先 cd，再按物理目录身份和原服务代际决定是否启动 CLI。
        format!(
            "test \"$(/usr/bin/stat -f '%d:%i' .)\" = {} && \
             test \"$(LC_ALL=C TZ=UTC /bin/ps -p {} -o lstart= | /usr/bin/xargs)\" = {} && \
             test \"$(/usr/bin/shasum -a 256 < {} | /usr/bin/cut -d ' ' -f1)\" = {} && {}",
            shell_single_quote(&_directory.stat_key()),
            service.runtime.pid,
            shell_single_quote(&started),
            shell_single_quote(&_registration_file.to_string_lossy()),
            shell_single_quote(_registration_digest),
            payload.macos_payload,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let macos_payload = payload.macos_payload;
    Ok(PreparedSpawn {
        effect_sig: payload.windows_payload.clone(),
        macos_payload,
        windows_payload: payload.windows_payload,
        env_secret: Some(secret),
    })
}

/// 生产装配（读进程环境：home = dirs::home_dir——候选隔离轮 HOME 重定向后规范
/// 安装位天然落在隔离数据根；PATH/OPENCODE_BINARY/XDG 同为进程环境）。
pub(crate) fn prepare_opencode_resume(session_id: &str) -> Result<PreparedResume, String> {
    let home = dirs::home_dir();
    let path_env = std::env::var("PATH").ok();
    let env_override = std::env::var_os(OPENCODE_BINARY_ENV);
    let xdg_state = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from);
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    prepare_opencode_resume_with_sources(
        home.as_deref(),
        xdg_state.as_deref(),
        xdg_config.as_deref(),
        path_env.as_deref(),
        env_override.as_deref(),
        session_id,
        &resume_sources_production(),
    )
}

/// 真实文件系统登记读取（生产装配用）：Err = 缺失/不可读 ⇒ 自托管形态。
fn read_state_file_real(path: &std::path::Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("state file unreadable: {e}"))
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

/// 构造 Terminal.app 起窗脚本（macOS 首选通道）——**只承载起窗段信号**（I4 交付/
/// 聚焦分离）：`do script "cd '<cwd>' && <resume>"`（do script 自带回车）。脚本
/// 整体成功 ⇔ `do script` 成功 ⇔ 窗口已起 = **交付信号成立**；聚焦段拆分至
/// [`terminal_focus_script`]（旧形态 activate 内联在起窗脚本里，聚焦失败会让整段
/// 脚本非零退出、被执行层判为未交付而转 iTerm2 开第二窗——聚焦失败不得掩盖交付）。
/// 转义同 [`iterm_open_window_script`]。
pub fn terminal_open_script(cwd: &str, resume: &str) -> String {
    format!(
        r#"tell application "Terminal"
	do script "{payload}"
	return "opened"
end tell
"#,
        payload = macos_command_payload(cwd, resume)
    )
}

/// 构造 Terminal.app 聚焦段脚本（I4 独立信号）：`activate` 置前聚焦——起窗交付
/// 成立后的**锦上添花**段，由生产 spawner best-effort 执行（失败只降级日志、
/// 前缀见 [`RESUME_FOCUS_FAIL_PREFIX`]，永不回改交付判定）。
pub fn terminal_focus_script() -> String {
    r#"tell application "Terminal"
	activate
	return "focused"
end tell
"#
    .to_string()
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
                if stderr.contains("(-1712)") {
                    Err(format!("{OSASCRIPT_TIMEOUT_PREFIX}（Apple Event -1712）"))
                } else {
                    Err(classify_resume_error(&stderr))
                }
            }
        }
        // 超时：杀 + 收尸（防僵尸），失败回执（与 spawn Err 同管道走端点 failed）。
        // 前缀常量与 HC 交付处置分类共享（防文案漂移致分类失明）。
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!(
                "{OSASCRIPT_TIMEOUT_PREFIX}（{OSASCRIPT_WAIT_SECS}s 上限，已终止）——请重试；若持续失败检查 macOS 自动化授权"
            ))
        }
        Err(e) => Err(format!("{OSASCRIPT_WAITFAIL_PREFIX}：{e}")),
    }
}

/// Windows 出手链（跨平台纯逻辑，wt 路径由调用方注入便于测试降级链）：wt 在场
/// 先试；spawn 失败（应用执行别名停用/损坏等场景）**降级重试一次 conhost** 再报
/// failed（两次错误合并披露，cwd 经 current_dir 继承不丢）。G5：`on_delivered`
/// 在场时 spawn Ok = 交付，立即回调（插 pending——Windows 无效果回查，交付即
/// 出手完成；opencode 链的观察窗由恢复状态机接管）。
fn open_windows_with(
    wt: Option<&str>,
    cwd: &str,
    resume: &str,
    spawner: &ScopedSpawnFn<'_>,
    on_delivered: Option<&DeliveryHook<'_>>,
) -> Result<(), String> {
    let mut wt_err: Option<String> = None;
    if let Some(wt) = wt {
        let spec = build_spawn_command_windows(Some(wt), cwd, resume);
        match spawner(&spec) {
            Ok(()) => {
                if let Some(hook) = on_delivered {
                    hook();
                }
                return Ok(());
            }
            Err(e) => {
                log::warn!("wt spawn 失败，降级重试 conhost：{e}");
                wt_err = Some(e);
            }
        }
    }
    let spec = build_spawn_command_windows(None, cwd, resume);
    match spawner(&spec) {
        Ok(()) => {
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        }
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
/// 与主扫描（adapter/mod.rs）同款口径。F4：恢复 pending 观察面（restore_pending）
/// 复用同一快照（真实窗口观察的进程面）。
pub(crate) fn refresh_cmd_snapshot(sys: &mut sysinfo::System) -> Vec<String> {
    refresh_cmd_cwd_snapshot(sys)
        .into_iter()
        .map(|e| e.cmd)
        .collect()
}

/// 命令行 + 工作目录快照条目（G5 迟到观察四要素之「目录」面消费）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CmdCwdEntry {
    #[allow(dead_code)]
    pub pid: u32,
    /// 命令行拼接串（[`refresh_cmd_snapshot`] 同款形态，分隔符归一）。
    pub cmd: String,
    /// 进程工作目录（sysinfo 刷不出 = None → 目录面不命中，fail-closed）。
    pub cwd: Option<std::path::PathBuf>,
}

/// 进程表全量刷新并返回（pid, cmd, cwd）条目（G5 迟到观察面：窗口/会话=cmd 特征、
/// 目录=cwd 核对）。刷新口径 = [`refresh_cmd_snapshot`] 基础上加 `with_cwd`（与
/// inject/create.rs `find_tui_pid`、adapter 主扫描同款——sysinfo 0.39 默认 Kind
/// 不刷 cmd/cwd，必须 specifics 显式开）。
pub(crate) fn refresh_cmd_cwd_snapshot(sys: &mut sysinfo::System) -> Vec<CmdCwdEntry> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .map(|p| CmdCwdEntry {
            pid: p.pid().as_u32(),
            cmd: p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().replace('\\', "/"))
                .collect::<Vec<_>>()
                .join(" "),
            cwd: p.cwd().map(|c| c.to_path_buf()),
        })
        .collect()
}

/// G5 交付回调缝（交付 = osascript 返回 / Windows spawn 返回）：opencode 恢复链
/// 在此刻**立即插入 pending**（观察窗开始前），使交付后任何时刻的复请求都落进
/// 「PendingRefused 拒绝面」——堵 F4 失败轮观察到的重复开窗。None = 无 pending 面
/// （非 opencode 工具 / 旧路径，零行为变更）。生命周期参数跟随调用帧（回调捕获
/// 本轮流转的注册表引用，不要求 'static）。
pub(crate) type DeliveryHook<'a> = dyn Fn() + 'a;

/// macOS 双通道缝出手（F1 治理后：**Terminal.app 优先、iTerm2 次选**——mac-reverify
/// b9a501c §四-A/R1②：iTerm2 3.7.2+macOS26 带命令 create window **恒产 0-tab 死窗**
/// 且 osascript 假成功（exit 0/stderr 空），在 osascript 契约内不可辨；而 Terminal
/// `do script` 同环境实测全通；R1③：上游升级复测后再翻转回 iTerm2 优先）。
/// 每通道 = 组装 [`SpawnSpec::MacosApplescript`] 交 spawner，Ok 后过
/// [`EffectCheckFn`] 效果回查：命中即收口。
///
/// G5（慢起窗不二窗）：`on_delivered = Some` 时，**通道交付成功（osascript 返回
/// Ok）即调回调（插 pending）**；此后效果回查未命中**不再转次选通道**（pending
/// 在场 ⇒ iTerm2 fallback 禁用，绝不自动开第二个窗口），返回 Ok——「已交付未核实」
/// 交恢复状态机按待确认语义收口（迟到观察只核对）。**交付失败分型（HD H4 释放
/// 规则）**：交付未知（osascript 超时/等待失败——Apple 事件可能已送达）⇒ 不允许
/// 备用终端，立即收口 Err（状态机占位保持转待确认）；确定性失败（请求未达
/// Terminal）仍按既有语义转次选（零交付，fallback 保留）。
/// `on_delivered = None`（非 opencode）保持 F1 既有全量语义：交付后回查未命中
/// 转次选，双败合并错误。
/// 双通道皆败（spawn Err 或 legacy 回查未命中）合并中文错误（通道名标注进账——
/// 端点 Err 臂 → 200 failed 回执 + 审计 `open failed:*`，把「死窗」纳入账实一致）。
/// M7：脚本载荷与回查特征**分体**（`resume` = shell 形态带引号；`effect_sig` =
/// argv 直拼形态不带引号）——shell 消费引号后进程 argv[0] 是字面路径，带引号
/// 特征对进程快照恒 miss（死窗假阴性）；非 opencode 工具两者同值（零行为变更）。
fn open_macos_with(
    cwd: &str,
    resume: &str,
    effect_sig: &str,
    spawner: &ScopedSpawnFn<'_>,
    effect_check: &EffectCheckFn,
    env_secret: Option<&EnvSecret>,
    on_delivered: Option<&DeliveryHook<'_>>,
) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    // I4 交付/聚焦分离：Terminal 通道 = 起窗段（交付信号）+ 独立聚焦段（best-effort，
    // 由生产 spawner 在起窗 Ok 后执行——聚焦失败不回改交付判定）；iTerm2 通道不拆
    // （activate 先于 create window 是其交付语义一部分，且末通道无 fallback 误触发面）。
    for (name, script, focus) in [
        (
            "Terminal.app",
            terminal_open_script(cwd, resume),
            Some(terminal_focus_script()),
        ),
        ("iTerm2", iterm_open_window_script(cwd, resume), None),
    ] {
        match spawner(&SpawnSpec::MacosApplescript {
            script,
            focus_script: focus,
            env_secret: env_secret.cloned(),
        }) {
            Ok(()) => {
                // G5：交付即回调（opencode 链 = 插 pending，观察窗开始前在场）
                if let Some(hook) = on_delivered {
                    hook();
                }
                if effect_check(effect_sig) {
                    return Ok(());
                }
                if on_delivered.is_some() {
                    // G5：已交付 + 回查未命中 ⇒ pending 在场，**不转次选**（不二窗）；
                    // Ok 交恢复状态机走观察窗/待确认语义（迟到观察只核对）
                    log::warn!(
                        "resume {name} 交付成功但窗口观察未命中——待确认态保留（G5 不二窗：不转次选通道）"
                    );
                    return Ok(());
                }
                log::warn!("resume {name} 出手后回查未命中（死窗/会话未建立），转次选通道");
                errs.push(format!("{name} 出手后回查未命中（死窗/会话未建立）"));
            }
            Err(e) => {
                // HD（H4 释放规则）：**交付未知**（osascript 超时/等待失败——Apple
                // 事件可能已送达）**不允许尝试备用终端**（不二窗），立即收口交恢
                // 复状态机按「占位保持→转待确认只核对」处置；仅确定性失败（请求
                // 未达终端）保留既有降级次选语义。
                if is_delivery_unknown_error(&e)
                    || super::restore_pending::created_target_error_code(&e).is_some()
                {
                    return Err(e);
                }
                errs.push(format!("{name}: {e}"));
            }
        }
    }
    Err(format!(
        "RESUME_ALL_CHANNELS_FAIL: 全部 resume 通道失败：{}",
        errs.join("；")
    ))
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
            .map_err(|e| format!("RESUME_SPAWN_FAIL: 终端启动失败（{program}）：{e}"))
        }
        #[cfg(not(windows))]
        SpawnSpec::Windows { program, .. } => Err(format!(
            "RESUME_SPAWN_UNSUPPORTED: 当前平台不支持终端 spawn（{program}）"
        )),
        #[cfg(target_os = "macos")]
        SpawnSpec::MacosApplescript {
            script,
            focus_script,
            env_secret,
        } => {
            // G2 受控启动入口（HC 全生命周期加固）：落盘 = 目录校验 + 独占创建
            // 0600 + 写失败清理（Err = 结构化失败，osascript 不出手零起窗——宁可
            // 不恢复也不把密码泄进命令文本）→ osascript → 按交付处置收口 env
            // 文件：Ok = 载荷 `rm -f` source 后自删（交付方不提前删）；确定性失败
            // （TCC 拒绝/启动失败/执行失败）= 立即删；交付未知（超时/等待失败）
            // = 保留至观察窗结算（restore_pending 结算钩子）+ 24h 清扫兜底。
            // I4：起窗 Ok 后聚焦段 best-effort（失败只降级日志——交付/聚焦分离）。
            deliver_macos_with_focus(
                script,
                focus_script.as_deref(),
                env_secret.as_ref(),
                &run_osascript_wait,
            )
        }
        #[cfg(not(target_os = "macos"))]
        SpawnSpec::MacosApplescript { .. } => {
            Err("AppleScript 执行层仅在 macOS 构建装配".to_string())
        }
    }
}

/// 起窗预备产物（F4 构造/执行分离）：载荷三形态（macOS shell / Windows cmd /
/// 效果回查特征）在**出手前**一次性备齐——pending 状态机（restore_pending）据此
/// 在出手前拿到观察特征，出手后无需再构造。G2：受控启动入口秘密随载荷同源携带
/// （自托管形态 None；Debug 脱敏由 [`EnvSecret`] 承载——密码不随调试输出外泄）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedSpawn {
    pub macos_payload: String,
    pub windows_payload: String,
    pub effect_sig: String,
    pub env_secret: Option<EnvSecret>,
}

/// 起窗预备（纯构造，零副作用）：命令表解析 → cwd 预检 → opencode 预备门
/// （M7 二进制解析 + 版本门 + F4 登记发现分支形态分派 + G2 三核对/受控入口）。
/// 错误契约：哨兵串 `"no_resume_command"` / `"no_cwd"` / G2 三核对哨兵
/// （远端端点映射 404；前端按钮同因禁用），其余为人类可读失败文案（spawner 未出手）。
pub(crate) fn prepare_session_terminal(session: &Session) -> Result<PreparedSpawn, String> {
    let tool = session.agent_type.tool_id();
    // 未查证/未入表工具：出手前拦截（spawner 不被调用）
    let Some(resume) = resume_command(tool, &session.id) else {
        return Err("no_resume_command".to_string());
    };
    let cwd = session.project_path.trim();
    if cwd.is_empty() {
        return Err("no_cwd".to_string());
    }
    // M7（B1）：opencode 专属预备门——解析规范二进制（规范安装位 > OPENCODE_BINARY
    // > PATH 带版本门）+ `--version` 有界探测 + 登记版本比对，产物为绝对路径载荷
    // （裸 PATH 相对名退役，Homebrew 1.19.4 错配即此堵死）。门失败 = 结构化诚实
    // 失败（spawner 不出手）。F4：登记发现分支在场 ⇒ 显式连接形态（--server 连接
    // 已有外部 serve）；缺失 ⇒ 自托管形态（M7 原载荷）。G2：登记显式形态前置
    // 端点回环 + 登记进程身份两核对，凭据经受控启动入口（env 文件）交付。
    // 其余工具零行为变更。
    let (macos_payload, windows_payload, env_secret) = if tool == "opencode" {
        let p = prepare_opencode_resume(&session.id)?;
        (
            p.payload.macos_payload,
            p.payload.windows_payload,
            p.env_secret,
        )
    } else {
        (resume.clone(), resume, None)
    };
    Ok(PreparedSpawn {
        effect_sig: windows_payload.clone(),
        macos_payload,
        windows_payload,
        env_secret,
    })
}

/// 起窗执行（F4 从 [`open_session_terminal_with`] 拆出的平台分派半程）：
/// Windows 出手序（评审 M1）：wt 探测命中 → 先试 wt；spawn 失败（应用执行别名
/// 停用/损坏等场景）**降级重试一次 conhost** 再报 failed（两次错误合并披露，
/// cwd 经 current_dir 继承不丢）。
pub(crate) fn execute_prepared_spawn(
    prepared: &PreparedSpawn,
    cwd: &str,
    spawner: &SpawnFn,
) -> Result<(), String> {
    execute_prepared_spawn_with_delivery(prepared, cwd, spawner, None)
}

/// 起窗执行 + G5 交付回调版：`on_delivered = Some`（opencode 恢复链）时，通道
/// 交付成功即回调（插 pending，观察窗开始前在场）+ 交付后回查未命中**不转次选**
/// （不二窗）；None = 既有语义（非 opencode 零行为变更）。
pub(crate) fn execute_prepared_spawn_with_delivery(
    prepared: &PreparedSpawn,
    cwd: &str,
    spawner: &ScopedSpawnFn<'_>,
    on_delivered: Option<&DeliveryHook<'_>>,
) -> Result<(), String> {
    match std::env::consts::OS {
        // Windows：wt 在场先试、败降级 conhost 一次（open_windows_with 跨平台可测）
        "windows" => open_windows_with(
            windows_terminal_path().as_deref(),
            cwd,
            &prepared.windows_payload,
            spawner,
            on_delivered,
        ),
        // macOS：Terminal.app 优先、iTerm2 次选（F1 治理，R1②/R1③）+ 出手后
        // 效果回查（死窗→转次选；双败 failed 回执+审计；G5：交付后未命中不转
        // 次选——见 open_macos_with）。open_macos_with 跨平台可测（回查缝注入
        // 假体）；实机验证归 Mac 回传清单
        "macos" => open_macos_with(
            cwd,
            &prepared.macos_payload,
            &prepared.effect_sig,
            spawner,
            &macos_effect_probe,
            prepared.env_secret.as_ref(),
            on_delivered,
        ),
        _ => Err("RESUME_UNSUPPORTED: 当前平台不支持一键恢复会话".to_string()),
    }
}

/// 核心入口（spawner 缝版本）：预备（构造）→ 平台分派（执行）。
pub fn open_session_terminal_with(session: &Session, spawner: &SpawnFn) -> Result<(), String> {
    let prepared = prepare_session_terminal(session)?;
    execute_prepared_spawn(&prepared, session.project_path.trim(), spawner)
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
            unread_turned_green_at_ms: None,
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
    /// macOS 构造层（跨平台可测）：cd '<cwd>' && <resume> 进脚本 + 置前聚焦；
    /// 转义路径——反斜杠/双引号经 applescript_escape，内嵌单引号经 POSIX '\'' 转义。
    /// I4 起 Terminal 起窗脚本不再内联 activate（聚焦段独立，见
    /// [`i4_terminal_window_script_has_no_inline_focus`]）；iTerm2 保持原样。
    #[test]
    fn macos_scripts_carry_cd_focus_and_escaping() {
        let s = iterm_open_window_script("/tmp/proj", "claude --resume abc");
        assert!(s.contains("create window with default profile command"));
        assert!(s.contains(r#"cd '/tmp/proj' && claude --resume abc"#));
        assert!(s.contains("activate"), "完成后置前聚焦");

        let t = terminal_open_script("/tmp/proj", "claude --resume abc");
        assert!(t.contains(r#"do script "cd '/tmp/proj' && claude --resume abc""#));
        assert!(
            !t.contains("activate"),
            "I4：Terminal 起窗段不内联聚焦（交付/聚焦分离）"
        );
        let f = terminal_focus_script();
        assert!(f.contains("activate"), "聚焦段独立脚本承载 activate");

        // 双引号/反斜杠经 applescript_escape（C:\wei"rd → C:\\wei\"rd），字面量不破
        let e = terminal_open_script(r#"C:\wei"rd"#, "claude --resume abc");
        assert!(e.contains(r#"cd 'C:\\wei\"rd'"#));
        // 内嵌单引号：先 shell '\'' 转义、后 applescript_escape 把该反斜杠再翻倍
        // （\\）——AppleScript 字面量解一转义后 shell 收到的仍是 `'\''`，语义正确
        let q = iterm_open_window_script("/tmp/it's", "claude --resume abc");
        assert!(q.contains(r#"cd '/tmp/it'\\''s'"#));
    }

    // ==== I4：起窗交付与聚焦分离 ====

    /// RED e（brief I4 e）脚本层契约：Terminal **起窗脚本只承载起窗段信号**
    /// （`do script` 成功 = 窗口已起 = 交付）——不得再内联 `activate` 聚焦段：
    /// 旧形态聚焦失败会让整段脚本非零退出、被执行层判为未交付而转 iTerm2 开
    /// 第二窗（聚焦段必须拆分为独立信号，聚焦失败不得掩盖交付）。
    #[test]
    fn i4_terminal_window_script_has_no_inline_focus() {
        let t = terminal_open_script("/tmp/proj", "claude --resume abc");
        assert!(
            t.contains(r#"do script "cd '/tmp/proj' && claude --resume abc""#),
            "起窗段信号必须在场：{t}"
        );
        assert!(
            !t.contains("activate"),
            "起窗脚本不得内联聚焦段（I4 交付/聚焦分离——聚焦失败不得判定为未交付）：{t}"
        );
    }

    /// RED e（brief I4 e）聚焦段独立信号：`terminal_focus_script` 承载 activate
    /// （聚焦段唯一载体——交付信号不再被聚焦失败污染，两段信号互不掺带）。
    #[test]
    fn i4_focus_script_is_independent_signal() {
        let f = terminal_focus_script();
        assert!(f.contains("activate"), "聚焦段承载 activate：{f}");
        assert!(f.contains("Terminal"), "聚焦段指向 Terminal.app：{f}");
        assert!(
            !f.contains("do script"),
            "聚焦段不得携带起窗段（两段信号分离）：{f}"
        );
    }

    /// RED e（brief I4 e）交付缝：起窗段 Ok + 聚焦段 Err ⇒ 整体 Ok（已交付——
    /// 聚焦失败永不改变交付判定）；起窗段 Err ⇒ 明确未交付（Err 原样上抛、聚焦
    /// 段零出手——只有明确未交付才允许下游转备用终端）。
    #[test]
    fn i4_focus_failure_does_not_mask_delivery_and_window_failure_stops_focus() {
        let run = |window_result: Result<(), String>| {
            let calls: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let calls2 = calls.clone();
            let runner = move |s: &str| -> Result<(), String> {
                calls2.lock().unwrap().push(s.to_string());
                if s.contains("do script") {
                    window_result.clone()
                } else {
                    Err("osascript 执行失败（退出码 1）".to_string())
                }
            };
            let r = deliver_macos_with_focus(
                &terminal_open_script("/tmp/proj", "x --session s1"),
                Some(&terminal_focus_script()),
                None,
                &runner,
            );
            // 尾表达式借临时 MutexGuard 会活过局部变量丢弃（E0597）——先落局部
            let recorded = calls.lock().unwrap().clone();
            (r, recorded)
        };
        // ① 起窗 Ok + 聚焦 Err ⇒ Ok（已交付），聚焦段真实出手过
        let (r, calls) = run(Ok(()));
        assert_eq!(r, Ok(()), "聚焦失败不得判定为未交付（已交付）");
        assert_eq!(calls.len(), 2, "起窗段 + 聚焦段各一次 osascript");
        assert!(calls[1].contains("activate"), "第二段是聚焦脚本：{calls:?}");
        // ② 起窗 Err（明确未交付）⇒ Err 上抛、聚焦段零出手
        let (r2, calls2) = run(Err("osascript 执行失败（退出码 1）".to_string()));
        assert!(r2.is_err(), "明确未交付必须 Err（才允许下游转备用终端）");
        assert_eq!(calls2.len(), 1, "起窗失败：聚焦段零出手：{calls2:?}");
    }

    /// RED e 前缀族对齐（主会话裁定 4）：聚焦失败标记入 `OSASCRIPT_*` 同族前缀
    /// 常量（同源 &'static str 常量族、防两处文案漂移），且**永不**被判为交付
    /// 未知——聚焦是交付后的锦上添花，不参与交付分类/备用终端决策。
    #[test]
    fn i4_focus_failure_marker_in_prefix_family_never_delivery_unknown() {
        let m = focus_failure_marker("osascript 执行失败（退出码 1）");
        assert!(m.starts_with(RESUME_FOCUS_FAIL_PREFIX), "结构化标记：{m}");
        let family: [&'static str; 3] = [
            OSASCRIPT_TIMEOUT_PREFIX,
            OSASCRIPT_WAITFAIL_PREFIX,
            RESUME_FOCUS_FAIL_PREFIX,
        ];
        assert!(family.iter().all(|p| !p.is_empty()), "前缀族同形非空");
        assert!(
            !is_delivery_unknown_error(&m),
            "聚焦失败永不参与交付未知分类：{m}"
        );
    }

    /// RED e 出手层面：Terminal spec 携带拆分后的聚焦段（iTerm2 不带——末通道
    /// 无 fallback 误触发面，保持原样零行为变更）；交付信号成立 ⇒ 单窗交付 +
    /// 交付回调恰一次（pending 按交付流转的触发点）。
    #[test]
    fn i4_delivered_signal_with_focus_failure_keeps_single_window() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<SpawnSpec>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let spawner = move |spec: &SpawnSpec| {
            calls2.lock().unwrap().push(spec.clone());
            Ok(())
        };
        let deliveries: std::sync::Arc<std::sync::Mutex<usize>> =
            std::sync::Arc::new(std::sync::Mutex::new(0));
        let deliveries2 = deliveries.clone();
        let hook = move || {
            *deliveries2.lock().unwrap() += 1;
        };
        assert_eq!(
            open_macos_with(
                "/tmp/proj",
                "claude --resume abc",
                "claude --resume abc",
                &spawner,
                &|_| false,
                None,
                Some(&hook)
            ),
            Ok(()),
            "交付信号成立（聚焦段失败由生产臂降级）⇒ Ok"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "交付信号成立：单窗（无 iTerm2 第二出手）"
        );
        let SpawnSpec::MacosApplescript {
            script,
            focus_script,
            ..
        } = &recorded[0]
        else {
            panic!("macOS 缝必须收 AppleScript 变体");
        };
        assert!(
            script.contains("do script"),
            "首通道 Terminal 起窗段：{script}"
        );
        let focus = focus_script
            .as_ref()
            .expect("Terminal 通道必须携带聚焦段信号");
        assert!(focus.contains("activate"), "聚焦段承载 activate：{focus}");
        assert_eq!(
            *deliveries.lock().unwrap(),
            1,
            "交付回调恰一次（交付 = pending 插入触发点）"
        );
    }

    /// RED f（brief I4 f）既有回归（精确化到交付信号层）：真正未交付（起窗段
    /// 失败 = 明确未交付）⇒ 才允许备用终端——iTerm2 次选出手；spec 形态逐通道
    /// 断言（Terminal 带聚焦段、iTerm2 不带）。
    #[test]
    fn i4_definite_non_delivery_falls_back_to_backup_terminal() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<SpawnSpec>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let spawner = move |spec: &SpawnSpec| {
            calls2.lock().unwrap().push(spec.clone());
            Err("osascript 执行失败（退出码 1）".to_string())
        };
        let r = open_macos_with(
            "/tmp/proj",
            "claude --resume abc",
            "claude --resume abc",
            &spawner,
            &|_| true,
            None,
            None,
        );
        assert!(r.is_err(), "明确未交付：双通道皆败 Err");
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "明确未交付才降级次选（恰两通道）");
        let SpawnSpec::MacosApplescript {
            script: first,
            focus_script: first_focus,
            ..
        } = &recorded[0]
        else {
            panic!("首通道 AppleScript 变体");
        };
        assert!(first.contains("do script"), "首通道 Terminal 起窗段");
        assert!(
            first_focus.is_some(),
            "Terminal 通道携带聚焦段（交付/聚焦分离后的 spec 形态）"
        );
        let SpawnSpec::MacosApplescript {
            script: second,
            focus_script: second_focus,
            ..
        } = &recorded[1]
        else {
            panic!("次通道 AppleScript 变体");
        };
        assert!(
            second.contains("create window"),
            "次通道 iTerm2（既有语义零行为变更）"
        );
        assert!(
            second_focus.is_none(),
            "iTerm2 不拆聚焦段（activate 先于 create 是其交付语义一部分；末通道无 fallback 误触发面）"
        );
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
                &spawner,
                None
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
            open_windows_with(None, "/tmp/proj", "claude --resume abc", &spawner2, None),
            Ok(())
        );
        assert_eq!(*calls2.lock().unwrap(), 1, "无 wt 恰一次出手");
    }

    /// F1 治理后 macOS 双通道入缝（跨平台可测）：**Terminal.app 优先**（R1②——
    /// iTerm2 带命令 create window 恒死窗假成功）；首通道败（spawn Err）→ iTerm2
    /// 次选（G5 后：交付后回查未命中不再转次选，spawn Err 交付未发生仍转）；spec
    /// 恒 MacosApplescript 变体且携带 cd '<cwd>' && <resume> 载荷；交付回调在
    /// 交付成功通道恰一次（spawn Err 零回调）。
    #[test]
    fn macos_dual_channel_dispatches_via_seam() {
        // ① 首通道（Terminal.app）成功 + 回查命中：恰一次出手，script 为 do script 形态；
        //   交付回调恰一次（G5：交付即回调）
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let deliveries = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Ok(())
            }
        };
        let deliveries_hook = deliveries.clone();
        let hook = move || {
            *deliveries_hook.lock().unwrap() += 1;
        };
        assert_eq!(
            open_macos_with(
                "/tmp/proj",
                "claude --resume abc",
                "claude --resume abc",
                &spawner,
                &|_| true,
                None,
                Some(&hook)
            ),
            Ok(())
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "首通道成功恰一次出手");
        assert_eq!(
            *deliveries.lock().unwrap(),
            1,
            "交付成功 ⇒ 交付回调恰一次（G5 交付即插 pending 的缝面）"
        );
        let SpawnSpec::MacosApplescript {
            script,
            focus_script,
            ..
        } = &recorded[0]
        else {
            panic!("macOS 缝必须收 AppleScript 变体");
        };
        assert!(script.contains("cd '/tmp/proj' && claude --resume abc"));
        assert!(
            !script.contains("activate"),
            "I4：起窗段不内联聚焦（聚焦段独立信号）"
        );
        assert!(
            focus_script
                .as_ref()
                .is_some_and(|f| f.contains("activate")),
            "聚焦段由 spec 独立携带（交付/聚焦分离）"
        );
        assert!(
            script.contains(r#"do script "cd '/tmp/proj'"#),
            "首通道是 Terminal.app 形态（F1：Terminal 优先）"
        );

        // ② 首通道 spawn 失败：降级 iTerm2 次选（第二次出手 create window 形态）；
        //    零交付 ⇒ 零回调（交付未发生）
        let calls2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner2 = {
            let calls2 = calls2.clone();
            move |spec: &SpawnSpec| {
                calls2.lock().unwrap().push(spec.clone());
                Err("osascript 失败".to_string())
            }
        };
        let deliveries2 = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let deliveries2_hook = deliveries2.clone();
        let hook2 = move || {
            *deliveries2_hook.lock().unwrap() += 1;
        };
        let _ = open_macos_with(
            "/tmp/proj",
            "claude --resume abc",
            "claude --resume abc",
            &spawner2,
            &|_| true,
            None,
            Some(&hook2),
        );
        let recorded2 = calls2.lock().unwrap().clone();
        assert_eq!(recorded2.len(), 2, "首通道败必须降级第二通道");
        assert_eq!(
            *deliveries2.lock().unwrap(),
            0,
            "spawn Err = 零交付 ⇒ 交付回调零次（零交付零 pending）"
        );
        let SpawnSpec::MacosApplescript { script: second, .. } = &recorded2[1] else {
            panic!("第二通道同为 AppleScript 变体");
        };
        assert!(
            second.contains("create window with default profile command"),
            "次选是 iTerm2 形态"
        );
    }

    /// HD（H4 释放规则·出手层面）：首通道**交付未知**（osascript 超时——Apple 事件
    /// 可能已送达）⇒ **不允许尝试备用终端**（不二窗）：iTerm2 次选零出手，Err 保留
    /// 超时前缀交恢复状态机按「占位保持→转待确认只核对」收口。仅确定性失败（请求
    /// 未达终端）保留既有降级次选语义（见 [`macos_dual_channel_dispatches_via_seam`] ②）。
    #[test]
    fn hd_first_channel_timeout_stops_backup_terminal() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Err(format!(
                    "{OSASCRIPT_TIMEOUT_PREFIX}（{OSASCRIPT_WAIT_SECS}s 上限，已终止）——请重试"
                ))
            }
        };
        let r = open_macos_with(
            "/tmp/proj",
            "claude --resume abc",
            "claude --resume abc",
            &spawner,
            &|_| true,
            None,
            None,
        );
        let err = r.expect_err("交付未知必须收口为 Err（不冒充成功）");
        assert!(
            is_delivery_unknown_error(&err),
            "超时前缀必须保留（恢复状态机交付未知分类依据）：{err}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "交付未知不允许备用终端——第二通道零出手（不二窗）"
        );
    }

    /// F1② 核心序列（G5 后语义 = **无 pending 面的 legacy 模式**）：Terminal 出手
    /// Ok 但**回查未命中（死窗）**→ 自动转 iTerm2 次选；次选回查命中 → Ok。此
    /// fallback 只为非 opencode 工具保留（on_delivered=None，零行为变更）；opencode
    /// 恢复链见 [`macos_delivered_but_unobserved_stops_cascade_no_second_window`]。
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
            open_macos_with(
                "/tmp/proj",
                "claude --resume abc",
                "claude --resume abc",
                &spawner,
                &check,
                None,
                None
            ),
            Ok(()),
            "Terminal 死窗必须自动转 iTerm2 次选（无 pending 面 legacy 语义）"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "死窗转次选恰两次出手");
        assert!(
            matches!(&recorded[0], SpawnSpec::MacosApplescript { script, .. } if script.contains("do script")),
            "首通道 Terminal.app"
        );
        assert!(
            matches!(&recorded[1], SpawnSpec::MacosApplescript { script, .. } if script.contains("create window")),
            "次选 iTerm2"
        );
        let checked = checks.lock().unwrap().clone();
        assert_eq!(checked.len(), 2, "每通道各回查一次");
        assert_eq!(checked[0], "claude --resume abc", "回查收 resume 命令特征");
    }

    /// G5 核心序列（RED d 出手层面）：**交付成功（osascript Ok）后回查未命中 ⇒
    /// 不转次选通道**（pending 在场禁 fallback）——无第二次 osascript、无第二窗，
    /// 返回 Ok 交恢复状态机走待确认语义（迟到观察只核对）。交付回调恰一次。
    #[test]
    fn macos_delivered_but_unobserved_stops_cascade_no_second_window() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let deliveries = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let spawner = {
            let calls = calls.clone();
            move |spec: &SpawnSpec| {
                calls.lock().unwrap().push(spec.clone());
                Ok(())
            }
        };
        let deliveries_hook = deliveries.clone();
        let hook = move || {
            *deliveries_hook.lock().unwrap() += 1;
        };
        // 恒未命中回查（慢起窗形态：窗口晚到/进程表滞后）
        assert_eq!(
            open_macos_with(
                "/tmp/proj",
                "'bin' --server 'http://127.0.0.1:63641' --session ses_g5",
                "'bin' --server 'http://127.0.0.1:63641' --session ses_g5",
                &spawner,
                &|_| false,
                None,
                Some(&hook)
            ),
            Ok(()),
            "已交付未核实 = Ok（待确认语义由恢复状态机承载），不是 Err"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "G5 不二窗：交付后回查未命中绝不补开第二窗（无 iTerm2 第二次 osascript）"
        );
        assert!(
            matches!(&recorded[0], SpawnSpec::MacosApplescript { script, .. } if script.contains("do script")),
            "唯一出手是 Terminal.app 首通道"
        );
        assert_eq!(
            *deliveries.lock().unwrap(),
            1,
            "交付回调恰一次（pending 已在观察窗开始前插入）"
        );
    }

    #[test]
    fn macos_created_target_invalid_stops_backup_terminal() {
        let error = format!(
            "{}: 创建目标服务代际已失效",
            crate::services::opencode_native::types::codes::OC_GENERATION_MISMATCH
        );
        let calls = std::sync::Mutex::new(Vec::new());
        let spawner = |spec: &SpawnSpec| {
            calls.lock().unwrap().push(spec.clone());
            Err(error.clone())
        };
        let result = open_macos_with(
            "/tmp/proj",
            "'bin' --session ses_created",
            "bin --session ses_created",
            &spawner,
            &|_| panic!("目标失效后不得回查起窗效果"),
            None,
            Some(&|| panic!("目标失效后不得报告交付")),
        );
        assert_eq!(result, Err(error));
        let calls = calls.into_inner().unwrap();
        assert!(matches!(
            calls.as_slice(),
            [SpawnSpec::MacosApplescript { script, .. }] if script.contains("do script")
        ));
    }

    /// F1② 双通道皆败（legacy 模式，on_delivered=None：spawn Err / 回查未命中两种
    /// 失败混合）→ Err 合并且**逐通道标注**——端点 Err 臂 → 200 failed 回执 + 审计
    /// open failed:*（账实一致，「死窗」入账；端点契约面由 server.rs
    /// session_open_endpoint_spawn_failure 系列测试锁定）。G5 后交付后未命中的
    /// cascade 只存在于 legacy（非 opencode）；spawn Err 形态（零交付）两模式同形。
    #[test]
    fn macos_both_channels_fail_merged_with_channel_labels() {
        // 回查未命中形态（两通道双双死窗：恒 false 无状态假体，dyn Fn 要求 Fn）
        let check = |_: &str| false;
        let spawner = |spec: &SpawnSpec| {
            let _ = spec;
            Ok(())
        };
        let err = open_macos_with(
            "/tmp/proj",
            "claude --resume abc",
            "claude --resume abc",
            &spawner,
            &check,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("Terminal.app"), "错误必须标注首通道：{err}");
        assert!(err.contains("iTerm2"), "错误必须标注次选通道：{err}");
        assert!(
            err.contains("回查未命中"),
            "死窗失败必须点明回查未命中：{err}"
        );

        // 纯 spawn Err 形态（既有语义，通道名标注；G5 交付回调在场也不改变
        // 零交付的失败合并——交付未发生，错误照常进账）
        let spawner_err = |_: &SpawnSpec| Err("osascript 失败".to_string());
        let err2 = open_macos_with(
            "/tmp/proj",
            "claude --resume abc",
            "claude --resume abc",
            &spawner_err,
            &|_| true,
            None,
            Some(&|| {}),
        )
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

    // ==== M7（B1 工程修复）：opencode 恢复链二进制解析 + 版本门 ====
    // 背景（N6 E1 精化）：resume 载荷恒为 PATH 相对名 `opencode`，macOS 登录 shell
    // PATH 下解析为 Homebrew 1.19.4（≠ 规范 2.2.5）→ 起窗二进制版本错配。修法 =
    // 解析规范二进制（规范安装位 > 显式配置 > PATH 回退带版本门）+ 起窗前
    // --version 有界探测 + 与登记版本不符拒绝。本组测试全部注入式（tempdir 假 home
    // / 假 PATH 串 / 显式 env 值参数），零接触真实 ~/.opencode 与真实 PATH。

    /// 写测试桩脚本（unix: sh 脚本 + 0o755；windows: .cmd）。返回其绝对路径串。
    #[cfg(unix)]
    fn write_exec_stub(dir: &std::path::Path, rel: &str, body: &str) -> String {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().to_string()
    }

    #[cfg(not(unix))]
    fn write_exec_stub(dir: &std::path::Path, rel: &str, body: &str) -> String {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().to_string()
    }

    /// 版本桩脚本体：`--version` 输出 `opencode v<ver>`（真 2.2.5 同形态）
    #[cfg(unix)]
    fn version_stub_body(ver: &str) -> String {
        format!("#!/bin/sh\necho \"opencode v{ver}\"\n")
    }

    #[cfg(not(unix))]
    fn version_stub_body(ver: &str) -> String {
        format!("@echo off\necho opencode v{ver}\n")
    }

    /// 闭包升级为 'static 引用（G2 依赖束测试装配专用：进程生命周期泄漏，量级
    /// 可忽略；restore_pending 测试同名先例）。
    fn leak_box<T: 'static>(value: T) -> &'static T {
        Box::leak(Box::new(value))
    }

    /// ① 规范安装位优先：`~/.opencode/bin/opencode` 在场 → 必选它，PATH 里的
    /// 诱饵（Homebrew 1.19.4 同构）永不被解析（N6 E1 反向锚）
    #[test]
    fn resolve_opencode_binary_prefers_canonical_home_over_path_decoy() {
        let home = tempfile::tempdir().unwrap();
        let canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let decoy_dir = tempfile::tempdir().unwrap();
        let _decoy = write_exec_stub(decoy_dir.path(), "opencode", &version_stub_body("1.19.4"));
        let path_env = format!("{}:{}", decoy_dir.path().display(), "/usr/bin");
        let hit = resolve_opencode_binary_in(Some(home.path()), Some(path_env.as_str()), None)
            .expect("规范安装位在场必须解析成功");
        assert_eq!(hit, canonical, "必须解析规范安装位，不是 PATH 诱饵");
    }

    /// ② 显式配置：规范安装位缺席时 OPENCODE_BINARY 生效（优先级 2，仍压 PATH）
    #[test]
    fn resolve_opencode_binary_env_override_when_canonical_missing() {
        let home = tempfile::tempdir().unwrap(); // 空假 home（无规范安装位）
        let cfg_dir = tempfile::tempdir().unwrap();
        let cfg_bin = write_exec_stub(cfg_dir.path(), "custom-oc", &version_stub_body("2.2.5"));
        let decoy_dir = tempfile::tempdir().unwrap();
        let _decoy = write_exec_stub(decoy_dir.path(), "opencode", &version_stub_body("1.19.4"));
        let path_env = decoy_dir.path().display().to_string();
        let hit = resolve_opencode_binary_in(
            Some(home.path()),
            Some(path_env.as_str()),
            Some(std::ffi::OsStr::new(&cfg_bin)),
        )
        .expect("显式配置在场必须解析成功");
        assert_eq!(hit, cfg_bin, "显式配置必须压过 PATH 诱饵");
    }

    /// ②b 显式配置指认的文件缺失 = 配置错误 → 诚实失败，**不静默回落 PATH**
    /// （静默回落会把用户指认面悄悄换成 PATH 任意二进制——正是 N6 要堵的形态）
    #[test]
    fn resolve_opencode_binary_env_override_missing_is_honest_no_path_fallthrough() {
        let home = tempfile::tempdir().unwrap();
        let decoy_dir = tempfile::tempdir().unwrap();
        let _decoy = write_exec_stub(decoy_dir.path(), "opencode", &version_stub_body("1.19.4"));
        let path_env = decoy_dir.path().display().to_string();
        let err = resolve_opencode_binary_in(
            Some(home.path()),
            Some(path_env.as_str()),
            Some(std::ffi::OsStr::new("/nonexistent/oc-bin")),
        )
        .unwrap_err();
        assert!(
            err.starts_with(OPENCODE_BINARY_MISSING),
            "缺配置必须结构化报缺失：{err}"
        );
        assert!(
            err.contains("OPENCODE_BINARY"),
            "错误必须披露显式配置键：{err}"
        );
    }

    /// ③ PATH 回退：规范安装位与显式配置均缺席 → PATH 首个含可执行 opencode 的
    /// 目录命中（shell 解析语义同构）
    #[test]
    fn resolve_opencode_binary_path_fallback_when_no_canonical_no_env() {
        let home = tempfile::tempdir().unwrap();
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        // d1 无命中；d2 才有——断言逐目录扫描
        std::fs::create_dir_all(d1.path().join("empty")).unwrap();
        let hit = write_exec_stub(d2.path(), "opencode", &version_stub_body("2.2.5"));
        let path_env = format!(
            "{}:{}:{}",
            d1.path().join("empty").display(),
            d2.path().display(),
            "/usr/bin"
        );
        let got = resolve_opencode_binary_in(Some(home.path()), Some(path_env.as_str()), None)
            .expect("PATH 回退必须命中");
        assert_eq!(got, hit);
    }

    /// ①b 规范安装位存在但不可执行 = 安装面破损 → 结构化诚实失败，**不回落
    /// PATH**（回落 Homebrew 正是 N6 E1 要堵的死法）
    #[cfg(unix)]
    #[test]
    fn resolve_opencode_binary_canonical_not_executable_is_honest_no_fallback() {
        let home = tempfile::tempdir().unwrap();
        let p = home.path().join(".opencode/bin/opencode");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, version_stub_body("2.2.5")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let decoy_dir = tempfile::tempdir().unwrap();
        let _decoy = write_exec_stub(decoy_dir.path(), "opencode", &version_stub_body("1.19.4"));
        let path_env = decoy_dir.path().display().to_string();
        let err = resolve_opencode_binary_in(Some(home.path()), Some(path_env.as_str()), None)
            .unwrap_err();
        assert!(
            err.starts_with(OPENCODE_BINARY_NOT_EXECUTABLE),
            "不可执行的权威候选必须结构化报错：{err}"
        );
        assert!(
            !err.contains(decoy_dir.path().to_string_lossy().as_ref()),
            "错误不得回落 PATH 诱饵：{err}"
        );
    }

    /// ④ 三路全缺席 → OPENCODE_BINARY_MISSING 结构化缺失（列出搜索面）
    #[test]
    fn resolve_opencode_binary_all_missing_is_structured() {
        let home = tempfile::tempdir().unwrap();
        let err = resolve_opencode_binary_in(Some(home.path()), Some("/nonexistent-dir"), None)
            .unwrap_err();
        assert!(
            err.starts_with(OPENCODE_BINARY_MISSING),
            "必须结构化报缺失：{err}"
        );
        assert!(
            err.contains(".opencode"),
            "错误必须披露搜索过的规范安装位：{err}"
        );
    }

    /// 版本输出解析：`opencode v2.2.5` → `2.2.5`（剥 v 前缀）；无数字 token /
    /// 剥前缀后非数字开头 → None（fail-closed）
    #[test]
    fn parse_version_output_strips_v_and_requires_digit() {
        assert_eq!(
            parse_version_output("opencode v2.2.5").as_deref(),
            Some("2.2.5")
        );
        assert_eq!(parse_version_output("2.2.5").as_deref(), Some("2.2.5"));
        assert_eq!(
            parse_version_output("opencode 1.19.4 (homebrew)").as_deref(),
            Some("1.19.4")
        );
        assert_eq!(parse_version_output("opencode dev"), None);
        assert_eq!(parse_version_output(""), None);
    }

    /// 版本门活桩（unix）：真子进程 `--version` 探测——2.2.5 桩过门且载荷为绝对
    /// 路径形态；1.19.4 桩（N6 诱饵同版）拒门并给可读原因
    #[cfg(unix)]
    #[test]
    fn prepare_opencode_resume_gates_by_probed_version() {
        let home = tempfile::tempdir().unwrap();
        // 过门形态：2.2.5 桩 → Ok，载荷带绝对路径 + --session + 效果特征
        let canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let p = prepare_opencode_resume_with(Some(home.path()), Some("/usr/bin"), None, "ses_x")
            .expect("2.2.5 桩必须过版本门");
        assert_eq!(
            p.payload.windows_payload,
            format!("{canonical} --session ses_x"),
            "效果特征/cmd 载荷 = 绝对路径直拼"
        );
        assert_eq!(
            p.payload.macos_payload,
            format!("'{}' --session ses_x", canonical),
            "macOS 脚本载荷路径须单引号包裹（防空格路径断词）"
        );
        assert!(p.env_secret.is_none(), "自托管形态无受控启动入口秘密");
        // 拒门形态：1.19.4 桩（PATH 诱饵同版）→ 结构化拒绝 + 双版本可读原因
        let home2 = tempfile::tempdir().unwrap();
        let _old = write_exec_stub(
            home2.path(),
            ".opencode/bin/opencode",
            &version_stub_body("1.19.4"),
        );
        let err = prepare_opencode_resume_with(Some(home2.path()), Some("/usr/bin"), None, "ses_x")
            .unwrap_err();
        assert!(
            err.starts_with(OPENCODE_VERSION_MISMATCH),
            "版本错配必须结构化拒绝：{err}"
        );
        assert!(err.contains("1.19.4"), "错误必须披露实测版本：{err}");
        assert!(
            err.contains(OPENCODE_ANCHORED_VERSION),
            "错误必须披露登记版本：{err}"
        );
    }

    /// 版本探测有界超时：`--version` 挂死（sleep 30）→ 有界失败（≤ 探测超时 + 裕量），
    /// 不悬挂恢复链
    #[cfg(unix)]
    #[test]
    fn probe_opencode_version_timeout_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let hang = write_exec_stub(dir.path(), "hang-oc", "#!/bin/sh\nsleep 30\n");
        let t0 = std::time::Instant::now();
        let err = probe_opencode_version(&hang).unwrap_err();
        let elapsed = t0.elapsed();
        assert!(
            err.starts_with(OPENCODE_VERSION_PROBE_FAILED),
            "挂死探测必须结构化失败：{err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(OPENCODE_VERSION_PROBE_TIMEOUT_SECS + 10),
            "探测必须有界返回（实测 {elapsed:?}）"
        );
    }

    /// 载荷构造纯函数：macOS 形态单引号、cmd/效果形态直拼；会话 id 直通
    #[test]
    fn opencode_payload_build_quoted_script_and_bare_sig() {
        let p = build_opencode_resume_payload("/opt/oc bin/opencode", "ses_7");
        assert_eq!(p.macos_payload, "'/opt/oc bin/opencode' --session ses_7");
        assert_eq!(p.windows_payload, "/opt/oc bin/opencode --session ses_7");
        // 效果回查特征 = cmd 形态（进程 argv 直拼串）
        let snap = vec![p.windows_payload.clone()];
        assert!(resume_effect_in_snapshot(&p.windows_payload, &snap));
    }

    /// M7 载荷/特征分体锚（跨平台可测）：macOS 脚本载荷携带**单引号**绝对路径，
    /// 效果回查收**不带引号**的 argv 直拼形态——shell 消费引号后进程 argv[0] 是
    /// 字面路径，带引号特征对进程快照恒 miss（死窗假阴性）
    #[test]
    fn macos_script_and_effect_sig_are_split_forms() {
        let checks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = |_spec: &SpawnSpec| Ok(());
        let checks_ref = checks.clone();
        let check = move |sig: &str| {
            checks_ref.lock().unwrap().push(sig.to_string());
            true
        };
        open_macos_with(
            "/tmp/proj",
            "'/oc bin/opencode' --session s1",
            "/oc bin/opencode --session s1",
            &spawner,
            &check,
            None,
            None,
        )
        .expect("回查命中必须收口");
        let checked = checks.lock().unwrap().clone();
        assert_eq!(
            checked,
            vec!["/oc bin/opencode --session s1".to_string()],
            "效果回查特征必须是不带引号的直拼形态"
        );
    }

    // ==== F4（Ruling 4）+ G2：显式连接形态（登记在场）+ 密码脱离命令文本 ====

    /// 全缝版登记读取（测试内真实 fs 读 tempdir，零接触真实状态根）。
    fn fs_reader() -> impl Fn(&std::path::Path) -> Result<Vec<u8>, String> {
        |path: &std::path::Path| std::fs::read(path).map_err(|e| format!("unreadable: {e}"))
    }

    /// G2 全缝依赖束（测试装配）：真实 tempdir 登记读取 + 注入进程事实 +
    /// tempdir 受控入口路径（真实密码零复制零落盘）。
    #[cfg(unix)]
    fn gb_sources<'a>(
        process_fact: &'a dyn Fn(u32) -> Option<i64>,
        secret_dir: &std::path::Path,
    ) -> ResumeSources<'a> {
        let reader = fs_reader();
        let dir = secret_dir.to_path_buf();
        ResumeSources {
            read_state_file: leak_box(reader),
            process_fact,
            env_secret_writer: leak_box(move |password: &str| {
                Ok(EnvSecret {
                    path: dir.join(env_secret_token()),
                    password: password.to_string(),
                })
            }),
        }
    }

    /// 显式连接形态 + G2 五媒介零凭据断言（RED a）：登记发现分支在场 ⇒ 载荷携带
    /// `--server <登记端点>` + 受控启动入口（env 文件 source 即删），**五媒介
    /// （命令文本 / osascript 参数 / argv / 日志回执文 / 审计摘要）零凭据明文**，
    /// 子进程 env 照常命中 OPENCODE_PASSWORD（假凭据 `Gpw'x&1$$2` 同级怪字符）。
    #[cfg(unix)]
    #[test]
    fn explicit_resume_five_media_zero_plaintext_and_subprocess_env_hit() {
        const PW: &str = "Gpw'x&1$$2";
        let home = tempfile::tempdir().unwrap();
        // 桩 bin：--version 过门；被恢复调用时把子进程 env 全量落 dump（env 命中断言）
        let dump = home.path().join("env-dump.txt");
        let canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'opencode v2.2.5\\n'; exit 0; fi\nenv > \"${GB_ENV_DUMP:?}\"\n",
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            format!(
                r#"{{"id":"gb-reg-1","version":"2.2.5","url":"http://127.0.0.1:63641","pid":52284,"password":"{PW}"}}"#
            ),
        )
        .unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let fact = |_pid: u32| Some(1_234_i64);
        let p = prepare_opencode_resume_with_sources(
            Some(home.path()),
            None,
            None,
            Some("/usr/bin"),
            None,
            "ses_gb_a1",
            &gb_sources(&fact, secret_dir.path()),
        )
        .expect("登记在场 + 三核对通过必须构造显式连接载荷");
        let secret = p.env_secret.as_ref().expect("显式形态必带受控入口秘密");
        assert_eq!(secret.password, PW, "秘密随载荷同源携带（内存态，不落盘）");
        // 媒介 1（Terminal 命令文本）：macos_payload 零明文，改为 source <env 路径>
        // 即删形态；HOME 目标环境照随（非机密）
        assert!(
            !p.payload.macos_payload.contains(PW),
            "媒介1 命令文本零凭据明文：{}",
            p.payload.macos_payload
        );
        assert!(
            p.payload.macos_payload.starts_with(&format!(
                "source '{}' && rm -f '{}' && env HOME='{}' ",
                secret.path.display(),
                secret.path.display(),
                home.path().display()
            )),
            "载荷 = source <env> 即删 + HOME 目标环境 + 二进制：{}",
            p.payload.macos_payload
        );
        assert!(
            p.payload.macos_payload.contains(&format!(
                "'{}' --server 'http://127.0.0.1:63641' --session ses_gb_a1",
                canonical
            )),
            "二进制/端点单引号包裹：{}",
            p.payload.macos_payload
        );
        // 媒介 2（osascript 参数 = 脚本全文）：双通道脚本零明文
        for script in [
            terminal_open_script("/tmp/proj", &p.payload.macos_payload),
            iterm_open_window_script("/tmp/proj", &p.payload.macos_payload),
        ] {
            assert!(
                !script.contains(PW),
                "媒介2 osascript 脚本/参数零凭据明文：{script}"
            );
        }
        // 媒介 3（进程 argv = effect_sig / windows 载荷）：零明文
        assert!(
            !p.payload.windows_payload.contains(PW),
            "媒介3 argv 零凭据明文：{}",
            p.payload.windows_payload
        );
        // 媒介 4（日志/回执文）：osascript stderr 回显整段脚本时分类产物仍零明文；
        // 审计摘要（命令表产物）零明文
        let echoed = classify_resume_error(&format!(
            "execution error: <script>：{}",
            p.payload.macos_payload
        ));
        assert!(
            !echoed.contains(PW),
            "媒介4 回执/日志零凭据明文（stderr 回显命令文本也不泄）：{echoed}"
        );
        let audit = resume_command("opencode", "ses_gb_a1").unwrap_or_default();
        assert!(!audit.contains(PW), "媒介4 审计摘要零凭据明文：{audit}");
        // 子进程 env 命中 + source 后即删（交付时刻落盘 → 载荷执行链端到端）：
        // 0600 落盘 → bash 真跑载荷文本（cd + source + rm + env <bin>）→ dump 断言
        materialize_env_secret(secret).expect("交付时刻落盘必须成功");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "env 文件必须 0600");
        assert!(
            std::fs::read_to_string(&secret.path)
                .unwrap()
                .contains(&render_env_secret_content(PW)),
            "env 文件内容 = export OPENCODE_PASSWORD（单引号转义）"
        );
        let full_cmd =
            macos_command_payload(&home.path().to_string_lossy(), &p.payload.macos_payload);
        let out = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(&full_cmd)
            .env("GB_ENV_DUMP", &dump)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("载荷必须可在 shell 中执行");
        assert!(
            out.status.success(),
            "载荷执行链必须成功（cd+source+rm+env bin）：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let env_dump = std::fs::read_to_string(&dump).expect("桩 bin 落 env dump");
        assert!(
            env_dump.contains(&format!("OPENCODE_PASSWORD={PW}")),
            "子进程 env 必须命中 OPENCODE_PASSWORD（受控入口等效性）：{env_dump}"
        );
        assert!(
            !secret.path.exists(),
            "source 后即删：env 文件不得残留（rm -f 在 && 链上）"
        );
    }

    /// G2 三核对之①端点回环：非回环登记（局域网/公网/链路本地地址）⇒ 结构化拒绝，
    /// 零起窗（不回落自托管——回落=开新窗）；回环（127.0.0.1/localhost）放行。
    #[cfg(unix)]
    #[test]
    fn non_loopback_registration_refused_without_payload() {
        let fact = |_pid: u32| Some(1_i64);
        for (url, expect_refuse) in [
            ("http://192.168.1.10:63641", true),
            ("http://[fe80::1]:63641", true),
            ("http://127.0.0.1:63641", false),
            ("http://localhost:63641", false),
        ] {
            let home = tempfile::tempdir().unwrap();
            write_exec_stub(
                home.path(),
                ".opencode/bin/opencode",
                &version_stub_body("2.2.5"),
            );
            let state_dir = home.path().join(".local/state/opencode");
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(
                state_dir.join("service.json"),
                format!(
                    r#"{{"id":"gb-reg-lb","version":"2.2.5","url":"{url}","pid":7,"password":"pw"}}"#
                ),
            )
            .unwrap();
            let secret_dir = tempfile::tempdir().unwrap();
            let r = prepare_opencode_resume_with_sources(
                Some(home.path()),
                None,
                None,
                Some("/usr/bin"),
                None,
                "ses_gb_lb",
                &gb_sources(&fact, secret_dir.path()),
            );
            if expect_refuse {
                let err = r.expect_err("非回环登记必须拒绝");
                assert!(
                    err.starts_with(OC_RESUME_ENDPOINT_NOT_LOOPBACK),
                    "结构化哨兵：{err}"
                );
                assert!(err.contains(url), "错误披露非回环端点：{err}");
            } else {
                let p = r.expect("回环端点必须放行");
                assert!(
                    p.payload.windows_payload.contains("--server"),
                    "回环端点走显式连接形态"
                );
            }
        }
    }

    /// GB fix round 1（GR1 GB-1）：校验哨兵前缀识别 + 裸码归一——带前缀形态
    /// （生产产出形态）与裸码都命中；非校验失败与「相似但非本哨兵」不误吞。
    #[test]
    fn resume_validation_error_code_matches_prefixed_and_bare() {
        assert_eq!(resume_validation_error_code("no_cwd"), Some("no_cwd"));
        assert_eq!(
            resume_validation_error_code("no_resume_command"),
            Some("no_resume_command")
        );
        assert_eq!(
            resume_validation_error_code("no_loopback_endpoint: 登记端点非回环（x），拒绝起窗"),
            Some("no_loopback_endpoint")
        );
        assert_eq!(
            resume_validation_error_code(
                "registration_process_unverified: 登记进程（pid 99）已不在运行"
            ),
            Some("registration_process_unverified")
        );
        assert_eq!(
            resume_validation_error_code(
                "session_directory_unresolved: 目标会话目录不可解析（/tmp/gone），拒绝起窗"
            ),
            Some("session_directory_unresolved")
        );
        assert_eq!(
            resume_validation_error_code("RESUME_ALL_CHANNELS_FAIL: 全部 resume 通道失败"),
            None,
            "非校验失败 = None（走 200 failed + 审计既有契约）"
        );
        assert_eq!(
            resume_validation_error_code("no_cwd_x"),
            None,
            "相似但不带「: 」分隔的形态不误吞"
        );
    }

    /// G2 残留清扫（生命周期申报面收敛）：超 24h 的 resume-env-*.sh 残留被删，
    /// 新鲜残留与非本前缀文件不动。stale 面用 File::set_times 拨旧 mtime 构造。
    #[cfg(unix)]
    #[test]
    fn resume_env_sweep_removes_stale_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("resume-env-old.sh");
        std::fs::write(&stale, "x").unwrap();
        let fresh = dir.path().join("resume-env-new.sh");
        std::fs::write(&fresh, "x").unwrap();
        let other = dir.path().join("unrelated.sh");
        std::fs::write(&other, "x").unwrap();
        use std::time::{Duration, SystemTime};
        let old = SystemTime::now() - Duration::from_secs(25 * 3600);
        {
            let f = std::fs::File::options().write(true).open(&stale).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        sweep_stale_resume_env_files(dir.path());
        assert!(!stale.exists(), "超 24h 残留必须被清扫");
        assert!(fresh.exists(), "新鲜残留保留（24h 窗内）");
        assert!(other.exists(), "非 resume-env 前缀文件不得误删");
    }

    /// G2 三核对之②登记进程身份：登记 pid 进程事实不可读（进程死/表读不到）⇒
    /// 结构化拒绝零起窗（旧代际残迹不连）。
    #[cfg(unix)]
    #[test]
    fn dead_registration_pid_refused_without_payload() {
        let home = tempfile::tempdir().unwrap();
        write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            br#"{"id":"gb-reg-dp","version":"2.2.5","url":"http://127.0.0.1:63641","pid":99,"password":"pw"}"#,
        )
        .unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let dead_fact = |_pid: u32| -> Option<i64> { None };
        let err = prepare_opencode_resume_with_sources(
            Some(home.path()),
            None,
            None,
            Some("/usr/bin"),
            None,
            "ses_gb_dp",
            &gb_sources(&dead_fact, secret_dir.path()),
        )
        .expect_err("登记进程死必须拒绝");
        assert!(
            err.starts_with(OC_RESUME_REGISTRATION_UNVERIFIED),
            "结构化哨兵：{err}"
        );
        assert!(err.contains("99"), "错误披露登记 pid：{err}");
        // 活 pid → 放行（登记发现分支命中）
        let alive_fact = |_pid: u32| -> Option<i64> { Some(456) };
        let p = prepare_opencode_resume_with_sources(
            Some(home.path()),
            None,
            None,
            Some("/usr/bin"),
            None,
            "ses_gb_dp2",
            &gb_sources(&alive_fact, secret_dir.path()),
        )
        .expect("登记进程活必须放行");
        assert!(p.env_secret.is_some(), "显式形态带受控入口秘密");
    }

    /// 显式连接形态的 XDG 传递：xdg 两参在场 → macOS 载荷携带 XDG_STATE_HOME /
    /// XDG_CONFIG_HOME（目标环境白名单随载荷走）。
    #[cfg(unix)]
    #[test]
    fn opencode_explicit_payload_carries_xdg_env_when_provided() {
        let home = tempfile::tempdir().unwrap();
        let _canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            br#"{"id":"f4-reg-2","version":"2.2.5","url":"http://127.0.0.1:63642","pid":1,"password":"pw2"}"#,
        )
        .unwrap();
        let xdg_state = home.path().join("xdg-state");
        let xdg_config = home.path().join("xdg-config");
        let state_dir = xdg_state.join("opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            br#"{"id":"f4-reg-2","version":"2.2.5","url":"http://127.0.0.1:63642","pid":1,"password":"pw2"}"#,
        )
        .unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let fact = |_pid: u32| Some(1_i64);
        let p = prepare_opencode_resume_with_sources(
            Some(home.path()),
            Some(&xdg_state),
            Some(&xdg_config),
            Some("/usr/bin"),
            None,
            "ses_f4y",
            &gb_sources(&fact, secret_dir.path()),
        )
        .expect("登记在场必须构造显式连接载荷");
        assert!(
            p.payload
                .macos_payload
                .contains(&format!("XDG_STATE_HOME='{}'", xdg_state.display()))
                && p.payload
                    .macos_payload
                    .contains(&format!("XDG_CONFIG_HOME='{}'", xdg_config.display())),
            "XDG 两参必须进 macOS env 前缀：{}",
            p.payload.macos_payload
        );
    }

    /// 登记形状非法（缺 password/端点缺端口）→ 回落自托管形态（fail-closed，不猜
    /// 端点），M7 原载荷零回退。
    #[cfg(unix)]
    #[test]
    fn opencode_malformed_registration_falls_back_to_self_host() {
        let home = tempfile::tempdir().unwrap();
        let canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        for bad in [
            br#"{"id":"x","version":"2.2.5","url":"http://127.0.0.1:63643","pid":1}"#.as_slice(), // 缺 password
            br#"{"id":"x","version":"2.2.5","url":"http://127.0.0.1","pid":1,"password":"p"}"#
                .as_slice(), // 端点缺端口
        ] {
            std::fs::write(state_dir.join("service.json"), bad).unwrap();
            let secret_dir = tempfile::tempdir().unwrap();
            let fact = |_pid: u32| Some(1_i64);
            let p = prepare_opencode_resume_with_sources(
                Some(home.path()),
                None,
                None,
                Some("/usr/bin"),
                None,
                "ses_f4z",
                &gb_sources(&fact, secret_dir.path()),
            )
            .expect("非法登记回落自托管");
            assert_eq!(
                p.payload.windows_payload,
                format!("{canonical} --session ses_f4z"),
                "非法登记必须回落自托管形态（无 --server）"
            );
            assert!(!p.payload.macos_payload.contains("--server"));
            assert!(p.env_secret.is_none(), "自托管形态无受控入口秘密");
        }
    }

    /// 登记在场时，规范二进制解析结果必须进入 macOS 起窗脚本。所有输入都由夹具
    /// 提供，不依赖本机安装、登记或进程表，也不实际起窗。
    #[cfg(target_os = "macos")]
    #[test]
    fn core_open_opencode_end_to_end_uses_resolved_binary() {
        const ENDPOINT: &str = "http://127.0.0.1:63641";
        const PASSWORD: &str = "resume-fixture-password";
        let home = tempfile::tempdir().unwrap();
        let canonical = write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body(OPENCODE_ANCHORED_VERSION),
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            serde_json::json!({
                "id": "m7-fixture",
                "version": OPENCODE_ANCHORED_VERSION,
                "url": ENDPOINT,
                "pid": 7,
                "password": PASSWORD,
            })
            .to_string(),
        )
        .unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let fact = |pid| (pid == 7).then_some(1_i64);
        let prepared = prepare_opencode_resume_with_sources(
            Some(home.path()),
            None,
            None,
            None,
            None,
            "abc",
            &gb_sources(&fact, secret_dir.path()),
        )
        .expect("有效的隔离登记必须通过恢复预备链");
        let secret = prepared.env_secret.as_ref().expect("登记形态必须带秘密");
        let calls = std::sync::Mutex::new(Vec::new());
        let spawner = |spec: &SpawnSpec| {
            calls.lock().unwrap().push(spec.clone());
            Ok(())
        };
        let expected_effect = format!("{canonical} --server {ENDPOINT} --session abc");
        open_macos_with(
            "/tmp/proj-oc-m7",
            &prepared.payload.macos_payload,
            &prepared.payload.windows_payload,
            &spawner,
            &move |effect| effect == expected_effect,
            Some(secret),
            None,
        )
        .expect("规范安装位在场且版本匹配时必须过门出手");
        let recorded = calls.into_inner().unwrap();
        assert_eq!(recorded.len(), 1, "过门后恰好一次出手");
        let SpawnSpec::MacosApplescript { script, .. } = &recorded[0] else {
            panic!("macOS 必须收 AppleScript 变体");
        };
        assert!(
            script.contains(&format!(
                "cd '/tmp/proj-oc-m7' && source '{}' && rm -f '{}'",
                secret.path.display(),
                secret.path.display(),
            )),
            "脚本必须 source 受控入口后即删：{script}"
        );
        assert!(
            script.contains(&format!(
                "'{canonical}' --server '{ENDPOINT}' --session abc"
            )),
            "脚本必须携带规范二进制绝对路径、登记端点和会话 id：{script}"
        );
        assert!(
            !script.contains("OPENCODE_PASSWORD") && !script.contains(PASSWORD),
            "命令文本不得包含凭据（密码只经 env 文件交付）：{script}"
        );
    }

    // ==== M7 隔离活体（#[ignore] 环境门控，非 CI）====
    // 复审裁决（2026-11-16）：resume 起窗不得继承 PATH 解析到 Homebrew 1.19.4，
    // 必须规范二进制 + 正确 CLI 参数，并验正确目录/会话/原生首句。本测试在
    // **进程级隔离 env** 下真跑生产链：HOME 重定向（dirs::home_dir 读进程 env →
    // 规范安装位落在隔离根）+ PATH 前置 1.19.4 诱饵 + 记录桩替换真二进制
    // （零真实 opencode 状态触碰）+ 生产 spawn_terminal 真开 Terminal 窗 +
    // 生产效果回查。运行法（前置：mkdir -p /tmp/m7-live-root）：
    //   HOME=/tmp/m7-live-root PATH=/tmp/m7-live-root/decoy:/usr/bin:/bin:/usr/sbin \
    //   M7_ISO_ROOT=/tmp/m7-live-root \
    //   cargo test --lib live_opencode_resume_isolated_three_assertions -- --ignored --nocapture
    #[ignore = "M7 隔离活体：需隔离 env（HOME/PATH/M7_ISO_ROOT）+ 真 Terminal 窗，显式运行"]
    #[test]
    fn live_opencode_resume_isolated_three_assertions() {
        let root = std::env::var_os("M7_ISO_ROOT")
            .map(std::path::PathBuf::from)
            .expect("必须设 M7_ISO_ROOT（隔离根）");
        let project = root.join("test-project");
        std::fs::create_dir_all(&project).unwrap();
        let journal_a = root.join("journal-phase-a.log"); // 2.2.5 恢复桩流水
        let journal_b = root.join("journal-phase-b.log"); // 1.19.4 拒绝桩流水
        let journal_d = root.join("journal-decoy.log"); // 诱饵桩流水（恒应为空）
        for j in [&journal_a, &journal_b, &journal_d] {
            let _ = std::fs::remove_file(j);
        }
        let session_id = format!(
            "ses_m7_live_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let write_stub = |path: &std::path::Path,
                          version: &str,
                          journal: &std::path::Path,
                          hang: bool| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let hang_block = if hang {
                "sleep 30".to_string() // 挂死形态：--version 不返回（有界探测断言面）
            } else {
                format!("echo \"opencode v{version}\"; exit 0")
            };
            let body = format!(
                "#!/bin/sh\nprintf '%s\\n' \"invoked $(date +%H:%M:%S) cwd=$PWD argv=$*\" >> {journal}\nif [ \"$1\" = \"--version\" ]; then\n  {hang_block}\nfi\nsleep 6\nexit 0\n",
                journal = journal.display(),
                hang_block = hang_block,
            );
            std::fs::write(path, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let canon = root.join(".opencode/bin/opencode");
        let decoy = root.join("decoy/opencode");
        // ---- Phase A：恢复三断言 + 诱饵反证（规范桩 2.2.5、诱饵在 PATH 首位）----
        write_stub(&canon, "2.2.5", &journal_a, false);
        write_stub(&decoy, "1.19.4", &journal_d, false);
        let mut session = sess(
            crate::session::AgentType::OpenCode,
            project.to_string_lossy().as_ref(),
        );
        session.id = session_id.clone();
        let t0 = std::time::Instant::now();
        let r = open_session_terminal(&session);
        println!(
            "[M7-LIVE] Phase A open_session_terminal → {r:?}（耗时 {:?}）",
            t0.elapsed()
        );
        // 生产效果回查命中才 Ok；双通道死窗才 Err（记录桩存活 6s > 回查窗 3s，
        // 正常宿主必 Ok）
        r.expect("Phase A：隔离起窗必须收口（Terminal/iTerm2 双通道皆败则检查宿主 GUI 能力）");
        let ja = std::fs::read_to_string(&journal_a).unwrap_or_default();
        let jd = std::fs::read_to_string(&journal_d).unwrap_or_default();
        println!("[M7-LIVE] 规范桩流水：\n{ja}\n[M7-LIVE] 诱饵桩流水：\n{jd}");
        // 断言 2（正确会话）：打开即被恢复的 sessionID——窗口桩收到的 argv = --session <id>
        assert!(
            ja.contains(&format!("argv=--session {session_id}")),
            "窗口载荷必须携带被恢复会话 id：{ja}"
        );
        // 断言 1（正确目录）：窗口桩的 cwd = 隔离项目目录（cd 落点实证）
        let want_cwd = format!("cwd={}", project.display());
        assert!(
            ja.lines()
                .any(|l| l.contains(&want_cwd) && l.contains("argv=--session")),
            "窗口桩 cwd 必须是隔离项目目录：{ja}"
        );
        // 断言 3（首次派发仅一次 + 零注入）：窗口调用恰一次、无 --version 之外的
        // 第二次起窗、argv 无消息文本（resume 零注入——首句由交付入口原生 send
        // 恰一次承担，H07/H13/N5 在案）
        let window_invocations = ja.lines().filter(|l| l.contains("argv=--session")).count();
        assert_eq!(
            window_invocations, 1,
            "恢复起窗必须恰一次（无重复派发）：{ja}"
        );
        assert!(
            !ja.contains("firstMessage") && !window_text_has_message(&ja),
            "resume 载荷必须零消息注入：{ja}"
        );
        // 诱饵反证：1.19.4 诱饵在 PATH 首位，修复后规范安装位胜出——诱饵零调用
        assert!(
            jd.trim().is_empty(),
            "PATH 诱饵（伪装 1.19.4）不得被解析/调用：{jd}"
        );
        // ---- Phase B：版本门拒绝面（规范桩改报 1.19.4）----
        write_stub(&canon, "1.19.4", &journal_b, false);
        let mut s2 = sess(
            crate::session::AgentType::OpenCode,
            project.to_string_lossy().as_ref(),
        );
        s2.id = format!("{session_id}-b");
        let rb = open_session_terminal(&s2);
        println!("[M7-LIVE] Phase B（1.19.4 桩）→ {rb:?}");
        let eb = rb.unwrap_err();
        assert!(
            eb.starts_with(OPENCODE_VERSION_MISMATCH),
            "版本错配必须拒绝恢复：{eb}"
        );
        assert!(
            eb.contains("1.19.4") && eb.contains(OPENCODE_ANCHORED_VERSION),
            "拒绝原因必须披露实测/登记双版本：{eb}"
        );
        let jb = std::fs::read_to_string(&journal_b).unwrap_or_default();
        assert!(
            !jb.lines().any(|l| l.contains("argv=--session")),
            "拒绝后零起窗（窗口桩零调用）：{jb}"
        );
        assert!(
            std::fs::read_to_string(&journal_d)
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "诱饵全程零调用"
        );
        // ---- Phase C：探测有界（规范桩 --version 挂死）----
        write_stub(&canon, "2.2.5", &journal_a, true);
        let mut s3 = sess(
            crate::session::AgentType::OpenCode,
            project.to_string_lossy().as_ref(),
        );
        s3.id = format!("{session_id}-c");
        let t1 = std::time::Instant::now();
        let rc = open_session_terminal(&s3).unwrap_err();
        let ec = t1.elapsed();
        println!("[M7-LIVE] Phase C（挂死桩）→ {rc:?}（耗时 {ec:?}）");
        assert!(
            rc.starts_with(OPENCODE_VERSION_PROBE_FAILED),
            "挂死探测必须结构化失败：{rc}"
        );
        assert!(
            ec < std::time::Duration::from_secs(OPENCODE_VERSION_PROBE_TIMEOUT_SECS + 10),
            "探测必须有界返回（实测 {ec:?}）"
        );
        // 恢复 2.2.5 桩（现场复位，便于复跑）
        write_stub(&canon, "2.2.5", &journal_a, false);
        println!("[M7-LIVE] 三断言 + 诱饵反证 + 版本门 + 有界探测 全部 PASS");
    }

    /// 活体断言辅助：窗口桩流水中不得出现消息文本形态（`--session` 之外的第二
    /// 位置参数）。resume 零注入的机器可查形态。
    fn window_text_has_message(journal: &str) -> bool {
        journal
            .lines()
            .filter(|l| l.contains("argv=--session"))
            .any(|l| {
                // argv=--session <id> 后不得再有任何 token（消息文本会作为尾参出现）
                let argv = l.split("argv=").nth(1).unwrap_or("");
                argv.split_whitespace().count() > 2
            })
    }

    // ==== HC（H3）：凭据文件从创建起保持私有——全生命周期加固 ====

    /// HC 测试辅助：目录条目数（零文件产生/零残留断言面）。
    #[cfg(unix)]
    fn dir_entry_count(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(usize::MAX)
    }

    /// RED a（brief HC a）：同名文件已存在 ⇒ **不覆盖**：结构化拒绝（独占创建
    /// create-new 语义；token 含纳秒+pid+进程计数器，冲突几乎必然是残留/栽赃，
    /// fail-closed 不换名绕过、不覆盖他者文件）——原文件内容与权限逐字不动，
    /// 目录零新增。符号链接形态：文件位被 symlink 占据 ⇒ 同样拒绝，且**不跟随**
    /// ——symlink 目标零创建零写入。
    #[cfg(unix)]
    #[test]
    fn hc_env_secret_collision_refuses_and_keeps_original_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume-env-pre.sh");
        std::fs::write(&path, "export OPENCODE_PASSWORD=OTHER\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let secret = EnvSecret {
            path: path.clone(),
            password: "HCpw'1".to_string(),
        };
        let err = materialize_env_secret(&secret).expect_err("同名已存在必须拒绝");
        assert!(
            err.starts_with(ENV_SECRET_COLLISION),
            "结构化冲突哨兵：{err}"
        );
        // 原文件不动：内容逐字 + 0644 权限保持（未被覆盖/收紧）
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "export OPENCODE_PASSWORD=OTHER\n",
            "他者遗留文件内容必须原样保留"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "他者遗留文件权限必须原样保留"
        );
        assert_eq!(dir_entry_count(dir.path()), 1, "目录零新增文件");
        // 符号链接形态：文件位是 symlink ⇒ 拒绝且不跟随（目标零创建）
        std::fs::remove_file(&path).unwrap();
        let target = dir.path().join("outside-target.sh");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let err2 = materialize_env_secret(&secret).expect_err("文件位 symlink 必须拒绝");
        assert!(
            err2.starts_with(ENV_SECRET_COLLISION),
            "symlink 占位同样按冲突拒绝（O_EXCL 不跟随）：{err2}"
        );
        assert!(
            !target.exists(),
            "symlink 目标不得被创建/写入（拒绝必须不跟随）"
        );
        assert_eq!(dir_entry_count(dir.path()), 1, "目录只含 symlink 本体");
    }

    /// RED b（brief HC b）：异常目录 ⇒ 拒绝落盘，零文件产生。四异常面：组可写 /
    /// 他者（world）可写 / 目录本体是符号链接 / 路径 `..` 逃逸；外加归属面（uid
    /// 注入视角：目录真实 owner=当前用户，校验视角换成他者 uid ⇒ 拒绝）与正向锚
    /// （真实 owner 视角放行）。
    #[cfg(unix)]
    #[test]
    fn hc_env_secret_unsafe_dir_refused_zero_files() {
        use std::os::unix::fs::PermissionsExt;
        // ① 组可写（0o730：组 rwx）
        let d1 = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d1.path(), std::fs::Permissions::from_mode(0o730)).unwrap();
        let s1 = EnvSecret {
            path: d1.path().join("resume-env-1.sh"),
            password: "HCpw".to_string(),
        };
        let e1 = materialize_env_secret(&s1).expect_err("组可写目录必须拒绝");
        assert!(e1.starts_with(ENV_SECRET_DIR_UNSAFE), "结构化哨兵：{e1}");
        assert_eq!(dir_entry_count(d1.path()), 0, "零文件产生");
        // ② 他者可写（0o707：other rwx）
        let d2 = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d2.path(), std::fs::Permissions::from_mode(0o707)).unwrap();
        let s2 = EnvSecret {
            path: d2.path().join("resume-env-2.sh"),
            password: "HCpw".to_string(),
        };
        let e2 = materialize_env_secret(&s2).expect_err("他者可写目录必须拒绝");
        assert!(e2.starts_with(ENV_SECRET_DIR_UNSAFE), "结构化哨兵：{e2}");
        assert_eq!(dir_entry_count(d2.path()), 0, "零文件产生");
        // ③ 目录本体是符号链接（指向干净真目录）⇒ 拒绝，真目录零文件
        let d3 = tempfile::tempdir().unwrap();
        let real = d3.path().join("real-dir");
        std::fs::create_dir_all(&real).unwrap();
        let link = d3.path().join("link-dir");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let s3 = EnvSecret {
            path: link.join("resume-env-3.sh"),
            password: "HCpw".to_string(),
        };
        let e3 = materialize_env_secret(&s3).expect_err("符号链接目录必须拒绝");
        assert!(e3.starts_with(ENV_SECRET_DIR_UNSAFE), "结构化哨兵：{e3}");
        assert_eq!(dir_entry_count(&real), 0, "符号链接目标目录零文件");
        // ④ 路径 `..` 逃逸（父目录干净也不能让 `..` 分量过关）
        let d4 = tempfile::tempdir().unwrap();
        let s4 = EnvSecret {
            path: d4.path().join("sub/../resume-env-x.sh"),
            password: "HCpw".to_string(),
        };
        let e4 = materialize_env_secret(&s4).expect_err("`..` 逃逸路径必须拒绝");
        assert!(e4.starts_with(ENV_SECRET_PATH_UNSAFE), "结构化哨兵：{e4}");
        assert_eq!(dir_entry_count(d4.path()), 0, "零文件产生");
        // ⑤ 归属面（uid 注入）：真实 owner = 当前用户，校验视角 = 他者 uid ⇒ 拒绝；
        //    真实 owner 视角 ⇒ 放行（校验器本身可注入测试）
        let d5 = tempfile::tempdir().unwrap();
        let me = current_uid();
        let err_other = validate_resume_env_dir(d5.path(), me.wrapping_add(1))
            .expect_err("他者归属视角必须拒绝");
        assert!(
            err_other.starts_with(ENV_SECRET_DIR_UNSAFE),
            "结构化哨兵：{err_other}"
        );
        validate_resume_env_dir(d5.path(), me)
            .expect("当前用户归属的干净目录（tempdir 0700）必须放行");
    }

    /// RED c（brief HC c）：创建成功后写入中断（writer 缝注入失败）⇒ 半成品被清理，
    /// 目录零残留（凭据正文零落盘面）。
    #[cfg(unix)]
    #[test]
    fn hc_env_secret_write_failure_cleans_half_made_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret = EnvSecret {
            path: dir.path().join("resume-env-w.sh"),
            password: "HCpw".to_string(),
        };
        let err = materialize_env_secret_with_writer(&secret, &|_: &mut std::fs::File, _: &str| {
            Err("注入写失败".to_string())
        })
        .expect_err("注入写失败必须结构化失败");
        assert!(err.contains("注入写失败"), "错误必须携带写失败原因：{err}");
        assert!(
            !secret.path.exists(),
            "半成品文件必须被清理：{}",
            secret.path.display()
        );
        assert_eq!(dir_entry_count(dir.path()), 0, "目录零残留");
    }

    /// RED d-1（brief HC d）：交付处置分类——Ok = 载荷自删（不得提前删，窗口异步
    /// source 中）；确定性失败（启动失败/执行失败/TCC 拒绝）= 立即删；交付未知
    /// （超时/等待失败，Apple 事件可能已送达）= 保留至观察窗结算 + 24h 清扫兜底。
    #[test]
    fn hc_delivery_disposition_classifies_unknown_vs_definite() {
        let r = |s: &str| disposition_for_osascript_result(&Err(s.to_string()));
        assert_eq!(
            disposition_for_osascript_result(&Ok(())),
            EnvSecretDeliveryDisposition::PayloadOwnsDeletion,
            "交付 Ok：载荷 rm 自删，交付方不提前删"
        );
        assert_eq!(
            r("osascript 执行超时（10s 上限，已终止）——请重试；若持续失败检查 macOS 自动化授权"),
            EnvSecretDeliveryDisposition::RetainForSettle,
            "超时 = 交付未知（事件可能已送达）：保留至结算"
        );
        assert_eq!(
            r("osascript 等待失败：broken pipe"),
            EnvSecretDeliveryDisposition::RetainForSettle,
            "等待失败 = 交付未知：保留至结算"
        );
        assert_eq!(
            r("osascript 启动失败：no such file"),
            EnvSecretDeliveryDisposition::DeleteNow,
            "启动失败 = 请求未达 Terminal：立即删"
        );
        assert_eq!(
            r("osascript 执行失败（退出码 1）"),
            EnvSecretDeliveryDisposition::DeleteNow,
            "非零退出 = 请求未达 Terminal：立即删"
        );
        assert_eq!(
            r("execution error: 未获授权（TCC 拒绝）——请检查系统设置"),
            EnvSecretDeliveryDisposition::DeleteNow,
            "TCC 拒绝 = 请求未达 Terminal：立即删"
        );
    }

    /// RED d-1 接线（brief HC b+d）：macOS 交付缝——① 目录毒化 ⇒ 结构化 Err 且
    /// osascript runner **零调用**（零起窗）、目录零文件；② 超时（未知）⇒ 文件
    /// 保留且保持 0600；③ 确定性失败 ⇒ 文件立即删；④ Ok ⇒ 文件保留（载荷自删，
    /// 交付方不提前删）。runner = 记录型假体（零真实 osascript）。
    #[cfg(unix)]
    #[test]
    fn hc_delivery_wiring_zero_window_and_disposition_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        // ① 毒化目录（组可写）→ 拒绝 + 零起窗
        let d1 = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d1.path(), std::fs::Permissions::from_mode(0o730)).unwrap();
        let s1 = EnvSecret {
            path: d1.path().join("resume-env-d1.sh"),
            password: "HCpw".to_string(),
        };
        let calls = std::cell::Cell::new(0usize);
        let runner_counts = |_script: &str| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let r1 = deliver_macos_with_env_secret("return 0", Some(&s1), &runner_counts);
        let err1 = r1.expect_err("毒化目录必须拒绝交付");
        assert!(
            err1.starts_with(ENV_SECRET_DIR_UNSAFE),
            "结构化哨兵：{err1}"
        );
        assert_eq!(calls.get(), 0, "零起窗：校验失败在 osascript 出手前短路");
        assert_eq!(dir_entry_count(d1.path()), 0, "零文件产生");
        // ② 超时（交付未知）→ 文件保留 + 0600
        let d2 = tempfile::tempdir().unwrap();
        let s2 = EnvSecret {
            path: d2.path().join("resume-env-d2.sh"),
            password: "HCpw".to_string(),
        };
        let r2 = deliver_macos_with_env_secret("return 0", Some(&s2), &|_| {
            Err("osascript 执行超时（10s 上限，已终止）——请重试".to_string())
        });
        assert!(r2.is_err(), "超时按既有契约回执失败");
        assert!(s2.path.exists(), "交付未知：文件保留至观察窗结算");
        assert_eq!(
            std::fs::metadata(&s2.path).unwrap().permissions().mode() & 0o777,
            0o600,
            "留存期文件必须保持 0600（不留权限过宽文件）"
        );
        // ③ 确定性失败 → 立即删（新路径：retained 文件在是冲突面，重投天然换 token）
        let d3 = tempfile::tempdir().unwrap();
        let s3 = EnvSecret {
            path: d3.path().join("resume-env-d3.sh"),
            password: "HCpw".to_string(),
        };
        let r3 = deliver_macos_with_env_secret("return 0", Some(&s3), &|_| {
            Err("osascript 执行失败（退出码 1）".to_string())
        });
        assert!(r3.is_err());
        assert!(!s3.path.exists(), "确定性失败：文件立即删除");
        assert_eq!(dir_entry_count(d3.path()), 0, "目录零残留");
        // ④ Ok → 文件保留（载荷 `rm -f` source 后自删；交付方不得提前删——窗口异步 source）
        let d4 = tempfile::tempdir().unwrap();
        let s4 = EnvSecret {
            path: d4.path().join("resume-env-d4.sh"),
            password: "HCpw".to_string(),
        };
        deliver_macos_with_env_secret("return 0", Some(&s4), &|_| Ok(())).expect("交付成功");
        assert!(
            s4.path.exists(),
            "交付 Ok：文件由载荷 rm 自删，交付方不得提前删"
        );
    }

    /// HC 白名单锚（裁定 3：预期目录 = `<cache>/tuvis/resume-env/`）：生产受控入口
    /// writer 只出白名单目录内路径（GB 现势锁定）+ 文件名形状 + 密码内存态携带。
    #[test]
    fn hc_env_secret_writer_production_pins_whitelist_dir() {
        let s = env_secret_writer_production("HCpw").expect("生产 writer 必须可构造");
        let dir = resume_env_dir().expect("缓存目录必须可解析");
        assert_eq!(
            s.path.parent().map(|p| p.to_path_buf()),
            Some(dir.clone()),
            "生产受控入口路径必须落在白名单目录内"
        );
        assert!(
            dir.ends_with(std::path::Path::new("tuvis").join("resume-env")),
            "白名单目录 = <cache>/tuvis/resume-env/（GB 现势）：{}",
            dir.display()
        );
        let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.starts_with("resume-env-") && name.ends_with(".sh"),
            "文件名形状 resume-env-*.sh：{name}"
        );
        assert_eq!(
            s.password, "HCpw",
            "密码内存态随载荷携带（Debug 脱敏面不变）"
        );
    }

    /// RED e（brief HC e）：服务身份校验失败 ⇒ 零起窗之外，**零受控入口生成 +
    /// 零文件**（env_secret_writer 缝零调用——校验在凭据面出手前短路）。
    #[cfg(unix)]
    #[test]
    fn hc_identity_refusal_zero_secret_writer_invocation_and_zero_files() {
        let home = tempfile::tempdir().unwrap();
        write_exec_stub(
            home.path(),
            ".opencode/bin/opencode",
            &version_stub_body("2.2.5"),
        );
        let state_dir = home.path().join(".local/state/opencode");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("service.json"),
            br#"{"id":"hc-reg-dead","version":"2.2.5","url":"http://127.0.0.1:63644","pid":99,"password":"HCpw"}"#,
        )
        .unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let invoked_for_writer = invoked.clone();
        let dir_for_writer = secret_dir.path().to_path_buf();
        let fact = |_pid: u32| -> Option<i64> { None }; // 登记进程死
        let sources = ResumeSources {
            read_state_file: leak_box(fs_reader()),
            process_fact: leak_box(fact),
            env_secret_writer: leak_box(move |pw: &str| {
                invoked_for_writer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(EnvSecret {
                    path: dir_for_writer.join(env_secret_token()),
                    password: pw.to_string(),
                })
            }),
        };
        let r = prepare_opencode_resume_with_sources(
            Some(home.path()),
            None,
            None,
            Some("/usr/bin"),
            None,
            "ses_hc_e1",
            &sources,
        );
        let err = r.expect_err("登记进程死必须拒绝");
        assert!(
            err.starts_with(OC_RESUME_REGISTRATION_UNVERIFIED),
            "结构化哨兵：{err}"
        );
        assert_eq!(
            invoked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "服务身份校验失败：受控入口生成缝零调用"
        );
        assert_eq!(dir_entry_count(secret_dir.path()), 0, "零文件产生");
    }

    /// GREEN 行为锚（GB 现势保持）：目录缺失 ⇒ 创建 + 收紧 0700 + 文件 0600 落盘
    /// 成功（生产首跑形态——加固不得破坏可用性）。
    #[cfg(unix)]
    #[test]
    fn hc_env_secret_creates_missing_dir_tightened_0700() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("tuvis").join("resume-env");
        let secret = EnvSecret {
            path: nested.join("resume-env-new.sh"),
            password: "HCpw".to_string(),
        };
        materialize_env_secret(&secret).expect("目录缺失场景必须自建并落盘成功");
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700,
            "自建目录必须收紧 0700"
        );
        assert_eq!(
            std::fs::metadata(&secret.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "env 文件必须 0600"
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
