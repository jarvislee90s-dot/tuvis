//! Codex 兼容通道 · 实机 E2E（H8）
//!
//! 本文件只有 **Codex 单例**（本轮交付范围；zcode / WorkBuddy / claude-kimi-opencode
//! 的实机 E2E 留在 PR #117 续作）。用例 **`#[ignore]`**：常规门禁（`cargo test` 不带
//! `--ignored`）只验证本文件**能编译**，零真实调用、零配额消耗。实机跑法：
//!
//! ```text
//! cd src-tauri
//! cargo test --test headless_e2e codex_queue_consumed -- --ignored --nocapture
//! ```
//!
//! ## 前置（缺席即如实 skip，绝不静默当通过）
//! - codex CLI 在场（PATH 上 `codex` / `codex.exe` / `codex.cmd`）；
//! - **Desktop（ChatGPT.app / Codex.app）在场且已打开目标 thread**（queue 路前提）；
//! - 一条 24h 内的 rollout，或用 `MAM_E2E_CODEX_THREAD`（必需）/ `MAM_E2E_CODEX_ROLLOUT`
//!   / `MAM_E2E_PROJECT` 钉住。
//!
//! ## 成本与风险
//! 本例**真的会入队一条消息并写真实 Codex 会话库**（消费等待最长
//! `CONSUME_ATTEMPTS × CONSUME_INTERVAL_MS` = 60s），**不进 CI、不无人值守跑**。
//!
//! ## 证据纪律
//! 本文件在交付批只做到「能编译 + `--ignored --list` 列出用例名」，**未实机执行**
//! （配额与目标会话纪律）。实机行为属未取证面；失败先读回执 stage/reason。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use multi_agents_manager_lib::inject::headless;
use multi_agents_manager_lib::inject::headless::codex;
use multi_agents_manager_lib::inject::headless::receipt::ReceiptStatus;
use multi_agents_manager_lib::inject::headless::runner;
use multi_agents_manager_lib::inject::headless::turn;
use multi_agents_manager_lib::inject::normalize;
use multi_agents_manager_lib::inject::routing::HeadlessKind;
use multi_agents_manager_lib::session::model::ProcessForm;

/// 设备花名（正文尾签名 = ` [mobile e2e]`；与终端注入同一条 W4 组装口径）
const E2E_DEVICE: &str = "e2e";
/// rollout 发现的新鲜窗（codex 例自动发现探针会话时用）
const ROLLOUT_FRESH: Duration = Duration::from_secs(24 * 3600);

// ============================================================
// 通用：如实跳过 / 环境钉 / 探针目标发现
// ============================================================

/// 如实跳过：打印原因并返回——**绝不静默算过**。
fn skip(case: &str, why: &str) {
    eprintln!("[SKIP] {case}：{why}");
}

/// 通过（与 `[SKIP]` 成对，输出里一眼能数清「跑了几例、跳了几例」）
fn pass(case: &str) {
    eprintln!("[PASS] {case}");
}

/// 环境钉（空串/纯空白 = 未设）
fn pin(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 用户主目录（读真实 CLI 数据根；缺席即 skip）
fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 探针正文（`MAM_E2E_TEXT` 可覆盖；默认最短非空）
fn probe_text() -> String {
    pin("MAM_E2E_TEXT").unwrap_or_else(|| "hi".to_string())
}

/// 最终载荷（W4 单点组装：换行归一字面化 + 尾签名 + 花名白名单）
fn payload() -> String {
    normalize::compose_injection(E2E_DEVICE, &probe_text())
}

/// codex 探针目标（thread UUID / rollout 路径 / 项目目录）
struct CodexProbe {
    thread: String,
    rollout: Option<String>,
    project: String,
}

/// rollout 头部首行的 `payload.cwd`（**测试本地小读件**：生产读件是 `pub(crate)`，
/// 集成测试拿不到；这里只读首行、只取一个字段，不复制任何生产解析逻辑）
fn rollout_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let first = std::io::BufReader::new(f).lines().next()?.ok()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.get("payload")?
        .get("cwd")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// `~/.codex/sessions` 下最新的 rollout（深度受限递归：`<Y>/<M>/<D>/rollout-*.jsonl`）
fn newest_rollout(root: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: usize, best: &mut Option<(SystemTime, PathBuf)>) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, best);
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                continue;
            }
            let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                *best = Some((mtime, p));
            }
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    walk(root, 0, &mut best);
    let (mtime, path) = best?;
    let fresh = mtime
        .elapsed()
        .map(|age| age <= ROLLOUT_FRESH)
        .unwrap_or(true);
    fresh.then_some(path)
}

/// codex 探针目标：环境钉优先；否则取 `~/.codex/sessions` 下最新 rollout（thread id 取
/// **文件名 UUID**，spec H8 唯一来源；项目目录取 rollout 头 `session_meta.cwd`）。
fn codex_probe(home: &Path) -> Result<CodexProbe, String> {
    if let Some(thread) = pin("MAM_E2E_CODEX_THREAD") {
        let rollout = pin("MAM_E2E_CODEX_ROLLOUT");
        let project =
            match (pin("MAM_E2E_PROJECT"), rollout.as_deref()) {
                (Some(p), _) => p,
                (None, Some(r)) => rollout_cwd(Path::new(r)).ok_or_else(|| {
                    format!("MAM_E2E_CODEX_ROLLOUT 指向的 rollout 头部读不到 cwd：{r}")
                })?,
                (None, None) => return Err(
                    "给了 MAM_E2E_CODEX_THREAD 就需要 MAM_E2E_PROJECT（或 MAM_E2E_CODEX_ROLLOUT \
                 让本项目从头 session_meta 读 cwd）"
                        .to_string(),
                ),
            };
        return Ok(CodexProbe {
            thread,
            rollout,
            project,
        });
    }
    let root = home.join(".codex").join("sessions");
    let newest = newest_rollout(&root).ok_or_else(|| {
        format!(
            "{} 下没有 24h 内的 rollout-*.jsonl（还没跑过 codex？）——或用 \
             MAM_E2E_CODEX_THREAD / MAM_E2E_CODEX_ROLLOUT / MAM_E2E_PROJECT 钉住",
            root.display()
        )
    })?;
    let thread = codex::thread_id_of(&newest.to_string_lossy()).ok_or_else(|| {
        format!(
            "最新 rollout 文件名里没有 UUID 形态的 thread id：{}",
            newest.display()
        )
    })?;
    let project = rollout_cwd(&newest).ok_or_else(|| {
        format!(
            "rollout 头部读不到 cwd（session_meta）：{}——请用 MAM_E2E_PROJECT 钉住",
            newest.display()
        )
    })?;
    Ok(CodexProbe {
        thread,
        rollout: Some(newest.to_string_lossy().to_string()),
        project,
    })
}

// ============================================================
// codex：queue 路的消费确认
// ============================================================

/// codex APP 托管会话：`queue` 入队 → **消费确认**（rollout 追加命中）。
/// PASS 判据：回执 `ok` 且回执来源 = `RolloutAppend`（只拿到入队回执 = `Queued`/`Enqueue`
/// 不算通过——那只是「已入队未消费」，见回执 reason 原文）。
#[tokio::test]
#[ignore = "实机显式跑：codex queue 真入队+等消费（前置=codex CLI 在场 + ChatGPT.app 在场且目标 thread 已打开）"]
async fn codex_queue_consumed() {
    let case = "codex_queue_consumed";
    let os = std::env::consts::OS;
    let Some(home) = home() else {
        return skip(case, "无法确定用户主目录");
    };
    // ① CLI 在场
    let Some(exe) = codex::production_exe() else {
        return skip(
            case,
            "codex CLI 不可达（PATH 上未找到 codex / codex.exe / codex.cmd）",
        );
    };
    // ② APP 在场（queue 路前提：thread 被 APP 打开才会被消费；关着就该走 exec 路，非本例）
    let presence = codex::presence_of(&codex::production_presence(ProcessForm::App, 0));
    if presence != codex::AppPresence::Open {
        return skip(
            case,
            "codex APP 不在场（queue 路前提）——请先打开 ChatGPT.app / Codex.app 并打开目标会话；\
             只想验 exec 兜底请走终端注入面",
        );
    }
    // ③ 探针目标
    let probe = match codex_probe(&home) {
        Ok(v) => v,
        Err(why) => return skip(case, &why),
    };
    // ④ 版本门控（queue 子命令在场；不触模型、不占无头名额）
    let verdict = codex::probe_cli(&exe, codex::probe_cache(), os).await;
    if !verdict.is_pass() {
        panic!(
            "{case}：版本门控未通过（queue 子命令不在场？）——hint={:?}",
            verdict.hint()
        );
    }
    let text = payload();
    let Some(_slot) = turn::registry().begin_guarded(
        &probe.thread,
        codex::TurnSlot::placeholder("codex", text.clone())
            .with_channel(HeadlessKind::CodexQueue.wire_name()),
    ) else {
        return skip(
            case,
            &format!("thread {} 已有在飞的无头回合（串行锁）", probe.thread),
        );
    };
    eprintln!(
        "[E2E] {case}：thread={}；rollout={:?}；项目={}；CLI={exe}",
        probe.thread, probe.rollout, probe.project
    );

    // ⑤ 计划 + 真回合（消费确认在 queue 路内部自建）
    let plan = codex::dispatch(presence, &probe.thread, &probe.project, &text);
    assert!(
        matches!(plan, codex::Plan::Queue { .. }),
        "{case}：APP 在场却没有分派到 queue 路——分派纯核与在场判定不一致，先查这条"
    );
    let shape = codex::spawn_shape(&exe, os);
    let build = |plan: &codex::Plan| {
        // 与端点同规：`shape.program` + `shape.prefix` ++ 计划载荷段（`-C` 已在计划里）；
        // 差异只在超时取默认值（端点读设置页）
        let mut cfg = runner::RunnerCfg::new(shape.program.clone());
        cfg = cfg
            .args(
                shape
                    .prefix
                    .iter()
                    .cloned()
                    .chain(plan.args().iter().cloned()),
            )
            .session_id(probe.thread.clone())
            .timeout_ms(headless::DEFAULT_TIMEOUT_MS);
        cfg
    };
    let deps = codex::TurnDeps::production();
    let seam = codex::run_seam();
    let args = codex::TurnArgs {
        sid: &probe.thread,
        project: &probe.project,
        rollout: probe.rollout.as_deref(),
        gate: None,
    };
    let out = codex::run_turn(&args, &plan, &build, &deps, &*seam).await;
    eprintln!(
        "[E2E] {case}：回执={:?}；走向={:?}；改道={}；来源={:?}",
        out.receipt, out.plan_used, out.diverted, out.receipt_source
    );
    // 消费等待由 queue 路内部自建（30×2000ms 预算）；本例只做终态断言

    assert_eq!(
        out.receipt.status,
        ReceiptStatus::Ok,
        "{case}：回执不是 ok（stage={:?}，reason={:?}）",
        out.receipt.stage,
        out.receipt.reason
    );
    assert_eq!(
        out.receipt_source,
        codex::ReceiptSource::RolloutAppend,
        "{case}：**消费未确认**（未观测到目标 rollout 追加）——回执 reason 原文={:?}；\
         最常见原因 = 该 thread 未在 APP 打开",
        out.receipt.reason
    );
    pass(case);
}
