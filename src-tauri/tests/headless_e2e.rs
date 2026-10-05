//! H 系无头通道 · 实机 E2E（Task 14 / plan Task 14 Step 1）
//!
//! 四例**全部 `#[ignore]`**：常规门禁（`cargo test` 不带 `--ignored`）只验证本文件**能编译**，
//! 零真实调用、零配额消耗。实机跑法：
//!
//! ```text
//! cd src-tauri
//! cargo test --test headless_e2e -- --ignored --nocapture --test-threads=1
//! # 单例：cargo test --test headless_e2e zcode_send_roundtrip -- --ignored --nocapture
//! ```
//!
//! ## 跨平台纪律（Task 14 明令：**不做 windows-only 编译门**）
//! 无头通道本就双平台（zcode 两端 argv 分叉 / WB 端点两形态 / claude·kimi·opencode 原生 CLI），
//! 故本文件**没有** `#![cfg(windows)]`（对照 `m9r_e2e.rs` 的 Windows 门）。平台/工具/端点层面的
//! 不可跑一律在**运行期**处理：`eprintln!("[SKIP] …原因…")` + 提前 return——**绝不静默当通过，
//! 也绝不用编译期排除**。故四例全 skip 也是一份如实输出（每行都写了为什么）。
//!
//! ## 前置（逐例打印原因；缺席即 skip）
//! - `zcode_send_roundtrip`：ZCode 安装在场（`zcode::resolve_spec`）+ `~/.zcode` 库里有「目录在场的在册 interactive 会话」（或用 `MAM_E2E_ZCODE_SESSION` + `MAM_E2E_PROJECT` 钉住）。
//! - `codex_queue_consumed`：codex CLI 在场 + **ChatGPT.app / Codex.app 在场**（queue 路前提）+ 一条 24h 内的 rollout（或用 `MAM_E2E_CODEX_THREAD`（必需）/ `MAM_E2E_CODEX_ROLLOUT` / `MAM_E2E_PROJECT` 钉住）。
//! - `wb_acp_prompt_landed`：WorkBuddy 端点在场（心跳 `endpoint` 或端口指纹；**Win 5.7.3 未启用远程控制 ⇒ 本机走如实 skip**，风险 16）+ `workbuddy.db` 里有「目录在场的会话」（或用 `MAM_E2E_WB_SESSION` + `MAM_E2E_PROJECT` 钉住）。
//! - `claude_approval_roundtrip`：claude CLI 在场 + 一个可当工作目录的项目（默认 = 本仓库根，可用 `MAM_E2E_PROJECT` 改）；给了 `MAM_E2E_CLAUDE_SESSION` 则走 `--resume`，否则走 fresh `--session-id`。
//!
//! 公共覆盖：`MAM_E2E_TEXT` = 消息正文（默认 `hi`；**claude 例例外**——它默认用「列出本目录文件」以触发工具审批，见 `CLAUDE_TOOL_TEXT`）。正文一律经 W4 单点 [`normalize::compose_injection`] 组装，故真实落盘文本带 ` [mobile e2e]` 尾签名——这也是消费/落盘核验的 grep 锚。
//!
//! ## 成本与风险（跑一次要花什么——必读）
//! 这四例**真的会消耗真实账号配额、并写真实工具会话库**：
//! - zcode 例先跑版本门控探针（= **一次最小真实回合**，`gate::PROBE_PROMPT`）再跑真回合 → 2 次调用；
//! - codex 例真入队并等消费（最长 `CONSUME_ATTEMPTS × CONSUME_INTERVAL_MS` = 60s）；
//! - WB 例真投 ACP prompt（写入 WorkBuddy 会话）；
//! - claude 例真跑一个**带审批**的回合（批准工具执行）。
//!
//! 故：**不进 CI、不无人值守跑**；跑之前确认目标会话是「可以随便插一句」的。真机串行
//! （`--test-threads=1`）是建议而非必需（四例互不共享会话），但并发跑会同时占无头全局名额。
//!
//! ## 证据纪律（Task 14 交底，别把它们读成「已实机验证」）
//! 本文件在 Task 14 只做到任务书要求的「**空跑验证编译 + `--ignored --list` 列出四名**」，
//! **四例均未在 Task 14 期间实机执行**（配额红线）。故：① 四例的实机行为属**未取证面**；
//! ② 首次实机跑若断言失败，先读失败信息里的诊断串（回执 stage/reason、判定、佐证、帧统计）
//! 再回报主线；③ 与生产端点的**唯一已知差异**：本文件用 `RunnerCfg::new` + 底座默认
//! 600s 超时（端点侧经 `headless::runner_from_conn` 读设置页值、并落审计行）——E2E 不建
//! HTTP/DB 上下文，故**不写审计表、不读设置页超时**，链路本身同源。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use multi_agents_manager_lib::inject::headless;
use multi_agents_manager_lib::inject::headless::cli_three as c3;
use multi_agents_manager_lib::inject::headless::codex;
use multi_agents_manager_lib::inject::headless::gate;
use multi_agents_manager_lib::inject::headless::receipt::{ReceiptStatus, Stage};
use multi_agents_manager_lib::inject::headless::runner;
use multi_agents_manager_lib::inject::headless::turn;
use multi_agents_manager_lib::inject::headless::wb_acp;
use multi_agents_manager_lib::inject::headless::zcode;
use multi_agents_manager_lib::inject::normalize;
use multi_agents_manager_lib::inject::routing::HeadlessKind;
use multi_agents_manager_lib::monitor::workbuddy_parser;
use multi_agents_manager_lib::monitor::zcode_parser;
use multi_agents_manager_lib::session::model::ProcessForm;

/// 设备花名（正文尾签名 = ` [mobile e2e]`；与终端注入同一条 W4 组装口径）
const E2E_DEVICE: &str = "e2e";
/// 等审批帧的上限（claude 例：回合起跑到 `can_use_tool` 出现）
const APPROVAL_WAIT: Duration = Duration::from_secs(180);
/// 等回合终结的上限（claude 例；> 生产默认 600s 不行，这里给 300s 就够一个短回合）
const TURN_WAIT: Duration = Duration::from_secs(300);
/// claude 例的默认正文（**必须触发工具**才会弹审批卡；措辞同 Task 15 M10）
const CLAUDE_TOOL_TEXT: &str = "列出本目录文件";
/// rollout 发现的新鲜窗（codex 例自动发现探针会话时用）
const ROLLOUT_FRESH: Duration = Duration::from_secs(24 * 3600);

// ============================================================
// 通用：如实跳过 / 环境钉 / 探针目标发现
// ============================================================

/// 如实跳过：打印原因并返回——**绝不静默算过**（Task 14 的平台条件 skip 口径）。
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

/// 用户主目录（四例都要读真实 CLI 数据根；缺席即 skip）
fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 探针正文（`MAM_E2E_TEXT` 可覆盖；默认最短非空）
fn probe_text() -> String {
    pin("MAM_E2E_TEXT").unwrap_or_else(|| "hi".to_string())
}

/// 最终载荷（W4 单点组装：换行归一字面化 + 尾签名）
fn payload() -> String {
    normalize::compose_injection(E2E_DEVICE, &probe_text())
}

/// 会话号（claude fresh 形态用）：时间纳秒 xor pid → 8-4-4-4-12 hex，并强制 v4 版本位与
/// RFC 变体位。只要求形态合法（CLI 不做严格版本校验），故不为此引 UUID 依赖。
fn fresh_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = nanos ^ ((std::process::id() as u128) << 64);
    let mut chars: Vec<char> = format!("{mix:032x}").chars().collect();
    chars[12] = '4';
    chars[16] = '8';
    let hex: String = chars.into_iter().collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// zcode 探针会话（`sess_id`, 项目目录）：环境钉优先；否则取 `~/.zcode` 库里**目录在场**的
/// 最近一条在册 interactive 会话（读件复用读链路 `stored_sessions_home`，不另写库查询）。
fn zcode_probe_session(home: &Path) -> Result<(String, String), String> {
    let sid = pin("MAM_E2E_ZCODE_SESSION");
    let proj = pin("MAM_E2E_PROJECT");
    match (sid, proj) {
        (Some(sid), Some(proj)) => return Ok((sid, proj)),
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "MAM_E2E_ZCODE_SESSION 与 MAM_E2E_PROJECT 必须成对给出（只给一半无法定位会话）"
                    .to_string(),
            )
        }
        (None, None) => {}
    }
    let rows = zcode_parser::stored_sessions_home(home).ok_or_else(|| {
        "~/.zcode 会话库不可读（缺库 / 被锁 / 格式漂移）——无探针会话可定".to_string()
    })?;
    rows.into_iter()
        .find(|(_, dir, _)| !dir.trim().is_empty() && Path::new(dir).is_dir())
        .map(|(id, dir, _)| (id, dir))
        .ok_or_else(|| {
            "~/.zcode 库里没有「目录在场的在册会话」——请先在 ZCode 里开一个会话说一句，\
             或用 MAM_E2E_ZCODE_SESSION / MAM_E2E_PROJECT 钉住"
                .to_string()
        })
}

/// codex 探针目标（thread UUID / rollout 路径 / 项目目录）
struct CodexProbe {
    thread: String,
    rollout: Option<String>,
    project: String,
}

/// rollout 头部首行的 `payload.cwd`（**测试本地小读件**：生产读件 `jsonl::extract_cwd_from_jsonl`
/// 是 `pub(crate)`，集成测试拿不到；这里只读首行、只取一个字段，不复制任何生产解析逻辑）
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
        .unwrap_or(true); // 时钟回拨：不因此判「不新鲜」
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

/// WB 探针会话（`session_id`, cwd）：环境钉优先；否则读 `workbuddy.db`（**只读连接**，
/// 与读链路 `read_db_sessions` 同纪律）取「目录在场的会话」。
fn wb_probe_session(home: &Path) -> Result<(String, String), String> {
    let sid = pin("MAM_E2E_WB_SESSION");
    let proj = pin("MAM_E2E_PROJECT");
    match (sid, proj) {
        (Some(sid), Some(proj)) => return Ok((sid, proj)),
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "MAM_E2E_WB_SESSION 与 MAM_E2E_PROJECT 必须成对给出（只给一半无法定位会话）"
                    .to_string(),
            )
        }
        (None, None) => {}
    }
    let db = home.join(".workbuddy").join("workbuddy.db");
    if !db.is_file() {
        return Err(format!(
            "workbuddy.db 不在场（{}）——无探针会话可定，或用 MAM_E2E_WB_SESSION / \
             MAM_E2E_PROJECT 钉住",
            db.display()
        ));
    }
    let rows = workbuddy_parser::read_db_sessions(&db)
        .map_err(|e| format!("workbuddy.db 只读查询失败：{e}"))?;
    rows.into_iter()
        .find(|r| !r.id.trim().is_empty() && !r.cwd.trim().is_empty() && Path::new(&r.cwd).is_dir())
        .map(|r| (r.id, r.cwd))
        .ok_or_else(|| {
            "workbuddy.db 的 sessions 表里没有「目录在场的会话」——请在 WorkBuddy 里开一个会话\
             说一句，或用 MAM_E2E_WB_SESSION / MAM_E2E_PROJECT 钉住"
                .to_string()
        })
}

// ============================================================
// 1) zcode：发 → 回执 → 落盘（会话库）核验
// ============================================================

/// zcode 在册会话一个回合：**版本门控 → 真回合 → 库确认**（与 H7 端点同序）。
/// PASS 判据（三件齐）：① 版本门控通过；② 回执 `ok` 且来源不是「未确认」；
/// ③ 会话库里末条 assistant **消息 id 变了**（落盘核验——纯文本比较分不开同文两轮）。
#[tokio::test]
#[ignore = "实机显式跑：zcode 真回合（消耗真实配额；前置=ZCode 安装在场 + 在册会话/环境钉）"]
async fn zcode_send_roundtrip() {
    let case = "zcode_send_roundtrip";
    let os = std::env::consts::OS;
    if os != "windows" && os != "macos" {
        return skip(
            case,
            "zcode 无头通道只有 Windows/macOS 两形态（与端点平台门同口径）",
        );
    }
    let Some(home) = home() else {
        return skip(case, "无法确定用户主目录");
    };
    // ① 安装在场（宿主 pid 传 0：只走常见安装路径，不猜某个进程）
    let Some(spec) = zcode::resolve_spec(&zcode::production_roots(0), os) else {
        return skip(
            case,
            "ZCode 安装路径不可达（未找到 resources/glm/zcode.cjs）——本机未装或装在非常规位置",
        );
    };
    // ② 探针会话
    let (sid, project) = match zcode_probe_session(&home) {
        Ok(v) => v,
        Err(why) => return skip(case, &why),
    };
    let text = payload();
    // ③ 会话串行锁（与端点同规：占位失败 = 该会话已有在飞回合，如实 skip 不排队）
    if !turn::registry().begin(
        &sid,
        zcode::TurnSlot::placeholder("zcode", text.clone())
            .with_channel(HeadlessKind::Zcode.wire_name()),
    ) {
        return skip(case, &format!("会话 {sid} 已有在飞的无头回合（串行锁）"));
    }
    eprintln!(
        "[E2E] {case}：会话={sid}；项目={project}；安装根 cjs={}",
        spec.cjs
    );

    // ④ 版本门控（= 一次最小真实回合，见 gate::PROBE_PROMPT；缓存命中则不重跑）
    let verdict = gate::probe(
        &gate::ProbeSpec::Zcode {
            exe: spec.exe.clone(),
            cjs: spec.cjs.clone(),
        },
        zcode::probe_cache(),
        os,
    )
    .await;
    if let Some(receipt) = gate::version_gate_receipt(&sid, &verdict) {
        turn::registry().end(&sid);
        panic!(
            "{case}：版本门控未通过（拒发）——按端点同规不得盲发；回执={receipt:?}；\
             判定={verdict:?}（缺 provider config 时先查 ZCODE_BUILTIN_PROVIDER_CONFIG_FILE 推导路径）"
        );
    }

    // ⑤ 真回合（回执源基线 = 回合前库快照）
    let before = zcode_parser::store_snapshot_home(&home, &sid);
    let inv = zcode::build_argv(&spec, &probe_text(), &sid, &project, Some(E2E_DEVICE));
    let build = |inv: &zcode::ZcodeInvocation| {
        // 与端点 `runner_from_conn` 的唯一差异：E2E 无 DB 上下文 ⇒ 用底座默认 600s 超时
        // （并发仍是同一份进程级全局名额——`RunnerCfg::new` 自取 `runner::global_sem()`）
        let mut cfg = runner::RunnerCfg::new(&inv.program)
            .args(inv.argv.iter().cloned())
            .cwd(project.clone())
            .session_id(sid.clone())
            .timeout_ms(headless::DEFAULT_TIMEOUT_MS);
        for (k, v) in &inv.env {
            cfg = cfg.env(k.clone(), v.clone());
        }
        cfg
    };
    let deps = zcode::TurnDeps::production(os);
    let seam = turn::production_run_seam();
    let out = zcode::run_turn(&inv, &sid, &project, &build, &deps, &*seam).await;
    turn::registry().end(&sid);
    eprintln!(
        "[E2E] {case}：回执={:?}；来源={:?}；尝试={}；忙终={}",
        out.receipt, out.receipt_source, out.attempts, out.busy_final
    );

    // ⑥ 落盘核验（会话库：末条 assistant 消息 id 变化）
    let after = zcode_parser::store_snapshot_home(&home, &sid);
    let before_id = before.as_ref().and_then(|s| s.last_assistant_id.clone());
    let after_id = after.as_ref().and_then(|s| s.last_assistant_id.clone());

    assert_eq!(
        out.receipt.status,
        ReceiptStatus::Ok,
        "{case}：回执不是 ok（stage={:?}，reason={:?}）——按回执原文核对，不要重发",
        out.receipt.stage,
        out.receipt.reason
    );
    assert!(
        out.receipt_source != zcode::ReceiptSource::Unconfirmed,
        "{case}：回执 ok 但**未确认**（stdout 无 JSON 帧且会话库无新回复）——如实不确认即 FAIL"
    );
    assert!(
        after_id.is_some() && after_id != before_id,
        "{case}：会话库未观测到新的 assistant 回复（落盘核验失败）——before={before_id:?} / \
         after={after_id:?}；snapshot={after:?}"
    );
    pass(case);
}

// ============================================================
// 2) codex：queue 路的消费确认
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
    if !turn::registry().begin(
        &probe.thread,
        codex::TurnSlot::placeholder("codex", text.clone())
            .with_channel(HeadlessKind::CodexQueue.wire_name()),
    ) {
        return skip(
            case,
            &format!("thread {} 已有在飞的无头回合（串行锁）", probe.thread),
        );
    }
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
        // 差异只在超时取默认值（见文件头「唯一已知差异」）
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
    };
    let out = codex::run_turn(&args, &plan, &build, &deps, &*seam).await;
    turn::registry().end(&probe.thread);
    eprintln!(
        "[E2E] {case}：回执={:?}；走向={:?}；改道={}；来源={:?}",
        out.receipt, out.plan_used, out.diverted, out.receipt_source
    );

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
         最常见原因 = 该 thread 未在 APP 打开（Mac 实测可滞留 19.2min，Win 抽验消费 ~55s）",
        out.receipt.reason
    );
    pass(case);
}

// ============================================================
// 3) WorkBuddy：ACP prompt 落盘（转写佐证）
// ============================================================

/// WB ACP 全链：端点发现 → 握手 → `session/load` + `session/prompt` → 落盘佐证。
/// PASS 判据：回执 `ok` 且 `corroboration.confirmed()`（转写 mtime 推进 ∪ 尾签名命中）。
/// 端点不在场（本机 Win 5.7.3 未启用远程控制即此路）按**前置缺席如实 skip**，
/// skip 文案直接引用生产发现链给出的 reason（不另编）。
#[test]
#[ignore = "实机显式跑：WB ACP prompt 真投递（前置=WorkBuddy 已开远程控制端点 + 有活跃/新建探针会话）"]
fn wb_acp_prompt_landed() {
    let case = "wb_acp_prompt_landed";
    let os = std::env::consts::OS;
    if os != "windows" && os != "macos" {
        return skip(
            case,
            "WB ACP 端点发现只有 Windows/macOS 两形态（与端点平台门同口径）",
        );
    }
    let Some(home) = home() else {
        return skip(case, "无法确定用户主目录");
    };
    let (sid, project) = match wb_probe_session(&home) {
        Ok(v) => v,
        Err(why) => return skip(case, &why),
    };
    eprintln!("[E2E] {case}：会话={sid}；项目={project}");
    // 探针依赖与端点同源（真心跳读取 + 真端口指纹；home 缝给真实 home）
    let deps = wb_acp::deps_for(
        os,
        Some(home),
        headless::DEFAULT_TIMEOUT_MS,
        runner::global_sem(),
    );
    let args = wb_acp::WbTurnArgs {
        sid: sid.clone(),
        project,
        payload: payload(),
        // 0 = 无宿主（db 源哨兵卡；与端点对 db 源会话的取值一致）
        pid: 0,
    };
    let out = wb_acp::run_turn(&args, &deps);
    eprintln!(
        "[E2E] {case}：端点={:?}；回执={:?}；判定={:?}；佐证={:?}",
        out.endpoint, out.receipt, out.verdict, out.corroboration
    );
    if out.receipt.stage == Some(Stage::Refused) {
        return skip(
            case,
            &format!(
                "WorkBuddy 端点不可用（生产发现链如实结论，非本测试编造）：{}",
                out.receipt.reason.clone().unwrap_or_default()
            ),
        );
    }
    assert_eq!(
        out.receipt.status,
        ReceiptStatus::Ok,
        "{case}：回执不是 ok（stage={:?}，reason={:?}）——按回执原文核对（已结束会话会报「需复活」）",
        out.receipt.stage,
        out.receipt.reason
    );
    assert!(
        out.corroboration.confirmed(),
        "{case}：回执 ok 但转写未确认落盘（佐证={:?}）——不冒充成功即 FAIL",
        out.corroboration
    );
    pass(case);
}

// ============================================================
// 4) claude：审批双向 round-trip
// ============================================================

/// claude 审批双向桥（H11/C4）：起长驻回合 → 等 `can_use_tool` 上卡 → **批准** →
/// 工具执行 → 回合终结。
/// PASS 判据（四件齐）：① 收到控制请求（`controls >= 1`）；② 批准计数 = 1；
/// ③ 命中 turn 终点判据（`turn_ended`）；④ 回执 `ok` 且 reason 里有工具结果计数
/// （只看「已送达」不足以判 PASS——附录 E-② 明示 `updatedInput` 缺失时工具**永不执行**）。
///
/// **未取证面（Task 13 交底）**：`can_use_tool` 帧在本批从未被真机捕获（该次探针模型自选
/// 不调工具 / 宿主侧 allow 规则可能短路）——审批 wire 的权威是 spec 附录 E-②。故本例若
/// 等不到审批帧，**不是**普通失败：按 Task 15 M10 附则 1 属**形态取证**（宿主 allow 短路）。
/// 断言失败信息里两者都写明，请照抄回报主线。
#[tokio::test]
#[ignore = "实机显式跑：claude 审批卡双向（真回合+真批准；前置=claude CLI 在场 + 可当 cwd 的项目）"]
async fn claude_approval_roundtrip() {
    let case = "claude_approval_roundtrip";
    let os = std::env::consts::OS;
    let Some(shape) = c3::production_cli_spec(c3::CLAUDE_PROGRAM, os) else {
        return skip(case, "claude CLI 不可达（PATH 上未找到 claude）");
    };
    // 工作目录：默认 = 本仓库根（用户自己的项目，claude 会在这里执行「列目录」类只读工具）
    let project = pin("MAM_E2E_PROJECT").unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").to_string())
    });
    if !Path::new(&project).is_dir() {
        return skip(
            case,
            &format!("工作目录不在场：{project}（用 MAM_E2E_PROJECT 指定）"),
        );
    }
    // 会话号：钉住 → `--resume`（在册会话）；缺席 → fresh `--session-id`（自铸）
    let resume = pin("MAM_E2E_CLAUDE_SESSION");
    let sid = resume.clone().unwrap_or_else(fresh_session_id);
    // 正文：**本例默认必须触发工具**（否则回合不弹审批卡，测不到双向桥）——
    // 措辞与 Task 15 M10 同款；`MAM_E2E_TEXT` 可覆盖
    let text = normalize::compose_injection(
        E2E_DEVICE,
        &pin("MAM_E2E_TEXT").unwrap_or_else(|| CLAUDE_TOOL_TEXT.to_string()),
    );
    if !turn::registry().begin(
        &sid,
        c3::placeholder_slot(
            c3::CLAUDE_PROGRAM,
            text.clone(),
            HeadlessKind::ClaudeP.wire_name(),
        ),
    ) {
        return skip(case, &format!("会话 {sid} 已有在飞的无头回合（串行锁）"));
    }
    eprintln!(
        "[E2E] {case}：会话={sid}；形态={}；项目={project}",
        if resume.is_some() {
            "--resume"
        } else {
            "--session-id（fresh）"
        }
    );

    let spec = headless::PermissionSpec::claude_default();
    let argv = c3::claude_argv(
        &text,
        resume.as_deref(),
        resume.is_none().then_some(sid.as_str()),
    );
    let sp = c3::ClaudeSpawn {
        shape,
        argv,
        cwd: project,
    };
    let proc = match c3::spawn_claude(&sp).await {
        Ok(p) => p,
        Err(e) => {
            turn::registry().end(&sid);
            panic!("{case}：claude spawn 失败：{e}");
        }
    };
    let tree = proc.tree.clone();
    let kill = proc.kill_fn();
    let input = c3::ClaudeTurnInput {
        session_id: sid.clone(),
        channel: HeadlessKind::ClaudeP.wire_name(),
        user_frame: c3::claude_user_frame(&text),
        timeout_ms: headless::DEFAULT_TIMEOUT_MS,
        tier: spec.tier(),
        permission_mode: spec.permission_mode().unwrap_or("default"),
        cancel: None,
    };
    let turn_task = tokio::spawn(async move { c3::run_claude_turn(proc.io, input, &kill).await });

    // 等审批帧上卡（轮询进程级待答登记表——与 GET /session-headless-approval 同源）
    let deadline = Instant::now() + APPROVAL_WAIT;
    let mut pending: Option<serde_json::Value> = None;
    while Instant::now() < deadline {
        if let Some(p) = c3::pending_registry().payload_of(&sid) {
            pending = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let Some(p) = pending else {
        // 等不到审批帧：先收摊（不把孤儿 claude 留给用户的机器），再如实报「形态取证」
        let _ = tree.kill();
        let out = tokio::time::timeout(TURN_WAIT, turn_task)
            .await
            .ok()
            .and_then(|r| r.ok());
        turn::registry().end(&sid);
        panic!(
            "{case}：{APPROVAL_WAIT:?} 内未等到 `can_use_tool` 审批帧——**属形态取证，不是普通失败**\
             （Task 15 M10 附则 1：宿主侧 allow 规则可能短路了审批；请查 ~/.claude/settings.json 的 \
             permissions.allow 是否命中该工具）。回合结局={out:?}"
        );
    };
    let request_id = p
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    eprintln!(
        "[E2E] {case}：审批卡载荷={p}（工具={:?}；权限档={:?}）",
        p.get("toolName"),
        p.get("tier")
    );
    assert!(
        !request_id.is_empty(),
        "{case}：待答载荷缺 requestId——不可答的帧不该上卡（附录 E-②）"
    );
    // 批准（= 移动端点「批准」的同一条投递路径：登记表 deliver → 回合写 control_response）
    if let Err(e) = c3::pending_registry().deliver(&sid, &request_id, c3::Decision::Allow) {
        let _ = tree.kill();
        let _ = tokio::time::timeout(TURN_WAIT, turn_task).await;
        turn::registry().end(&sid);
        panic!("{case}：批准未送达：{e}");
    }

    let out = match tokio::time::timeout(TURN_WAIT, turn_task).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            turn::registry().end(&sid);
            panic!("{case}：回合任务异常终止：{e}");
        }
        Err(_) => {
            let _ = tree.kill();
            turn::registry().end(&sid);
            panic!("{case}：回合未在 {TURN_WAIT:?} 内终结——按超时如实上报，勿当成功");
        }
    };
    turn::registry().end(&sid);
    eprintln!(
        "[E2E] {case}：回执={:?}；控制请求={}（批准 {}/拒绝 {}/问答 {}）；命中终点={}；stop_reason={:?}",
        out.receipt, out.controls, out.allowed, out.denied, out.answered, out.turn_ended,
        out.last_stop_reason
    );

    assert!(
        out.controls >= 1,
        "{case}：**未收到任何控制请求**——审批 wire 未取证（附录 E-② 权威，Task 15 M10 附则 1）"
    );
    assert_eq!(
        out.allowed, 1,
        "{case}：批准计数应为 1（实际 {}）",
        out.allowed
    );
    assert!(
        out.turn_ended,
        "{case}：未命中 turn 终点判据（stop_reason={:?}）——回执={:?}",
        out.last_stop_reason, out.receipt
    );
    assert_eq!(
        out.receipt.status,
        ReceiptStatus::Ok,
        "{case}：回执不是 ok（stage={:?}，reason={:?}）",
        out.receipt.stage,
        out.receipt.reason
    );
    let reason = out.receipt.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("工具结果"),
        "{case}：回执 reason 里没有工具结果计数（{reason:?}）——只看到「已送达」不足以判 PASS：\
         附录 E-② 明示 updatedInput 缺失时工具永不执行（静默失败）"
    );
    pass(case);
}
