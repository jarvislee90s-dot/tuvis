// 版本门控探针（H6）：投递前校验各工具 flag 面，结果缓存 + 版本变化提示复核，不盲发。
//
// **探测定案（spec 附录 E / H7，测试断言以此为真值）**：
// - zcode：**必须 `--prompt` 干跑**——`--version`/`--help` 不需要 provider config，
//   会漏判「Mac 打包布局 bug 下缺 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 时 `--prompt`
//   静默无 JSON」；干跑超时 15s，**能出 JSON 即过**（前缀噪音行不算失败）；
// - codex：`queue --help` 子命令在场（experimental 面，升级可能消失）。
//
// 结果缓存键 = **可执行文件在场性 + mtime**（含辅助产物如 zcode.cjs）——工具升级即
// 失效重探（版本漂移复核）；进程缺席直接判失败，绝不 spawn 一个不存在的程序。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::receipt::{parse_json_skipping_prefix, Receipt, Stage};
use super::runner::{GlobalSem, RunnerCfg};

/// 探针干跑超时（H6：15s；探针是短命干跑，不是 turn）
pub const PROBE_TIMEOUT_MS: u64 = 15_000;
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
pub fn probe_argv(spec: &ProbeSpec) -> Vec<String> {
    match spec {
        // `--prompt <最短非空载荷>` 干跑：真机 CLI 拒绝空串（见 [`PROBE_PROMPT`]）；
        // provider 缺失/打包错位时这里就出不了 JSON → 门控拦下
        ProbeSpec::Zcode { cjs, .. } => vec![
            cjs.clone(),
            "--prompt".into(),
            PROBE_PROMPT.into(),
            "--mode".into(),
            "yolo".into(),
            "--json".into(),
        ],
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

/// 探针结论（失败即 version_gate 拒发，H6）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    Pass { detail: String },
    Fail { hint: String },
}

impl ProbeVerdict {
    pub fn pass(detail: impl Into<String>) -> Self {
        ProbeVerdict::Pass {
            detail: detail.into(),
        }
    }

    pub fn fail(hint: impl Into<String>) -> Self {
        ProbeVerdict::Fail { hint: hint.into() }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, ProbeVerdict::Pass { .. })
    }

    pub fn hint(&self) -> Option<&str> {
        match self {
            ProbeVerdict::Fail { hint } => Some(hint),
            ProbeVerdict::Pass { .. } => None,
        }
    }
}

/// 拒发回执（H6：探针失败 = version_gate 拒发回执——通过时 `None`，不生成多余回执）
pub fn version_gate_receipt(session_id: &str, verdict: &ProbeVerdict) -> Option<Receipt> {
    match verdict {
        ProbeVerdict::Pass { .. } => None,
        ProbeVerdict::Fail { hint } => Some(
            Receipt::failed(
                Stage::VersionGate,
                &format!("版本门控未通过（拒发，不盲发）；{hint}"),
            )
            .with_session(session_id),
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
            "Mac 打包布局下须设 {ZCODE_PROVIDER_CONFIG_ENV}=<Resources>/config/provider/zcode-builtin.json（不设则 --prompt 静默无 JSON）"
        )
    } else {
        "检查 ZCode 安装与 provider 配置（--prompt 干跑无 JSON 输出）".to_string()
    };
    ProbeVerdict::fail(format!(
        "zcode --prompt 干跑未出 JSON（exit={exit:?}）{mac_hint}；stderr={detail}"
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
        "codex queue 子命令不在场（experimental 面，升级可能消失；exit={exit:?}）；stderr={detail}"
    ))
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
        ProbeVerdict::fail(format!("可执行文件不在场（不 spawn）：{}", key.exe))
    };
    cache.put_at(key, verdict.clone(), now);
    verdict
}

/// 生产探针（真 spawn；Task 8/9/12 消费）：走 H4 底座（watchdog = [`PROBE_TIMEOUT_MS`]），
/// stdout/stderr 归一后交纯判定，结论入缓存。**探针不占无头 turn 的全局名额**
/// （自持 cap=1 的名额——版本门控是前置动作，不该让用户请求陪它排队）。
pub async fn probe(spec: &ProbeSpec, cache: &ProbeCache, os: &str) -> ProbeVerdict {
    let key = ProbeKey::of(spec);
    if let Some(hit) = cache.get(&key) {
        return hit;
    }
    if !key.exe_present {
        let v = ProbeVerdict::fail(format!("可执行文件不在场（不 spawn）：{}", key.exe));
        cache.put(key, v.clone());
        return v;
    }
    let mut r = RunnerCfg::new(spec.exe())
        .args(probe_argv(spec))
        .timeout_ms(PROBE_TIMEOUT_MS)
        .sem(GlobalSem::new(1));
    for (k, v) in probe_env(spec, os) {
        r = r.env(k, v);
    }
    let receipt = r.run().await;
    let verdict = if receipt.stage == Some(Stage::Timeout) {
        ProbeVerdict::fail("探针干跑超时（15s）——工具可能挂死或缺 provider config")
    } else if receipt.stage == Some(Stage::Spawn) {
        ProbeVerdict::fail(format!("探针 spawn 失败：{}", spec.exe()))
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
    };
    cache.put(key, verdict.clone());
    verdict
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
}
