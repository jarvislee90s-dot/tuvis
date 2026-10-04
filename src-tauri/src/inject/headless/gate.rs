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
        // --prompt "" 干跑：不真喂消息（provider 缺失时这里就会静默无 JSON → 门控拦下）
        ProbeSpec::Zcode { cjs, .. } => vec![
            cjs.clone(),
            "--prompt".into(),
            String::new(),
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

/// 探针环境（平台分叉）：Electron 主程序当 node 跑恒定；macOS 另设 provider config
pub fn probe_env(spec: &ProbeSpec, os: &str) -> Vec<(String, String)> {
    match spec {
        ProbeSpec::Zcode { cjs, .. } => {
            let mut env = vec![("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())];
            if os == "macos" {
                if let Some(cfg) = zcode_provider_config_path(cjs) {
                    env.push((ZCODE_PROVIDER_CONFIG_ENV.to_string(), cfg));
                }
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

/// 探针结果缓存（进程存在性 + mtime 键 → 结论）
#[derive(Default)]
pub struct ProbeCache {
    map: Mutex<HashMap<String, (ProbeKey, ProbeVerdict)>>,
}

impl ProbeCache {
    pub fn get(&self, key: &ProbeKey) -> Option<ProbeVerdict> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(&key.exe) {
            Some((k, v)) if k == key => Some(v.clone()),
            _ => None,
        }
    }

    pub fn put(&self, key: ProbeKey, verdict: ProbeVerdict) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.exe.clone(), (key, verdict));
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 带缓存的探针执行：命中即返回（不跑执行体）；缺席的可执行文件直接判失败（不 spawn）
pub fn probe_cached<F>(cache: &ProbeCache, key: ProbeKey, run: F) -> ProbeVerdict
where
    F: FnOnce() -> ProbeVerdict,
{
    if let Some(hit) = cache.get(&key) {
        return hit;
    }
    let verdict = if key.exe_present {
        run()
    } else {
        ProbeVerdict::fail(format!("可执行文件不在场（不 spawn）：{}", key.exe))
    };
    cache.put(key, verdict.clone());
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
        // 干跑档 = yolo + --json（能出 JSON 即过）
        assert!(argv.iter().any(|a| a == "--mode"));
        assert!(argv.iter().any(|a| a == "--json"));
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
            "Windows 只设 Electron 当 node 跑的开关"
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
