// 版本门控探针（H6）：投递前校验 codex 兼容通道的 flag 面，结果缓存 + 版本变化提示
// 复核，不盲发。
//
// **探测定案（spec 附录 E / H8）**：codex = `queue --help` 子命令在场（experimental 面，
// 升级可能消失）；零模型成本、不占无头回合名额。
//
// 结果缓存键 = **可执行文件在场性 + mtime**——工具升级即失效重探（版本漂移复核）；
// 进程缺席直接判失败，绝不 spawn 一个不存在的程序。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::receipt::{Receipt, Stage};

/// 探针超时（45s）：`queue --help` 实测远小于此值，留足冷启动余量。
pub const PROBE_TIMEOUT_MS: u64 = 45_000;

/// 探针对象（argv/env 由本模块派生）：本轮只服务 codex 兼容通道
/// （`queue --help` 子命令在场性；零模型成本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSpec {
    /// codex（原生 CLI）
    Codex { exe: String },
}

impl ProbeSpec {
    pub fn exe(&self) -> &str {
        match self {
            ProbeSpec::Codex { exe } => exe,
        }
    }

    /// 本轮无辅助产物（zcode.cjs 属续作通道）。
    pub fn aux_path(&self) -> Option<&str> {
        match self {
            ProbeSpec::Codex { .. } => None,
        }
    }
}

/// 探针 argv（**不含 exe 自身**；生产 spawn = exe + 本 argv + [`probe_env`]）
///
/// codex 的过判据是 `queue` 子命令在场（`queue --help` 的 usage 文本）。
pub fn probe_argv(spec: &ProbeSpec) -> Vec<String> {
    match spec {
        // codex：`queue --help`（子命令在场性；不触模型、不占回合名额）
        ProbeSpec::Codex { .. } => vec!["queue".into(), "--help".into()],
    }
}

/// 探针环境：原生 CLI 无 Electron/provider-config 面（codex 恒空）。
pub fn probe_env(spec: &ProbeSpec, os: &str) -> Vec<(String, String)> {
    let _ = (spec, os);
    // 原生 CLI 不带 Electron 开关（带着反而会被误当 Electron 参数）
    Vec::new()
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
/// 探针成本低但结论仍影响投递——一次瞬时失败（工作区争用
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::headless::receipt::Stage;

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

    /// 原生 CLI 不带 Electron 开关（带着反而会被误当 Electron 参数）
    #[test]
    fn codex_probe_env_is_empty() {
        let spec = ProbeSpec::Codex {
            exe: "codex".into(),
        };
        assert!(probe_env(&spec, "windows").is_empty());
        assert!(probe_env(&spec, "macos").is_empty());
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

    /// 探针失败 = version_gate 拒发回执 + 修复提示（queue 子命令消失是升级面）
    #[test]
    fn probe_failure_maps_to_version_gate_with_remediation_hint() {
        let v = codex_verdict("", "error: unrecognized subcommand 'queue'", Some(2));
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(
            hint.contains("queue") && hint.contains("升级"),
            "升级导致子命令消失的修复提示必须给出: {hint}"
        );
        let r = version_gate_receipt("s1", &v).expect("拒发必须出回执");
        assert_eq!(r.stage, Some(Stage::VersionGate));
        assert_eq!(
            r.status,
            crate::inject::headless::receipt::ReceiptStatus::Failed
        );
        assert_eq!(r.session_id, "s1");
        // 通过时不出拒发回执
        assert!(version_gate_receipt("s1", &ProbeVerdict::pass("ok")).is_none());
    }

    /// 缓存命中不重跑；**mtime 变化即失效**（工具升级 = 版本漂移复核，H6）
    #[test]
    fn probe_cache_hit_and_mtime_invalidation() {
        let cache = ProbeCache::default();
        let key = ProbeKey::of_parts("codex.exe", true, Some(1_000), None);
        cache.put(key.clone(), ProbeVerdict::pass("first"));
        // 同键（进程在场 + mtime 未变）→ 命中缓存
        let v2 = cache.get(&key).expect("同键必须命中");
        assert!(v2.is_pass(), "命中缓存必须返回上次结论");
        // mtime 变（工具升级）→ 键不等 = 未命中，重探后覆盖
        let bumped = ProbeKey::of_parts("codex.exe", true, Some(9_999), None);
        assert!(cache.get(&bumped).is_none(), "mtime 变化必须失效重探");
        cache.put(bumped, ProbeVerdict::fail("升级后 flag 面变了"));
        assert_eq!(
            cache.len(),
            1,
            "同一可执行文件只占一格（升级后的新结论覆盖旧的）"
        );
        // 进程缺席也是缓存键的一部分（缺席 → 键不等 → 未命中，直接判失败不 spawn）
        let missing = ProbeKey::of_parts("nope.exe", false, None, None);
        assert!(!missing.exe_present);
        assert!(cache.get(&missing).is_none());
        assert_eq!(cache.len(), 1, "未命中的键不写入缓存");
    }

    /// 失败结论**带 TTL**、成功结论长缓存——一次瞬时失败不得把通道钉死整个进程生命周期；
    /// 反过来也不能让「坏着」的通道每次投递都付一次探针成本（TTL 内仍复用失败结论）。
    #[test]
    fn failure_verdicts_expire_but_successes_do_not() {
        let cache = ProbeCache::default();
        let key = ProbeKey::of_parts("codex.exe", true, Some(1), None);
        cache.put_at(key.clone(), ProbeVerdict::fail("瞬时抖动"), 1_000);
        // TTL 内命中（坏着时不重复付探针成本）
        let hit = cache
            .get_at(&key, 1_000 + FAILURE_TTL_MS - 1)
            .expect("TTL 内必须命中");
        assert!(!hit.is_pass());
        // 过期即未命中 → 下一次投递自然重探（通道不会被永久钉死）
        assert!(
            cache.get_at(&key, 1_000 + FAILURE_TTL_MS).is_none(),
            "失败结论过期必须失效（否则一次抖动 = 永久拒发）"
        );
        // 成功不受 TTL 约束（mtime 键长缓存：工具升级才失效）
        cache.put_at(key.clone(), ProbeVerdict::pass("ok"), 1_000);
        assert!(
            cache
                .get_at(&key, 1_000 + FAILURE_TTL_MS * 100)
                .is_some_and(|v| v.is_pass()),
            "成功结论必须长缓存"
        );
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
        assert_eq!(key.aux_mtime_ms, None, "codex 键无辅助产物面");
        let bogus = ProbeKey::of(&ProbeSpec::Codex {
            exe: "mam-nonexistent-probe-xyz".into(),
        });
        assert!(!bogus.exe_present);
        assert!(bogus.exe_mtime_ms.is_none());
        // 缺席即拒发（不 spawn 一个不存在的程序）：缺席键不得命中缓存
        assert!(ProbeCache::default().get(&bogus).is_none());
    }

    /// A 兜底：阈值钉值 45s（本机实测最小回合 15–19s ×2 余量；改常量必须连着 hint 走，
    /// 见 [`timeout_with_zero_output_fails_with_new_hint`]）
    #[test]
    fn probe_timeout_is_45s() {
        assert_eq!(PROBE_TIMEOUT_MS, 45_000);
    }

    /// 超时且零输出 → Fail + 新 hint（秒数从常量格式化生成、含「网络/模型服务」，
    /// 旧误导归因「挂死」必须退场）
    #[test]
    fn timeout_with_zero_output_fails_with_new_hint() {
        let v = ProbeVerdict::fail(timeout_zero_output_hint());
        assert!(!v.is_pass());
        let hint = v.hint().unwrap_or_default();
        assert!(
            hint.contains(&format!("{}s", PROBE_TIMEOUT_MS / 1000)),
            "秒数必须从常量生成（改阈值文案跟着走）: {hint}"
        );
        assert!(hint.contains("网络/模型服务"), "{hint}");
        assert!(!hint.contains("挂死"), "旧误导归因必须退场: {hint}");
    }
}
