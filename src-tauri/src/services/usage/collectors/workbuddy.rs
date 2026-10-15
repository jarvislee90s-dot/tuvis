//! WorkBuddy 采集器。实测约束（说明书 §5.1 / §8.1、矩阵 §3.1、`research/可得性探测-SQLite与投影四源.md` §1.3/§2.3/§3.3/§4.3/§5.3——并已在本机 `~/.workbuddy` 的 3 个真实会话 / 266 行上逐项复核）：
//!
//! * **token 在 jsonl 的 `message.usage`**（**不在行顶层**；本机 61 行 = 54 条 `function_call` + 7 条 assistant `message`，其中 59 行带 `cache_read_input_tokens`）。DB 的 `session_usage.used/size` 是**上下文填充量、不是 token 桶**——绝不采信；
//! * **模型必须取 jsonl `providerData.model`**（DB `sessions.model` 会过期：真机 `0758b73e` 的 DB=`hy3`、jsonl 全程 `deepseek-v4.1-flash`）；
//! * **无回合概念**（`traceId` 是「一次用户请求下连续若干次模型请求」的分组，不是回合定义：真机 61 次请求只有 7 个 traceId）→ turns 恒 0，查询层按 `caps.turn = false` 出空态；
//! * **供应商不可得**（DB 全表 + jsonl 全键三路搜索均空）→ 经唯一入口仍得 `("", Unknown)`；
//! * 工具耗时无原生字段 → 行 `timestamp` 配对，**配对键是 `callId`**（真机 71 次调用 69 次可配对）；
//! * 工具失败在 `providerData.toolResult.rawResponse.is_error`（真机 4 条：exitCode 1/56、`tool_error_code` 8002）；
//! * DB 只用两件事：枚举会话 + 取 `sessions.cwd`（精确；目录名编码不可反解）。
//!
//! **用户输入(估)不可得**（`caps.user_est = false`）→ `user_est` 恒为 `None`（落库写 NULL，绝不填 0）。
// （本文件非测试代码不用 `HashMap`——测试模块自带 `use std::collections::HashMap;`，文件级不再导入，否则 `cargo clippy` 报 unused import。）

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file_key_of, file_read_failure_is_benign, DeltaBuilder, PendingTools};
use crate::monitor::workbuddy_parser::find_session_jsonl;
use crate::services::usage::collect::{CollectContext, UsageCollector};
use crate::services::usage::delta::{DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{default_policy, normalize, resolve_semantics, RawUsage};

// **Q-4 / A-1**：workbuddy 的 jsonl 没有自带时区字段；即便将来带了，也**只许读入备查**
// ——键一律走 `hour_key_of_host`（spec §P6 改写后：一律以宿主本地时区为基准）。
pub struct WorkBuddyCollector;

impl UsageCollector for WorkBuddyCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::WorkBuddy
    }
    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        collect_with_home(ctx, ctx.home)
    }
}

/// jsonl 行里用量对象的路径：**真机在 `message.usage`**（本机 61 行 = 54 `function_call`
/// + 7 assistant `message`）——**不在行顶层**。
///
/// ⚠️ 任务书正文写的是行顶层 `v.get("usage")`（`task-15-brief.md:213`），**真机一列都读不到**：
/// 行顶层根本没有 `usage` 键，用量在 `message.usage`（三份表示同时在场的另外两份是
/// `providerData.usage`（驼峰）与 `providerData.rawUsage`（OpenAI 原始形状，其
/// `cache_read_input_tokens` **61/61 恒为 0**，真缓存读在 `message.usage` 里））。
/// 而任务书自己的夹具把 `usage` 放在顶层 → 用例全绿（D-29 同族：夹具绕开真机形态 = 假绿）。
fn usage_of(v: &Value) -> Option<&Value> {
    v.pointer("/message/usage").filter(|u| u.is_object())
}

/// 工具调用的配对键：**`callId` 优先**。
///
/// 真机实测（本机 71 次调用 / 69 次可配对全量核算，样例见探测文档 §2.3）：
/// `function_call` 与 `function_call_result` **唯一相同**的字段是 `callId`；两行的 `id`
/// **各不相干**（result 的 `id` 是自己那一行的 id，call 的 `id` 只出现在 result 的 `parentId` 里）
/// → 任务书正文的 `v.get("id").or_else(|| v.get("callId"))` 在真机上 **69/69 全部配不上**
/// （`tool_ms` 恒 0），而任务书夹具两行**都没有任何 id** → 走 `finish_oldest` → 用例照样绿。
/// 仅当 `callId` 缺失时才退回行 `id`（形状演进兜底）。
fn pair_key(v: &Value) -> Option<&str> {
    v.get("callId")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
        })
}

/// 工具失败判据：真机在 **`providerData.toolResult.rawResponse.is_error`**（4 条）。
/// 说明书 §报错表与矩阵把这一路写成 `providerData.rawResponse.is_error`（省掉 `toolResult`
/// 这一层，真机 `rawResponse` 从不直接挂在 `providerData` 下）——两种形态都认，
/// 先取真机那一层（任务书只认扁平那一种 → 真机 `error_tool` 恒 0，而 caps 声明它可得）。
fn tool_is_error(v: &Value) -> bool {
    [
        "/providerData/toolResult/rawResponse/is_error",
        "/providerData/rawResponse/is_error",
    ]
    .iter()
    .any(|p| v.pointer(p).and_then(|x| x.as_bool()).unwrap_or(false))
}

/// 「模型未知期间」缓存行的上限（**D-32 同款**）：真机（本机 266 行）**0 例**落到缓存
/// （没有模型的 30 行里，产明细行的只有 2 条中断 assistant 行，它们前面已有模型）；
/// 上限只为防「形状演进后无界缓存」。超限即丢 + 计数 + `log::warn!`，**绝不**写 `model=""` 行。
const PENDING_LINES_CAP: usize = 4096;

/// 本行**会不会**产明细行（决定「模型未知」时要不要缓存重放；保守取超集：
/// 缓存多了只是延后处理，缓存漏了就会落 `model=""` 幽灵行）。
fn produces_detail(v: &Value) -> bool {
    if usage_of(v).is_some() {
        return true;
    }
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "function_call" | "function_call_result" => true,
        "message" => v.get("role").and_then(|r| r.as_str()) == Some("assistant"),
        _ => false,
    }
}

/// **单行处理（唯一实现）**：正常路径一行调一次；「模型未知期间」缓存下来的原始行在解析出模型后
/// **按原序重放**时也走这里 —— 两条路径逐字同源，不会长出第二套分流。
///
/// 返回 `false` = **本行会产明细行、但此刻还不知道属于哪个模型**（调用方负责缓存、稍后重放，或收尾时显式丢弃）：
/// * 真机（本机 266 行）有 **30 行**没有 `providerData.model`（17 `file-history-snapshot` + 11 `message` + 2 `ai-title`），其中**产明细行的只有那 2 条**中断 assistant 行 —— 它们排在文件中部、前面已有模型 → 靠 `state.last_model` 继承即可，本机 0 例落到缓存；
/// * 但「**文件首条产明细行就无模型**」是结构性边界：直接落行就是 `model=""` 的**幽灵行**（B2 缺陷③：进明细表与日聚合主键、UI 显空名）→ 按 **D-32** 明文裁决「绝不再写 `model=""` 行」缓存重放（与 kimi 同一套模式）。
///
/// 不产明细行的行（`file-history-snapshot` / `ai-title` / user 行）直接返回 `true`（不进缓存）。
#[allow(clippy::too_many_arguments)]
fn process_line(
    b: &mut DeltaBuilder,
    state: &mut WbState,
    v: &Value,
    session_id: &str,
    cwd: &str,
    now_ms: i64,
) -> bool {
    let ts = v
        .get("timestamp")
        .and_then(|x| x.as_i64())
        .unwrap_or(now_ms);
    let hour = hour_key_of_host(ts);
    let mut model = v
        .pointer("/providerData/model")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        // **模型继承**（与 claude 的 `dominant_model` / codex 的跨轮模型继承同族）：
        // 真机那 2 条中断 assistant 行 `providerData` 里没有任何模型字段（只有 `{skipRun, error, agent}`）
        // → 继承本文件**本轮或上一轮**最近一次见到的模型（经 `state_json` 跨轮）。
        model = state.last_model.clone();
    } else {
        state.last_model = model.clone();
    }
    if model.is_empty() {
        // 模型还不知道 → 交调用方缓存（**唯一**允许「不处理」的出口；绝不落 `model=""` 行）
        return !produces_detail(v);
    }
    // **供应商唯一入口**（GC 6 / B6）：workbuddy 三路搜索都没有供应商字段（实测）
    // → 无 explicit 时经 `provider_of` 仍得 `("", Unknown)`（用户规则命中则 Inferred）。
    // 走唯一入口的价值：三态一定会被记录，将来补上字段/规则时口径自动一致。
    let (provider, _) = b.provider_of(None, &model);
    let key = DetailKey {
        session_id: session_id.to_string(),
        hour_key: hour.clone(),
        day_key: hour.chars().take(10).collect(),
        // 记录级项目键：workbuddy 的归属来自 DB `sessions.cwd`（精确；目录名不可反解）。
        // 真机复核：266/266 行的行内 `cwd` 与 `sessions.cwd` 逐字相等 → 两者取值等价。
        project_key: project_key_of(Some(cwd)),
        model,
        provider,
    };
    // **一次模型请求 = 一份 `message.usage`**（真机 61 份：54 份在 `function_call` 行上、
    // 7 份在 assistant `message` 行上）→ 用量读取必须放在类型分派**之前**：
    // 只读 assistant 分支会漏掉 54/61 = 88.5%（任务书写法）。
    if let Some(u) = usage_of(v) {
        let raw = RawUsage {
            input_raw: u.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
            cache_read: u
                .get("cache_read_input_tokens")
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
            // 真机 `message.usage` 里没有 cache_creation 字段（缺失 = 0，不是「不可得」：
            // 四桶口径按 0 处理；真机的缓存写量为 0）
            cache_write: u
                .get("cache_creation_input_tokens")
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
            output: u.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
            reasoning: 0,
            // total_tokens 在场 → 逐条判定（判据优先，Global Constraints 14）
            total_raw: u.get("total_tokens").and_then(|x| x.as_i64()),
        };
        let sem = resolve_semantics(default_policy(UsageSourceId::WorkBuddy), &raw);
        if let Some(n) = normalize(&raw, sem) {
            let d = b.detail(key.clone(), ts);
            d.buckets.add(&n.buckets);
            d.request_total += n.request_total;
            d.requests += 1;
            d.cache_semantics = sem;
        }
    }
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        // assistant 文本行：**turns 恒 0**（无回合概念，D16 / caps.turn = false，
        // 不得在这里数 user/assistant 行）；**user_est 恒 None**（读不到用户文本，
        // caps.user_est = false → 落库写 NULL，GC 7 / GC 18-R1）。
        // 中断行（真机 2 条）：`status == "incomplete"` 或正文含 "Interrupted by user"
        // （真机两条**同时**满足；正文那一路是防退化的第二判据）。
        "message" if v.get("role").and_then(|r| r.as_str()) == Some("assistant") => {
            let text = v
                .pointer("/content")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if v.get("status").and_then(|s| s.as_str()) == Some("incomplete")
                || text.contains("Interrupted by user")
            {
                b.detail(key.clone(), ts).counters.interrupted += 1;
            }
        }
        "function_call" => {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("unknown");
            let d = b.detail(key.clone(), ts);
            d.counters.tool_calls += 1;
            d.counters
                .tool_stats
                .entry(name.to_string())
                .or_insert((0, 0))
                .0 += 1;
            if let Some(call_id) = pair_key(v) {
                state.tools.start(call_id, ts, name);
            }
        }
        "function_call_result" => {
            let call_id = pair_key(v);
            // 没有配对键时按「**最早**一个未配对调用」收尾（真机 71 次全部带 `callId`，
            // 这条只是形状演进的兜底；`PendingTools::finish_oldest` 按时间戳取最早）
            let finished = match call_id {
                Some(k) => state.tools.finish(k, ts),
                None => state.tools.finish_oldest(ts),
            };
            if let Some((dur, name)) = finished {
                let d = b.detail(key.clone(), ts);
                d.counters.tool_ms += dur;
                d.counters.tool_stats.entry(name).or_insert((0, 0)).1 += dur;
            }
            if tool_is_error(v) {
                b.detail(key, ts).counters.error_tool += 1;
            }
        }
        _ => {}
    }
    true
}

/// 把「模型未知期间」缓存的行**按原序重放**，返回**重放后仍无法归属**的行数。
///
/// 顺序天然正确：缓存行都排在**首个带模型的行**之前，重放发生在它之后；工具状态
/// （`state.tools`）也随之按原序推进 → 跨缓存边界的「调用在缓存里、回执在缓存外」照样配对。
/// **返回值不许吞**（D-32 复审 Low #2）：真机 0 例，但缓存里若混进仍无模型的行，调用方计数并 warn。
fn replay_pending(
    pending: &mut Vec<String>,
    b: &mut DeltaBuilder,
    state: &mut WbState,
    session_id: &str,
    cwd: &str,
    now_ms: i64,
) -> i64 {
    let mut unattributed = 0i64;
    for line in pending.drain(..) {
        // 进缓存的行一定在主循环解析成功过 → 这里的 Err 理论上不可达（真发生即计数，不静默）
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => {
                if !process_line(b, state, &v, session_id, cwd, now_ms) {
                    unattributed += 1;
                }
            }
            Err(_) => unattributed += 1,
        }
    }
    unattributed
}

/// 重放后仍无法归属的行 → **不静默**（真机 0 条；见 `replay_pending` 的文档）
fn warn_unattributed(jsonl: &std::path::Path, n: i64) {
    if n > 0 {
        log::warn!(
            "usage/workbuddy: {} 重放后有 {n} 行仍无法归属模型（这些记录不入账）",
            jsonl.display()
        );
    }
}

/// home 注入版（单测直调）
pub fn collect_with_home(
    ctx: &CollectContext<'_>,
    home: &std::path::Path,
) -> Result<SourceDelta, UsageError> {
    let db = home.join(".workbuddy").join("workbuddy.db");
    let Some(conn) = crate::monitor::sqlite::open_usage_db(&db) else {
        // ⚠️ `open_usage_db` 把三种情况都合并成 `None`（`monitor/sqlite.rs:47-51`）：
        // ① 文件不存在（未安装）；② `CANTOPEN` / `NOTADB` / 权限；③ `BUSY` 且 `immutable` 也失败。
        // **只有 ① 才是「未安装」**；「已安装、库却在，只是打不开」= 这个源读不了，必须整源响亮失败
        // （W-06 / D-24 同族）——否则 UI 上表现为「WorkBuddy 没有用量」，而且此后每轮都是 0。
        // （Task 14 在 `sqlite.rs:56-61` 有意选择的「BUSY 时静默降级到 immutable」那条取舍**不动**：
        //  它管的是「能打开」，这里管的是「两者都打不开」。）
        if db.exists() {
            return Err(UsageError::new(
                "usage-source-io",
                format!("open {} failed (db exists but unreadable)", db.display()),
            ));
        }
        return Ok(SourceDelta::default()); // 未安装 WorkBuddy：零产物不算失败
    };
    let sessions = load_sessions(&conn, &db)?;
    // **供应商规则从 ctx 注入**（GC 21 / §3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
    // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
    let rules = ctx.provider_rules();
    let mut b = DeltaBuilder::new(UsageSourceId::WorkBuddy, rules);
    let root = home.join(".workbuddy");
    for (id, cwd, title) in sessions {
        let Some(jsonl) = find_session_jsonl(home, &cwd, &id) else {
            continue;
        };
        // 游标键 = **相对 `~/.workbuddy` 的路径**（B5：唯一性由路径给，不由文件名给）
        let file_key = file_key_of(&root, &jsonl);
        let read = match ctx.read_incremental(&jsonl, &file_key) {
            Ok(r) => r,
            Err(e) => {
                // **W-06 / D-24**：只有「枚举与读取之间的轮转竞态」（文件已消失）才允许跳过；
                // 其余读失败（权限 / IO / mtime 不可得）必须整源响亮失败 → `UsageSourceStatus.errorCode`，
                // 不得只 `log::warn!` 然后静默少算。
                if file_read_failure_is_benign(&jsonl, &e) {
                    log::warn!(
                        "usage/workbuddy: 读取 {} 失败（文件已消失，跳过本轮）: {}",
                        jsonl.display(),
                        e
                    );
                    continue;
                }
                return Err(UsageError::new(
                    "usage-source-io",
                    format!("read {} failed: {e}", jsonl.display()),
                ));
            }
        };
        if read.lines.is_empty() && !read.rescan {
            // 未变文件（L2 快速路）：游标原样推回，**不计 parsed_files**（W-50：本字段的口径是
            // 「本轮**真读到**的文件数」——与 claude / codex / kimi 一致，未变更文件在读之前就退出了）
            b.push_cursor(read.next);
            continue;
        }
        let mut state: WbState = if read.rescan {
            // 截断/重写：续读状态随文件代际一起重置（旧状态里的未配对调用指向的行已经不在了）
            WbState::default()
        } else {
            // **游标必须被读**：增量读从文件中段开始时，上一轮发起的工具调用还在等回执，
            // 只写不读 → 第二轮的 `open` 是空表 → 跨轮回执永远配不上（静默少算）。
            // **解析失败不得静默重置**（形状演进时旧游标会连 `tools` / `last_model` 一起丢：
            // 表现为 `tool_ms` 少算 + 模型继承失效，且没有任何信号）→ warn + 按 rescan 语义处理
            // （重置本身语义等价，但它现在**可见**了）。
            match ctx.cursor_of(&file_key) {
                Some(p) => match serde_json::from_str::<WbState>(&p.state_json) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!(
                            "usage/workbuddy: {} 的游标状态反序列化失败（按 rescan 语义重置该文件续读状态）: {e}（state_json={:?}）",
                            jsonl.display(),
                            p.state_json
                        );
                        WbState::default()
                    }
                },
                None => WbState::default(),
            }
        };
        // 「模型未知期间」的原始行（**按行缓存**，见 `process_line` 的文档；D-32 同款）
        let mut pending: Vec<String> = Vec::new();
        let mut dropped_no_model: i64 = 0;
        // ---- 一遍扫描：四桶 + 工具 + 报错 全在这一轮里 ----
        for line in &read.lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue; // 坏行单行丢弃，不影响同文件其余行
            };
            b.new_records += 1;
            // 模型已就绪 → 先把缓存行按**原序**重放（它们都排在本行之前）
            if !pending.is_empty() && !state.last_model.is_empty() {
                let left = replay_pending(&mut pending, &mut b, &mut state, &id, &cwd, ctx.now_ms);
                warn_unattributed(&jsonl, left);
            }
            if !process_line(&mut b, &mut state, &v, &id, &cwd, ctx.now_ms) {
                if pending.len() < PENDING_LINES_CAP {
                    pending.push(line.clone());
                } else {
                    if dropped_no_model == 0 {
                        log::warn!(
                            "usage/workbuddy: {} 的「模型未知」缓存超上限 {PENDING_LINES_CAP} 行，\
                             超出部分丢弃（这些记录不入账）",
                            jsonl.display()
                        );
                    }
                    dropped_no_model += 1;
                }
            }
        }
        // 收尾结算缓存行（**D-32 同款**，顺序不许颠倒：本文件解析出的模型 → 丢弃）：
        // `state.last_model` 此刻可能已非空 —— 若本文件首条（也是唯一一条）带模型的行恰好落在
        // **最后一行**，缓存就没机会在行循环里 inline 重放；漏掉这一步会把整段记录静默丢掉。
        if !pending.is_empty() {
            if state.last_model.is_empty() {
                log::warn!(
                    "usage/workbuddy: {} 有 {} 行产明细行的记录出现在任何带模型的行之前，且**整轮**\
                     都拿不到模型 → 丢弃（不写 model=\"\" 行；B2 缺陷③ / D-32 同族）",
                    jsonl.display(),
                    pending.len()
                );
                state.dropped_no_model += pending.len() as i64;
                pending.clear();
            } else {
                let left = replay_pending(&mut pending, &mut b, &mut state, &id, &cwd, ctx.now_ms);
                warn_unattributed(&jsonl, left);
            }
        }
        if dropped_no_model > 0 {
            // 丢弃计数要有**可观测面**（不能只用来抑制重复 warn）：仓内无日志捕获设施（W-37），
            // 故同时落进 `state_json`（有界、可断言、跨轮累计）。
            state.dropped_no_model += dropped_no_model;
            log::warn!(
                "usage/workbuddy: {} 本轮因「模型未知」缓存超上限共丢弃 {dropped_no_model} 行（累计 {}）",
                jsonl.display(),
                state.dropped_no_model
            );
        }
        // **W-13 调用纪律**：`b.session(...)` 每会话**只调一次**（就在此处，循环之外）→
        // `DeltaBuilder::session` 里那次 `record_project_for`（内含 `canonicalize`，本域唯一的
        // 非纯函数）也就每会话一次；逐记录一律只走**零 FS** 的 `project_key_of(cwd)`（上面建 key 处）。
        // 机械锁在共用层：`collectors::tests::record_project_is_called_once_per_session_not_per_record`。
        // **本调用点拿不到独立的机械锁**（已用变异实证、如实登记）：`RECORD_PROJECT_CALLS` 只在
        // `record_project_for` 里自增，采集器即使**逐记录直连** `project::record_project` 也数不着
        // （mod.rs 的 W-13 注记已声明这半是「部分覆盖」），所以此处只做读码级核对——
        // 评审请核这一行是否仍在循环外（真机 266 行，挂进循环就是 266 次 syscall）。
        b.session(&id, Some(&cwd), title, false, None, None, ctx.now_ms);
        // **W-50 口径**：`parsed_files` = 本轮**真读到**的文件数（走到了这里 = 真的读了）。
        // 循环开头的 `find_session_jsonl(...) else { continue }`（没有 jsonl 的会话）与
        // `read.lines.is_empty() && !read.rescan`（未变更文件）都在计数**之前**退出 →
        // 未变更文件的第二轮计 0，与 claude / codex / kimi 同口径（不得改成逐会话累加）。
        b.parsed_files += 1;
        let mut next = read.next;
        // 序列化失败**不得静默**：写空的 `state_json` 会让下一轮反序列化失败并重置续读状态
        // （跨轮未配对调用与模型继承一起丢）→ warn（下一轮的 warn 会再报一次，两处都可见）
        next.state_json = match serde_json::to_string(&state) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "usage/workbuddy: {} 的游标状态序列化失败（下一轮会按 rescan 语义重置该文件续读状态）: {e}",
                    jsonl.display()
                );
                String::new()
            }
        };
        b.push_cursor(next);
    }
    Ok(b.finish())
}

/// 会话枚举。**失败必须整源响亮失败**（评审裁决 A / plan-mandated）：`open_usage_db` 的探针
/// 只摸一句 `SELECT count(*) FROM sqlite_master`（`monitor/sqlite.rs:62-67`），**不碰 `sessions` 表**
/// → 「已安装、库能打开，但 `sessions` / `custom_title` / `deleted_at` 任一处不存在」（版本漂移 /
/// 库损坏）以前会 `return Vec::new()` → 0 会话 → `ok=true` + `parsed_files=0` + 无 `errorCode`
/// = UI 上的「WorkBuddy 没有用量」，而且**此后每轮都是 0**（D-35 同族：把失败伪装成合法值）。
///
/// 行级错误同样**不得静默丢行**（旧实现 `flatten()` 把它吞了）：逐条 `log::warn!` + 计数，
/// 然后整源 `Err` —— `SourceDelta` 形状是冻结的、没有「丢了几行」的字段，`Err` 是唯一**可见**的通道。
fn load_sessions(
    conn: &Connection,
    db: &std::path::Path,
) -> Result<Vec<(String, String, Option<String>)>, UsageError> {
    let io_err = |e: &dyn std::fmt::Display| {
        UsageError::new(
            "usage-source-io",
            format!("read sessions from {} failed: {e}", db.display()),
        )
    };
    let mut stmt = conn
        .prepare(
            "SELECT id, cwd, COALESCE(NULLIF(custom_title,''), title) FROM sessions WHERE deleted_at IS NULL",
        )
        .map_err(|e| io_err(&e))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| io_err(&e))?;
    let mut out = Vec::new();
    let mut row_errs = 0usize;
    let mut first_err = String::new();
    for row in rows {
        match row {
            Ok(t) => out.push(t),
            Err(e) => {
                row_errs += 1;
                if first_err.is_empty() {
                    first_err = e.to_string();
                }
                log::warn!(
                    "usage/workbuddy: 读取 {} 的会话行失败（第 {} 行出错）: {e}",
                    db.display(),
                    out.len() + row_errs
                );
            }
        }
    }
    if row_errs > 0 {
        return Err(io_err(&format!("{row_errs} 行出错，首条：{first_err}")));
    }
    Ok(out)
}

/// 续读状态（进游标 `state_json`）：跨轮保住未配对工具调用（增量读从文件中段开始时，上一轮发起的调用还在等回执）；
/// **模型继承**的落点（`last_model`：真机那 2 条无模型字段的中断行靠它避免 `model=""` 幽灵行）；
/// 「模型未知」丢弃计数（`dropped_no_model`：D-32 的「丢弃 + 计数」要有可断言面）。
/// `#[serde(default)]` 让缺这些字段的旧 `state_json` 仍可反序列化，不丢 `tools`。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct WbState {
    tools: PendingTools,
    #[serde(default)]
    last_model: String,
    #[serde(default)]
    dropped_no_model: i64,
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::caps::caps_of;
    use crate::services::usage::collect::{CollectContext, CollectStats};
    use crate::services::usage::delta::{CursorDelta, SourceDelta};
    // D-01：`use super::*;` 只带进**本模块自己** `use` 进来的名字与本地 item，
    // 不会穿透到 `services/usage/mod.rs` 的 `pub use model::*;` → 用到的值对象显式补。
    use crate::services::usage::model::{SourceKind, UsageSourceId};
    use crate::services::usage::provider::ProviderRule;
    use crate::services::usage::semantics::CacheSemantics;
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::HashMap;

    /// 建 `~/.workbuddy/workbuddy.db`（列 = 真机 `sessions` 表的用到的子集：
    /// `custom_title` / `deleted_at` 真机存在且为 NULL，`model` 一律写 `hy3`
    /// ——真机 DB 的模型会过期，夹具里用它证明采集器**不采信** DB 的模型）。
    fn make_db(home: &std::path::Path, sessions: &[(&str, &str)]) {
        let db = home.join(".workbuddy/workbuddy.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, custom_title TEXT,
                 model TEXT, deleted_at TEXT);",
        )
        .unwrap();
        for (id, cwd) in sessions {
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, model) VALUES (?1, ?2, '标题', 'hy3')",
                rusqlite::params![id, cwd],
            )
            .unwrap();
        }
    }

    /// 写一个会话 jsonl（真机路径形态：`~/.workbuddy/projects/<编码 cwd>/<sessionId>.jsonl`）。
    fn write_jsonl(
        home: &std::path::Path,
        project_dir: &str,
        session_id: &str,
        lines: &[serde_json::Value],
    ) {
        let proj = home.join(".workbuddy/projects").join(project_dir);
        std::fs::create_dir_all(&proj).unwrap();
        let body: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect();
        std::fs::write(proj.join(format!("{session_id}.jsonl")), body).unwrap();
    }

    /// 追加一行（跨轮增量用；真机是只追加的工具日志）。
    fn append_jsonl(
        home: &std::path::Path,
        project_dir: &str,
        session_id: &str,
        line: &serde_json::Value,
    ) {
        use std::io::Write;
        let p = home
            .join(".workbuddy/projects")
            .join(project_dir)
            .join(format!("{session_id}.jsonl"));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }

    /// 采集一轮（home 注入版）
    fn run(
        home: &std::path::Path,
        cursors: &HashMap<String, CursorDelta>,
    ) -> (SourceDelta, CollectStats) {
        let ctx = CollectContext::new(home, 1_700_000_000_000, cursors);
        let delta = collect_with_home(&ctx, home).unwrap();
        (delta, ctx.stats())
    }

    /// 真机同形的 fixture（逐字依据：`research/可得性探测-SQLite与投影四源.md` §2.3 / §3.3 / §5.3）：
    /// * 用量在 **`message.usage`**（不在行顶层；本机 61 行 = 54 条 `function_call` + 7 条 assistant `message`）；
    /// * 工具配对靠 **`callId`**（两行的 `id` 各不相干：result 的 `id` 是自己那一行的 id，
    ///   call 的 `id` 只出现在 result 的 `parentId` 里）；
    /// * 工具失败在 **`providerData.toolResult.rawResponse.is_error`**（真机 4 条）；
    /// * 中断行 = `status:"incomplete"` + 正文 "Interrupted by user"（真机 2 条两个判据同时在场）；
    /// * 同一行的三份用量表示（`message.usage` / `providerData.usage` / `providerData.rawUsage`）
    ///   真机同时在场且数值一致，其中 `rawUsage.cache_read_input_tokens` **61/61 恒为 0**
    ///   （真缓存读在 `message.usage.cache_read_input_tokens`）；
    /// * DB 的 `sessions.model` 会过期（真机 DB=`hy3`，jsonl 全程 `deepseek-v4.1-flash`）。
    fn fixture(home: &std::path::Path) {
        make_db(home, &[("s1", "/p/A")]);
        write_jsonl(
            home,
            "p-A",
            "s1",
            &[
                json!({"type":"message","role":"user","timestamp":1000,"cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"input_text","text":"你好 world"}]}),
                json!({"type":"message","role":"assistant","status":"completed","timestamp":1100,
                       "cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"output_text","text":"ok"}],
                       "message":{"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
                                           "cache_read_input_tokens":80}},
                       "providerData":{"model":"deepseek-v4.1-flash","requestModelId":"deepseek-v4.1-flash",
                           "usage":{"requests":1,"inputTokens":100,"outputTokens":20,"totalTokens":120,
                                    "inputTokensDetails":[{"cached_tokens":80}],
                                    "outputTokensDetails":[{"reasoning_tokens":0}]},
                           "rawUsage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                                       "cache_read_input_tokens":0,"prompt_cache_hit_tokens":80}}}),
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"row-call-1",
                       "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
                json!({"type":"function_call_result","id":"row-res-1","callId":"call_1","timestamp":1700,
                       "cwd":"/p/A","sessionId":"s1",
                       "providerData":{"model":"deepseek-v4.1-flash",
                           "toolResult":{"rawResponse":{"is_error":true,"exitCode":1,
                                                       "tool_error_code":"8002"}}}}),
                json!({"type":"message","role":"assistant","status":"incomplete","timestamp":1800,
                       "cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"output_text","text":"Interrupted by user"}],
                       "providerData":{"model":"deepseek-v4.1-flash",
                                       "error":{"message":"Interrupted by user"}}}),
            ],
        );
    }

    #[test]
    fn model_comes_from_jsonl_and_turns_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let (delta, stats) = run(dir.path(), &no_cursors);
        assert_eq!(stats.repeat_reads, 0);
        // W-15：只断言「值对了」不等于「路径对了」——把 reads / bytes_read 也钉住。
        assert_eq!(
            stats.reads, 1,
            "同一文件一轮只读一遍（GC 4：一次扫描产出全部指标）"
        );
        assert!(stats.bytes_read > 0, "首读必须是真读（不是快速路）");
        // 模型取 jsonl providerData.model（DB 的 hy3 不得出现）
        assert!(delta
            .details
            .iter()
            .all(|d| d.key.model == "deepseek-v4.1-flash"));
        // 供应商不可得（三路搜索全空）→ 空串 + 未知态（由 provider 层判定，这里断言空串 + 三态）
        assert!(delta.details.iter().all(|d| d.key.provider.is_empty()));
        assert!(delta
            .details
            .iter()
            .all(|d| d.provider_kind == SourceKind::Unknown));
        // 前提断言（防假绿）：同一小时/模型/供应商/项目 → 只有一行明细，下面取下标 0 才有意义
        assert_eq!(delta.details.len(), 1, "夹具全部落在同一小时桶 → 一行明细");
        // 四桶：声明 Exclusive + total 判据在场（total == input + output → Subset 改判）
        let d = &delta.details[0];
        assert_eq!(d.buckets.input_fresh, 20, "Subset：未缓存 = 100 - 80");
        assert_eq!(d.buckets.cache_read, 80);
        assert_eq!(d.buckets.output, 20);
        assert_eq!(d.request_total, 100);
        assert_eq!(
            d.requests, 1,
            "一行 usage = 一次请求（同一行的三份用量表示不得各算一次）"
        );
        assert_eq!(
            d.cache_semantics,
            CacheSemantics::Subset,
            "GC 14：workbuddy 判据在场 → 逐条判定"
        );
        // 无回合概念（D16）：turns 恒为 0，caps 为 false → 查询层给空态
        assert_eq!(
            delta.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            0
        );
        assert!(!caps_of(UsageSourceId::WorkBuddy).turn);
        // 工具：时间戳配对 69/71（此处 1/1）+ 工具失败来自 rawResponse.is_error
        assert_eq!(d.counters.tool_calls, 1);
        assert_eq!(d.counters.tool_ms, 500);
        assert_eq!(d.counters.error_tool, 1);
        assert_eq!(d.counters.tool_stats.get("shell"), Some(&(1, 500)));
        // 中断行单列（不计入错误）
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.interrupted)
                .sum::<i64>(),
            1
        );
        // D21：cwd 来自 DB（精确；目录名编码不可反解）
        assert_eq!(delta.sessions[0].project_key, "a");
        assert_eq!(delta.sessions[0].project_path_raw, "/p/A");
        // **R1 / GC 7**：workbuddy 读不到用户文本 → user_est 必须 NULL（不得填 0）
        assert!(delta.details.iter().all(|d| d.user_est.is_none()));
        // **W-50**：parsed_files = 本轮真读到的文件数（不是会话数）
        assert_eq!(delta.parsed_files, 1);
        assert_eq!(delta.new_records, 5, "5 行全部解析");
    }

    /// **真机主形态（本机 54/61 = 88.5%）**：用量挂在 **`function_call`** 行的 `message.usage` 上
    /// （assistant `message` 行只有 7/61）。任务书把它读在 `type=="message" && role=="assistant"`
    /// 分支里 → 真机漏 88.5%；而任务书夹具把用量放在 message 行 → 用例全绿（D-29 同族假绿）。
    ///
    /// 两个样本逐字取自矩阵 §5.3 的**真机边界样本**：一条不带 `cache_read_input_tokens`
    /// （判据只剩 total → 按互斥式判定）、一条带（→ Subset）。
    #[test]
    fn usage_on_function_call_rows_is_counted_with_the_real_boundary_samples() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"function_call","name":"Bash","timestamp":1789570300586i64,
                       "id":"row-call-1","callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "message":{"usage":{"input_tokens":35565,"output_tokens":205,"total_tokens":35770}},
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
                json!({"type":"function_call","name":"Read","timestamp":1789570305318i64,
                       "id":"row-call-2","callId":"call_2","cwd":"/p/A","sessionId":"s1",
                       "message":{"usage":{"input_tokens":34355,"output_tokens":299,"total_tokens":34654,
                                           "cache_read_input_tokens":7360}},
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
                // 中断判据的**第二条**（正文文本）：真机两条两个判据同时在场，
                // 这里单列文本一路，免得「只认 status」的退化拿不到红。
                json!({"type":"message","role":"assistant","timestamp":1789570306000i64,
                       "cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"output_text","text":"Interrupted by user"}],
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (delta, stats) = run(dir.path(), &no_cursors);
        assert_eq!(stats.repeat_reads, 0);
        assert_eq!(
            delta.details.iter().map(|d| d.requests).sum::<i64>(),
            2,
            "两条 function_call 行各带一份 usage = 两次模型请求（只在 assistant 分支读 → 0）"
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.buckets.input_fresh)
                .sum::<i64>(),
            62560,
            "35565 + (34355 - 7360)"
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.buckets.cache_read)
                .sum::<i64>(),
            7360
        );
        assert_eq!(
            delta.details.iter().map(|d| d.buckets.output).sum::<i64>(),
            504
        );
        assert_eq!(
            delta.details.iter().map(|d| d.request_total).sum::<i64>(),
            69920,
            "35565 + 34355"
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            2
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.interrupted)
                .sum::<i64>(),
            1,
            "只有正文、没有 status 的那一条也必须算出来"
        );
        assert_eq!(delta.parsed_files, 1);
        assert_eq!(delta.new_records, 3);
    }

    /// **W-50 口径锁**（控制窗口已复核语义，这里把它钉死）：`parsed_files` 记的是
    /// **「本轮真读到的文件数」**（与 claude / codex / kimi 一致），不是会话数、
    /// 也不是「枚举到的文件数」：
    /// * 没有 jsonl 的会话（`find_session_jsonl` 回 None）在读之前就 `continue` → 不计；
    /// * **未变更文件**（快速路）也在计数之前 `continue` → 第二轮不得计。
    #[test]
    fn parsed_files_counts_files_actually_read_and_unchanged_files_are_not_reread() {
        let dir = tempfile::tempdir().unwrap();
        // s2 在 DB 里但**没有** jsonl（会话被清理 / 未落盘）→ 不得计入
        make_db(
            dir.path(),
            &[("s1", "/p/A"), ("s2", "/p/B"), ("s3", "/p/C")],
        );
        for (sid, proj, cwd) in [("s1", "p-A", "/p/A"), ("s3", "p-C", "/p/C")] {
            write_jsonl(
                dir.path(),
                proj,
                sid,
                &[json!({"type":"message","role":"assistant","timestamp":1000,
                        "cwd":cwd,"sessionId":sid,
                        "content":[{"type":"output_text","text":"ok"}],
                        "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                        "providerData":{"model":"hy4-preview"}})],
            );
        }
        let no_cursors = HashMap::new();
        let (d1, s1) = run(dir.path(), &no_cursors);
        assert_eq!(
            d1.parsed_files, 2,
            "只有两个会话有 jsonl（第三个连读都没读）"
        );
        assert_eq!(d1.cursors.len(), 2);
        assert_eq!(s1.reads, 2, "两个文件各读一次");
        assert_eq!(d1.sessions.len(), 2, "无 jsonl 的会话不产会话维度行");

        // 第二轮：两个文件都没变 → 快速路 → 零新增、零字节；**parsed_files 必须为 0**
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, s2) = run(dir.path(), &cursors);
        assert_eq!(
            d2.parsed_files, 0,
            "未变更文件不得算进「本轮真读到的文件数」（W-50：与 claude/codex/kimi 同口径）"
        );
        assert_eq!(d2.new_records, 0);
        assert!(d2.details.is_empty(), "不得重复计入");
        assert_eq!(s2.repeat_reads, 0);
        assert_eq!(
            s2.reads, 2,
            "仍各进一次 read_incremental（只是走 L2 快速路）"
        );
        assert_eq!(
            s2.bytes_read, 0,
            "未变文件必须零字节读取（W-15：把 bytes_read 也纳入断言）"
        );
    }

    /// **游标必须被「读」参与增量**（不是只写）：未配对工具调用的状态存在 `state_json` 里，
    /// 第二轮必须经 `ctx.cursor_of(&file_key)` 反序列化回 `WbState` 才能把回执配上。
    /// 只写不读 → 第二轮的 `open` 是空的 → `tool_ms` 恒 0（静默少算）。
    #[test]
    fn cursor_state_carries_unpaired_tool_calls_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"row-call-1",
                    "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert_eq!(d1.cursors.len(), 1, "前提：R1 必须产出游标");
        let c1 = &d1.cursors[0];
        assert!(
            c1.state_json.contains("call_1"),
            "前提：未配对调用必须进 state_json，否则 R2 的配对无从谈起：{}",
            c1.state_json
        );
        assert_eq!(
            d1.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            0,
            "R1 没有回执行"
        );

        // 第二轮：追加它的回执行（真机形态：result 带自己的 id + 与 call 相同的 callId）
        append_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &json!({"type":"function_call_result","id":"row-res-1","callId":"call_1",
                    "timestamp":1700,"cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = run(dir.path(), &cursors);
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            0,
            "本轮没有新的 function_call"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            500,
            "跨轮配对：WbState 必须从 ctx.cursor_of(file_key) 读回来（只写不读 → open 为空 → 恒 0）"
        );
        assert_eq!(
            d2.details
                .iter()
                .find_map(|d| d.counters.tool_stats.get("shell").copied()),
            Some((0, 500)),
            "跨轮配对在单轮视角是 (0, ms)（W-39：落库累加后才是 (1, ms)）"
        );
    }

    /// 未安装 WorkBuddy（没有 `workbuddy.db`）→ 零产物**不算失败**（降级路径）。
    #[test]
    fn missing_workbuddy_db_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let no_cursors = HashMap::new();
        let (delta, stats) = run(dir.path(), &no_cursors);
        assert!(delta.details.is_empty());
        assert!(delta.sessions.is_empty());
        assert!(delta.cursors.is_empty());
        assert_eq!(delta.parsed_files, 0);
        assert_eq!(stats.reads, 0, "未安装 → 连文件都不该枚举/读");
    }

    /// 坏 JSON 行（截断 / 半行 / 非法 UTF-8 都可能）→ **单行丢弃**，不得让整文件或整源失败。
    #[test]
    fn malformed_json_line_is_skipped_without_losing_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        let proj = dir.path().join(".workbuddy/projects/p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let body = format!(
            "{}\n{{not json\n{}\n",
            serde_json::to_string(
                &json!({"type":"message","role":"assistant","timestamp":1000,
                    "cwd":"/p/A","sessionId":"s1",
                    "content":[{"type":"output_text","text":"ok"}],
                    "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                    "providerData":{"model":"deepseek-v4.1-flash"}})
            )
            .unwrap(),
            serde_json::to_string(
                &json!({"type":"function_call","name":"shell","timestamp":1200,
                    "id":"row-call-1","callId":"call_1","cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}})
            )
            .unwrap(),
        );
        std::fs::write(proj.join("s1.jsonl"), body).unwrap();
        let no_cursors = HashMap::new();
        let (delta, _) = run(dir.path(), &no_cursors);
        assert_eq!(
            delta.new_records, 2,
            "坏行不计入 new_records，但**后面的好行必须照常处理**"
        );
        assert_eq!(delta.details.iter().map(|d| d.requests).sum::<i64>(), 1);
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            1
        );
        assert_eq!(delta.parsed_files, 1);
    }

    /// **W-06 / D-24**：只有「枚举与读取之间的轮转竞态」（文件已消失）才允许跳过；
    /// 其余读失败（权限 / IO / mtime 不可得 / 根本不是文件）必须**整源响亮失败**
    /// → `usage-source-io` 落进 `UsageSourceStatus.errorCode`，不得只 `log::warn!` 后静默少算。
    ///
    /// 构造：把 `<id>.jsonl` 做成**目录**（`find_session_jsonl` 的 `exists()` 会命中，
    /// 而 `fs::read` 必然失败）——跨平台、不依赖 chmod（D-35 的教训）。
    #[test]
    fn unreadable_session_file_fails_the_source_loudly() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A"), ("s2", "/p/B")]);
        write_jsonl(
            dir.path(),
            "p-B",
            "s2",
            &[
                json!({"type":"message","role":"assistant","timestamp":1000,"cwd":"/p/B",
                    "sessionId":"s2","content":[{"type":"output_text","text":"ok"}],
                    "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        std::fs::create_dir_all(dir.path().join(".workbuddy/projects/p-A/s1.jsonl")).unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = collect_with_home(&ctx, dir.path())
            .expect_err("读不了的文件必须整源报错，不得静默跳过（W-06 / D-24）");
        assert_eq!(err.code, "usage-source-io");
        assert!(
            err.detail.contains("s1.jsonl"),
            "错误里要能定位到文件（W-28 同族）：{}",
            err.detail
        );
    }

    /// 截断 / 重写 → `rescan` → 本文件的**续读状态必须重置**（旧状态指向的行已经不在了）。
    /// 沿用旧状态的后果：新文件里一条「无配对键的回执行」会配到**上一代文件**的调用上，
    /// 凭空多出一个耗时（错算）。
    #[test]
    fn truncated_rewrite_resets_the_pending_tool_state() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"message","role":"user","timestamp":1000,"cwd":"/p/A",
                       "sessionId":"s1","content":[{"type":"input_text","text":"hi"}]}),
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"row-call-1",
                       "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        let c1 = &d1.cursors[0];
        assert!(
            c1.state_json.contains("call_1"),
            "前提：R1 必须留下未配对状态：{}",
            c1.state_json
        );
        // 截断重写：新内容**更短**（必然触发 rescan，而不是快速路）
        let body = format!(
            "{}\n",
            serde_json::to_string(&json!({"type":"function_call_result","timestamp":1700,
                    "cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}}))
            .unwrap()
        );
        std::fs::write(dir.path().join(".workbuddy/projects/p-A/s1.jsonl"), body).unwrap();
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = run(dir.path(), &cursors);
        assert!(
            d2.cursors[0].byte_offset < c1.byte_offset,
            "前提：更短的重写必须触发 rescan（游标按新文件重算）"
        );
        assert!(
            d2.details.is_empty(),
            "状态必须随 rescan 重置：沿用旧状态会凭空配出 500ms 的假耗时"
        );
    }

    /// **GC 6 / B6 唯一入口锁**：供应商一律经 `DeltaBuilder::provider_of`。
    /// 直连 `provider::resolve_provider` 时**行键里的 provider 一字不差**，但三态记录不下来
    /// → `provider_kind` 退化成 `Unknown`（UI 的三态标记全丢）→ 下面这条必须红。
    #[test]
    fn provider_goes_through_the_single_entry_and_rules_come_from_ctx() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let no_cursors = HashMap::new();
        let rules = vec![ProviderRule {
            prefix: "deepseek-".to_string(),
            provider: "volcengine".to_string(),
        }];
        // 规则只能从 ctx 注入（GC 21）：lib 单测构建里 `CollectContext::new` 拿到的是空规则，
        // 采集器**不得**自己去读全局设置库。
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors)
            .with_provider_rules(rules);
        let delta = collect_with_home(&ctx, dir.path()).unwrap();
        assert_eq!(delta.details.len(), 1);
        let d = &delta.details[0];
        assert_eq!(d.key.provider, "volcengine", "用户规则命中 → 按规则归因");
        assert_eq!(
            d.provider_kind,
            SourceKind::Inferred,
            "供应商必须经 DeltaBuilder::provider_of 记录三态（直连 resolve_provider 会退化成 unknown）"
        );
    }

    /// **模型继承锁**：真机那 **2 条** assistant 中断行 `providerData` 里**没有任何模型字段**
    /// （只有 `{skipRun, error, agent}`）——直接落 `model=""` 会造出明细表主键里的**幽灵行**
    /// （B2 缺陷③ / D-32 同族），而任务书夹具给每行都写了模型 → 这条路径在任务书用例里零覆盖。
    /// 本用例：R1 一行带模型（顺带把模型写进 `state_json`）+ R2 追加一行**无模型**的中断行。
    #[test]
    fn model_less_rows_inherit_the_file_model_instead_of_a_ghost_row() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"message","role":"assistant","status":"completed","timestamp":1000,
                    "cwd":"/p/A","sessionId":"s1",
                    "content":[{"type":"output_text","text":"ok"}],
                    "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert_eq!(d1.details.len(), 1);
        assert!(
            d1.cursors[0].state_json.contains("deepseek-v4.1-flash"),
            "前提：模型必须进 state_json（跨轮继承的载体）：{}",
            d1.cursors[0].state_json
        );
        // R2：真机中断行的真实形态——**没有任何模型字段**
        append_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &json!({"type":"message","role":"assistant","status":"incomplete","timestamp":1800,
                    "cwd":"/p/A","sessionId":"s1",
                    "content":[{"type":"output_text","text":"Interrupted by user"}],
                    "providerData":{"agent":"cli","skipRun":true,
                                    "error":{"message":"Interrupted by user"}}}),
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = run(dir.path(), &cursors);
        assert_eq!(
            d2.details.iter().filter(|d| d.key.model.is_empty()).count(),
            0,
            "不得造 `model=\"\"` 幽灵行（继承本文件最近一次见到的模型；跨轮经 state_json）"
        );
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.counters.interrupted)
                .sum::<i64>(),
            1
        );
        assert_eq!(
            d2.details[0].key.model, "deepseek-v4.1-flash",
            "中断计数落到继承来的模型行上（不是 DB 的 hy3）"
        );
    }

    /// **M1 就地锁（R3 / D17 计数分层）**：workbuddy 本机无子代理会话、也无子代理概念
    /// → `is_subagent` 必须是 `false`。置真的后果**不可逆**：① Task 18 按 R3 **硬过滤**该位
    /// → 整个源从会话数 / turn 层消失；② DAO 是 `MAX(is_subagent, excluded)` **只升不降**（W-31）。
    /// （peer 都有就地锁：`kimi.rs` / `codex.rs` / `claude.rs`。）
    #[test]
    fn session_is_never_marked_as_a_subagent() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let no_cursors = HashMap::new();
        let (delta, _) = run(dir.path(), &no_cursors);
        assert_eq!(delta.sessions.len(), 1);
        assert!(
            !delta.sessions[0].is_subagent,
            "workbuddy 无子代理会话：is_subagent 必须为 false（置真会被 Task 18 硬过滤且不可逆）"
        );
        // 前提断言（防假绿）：会话维度真的产出了（否则上面那条恒真）
        assert_eq!(delta.sessions[0].project_key, "a");
    }

    /// **裁决 A（plan-mandated）**：`sessions` 表不在（版本漂移 / 库损坏）→ 必须**整源 Err**。
    /// `open_usage_db` 的探针只摸一句 `SELECT count(*) FROM sqlite_master`，**不碰 `sessions` 表**
    /// → 旧实现 `prepare` 失败即 `return Vec::new()` → 0 会话 → `ok=true` + `parsed_files=0` + 无
    /// `errorCode` = UI 的「WorkBuddy 没有用量」，且**此后每轮都是 0**（D-35 同族）。
    #[test]
    fn sessions_table_missing_fails_the_source_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".workbuddy/workbuddy.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE other (x TEXT);").unwrap(); // **没有** sessions 表
        drop(conn);
        // 前提断言：库本身打得开（否则测的是「未安装 / 打不开」那条路）
        assert!(
            crate::monitor::sqlite::open_usage_db(&db).is_some(),
            "前提不成立：这条构造应当能打开库"
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = collect_with_home(&ctx, dir.path())
            .expect_err("sessions 表不在 → 必须整源报错，不得变成 ok=true + 0 会话");
        assert_eq!(err.code, "usage-source-io");
        assert!(err.detail.contains("sessions"), "{}", err.detail);
    }

    /// **裁决 A 的行级路径**：`id` 是 `TEXT PRIMARY KEY`，而 SQLite 允许主键为 `NULL`（legacy）
    /// → 取行时类型错。旧实现 `flatten()` 把它**静默丢掉**（少一个会话、没有任何信号）；
    /// 现在必须 warn + 计数 + 整源 `Err`（`SourceDelta` 形状冻结、没有「丢了几行」的字段，
    /// `Err` 是唯一**可见**的通道）。
    #[test]
    fn row_level_session_error_fails_the_source_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".workbuddy/workbuddy.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, custom_title TEXT,
                 model TEXT, deleted_at TEXT);
             INSERT INTO sessions (id, cwd, title) VALUES (NULL, '/p/A', '标题');",
        )
        .unwrap();
        drop(conn);
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = collect_with_home(&ctx, dir.path())
            .expect_err("行级错误不得静默丢行（旧实现 flatten() 把它吞了）");
        assert_eq!(err.code, "usage-source-io");
        assert!(err.detail.contains("1 行出错"), "{}", err.detail);
    }

    /// **裁决 B / M2**：`open_usage_db` 把「文件不存在」「CANTOPEN / NOTADB / 权限」
    /// 「BUSY 且 immutable 也失败」合并成 `None`（`monitor/sqlite.rs:47-51`）。**只有前者**
    /// 才是「未安装」；库**在**却打不开 = 这个源读不了 → 整源响亮失败（W-06 / D-24 同族）。
    /// 构造用**目录**（两种打开方式都必然失败；不依赖 chmod —— D-35 的教训）。
    #[test]
    fn unreadable_db_file_fails_the_source_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".workbuddy/workbuddy.db");
        std::fs::create_dir_all(&db).unwrap();
        // 前提断言（防假绿）：这条构造必须真的走到 `open_usage_db → None` 那一支
        assert!(
            crate::monitor::sqlite::open_usage_db(&db).is_none(),
            "前提不成立：这条构造没走到 None 分支，用例测的是别的路径"
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = collect_with_home(&ctx, dir.path())
            .expect_err("库存在但打不开 → 整源报错，不得当成「未安装」");
        assert_eq!(err.code, "usage-source-io");
        assert!(err.detail.contains("workbuddy.db"), "{}", err.detail);
    }

    /// **裁决 B / M3**：`state_json` 形状演进 / 损坏时**不得静默重置**（跨轮未配对调用与模型继承
    /// 会一起丢，表现为 `tool_ms` 少算且没有任何信号）——实现改为 `log::warn!` + 按 rescan 语义重置。
    /// 仓内无日志捕获设施（W-37 先例）→ warn 本身无法断言；这里锁**可观测的部分**：
    /// 不 panic、不整源失败、坏状态按重置语义处理（不会凭空虚配）。
    #[test]
    fn corrupted_cursor_state_is_reset_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"row-call-1",
                    "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert_eq!(d1.cursors.len(), 1);
        // 损坏游标状态（模拟形状演进 / 写坏）
        let mut cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        for c in cursors.values_mut() {
            c.state_json = "{\"tools\":".to_string();
        }
        append_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &json!({"type":"function_call_result","id":"row-res-1","callId":"call_1",
                    "timestamp":1700,"cwd":"/p/A","sessionId":"s1",
                    "providerData":{"model":"deepseek-v4.1-flash"}}),
        );
        let (d2, _) = run(dir.path(), &cursors);
        assert_eq!(d2.new_records, 1, "坏状态不得影响本轮新增行的处理");
        assert!(
            d2.details.is_empty(),
            "坏状态按 rescan 语义重置 → 上一轮那个未配对调用**不在**了（不得凭空配对出 500ms）"
        );
        assert!(
            serde_json::from_str::<WbState>(&d2.cursors[0].state_json).is_ok(),
            "写回的必须是可反序列化的新状态：{}",
            d2.cursors[0].state_json
        );
    }

    /// **裁决 C（D-32 同款）inline 重放**：首条产明细行**没有模型**时缓存原始行，
    /// 首个带模型的行之后**按原序重放** —— 工具状态随之按原序推进，
    /// 所以「调用在缓存里、回执在缓存外」照样配上。
    #[test]
    fn model_less_prefix_is_replayed_in_order_and_keeps_tool_pairing() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                // ① 首条产明细行**无模型**：带 usage + 发起一次工具调用
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"r1",
                       "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "message":{"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
                                           "cache_read_input_tokens":80}},
                       "providerData":{"agent":"cli"}}),
                // ② 首条带模型的行（排在缓存行之后）→ 触发重放
                json!({"type":"message","role":"assistant","status":"completed","timestamp":1300,
                       "cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"output_text","text":"ok"}],
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
                // ③ 回执行（带模型）：重放必须把 ① 的调用按原序送进 state.tools 才能配上
                json!({"type":"function_call_result","id":"r3","callId":"call_1","timestamp":1700,
                       "cwd":"/p/A","sessionId":"s1",
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (delta, stats) = run(dir.path(), &no_cursors);
        assert_eq!(stats.repeat_reads, 0);
        assert_eq!(
            delta
                .details
                .iter()
                .filter(|d| d.key.model.is_empty())
                .count(),
            0,
            "不得写 `model=\"\"` 幽灵行（D-32 明文裁决）"
        );
        assert_eq!(delta.details.len(), 1, "重放后全部落到同一个模型行");
        let d = &delta.details[0];
        assert_eq!(
            d.key.model, "deepseek-v4.1-flash",
            "缓存行归到**解析出来的**模型（不是 DB 的 hy3）"
        );
        assert_eq!(d.requests, 1, "① 的 usage 必须入账（不重放 → 0）");
        assert_eq!(d.buckets.input_fresh, 20);
        assert_eq!(d.counters.tool_calls, 1);
        assert_eq!(
            d.counters.tool_ms, 500,
            "跨缓存边界的配对：重放按原序推进 state.tools（inline 重放被拿掉 → 0）"
        );
        assert_eq!(delta.new_records, 3);
    }

    /// **裁决 C：收尾结算**。若本文件**唯一**带模型的行恰好是**最后一行**，缓存行就没机会
    /// 在行循环里 inline 重放 —— 漏掉收尾这一步会把整段记录**静默丢掉**（kimi 的 D-32 修过一次同款）。
    #[test]
    fn model_known_only_at_the_last_line_still_attributes_the_earlier_rows() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"r1",
                       "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                       "providerData":{"agent":"cli"}}),
                json!({"type":"function_call_result","id":"r2","callId":"call_1","timestamp":1700,
                       "cwd":"/p/A","sessionId":"s1","providerData":{"agent":"cli"}}),
                // ③ **最后一行**才出现模型
                json!({"type":"message","role":"assistant","status":"completed","timestamp":1800,
                       "cwd":"/p/A","sessionId":"s1",
                       "content":[{"type":"output_text","text":"ok"}],
                       "message":{"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}},
                       "providerData":{"model":"deepseek-v4.1-flash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (delta, _) = run(dir.path(), &no_cursors);
        assert_eq!(
            delta
                .details
                .iter()
                .filter(|d| d.key.model.is_empty())
                .count(),
            0
        );
        assert_eq!(delta.details.len(), 1);
        let d = &delta.details[0];
        assert_eq!(d.key.model, "deepseek-v4.1-flash");
        assert_eq!(d.requests, 2, "①② 的入账不能丢（漏收尾结算 → 只剩 ③）");
        assert_eq!(d.buckets.input_fresh, 15, "10 + 5");
        assert_eq!(d.request_total, 15);
        assert_eq!(d.counters.tool_calls, 1);
        assert_eq!(d.counters.tool_ms, 500, "①② 按原序重放 → 回执照样配上");
    }

    /// **裁决 C：整轮拿不到模型 → 丢弃 + 计数**（绝不写 `model=""` 行）。
    /// 计数要有可断言面：仓内无日志捕获设施（W-37），故 `dropped_no_model` 落进 `state_json`。
    #[test]
    fn file_without_any_model_row_drops_with_a_count_and_never_writes_an_empty_model_row() {
        let dir = tempfile::tempdir().unwrap();
        make_db(dir.path(), &[("s1", "/p/A")]);
        write_jsonl(
            dir.path(),
            "p-A",
            "s1",
            &[
                json!({"type":"function_call","name":"shell","timestamp":1200,"id":"r1",
                       "callId":"call_1","cwd":"/p/A","sessionId":"s1",
                       "message":{"usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}},
                       "providerData":{"agent":"cli"}}),
                json!({"type":"function_call_result","id":"r2","callId":"call_1","timestamp":1700,
                       "cwd":"/p/A","sessionId":"s1","providerData":{"agent":"cli"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (delta, _) = run(dir.path(), &no_cursors);
        assert!(
            delta.details.is_empty(),
            "整轮拿不到模型 → 丢弃（不写 `model=\"\"` 行）"
        );
        assert_eq!(delta.new_records, 2, "行都解析过（只是不入账）");
        assert_eq!(delta.parsed_files, 1);
        assert_eq!(delta.sessions.len(), 1, "会话维度照常登记");
        let state = &delta.cursors[0].state_json;
        assert!(
            state.contains("\"dropped_no_model\":2"),
            "丢弃必须**计数**且可断言：{state}"
        );
    }

    /// **登记锁（GC 20 / §3.2.2 阻塞 4）**：漏登记不是编译错——`run_collection` 只遍历
    /// `collectors::all()`，这个源会被静默跳过。
    #[test]
    fn collector_is_registered_in_all() {
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::WorkBuddy),
            "workbuddy 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
