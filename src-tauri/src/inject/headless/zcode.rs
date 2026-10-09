//! H7 zcode 无头通道（Task 8）：**只做 zcode 特有的事**——命令形态（argv/env 平台分叉）、
//! 退出分类（工作区争用锁）、工作区活跃探活、信任可见性、以及把 Task 6 底座串成单回合
//! 编排（并发名额/看门狗/取消/kill 树/回执归一全在 [`super::runner`] / [`super::receipt`]，
//! 本模块**不重复实现**）。
//!
//! # 命令形态（spec H7 双平台实测，逐字）
//! - Windows：`ELECTRON_RUN_AS_NODE=1 <ZCode.exe> <install>/resources/glm/zcode.cjs
//!   --prompt "<text> [mobile <name>]" --resume <sess> --cwd <project> --mode yolo --json`
//! - macOS：同骨架，**必须**追加 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=<Resources>/config/
//!   provider/zcode-builtin.json`（打包布局 bug，反编译实证：不设则 `--prompt` **静默
//!   无 JSON**）——路径推导复用 [`super::gate::zcode_provider_config_path`]（单一来源）。
//! - `--mode yolo` 固定（裁决 14）：`build` 档在无 permission client 时**阻断全部工具
//!   执行**（`No permission client configured for Bash`），可用性由 H3 总开关（默认关）
//!   + 开启安全说明承担。**档位旗子来自 [`super::PermissionSpec::zcode_default`]**（H5
//!   参数面，Task 10）——本模块不再写字面 `yolo`。**zcode 无头没有审批面**：无头 argv 只
//!   表达档位，审批/许可交互只在 APP 内；H3 开启知情文案是这条边界的兜底控制。
//! - stdout 前缀污染（Mac 实测 `ZCode Built-in missing/skipped (not-due)`）由 Task 6 的
//!   [`super::receipt::FrameAccumulator`] 前缀跳过消化——本模块**不另写一份解析**。
//!
//! # APP × 无头并发 = 工作区级**争用型**瞬时锁（Mac 变量排除法实证）
//! APP 正在该工作区活动时，无头回合约 1s 退出、stdout 打 `Model creation failed` 且
//! **退出码为 0**（！）——按退出码先判会把它当成功（假成功）。故 [`classify_exit`] 先判
//! 争用特征串、再判退出码，命中即 [`Stage::WorkspaceBusy`] + 可重试；重试节奏与上限由
//! [`busy_step`] 裁决（探活：APP 不在该工作区了就不必空等）。
//!
//! # 可见性（两端定案）
//! 项目在 APP 信任列表（`~/.zcode/v2/setting.json` 的 `recentProjects`，**只读**，元素是
//! **路径字符串**）⇒ 重启 ZCode 应用后可见；否则仅兔维斯可见。映射**复用 Task 7** 的
//! [`crate::inject::routing::zcode_visibility`] 与 [`Visibility::note`]，不另造第二份。
//!
//! # 会话串行锁
//! zcode 无头自身不拒绝并发，同一会话两回合重叠会让两个进程交错写同一个库——**由 MAM
//! 串行**（[`TurnRegistry`]）：同会话第二次请求**如实拒绝**（不排队、不覆盖），取消端点经
//! 同一登记表找靶子（Task 6 的 [`CancelHandle`] 被包成 [`CancelFn`] 以便测试注入）。
//!
//! # 测试纪律（宪法级）
//! 单测**绝不** spawn 真 ZCode（真实账号配额 + 真实 `~/.zcode` 写入）：安装路径发现与
//! 进程表扫描在 `cfg(test)` 下恒为空表（见 [`production_roots`] / [`well_known_roots`]），
//! 回合编排经注入的 `make_runner`/`run` 缝驱动 Task 6 的 `run_once` 脚本缝，信任探针走
//! tempdir 夹具。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
// `Mutex` 只在测试面用（脚本桩构造）：生产码的登记表内部锁归 `turn::TurnRegistry`
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::gate::provider_config_env;
use super::receipt::{FrameAccumulator, Receipt, ReceiptStatus, Stage};
use super::runner::RunnerCfg;
// H5 权限档参数面（Task 10）：`--mode` 档位从它取——**单一来源**，勿在本模块另写字面
use super::PermissionSpec;
use crate::inject::normalize;
use crate::inject::routing::{zcode_visibility, Visibility};

/// Electron「以自身作 node 运行」开关（两端同款；不设 = ZCode.exe 会尝试开一个窗口）
pub const ELECTRON_RUN_AS_NODE: &str = "ELECTRON_RUN_AS_NODE";
/// 工作区争用锁的**实测特征串**（探测定案）：APP 活跃于该工作区时无头回合 ~1s 退出并打印它
pub const WORKSPACE_BUSY_MARKER: &str = "Model creation failed";
/// busy 重试上限（**重试次数**；总尝试 ≤ 3。spec H7「retry ≤2」）
pub const BUSY_MAX_RETRIES: usize = 2;
/// busy 重试间隔（spec H7：5s）
pub const BUSY_RETRY_SPACING_MS: u64 = 5_000;
/// 斜杠命令拒绝文案（spec H7：slash 命令是**字面文本**、与 APP 内命令不具等价性——
/// 显式告知而不是静默透传冒充支持）
pub const SLASH_REFUSAL: &str =
    "斜杠命令在无头通道不可用（无头 CLI 把 `/…` 当普通文本，不具 ZCode 应用内命令语义）——请在 ZCode 应用内执行";

// ============================================================
// 命令形态
// ============================================================

/// ZCode 安装形态（exe + 会话运行时脚本 cjs；os 取自 `std::env::consts::OS`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeSpec {
    pub exe: String,
    pub cjs: String,
    pub os: &'static str,
}

impl ZcodeSpec {
    /// Windows 安装根（如 `D:/Program Files/ZCode`）：exe = `<root>/ZCode.exe`，
    /// cjs = `<root>/resources/glm/zcode.cjs`
    pub fn win(root: &str) -> Self {
        let root = PathBuf::from(root);
        Self {
            exe: root.join("ZCode.exe").to_string_lossy().to_string(),
            cjs: root
                .join("resources")
                .join("glm")
                .join("zcode.cjs")
                .to_string_lossy()
                .to_string(),
            os: "windows",
        }
    }

    /// macOS 包形态（如 `/Applications/ZCode.app`）：exe = `<bundle>/Contents/MacOS/ZCode`，
    /// cjs = `<bundle>/Contents/Resources/glm/zcode.cjs`
    pub fn mac(bundle: &str) -> Self {
        let bundle = PathBuf::from(bundle);
        Self {
            exe: bundle
                .join("Contents")
                .join("MacOS")
                .join("ZCode")
                .to_string_lossy()
                .to_string(),
            cjs: bundle
                .join("Contents")
                .join("Resources")
                .join("glm")
                .join("zcode.cjs")
                .to_string_lossy()
                .to_string(),
            os: "macos",
        }
    }

    pub fn is_mac(&self) -> bool {
        self.os == "macos"
    }
}

/// 一次回合的命令形态（program + argv + env + 最终 prompt 载荷）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeInvocation {
    /// spawn 的 program（ZCode.exe / ZCode）
    pub program: String,
    /// argv（**不含 program 自身**；首元素是 cjs 脚本路径）
    pub argv: Vec<String>,
    /// env 集（平台分叉在此体现）
    pub env: BTreeMap<String, String>,
    /// 最终 `--prompt` 载荷（W4 归一 + 签名产物）——审计 content 与回执摘要同源
    pub prompt: String,
}

impl std::ops::Deref for ZcodeInvocation {
    type Target = [String];
    fn deref(&self) -> &Self::Target {
        &self.argv
    }
}

/// 回合 flag 表（spec H7 命令形态逐字序：`--prompt <text> [--resume <sess>] --cwd <proj>
/// <权限档旗子> --json`）。
///
/// `perm` = **H5 权限档旗子**（唯一来源 [`PermissionSpec::zcode_default`]——本函数与调用方
/// 都不再写字面 `yolo`）：按实测序插在 `--cwd <项目>` 之后、`--json` 之前。
/// zcode **没有审批面**（裁决 14）：这里只表达档位，审批/许可交互只存在于 APP 内，
/// 安全面由 H3 总开关（默认关）+ 开启知情文案承担。
///
/// `resume = None` 即 **H10 无头新建**形态（无在册会话可续）——本批（Task 8）不接线，
/// 形态先留好：新增调用点只需换传 `None`，不需要动 flag 序。
fn turn_flags(
    resume: Option<&str>,
    project: &str,
    prompt: &str,
    cjs: &str,
    perm: &[&str],
) -> Vec<String> {
    let mut argv = vec![cjs.to_string(), "--prompt".into(), prompt.to_string()];
    if let Some(id) = resume {
        argv.push("--resume".into());
        argv.push(id.to_string());
    }
    argv.push("--cwd".into());
    argv.push(project.to_string());
    // H5 权限档（Task 10）：档位旗子来自 spec，**不是**本文件里的字面量
    argv.extend(perm.iter().map(|f| (*f).to_string()));
    argv.push("--json".into());
    argv
}

/// 构造一次回合的命令形态。
///
/// `device_name = Some(名)` ⇒ 正文经 **W4 单点** [`normalize::compose_injection`]（换行归一
/// 成字面 `\n` + 尾部 ` [mobile 名]` 签名——与终端注入同一条组装出口，不另造一份）；
/// `None` = 调用方已自行拼好签名/裸文本（原样上送，不二次签名）。
///
/// **`Result`（裁决 24b）**：花名过白名单判据（`turn::device_name_refusal`）失败时
/// [`normalize::compose_injection`] 回 `Err(原因)`——本函数**原样上抛**，调用方按
/// `refused` 档零字节投递（**不吞、不降级成无签名载荷**：那会把「花名不安全」静默变成
/// 「这条消息没有来源标注」）。
pub fn build_argv(
    spec: &ZcodeSpec,
    text: &str,
    session_id: &str,
    project: &str,
    device_name: Option<&str>,
) -> Result<ZcodeInvocation, String> {
    build_with(Some(session_id), spec, text, project, device_name)
}

/// 命令形态组装内核（**唯一**组装出口；`resume` 是它与 [`build_create_argv`] 的唯一差别）：
/// `device_name = Some(名)` ⇒ 正文经 **W4 单点** [`normalize::compose_injection`]（换行归一
/// 成字面 `\n` + 尾部 ` [mobile 名]` 签名——与终端注入同一条组装出口，不另造一份）；
/// `None` = 调用方已自行拼好签名/裸文本（原样上送，不二次签名）。
///
/// 花名判据不在这里（也不该在任何 argv 构造器里）：门设在共享组装单点
/// [`normalize::compose_injection`] 内，本函数只是 `?` 上抛——**通道无从绕开**。
fn build_with(
    resume: Option<&str>,
    spec: &ZcodeSpec,
    text: &str,
    project: &str,
    device_name: Option<&str>,
) -> Result<ZcodeInvocation, String> {
    let prompt = match device_name {
        Some(name) => normalize::compose_injection(name, text)?,
        None => text.to_string(),
    };
    let mut env = BTreeMap::new();
    // Electron 以自身作 node 跑 cjs：两端都要（不设则起 GUI 窗口）
    env.insert(ELECTRON_RUN_AS_NODE.to_string(), "1".to_string());
    // provider config：**两端共用单点**（[`super::gate::provider_config_env`]——探针与真回合
    // 必须同一份口径，否则「探针过了、回合却报定位不到」）。Windows 亦适用：Task 8 真机实证
    // 装包把文件放在 `<resources>/config/provider/`，而 CLI 自身的查找表里没有该位置。
    if let Some((k, v)) = provider_config_env(&spec.cjs, spec.os) {
        env.insert(k, v);
    }
    Ok(ZcodeInvocation {
        program: spec.exe.clone(),
        // H5：档位旗子从 spec 取（单一来源）——`--mode yolo` 不再由本模块字面直写
        argv: turn_flags(
            resume,
            project,
            &prompt,
            &spec.cjs,
            &PermissionSpec::zcode_default().flags(),
        ),
        env,
        prompt,
    })
}

/// **H10 无头新建形态**（Task 12）：`resume = None` —— 无在册会话可续，argv 里**没有**
/// `--resume`（spec H10 逐字形态）。
///
/// flag 序与 [`build_argv`] **同源**（同一个 [`turn_flags`]），本函数只把 resume 位显式
/// 留空——**不另写第二个 argv 构造器**（Task 8 的 `turn_flags` 文档早已预留本形态）。
/// 花名判据同 [`build_argv`]（`Result` 的来由见其文档；判据本体在共享组装单点内）。
pub fn build_create_argv(
    spec: &ZcodeSpec,
    text: &str,
    project: &str,
    device_name: Option<&str>,
) -> Result<ZcodeInvocation, String> {
    build_with(None, spec, text, project, device_name)
}

/// **H10 新建的项目级串行锁键**（Task 12）：项目路径按**工作区比对口径**归一
/// （复用 [`norm_path`]——与 [`same_workspace`] 同源：分隔符归一 + 去尾分隔 +
/// Windows 大小写折叠），前缀 `create|` 与真实会话号的命名空间隔离（锁表是同一张
/// [`TurnRegistry`]，键必须不可能撞上 `sess_…`）。
pub fn create_lock_key(project: &str, os: &str) -> String {
    format!("create|{}", norm_path(project, os))
}

/// 斜杠命令的**显式拒绝**（spec H7；`None` = 普通消息可发）。
/// 判据复用 [`normalize::is_slash_message`]（与终端路径同一份判据，不另立）。
pub fn slash_refusal(text: &str) -> Option<&'static str> {
    normalize::is_slash_message(text).then_some(SLASH_REFUSAL)
}

// ============================================================
// 退出分类（争用锁）
// ============================================================

/// 进程退出观测（判定输入；`*_head` 是证据串——生产传 stdout/stderr 前
/// [`EVIDENCE_HEAD_LINES`] 行，判定为子串匹配）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitObs<'a> {
    pub code: i32,
    pub stderr_head: &'a str,
    pub stdout_head: &'a str,
    pub duration_ms: u64,
}

/// 退出分类结论。
///
/// `stage` 语义（**勿混用**）：`Some(WorkspaceBusy)` = 争用锁（**必须覆盖回执**——该形态
/// 退出码是 0，runner 会归成 ok，按退出码先判即假成功）；`Some(Crash)` = 真失败（runner
/// 回执已是 Crash，**不覆盖**，只为「不重试」判定）；`None` = 正常退出（沿用 runner 回执）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitClass {
    pub stage: Option<Stage>,
    /// 是否属**争用型**（重试有效）——只有 WorkspaceBusy 为 true
    pub retryable: bool,
    /// 争用证据片段（busy 时的诚实原因；其他档为 None，原因由 runner 回执自带）
    pub evidence: Option<String>,
}

fn busy_hit(obs: &ExitObs<'_>) -> bool {
    let needle = WORKSPACE_BUSY_MARKER.to_lowercase();
    obs.stdout_head.to_lowercase().contains(&needle)
        || obs.stderr_head.to_lowercase().contains(&needle)
}

/// 退出分类（纯核，探测定案口径）：
/// 1. **先判争用**（特征串命中，无论退出码）→ [`Stage::WorkspaceBusy`] + `retryable`；
/// 2. 退出码非 0 → [`Stage::Crash`]（H4：不自动重试）；
/// 3. 退出码 0 且无争用串 → `None`（无覆盖）。
pub fn classify_exit(obs: &ExitObs<'_>) -> ExitClass {
    if busy_hit(obs) {
        return ExitClass {
            stage: Some(Stage::WorkspaceBusy),
            retryable: true,
            evidence: Some(format!(
                "{WORKSPACE_BUSY_MARKER}（{}ms 退出；ZCode 应用在该工作区活跃，争用型拒绝）",
                obs.duration_ms
            )),
        };
    }
    if obs.code != 0 {
        return ExitClass {
            stage: Some(Stage::Crash),
            retryable: false,
            evidence: None,
        };
    }
    ExitClass {
        stage: None,
        retryable: false,
        evidence: None,
    }
}

/// 争用锁重试决策（纯核）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyStep {
    /// 重试次数用尽 → 如实失败
    GiveUp,
    /// APP 已不在该工作区 → 争用锁应已释放，立即重试（不空等）
    RetryNow,
    /// APP 仍在该工作区活跃 → 等间隔后重试
    RetryAfterWait(Duration),
}

/// `attempts_done` = 已完成的尝试次数（1 起）；`max_retries` = **重试**上限。
pub fn busy_step(
    attempts_done: usize,
    app_active: bool,
    max_retries: usize,
    spacing: Duration,
) -> BusyStep {
    if attempts_done > max_retries {
        BusyStep::GiveUp
    } else if app_active {
        BusyStep::RetryAfterWait(spacing)
    } else {
        BusyStep::RetryNow
    }
}

// ============================================================
// 工作区活跃探活（探测定案口径：按 app-server 进程 cwd 扫描）
// ============================================================

/// 路径归一（**比对用**）：分隔符统一 `/` + 去尾分隔；Windows 侧再小写（NTFS 默认
/// 大小写不敏感；macOS/Linux 保持原样比较，不做「猜测式」折叠）。
fn norm_path(p: &str, os: &str) -> String {
    let s = p.trim().trim_end_matches(['/', '\\']).replace('\\', "/");
    if os == "windows" {
        s.to_lowercase()
    } else {
        s
    }
}

/// 两个路径是否同一工作区（相等或互为子树；**分隔符边界严判**——`E:/proj` 不得命中
/// `E:/proj2`）。
pub fn same_workspace(a: &str, b: &str, os: &str) -> bool {
    let (a, b) = (norm_path(a, os), norm_path(b, os));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// 工作区活跃判定（纯核）：给定「ZCode 会话运行时（app-server）进程 cwd 表」，
/// 是否有进程正停在该工作区。
pub fn workspace_active_in(project: &str, runtime_cwds: &[String], os: &str) -> bool {
    runtime_cwds
        .iter()
        .any(|cwd| same_workspace(cwd, project, os))
}

/// app-server（会话运行时）判定：exe 命中 ZCode（单源
/// [`crate::monitor::zcode_parser::exe_basename_is_zcode`]）**且**命令行含 `zcode.cjs`。
/// Electron 辅助进程（`--type=`）刻意排除——它们的 cwd 是应用目录，不代表某工作区活跃。
pub fn is_app_server(exe: Option<&Path>, cmd: &[std::ffi::OsString]) -> bool {
    let exe_hit = exe
        .map(|e| crate::monitor::zcode_parser::exe_basename_is_zcode(&e.to_string_lossy()))
        .unwrap_or(false);
    exe_hit
        && cmd.iter().any(|a| {
            a.to_string_lossy()
                .to_lowercase()
                .replace('\\', "/")
                .contains("zcode.cjs")
        })
}

/// 生产收集：ZCode 会话运行时（app-server）进程的 cwd 表（只读进程表，零注入）。
pub fn zcode_runtime_cwds() -> Vec<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter(|p| is_app_server(p.exe(), p.cmd()))
        .filter_map(|p| p.cwd().map(|c| c.to_string_lossy().to_string()))
        .collect()
}

// ============================================================
// 信任可见性（~/.zcode/v2/setting.json 的 recentProjects，只读）
// ============================================================

/// ZCode 信任工作区表（**只读**）。任何读/解析失败一律回空表 = **未信任**（保守：
/// 宁可少承诺「重启后可见」，绝不谎报）。文件首字节可能带 BOM（真机实测）——先剥。
pub fn trusted_projects(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let path = crate::monitor::zcode_parser::zcode_home_with(home)
        .join("v2")
        .join("setting.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let raw = raw.trim_start_matches('\u{feff}');
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    v.get("recentProjects")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 项目是否在信任表内（同 [`same_workspace`] 的路径口径）
pub fn project_trusted(project: &str, trusted: &[String], os: &str) -> bool {
    trusted.iter().any(|p| same_workspace(p, project, os))
}

/// 可见性档：**复用 Task 7 的单点映射** [`zcode_visibility`]（未信任 ⇒ 仅兔维斯可见）
pub fn visibility_of(project: &str, home: Option<&Path>, os: &str) -> Visibility {
    zcode_visibility(project_trusted(project, &trusted_projects(home), os))
}

// ============================================================
// 安装路径发现
// ============================================================

/// 从宿主可执行路径反推安装根：Windows = exe 所在目录；macOS = 最内层 `.app` 包
/// （取最内层 `.app/` 段，与 macOS 构建下的
/// [`crate::window::app_activation::app_bundle_from_exe`] 同口径；本函数**跨平台实现**
/// ——D1 平台分叉必须在非 macOS 构建上也可测，故不依赖 macOS-only 模块）。
pub fn root_from_exe(exe: &str, os: &str) -> Option<String> {
    let normalized = exe.replace('\\', "/");
    if os == "macos" {
        if let Some(idx) = normalized.rfind(".app/") {
            return Some(normalized[..idx + ".app".len()].to_string());
        }
    }
    Path::new(exe)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
}

/// 安装根候选表（**纯核**）：宿主进程证据优先（唯一能定位非默认安装位置的证据），
/// 其次常见安装路径；去重保序。
pub fn candidate_roots(host_exe: Option<&str>, well_known: &[String], os: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(exe) = host_exe {
        // 只在 exe 确属 ZCode 时才反推（防把任意会话 pid 的宿主目录当安装根）
        if crate::monitor::zcode_parser::exe_basename_is_zcode(exe) {
            if let Some(root) = root_from_exe(exe, os) {
                out.push(root);
            }
        }
    }
    for r in well_known {
        if !out.iter().any(|x| same_workspace(x, r, os)) {
            out.push(r.clone());
        }
    }
    out
}

/// 常见安装路径（Windows：Program Files / LOCALAPPDATA / 各固定盘 `Program Files`；
/// macOS：`/Applications/ZCode.app`）。**`cfg(not(test))`**：测试构建不咨询真机安装路径
/// （理由见 [`production_roots`]），故本函数在测试构建里不参与编译。
#[cfg(not(test))]
fn well_known_roots(os: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if os == "macos" {
        out.push("/Applications/ZCode.app".to_string());
        return out;
    }
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                out.push(Path::new(&v).join("ZCode").to_string_lossy().to_string());
            }
        }
    }
    if let Ok(v) = std::env::var("LOCALAPPDATA") {
        if !v.trim().is_empty() {
            out.push(
                Path::new(&v)
                    .join("Programs")
                    .join("ZCode")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    // 非默认盘符安装（真机探测实证：D:\Program Files\ZCode）：逐盘探 cjs 存在性——
    // 一次 stat/盘，代价可忽略；不存在即跳过。
    for letter in b'C'..=b'Z' {
        let root = format!("{}:\\Program Files\\ZCode", letter as char);
        let cjs = Path::new(&root)
            .join("resources")
            .join("glm")
            .join("zcode.cjs");
        if cjs.is_file() {
            out.push(root);
        }
    }
    out
}

/// 生产候选表（宿主 pid → exe，只做**定向**进程刷新，不全量扫描）。
///
/// **`cfg(test)` 恒空表**（宪法级测试纪律）：本机真装了 ZCode（真机事实），若单测也去
/// 咨询真机安装路径/进程表，`session-send` 的端点用例就会**真的 spawn 一个真实回合**
/// ——消耗用户真实账号配额并写真实 `~/.zcode`。故测试构建一律「找不到安装」，
/// 端点用例断言的是**如实的安装不可达失败**；安装发现的纯核
/// （[`candidate_roots`] / [`root_from_exe`] / [`resolve_spec`]）用注入表覆盖。
pub fn production_roots(host_pid: u32) -> Vec<String> {
    #[cfg(test)]
    {
        let _ = host_pid;
        Vec::new()
    }
    #[cfg(not(test))]
    {
        let os = std::env::consts::OS;
        candidate_roots(host_exe_of(host_pid).as_deref(), &well_known_roots(os), os)
    }
}

/// 宿主 pid 的 exe（定向刷新：只问这一个 pid，不做全表扫描）。
/// **`cfg(not(test))`**：同 [`well_known_roots`] —— 测试构建零真实进程表接触
#[cfg(not(test))]
fn host_exe_of(pid: u32) -> Option<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    if pid == 0 {
        return None;
    }
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    sys.process(sysinfo::Pid::from_u32(pid))
        .and_then(|p| p.exe())
        .map(|e| e.to_string_lossy().to_string())
}

/// 候选表 → 首个**cjs 真在场**的形态（缺席即 None = 如实报「安装路径不可达」，
/// 绝不 spawn 一个不存在的程序）。
pub fn resolve_spec(roots: &[String], os: &str) -> Option<ZcodeSpec> {
    roots.iter().find_map(|root| {
        let spec = if os == "macos" {
            ZcodeSpec::mac(root)
        } else {
            ZcodeSpec::win(root)
        };
        Path::new(&spec.cjs).is_file().then_some(spec)
    })
}

// ============================================================
// 单回合编排（Task 6 底座的唯一消费入口）
// ============================================================
//
// **共享件已上提**（Task 9 复审）：执行缝（[`RunSeam`]/[`TurnObs`]/[`production_run_seam`]）、
// 回执→审计词（[`receipt_result_word`]）、会话串行锁 / 取消靶子登记表
// （[`registry`]/[`TurnSlot`]/[`CancelFn`]）、证据头（[`head_of`]）现在定义在
// [`super::turn`]——zcode 与 codex 共用底座，通道之间不互相依赖（WB 接入时同规）。
// 下方 `pub use` 是**兼容转出**：Task 8 既有调用面（`zcode::registry()` 等）保持不变。
use super::turn::head_of;
pub use super::turn::{
    production_run_seam, receipt_result_word, registry, BoxFuture, CancelFn, RunSeam, TurnObs,
    TurnRegistry, TurnSlot,
};

/// 编排依赖（探活/等待/回执源/重试节奏全可注入，测试零真实等待零真实进程表零真实会话库）
pub struct TurnDeps {
    pub max_busy_retries: usize,
    pub retry_spacing: Duration,
    /// 工作区活跃探针（生产 = [`zcode_runtime_cwds`] + [`workspace_active_in`]）
    pub workspace_active: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// 等待缝（生产 = `tokio::time::sleep`；测试 = 记录桩，零真实等待）
    pub wait: Box<dyn Fn(Duration) -> BoxFuture<()> + Send + Sync>,
    /// **会话库读缝（回执源）**：生产 = `zcode_parser::store_snapshot_home`（只读
    /// `~/.zcode/cli/db/db.sqlite`）；测试 = 脚本桩（**测试绝不读真实 ~/.zcode**）
    pub store_probe: Box<StoreProbe>,
    /// 库确认的有界轮询：读取次数（≥1）与间隔（懒落库/WAL 可见性可能滞后）
    pub store_confirm_attempts: usize,
    pub store_confirm_interval: Duration,
}

/// 库确认轮询的生产界（**有界**：3 次 × 1s = 最多 ~2s 额外等待；进程已退出，
/// 滞后量级为百毫秒，够用且不会把回执拖长）
pub const STORE_CONFIRM_ATTEMPTS: usize = 3;
pub const STORE_CONFIRM_INTERVAL_MS: u64 = 1_000;

impl TurnDeps {
    /// 生产依赖（探活走真进程表、回执源走真会话库只读、等待走 tokio 定时器）
    pub fn production(os: &'static str) -> Self {
        Self {
            max_busy_retries: BUSY_MAX_RETRIES,
            retry_spacing: Duration::from_millis(BUSY_RETRY_SPACING_MS),
            workspace_active: Box::new(move |project: &str| {
                workspace_active_in(project, &zcode_runtime_cwds(), os)
            }),
            wait: Box::new(|d: Duration| Box::pin(tokio::time::sleep(d))),
            store_probe: Box::new(|session_id: &str| {
                dirs::home_dir()
                    .and_then(|h| crate::monitor::zcode_parser::store_snapshot_home(&h, session_id))
            }),
            store_confirm_attempts: STORE_CONFIRM_ATTEMPTS,
            store_confirm_interval: Duration::from_millis(STORE_CONFIRM_INTERVAL_MS),
        }
    }
}

/// 回合结局（回执 + 尝试次数 + 回执来源）
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub receipt: Receipt,
    pub attempts: usize,
    pub busy_final: bool,
    /// 回执来源（`SessionStore` = 真机常态；`Unconfirmed` = 如实的不确认）
    pub receipt_source: ReceiptSource,
}

/// 帧统计（复用 Task 6 的前缀跳过累积器——**不另写解析**）
pub fn frames_of(stdout: &[String]) -> FrameAccumulator {
    let mut acc = FrameAccumulator::default();
    for line in stdout {
        acc.push_line(line);
    }
    acc
}

/// 回执帧（**两级**：逐行 → 整段）。
///
/// 逐行喂法只认「对象在一行内」的 JSON；而真机 CLI 的 `--json` 是**摘要**输出
/// （`wantsJsonSummary`，v0.16.9 反编译），**可能是 pretty-print 多行**——那时逐行喂法
/// 每行都不成对象（`{` / `"sessionId": …` / `}` 各自一行）→ 误判「无回执帧」。
/// 故逐行无命中时，用 Task 6 的 [`super::receipt::parse_json_skipping_prefix`] 对**整段**
/// stdout 重解析（该函数本身是流式跨行的），命中即把该对象规整成单行再喂累积器
/// （累积器只认单行；规整不改变字段语义）。
pub fn frames_of_stdout(stdout: &[String]) -> FrameAccumulator {
    let linewise = frames_of(stdout);
    if linewise.json_frames > 0 || linewise.unknown_frames > 0 {
        return linewise;
    }
    let raw = stdout.join("\n");
    match super::receipt::parse_json_skipping_prefix(&raw) {
        Some(v) => {
            // 整段解释成立时，逐行噪音计数失去意义（那些行其实是同一个对象的成员）——
            // 只喂规整后的单行对象：json_frames=1、noise_lines=0（诊断口径与整段解释一致）
            let mut acc = FrameAccumulator::default();
            acc.push_line(&v.to_string());
            acc
        }
        None => linewise,
    }
}

/// 「0 退出但无回执帧」的**如实**原因（含可诊断的证据片段；**不谎称「未产出结果」**
/// ——真机实证：回合可能已经跑完并写进会话库，MAM 只是没拿到 JSON 帧）。
/// `store` = 会话库确认结论（`None` = 本档不咨询库，如非 0 退出/取消）。
fn channel_error_reason(
    stdout: &[String],
    stderr: &str,
    store: Option<&StoreConfirmation>,
) -> String {
    let out = crate::inject::normalize::summarize(
        &head_of(stdout),
        crate::inject::normalize::AUDIT_SUMMARY_CHARS,
    );
    let err =
        crate::inject::normalize::summarize(stderr, crate::inject::normalize::AUDIT_SUMMARY_CHARS);
    let tail = match store {
        Some(StoreConfirmation::Unavailable) => {
            "会话库不可读，无法确认本轮结果——请在会话内容中确认后再决定是否重发"
        }
        _ => {
            "会话库里**没有**本轮新的 assistant 回复（回合可能被应用侧中断/吞掉）——\
             请在会话内容中确认后再决定是否重发"
        }
    };
    format!("进程正常退出（exit 0）但未拿到回执：stdout 无可解析 JSON 帧，且{tail}；stdout 头={out}；stderr={err}")
}

// ============================================================
// 回执源：会话库确认（H6 契约的真源；stdout 只作完成信号）
// ============================================================

/// 会话库快照（`monitor::zcode_parser` 的**只读**读取件产物；本模块只做前后对比）
pub use crate::monitor::zcode_parser::ZcodeStoreSnapshot as StoreSnapshot;

/// 会话库读缝（生产 = `zcode_parser::store_snapshot_home`；测试 = 脚本桩——
/// **测试绝不读真实 ~/.zcode**）
pub type StoreProbe = dyn Fn(&str) -> Option<StoreSnapshot> + Send + Sync;

/// 库确认结论（[`confirm_from_store`] 的产物）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreConfirmation {
    /// 库确认本轮写入了**新的 assistant 回复**（文本 + 可选 tokens）
    NewAssistant { text: String, tokens: Option<u64> },
    /// 库可读，但本轮**没有**新的 assistant 回复（如只落了用户消息）——如实不确认
    NoNewAssistant,
    /// 库读不到（不可确认）
    Unavailable,
}

/// 回合前后快照对比（纯核）。**确认判据**：库里存在 assistant 文本，且它相对基线是
/// **新的**——按消息 id 判（最稳：同一文本的两轮回复也能区分），id 不可得时退化为文本比较
/// （宁可漏确认，不可假确认）。序列前进只是必要背景（只落用户消息时序列同样前进，
/// 那时**不算**确认）。
pub fn confirm_from_store(before: &StoreSnapshot, now: &StoreSnapshot) -> StoreConfirmation {
    let Some(text) = now.last_assistant.clone() else {
        return StoreConfirmation::NoNewAssistant;
    };
    let same_message = match (&before.last_assistant_id, &now.last_assistant_id) {
        (Some(old), Some(new)) => old == new,
        _ => before.last_assistant.as_deref() == Some(text.as_str()),
    };
    if same_message {
        return StoreConfirmation::NoNewAssistant;
    }
    StoreConfirmation::NewAssistant {
        text,
        tokens: now.tokens,
    }
}

/// 回执来源（可观测面：测试与审计据此分辨「stdout JSON / 会话库 / 未确认」）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptSource {
    /// stdout 有可解析 JSON 帧（Task 6 原口径，第一优先）
    StdoutJson,
    /// 会话库确认（真机 `--resume` 回合的正常路径）
    SessionStore,
    /// 两者都没有 → 如实不确认（channel_error）
    Unconfirmed,
    /// 非 0 退出 / 超时 / 取消：库不参与判定
    NotApplicable,
}

/// 退避窗取消闩（**回合生命期**；R9）。
///
/// 争用锁拒绝后子进程已经退了，runner 的取消靶子随 `disarm` 消失——此刻
/// [`super::runner::CancelHandle::cancel`] 恒 `false`，退避窗内到达的取消请求会被静默
/// 吞掉（用户已叫停，回合却还在重试）。闩把两件事分开记：
/// - `requested`：登记表上到达过取消请求（退避醒来后据此收尾为 `cancelled`）；
/// - `live`：回合仍在 [`run_turn`] 内（**返回后迟到的取消不得冒领送达**——否则取消端点会
///   为一条已以别的终态收尾的回合落 `cancelled` 取消审计行，与回合自身的行口径矛盾），
///   由 [`LiveGuard`] 在任一出口落闩。
struct CancelLatch {
    requested: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
}

impl CancelLatch {
    fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicBool::new(true)),
        }
    }

    /// 逐尝试武装的取消靶子：**透传给 runner**（活体尝试才真能 kill 进程树）+ **闩住请求**。
    ///
    /// 返回值与 runner 同口径（`true` = 送达；先到者生效）：活体尝试交 runner 仲裁；
    /// 进程已退/尚未起跑时 runner 报 `false`，**但只要回合还在跑，这一发就是有效的**
    /// （退避醒来即收尾为 `cancelled`）——如实报「送达」，取消端点才会落取消审计行、
    /// 用户也才不会收到「回合已终结」的假话。
    fn target(&self, cfg: &RunnerCfg) -> CancelFn {
        let inner = super::turn::cancel_fn_of(cfg);
        let requested = self.requested.clone();
        let live = self.live.clone();
        Arc::new(move || {
            let first = !requested.swap(true, Ordering::SeqCst);
            let delivered = inner();
            delivered || (first && live.load(Ordering::SeqCst))
        })
    }

    /// 退避窗内是否已收到取消请求（[`run_turn`] 的检查点读它）
    fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

/// `live` 落闩守卫（`run_turn` 任一出口生效；**Drop 而非逐 return 点写**——漏一处就会让
/// 「回合已终结」的取消被冒领）
struct LiveGuard(Arc<AtomicBool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 取消收尾的终态（**两段取消检查点共用**：退避窗内 [`CancelLatch`] 检查点，与**武装后起跑前**
/// 的复检命中——后者不经退避窗，故原因文案必须同时覆盖两段，见下），形状**对齐 codex
/// `cancelled_outcome`**：`Receipt::cancelled` + 会话号 + 回合墙钟；stage 留空——取消不是失败
/// 阶段，绝不报成 timeout / channel_error。
fn cancelled_outcome(session_id: &str, attempts: usize, total_ms: u64) -> TurnOutcome {
    TurnOutcome {
        receipt: Receipt::cancelled(
            "已取消（移动端请求）——叫停发生在工作区争用锁的重试退避窗内、或本轮武装后起跑前的\
             复检窗口（含武装后起跑前复检命中）：本条不再重试、后续尝试一律不发起；本次是否已\
             送达请在 ZCode 会话内容中核实，ZCode 应用仍占用该工作区时请稍后重发",
        )
        .with_session(session_id)
        .with_duration_ms(total_ms),
        attempts,
        busy_final: false,
        receipt_source: ReceiptSource::NotApplicable,
    }
}

/// 单回合编排（**唯一**消费 Task 6 底座的地方：并发经 runner 的全局名额、看门狗与取消
/// 归 runner、回执归 receipt 归一）。流程：
/// 每次尝试 → `make_runner(inv)` 建回合配置（端点侧经 `headless::runner_from_conn`，
/// 超时/并发按设置）→ 逐尝试 `registry().arm` 更新取消靶子 → 执行 → 退出分类：
/// - 争用锁 → 探活 + [`busy_step`]（≤[`TurnDeps::max_busy_retries`] 次重试，仍忙 →
///   **如实的 [`Stage::WorkspaceBusy`] 失败回执**，绝不冒充成功）；
/// - 其他 → 终结（见 [`finalize`]）。
///
/// **取消检查点（R9 两段）**：① **退避后**（每次退避等待之后、下一次尝试武装之前）与
/// ② **武装后起跑前**（复检；R12-S3 / 裁决 24c-③）各查一次 [`CancelLatch`]——两段窗口里
/// 到达的取消都在此收尾为 [`cancelled_outcome`]（此时进程已退或尚未起跑，只有闩记得住那一发）。
/// **两处都不注销槽位**（单主不变量：`begin`/`end` 成对归包装器——理由与误删竞态见复检臂注释）。
///
/// # 回执源（H6 契约的真源；真机实证修订）
/// **stdout 只作完成信号**（退出码 / 看门狗 / 争用锁特征串）；`lastAssistant`/`tokens`
/// 取自**会话库**（真机实证：`--resume` 回合 exit 0 且库里有回复，但 stdout 无可解析 JSON）。
/// 顺序：① stdout 有 JSON 帧 → 用它（未来/其他子命令可能仍出 JSON）；② 库确认本轮写入了
/// **新的 assistant 回复** → 用库的文本 + tokens；③ 库也确认不了 → 如实的 channel_error
/// （绝不把「确认不了」说成「没产出」，也绝不把「进程退 0」当成功）。
///
/// 回执耗时 = 整回合墙钟（含重试等待），审计口径同源。
pub async fn run_turn(
    inv: &ZcodeInvocation,
    session_id: &str,
    project: &str,
    make_runner: &(dyn Fn(&ZcodeInvocation) -> RunnerCfg + Send + Sync),
    deps: &TurnDeps,
    run: &RunSeam,
) -> TurnOutcome {
    let started = Instant::now();
    // 回执源基线（回合**前**读一次库）：本轮是否真的写入了新的 assistant 回复
    let before = (deps.store_probe)(session_id);
    // 退避窗取消闩（R9）：逐尝试武装、退避醒来查；`_live` 保证「回合终结后不冒领」
    let cancel = CancelLatch::new();
    let _live = LiveGuard(cancel.live.clone());
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        let cfg = make_runner(inv);
        // 取消靶子逐尝试更新（版本门控期间靶子为空 → 取消如实报「未送达」）——
        // 包法单点复用底座 [`super::turn::cancel_fn_of`]，外加退避窗闩（[`CancelLatch`]）
        registry().arm(session_id, cancel.target(&cfg));
        // **取消复检（R9 微窗口，R12-S3 / 裁决 24c-③）**：武装之后、**起跑之前**再查一次闩。
        // 上一发检查点（循环尾）与本次武装之间还有一段（`make_runner` 读设置 + 武装本身），
        // 落在其中的取消会让**本发照跑**：runner 的取消 sink 要到 `run()` 内部（spawn 之后）
        // 才武装，`CancelHandle::cancel` 此刻恒 `false` ⇒ 只有闩记得住那一发。不查的后果
        // 不是「少杀一个进程」而是**多起一个进程**（恒忙夹具下会多试一次；真机上是多 spawn
        // 一次并可能跑完整个回合），而取消端点已按闩如实报「送达」——两边口径矛盾。
        //
        // 收尾 = **只回 [`cancelled_outcome`]，绝不 `registry().end()`**。槽位是**包装器**的：
        // `begin`/`end` 成对归各分派臂（`remote::api::run_zcode_turn` 的注销落在审计行**之后**；
        // 新建通道同形，注销的是项目锁键），而 `run_turn` 有多条调用面（`zcode_create` 也走它）
        // ⇒ 它**不拥有**槽位。代庖注销会开出**误删竞态**（U1，2026-10-05 收尾轮）：本臂一删旧
        // 槽位，包装器写审计行的窗口里移动端立刻发下一条 ⇒ 新回合 `begin()` 成功（旧槽位已不在，
        // 串行锁放行）⇒ 包装器随后按**会话号**注销，删掉的正是**新**回合的槽位（新回合从此无取消
        // 靶子、串行锁形同虚设；它自己的 `end` 还会再去删下一条）。**单主不变量：只有占位者能
        // 注销**——本臂返回后由包装器注销（`post_arm_cancel_does_not_end_the_slot_the_wrapper_owns`
        // 钉死该时序）。
        //
        // 「不留已武装靶子」的顾虑（上一轮代庖注销的理由）另有解：本臂返回后**同一 detached 任务**
        // 随即写审计行并注销槽位；触发本臂的那一发取消本就打在**已武装靶子**上，由闩如实报
        // 「送达」——而它换来的回执正是 `cancelled`（**自洽**：靶子留给它真正的收件人，而非抢着
        // 注销）；`run_turn` 返回后（`LiveGuard` 已落闩）到达的取消由同一靶子如实报「未送达」，
        // 取消端点据此说「回合已终结」（**不冒领**）。
        //
        // **登记的残余（已知限制，未在本轮关闭；裁决 27）**：本复检只覆盖「武装后、`run()`
        // 之前」这一段；取消若落在 `run()` 内部而 runner 尚未武装 sink（**spawn 窗口**，含并发
        // 名额满的 `queued` 早退臂），仍会被闩记成「已送达」而不生效——那一发的回执按**实际
        // 结局**如实上报（可能是 `ok`，即「叫停来晚了，这一发已经跑完」）。要彻底关掉需在 runner
        // 侧把 sink 武装前移到 spawn 之前（改的是公共底座、影响所有 spawn 型通道），不在本轮范围。
        // **已登记 issue [#115](https://github.com/jarvislee90s-dot/MultiAgents-Manager/issues/115)，
        // 排期下一批**（计划遗留登记 **L-36** 同条）：窗口毫秒级、后果 = 多跑一发（token 消耗），
        // 不是数据风险（串行锁与审计不受影响）；**勿用「跑完后查闩改判 cancelled」的错误修法**
        // ——那会把已跑完的回合谎报成取消（issue 正文明确拒绝过该方案）。
        if cancel.requested() {
            return cancelled_outcome(session_id, attempts, started.elapsed().as_millis() as u64);
        }
        let obs = run(cfg);
        let obs = obs.await;
        // 证据串（先判争用锁再判退出码：busy 形态退出码是 0）
        let stdout_head = head_of(&obs.stdout);
        let stderr_lines: Vec<String> = obs.stderr.lines().map(str::to_string).collect();
        let stderr_head = head_of(&stderr_lines);
        let class = classify_exit(&ExitObs {
            code: obs.exit.unwrap_or(-1),
            stderr_head: &stderr_head,
            stdout_head: &stdout_head,
            duration_ms: obs.receipt.duration_ms,
        });
        if class.stage != Some(Stage::WorkspaceBusy) {
            let total_ms = started.elapsed().as_millis() as u64;
            let (receipt, source) =
                finalize(obs, session_id, total_ms, before.as_ref(), deps).await;
            return TurnOutcome {
                receipt,
                attempts,
                busy_final: false,
                receipt_source: source,
            };
        }
        let step = busy_step(
            attempts,
            (deps.workspace_active)(project),
            deps.max_busy_retries,
            deps.retry_spacing,
        );
        match step {
            BusyStep::GiveUp => {
                return TurnOutcome {
                    receipt: busy_receipt(
                        session_id,
                        project,
                        attempts,
                        deps.retry_spacing,
                        started.elapsed().as_millis() as u64,
                        class.evidence.as_deref(),
                    ),
                    attempts,
                    busy_final: true,
                    receipt_source: ReceiptSource::NotApplicable,
                }
            }
            BusyStep::RetryNow => {}
            BusyStep::RetryAfterWait(d) => (deps.wait)(d).await,
        }
        // **取消检查点（R9）**：退避等待之后、下一次尝试武装之前——退避窗内到达的取消在
        // 这里收尾（子进程已退、runner 靶子已解除武装，只有闩记得住那一发；不查就会再试一次）
        if cancel.requested() {
            return cancelled_outcome(session_id, attempts, started.elapsed().as_millis() as u64);
        }
    }
}

/// 终结归一（**回执源三级**，见 [`run_turn`] 文档）：
/// ① stdout JSON 帧优先；② 库确认新 assistant 回复 → 库文本 + tokens；
/// ③ 都不行 → channel_error（如实「未确认」，绝不冒充成功）。
async fn finalize(
    obs: TurnObs,
    session_id: &str,
    total_ms: u64,
    before: Option<&StoreSnapshot>,
    deps: &TurnDeps,
) -> (Receipt, ReceiptSource) {
    // 先拆包（回执随后被改写；证据串留用）
    let TurnObs {
        receipt,
        stdout,
        stderr,
        exit: _,
    } = obs;
    let acc = frames_of_stdout(&stdout);
    let mut r = receipt;
    if r.status != ReceiptStatus::Ok {
        // 非 0 退出 / 超时 / 取消：库不参与判定，原样如实上抛
        r.session_id = if r.session_id.is_empty() {
            session_id.to_string()
        } else {
            r.session_id
        };
        r.duration_ms = total_ms;
        return (r, ReceiptSource::NotApplicable);
    }
    // ① stdout JSON 帧（Task 6 原口径）
    if acc.json_frames > 0 {
        if r.last_assistant.is_none() && acc.last_assistant.is_some() {
            r = acc.receipt(session_id, ReceiptStatus::Ok, None, total_ms);
        }
        r.duration_ms = total_ms;
        return (r, ReceiptSource::StdoutJson);
    }
    // ② 会话库确认（**有界轮询**：懒落库/ WAL 可见性可能滞后，最多 `store_confirm_attempts`
    //    次读、间隔 `store_confirm_interval`；生产 3×1s —— 进程已退出，滞后量级为百毫秒）
    let confirmation = match before {
        None => StoreConfirmation::Unavailable,
        Some(base) => {
            let mut verdict = StoreConfirmation::NoNewAssistant;
            for attempt in 0..deps.store_confirm_attempts.max(1) {
                if attempt > 0 {
                    (deps.wait)(deps.store_confirm_interval).await;
                }
                match (deps.store_probe)(session_id) {
                    Some(now) => {
                        verdict = confirm_from_store(base, &now);
                        if matches!(verdict, StoreConfirmation::NewAssistant { .. }) {
                            break;
                        }
                    }
                    None => verdict = StoreConfirmation::Unavailable,
                }
            }
            verdict
        }
    };
    match confirmation {
        StoreConfirmation::NewAssistant { text, tokens } => (
            ok_receipt_from_store(session_id, &text, tokens, total_ms),
            ReceiptSource::SessionStore,
        ),
        other => (
            Receipt::failed(
                Stage::ChannelError,
                &channel_error_reason(&stdout, &stderr, Some(&other)),
            )
            .with_session(session_id)
            .with_duration_ms(total_ms),
            ReceiptSource::Unconfirmed,
        ),
    }
}

/// 库确认成功时的回执（**截断口径复用 Task 6 单点**：`receipt::LAST_ASSISTANT_CHARS` +
/// `normalize::summarize`——不另写一份截断）。定义已上提到 `turn` 底座的
/// [`super::turn::ok_receipt_with_assistant`]（Task 12：H10 新建路径要用同一条口径；
/// 它是通道无关的收尾件），此处保留薄壳以维持 Task 8 既有调用面。
fn ok_receipt_from_store(
    session_id: &str,
    text: &str,
    tokens: Option<u64>,
    total_ms: u64,
) -> Receipt {
    super::turn::ok_receipt_with_assistant(session_id, text, tokens, total_ms)
}

/// 争用锁重试用尽的失败回执（**如实报「工作区忙」**，不冒充成功）
fn busy_receipt(
    session_id: &str,
    project: &str,
    attempts: usize,
    spacing: Duration,
    total_ms: u64,
    evidence: Option<&str>,
) -> Receipt {
    let retries = attempts.saturating_sub(1);
    let ev = evidence.unwrap_or(WORKSPACE_BUSY_MARKER);
    Receipt::failed(
        Stage::WorkspaceBusy,
        &format!(
            "工作区忙：ZCode 应用在项目 {project} 活跃，无头回合被应用层争用锁拒绝（{ev}）；\
             已重试 {retries} 次（每次间隔 {}s）仍失败——请在该工作区空闲后重发",
            spacing.as_secs()
        ),
    )
    .with_session(session_id)
    .with_duration_ms(total_ms)
}

// ============================================================
// 版本门控探针缓存（会话串行锁 / 取消靶子登记表已上提 [`super::turn`]）
// ============================================================

/// 版本门控探针缓存（进程级；键 = exe 在场性 + mtime，工具升级即失效重探）
static PROBE_CACHE: std::sync::LazyLock<super::gate::ProbeCache> =
    std::sync::LazyLock::new(super::gate::ProbeCache::default);

pub fn probe_cache() -> &'static super::gate::ProbeCache {
    &PROBE_CACHE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::gate::ZCODE_PROVIDER_CONFIG_ENV;
    use crate::inject::headless::receipt::Stage;

    // ===== 计划 Step 1 原例（Task 8 的红灯测试） =====

    /// 探测定案 D1：Mac 必须补 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE`（否则 --prompt 静默无 JSON）；
    /// 两端都设 `ELECTRON_RUN_AS_NODE`；argv 含 cjs 脚本；档位段来自 [`PermissionSpec`]（裁决 14）
    #[test]
    fn argv_windows_and_mac_diverge() {
        // 探测定案 D1：Mac 必须补 ZCODE_BUILTIN_PROVIDER_CONFIG_FILE
        let w = super::build_argv(
            &super::ZcodeSpec::win("D:/Program Files/ZCode"),
            "你好 [mobile iPad]",
            "sess_1",
            "E:/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert!(w.iter().any(|a| a.contains("zcode.cjs")));
        // 档位段**从 spec 派生**期望（不是本测自己抄一份字面）——spec 改档时本测跟着走；
        // 逐字面锁在同文件的 `argv_pins_the_probed_command_form`（独立期望，故意写死）
        let perm = PermissionSpec::zcode_default().flags();
        assert!(
            w.argv
                .windows(perm.len())
                .any(|p| p.iter().map(String::as_str).eq(perm.iter().copied())),
            "argv 档位段必须来自己 H5 档面（裁决 14）: {:?}",
            w.argv
        );
        let m = super::build_argv(
            &super::ZcodeSpec::mac("/Applications/ZCode.app"),
            "hi",
            "sess_1",
            "/tmp/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert!(m.env.contains_key("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE"));
        assert!(m.env.contains_key("ELECTRON_RUN_AS_NODE"));
    }

    /// 探测定案：APP 活跃工作区 → "Model creation failed" 1s 退出 → Stage::WorkspaceBusy（探活重试）
    #[test]
    fn workspace_busy_maps_to_retryable_stage() {
        let r = super::classify_exit(&super::ExitObs {
            code: 0,
            stderr_head: "",
            stdout_head: "Error: Model creation failed",
            duration_ms: 1000,
        });
        assert!(matches!(r.stage, Some(Stage::WorkspaceBusy)));
        assert!(r.retryable);
    }

    // ===== 命令形态 =====

    /// spec H7 命令形态逐字：`<cjs> --prompt <text> --resume <sess> --cwd <proj> --mode yolo --json`
    /// + program/env 平台分叉（Mac provider config 路径从 cjs 反推）
    #[test]
    fn argv_pins_the_probed_command_form() {
        let w = build_argv(
            &ZcodeSpec::win("D:/Program Files/ZCode"),
            "正文",
            "sess_9",
            "E:/proj",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert_eq!(
            w.program,
            PathBuf::from("D:/Program Files/ZCode")
                .join("ZCode.exe")
                .to_string_lossy()
        );
        assert_eq!(
            w.argv,
            vec![
                PathBuf::from("D:/Program Files/ZCode")
                    .join("resources")
                    .join("glm")
                    .join("zcode.cjs")
                    .to_string_lossy()
                    .to_string(),
                "--prompt".into(),
                "正文".into(),
                "--resume".into(),
                "sess_9".into(),
                "--cwd".into(),
                "E:/proj".into(),
                // 注意：这里的 `--mode` / `yolo` 是**测试面字面量**（独立期望——向量锁故意
                // 不从实现派生，实现真被改坏时它才拦得住）；「档位来自 spec」的同源断言见
                // `mode_flag_is_injected_from_the_permission_spec`
                "--mode".into(),
                "yolo".into(),
                "--json".into(),
            ],
            "Windows argv 必须逐字对齐探测定案（顺序敏感）"
        );
        assert_eq!(
            w.env.get(ELECTRON_RUN_AS_NODE).map(String::as_str),
            Some("1")
        );
        // Windows 与 provider config（Task 8 真机实证修订）：推导路径**在场才设**。
        // 用必定不存在的根断言「不设」——不得依赖真机安装路径（否则本机绿、CI 红）
        let absent = build_argv(
            &ZcodeSpec::win("Z:/mam-nonexistent-zcode-root"),
            "正文",
            "sess_9",
            "E:/proj",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert!(
            !absent.env.contains_key(ZCODE_PROVIDER_CONFIG_ENV),
            "推导路径不在场 → Windows 不设（不指向不存在的文件；真机实证见 gate::provider_config_env）"
        );
        let (_fixture, fixture_root) = tmp_install("windows");
        let present = build_argv(&ZcodeSpec::win(&fixture_root), "x", "s", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert!(
            present
                .env
                .get(ZCODE_PROVIDER_CONFIG_ENV)
                .is_some_and(|v| v.ends_with("zcode-builtin.json")),
            "推导路径在场（tempdir 夹具）→ Windows 必须设（真机装包就在该位置）：{:?}",
            present.env
        );
        // Mac：同骨架 + provider config env（值 = <Resources>/config/provider/zcode-builtin.json）
        let m = build_argv(
            &ZcodeSpec::mac("/Applications/ZCode.app"),
            "hi",
            "s",
            "/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let cfg = m
            .env
            .get(ZCODE_PROVIDER_CONFIG_ENV)
            .expect("Mac 必须设 provider config");
        assert!(
            cfg.ends_with("zcode-builtin.json") && cfg.contains("config"),
            "provider config 路径从 cjs 反推（Resources/config/provider/zcode-builtin.json）：{cfg}"
        );
        assert_eq!(
            m.argv[0],
            PathBuf::from("/Applications/ZCode.app")
                .join("Contents")
                .join("Resources")
                .join("glm")
                .join("zcode.cjs")
                .to_string_lossy()
        );
    }

    /// H5 权限档（Task 10）：zcode 的 `--mode` 段**来自** [`PermissionSpec::zcode_default`]
    /// ——本文件不再写字面 `yolo`（单一来源）。旗子按实测序插在 `--cwd <项目>` 之后、
    /// `--json` 之前；zcode **没有审批面**（裁决 14），这里只表达档位。
    #[test]
    fn mode_flag_is_injected_from_the_permission_spec() {
        let spec = PermissionSpec::zcode_default();
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_1", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let i = inv
            .argv
            .iter()
            .position(|a| a == "--mode")
            .expect("`--mode` 必须在场（裁决 14：yolo 保可用性）");
        assert_eq!(
            &inv.argv[i..i + spec.flags().len()],
            spec.flags().as_slice(),
            "argv 的档位段必须逐字来自 spec（顺序敏感）"
        );
        assert_eq!(
            inv.argv.iter().filter(|a| *a == "--mode").count(),
            1,
            "档位旗子只此一处（不得既从 spec 注入又留字面）"
        );
        assert_eq!(
            inv.argv.last().map(String::as_str),
            Some("--json"),
            "`--json` 仍是尾 flag（档位段插在它之前）"
        );
        // 注入点**参数化**（构造器不写死 yolo）：换一组旗子即换输出
        let alt = turn_flags(
            Some("sess_1"),
            "E:/p",
            "hi",
            "zcode.cjs",
            &["--mode", "plan"],
        );
        assert!(
            alt.windows(2).any(|w| w == ["--mode", "plan"]),
            "构造器必须按传入旗子组装: {alt:?}"
        );
        assert!(
            !alt.iter().any(|a| a == "yolo"),
            "构造器不得自带字面档位: {alt:?}"
        );
    }

    /// **H10 无头新建形态**（Task 12）：`build_create_argv` 与 [`build_argv`] 同源同序，
    /// **唯一差别 = 没有 `--resume`**（以及不传会话号）——完整向量已由
    /// `argv_pins_the_probed_command_form` 钉死，本测**只加**新建特有的断言（不抄一份向量）
    #[test]
    fn create_form_omits_resume_and_keeps_the_flag_order() {
        let spec = ZcodeSpec::win("D:/Program Files/ZCode");
        let c = build_create_argv(&spec, "hi", "E:/proj", Some("iPad"))
            .expect("测试花名在册（新建 argv 构造，见 turn::device_name_refusal）");
        assert!(
            !c.argv.iter().any(|a| a == "--resume"),
            "H10 新建形态绝无 `--resume`（无在册会话可续）: {:?}",
            c.argv
        );
        assert!(
            c.argv.windows(2).any(|w| w == ["--cwd", "E:/proj"]),
            "`--cwd <项目>` 必须在场: {:?}",
            c.argv
        );
        assert_eq!(c.prompt, "hi [mobile iPad]", "W4 单点组装（签名后置）");
        assert_eq!(c.argv[2], c.prompt, "argv 载荷与 prompt 同源");
        assert_eq!(
            c.argv.last().map(String::as_str),
            Some("--json"),
            "`--json` 仍是尾 flag: {:?}",
            c.argv
        );
        // 档位段仍来自 H5 spec（单一来源），且**紧跟 `--cwd`**（Task 8 实测序）
        let perm = PermissionSpec::zcode_default().flags();
        let i = c
            .argv
            .iter()
            .position(|a| a == "--cwd")
            .expect("--cwd 在场");
        assert_eq!(
            &c.argv[i + 2..i + 2 + perm.len()],
            perm.as_slice(),
            "新建形态与 H7 同一 flag 序（档位段在 --cwd 之后）: {:?}",
            c.argv
        );
        // 与 H7 形态同源：只差 resume 两位
        let h7 = build_argv(&spec, "hi", "sess_9", "E:/proj", Some("iPad"))
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let mut expected_drop = h7.argv.clone();
        let ri = expected_drop
            .iter()
            .position(|a| a == "--resume")
            .expect("H7 形态带 --resume");
        expected_drop.drain(ri..ri + 2);
        assert_eq!(c.argv, expected_drop, "两形态只差 resume 段（同源同序）");
    }

    /// H10 项目级串行锁键：工作区口径归一（分隔符/尾分隔/平台大小写）——同项目两次新建
    /// 必须落同一个键（否则重叠创建不受串行锁保护）
    #[test]
    fn create_lock_key_normalizes_like_the_workspace_probe() {
        let a = create_lock_key("E:\\LLMproject\\Proj\\", "windows");
        assert_eq!(a, create_lock_key("E:/LLMproject/proj", "windows"));
        assert!(a.starts_with("create|"), "与 sess_… 命名空间隔离: {a}");
        assert!(
            !a.starts_with("sess_"),
            "锁键不得与会话号形态混淆（取消端点只认 sess_… 的槽位）: {a}"
        );
        assert_ne!(
            create_lock_key("/tmp/P", "macos"),
            create_lock_key("/tmp/p", "macos"),
            "macOS 不折叠大小写（与 same_workspace 同口径）"
        );
    }

    /// W4 单点：给设备名 → `{归一正文} [mobile 名]`（多行归一为字面 `\n`）；不给 → 原样
    /// （调用方已拼好，不二次签名）；审计 content 与 argv 载荷同源
    #[test]
    fn prompt_uses_w4_composition_only_when_device_given() {
        let inv = build_argv(
            &ZcodeSpec::win("D:/ZCode"),
            "第一行\n第二行",
            "sess_1",
            "E:/p",
            Some("iPad"),
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert_eq!(
            inv.prompt, "第一行\\n第二行 [mobile iPad]",
            "多行归一走 W4 单点（字面 \\n）+ 尾签名"
        );
        assert_eq!(inv.argv[2], inv.prompt, "argv 载荷与 prompt 同源");
        let raw = build_argv(
            &ZcodeSpec::win("D:/ZCode"),
            "已拼好 [mobile iPad]",
            "s",
            "E:/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert_eq!(
            raw.prompt, "已拼好 [mobile iPad]",
            "None = 原样（不二次签名）"
        );
    }

    /// 斜杠命令**显式拒绝**（spec H7：字面文本、不具等价性——不静默透传）
    #[test]
    fn slash_commands_are_refused_explicitly() {
        assert!(slash_refusal("/plan 做点什么").is_some());
        assert!(
            slash_refusal(" /mode").is_none(),
            "单点判据（normalize::is_slash_message）不跳前导空白——与终端路径同口径，勿另立更宽判据"
        );
        assert!(slash_refusal("普通消息").is_none());
        assert!(
            slash_refusal("/plan")
                .unwrap_or_default()
                .contains("无头通道不可用"),
            "拒绝文案必须说清「不可用」，不得冒充支持"
        );
    }

    // ===== 争用锁判定 =====

    /// 退出码 **0** 也必须判争用（探测定案：busy 形态 1s 退出、退出码 0——按码先判即假成功）
    #[test]
    fn busy_marker_beats_zero_exit_code() {
        let r = classify_exit(&ExitObs {
            code: 0,
            stderr_head: "",
            stdout_head: "ZCode Built-in skipped (not-due)\nError: Model creation failed",
            duration_ms: 900,
        });
        assert_eq!(r.stage, Some(Stage::WorkspaceBusy));
        assert!(r
            .evidence
            .unwrap_or_default()
            .contains(WORKSPACE_BUSY_MARKER));
        // stderr 侧命中同样算（探测定案只说「打印」，未限定流）
        let s = classify_exit(&ExitObs {
            code: 0,
            stderr_head: "model creation FAILED",
            stdout_head: "",
            duration_ms: 1200,
        });
        assert_eq!(s.stage, Some(Stage::WorkspaceBusy));
    }

    /// 非零退出 → Crash（不重试）；0 退出无争用 → None（沿用 runner 回执）；
    /// 证据串为空的 0 退出不得被误判成 busy
    #[test]
    fn other_exits_classify_honestly() {
        let crash = classify_exit(&ExitObs {
            code: 3,
            stderr_head: "boom",
            stdout_head: "",
            duration_ms: 500,
        });
        assert_eq!(crash.stage, Some(Stage::Crash));
        assert!(!crash.retryable, "H4：崩溃不自动重试");
        let ok = classify_exit(&ExitObs {
            code: 0,
            stderr_head: "",
            stdout_head: "{\"sessionId\":\"s1\"}",
            duration_ms: 8000,
        });
        assert_eq!(ok.stage, None, "正常退出不覆盖 runner 回执");
        assert!(!ok.retryable);
    }

    /// 重试节奏：APP 仍活跃 → 等 5s 再试；APP 已离开该工作区 → 立即重试（不空等）；
    /// 重试次数用尽 → 如实放弃（总尝试 ≤ 重试上限 + 1）
    #[test]
    fn busy_step_waits_only_while_the_app_is_active() {
        let sp = Duration::from_millis(BUSY_RETRY_SPACING_MS);
        assert_eq!(busy_step(1, true, 2, sp), BusyStep::RetryAfterWait(sp));
        assert_eq!(busy_step(1, false, 2, sp), BusyStep::RetryNow);
        assert_eq!(busy_step(2, true, 2, sp), BusyStep::RetryAfterWait(sp));
        assert_eq!(
            busy_step(3, true, 2, sp),
            BusyStep::GiveUp,
            "重试上限 2 → 第 3 次尝试后放弃（总尝试 3）"
        );
        assert_eq!(BUSY_MAX_RETRIES, 2, "spec H7：retry ≤ 2");
        assert_eq!(BUSY_RETRY_SPACING_MS, 5_000, "spec H7：间隔 5s");
    }

    /// 工作区活跃探活：相等或子树命中；`proj` 不得命中 `proj2`（分隔符边界严判）；
    /// Windows 大小写不敏感；空项目/空 cwd 不命中
    #[test]
    fn workspace_probe_matches_project_with_boundary() {
        let cwds = vec!["E:\\LLMproject\\proj".to_string()];
        assert!(workspace_active_in("E:/LLMproject/proj", &cwds, "windows"));
        assert!(
            workspace_active_in("e:/llmproject/PROJ", &cwds, "windows"),
            "Windows 大小写不敏感"
        );
        assert!(
            workspace_active_in("E:/LLMproject/proj/sub", &cwds, "windows"),
            "子目录同属该工作区"
        );
        assert!(
            !workspace_active_in("E:/LLMproject/proj2", &cwds, "windows"),
            "不得跨工作区误命中"
        );
        assert!(!workspace_active_in("", &cwds, "windows"));
        assert!(!workspace_active_in("E:/other", &cwds, "windows"));
        assert!(!workspace_active_in("E:/LLMproject/proj", &[], "windows"));
        // macOS/Linux：不做大小写折叠（避免猜测式命中）
        assert!(workspace_active_in(
            "/tmp/p",
            &["/tmp/p".to_string()],
            "macos"
        ));
        assert!(!workspace_active_in(
            "/tmp/P",
            &["/tmp/p".to_string()],
            "macos"
        ));
    }

    /// app-server 判定（探测定案口径）：exe 命中 ZCode **且** 命令行含 zcode.cjs；
    /// Electron 辅助进程（--type=）与别家 exe 不算
    #[test]
    fn app_server_requires_zcode_exe_and_runtime_cmdline() {
        use std::ffi::OsString;
        let os = |s: &str| OsString::from(s);
        assert!(is_app_server(
            Some(Path::new("D:\\Programs\\ZCode\\ZCode.exe")),
            &[
                os("ZCode.exe"),
                os("D:\\Programs\\ZCode\\resources\\glm\\zcode.cjs")
            ]
        ));
        assert!(!is_app_server(
            Some(Path::new("D:\\Programs\\ZCode\\ZCode.exe")),
            &[os("ZCode.exe"), os("--type=renderer")]
        ));
        assert!(!is_app_server(
            Some(Path::new("/usr/bin/node")),
            &[os("node"), os("zcode.cjs")]
        ));
        assert!(!is_app_server(None, &[os("zcode.cjs")]));
    }

    // ===== 信任可见性 =====

    fn tmp_home_with_setting(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let v2 = dir.path().join(".zcode").join("v2");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(v2.join("setting.json"), json).unwrap();
        dir
    }

    /// 信任表只读解析：`recentProjects` 是**路径字符串数组**（真机形态）；BOM/缺失/坏 JSON
    /// 一律回空表（保守 = 未信任，不谎报「重启后可见」）
    #[test]
    fn trust_probe_reads_recent_projects_and_fails_closed() {
        let dir = tmp_home_with_setting(
            "{\"recentProjects\":[\"E:\\\\LLMproject\\\\proj\",\"/tmp/other\"],\"locale\":\"zh-CN\"}",
        );
        let t = trusted_projects(Some(dir.path()));
        assert_eq!(t.len(), 2, "路径字符串数组（不是对象表）：{t:?}");
        assert!(project_trusted("E:/LLMproject/proj", &t, "windows"));
        assert!(!project_trusted("E:/LLMproject/proj2", &t, "windows"));
        // BOM（真机文件首字节实测）
        let bom = tmp_home_with_setting("\u{feff}{\"recentProjects\":[\"/tmp/x\"]}");
        assert_eq!(
            trusted_projects(Some(bom.path())).len(),
            1,
            "BOM 不得让解析失败"
        );
        // 坏 JSON / 缺键 / 无 home / 文件缺失 → 空表
        let bad = tmp_home_with_setting("{ not json");
        assert!(trusted_projects(Some(bad.path())).is_empty());
        let no_key = tmp_home_with_setting("{\"locale\":\"zh-CN\"}");
        assert!(trusted_projects(Some(no_key.path())).is_empty());
        assert!(trusted_projects(None).is_empty());
        assert!(trusted_projects(Some(Path::new("/nonexistent-home-xyz"))).is_empty());
    }

    /// 可见性文案经 Task 7 单点（两档逐字）——本模块不另造映射
    #[test]
    fn visibility_comes_from_routing_single_point() {
        let dir = tmp_home_with_setting("{\"recentProjects\":[\"E:\\\\p\"]}");
        let trusted = visibility_of("E:/p", Some(dir.path()), "windows");
        assert_eq!(trusted, Visibility::AfterRestart);
        assert_eq!(trusted.note(), "已信任工作区：重启 ZCode 应用后可见");
        let untrusted = visibility_of("E:/q", Some(dir.path()), "windows");
        assert_eq!(untrusted, Visibility::TuvisOnly);
        assert_eq!(untrusted.note(), "未信任工作区：仅兔维斯可见");
        assert_eq!(
            visibility_of("E:/p", None, "windows"),
            Visibility::TuvisOnly,
            "读不到信任表 = 保守判未信任"
        );
    }

    // ===== 安装路径发现 =====

    /// 临时安装夹具（**真机布局**：cjs + `<resources>/config/provider/zcode-builtin.json`——
    /// Task 8 真机实证的文件落点，正是 CLI 自身查找表里**没有**的那一格）
    fn tmp_install(os: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let (cjs, provider) = if os == "macos" {
            (
                Path::new(&root)
                    .join("Contents")
                    .join("Resources")
                    .join("glm")
                    .join("zcode.cjs"),
                Path::new(&root)
                    .join("Contents")
                    .join("Resources")
                    .join("config")
                    .join("provider")
                    .join("zcode-builtin.json"),
            )
        } else {
            (
                Path::new(&root)
                    .join("resources")
                    .join("glm")
                    .join("zcode.cjs"),
                Path::new(&root)
                    .join("resources")
                    .join("config")
                    .join("provider")
                    .join("zcode-builtin.json"),
            )
        };
        std::fs::create_dir_all(cjs.parent().unwrap()).unwrap();
        std::fs::create_dir_all(provider.parent().unwrap()).unwrap();
        std::fs::write(&cjs, "// stub").unwrap();
        std::fs::write(&provider, "{}").unwrap();
        (dir, root)
    }

    /// 解析要求 **cjs 真在场**（缺席 = 如实报安装不可达，绝不 spawn 不存在的程序）；
    /// 候选表：宿主 exe 证据优先 + 常见路径，去重保序
    #[test]
    fn resolve_spec_requires_cjs_in_place() {
        let (_d, root) = tmp_install("windows");
        let spec =
            resolve_spec(std::slice::from_ref(&root), "windows").expect("cjs 在场即解析成功");
        assert!(spec.exe.ends_with("ZCode.exe"));
        assert!(spec.cjs.ends_with("zcode.cjs"));
        assert!(!spec.is_mac());
        assert!(
            resolve_spec(&["Z:/nope".to_string()], "windows").is_none(),
            "缺席即 None（不猜路径）"
        );
        // 宿主 exe → 安装根；well_known 去重保序；非 ZCode 的 exe 不反推
        let roots = candidate_roots(
            Some("D:\\Program Files\\ZCode\\ZCode.exe"),
            &[
                "D:\\Program Files\\ZCode".to_string(),
                "C:\\Program Files\\ZCode".to_string(),
            ],
            "windows",
        );
        assert_eq!(roots[0], "D:\\Program Files\\ZCode");
        assert_eq!(roots.len(), 2, "同根去重：{roots:?}");
        let foreign = candidate_roots(
            Some("/usr/bin/node"),
            &["/Applications/ZCode.app".into()],
            "macos",
        );
        assert_eq!(foreign, vec!["/Applications/ZCode.app".to_string()]);
        // macOS：exe → 最内层 .app 包（复用 app_bundle_from_exe）
        assert_eq!(
            root_from_exe("/Applications/ZCode.app/Contents/MacOS/ZCode", "macos").unwrap(),
            "/Applications/ZCode.app"
        );
    }

    /// 测试构建**绝不咨询真机安装路径**（宪法级：本机真装了 ZCode，若咨询就会真 spawn）——
    /// 这保证端点用例只可能拿到「安装不可达」这一确定性失败
    #[test]
    fn production_roots_is_empty_under_cfg_test() {
        assert!(
            production_roots(std::process::id()).is_empty(),
            "测试构建必须零真实安装路径/进程表接触（零真实配额消耗 + 零真实 ~/.zcode 写入）"
        );
    }

    // ===== 回执/审计口径 =====

    /// 审计 result 词：终态 + 阶段码（阶段词经 serde 单一来源，不另抄词表）
    #[test]
    fn receipt_result_word_covers_every_stage() {
        assert_eq!(receipt_result_word(&Receipt::ok("s", 1)), "ok");
        assert_eq!(receipt_result_word(&Receipt::queued(2)), "queued");
        assert_eq!(receipt_result_word(&Receipt::cancelled("x")), "cancelled");
        assert_eq!(
            receipt_result_word(&Receipt::failed(Stage::WorkspaceBusy, "忙")),
            "failed(workspace_busy)"
        );
        assert_eq!(
            receipt_result_word(&Receipt::failed(Stage::Timeout, "超时")),
            "failed(timeout)"
        );
        assert_eq!(
            receipt_result_word(&Receipt::failed(Stage::VersionGate, "门控")),
            "failed(version_gate)"
        );
        assert_eq!(
            receipt_result_word(&Receipt::failed(Stage::Crash, "崩")),
            "failed(crash)"
        );
        assert_eq!(
            receipt_result_word(&Receipt::failed(Stage::ChannelError, "通道")),
            "failed(channel_error)"
        );
    }

    /// 帧统计复用 Task 6 前缀跳过（Mac 噪音前缀行不算帧）
    #[test]
    fn frames_of_skips_noise_prefix() {
        let acc = frames_of(&[
            "ZCode Built-in skipped (not-due)".to_string(),
            "{\"sessionId\":\"s1\",\"response\":\"hi\",\"tokens\":9}".to_string(),
        ]);
        assert_eq!(acc.json_frames, 1);
        assert_eq!(acc.noise_lines, 1);
        assert_eq!(acc.session_id.as_deref(), Some("s1"));
    }

    /// **多行（pretty-print）JSON 摘要**（真机实证形态：CLI `--json` 是摘要输出，可能跨行）：
    /// 逐行喂法解析不出 → **整段重解析**必须补上（否则真回合会被误报成 channel_error）
    #[test]
    fn frames_of_stdout_handles_pretty_printed_summary() {
        let pretty = vec![
            "ZCode Built-in skipped (not-due)".to_string(),
            "{".to_string(),
            "  \"sessionId\": \"sess_1\",".to_string(),
            "  \"response\": \"已改好\",".to_string(),
            "  \"tokens\": 42".to_string(),
            "}".to_string(),
        ];
        // 逐行（Task 6 原口径）：解析不出对象（每行都不成对象）——这正是误报的来源
        assert_eq!(frames_of(&pretty).json_frames, 0);
        let acc = frames_of_stdout(&pretty);
        assert_eq!(acc.json_frames, 1, "整段重解析必须解出跨行对象");
        assert_eq!(acc.session_id.as_deref(), Some("sess_1"));
        assert_eq!(acc.last_assistant.as_deref(), Some("已改好"));
        assert_eq!(acc.tokens, Some(42));
        assert_eq!(
            acc.noise_lines, 0,
            "整段解释成立时逐行噪音计数失去意义（那些行是同一对象的成员）"
        );
        // 整段也没有 JSON → 保持逐行结论（channel_error 臂不受影响）
        assert_eq!(
            frames_of_stdout(&["ZCode Built-in skipped (not-due)".to_string()]).json_frames,
            0
        );
    }

    // ===== 编排（脚本缝，零真实进程） =====

    /// 脚本执行缝：按尝试序号给不同脚本输出（第 1 次 busy，之后成功）
    fn scripted_seam(
        counter: Arc<std::sync::atomic::AtomicUsize>,
        busy_times: usize,
    ) -> Box<RunSeam> {
        Box::new(move |cfg: RunnerCfg| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut cfg = cfg;
            if n < busy_times {
                cfg = cfg.stdout_lines(vec![format!("Error: {WORKSPACE_BUSY_MARKER}")]);
            } else {
                cfg = cfg.stdout_lines(vec![
                    "ZCode Built-in skipped (not-due)".to_string(),
                    "{\"sessionId\":\"sess_1\",\"response\":\"pong\",\"tokens\":7}".to_string(),
                ]);
            }
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        })
    }

    fn test_deps(active: bool, waits: Arc<Mutex<Vec<u64>>>) -> TurnDeps {
        // 回执源：库读不到（`Unavailable`）——本批「库不参与」的用例沿用（stdout 无 JSON →
        // 如实 channel_error）；库确认路径由 store_* 用例单独驱动
        test_deps_with_store(active, waits, Box::new(|_| None))
    }

    fn test_deps_with_store(
        active: bool,
        waits: Arc<Mutex<Vec<u64>>>,
        store_probe: Box<StoreProbe>,
    ) -> TurnDeps {
        TurnDeps {
            max_busy_retries: BUSY_MAX_RETRIES,
            retry_spacing: Duration::from_millis(BUSY_RETRY_SPACING_MS),
            workspace_active: Box::new(move |_| active),
            wait: Box::new(move |d: Duration| {
                let waits = waits.clone();
                Box::pin(async move {
                    waits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(d.as_millis() as u64);
                })
            }),
            store_probe,
            store_confirm_attempts: STORE_CONFIRM_ATTEMPTS,
            store_confirm_interval: Duration::from_millis(STORE_CONFIRM_INTERVAL_MS),
        }
    }

    /// 争用锁命中一次后成功：回执如实为 ok（含 assistant/tokens/耗时），尝试 2 次，
    /// 退避按 5s 等待；**不冒充失败也不冒充成功**
    #[tokio::test]
    async fn run_turn_retries_once_after_busy_then_succeeds() {
        let inv = build_argv(
            &ZcodeSpec::win("D:/ZCode"),
            "hi",
            "sess_1",
            "E:/p",
            Some("iPad"),
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seam = scripted_seam(counter.clone(), 1);
        let out = run_turn(
            &inv,
            "sess_1",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_1"),
            &test_deps(true, waits.clone()),
            &*seam,
        )
        .await;
        assert_eq!(out.attempts, 2, "busy 一次 → 重试一次");
        assert!(!out.busy_final);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("pong"));
        assert_eq!(out.receipt.tokens, Some(7));
        assert_eq!(
            waits.lock().unwrap().clone(),
            vec![BUSY_RETRY_SPACING_MS],
            "APP 活跃时按 5s 间隔退避"
        );
    }

    /// 一直忙 → 重试用尽：**如实的 WorkspaceBusy 失败回执 +「工作区忙」原因**（绝不冒充成功），
    /// 尝试 3 次（= 重试上限 + 1），退避 2 次
    #[tokio::test]
    async fn run_turn_reports_workspace_busy_after_retries() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_1", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seam = scripted_seam(counter.clone(), usize::MAX);
        let out = run_turn(
            &inv,
            "sess_1",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_1"),
            &test_deps(true, waits.clone()),
            &*seam,
        )
        .await;
        assert_eq!(out.attempts, BUSY_MAX_RETRIES + 1);
        assert!(out.busy_final);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::WorkspaceBusy));
        let reason = out.receipt.reason.unwrap_or_default();
        assert!(reason.contains("工作区忙"), "{reason}");
        assert!(reason.contains("E:/p"), "原因必须点明工作区：{reason}");
        assert!(reason.contains("重试 2 次"), "{reason}");
        assert_eq!(waits.lock().unwrap().len(), BUSY_MAX_RETRIES);
    }

    /// **R9：退避窗内的取消必须收尾为 cancelled**——争用锁拒绝时子进程已经退了，runner 的
    /// 取消靶子随 `disarm` 消失：此刻移动端叫停只落在登记表上，旧实现静默吞掉它、照样再试。
    ///
    /// 还原动作（变异）：把 `run_turn` 退避后的取消检查点删掉 → 本用例先红
    /// （尝试 1 → 3、终态 `failed(workspace_busy)`）。
    #[tokio::test]
    async fn run_turn_honours_cancel_during_busy_backoff() {
        let sid = "sess_zcode_busy_cancel";
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", sid, "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        registry().begin(sid, TurnSlot::placeholder("zcode", "hi".into()));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seam = scripted_seam(runs.clone(), usize::MAX); // 恒忙：旧实现会试满 3 次
        let waits = Arc::new(Mutex::new(Vec::new()));
        let delivery = Arc::new(Mutex::new(None));
        let mut deps = test_deps(true, waits.clone());
        deps.wait = Box::new({
            let waits = waits.clone();
            let delivery = delivery.clone();
            move |d: Duration| {
                let waits = waits.clone();
                let delivery = delivery.clone();
                Box::pin(async move {
                    waits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(d.as_millis() as u64);
                    // 退避窗内到达的取消（生产 = 取消端点经登记表打的同一发）
                    *delivery.lock().unwrap_or_else(|e| e.into_inner()) =
                        registry().request_cancel(sid);
                })
            }
        });
        let out = run_turn(
            &inv,
            sid,
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id(sid),
            &deps,
            &*seam,
        )
        .await;
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "退避窗内叫停后不得再发起任何尝试（执行缝只能被调一次）"
        );
        assert_eq!(out.attempts, 1, "尝试计数不得增加");
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "终态必须是 cancelled（旧实现落到 failed(workspace_busy)）: {:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt.stage, None,
            "取消不是失败阶段——不得报成 timeout / channel_error"
        );
        assert_eq!(out.receipt.session_id, sid, "取消回执带出会话号");
        assert_eq!(
            out.receipt_source,
            ReceiptSource::NotApplicable,
            "取消档不咨询会话库（与 codex cancelled_outcome 同口径）"
        );
        assert!(!out.busy_final, "取消不是「工作区忙」终态");
        assert_eq!(
            receipt_result_word(&out.receipt),
            "cancelled",
            "审计 result 列 = 取消终态词（耗时由 audit_result 追加）"
        );
        assert!(
            crate::inject::headless::audit_result(&receipt_result_word(&out.receipt), 12)
                .starts_with("cancelled · "),
            "审计行按批次口径渲染为「cancelled · <n>ms」"
        );
        assert_eq!(
            *delivery.lock().unwrap_or_else(|e| e.into_inner()),
            Some(true),
            "退避窗内的取消必须如实报「送达」——否则取消端点谎报「回合已终结」且不落取消行"
        );
        assert_eq!(
            registry().request_cancel(sid),
            Some(false),
            "回合终结后的迟到取消不得冒领送达（先到者生效）"
        );
        registry().end(sid);
    }

    /// **R9②（回归锁）：回合已以别的工作区忙终态收尾后，迟到的取消不得被冒领**——
    /// 冒领会让取消端点为一个并非因取消而终结的回合落 `cancelled` 取消审计行
    /// （与回合自身的 `headless` 行口径矛盾）。
    #[tokio::test]
    async fn late_cancel_after_busy_terminal_turn_is_not_claimed() {
        let sid = "sess_zcode_late_cancel";
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", sid, "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        registry().begin(sid, TurnSlot::placeholder("zcode", "hi".into()));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seam = scripted_seam(runs.clone(), usize::MAX);
        let out = run_turn(
            &inv,
            sid,
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id(sid),
            &test_deps(true, Arc::new(Mutex::new(Vec::new()))),
            &*seam,
        )
        .await;
        assert!(out.busy_final, "恒忙 ⇒ 终态就是工作区忙: {:?}", out.receipt);
        assert_eq!(
            registry().request_cancel(sid),
            Some(false),
            "回合已终结（闩已落幕）⇒ 迟到取消如实未送达"
        );
        registry().end(sid);
    }

    /// **R9 微窗口（R12-S3 / 裁决 24c-③）**：闩检查通过之后、下一发**武装**前后到达的取消。
    ///
    /// 窗口（**R9 引入退避窗检查点之后仍在**的一段）：退避后的检查点（`run_turn` 循环尾）与下一发
    /// `registry().arm` 之间还有一段（`make_runner` 读设置 + 武装本身）——落在其中的取消会让
    /// **下一发照跑**：闩记得住（取消端点据此如实报「送达」），但那一发既没被叫停、也白跑一次
    /// （恒忙夹具下 = 多起一次进程；真机上可能跑完整个回合），用户看到的「已取消」与回合实际
    /// 结局矛盾。**复检臂**关掉的正是这段：武装之后、起跑之前再查一次闩。
    ///
    /// 时序构造（本用例的关键）：第 2 发的 `make_runner`（= 武装**之前**那一步）里发取消——
    /// 此刻槽位上的靶子仍是**第 1 发武装的闩靶子**，故这一发正落在闩上（`request_cancel`
    /// 返回 `Some(true)`：进程已退、但回合仍在跑 ⇒ 如实「送达」）。
    ///
    /// 还原动作（变异）：删掉武装后的闩复检 → 本用例先红，**红例形态已实跑核过**（收尾轮 U5
    /// 更正）：执行缝被调 **2** 次（`runs=2`；红在「执行缝只能被调一次」那一条）、
    /// `attempts=2`、由退避后的**循环尾**检查点收尾 ⇒ 终态**仍是 `cancelled`**、登记表槽位
    /// **仍在飞**（留给包装器注销）。即**这一发白跑了一次**，而不是试满 3 次。
    /// ⚠️「试满 3 次 + `failed(workspace_busy)`」是 **R9 退避窗检查点也一并删掉**（= R9 之前的
    /// 基线）时的形态——两个窗口不同，别把本用例的变异写成那个形态。
    #[tokio::test]
    async fn run_turn_honours_cancel_arriving_just_after_arm() {
        use std::sync::atomic::Ordering;
        let sid = "sess_zcode_arm_window";
        let inv =
            build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", sid, "E:/p", None).expect("测试花名在册");
        registry().begin(sid, TurnSlot::placeholder("zcode", "hi".into()));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seam = scripted_seam(runs.clone(), usize::MAX); // 恒忙：不修就会试满 3 次
        let issued = Arc::new(Mutex::new(None));
        let make = {
            let runs = runs.clone();
            let issued = issued.clone();
            move |_: &ZcodeInvocation| {
                // 第 2 发武装之前发取消（只发一次——第一发已跑过执行缝即说明闩检查已通过）
                if runs.load(Ordering::SeqCst) >= 1 {
                    let mut g = issued.lock().unwrap_or_else(|e| e.into_inner());
                    if g.is_none() {
                        *g = registry().request_cancel(sid);
                    }
                }
                RunnerCfg::for_test().session_id(sid)
            }
        };
        let out = run_turn(
            &inv,
            sid,
            "E:/p",
            &make,
            &test_deps(true, Arc::new(Mutex::new(Vec::new()))),
            &*seam,
        )
        .await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "微窗口内叫停后不得再发起任何尝试（执行缝只能被调一次）"
        );
        assert_eq!(
            out.attempts, 2,
            "第 2 发在**起跑前**收尾（尝试计数只到 2，不再 +1）"
        );
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "终态必须是 cancelled（旧实现落到 failed(workspace_busy)）: {:?}",
            out.receipt
        );
        assert_eq!(
            out.receipt.stage, None,
            "取消不是失败阶段——不得报成 timeout / channel_error"
        );
        assert_eq!(out.receipt.session_id, sid, "取消回执带出会话号");
        assert_eq!(
            out.receipt_source,
            ReceiptSource::NotApplicable,
            "取消档不咨询会话库（与退避窗臂同口径）"
        );
        assert!(!out.busy_final, "取消不是「工作区忙」终态");
        assert_eq!(receipt_result_word(&out.receipt), "cancelled");
        assert_eq!(
            *issued.lock().unwrap_or_else(|e| e.into_inner()),
            Some(true),
            "微窗口里的取消必须如实报「送达」（闩记得住——否则取消端点谎报「回合已终结」）"
        );
        // **登记表本身**的断言（不只是回执）：槽位**仍在飞**——注销归**包装器**（同一 detached
        // 任务里写完审计行之后），本臂**不代庖**（单主不变量；竞态与红例见
        // `post_arm_cancel_does_not_end_the_slot_the_wrapper_owns`）
        assert!(
            registry().in_flight(sid),
            "复检臂不得注销槽位（注销归包装器：remote::api::run_zcode_turn 的审计行之后）"
        );
        // 包装器注销（同形动作）→ 会话锁释放、不留已武装靶子
        registry().end(sid);
        assert!(
            !registry().in_flight(sid),
            "包装器注销后会话锁必须释放（否则该会话被自己的串行锁挡死）"
        );
        assert!(
            registry().cancel_snapshot(sid).is_none(),
            "包装器注销后不得留下已武装靶子（残留占位会让下一次取消请求打在一个已终结的回合上）"
        );
    }

    /// **U1 · 单主不变量：复检臂不得注销它不拥有的槽位**（收尾轮，2026-10-05）。
    ///
    /// 上一轮的复检臂在返回 `cancelled` 之前顺手 `registry().end(session_id)`——可它**不是**
    /// 槽位的主人：`begin`/`end` 都归**包装器**（`remote::api::run_zcode_turn` 的
    /// `registry().end(&ctx.sid)` 落在审计行**之后**；新建通道同形，见 `session_create_zcode`
    /// 的 `registry().end(&lock_key)`）。误删竞态（本用例钉死的就是它）：
    /// ① 复检命中 → `run_turn` 抢先注销**旧**槽位；② 包装器还要写一行审计（SQLite 写），
    /// 这个窗口里移动端立刻发下一条 ⇒ **新回合 `begin()` 成功**（旧槽位已不在，串行锁放行）；
    /// ③ 包装器回到 `registry().end(sid)`——按**会话号**删，删掉的正是**新**回合的槽位
    /// （新回合此后无取消靶子、串行锁形同虚设，而它自己的 `end` 还会再去删下一条……）。
    ///
    /// 断言三件事：① 包装器注销之前的 `begin()` 必须被串行锁**如实拒绝**——旧槽位仍在飞就
    /// 没有「新槽位」，也就不存在能被陈旧 `end` 误删的对象（**红例即此条**：旧实现里 begin
    /// 竟成功）；② 上一条的**因** = **单主不变量**（复检命中后旧槽位必须还在，旧实现此处已删）；
    /// ③ 包装器注销之后新回合方能占位，且此后**没有任何陈旧 `end`** 会波及它（新槽位存活）。
    ///
    /// 还原动作（变异）：把 `registry().end(session_id)` 加回复检臂 → ① 先红（`begin` 成功）、
    /// ② 随之也红（`in_flight`/快照为空）。
    #[tokio::test]
    async fn post_arm_cancel_does_not_end_the_slot_the_wrapper_owns() {
        use std::sync::atomic::Ordering;
        let sid = "sess_zcode_slot_owner";
        let inv =
            build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", sid, "E:/p", None).expect("测试花名在册");
        assert!(
            registry().begin(sid, TurnSlot::placeholder("zcode", "first".into())),
            "包装器占位（生产 = zcode_headless_dispatch 的串行锁臂）"
        );
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seam = scripted_seam(runs.clone(), usize::MAX); // 恒忙：不修就会试满 3 次

        // 与 S3 用例同一时序构造：第 2 发武装**之前**发取消（落在第 1 发武装的闩靶子上）
        let issued = Arc::new(Mutex::new(None));
        let make = {
            let runs = runs.clone();
            let issued = issued.clone();
            move |_: &ZcodeInvocation| {
                if runs.load(Ordering::SeqCst) >= 1 {
                    let mut g = issued.lock().unwrap_or_else(|e| e.into_inner());
                    if g.is_none() {
                        *g = registry().request_cancel(sid);
                    }
                }
                RunnerCfg::for_test().session_id(sid)
            }
        };
        let out = run_turn(
            &inv,
            sid,
            "E:/p",
            &make,
            &test_deps(true, Arc::new(Mutex::new(Vec::new()))),
            &*seam,
        )
        .await;
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "复检命中 ⇒ 回执仍是 cancelled（只有槽位归属变，回执不变）: {:?}",
            out.receipt
        );
        assert_eq!(
            *issued.lock().unwrap_or_else(|e| e.into_inner()),
            Some(true),
            "这一发必须如实报「送达」（闩记得住）"
        );
        // ① 竞态本体的**否定面**（先断言它，红例即竞态本身）：包装器的审计窗口里，移动端
        //    发下一条 ⇒ 新回合占位**必须被串行锁如实拒绝**（旧槽位仍在飞 ⇒ 不存在「新槽位」，
        //    也就没有能被陈旧 `end` 误删的对象）。旧实现此处 begin 成功 = 竞态成立。
        let old_slot = registry().slot_snapshot(sid).map(|(c, _, _)| c);
        let started_new = registry().begin(sid, TurnSlot::placeholder("zcode", "second".into()));
        assert!(
            !started_new,
            "复检臂抢先注销旧槽位 ⇒ 包装器审计窗口内的新回合得以占位（begin={started_new}）\
             ⇒ 包装器随后按会话号注销时删掉的正是**新**回合的槽位（竞态本体）"
        );
        // ② 上一条的**因**（单主不变量）：复检臂只回回执，不动别人的槽位
        assert_eq!(
            old_slot.as_deref(),
            Some("first"),
            "复检臂不得 end() 它不拥有的槽位（注销归包装器：审计行之后）——旧实现此处槽位已空"
        );
        assert_eq!(
            registry().slot_snapshot(sid).map(|(c, _, _)| c).as_deref(),
            Some("first"),
            "旧槽位（正文/工具/通道）必须原样留存，直到包装器注销"
        );
        // ③ 包装器注销（= `remote::api::run_zcode_turn` 的同形动作）之后，新回合才能占位；
        //    此后没有任何陈旧 end 会波及它 —— 新槽位存活
        registry().end(sid);
        assert!(!registry().in_flight(sid), "包装器注销后旧槽位释放");
        assert!(
            registry().begin(sid, TurnSlot::placeholder("zcode", "second".into())),
            "旧槽位释放后，新回合才能占位"
        );
        assert_eq!(
            registry().slot_snapshot(sid).map(|(c, _, _)| c).as_deref(),
            Some("second"),
            "新回合槽位存活（不被任何陈旧 end 波及）"
        );
        registry().end(sid);
    }

    /// APP 已离开该工作区 → 立即重试（不空等）；0 退出但**无 JSON 帧** → channel_error
    /// （不把「没产出」报成 ok——Mac 缺 provider config 的静默形态）
    #[tokio::test]
    async fn run_turn_retries_immediately_when_idle_and_flags_empty_output() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_1", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seam = scripted_seam(counter, 1);
        let out = run_turn(
            &inv,
            "sess_1",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_1"),
            &test_deps(false, waits.clone()), // 探活：APP 已不在该工作区
            &*seam,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok);
        assert!(
            waits.lock().unwrap().is_empty(),
            "探活为闲 → 立即重试，不空等 5s"
        );

        // 无 JSON 帧的空产出：0 退出也必须报 channel_error
        let empty: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg;
            cfg = cfg.stdout_lines(vec!["ZCode Built-in skipped (not-due)".to_string()]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let out = run_turn(
            &inv,
            "sess_1",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_1"),
            &test_deps(false, Arc::new(Mutex::new(Vec::new()))),
            &*empty,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        assert_eq!(out.receipt_source, ReceiptSource::Unconfirmed);
        assert!(
            out.receipt
                .reason
                .unwrap_or_default()
                .contains("会话库不可读，无法确认"),
            "库不可读时必须如实说「无法确认」（不是「没产出」）"
        );

        // 多行 JSON 摘要（真机形态）→ 整段重解析补全回执（不再误报 channel_error）
        let pretty: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg;
            cfg = cfg.stdout_lines(vec![
                "ZCode Built-in skipped (not-due)".to_string(),
                "{".to_string(),
                "  \"sessionId\": \"sess_1\",".to_string(),
                "  \"response\": \"pong\",".to_string(),
                "  \"tokens\": 11".to_string(),
                "}".to_string(),
            ]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let out = run_turn(
            &inv,
            "sess_1",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_1"),
            &test_deps(false, Arc::new(Mutex::new(Vec::new()))),
            &*pretty,
        )
        .await;
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("pong"));
        assert_eq!(out.receipt.tokens, Some(11));
        assert_eq!(out.receipt_source, ReceiptSource::StdoutJson);
    }

    // ===== 回执源：会话库确认（Task 8 复审追补；H6 契约的真源）=====

    fn snap(seq: i64, id: Option<&str>, text: Option<&str>, tokens: Option<u64>) -> StoreSnapshot {
        StoreSnapshot {
            last_seq: seq,
            last_assistant_id: id.map(str::to_string),
            last_assistant: text.map(str::to_string),
            tokens,
        }
    }

    /// 脚本库读缝：按序吐出队列里的快照（队列空 → None = 读不到）
    fn store_queue(items: Vec<Option<StoreSnapshot>>) -> (Box<StoreProbe>, Arc<Mutex<usize>>) {
        let q = Arc::new(Mutex::new(items));
        let calls = Arc::new(Mutex::new(0usize));
        let calls2 = calls.clone();
        let probe: Box<StoreProbe> = Box::new(move |_sid: &str| {
            *calls2.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                None
            } else {
                q.remove(0)
            }
        });
        (probe, calls)
    }

    /// 纯核：确认判据 = 末条 assistant **消息**变新（按 id；同文本两轮也能区分）；
    /// 只落用户消息（assistant 未变）**不算**确认；无 assistant 文本不算确认
    #[test]
    fn confirm_from_store_requires_a_new_assistant_message() {
        let base = snap(10, Some("m1"), Some("上一轮回复"), Some(5));
        // 新 assistant 消息（文本相同也算——按 id 区分）
        assert_eq!(
            confirm_from_store(&base, &snap(14, Some("m2"), Some("上一轮回复"), Some(9))),
            StoreConfirmation::NewAssistant {
                text: "上一轮回复".into(),
                tokens: Some(9)
            },
            "同一文本的新一轮回复必须确认（id 变了）"
        );
        // 只落了用户消息：assistant 未变 → 不确认
        assert_eq!(
            confirm_from_store(&base, &snap(11, Some("m1"), Some("上一轮回复"), Some(5))),
            StoreConfirmation::NoNewAssistant
        );
        // 库里没有 assistant 文本 → 不确认
        assert_eq!(
            confirm_from_store(&base, &snap(11, None, None, None)),
            StoreConfirmation::NoNewAssistant
        );
        // 全新会话（基线无 assistant）→ 有 assistant 即确认
        assert_eq!(
            confirm_from_store(
                &snap(0, None, None, None),
                &snap(2, Some("m9"), Some("首次回复"), None)
            ),
            StoreConfirmation::NewAssistant {
                text: "首次回复".into(),
                tokens: None
            }
        );
        // id 不可得（老库/脏行）→ 退化为文本比较（宁可漏确认，不可假确认）
        assert_eq!(
            confirm_from_store(
                &snap(3, None, Some("x"), None),
                &snap(5, None, Some("x"), None)
            ),
            StoreConfirmation::NoNewAssistant
        );
        assert!(matches!(
            confirm_from_store(
                &snap(3, None, Some("x"), None),
                &snap(5, None, Some("y"), None)
            ),
            StoreConfirmation::NewAssistant { .. }
        ));
    }

    /// **真机形态**：`--resume` 回合 exit 0 且 stdout 无 JSON，但会话库有**新的 assistant
    /// 回复** → 回执 Ok + 摘要 + tokens（来源 = 会话库）——不再误报 channel_error，
    /// 也把 H6 的 `lastAssistant`/`tokens`/`durationMs` 真正兑现
    #[tokio::test]
    async fn run_turn_builds_receipt_from_session_store() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_db", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let seam = scripted_seam(Arc::new(std::sync::atomic::AtomicUsize::new(0)), 0);
        // 逐行无 JSON 的 stdout（真机 resume 形态）
        let empty_stdout: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg;
            cfg = cfg.stdout_lines(vec!["ZCode Built-in skipped (not-due)".to_string()]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        drop(seam);
        let (probe, calls) = store_queue(vec![
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))), // 回合前基线
            Some(snap(14, Some("m2"), Some("本轮回复正文"), Some(16493))), // 回合后
        ]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let out = run_turn(
            &inv,
            "sess_db",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_db"),
            &test_deps_with_store(false, waits.clone(), probe),
            &*empty_stdout,
        )
        .await;
        assert_eq!(out.receipt_source, ReceiptSource::SessionStore);
        assert_eq!(out.receipt.status, ReceiptStatus::Ok, "{:?}", out.receipt);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("本轮回复正文"));
        assert_eq!(out.receipt.tokens, Some(16493), "tokens 取自库（不编数字）");
        assert_eq!(out.receipt.session_id, "sess_db");
        assert_eq!(out.receipt.duration_ms, out.receipt.duration_ms); // 墙钟由编排给
        assert_eq!(*calls.lock().unwrap(), 2, "基线一次 + 回合后一次");
        assert!(
            waits.lock().unwrap().is_empty(),
            "首次读库即确认 → 不触发轮询等待"
        );
    }

    /// 库可读但**没有新的 assistant 回复**（如只落了用户消息）→ 如实的不确认（channel_error，
    /// 不冒充成功）；原因点明「库里没有新的 assistant 回复」
    #[tokio::test]
    async fn run_turn_reports_unconfirmed_when_store_shows_no_new_assistant() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_db2", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let empty_stdout: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg;
            cfg = cfg.stdout_lines(vec!["ZCode Built-in skipped (not-due)".to_string()]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let (probe, calls) = store_queue(vec![
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))),
            Some(snap(11, Some("m1"), Some("上一轮"), Some(5))), // 只多了用户消息
            Some(snap(11, Some("m1"), Some("上一轮"), Some(5))),
            Some(snap(11, Some("m1"), Some("上一轮"), Some(5))),
        ]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let out = run_turn(
            &inv,
            "sess_db2",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_db2"),
            &test_deps_with_store(false, waits.clone(), probe),
            &*empty_stdout,
        )
        .await;
        assert_eq!(out.receipt_source, ReceiptSource::Unconfirmed);
        assert_eq!(out.receipt.status, ReceiptStatus::Failed);
        assert_eq!(out.receipt.stage, Some(Stage::ChannelError));
        let reason = out.receipt.reason.clone().unwrap_or_default();
        assert!(
            reason.contains("本轮新的 assistant 回复") && reason.contains("没有"),
            "原因必须点明库里的实际情况：{reason}"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            STORE_CONFIRM_ATTEMPTS + 1,
            "有界轮询：基线 1 次 + 未确认时读满 {} 次",
            STORE_CONFIRM_ATTEMPTS
        );
        assert_eq!(
            waits.lock().unwrap().len(),
            STORE_CONFIRM_ATTEMPTS - 1,
            "轮询间隔等待 = 读取次数 - 1（有界，不无限等）"
        );
    }

    /// 库写入**滞后**：前两次读还没看到新回复、第三次读到 → **有界轮询**必须等到它
    /// （懒落库/WAL 可见性），并如实给 Ok 回执
    #[tokio::test]
    async fn run_turn_polls_store_until_the_write_lands() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_db3", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let empty_stdout: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            let mut cfg = cfg;
            cfg = cfg.stdout_lines(vec!["ZCode Built-in skipped (not-due)".to_string()]);
            Box::pin(async move {
                let receipt = cfg.run_once(|_| {});
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        let (probe, calls) = store_queue(vec![
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))),
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))), // 滞后：还没落库
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))),
            Some(snap(13, Some("m2"), Some("终于落库了"), Some(7))), // 落库
        ]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let out = run_turn(
            &inv,
            "sess_db3",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_db3"),
            &test_deps_with_store(false, waits.clone(), probe),
            &*empty_stdout,
        )
        .await;
        assert_eq!(out.receipt_source, ReceiptSource::SessionStore);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("终于落库了"));
        assert_eq!(out.receipt.tokens, Some(7));
        assert_eq!(*calls.lock().unwrap(), 4);
        assert_eq!(
            waits.lock().unwrap().clone(),
            vec![STORE_CONFIRM_INTERVAL_MS, STORE_CONFIRM_INTERVAL_MS],
            "两次滞后各等一个间隔（界内）"
        );
    }

    /// stdout JSON **优先于**库（Task 6 原口径不退化；未来子命令仍可能出 JSON）
    #[tokio::test]
    async fn run_turn_prefers_stdout_json_over_store() {
        let inv = build_argv(&ZcodeSpec::win("D:/ZCode"), "hi", "sess_db4", "E:/p", None)
            .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        let seam = scripted_seam(Arc::new(std::sync::atomic::AtomicUsize::new(0)), 0);
        let (probe, calls) = store_queue(vec![
            Some(snap(10, Some("m1"), Some("上一轮"), Some(5))),
            Some(snap(14, Some("m2"), Some("库里的新回复"), Some(999))),
        ]);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let out = run_turn(
            &inv,
            "sess_db4",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_db4"),
            &test_deps_with_store(false, waits.clone(), probe),
            &*seam,
        )
        .await;
        assert_eq!(out.receipt_source, ReceiptSource::StdoutJson);
        assert_eq!(out.receipt.last_assistant.as_deref(), Some("pong"));
        assert_eq!(out.receipt.tokens, Some(7), "stdout 帧的 tokens 优先");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "stdout 命中即不读库第二遍（只留基线那一次）"
        );
    }

    /// 取消（Task 6 语义透传）：回合进行中收到取消 → Cancelled 回执（stage 留空），
    /// 且登记表能拿到取消靶子
    #[tokio::test]
    async fn run_turn_passes_cancel_through() {
        let inv = build_argv(
            &ZcodeSpec::win("D:/ZCode"),
            "hi",
            "sess_cancel",
            "E:/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        registry().begin("sess_cancel", TurnSlot::placeholder("zcode", "hi".into()));
        let seam: Box<RunSeam> = Box::new(|cfg: RunnerCfg| {
            Box::pin(async move {
                let mut cfg = cfg.timeout_ms(5_000);
                let handle = cfg.cancel_handle();
                let t = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    handle.cancel()
                });
                let receipt = cfg.run_once(|_| std::thread::sleep(Duration::from_millis(400)));
                let _ = t.join();
                TurnObs {
                    receipt,
                    stdout: cfg.captured_stdout(),
                    stderr: cfg.captured_stderr(),
                    exit: cfg.last_exit_code(),
                }
            })
        });
        // 登记表靶子由 run_turn 逐尝试 arm（版本门控期间为空占位）→ 起跑后请求取消
        let deps = test_deps(false, Arc::new(Mutex::new(Vec::new())));
        let turn = run_turn(
            &inv,
            "sess_cancel",
            "E:/p",
            &|_: &ZcodeInvocation| RunnerCfg::for_test().session_id("sess_cancel"),
            &deps,
            &*seam,
        );
        let out = turn.await;
        assert_eq!(
            out.receipt.status,
            ReceiptStatus::Cancelled,
            "取消必须如实透传（不落 failed）: {:?}",
            out.receipt
        );
        assert_eq!(out.receipt.stage, None, "取消不是失败阶段");
        assert!(
            registry().in_flight("sess_cancel"),
            "run_turn 不注销槽位（终结归端点）"
        );
        registry().end("sess_cancel");
        assert!(!registry().in_flight("sess_cancel"));
    }

    /// 会话串行锁：同会话第二次 begin 必须失败（如实拒绝，不排队不覆盖）；取消未送达
    /// 时 `Some(false)`（先到者生效）；无在飞回合 → None；槽位快照供取消审计行取正文
    #[test]
    fn registry_serializes_sessions_and_reports_cancel_delivery() {
        let reg = TurnRegistry::default();
        assert!(reg.begin("s1", TurnSlot::placeholder("zcode", "正文".into())));
        assert!(
            !reg.begin("s1", TurnSlot::placeholder("zcode", "第二条".into())),
            "同会话重叠回合必须被拒（MAM 串行锁）"
        );
        assert!(reg.begin("s2", TurnSlot::placeholder("zcode", "别的会话".into())));
        assert_eq!(
            reg.request_cancel("s1"),
            Some(false),
            "占位靶子（尚未起跑/已终结）→ 取消未送达"
        );
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hit2 = hit.clone();
        reg.arm(
            "s1",
            Arc::new(move || {
                hit2.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        assert_eq!(reg.request_cancel("s1"), Some(true), "已武装靶子 → 送达");
        assert!(hit.load(std::sync::atomic::Ordering::SeqCst));
        let (content, agent_type, _ms) = reg.slot_snapshot("s1").unwrap();
        assert_eq!(content, "正文", "取消审计行取回合原文（与回合行同源）");
        assert_eq!(agent_type, "zcode", "取消审计行的工具列取回合会话工具");
        assert_eq!(reg.request_cancel("nope"), None);
        reg.end("s1");
        assert!(!reg.in_flight("s1"));
        assert_eq!(reg.request_cancel("s1"), None, "终结后取消无靶子");
        reg.end("s2");
    }
}
