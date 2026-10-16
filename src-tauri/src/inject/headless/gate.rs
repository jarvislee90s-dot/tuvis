// 版本门控探针（H6）：投递前校验各工具 flag 面，结果缓存 + 版本变化提示复核，不盲发。
//
// **探测定案（spec 附录 E / H7，测试断言以此为真值）**：
// - zcode：**必须 `--prompt` 真回合**——`--version`/`--help` 不需要 provider config，
//   会漏判「Mac 打包布局 bug 下缺 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 时 `--prompt`
//   静默无 JSON」。**两级化（裁决 33a）**：L0 快面（exe/cjs/provider-config
//   在场性 stat + `--help` 能起能印，零模型成本）过了才跑 L1 真回合——
//   **L0 是串联闸门不是替代**（`--help` 漏判静默形态，Task 8 实证教训）；
//   L1 超时被杀后**先看已捕获 stdout**（B 主修，裁决 29c：证据优先于
//   时序——有 JSON = 回合已完成即过），零输出才按超时 fail；
// - codex：`queue --help` 子命令在场（experimental 面，升级可能消失）。
//
// 结果缓存键 = **可执行文件在场性 + mtime**（含辅助产物如 zcode.cjs）——工具升级即
// 失效重探（版本漂移复核）；进程缺席直接判失败，绝不 spawn 一个不存在的程序。
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::receipt::{parse_json_skipping_prefix, Receipt, Stage};
use super::runner::{GlobalSem, RunnerCfg};
// H5 权限档（Task 10）：zcode 探针的档位旗子也从 spec 取——探针与真回合**同一份**口径
use super::PermissionSpec;

/// 探针回合超时（Task 2 / 裁决 29a+A：45s）。探针是**真实最小模型回合**（见
/// [`PROBE_PROMPT`]），本机实测最小回合 15–19s（空目录 18s / repo 15s，探测报告 §2）——
/// 旧值 15s 会把通道恒超时判死；取实测上界 ×2+ 余量。超时分支先看已捕获
/// stdout（B 主修），零输出才按本阈值 fail（hint 的秒数从本常量格式化生成）。
pub const PROBE_TIMEOUT_MS: u64 = 45_000;

/// L0 帮助面超时（裁决 33a）：`--help` 实测 0.9s 能起能印——10s 已是十倍
/// 余量；帮助面 10s 起不来即结构性坏（与 L1 真回合阈值无关，**不共享常量**）。
const L0_HELP_TIMEOUT_MS: u64 = 10_000;
/// **探针载荷（Task 8 真机实证修订）**：真机 ZCode CLI **拒绝空载荷**——
/// `--prompt ""` → `--prompt requires non-empty text.`（exit=1、0.5s 内退出、**不触模型**），
/// 故 Task 6 设计的「空串干跑」在真机恒失败（会让版本门控拒绝掉全部投递）。探针改用
/// **最短真实载荷**：一次极小回合（成本 = 一次最小模型往返，结论按 exe/cjs mtime 缓存，
/// 工具升级才重探）。取值与用户配额纪律一致（`hi` = 最短可用载荷）。
pub const PROBE_PROMPT: &str = "hi";
/// macOS zcode 的 provider config 环境变量名（spec H7：Mac 打包布局 bug 的对策，
/// 不设则 `--prompt` 静默无 JSON）
pub const ZCODE_PROVIDER_CONFIG_ENV: &str = "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE";

/// 探针对象（argv/env 由本模块派生）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSpec {
    /// zcode：Windows = `ELECTRON_RUN_AS_NODE=1 <exe> <cjs> …`；
    /// macOS = 同骨架 + provider config env（见 [`probe_env`]）
    Zcode { exe: String, cjs: String },
    /// codex（原生 CLI）
    Codex { exe: String },
}

impl ProbeSpec {
    pub fn exe(&self) -> &str {
        match self {
            ProbeSpec::Zcode { exe, .. } | ProbeSpec::Codex { exe } => exe,
        }
    }

    /// 辅助产物（zcode.cjs）：也参与缓存键（随主程序一起升级）
    pub fn aux_path(&self) -> Option<&str> {
        match self {
            ProbeSpec::Zcode { cjs, .. } => Some(cjs),
            ProbeSpec::Codex { .. } => None,
        }
    }
}

/// 探针 argv（**不含 exe 自身**；生产 spawn = exe + 本 argv + [`probe_env`]）
///
/// **H5 权限档（Task 10）**：zcode 面的档位旗子来自 [`PermissionSpec::zcode_default`]——
/// 与真回合（`zcode::build_argv`）**同一份 spec**，故不存在「探针探 yolo、回合 spawn 别的档」
/// 的静默漂移：档位一旦收紧/加变体，探针跟着探新档。测试
/// `zcode_probe_argv_mode_comes_from_the_permission_spec` 把探针与真回合钉在同一段旗子上。
pub fn probe_argv(spec: &ProbeSpec) -> Vec<String> {
    match spec {
        // `--prompt <最短非空载荷>` 干跑：真机 CLI 拒绝空串（见 [`PROBE_PROMPT`]）；
        // provider 缺失/打包错位时这里就出不了 JSON → 门控拦下。
        // 档位段（`--mode <档>`）插在 `--json` 之前——与真回合的 flag 序一致（H5）
        ProbeSpec::Zcode { cjs, .. } => {
            let mut argv = vec![cjs.clone(), "--prompt".into(), PROBE_PROMPT.into()];
            argv.extend(
                PermissionSpec::zcode_default()
                    .flags()
                    .into_iter()
                    .map(str::to_string),
            );
            argv.push("--json".into());
            argv
        }
        ProbeSpec::Codex { .. } => vec!["queue".into(), "--help".into()],
    }
}

/// macOS provider config 路径推导：`<Resources>/glm/zcode.cjs` →
/// `<Resources>/config/provider/zcode-builtin.json`（从 cjs 反推装包根，不需要额外入参）
pub fn zcode_provider_config_path(cjs: &str) -> Option<String> {
    let p = std::path::Path::new(cjs);
    let resources = p.parent()?.parent()?;
    Some(
        resources
            .join("config")
            .join("provider")
            .join("zcode-builtin.json")
            .to_string_lossy()
            .to_string(),
    )
}

/// **provider config 环境变量（两端共用单点；Task 8 真机实证修订）**。
///
/// # 真机实证（2026-10-05，本机 Windows 11 + ZCode `D:\Program Files\ZCode`）
/// 不设该变量时 `--prompt` 干跑**直接失败**（不是静默）：
/// `无法定位 CLI ZCode Built-in Provider Config：<root>\resources\glm\provider\zcode-builtin.json,
/// <盘>:\config\provider\zcode-builtin.json`（exit=1，0.5s 内退出、不触模型）。
/// 而装包**实际**把文件放在 `<root>\resources\config\provider\zcode-builtin.json`
/// （真机 stat 实证）——**与 Mac 同款的打包布局错位**：CLI 的查找表里没有打包的真实位置。
/// 两端的正确路径推导**同一个**（cjs 的祖父目录 + `config/provider/zcode-builtin.json`），
/// 故本函数按「cjs → 推导路径」取值，供探针与真回合共用（**单一出口**，两处不得各写一份）。
///
/// # 平台规则（有意不对称，各有实证依据）
/// - macOS：**无条件**给（spec H7 定案：不设则 `--prompt` **静默无 JSON**——静默形态
///   比报错更危险，宁可指向推导路径让 CLI 明确报「定位不到」）；
/// - Windows：**推导路径在场才给**（真机实证该位置就是打包位置；若某天装包修好、文件
///   挪去 `<resources>/glm/provider/`，本函数自动不设，让 CLI 走自己的查找——不指向
///   一个不存在的文件）。
///
/// 返回 `None` = 本平台不设该变量（CLI 走自身查找）。
pub fn provider_config_env(cjs: &str, os: &str) -> Option<(String, String)> {
    let path = zcode_provider_config_path(cjs)?;
    if os == "macos" || std::path::Path::new(&path).is_file() {
        Some((ZCODE_PROVIDER_CONFIG_ENV.to_string(), path))
    } else {
        None
    }
}

/// 探针环境（平台分叉）：Electron 主程序当 node 跑恒定；provider config 见
/// [`provider_config_env`]（Windows 亦适用——真机实证）。
pub fn probe_env(spec: &ProbeSpec, os: &str) -> Vec<(String, String)> {
    match spec {
        ProbeSpec::Zcode { cjs, .. } => {
            let mut env = vec![("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())];
            if let Some(kv) = provider_config_env(cjs, os) {
                env.push(kv);
            }
            env
        }
        // 原生 CLI 不带 Electron 开关（带着反而会被误当 Electron 参数）
        ProbeSpec::Codex { .. } => Vec::new(),
    }
}

/// 探针结论（失败即 version_gate 拒发，H6）。`duration_ms` = **实测墙钟**（Task 2：
/// 由探针编排层起止计时回填；纯判定函数构造的结论不带墙钟（默认 0），
/// [`version_gate_receipt`] 据此透传给回执——审计行不再「· 0ms」误导）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    Pass { detail: String, duration_ms: u64 },
    Fail { hint: String, duration_ms: u64 },
}

impl ProbeVerdict {
    pub fn pass(detail: impl Into<String>) -> Self {
        ProbeVerdict::Pass {
            detail: detail.into(),
            duration_ms: 0,
        }
    }

    pub fn fail(hint: impl Into<String>) -> Self {
        ProbeVerdict::Fail {
            hint: hint.into(),
            duration_ms: 0,
        }
    }

    /// 回填实测墙钟（探针编排层调用；纯判定函数不知道墙钟）
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        match &mut self {
            ProbeVerdict::Pass { duration_ms: d, .. }
            | ProbeVerdict::Fail { duration_ms: d, .. } => *d = duration_ms,
        }
        self
    }

    /// 实测墙钟（0 = 纯判定构造、未经编排层计时）
    pub fn duration_ms(&self) -> u64 {
        match self {
            ProbeVerdict::Pass { duration_ms, .. } | ProbeVerdict::Fail { duration_ms, .. } => {
                *duration_ms
            }
        }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, ProbeVerdict::Pass { .. })
    }

    pub fn hint(&self) -> Option<&str> {
        match self {
            ProbeVerdict::Fail { hint, .. } => Some(hint),
            ProbeVerdict::Pass { .. } => None,
        }
    }
}

/// 拒发回执（H6：探针失败 = version_gate 拒发回执——通过时 `None`，不生成多余回执）
pub fn version_gate_receipt(session_id: &str, verdict: &ProbeVerdict) -> Option<Receipt> {
    match verdict {
        ProbeVerdict::Pass { .. } => None,
        ProbeVerdict::Fail { hint, .. } => Some(
            Receipt::failed(
                Stage::VersionGate,
                &format!("GATE_VERSION_REFUSED: 版本门控未通过（拒发，不盲发）；{hint}"),
            )
            .with_session(session_id)
            // Task 2 duration 透传：审计行出真实耗时（旧态是回执默认值的「· 0ms」）
            .with_duration_ms(verdict.duration_ms()),
        ),
    }
}

/// zcode 探针判定（纯函数）：**能出 JSON 即过**（前缀噪音行不算失败）；
/// 缺 provider config 的 Mac 专档给环境变量名修复提示。
pub fn zcode_verdict(stdout: &str, stderr_tail: &str, exit: Option<i32>, os: &str) -> ProbeVerdict {
    if parse_json_skipping_prefix(stdout).is_some() {
        return ProbeVerdict::pass("zcode --prompt 干跑出 JSON");
    }
    let detail = crate::inject::normalize::summarize(
        stderr_tail,
        crate::inject::normalize::AUDIT_SUMMARY_CHARS,
    );
    let mac_hint = if os == "macos" {
        format!(
            "GATE_MAC_PROVIDER_HINT: Mac 打包布局下须设 {ZCODE_PROVIDER_CONFIG_ENV}=<Resources>/config/provider/zcode-builtin.json（不设则 --prompt 静默无 JSON）"
        )
    } else {
        "GATE_GENERIC_PROVIDER_HINT: 检查 ZCode 安装与 provider 配置（--prompt 干跑无 JSON 输出）"
            .to_string()
    };
    ProbeVerdict::fail(format!(
        "GATE_ZCODE_NO_JSON: zcode --prompt 干跑未出 JSON（exit={exit:?}）{mac_hint}；stderr={detail}"
    ))
}

/// codex 探针判定（纯函数）：exit 0 且 usage 提及 `queue` 子命令才算过
pub fn codex_verdict(stdout: &str, stderr_tail: &str, exit: Option<i32>) -> ProbeVerdict {
    if exit == Some(0) && stdout.contains("queue") {
        return ProbeVerdict::pass("codex queue 子命令在场");
    }
    let detail = crate::inject::normalize::summarize(
        stderr_tail,
        crate::inject::normalize::AUDIT_SUMMARY_CHARS,
    );
    ProbeVerdict::fail(format!(
        "GATE_CODEX_QUEUE_MISSING: codex queue 子命令不在场（experimental 面，升级可能消失；exit={exit:?}）；stderr={detail}"
    ))
}

// ============================================================
// Task 2（P0）：B 超时先看 stdout + L0 快面（裁决 29c / 33a）
// ============================================================

/// B 主修（裁决 29c）：**证据优先于时序**——watchdog 超时被杀后，先看已捕获
/// stdout：有 JSON = 进程真跑完了回合（只是超阈值被杀，stdout 是完成证据），
/// 判 **Pass**；零输出才按超时 fail。前提：runner kill 路径不丢弃已捕获
/// 缓冲（`runner::tests::runner_kill_preserves_captured_stdout_buffer` 钉死）。
fn timeout_verdict(captured_stdout: &str) -> ProbeVerdict {
    if parse_json_skipping_prefix(captured_stdout).is_some() {
        return ProbeVerdict::pass(
            "探针超时被杀但 stdout 已出 JSON——回合实际完成（证据优先于时序）",
        );
    }
    ProbeVerdict::fail(timeout_zero_output_hint())
}

/// 超时零输出的 fail hint（A 兜底文案：不再误导归因「挂死/缺 provider
/// config」——探针已是真实最小回合，超时更可能是网络/模型服务侧慢；
/// 秒数从 [`PROBE_TIMEOUT_MS`] 格式化生成，不写死字面——改阈值时文案跟着走）。
/// **`pub(crate)`（返工 M-2）**：`codex::probe_cli` 的超时分支复用本函数——
/// 两通道共用同一份文案生成，禁止各自维护副本漂移。
pub(crate) fn timeout_zero_output_hint() -> String {
    format!(
        "GATE_PROBE_TIMEOUT: 探针回合超时（本机实测最小回合 15–19s，阈值已放宽至 {}s）——反复超时请检查网络/模型服务",
        PROBE_TIMEOUT_MS / 1000
    )
}

/// L0 的 provider config 在场性（与 [`provider_config_env`] 的平台规则同源）：
/// - macOS：推导路径必须在场（env 无条件指向它，不在场 = CLI 必然定位不到）；
/// - Windows/其他：推导路径 **或** CLI 自查首选位（`<resources>/glm/provider/`）任一
///   在场——装包修好、文件挪去 CLI 自查位后不误杀（与
///   [`provider_config_env`] 的同一不对称：它在不在场时不设 env、让 CLI 走自己的查找表）。
///
/// **边界如实申报（返工 m-4）——以下两形态 L0 不认，会误报「provider config 缺失」拒发**：
/// ① CLI 查找表的第二位（盘根位 `<盘>:\config\provider\zcode-builtin.json`）——只查
///   推导位与 `<resources>/glm/provider/` 首选位，盘根位属异形安装；
/// ② 用户自设 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 环境变量指向自定义位置（env 已带
///   provider config 时 CLI 可定位，但 L0 的 stat 看不见进程 env）。
/// 兜底：误报的 fail 结论只缓存 [`FAILURE_TTL_MS`]（5min）——修复/重装/改回标准布局后
/// 自动失效重探，不损数据、不会永久钉死通道。
fn provider_config_present(cjs: &str, os: &str) -> bool {
    let derived = zcode_provider_config_path(cjs)
        .map(|p| std::path::Path::new(&p).is_file())
        .unwrap_or(false);
    if os == "macos" {
        return derived;
    }
    let cli_lookup = std::path::Path::new(cjs)
        .parent()
        .map(|glm| glm.join("provider").join("zcode-builtin.json").is_file())
        .unwrap_or(false);
    derived || cli_lookup
}

/// L0 结构检查（裁决 33a，零模型成本 stat）：exe / cjs / provider-config
/// 在场性。返回 `Some(fail)` = 结构性坏，直接拒发（连 L0 `--help`
/// spawn 都不起，更不烧 L1 真回合）；`None` = 过，放行去跑 L0
/// 帮助面。hint 区分「可执行不在场」与「provider config 缺失」。
fn l0_stat_verdict(spec: &ProbeSpec, os: &str) -> Option<ProbeVerdict> {
    if !std::path::Path::new(spec.exe()).is_file() {
        return Some(ProbeVerdict::fail(format!(
            "GATE_L0_EXE_MISSING: L0 结构检查：可执行不在场（不 spawn）：{}",
            spec.exe()
        )));
    }
    match spec {
        // codex 无 cjs/provider-config 概念（exe 在场性已在上方统一判定）
        ProbeSpec::Codex { .. } => None,
        ProbeSpec::Zcode { cjs, .. } => {
            if !std::path::Path::new(cjs).is_file() {
                return Some(ProbeVerdict::fail(format!(
                    "GATE_L0_CJS_MISSING: L0 结构检查：zcode.cjs 缺失（resources/glm 布局损坏）：{cjs}"
                )));
            }
            if !provider_config_present(cjs, os) {
                let expected = zcode_provider_config_path(cjs).unwrap_or_default();
                return Some(ProbeVerdict::fail(format!(
                    "GATE_L0_PROVIDER_CONFIG_MISSING: L0 结构检查：provider config 缺失（预期 {expected} 或 <resources>/glm/provider/zcode-builtin.json；修复/重装 ZCode 后自动重探）"
                )));
            }
            None
        }
    }
}

/// L0 帮助面 argv：`<exe> <cjs> --help`（不带档位段——帮助面不需要）
fn l0_help_argv(spec: &ProbeSpec) -> Vec<String> {
    match spec {
        ProbeSpec::Zcode { cjs, .. } => vec![cjs.clone(), "--help".into()],
        ProbeSpec::Codex { .. } => vec!["--help".into()],
    }
}

/// L0 帮助面执行体（裁决 33a）：`--help` 能起能印（实测 0.9s，零模型
/// 成本）——spawn 后交纯判定 [`l0_help_verdict`]（判定与执行分离，三分支
/// 可脱离真 spawn 缝测，见 `l0_help_verdict_covers_timeout_nonzero_exit_and_silent_zero_output`）。
async fn l0_help_run(spec: &ProbeSpec, os: &str) -> ProbeVerdict {
    let mut r = RunnerCfg::new(spec.exe())
        .args(l0_help_argv(spec))
        .timeout_ms(L0_HELP_TIMEOUT_MS)
        .sem(GlobalSem::new(1));
    for (k, v) in probe_env(spec, os) {
        r = r.env(k, v);
    }
    let receipt = r.run().await;
    l0_help_verdict(
        receipt.stage,
        r.last_exit_code(),
        &r.captured_stdout().join("\n"),
        &r.captured_stderr(),
        spec.exe(),
    )
}

/// L0 帮助面判定（纯函数，返工 m-5 缝）：Timeout/Spawn/非零退出/零输出 =
/// 结构性坏 fail（不烧 L1 真回合）；**能起能印**（exit 0 且 stdout/stderr
/// 有输出，两个条件缺一不可）→ pass 放行 L1。
fn l0_help_verdict(
    stage: Option<Stage>,
    exit: Option<i32>,
    stdout: &str,
    stderr: &str,
    exe: &str,
) -> ProbeVerdict {
    if stage == Some(Stage::Spawn) {
        return ProbeVerdict::fail(format!(
            "GATE_L0_HELP_SPAWN_FAIL: L0 --help spawn 失败：{exe}"
        ));
    }
    if stage == Some(Stage::Timeout) {
        return ProbeVerdict::fail(format!(
            "GATE_L0_HELP_TIMEOUT: L0 --help 超时（{}s 内未起未印——CLI 起动面已坏，不烧真回合）",
            L0_HELP_TIMEOUT_MS / 1000
        ));
    }
    let printed = !stdout.trim().is_empty() || !stderr.trim().is_empty();
    if exit == Some(0) && printed {
        return ProbeVerdict::pass("L0 过（--help 能起能印）；放行 L1 真回合");
    }
    let detail =
        crate::inject::normalize::summarize(stderr, crate::inject::normalize::AUDIT_SUMMARY_CHARS);
    ProbeVerdict::fail(format!(
        "GATE_L0_HELP_SILENT: L0 --help 未能起能印（exit={exit:?}，stdout {} 字节）；stderr={detail}",
        stdout.len()
    ))
}

/// 两级探针编排（裁决 33a；执行体注入可测）：L0 stat 败 → 直接
/// fail（零 spawn）；L0 帮助面败 → fail（不烧 L1）；L0 过 → **仍跑
/// L1**（红线：L0 是串联闸门不是替代——`--help` 漏判静默形态，
/// Task 8 实证教训；结论以 L1 为准）。起止计时在此回填：最终
/// 结论携带实测墙钟（Task 2 duration 透传的数据源）。
async fn two_level_probe<L0F, L1F, L0Fut, L1Fut>(
    spec: &ProbeSpec,
    os: &str,
    l0: L0F,
    l1: L1F,
) -> ProbeVerdict
where
    L0F: FnOnce() -> L0Fut,
    L0Fut: Future<Output = ProbeVerdict>,
    L1F: FnOnce() -> L1Fut,
    L1Fut: Future<Output = ProbeVerdict>,
{
    let started = Instant::now();
    let verdict = if let Some(stat_fail) = l0_stat_verdict(spec, os) {
        stat_fail
    } else {
        let l0v = l0().await;
        if l0v.is_pass() {
            l1().await
        } else {
            l0v
        }
    };
    verdict.with_duration_ms(started.elapsed().as_millis() as u64)
}

/// L1 执行体（zcode = 真回合 `"hi"`；codex 变体 = 既有单级探针执行体）：
/// spawn + watchdog（[`PROBE_TIMEOUT_MS`] ）+ 判定。超时分支走 [`timeout_verdict`]（B 主修）。
async fn probe_round(spec: &ProbeSpec, os: &str) -> ProbeVerdict {
    let mut r = RunnerCfg::new(spec.exe())
        .args(probe_argv(spec))
        .timeout_ms(PROBE_TIMEOUT_MS)
        .sem(GlobalSem::new(1));
    for (k, v) in probe_env(spec, os) {
        r = r.env(k, v);
    }
    let receipt = r.run().await;
    if receipt.stage == Some(Stage::Timeout) {
        // B 主修（裁决 29c）：证据优先于时序——kill 后已捕获 stdout
        // 仍可读（runner 缓冲保留语义由
        // `runner::tests::runner_kill_preserves_captured_stdout_buffer` 钉死）
        match spec {
            ProbeSpec::Zcode { .. } => timeout_verdict(&r.captured_stdout().join("\n")),
            // codex 的过判据是 exit 0 + usage 文本（不是 JSON）——超时无第二判据，如实 fail
            ProbeSpec::Codex { .. } => ProbeVerdict::fail(timeout_zero_output_hint()),
        }
    } else if receipt.stage == Some(Stage::Spawn) {
        ProbeVerdict::fail(format!("PROBE_SPAWN_FAIL: 探针 spawn 失败：{}", spec.exe()))
    } else {
        let stdout = r.captured_stdout().join("\n");
        let stderr = r.captured_stderr();
        // 评审 Minor 1：退出码取**真实值**（`last_exit_code`）——不得把非零退出谎报成
        // None 再塞进失败提示（用户看到的是「exit=Some(7)」而不是「exit=None」）
        let code = r.last_exit_code();
        match spec {
            ProbeSpec::Zcode { .. } => zcode_verdict(&stdout, &stderr, code, os),
            ProbeSpec::Codex { .. } => codex_verdict(&stdout, &stderr, code),
        }
    }
}

/// 缓存键：可执行文件**在场性 + mtime**（+ 辅助产物 mtime）——工具升级即失效
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeKey {
    pub exe: String,
    pub exe_present: bool,
    pub exe_mtime_ms: Option<u64>,
    pub aux_mtime_ms: Option<u64>,
}

impl ProbeKey {
    /// 真 fs stat（零网络；只在探针入口调用一次）
    pub fn of(spec: &ProbeSpec) -> Self {
        let (present, mtime) = stat_ms(spec.exe());
        let aux = spec.aux_path().and_then(|p| stat_ms(p).1);
        Self {
            exe: spec.exe().to_string(),
            exe_present: present,
            exe_mtime_ms: mtime,
            aux_mtime_ms: aux,
        }
    }

    /// 单 exe 键（真 fs stat，无辅助产物面）。**Task 7 平行 spec 结构的键单点**：
    /// codex app-server 探测体是交互式会话（spawn → initialize → 方法空参 ping，见
    /// `codex_app::probe_app_server`），不流经 argv 单发探针族（[`ProbeSpec`] /
    /// [`probe_argv`] / [`probe_round`]——强加变体要在多处 match 塞不可达臂），
    /// 故不扩枚举、走平行结构；但**缓存键纪律同源**：exe 在场性 + mtime
    /// （codex 升级即失效重探——版本漂移复核）。
    pub fn of_exe(exe: &str) -> Self {
        let (present, mtime) = stat_ms(exe);
        Self {
            exe: exe.to_string(),
            exe_present: present,
            exe_mtime_ms: mtime,
            aux_mtime_ms: None,
        }
    }

    /// 纯构造（测试用；生产走 [`ProbeKey::of`]）
    pub fn of_parts(
        exe: &str,
        exe_present: bool,
        exe_mtime_ms: Option<u64>,
        aux_mtime_ms: Option<u64>,
    ) -> Self {
        Self {
            exe: exe.to_string(),
            exe_present,
            exe_mtime_ms,
            aux_mtime_ms,
        }
    }
}

fn stat_ms(path: &str) -> (bool, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(md) => (
            true,
            md.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as u64)
                }),
        ),
        Err(_) => (false, None),
    }
}

/// 探针结果缓存（进程存在性 + mtime 键 → 结论 + 记录时刻）。
///
/// # 成功长缓存、失败带 TTL（Task 8 复审 Important 2）
/// 探针**已是真实最小回合**（见 [`PROBE_PROMPT`]）——一次瞬时失败（工作区争用
/// `Model creation failed`、15s 超时、网络抖动、provider 临时不可用）绝不能把通道
/// **钉死一整个进程生命周期**（旧行为：失败与成功同样长缓存 → 一次抖动 = 该工具永久
/// `version_gate` 拒发，且没有任何重探路径）。
/// 现策略：
/// - **成功**：按 (exe 在场性 + mtime) 键长缓存（工具升级才失效重探——成本 = 一次最小回合）；
/// - **失败**：只缓存 [`FAILURE_TTL_MS`]，过期即失效 → 下一次投递自然重探（成本上界 =
///   通道坏着时每 TTL 至多一次最小回合；下界 = 抖动恢复后 ≤ TTL 即自动恢复）。
#[derive(Default)]
pub struct ProbeCache {
    map: Mutex<HashMap<String, (ProbeKey, ProbeVerdict, u64)>>,
    /// **single-flight 名额表（评审 I13）**：per-exe 的排队锁——同名可执行文件只放
    /// **首个**请求跑真探针，后到者在这把锁上异步排队，醒来先读缓存（首者结论已在）。
    ///
    /// 键空间与 `map` 同源（exe 路径）：项数上界 = 本进程探过的可执行文件个数（生产里
    /// 就是装了几个工具），与缓存本身同量级，故**不做逐次清理**——清理要在「首者注销
    /// 名额」与「后到者接管」之间再补一轮登记，否则 mtime 变更场景下第三个请求会并行
    /// 重探，得不偿失（详见 [`probe_single_flight`]）。
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// 失败结论的缓存生存期（5 分钟）：ZCode 工作区争用锁的常见持续量级 ≥ 分钟级——
/// 太短会把「真实回合成本」付成高频重探，太长则抖动恢复被人为推迟。可用 `--`
/// 常量调；口径与「成功长缓存」不对称是**有意**的（失败便宜、成功贵）。
pub const FAILURE_TTL_MS: u64 = 300_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ProbeCache {
    /// 读缓存（真实时钟）：**失败且超 TTL → 视为未命中**（`get_at` 是纯口径）
    pub fn get(&self, key: &ProbeKey) -> Option<ProbeVerdict> {
        self.get_at(key, now_ms())
    }

    /// 读缓存（时钟注入；TTL 判定全在这里）：成功恒命中；失败只在 TTL 内命中
    pub fn get_at(&self, key: &ProbeKey, now_ms: u64) -> Option<ProbeVerdict> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(&key.exe) {
            Some((k, v, at)) if k == key => {
                if v.is_pass() || now_ms.saturating_sub(*at) < FAILURE_TTL_MS {
                    Some(v.clone())
                } else {
                    None // 失败过期：重探
                }
            }
            _ => None,
        }
    }

    /// 写缓存（真实时钟）
    pub fn put(&self, key: ProbeKey, verdict: ProbeVerdict) {
        self.put_at(key, verdict, now_ms());
    }

    /// 写缓存（时钟注入）
    pub fn put_at(&self, key: ProbeKey, verdict: ProbeVerdict, now_ms: u64) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.exe.clone(), (key, verdict, now_ms));
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取（或建）per-exe 的 single-flight 名额：**只登记不排队**——真正的排队在返回的
    /// tokio 锁上（`gate.lock().await`）。同 exe 的并发请求拿到**同一个** `Arc`，故首者
    /// 持锁跑探针、后到者异步等待（让出执行权，不阻塞线程，首者可照常跑满
    /// [`PROBE_TIMEOUT_MS`]）。
    fn inflight_gate(&self, exe: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(exe.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// 带缓存的探针执行：命中即返回（不跑执行体）；缺席的可执行文件直接判失败（不 spawn）。
/// 缓存命中语义（含失败 TTL）见 [`ProbeCache`]。
pub fn probe_cached<F>(cache: &ProbeCache, key: ProbeKey, run: F) -> ProbeVerdict
where
    F: FnOnce() -> ProbeVerdict,
{
    let now = now_ms();
    if let Some(hit) = cache.get_at(&key, now) {
        return hit;
    }
    let verdict = if key.exe_present {
        run()
    } else {
        ProbeVerdict::fail(format!(
            "PROBE_EXE_MISSING: 可执行文件不在场（不 spawn）：{}",
            key.exe
        ))
    };
    cache.put_at(key, verdict.clone(), now);
    verdict
}

/// **single-flight 探针核心（评审 I13）**：缓存 miss 时，同一可执行文件只放一个「首者」
/// 跑真探针；后到者在此排队等首者结论，醒来**直接复用首者写入缓存的结论**。
///
/// # 为什么需要（缺陷形态：check-then-run 无锁）
/// [`probe`] 在 `remote/api.rs` 的请求任务里被 `.await`（两条客户端请求天然并发）：
/// 旧实现「先查缓存、未命中就开跑」之间没有互斥——两个客户端同时首发同一个 exe
/// → 两边都 miss → **并行跑多次真实模型回合**。探针是 `--prompt "hi"` 的最小真回合
/// （见 [`PROBE_PROMPT`]），重复跑是纯浪费 + **真实配额消耗**（不只是慢）。
///
/// # 为什么失败结论也要共享
/// 失败同样付了一次真实回合的代价（工作区争用锁 / 超时 / 网络抖动），后到者若不等首者
/// 自己再跑一遍，等于对同一个瞬时故障付两份成本。失败结论本就按 [`FAILURE_TTL_MS`]
/// 进缓存（既有语义），本函数只是把「并发的 N 次首探」收敛成 1 次：**TTL 口径一字未改**
/// ——首者写缓存后，后到者的二次读走的是同一条 [`ProbeCache::get_at`] 判定（成功恒命中 /
/// 失败 TTL 内命中、过期即重探）。首者被取消（等待者等不到结论）时二次读必然 miss，
/// 后到者自行补跑（不会挂死、也不会把通道静默钉死）。
///
/// # 为什么用 tokio Mutex 排队而不是 `Notify`
/// `Notify::notify_waiters` 只唤醒**已注册**的等待者：后到者若在首者发通知之后才注册就
/// 永远等不到（丢唤醒 = 挂死）。tokio Mutex 的等待队列由运行时管理，无丢唤醒竞态；且它是
/// **异步**锁——等待者让出执行权而非阻塞线程。
///
/// # 不死锁的依据
/// 执行体（[`probe_round`] → `RunnerCfg::run`）只 spawn 子进程，**不回调**本函数；等待者
/// 只持名额锁、不持探针名额（[`GlobalSem`]），无锁序反转。首者 future 被 drop（请求中止）
/// → 名额锁随之释放 → 等待者醒来发现未命中即自行补跑。
async fn probe_single_flight<F, Fut>(cache: &ProbeCache, key: ProbeKey, run: F) -> ProbeVerdict
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ProbeVerdict>,
{
    // 快路径：命中即返回（成功长缓存 / 失败 TTL 内）——不进名额表，零并发代价
    if let Some(hit) = cache.get(&key) {
        return hit;
    }
    let gate = cache.inflight_gate(&key.exe);
    // 首者在此独占该 exe 的名额；后到者在此排队（异步等待，不阻塞线程）
    let _guard = gate.lock().await;
    // 二次检查：首者可能刚写完结论——**含失败结论**（按既有 TTL 语义共享，不再跑一遍）
    if let Some(hit) = cache.get(&key) {
        return hit;
    }
    let verdict = if key.exe_present {
        run().await
    } else {
        // 缺席即拒发（不 spawn 一个不存在的程序）——同样入缓存、同样被后到者复用
        ProbeVerdict::fail(format!(
            "PROBE_EXE_MISSING: 可执行文件不在场（不 spawn）：{}",
            key.exe
        ))
    };
    cache.put(key, verdict.clone());
    verdict
}

/// 生产探针（真 spawn；Task 8/9/12 消费）：**zcode 走两级**（L0 快面 → L1
/// 真回合，裁决 33a；缓存命中（含成功长缓存）时 L0/L1 都不跑）；codex
/// 变体维持单级（其活体探针在 `codex::probe_cli`，本函数的 Codex 分支
/// 仅作同构兜底）。结论入缓存（成功长缓存 / 失败 5min TTL，见
/// [`ProbeCache`]）。**探针不占无头 turn 的全局名额**（L0/L1 各自持 cap=1
/// 的名额——版本门控是前置动作，不该让用户请求陪它排队）。
///
/// **并发纪律（评审 I13）**：真正的执行体在 [`probe_single_flight`] 内——同 exe 的
/// 并发首发只跑一次真回合，后到者复用首者结论（含失败结论）。缓存 TTL/键
/// （exe 在场性 + mtime）、失败结论缓存策略与公开 API 形状均未改。
pub async fn probe(spec: &ProbeSpec, cache: &ProbeCache, os: &str) -> ProbeVerdict {
    let key = ProbeKey::of(spec);
    probe_single_flight(cache, key, || async {
        match spec {
            // 两级编排（裁决 33a）：L0 stat → L0 `--help` → L1 真回合；
            // duration（实测墙钟）由 two_level_probe 起止计时回填
            ProbeSpec::Zcode { .. } => {
                two_level_probe(spec, os, || l0_help_run(spec, os), || probe_round(spec, os)).await
            }
            // codex：无 L0 概念（其探针本就是零模型成本的 `queue --help`
            // 面；活体路径在 `codex::probe_cli`）——单级，仅回填墙钟
            ProbeSpec::Codex { .. } => {
                let started = Instant::now();
                probe_round(spec, os)
                    .await
                    .with_duration_ms(started.elapsed().as_millis() as u64)
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::receipt::Stage;

    /// 计划书 Step 3 原例（附录 E/探测定案）：`--version` 不需要 provider config 会
    /// 漏判——探针**必须 `--prompt` 干跑**。
    #[test]
    fn zcode_probe_uses_prompt_dryrun_not_version() {
        let spec = ProbeSpec::Zcode {
            exe: "D:/Program Files/ZCode/ZCode.exe".into(),
            cjs: "D:/Program Files/ZCode/resources/glm/zcode.cjs".into(),
        };
        let argv = probe_argv(&spec);
        assert!(
            argv.iter().any(|a| a == "--prompt"),
            "探针须 --prompt 干跑: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--version"),
            "探针不得用 --version（会漏判 provider 缺失）: {argv:?}"
        );
        // 干跑档 = yolo + --json（能出 JSON 即过）；**载荷必须非空**
        // （Task 8 真机实证：`--prompt ""` 被 CLI 拒绝——`--prompt requires non-empty text.`，
        // 空串探针在真机恒失败，会把门控变成「永远拒发」）
        assert!(argv.iter().any(|a| a == "--mode"));
        assert!(argv.iter().any(|a| a == "--json"));
        let i = argv
            .iter()
            .position(|a| a == "--prompt")
            .expect("须有 --prompt");
        assert_eq!(argv[i + 1], PROBE_PROMPT);
        assert!(
            !argv[i + 1].trim().is_empty(),
            "探针载荷不得为空（真机 CLI 拒绝空串）：{argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--help"),
            "zcode 探针须干跑而非帮助面"
        );
    }

    /// **H5（Task 10）：探针的档位段与真回合同源**——两处都从 [`PermissionSpec::zcode_default`]
    /// 取旗子。此测是「探针不得留旧档位字面」的防漂移锁：档位一旦收紧/新增变体，
    /// 探针与 `zcode::build_argv` 必须同时跟着 spec 走（否则门控探的是旧档、回合 spawn 的是新档）。
    #[test]
    fn zcode_probe_argv_mode_comes_from_the_permission_spec() {
        let spec = ProbeSpec::Zcode {
            exe: "D:/Program Files/ZCode/ZCode.exe".into(),
            cjs: "D:/Program Files/ZCode/resources/glm/zcode.cjs".into(),
        };
        let argv = probe_argv(&spec);
        let want = PermissionSpec::zcode_default().flags();
        let i = argv
            .iter()
            .position(|a| a == "--mode")
            .expect("探针须带档位旗子");
        assert_eq!(
            &argv[i..i + want.len()],
            want.as_slice(),
            "探针档位段必须逐字来自 spec: {argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--mode").count(),
            1,
            "档位旗子只此一处（不得既从 spec 注入又留字面）: {argv:?}"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("--json"),
            "`--json` 仍是尾 flag（档位段插在它之前，与真回合同序）"
        );
        // 与**真回合**同源：zcode::build_argv 的 argv 里是同一段旗子（同一 spec）
        let turn = crate::inject::headless::zcode::build_argv(
            &crate::inject::headless::zcode::ZcodeSpec::win("Z:/mam-nonexistent-zcode-root"),
            "hi",
            "sess_1",
            "E:/p",
            None,
        )
        .expect("测试花名在册（argv 构造，见 turn::device_name_refusal）");
        assert!(
            turn.argv
                .windows(want.len())
                .any(|w| w.iter().map(String::as_str).eq(want.iter().copied())),
            "真回合 argv 必须带同一段档位旗子: {:?}",
            turn.argv
        );
    }

    /// codex 探针 = `queue --help` 子命令在场（不是 --version）
    #[test]
    fn codex_probe_checks_queue_subcommand() {
        let spec = ProbeSpec::Codex {
            exe: "codex".into(),
        };
        let argv = probe_argv(&spec);
        assert_eq!(argv, vec!["queue".to_string(), "--help".to_string()]);
        assert!(!argv.iter().any(|a| a == "--version"));
    }

    /// 探针判定（纯函数）：噪音前缀 + JSON 行即过（Mac 实测污染行不得误伤）
    #[test]
    fn zcode_verdict_passes_on_json_after_noise() {
        let v = zcode_verdict(
            "ZCode Built-in skipped (not-due)\n{\"sessionId\":\"probe-1\"}",
            "",
            Some(0),
            "macos",
        );
        assert!(v.is_pass(), "能出 JSON 即过: {v:?}");
    }

    /// 探针失败 = version_gate 拒发回执 + **修复提示**（Mac 缺 provider config 专档）
    #[test]
    fn probe_failure_maps_to_version_gate_with_remediation_hint() {
        let v = zcode_verdict("", "some provider error", Some(1), "macos");
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(
            hint.contains("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE"),
            "Mac 缺 provider config 的修复提示必须给出环境变量名: {hint}"
        );
        let r = version_gate_receipt("s1", &v).expect("拒发必须出回执");
        assert_eq!(r.stage, Some(Stage::VersionGate));
        assert_eq!(
            r.status,
            crate::inject::headless::receipt::ReceiptStatus::Failed
        );
        assert_eq!(r.session_id, "s1");
        assert!(r
            .reason
            .unwrap_or_default()
            .contains("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE"));
        // 通过时不出拒发回执
        assert!(version_gate_receipt("s1", &ProbeVerdict::pass("ok")).is_none());
    }

    /// 平台分叉的 env：Windows = ELECTRON_RUN_AS_NODE；macOS 另设 provider config 路径
    /// （从 cjs 路径反推 Resources 根——不设则 --prompt 静默无 JSON）
    #[test]
    fn probe_env_sets_electron_flag_and_mac_provider_config() {
        let spec = ProbeSpec::Zcode {
            exe: "/Applications/ZCode.app/Contents/MacOS/ZCode".into(),
            cjs: "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs".into(),
        };
        let win = probe_env(&spec, "windows");
        assert_eq!(
            win,
            vec![("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())],
            "Windows 且推导路径不在场：只设 Electron 当 node 跑的开关（不指向不存在的文件）"
        );
        let mac = probe_env(&spec, "macos");
        assert!(mac.contains(&("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())));
        let (k, path) = mac
            .iter()
            .find(|(k, _)| k == "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE")
            .expect("macOS 必须设 provider config 环境变量");
        assert_eq!(k, "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE");
        // 期望值按平台分隔符构造（本函数是路径推导，不负责分隔符风格）
        let expected = std::path::Path::new("/Applications/ZCode.app/Contents/Resources")
            .join("config")
            .join("provider")
            .join("zcode-builtin.json")
            .to_string_lossy()
            .to_string();
        assert_eq!(path, &expected);
        // codex 探针不需要 Electron 开关（原生 CLI）
        assert!(probe_env(
            &ProbeSpec::Codex {
                exe: "codex".into()
            },
            "macos"
        )
        .is_empty());
    }

    /// **Windows 的 provider config 规则（Task 8 真机实证修订）**：推导路径在场 → 设
    /// （真机 ZCode 装包就把文件放在 `<resources>/config/provider/`，而 CLI 自己的查找表
    /// 里没有该位置——不设则 `--prompt` 直接报「无法定位」）；不在场 → 不设（不指向
    /// 不存在的文件，让 CLI 走自身查找）。用 tempdir 夹具驱动，**不触真机安装路径**。
    #[test]
    fn provider_config_env_is_set_on_windows_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let cjs = std::path::Path::new(&root)
            .join("resources")
            .join("glm")
            .join("zcode.cjs");
        let cfgdir = std::path::Path::new(&root)
            .join("resources")
            .join("config")
            .join("provider");
        std::fs::create_dir_all(&cfgdir).unwrap();
        std::fs::create_dir_all(cjs.parent().unwrap()).unwrap();
        std::fs::write(&cjs, "// stub").unwrap();
        let cjs_s = cjs.to_string_lossy().to_string();
        // 不在场（文件还没写）→ 不设
        assert!(
            provider_config_env(&cjs_s, "windows").is_none(),
            "推导路径不在场时不得设（否则指向不存在的文件，CLI 报定位失败）"
        );
        // 在场 → 设，且值 = 推导路径
        let cfg = cfgdir.join("zcode-builtin.json");
        std::fs::write(&cfg, "{}").unwrap();
        let (k, v) = provider_config_env(&cjs_s, "windows").expect("文件在场必须设");
        assert_eq!(k, ZCODE_PROVIDER_CONFIG_ENV);
        assert_eq!(v, cfg.to_string_lossy().to_string());
        // macOS 侧与文件在场性无关（spec：无条件设）
        let (_, mv) = provider_config_env("/nope/glm/zcode.cjs", "macos").unwrap();
        assert!(mv.ends_with("zcode-builtin.json"));
    }

    /// 缓存命中不重跑；**mtime 变化即失效**（工具升级 = 版本漂移复核，H6）
    #[test]
    fn probe_cache_hit_and_mtime_invalidation() {
        let cache = ProbeCache::default();
        let key = ProbeKey::of_parts("zcode.exe", true, Some(1_000), Some(2_000));
        let mut calls = 0;
        let v1 = probe_cached(&cache, key.clone(), || {
            calls += 1;
            ProbeVerdict::pass("first")
        });
        assert!(v1.is_pass());
        assert_eq!(calls, 1);
        // 同键（进程在场 + mtime 未变）→ 命中缓存，不再跑
        let v2 = probe_cached(&cache, key.clone(), || {
            calls += 1;
            ProbeVerdict::fail("should not run")
        });
        assert!(v2.is_pass(), "命中缓存必须返回上次结论");
        assert_eq!(calls, 1, "缓存命中不得重跑探针");
        // mtime 变（工具升级）→ 失效重跑
        let bumped = ProbeKey::of_parts("zcode.exe", true, Some(9_999), Some(2_000));
        let v3 = probe_cached(&cache, bumped, || {
            calls += 1;
            ProbeVerdict::fail("升级后 flag 面变了")
        });
        assert!(!v3.is_pass());
        assert_eq!(calls, 2, "mtime 变化必须重跑（版本漂移复核）");
        assert_eq!(
            cache.len(),
            1,
            "同一可执行文件只占一格（升级后的新结论覆盖旧的）"
        );
        // 进程缺席也是缓存键的一部分（缺席 → 直接判失败，不 spawn）
        let missing = ProbeKey::of_parts("nope.exe", false, None, None);
        assert!(!missing.exe_present);
        assert!(!probe_cached(&cache, missing, || ProbeVerdict::fail("缺少可执行文件")).is_pass());
        assert_eq!(cache.len(), 2, "另一可执行文件另占一格");
    }

    /// **Task 8 复审 Important 2**：失败结论**带 TTL**、成功结论长缓存——探针已是真实最小
    /// 回合，一次瞬时失败（争用锁/超时/网络抖）不得把通道钉死整个进程生命周期；反过来
    /// 也不能让「坏着」的通道每次投递都付一次真实回合（TTL 内仍复用失败结论）。
    #[test]
    fn failure_verdicts_expire_but_successes_do_not() {
        let cache = ProbeCache::default();
        let key = ProbeKey::of_parts("zcode.exe", true, Some(1), Some(2));
        cache.put_at(key.clone(), ProbeVerdict::fail("瞬时抖动"), 1_000);
        // TTL 内命中（坏着时不重复烧真实回合）
        let hit = cache
            .get_at(&key, 1_000 + FAILURE_TTL_MS - 1)
            .expect("TTL 内必须命中");
        assert!(!hit.is_pass());
        // 过期即未命中 → 下一次投递自然重探（通道不会被永久钉死）
        assert!(
            cache.get_at(&key, 1_000 + FAILURE_TTL_MS).is_none(),
            "失败结论过期必须失效（否则一次抖动 = 永久拒发）"
        );
        // 成功不受 TTL 约束（mtime 键长缓存：工具升级才失效——重探成本 = 一次真实回合）
        cache.put_at(key.clone(), ProbeVerdict::pass("ok"), 1_000);
        assert!(
            cache
                .get_at(&key, 1_000 + FAILURE_TTL_MS * 100)
                .is_some_and(|v| v.is_pass()),
            "成功结论必须长缓存"
        );
        // 生产路径（真实时钟）：过期的失败必须真的重跑执行体，且恢复结论可上线
        let expired = ProbeKey::of_parts("z2.exe", true, Some(1), None);
        cache.put_at(expired.clone(), ProbeVerdict::fail("旧失败"), 0);
        let mut ran = 0;
        let v = probe_cached(&cache, expired, || {
            ran += 1;
            ProbeVerdict::pass("抖动恢复")
        });
        assert_eq!(ran, 1, "失败过期后 probe_cached 必须真的重探");
        assert!(v.is_pass(), "重探结论必须可用（通道自动恢复）");
    }

    /// 真 fs 键：不存在的可执行文件 → 缺席；存在的 → 带 mtime（零网络、只 stat）
    #[test]
    fn probe_key_of_stats_the_real_binary() {
        let exe = std::env::current_exe().expect("测试二进制路径");
        let spec = ProbeSpec::Codex {
            exe: exe.to_string_lossy().to_string(),
        };
        let key = ProbeKey::of(&spec);
        assert!(key.exe_present, "自身二进制必然在场: {key:?}");
        assert!(key.exe_mtime_ms.is_some());
        let bogus = ProbeKey::of(&ProbeSpec::Codex {
            exe: "mam-nonexistent-probe-xyz".into(),
        });
        assert!(!bogus.exe_present);
        assert!(bogus.exe_mtime_ms.is_none());
        // 缺席即拒发（不 spawn 一个不存在的程序）
        let v = probe_cached(&ProbeCache::default(), bogus, || {
            panic!("缺席的可执行文件不得进入探针执行体")
        });
        assert!(!v.is_pass());
    }

    /// codex 探针判定：`queue` 子命令在场（exit 0 + usage 提及）才算过
    #[test]
    fn codex_verdict_requires_queue_subcommand_in_place() {
        let ok = codex_verdict(
            "Run a task in a queue\n\nUsage: codex queue [OPTIONS] <COMMAND>",
            "",
            Some(0),
        );
        assert!(ok.is_pass(), "{ok:?}");
        let old = codex_verdict("error: unrecognized subcommand 'queue'", "", Some(2));
        assert!(!old.is_pass());
        assert!(old.hint().unwrap_or_default().contains("queue"));
    }

    // ============================================================
    // Task 2（P0）探针修复：A 阈值 45s + B 超时先看 stdout + L0 快面 + duration
    // ============================================================

    /// L0 全在场夹具（tempdir：exe + cjs + 推导位 provider config 都是普通 stub 文件
    /// ——stat 只看在场性，不触真机安装路径）
    fn l0_ok_fixture() -> (tempfile::TempDir, ProbeSpec) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let exe = root.join("ZCode.exe");
        std::fs::write(&exe, "stub").unwrap();
        let cjs = root.join("resources").join("glm").join("zcode.cjs");
        std::fs::create_dir_all(cjs.parent().unwrap()).unwrap();
        std::fs::write(&cjs, "// stub").unwrap();
        let cfg = root.join("resources").join("config").join("provider");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("zcode-builtin.json"), "{}").unwrap();
        (
            dir,
            ProbeSpec::Zcode {
                exe: exe.to_string_lossy().to_string(),
                cjs: cjs.to_string_lossy().to_string(),
            },
        )
    }

    /// A 兜底：阈值钉值 45s（本机实测最小回合 15–19s ×2 余量；改常量必须连着 hint 走，
    /// 见 [`timeout_with_zero_output_fails_with_new_hint`]）
    #[test]
    fn probe_timeout_is_45s() {
        assert_eq!(PROBE_TIMEOUT_MS, 45_000);
    }

    /// B 主修（裁决 29c）：超时被杀但已捕获 stdout 有 JSON → **Pass**——证据优先于
    /// 时序（进程真跑完了回合、只是超阈值被杀，stdout 是完成证据）；纯噪音前缀不算证据。
    #[test]
    fn timeout_with_captured_stdout_json_passes() {
        let v = timeout_verdict(
            "ZCode Built-in skipped (not-due)\n{\"sessionId\":\"s1\",\"response\":\"hi\"}",
        );
        assert!(v.is_pass(), "超时但有 JSON = 回合已完成，必须过: {v:?}");
        assert!(
            !timeout_verdict("ZCode Built-in skipped (not-due)").is_pass(),
            "噪音前缀不是完成证据，不得混过"
        );
    }

    /// B/A 合流：超时且零输出 → Fail + 新 hint（秒数从常量格式化生成、含「网络/模型服务」，
    /// 旧误导归因「挂死」必须退场）
    #[test]
    fn timeout_with_zero_output_fails_with_new_hint() {
        let v = timeout_verdict("");
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(
            hint.contains(&format!("{}s", PROBE_TIMEOUT_MS / 1000)),
            "秒数必须从常量生成（改阈值文案跟着走）: {hint}"
        );
        assert!(hint.contains("网络/模型服务"), "{hint}");
        assert!(!hint.contains("挂死"), "旧误导归因必须退场: {hint}");
    }

    /// duration 透传：探针结论携带实测墙钟（≥ 注入的 L1 睡眠时长，绝非默认 0），
    /// `version_gate_receipt` 原样透传给回执（审计行不再「· 0ms」）
    #[tokio::test]
    async fn verdict_carries_real_wallclock_duration() {
        let (_dir, spec) = l0_ok_fixture();
        let v = two_level_probe(
            &spec,
            "windows",
            || async { ProbeVerdict::pass("L0 过") },
            || async {
                std::thread::sleep(std::time::Duration::from_millis(30));
                ProbeVerdict::fail("L1 跑了")
            },
        )
        .await;
        assert!(!v.is_pass());
        assert!(
            v.duration_ms() >= 30,
            "duration 必须是实测墙钟（≥ 注入的 30ms）: {}",
            v.duration_ms()
        );
        // 透传链：verdict.duration_ms → Receipt.duration_ms（审计行的数据源）
        let receipt = version_gate_receipt("s1", &v).expect("fail 必须出回执");
        assert_eq!(receipt.duration_ms, v.duration_ms());
    }

    /// L0 快面（裁决 33a）：stat 层结构性坏 → 直接 fail——连 L0 帮助面 spawn 都不起、
    /// 更不烧 L1 真回合；hint 区分「可执行不在场」与「provider config 缺失」。
    #[tokio::test]
    async fn l0_stat_failure_blocks_l1_without_burning_a_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // ① 可执行缺席
        let spec = ProbeSpec::Zcode {
            exe: "mam-nonexistent-zcode-exe.exe".into(),
            cjs: "mam-nonexistent-zcode.cjs".into(),
        };
        let burned = std::sync::Arc::new(AtomicUsize::new(0));
        let b = burned.clone();
        let v = two_level_probe(
            &spec,
            "windows",
            || async { panic!("stat 失败时 L0 帮助面不得起") },
            move || async move {
                b.fetch_add(1, Ordering::SeqCst);
                ProbeVerdict::pass("L1 不该跑")
            },
        )
        .await;
        assert!(!v.is_pass());
        assert!(
            v.hint().unwrap_or_default().contains("不在场"),
            "可执行缺席要有专属 hint: {:?}",
            v.hint()
        );
        assert_eq!(burned.load(Ordering::SeqCst), 0, "L1 真回合一次都不烧");
        // ② provider config 缺失（exe/cjs 都在）→ 与「不在场」分列的专属 hint
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let exe = std::path::Path::new(&root).join("ZCode.exe");
        std::fs::write(&exe, "stub").unwrap();
        let cjs = std::path::Path::new(&root)
            .join("resources")
            .join("glm")
            .join("zcode.cjs");
        std::fs::create_dir_all(cjs.parent().unwrap()).unwrap();
        std::fs::write(&cjs, "// stub").unwrap();
        let spec = ProbeSpec::Zcode {
            exe: exe.to_string_lossy().to_string(),
            cjs: cjs.to_string_lossy().to_string(),
        };
        let v = two_level_probe(
            &spec,
            "windows",
            || async { panic!("stat 失败时 L0 帮助面不得起") },
            || async { ProbeVerdict::pass("L1 不该跑") },
        )
        .await;
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(hint.contains("provider config"), "{hint}");
        assert!(!hint.contains("不在场"), "缺失与不在场必须分列: {hint}");
    }

    /// L0 快面红线：L0 过 **不替代** L1（`--help` 漏判静默形态，Task 8 实证教训）——
    /// L0 过只是放行，结论以 L1 为准（L1 fail ⇒ 整体 fail）。
    #[tokio::test]
    async fn l0_pass_does_not_shortcircuit_l1() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_dir, spec) = l0_ok_fixture();
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let r2 = ran.clone();
        let v = two_level_probe(
            &spec,
            "windows",
            || async { ProbeVerdict::pass("L0 过（--help 能起能印）") },
            move || async move {
                r2.fetch_add(1, Ordering::SeqCst);
                ProbeVerdict::pass("L1 真回合出 JSON")
            },
        )
        .await;
        assert_eq!(ran.load(Ordering::SeqCst), 1, "L0 过必须放行 L1 真回合");
        assert!(v.is_pass());
        assert!(
            matches!(&v, ProbeVerdict::Pass { detail, .. } if detail.contains("L1")),
            "结论必须来自 L1（L0 的 detail 不得冒充通过结论）: {v:?}"
        );
        // 红线反向：L1 才有判定权——L0 过但 L1 fail ⇒ 整体 fail
        let v2 = two_level_probe(
            &spec,
            "windows",
            || async { ProbeVerdict::pass("L0 过") },
            || async { ProbeVerdict::fail("L1 静默无 JSON") },
        )
        .await;
        assert!(!v2.is_pass(), "L0 过不能替代 L1 的判定权");
    }

    /// 缓存纪律不回归：成功结论命中 → 探针零执行（L0/L1 都不跑；stub 文件真去
    /// spawn 必失败，返回 pass 即证明零执行）；mtime 键长缓存语义不变。
    #[tokio::test]
    async fn cached_success_skips_probe() {
        let (_dir, spec) = l0_ok_fixture();
        let cache = ProbeCache::default();
        cache.put(ProbeKey::of(&spec), ProbeVerdict::pass("已验证过"));
        let v = probe(&spec, &cache, "windows").await;
        assert!(v.is_pass());
        assert!(
            matches!(&v, ProbeVerdict::Pass { detail, .. } if detail == "已验证过"),
            "命中缓存必须原样返回: {v:?}"
        );
    }

    /// **返工 m-5**：L0 帮助面判定的三个失败分支（纯函数缝）——超时 / exit≠0 /
    /// exit 0 但零输出（能起不能印不算过）；正向形态（exit 0 + 有输出 = 能起能印）
    /// 顺手钉死。
    #[test]
    fn l0_help_verdict_covers_timeout_nonzero_exit_and_silent_zero_output() {
        // 超时 → fail（CLI 起动面已坏，hint 说明不烧真回合）
        let v = l0_help_verdict(Some(Stage::Timeout), None, "", "", "stub.exe");
        assert!(!v.is_pass(), "{v:?}");
        assert!(v.hint().unwrap_or_default().contains("未起未印"));
        // exit≠0（有输出也救不了——退出码非 0 = 起动面自己认输）
        let v = l0_help_verdict(None, Some(1), "usage: zcode ...", "", "stub.exe");
        assert!(!v.is_pass(), "{v:?}");
        assert!(v.hint().unwrap_or_default().contains("未能起能印"));
        // exit 0 但零输出 → fail（能起**不能印**不算过——「能起能印」两个条件缺一不可）
        let v = l0_help_verdict(None, Some(0), "", "", "stub.exe");
        assert!(!v.is_pass(), "{v:?}");
        assert!(v.hint().unwrap_or_default().contains("未能起能印"));
        // 正向：exit 0 + 有输出 → pass（放行 L1）
        let v = l0_help_verdict(None, Some(0), "Usage: zcode", "", "stub.exe");
        assert!(v.is_pass(), "{v:?}");
    }

    /// **返工 m-5**：三个 L0 失败形态经 [`two_level_probe`] 的 L0 注入点 → **L1 一次都不
    /// 烧**、最终结论 = L0 的 fail 原样透传（不重写不吞）。
    #[tokio::test]
    async fn l0_help_failure_shapes_block_l1_via_two_level_seam() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_dir, spec) = l0_ok_fixture();
        let shapes = vec![
            l0_help_verdict(Some(Stage::Timeout), None, "", "", "stub.exe"),
            l0_help_verdict(None, Some(1), "usage: zcode ...", "", "stub.exe"),
            l0_help_verdict(None, Some(0), "", "", "stub.exe"),
        ];
        for shape in shapes {
            let ran = std::sync::Arc::new(AtomicUsize::new(0));
            let r2 = ran.clone();
            let injected = shape.clone();
            let want_hint = shape.hint().map(str::to_string);
            let v = two_level_probe(
                &spec,
                "windows",
                move || async move { injected },
                move || async move {
                    r2.fetch_add(1, Ordering::SeqCst);
                    ProbeVerdict::pass("L1 不该跑")
                },
            )
            .await;
            assert!(!v.is_pass(), "L0 失败形态必须直接 fail: {v:?}");
            assert_eq!(ran.load(Ordering::SeqCst), 0, "L0 帮助面失败不烧 L1 真回合");
            assert_eq!(
                v.hint().map(str::to_string),
                want_hint,
                "最终结论 = L0 的 fail 原样透传"
            );
        }
    }

    /// Task 7：单 exe 键推导（codex app-server 探测的平行 spec 结构键单点）——
    /// 真 stat：在场带 mtime、缺席判 false、无辅助产物面
    #[test]
    fn probe_key_of_exe_stats_plain_binary() {
        let exe = std::env::current_exe().expect("测试二进制路径");
        let key = ProbeKey::of_exe(&exe.to_string_lossy());
        assert!(key.exe_present, "自身二进制必然在场: {key:?}");
        assert!(key.exe_mtime_ms.is_some());
        assert_eq!(key.aux_mtime_ms, None, "单 exe 键无辅助产物面");
        let bogus = ProbeKey::of_exe("mam-nonexistent-appserver-probe");
        assert!(!bogus.exe_present);
        assert!(bogus.exe_mtime_ms.is_none());
    }

    // ============================================================
    // 评审 I13：版本探针 single-flight（并发首发同 exe 只跑一次真探针）
    // ============================================================

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 合成探针执行体的返回类型（boxed：满足 `tokio::spawn` 的 `'static`，且能按
    /// 趟次给不同结论——「第二趟的结论」本身就是缺陷复现的证据）
    type SyntheticRound = std::pin::Pin<Box<dyn Future<Output = ProbeVerdict> + Send>>;

    /// 并发时序窗口（[`synthetic_round`] 用）：首个进执行体的请求先等「两个请求都已
    /// 进场」，再多停这么久——把**后到者的缓存检查**钉在**首者的缓存写入**之前。
    ///
    /// 为什么必须这样：旧实现（check-then-run 无锁）下若首者先写完缓存，后到者就会
    /// 「命中缓存」而看不出缺陷（用例假绿）。本窗口把「没有 single-flight ⇒ 后到者
    /// 必然 miss ⇒ 必然跑第二遍」变成**确定的时序**而非概率竞态：150ms ≫ 两次
    /// HashMap 查找，且后到者的缓存检查发生在它**同一轮 poll 内**（进场报名之后
    /// 立即执行），必然早于本窗口结束。
    const CONCURRENT_PROBE_WINDOW_MS: u64 = 150;

    /// 合成执行体（替代真 spawn：**不触模型、不付配额**，只计数）：
    /// - `runs` = 「真实探针跑了第几遍」（0 起）——测试断言它的总数 = 真回合次数；
    /// - `arrived` = 已进入探针的请求数（在**调探针之前**报名，见 [`race_two_probes`]）
    ///   ——首者的等待因此不依赖「后到者是否真的进了执行体」（single-flight 生效时
    ///   后到者永远进不了执行体，只能靠进场报名让它看到 2）。
    fn synthetic_round(
        arrived: Arc<AtomicUsize>,
        runs: Arc<AtomicUsize>,
        verdicts: Vec<ProbeVerdict>,
    ) -> impl FnOnce() -> SyntheticRound + Send + 'static {
        move || {
            Box::pin(async move {
                let nth = runs.fetch_add(1, Ordering::SeqCst);
                // 等两个请求都进场（2s 兜底：单边缺席时不挂死，只是白等）
                let deadline = Instant::now() + std::time::Duration::from_secs(2);
                while arrived.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(CONCURRENT_PROBE_WINDOW_MS))
                    .await;
                verdicts
                    .get(nth)
                    .cloned()
                    .unwrap_or_else(|| ProbeVerdict::fail("多余的探针趟次（single-flight 失效）"))
            })
        }
    }

    /// 并发两请求打同一个 cache / 同一个键（真并发任务，与生产里两客户端并发投递同构），
    /// 返回（请求 1 结论、请求 2 结论、**真探针执行体被调用的次数**）。
    /// `verdicts[n]` = 第 n 趟执行体的结论——趟次 >0 即缺陷复现。
    async fn race_two_probes(
        cache: Arc<ProbeCache>,
        key: ProbeKey,
        verdicts: Vec<ProbeVerdict>,
    ) -> (ProbeVerdict, ProbeVerdict, usize) {
        let arrived = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let cache = cache.clone();
            let key = key.clone();
            let mark = arrived.clone();
            let run = synthetic_round(arrived.clone(), runs.clone(), verdicts.clone());
            tasks.push(tokio::spawn(async move {
                // 进场报名（在探针之前）——首者据此确认「后到者已在跑」，从而把窗口
                // 拉长到后到者完成缓存检查之后
                mark.fetch_add(1, Ordering::SeqCst);
                probe_single_flight(&cache, key, run).await
            }));
        }
        let v1 = tasks.remove(0).await.expect("并发任务 1 必须正常结束");
        let v2 = tasks.remove(0).await.expect("并发任务 2 必须正常结束");
        (v1, v2, runs.load(Ordering::SeqCst))
    }

    /// **I13 主测**：两个客户端同时首发同一个 exe（同键）→ **真探针只跑一遍**，两边拿到
    /// 同一结论。抓缺陷机理：撤掉 single-flight（回到 check-then-run 无锁）后，后到者
    /// 必然 miss（时序窗口保证它先于首者的缓存写入完成检查）→ 执行体被调 2 次 = 两次
    /// **真实模型回合**、两份真实配额；本测随即变红（`runs == 2` + 后到者结论是第二趟的）。
    #[tokio::test]
    async fn concurrent_probe_runs_real_round_only_once() {
        let cache = Arc::new(ProbeCache::default());
        let key = ProbeKey::of_parts("mam-concurrent-zcode.exe", true, Some(42), Some(7));
        let leader = ProbeVerdict::pass("首者真回合出 JSON");
        let (v1, v2, runs) = race_two_probes(
            cache.clone(),
            key,
            vec![leader.clone(), ProbeVerdict::pass("第二遍不该跑")],
        )
        .await;
        assert_eq!(
            runs, 1,
            "并发首发同一 exe 只许跑一次真探针（single-flight 失效 = 真实配额被重复消耗）"
        );
        assert_eq!(v1, leader, "首者结论原样返回");
        assert_eq!(
            v2, leader,
            "后到者必须复用首者写入缓存的结论（不是自己重探的）"
        );
        assert_eq!(cache.len(), 1, "结论只落一格（缓存键/TTL 语义未变）");
        assert_eq!(
            cache
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1,
            "同 exe 只登记一个名额（名额表键空间与缓存同源）"
        );
    }

    /// **I13 失败面**：首者跑出**失败**结论（成本已付）→ 后到者复用失败结论、不再跑第二
    /// 遍；且失败结论仍按既有 [`FAILURE_TTL_MS`] 语义进缓存（TTL 内命中、过期即失效重探
    /// ——口径不因 single-flight 改变）。撤掉 single-flight 后本测同样变红：后到者会跑出
    /// 第二趟的 pass 结论（`v2 != leader`）且 `runs == 2`。
    #[tokio::test]
    async fn concurrent_probe_reuses_leader_failure_verdict() {
        let cache = Arc::new(ProbeCache::default());
        let key = ProbeKey::of_parts("mam-concurrent-zcode.exe", true, Some(42), Some(7));
        let leader = ProbeVerdict::fail("首者失败：工作区争用（真实回合成本已付）");
        let t0 = now_ms();
        let (v1, v2, runs) = race_two_probes(
            cache.clone(),
            key.clone(),
            vec![
                leader.clone(),
                ProbeVerdict::pass("第二遍不该跑（跑到这里 = 缺陷复现）"),
            ],
        )
        .await;
        assert_eq!(
            runs, 1,
            "失败结论也要共享——不得对同一个瞬时故障付两份真实回合成本"
        );
        assert!(
            !v1.is_pass() && !v2.is_pass(),
            "首者失败 ⇒ 两边都拒发：{v1:?} / {v2:?}"
        );
        assert_eq!(
            v2, leader,
            "后到者必须原样复用首者的失败结论（含 hint，不重写不重探）"
        );
        // 既有 TTL 口径未变（失败结论仍进缓存共享，不是「每次并发都重探」）
        assert!(
            cache.get_at(&key, t0 + FAILURE_TTL_MS - 1).is_some(),
            "TTL 内必须命中失败结论"
        );
        assert!(
            cache.get_at(&key, t0 + FAILURE_TTL_MS + 1_000).is_none(),
            "失败结论过期必须失效（通道不被永久钉死）"
        );
    }

    /// **I13 委派锁**：生产入口 [`probe`] 必须流经 [`probe_single_flight`]——否则上面的
    /// 并发去重对生产无效（只测内部缝就成了「测了个没人走的函数」）。观测点：缺席的可执行
    /// 文件走「不 spawn + 判失败 + 入缓存」老路（零 spawn、零真机依赖），但**名额表必须已
    /// 登记**——只有 single-flight 核心会登记；撤掉修复此测同样变红。
    #[tokio::test]
    async fn probe_entry_delegates_to_single_flight_gate() {
        let cache = ProbeCache::default();
        let spec = ProbeSpec::Codex {
            exe: "mam-nonexistent-i13-exe".into(),
        };
        let v = probe(&spec, &cache, "windows").await;
        assert!(!v.is_pass(), "可执行缺席必须判失败（不 spawn）: {v:?}");
        assert!(
            v.hint().unwrap_or_default().contains("不在场"),
            "缺席 hint 语义未变: {v:?}"
        );
        assert_eq!(cache.len(), 1, "缺席结论仍进缓存（既有语义未变）");
        assert!(
            cache
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("mam-nonexistent-i13-exe"),
            "probe 必须流经 single-flight 名额表（否则并发去重对生产入口无效）"
        );
    }
}
