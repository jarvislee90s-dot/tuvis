//! Kimi Code 采集器。数据布局与路径发现**复用既有 `monitor::kimi_parser`**
//! （`resolve_data_root` / `parse_session_index` / `resolve_session_dir`），不另起一套。
//! 关键约束（矩阵 §3.1/§3.5）：
//! * **必须 glob 全部 `agents/*/wire.jsonl`**——只读 `main/` 会系统性少算 output 的 **21.6%**
//!   （本机 agents/ 下 40 个子代理目录、553 条 usage.record）；
//! * `usageScope=="turn"` 是 **per-LLM-step**（虚高 9.14×）→ turn 数只能数
//!   `context.append_loop_event[].event.turnId` 去重；
//! * `metadata` 行**天然无 `time`**（硬取 `d["time"]` 必崩）→ 必须先判 type 再取字段；
//! * `usage.record` **没有 turnId**、绝大多数没有 agentId → 归属只能靠目录；
//! * 字段名与别家不同：`inputOther / output / inputCacheRead / inputCacheCreation`。
//!
//! ## 三条跨任务纪律
//! * **W-19**：`SessionFileScan` 的 namespace 由 `scan_namespace(sid)` 派生，不手写字符串字面量；
//! * **W-11**：`DedupSet`/`PendingTools` 都是**单文件、单轮**实例（每个文件各自新建），
//!   跨轮靠 `state_json`（`turn_ids` 去重集合 + `PendingTools`）；
//! * **W-06 / D-24（W-42 同族）**：单文件读失败**不一律 warn + 跳过**——只有「枚举与读取之间的
//!   轮转竞态」（文件已消失）才跳过，其余一律整源 `Err("usage-source-io")`（否则用户看到的是
//!   「这个源没有用量」而不是「这个源读不了」）。
//!
//! ## 与任务书代码块的 4 处**有意偏离**（真机语料对齐；逐条见报告「偏差申报」）
//! 任务书代码块在下面 4 处与真机（`~/.kimi-code/sessions`，82 文件 / 17,068 行）形态不符，
//! **错了不编译报错、只静默错数**，每处都有真能变红的回归用例：
//! * **事件是嵌套的**：`tool.call` / `tool.result` / `step.*` 全都嵌在
//!   `context.append_loop_event.event` 下（顶层只有 `usage.record` / `turn.prompt` /
//!   `turn.ended` 等）。任务书按**顶层** `"tool.call"` 匹配 → 真机 `tool_calls` 恒为 **0**
//!   （静默少采 100%；matrix 记的是 2,574 条 + 配对 100%）。所以先把事件**归一**再分流；
//! * **模型要继承**：嵌套事件里没有 `model` 字段，只认本行字段会让 turn/tool 计数全部落到
//!   `model=""` / `provider=""` 的独立明细行上（按模型/供应商分组时整块消失）；
//! * **原生时长读在顶层**：`turn.ended.durationMs`（11 条 / 覆盖率 5.5% / max 147,556 ms）
//!   在**顶层**，任务书读的是 `/event/durationMs`（嵌套对象里没有该字段）→ 一条都采不到；
//!   且 `turn.ended.turnId` 是**数字** `0`，`append_loop_event.event.turnId` 是**字符串** `"0"`。
//!   缺失时按 `step.begin` → 终结 `step.end` 的行级 `time` 推导（真机 242 turn 里 207 可推，
//!   max 2,156.5 s，与矩阵 §6 一致），两条路径靠 `state.dur_done` 保证**同一 turn 只出一条**；
//! * **`user_est` 缺了一整支**：kimi 属于「能读到用户文本」的三源（§4.3 / GC 18-R1），
//!   文本在顶层 `turn.prompt.input[].text`（真机 242 条 = 每 turn 一条）。任务书没有这一支 →
//!   「用户输入(估)」永久为 `NULL`，且没有任何用例会红。
use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::{file_key_of, file_read_failure_is_benign, DeltaBuilder, PendingTools};
use crate::monitor::kimi_parser::{parse_session_index, resolve_data_root, resolve_session_dir};
use crate::monitor::session_scan::SessionFileScan;
use crate::services::usage::collect::{scan_namespace, CollectContext, UsageCollector};
use crate::services::usage::dedup::turn_key;
use crate::services::usage::delta::{DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::provider::provider_from_model_prefix;
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{
    default_policy, normalize, resolve_semantics, user_est_of, RawUsage,
};

/// 本源 `SessionFileScan` 的 namespace（GC 5 / **W-19**）：**必须**由 `scan_namespace(sid)`
/// 派生，不得手写字符串字面量——同 ns 混存别的产物类型会让 `Arc::downcast` 失配、
/// 缓存静默失效。
///
/// ⚠️ **本源的 L2 摘要缓存角色由「游标快速路」承担，这里只保留 ns 归属**（评审裁决 C-Minor#3）：
/// kimi 的枚举来自 `session_index.jsonl` + `read_dir`（有界：索引条目 × agents 目录），
/// 单遍解析的产物**没有第二处消费者** → `get_or_parse` 在本源没有可缓存的东西；
/// 文件级「内容未变 → 零读取」由 `usage_cursor` 的 `(mtime,size)` + 尾指纹
/// （`stream.rs` 的快速路，GC 5 的 L2 同款语义）承担。
/// 保留 `SCAN` 只为 `retain_existing` 的 ns 归属与 W-19 的机械落点——**不要**据此认为
/// kimi 已经接上 L2 摘要缓存（那是 `monitor::kimi_parser` 的 `kimi-wire` ns 的事）。
const SCAN: SessionFileScan = SessionFileScan::new(scan_namespace(UsageSourceId::Kimi));

/// 跨轮 turnId 去重集合的上限（有界：只防长寿命会话把 state_json 撑爆）
const TURN_IDS_CAP: usize = 2048;
/// 未闭合 turn 的上限：同一 agent 目录内 turn 本是**顺序**的（正常恒为 1 条），
/// 留余量给「乱序 / 并行 turn」的异常形态，绝不让它无界。
const OPEN_TURNS_CAP: usize = 32;
/// 已出时长样本的 turnId 上限：原生 `durationMs` 与推导值对同一 turn **只能出一条**
const DUR_DONE_CAP: usize = 512;
/// 「模型未知期间」缓存行的上限：真机单文件最多几十行（首条 `usage.record` 之前的
/// turn/tool 事件共 368 行 / 分散在 61 个文件）。超限即丢 + 计数 + `log::warn!`，
/// **绝不**退化成写 `model=""` 行。
const PENDING_LINES_CAP: usize = 4096;

pub struct KimiCollector;

fn ts_of(v: &Value) -> Option<i64> {
    v.get("time").and_then(|t| t.as_i64())
}

/// turnId 的两种形态归一：`context.append_loop_event.event.turnId` 是**字符串** `"0"`，
/// 而 `turn.ended.turnId` 是**数字** `0`（本机 11 条原生时长全在后者）。
/// 只认字符串会让原生时长与推导值对不上同一个 turn（真机：0 条能配上）。
fn turn_id_of(v: &Value) -> String {
    match v.get("turnId") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 文本数组（`turn.prompt.input` = `[{type:"text",text}]`）→ 拼接成一个字符串。
/// 字段在场但内容为空 → `Some("")`（**在场的空是真数据**，估算值为 0 也是真值）；
/// 字段缺失 / 不是数组 → `None`（不可得，保持 `user_est` 为 `NULL`）。
fn text_of(parts: Option<&Value>) -> Option<String> {
    let arr = parts?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 一条 turn 时长样本（原生优先、推导兜底，两条路径共用）
fn push_turn_ms(b: &mut DeltaBuilder, key: DetailKey, ts: i64, ms: i64) {
    b.detail(key, ts).counters.turn_ms.push(ms);
}

/// 记下「该 turn 已出过时长样本」（有界）：原生 `durationMs` 与推导值不得同 turn 双计
fn mark_dur_done(state: &mut KimiFileState, tid: String) {
    state.dur_done.push(tid);
    if state.dur_done.len() > DUR_DONE_CAP {
        state.dur_done.remove(0);
    }
}

/// 把一个**已闭合**（有终结态）的 turn 结算成一条时长样本；`true` = 真的出了样本。
///
/// * 明细行键用**该 turn 自己**的模型原文重建，而不是「本轮最后一条事件」的模型
///   ——同一会话可以跨模型（真机：`xfyun/kimi-k2.6` → `opencode-go/deepseek-v4-flash`），
///   用末条模型会把早期 turn 的时长记到别的模型行上；
/// * 「已出过样本」的（原生 `durationMs` 已结算过）只清条目、不再出值（不双计）；
/// * `last_terminal.is_none()`（还没闭合）→ 不动它，等下一轮。
fn flush_one_turn(
    b: &mut DeltaBuilder,
    state: &mut KimiFileState,
    session_id: &str,
    work_dir: &str,
    tid: &str,
) -> bool {
    let Some(t) = state.open_turns.get(tid).cloned() else {
        return false;
    };
    let Some(end) = t.last_terminal else {
        return false;
    };
    state.open_turns.remove(tid);
    if state.dur_done.iter().any(|d| d == tid) {
        return false;
    }
    // **供应商唯一入口**（GC 6 / B6）：模型名前缀即供应商（实测 Measured）
    let (prefix, rest) = provider_from_model_prefix(&t.model_raw);
    let model = rest.unwrap_or_else(|| t.model_raw.clone());
    let (provider, _) = b.provider_of(
        if prefix.is_empty() {
            None
        } else {
            Some(&prefix)
        },
        &model,
    );
    let hour = hour_key_of_host(end);
    let key = DetailKey {
        session_id: session_id.to_string(),
        hour_key: hour.clone(),
        day_key: hour.chars().take(10).collect(),
        project_key: project_key_of(Some(work_dir)),
        model,
        provider,
    };
    push_turn_ms(b, key, end, (end - t.begin).max(0));
    mark_dur_done(state, tid.to_string());
    true
}

/// 单文件、跨轮的续读状态（进游标 `state_json`）
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct KimiFileState {
    tools: PendingTools,
    /// 已计过 turn 数的 turnId（跨轮去重）
    turn_ids: Vec<String>,
    /// 最近一次 `usage.record` 的**模型原文**（含供应商前缀；跨轮）：
    /// 嵌套事件里没有 `model`，只认本行字段会让所有计数落到 `model=""` 的行上
    model: String,
    /// 未闭合 turn：turnId → 起止（用于按行级 `time` 推导时长）
    open_turns: BTreeMap<String, OpenTurn>,
    /// 已出过时长样本的 turnId（有界）
    dur_done: Vec<String>,
}

/// 一个 turn 的跨轮状态（`step.begin` → 终结 `step.end`）
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct OpenTurn {
    /// 该 turn **首个** `step.begin` 的行级 `time`
    begin: i64,
    /// 该 turn **最后一个终结态** `step.end`（`finishReason != "tool_use"`）的行级 `time`；
    /// `None` = 还没闭合（读到本轮末尾时它还在跑）→ 留在 `state` 里，下一轮再结算
    last_terminal: Option<i64>,
    /// 该 turn 事发时的模型原文（结算时按它归行，见 `flush_one_turn`）
    model_raw: String,
}

impl UsageCollector for KimiCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::Kimi
    }

    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        let env_home = std::env::var("KIMI_CODE_HOME").ok();
        let Some(root) = resolve_data_root(env_home.as_deref(), ctx.home) else {
            return Ok(SourceDelta::default());
        };
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层（GC 21）。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::Kimi, rules);
        let mut live = HashSet::new();
        // 「**本轮首见模型**」（跨文件兜底，评审裁决 A 的括号项）：真机有 **23/82** 个文件
        // **通篇没有** `usage.record`（限流/被取消的空会话：只有 `turn.step.retrying` /
        // `turn.ended` / `turn.prompt` / loop 事件）→ 它们的 turn/报错计数必须有个落行处。
        // 用本轮第一个解析出的模型兜底，**而不是**写 `model=""` 行（D9 分组退化）。
        // 真机第 1 个文件（索引顺序 × agents 目录名序）就带 model → 这 23 个文件全都有兜底。
        let mut round_model: Option<String> = None;
        for entry in parse_session_index(&root) {
            let session_dir = resolve_session_dir(&root, &entry.session_dir);
            if !session_dir.starts_with(&root.sessions) {
                continue; // 信任边界：sessionDir 越界跳过（与既有解析器同规）
            }
            let agents_dir = session_dir.join("agents");
            let Ok(agents) = std::fs::read_dir(&agents_dir) else {
                continue;
            };
            let mut dirs: Vec<_> = agents.flatten().map(|e| e.path()).collect();
            dirs.sort();
            // 该会话本轮**最新事件时间**（会话维度 `last_seen_at`；agent 目录循环外只登记一次）
            let mut session_last_ts: Option<i64> = None;
            for agent_dir in dirs {
                let wire = agent_dir.join("wire.jsonl");
                if !wire.exists() {
                    continue;
                }
                live.insert(wire.clone());
                // 游标键 = **相对 sessions 根**的路径（B5）。**绝不能**取文件 stem：
                // kimi 的全部会话文件都叫 `wire.jsonl`（本机 82 个）→ stem 恒为 "wire"，
                // 82 个文件挤在一行游标里（后写覆盖先写），其余 81 个每轮重扫重计。
                // 根必须是 **sessions 根**（不是单个会话目录）：不同会话下的
                // `agents/main/wire.jsonl` 也要各自成键。
                let file_key = file_key_of(&root.sessions, &wire);
                let read = match ctx.read_incremental(&wire, &file_key) {
                    Ok(r) => r,
                    Err(e) => {
                        // **W-06 / D-24**：只有「文件已消失」这一种轮转竞态才允许跳过；
                        // 其余读失败（权限 / EIO / mtime 早于 1970 / 目录冒充文件）必须
                        // **整源响亮失败**（`usage-source-io` → `UsageSourceStatus.errorCode`），
                        // 不得只 `log::warn!` 后跳过——那会让 UI 显示「这个源没有用量」。
                        if file_read_failure_is_benign(&wire, &e) {
                            log::warn!(
                                "usage/kimi: 读取 {} 失败（文件已消失，跳过本轮）: {}",
                                wire.display(),
                                e
                            );
                            continue;
                        }
                        return Err(UsageError::new(
                            "usage-source-io",
                            format!("read {} failed: {e}", wire.display()),
                        ));
                    }
                };
                if read.lines.is_empty() && !read.rescan {
                    b.push_cursor(read.next);
                    continue;
                }
                // 跨轮续读状态：turnId 去重集合 + 工具配对 + 未闭合 turn（W-11：每个文件各自一份，
                // 随游标 `state_json` 跨轮）
                let mut state: KimiFileState = if read.rescan {
                    KimiFileState::default()
                } else {
                    ctx.cursor_of(&file_key)
                        .and_then(|p| serde_json::from_str(&p.state_json).ok())
                        .unwrap_or_default()
                };
                let mut turns = crate::services::usage::dedup::DedupSet::new();
                for tid in &state.turn_ids {
                    turns.admit(turn_key("kimi", &entry.session_id, tid));
                }
                // 会话维度 `last_seen_at`：只统计**本轮真有新行**的文件
                let seen = ts_now(&read.lines).unwrap_or(ctx.now_ms);
                session_last_ts = Some(session_last_ts.map_or(seen, |cur: i64| cur.max(seen)));
                // 「模型未知期间」的原始行（**按行缓存**，见 `handle_line` 的文档；裁决 A）
                let mut pending: Vec<String> = Vec::new();
                let mut dropped_no_model: i64 = 0;
                // ---- 一遍扫描（GC 4：同一遍里出四桶 / turn / 工具 / 报错 / 时长）----
                for line in &read.lines {
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    b.new_records += 1;
                    // 模型已就绪 → 先把缓存行按**原序**重放（它们都排在本行之前）
                    if !pending.is_empty() && !state.model.is_empty() {
                        let left = replay_pending(
                            &mut pending,
                            &mut b,
                            &mut state,
                            &mut turns,
                            &entry.session_id,
                            &entry.work_dir,
                        );
                        warn_unattributed(&wire, left);
                    }
                    if !handle_line(
                        &mut b,
                        &mut state,
                        &mut turns,
                        &entry.session_id,
                        &entry.work_dir,
                        &v,
                    ) {
                        if pending.len() < PENDING_LINES_CAP {
                            pending.push(line.clone());
                        } else {
                            if dropped_no_model == 0 {
                                log::warn!(
                                    "usage/kimi: {} 的「模型未知」缓存超上限 {PENDING_LINES_CAP} 行，\
                                     超出部分丢弃（该文件的这些事件不入账）",
                                    wire.display()
                                );
                            }
                            dropped_no_model += 1;
                        }
                    }
                }
                // 先记住「本轮首见模型」（本文件的模型可能刚刚才被解析出来）
                if round_model.is_none() && !state.model.is_empty() {
                    round_model = Some(state.model.clone());
                }
                // 收尾结算缓存行：**本文件解析出的模型 → 本轮首见模型 → 丢弃**（顺序不许颠倒！）。
                //
                // `state.model` **可能此刻已经非空**：若本文件首条（也是唯一一条）带 model 的
                // `usage.record` 恰好落在**最后一行**，pending 就没机会在行循环里 inline 重放，
                // 于是走到这里时 `state.model` 已是**本文件自己的模型**。
                // 旧写法直接 `match round_model`（完全不看 `state.model`）会把它覆盖成本轮首见模型
                // ——真机已命中：处理序第 72 个文件 `wd_financialart…/session_cba38477…`（13 行，
                // 唯一 `usage.record` 在末行、model `ollama-cloud/glm-5.3-flash`）的 11 行事件
                // 被记到第 0 个文件的 `opencode-go/kimi-k2.7-code` 行上，并把错模型写进游标 `state_json`
                // 供后续轮次沿用（复审 Important，2026-10-04）。
                if !pending.is_empty() {
                    let m = if state.model.is_empty() {
                        round_model.clone()
                    } else {
                        Some(state.model.clone())
                    };
                    match m {
                        Some(m) => {
                            // 兜底模型**写进 `state_json`**：该文件后续轮次沿用它，跨轮归属稳定
                            // （它反正永远解析不出自己的模型）；本文件已有模型时这里是**空操作**
                            state.model = m;
                            let left = replay_pending(
                                &mut pending,
                                &mut b,
                                &mut state,
                                &mut turns,
                                &entry.session_id,
                                &entry.work_dir,
                            );
                            warn_unattributed(&wire, left);
                        }
                        None => {
                            log::warn!(
                                "usage/kimi: {} 有 {} 行事件出现在首条 usage.record 之前，且**整轮**都\
                                 拿不到模型 → 丢弃（不写 model=\"\" 行；D9 分组退化）",
                                wire.display(),
                                pending.len()
                            );
                            pending.clear();
                        }
                    }
                }
                // Low #3（复审）：丢弃计数要有对外可观测面（不能只用来抑制重复 warn）
                if dropped_no_model > 0 {
                    log::warn!(
                        "usage/kimi: {} 本轮共丢弃 {dropped_no_model} 行「模型未知」事件（缓存超上限 {PENDING_LINES_CAP}）",
                        wire.display()
                    );
                }
                // ---- 文件末尾结算：把还剩在表里、本轮已闭合（有终结态）的 turn 出成时长样本 ----
                // 跨轮：`step.begin` 可能落在上一轮的字节里（上一轮读到末尾它还没闭合）→
                // `open_turns` 随 `state_json` 带过来，这里一并结算。
                // 未闭合的（`last_terminal == None`）**不出样本**、留在 state 里等下一轮
                // （宁可不报，也不报一个「把挂机时间算进去」的假值）。
                let remaining: Vec<String> = state.open_turns.keys().cloned().collect();
                for tid in remaining {
                    flush_one_turn(&mut b, &mut state, &entry.session_id, &entry.work_dir, &tid);
                }
                // 会话维度：**一个 index 条目 = 一个会话**（显式放在 agent 目录循环之外）。
                // kimi 的子代理目录与主会话共用同一 index 条目 → `is_subagent=false`
                // （会话不分层：D17 的分层对象是「会话」，子代理目录不是一个会话；turns 含子代理）；
                // `parent_session_id` 传 `None`：`state.json` 的 `parentAgentId` 是 **agent 名**，
                // 塞进「会话 id」列就是垃圾（评审裁决 C-Minor#4），而本源不存在会话级血缘。
                if let Some(last_seen) = session_last_ts {
                    b.session(
                        &entry.session_id,
                        Some(&entry.work_dir),
                        None,
                        false, // 会话本身不因子代理目录而分层
                        None,  // 本源无会话级血缘（旧实现在这里塞 agent 名）
                        None,  // originator 是 Codex 专有字段
                        last_seen,
                    );
                }
                b.parsed_files += 1;
                let mut next = read.next;
                next.state_json = serde_json::to_string(&state).unwrap_or_default();
                b.push_cursor(next);
            }
        }
        SCAN.retain_existing(&live);
        Ok(b.finish())
    }
}

fn ts_now(lines: &[String]) -> Option<i64> {
    lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| ts_of(&v))
        .max()
}

/// **单行处理（唯一实现）**：正常路径一行调一次；「模型未知期间」缓存下来的原始行在解析出模型后
/// **按原序重放**时也走这里 —— 两条路径逐字同源，不会长出第二套分流。
///
/// 返回 `false` = **本行需要模型、但此刻还不知道属于哪个模型**（调用方负责缓存、稍后重放，
/// 或收尾时显式丢弃）：
/// * 只有 `usage.record` 带 `model`（真机 2,395 条全带），`turn.prompt` / `tool.call` /
///   `step.*` 全都没有 → 直接落行就是 `model=""` / `provider=""` 的明细行；
/// * 真机有 **368 条** turn/tool 事件与 **65/242 条** `turn.prompt` 排在**本文件首条
///   `usage.record` 之前** → 近半「用户输入(估)」(27,694/58,646 = 47.2%) 会被归到空模型行上，
///   正是 `provider.rs:92-93` 自己点名要避免的「明细行 model 与 provider 双空（D9 分组退化）」。
///
/// 无 `time` 的行（`metadata`）不产生任何指标 → 直接返回 `true`（不进缓存）。
#[allow(clippy::too_many_arguments)]
fn handle_line(
    b: &mut DeltaBuilder,
    state: &mut KimiFileState,
    turns: &mut crate::services::usage::dedup::DedupSet,
    session_id: &str,
    work_dir: &str,
    v: &Value,
) -> bool {
    let raw_type = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    // **事件归一（真机形态）**：`tool.call` / `tool.result` / `step.*` 嵌在
    // `context.append_loop_event.event` 下，行级 `time` 在外层；顶层事件则整行即事件。
    // 任务书只认顶层 `"tool.call"` → 真机 2,574 条工具调用一条都采不到。
    let nested = raw_type == "context.append_loop_event";
    let null = Value::Null;
    let ev: &Value = if nested {
        v.get("event").unwrap_or(&null)
    } else {
        v
    };
    let etype = if nested {
        ev.get("type").and_then(|s| s.as_str()).unwrap_or("")
    } else {
        raw_type
    };
    // `metadata` 行天然无 time，先按 type 分流再取字段（硬取必崩）
    let Some(ts) = ts_of(v) else { return true };
    // 模型**有状态继承**（跨轮靠 `state.model`）：只有 `usage.record` 带 model；
    // 嵌套/顶层事件都没有，只认本行字段会让计数落到 `model=""` 的独立行上
    if raw_type == "usage.record" {
        if let Some(m) = v.get("model").and_then(|s| s.as_str()) {
            state.model = m.to_string();
        }
    }
    if state.model.is_empty() {
        return false; // 需要模型（含「连 model 字段都没有的 usage.record」）→ 交调用方缓存
    }
    let hour = hour_key_of_host(ts);
    let model_raw = state.model.as_str();
    let (provider_prefix, model_rest) = provider_from_model_prefix(model_raw);
    // D9：模型名完全一致才同一模型（前缀是**结构性**的供应商，不是模糊归并）
    let model = model_rest.unwrap_or_else(|| model_raw.to_string());
    // **供应商唯一入口**（GC 6 / B6）：kimi 的供应商内嵌在模型名前缀里
    // （实测 `ollama-cloud/glm-5.3-flash`）→ explicit 传前缀，判定为 **Measured**。
    // 直接调 `resolve_provider` 不写 `kind_by_provider` → `provider_kind` 落不了库。
    let (provider, _) = b.provider_of(
        if provider_prefix.is_empty() {
            None
        } else {
            Some(&provider_prefix)
        },
        &model,
    );
    let key = DetailKey {
        session_id: session_id.to_string(),
        hour_key: hour.clone(),
        day_key: hour.chars().take(10).collect(),
        // 记录级项目键：kimi 的归属来自 session_index.jsonl 的 workDir
        // （同一会话的 main 与全部子代理目录共用该条目 → 逐记录同值）
        project_key: project_key_of(Some(work_dir)),
        model,
        provider,
    };
    // ---- turn 计数 + 时长（**只在 `context.append_loop_event` 上**，与
    // caps.turn_semantics 的「context.append_loop_event[].event.turnId 去重」一致）----
    if nested {
        let tid = turn_id_of(ev);
        if !tid.is_empty() {
            // **先结算「别的」turn**：kimi 同一 agent 目录内 turn 是**顺序**的，
            // 见到另一个 turn 的事件就说明前者已经闭合。就地结算有两个好处：
            // ① `open_turns` 不会攒满整轮的 turn（首读一个 200+ turn 的会话时，
            //   若只在文件末尾统一结算，容量上限会把早期 turn 挤出 → 静默少算，
            //   这正是本实现第一版在真机上 207→151 的原因）；
            // ② 样本按**它自己**的模型归行。
            // 未闭合（无终结态）的条目不动：留在 state 里等后续轮次。
            let others: Vec<String> = state
                .open_turns
                .keys()
                .filter(|k| k.as_str() != tid)
                .cloned()
                .collect();
            for k in others {
                flush_one_turn(b, state, session_id, work_dir, &k);
            }
            // turn 数按 turnId 去重（本机 main 201 / sub 41 = 242）；
            // `usageScope=="turn"` 是 per-LLM-step（虚高 9.14×）→ 绝不能数它
            if turns.admit(turn_key("kimi", session_id, &tid)) {
                b.detail(key.clone(), ts).counters.turns += 1;
                state.turn_ids.push(tid.clone());
                if state.turn_ids.len() > TURN_IDS_CAP {
                    state.turn_ids.remove(0);
                }
            }
            // 时长推导：首个 step.begin → 最后一个**终结态** step.end 的行级 time
            // （真机 242 turn 里 207 可推；`tool_use` 是中间步，不是终结态）
            match etype {
                "step.begin" => {
                    let e = state.open_turns.entry(tid.clone()).or_insert(OpenTurn {
                        begin: ts,
                        last_terminal: None,
                        model_raw: state.model.clone(),
                    });
                    // 同一 turn 的重试/多步：以**最后一次**终结态为准
                    e.last_terminal = None;
                }
                // 用 match guard 而不是嵌套 if：`collapsible_match` 在
                // `-D warnings` 下是 error（与 D-25 同族的门禁修复）
                "step.end"
                    if ev.get("finishReason").and_then(|s| s.as_str()) != Some("tool_use") =>
                {
                    if let Some(e) = state.open_turns.get_mut(&tid) {
                        e.last_terminal = Some(ts);
                    }
                }
                _ => {}
            }
            if state.open_turns.len() > OPEN_TURNS_CAP {
                // 有界兜底（顺序 turn 下恒为 1 条，正常不触发）：丢 begin 最早的那条。
                // **不静默**：丢一条就是少一条时长样本，必须留下信号。
                if let Some((oldest, o)) = state
                    .open_turns
                    .iter()
                    .min_by_key(|(_, t)| t.begin)
                    .map(|(k, t)| (k.clone(), t.begin))
                {
                    log::warn!(
                        "usage/kimi: 未闭合 turn 超上限 {OPEN_TURNS_CAP}，丢弃最早的一条 \
                         {oldest}（begin={o}；该 turn 的时长样本不会再产出）"
                    );
                    state.open_turns.remove(&oldest);
                }
            }
        }
    }
    match etype {
        "usage.record" => {
            let u = v.get("usage").cloned().unwrap_or(Value::Null);
            let raw = RawUsage {
                input_raw: u.get("inputOther").and_then(|x| x.as_i64()).unwrap_or(0),
                cache_read: u
                    .get("inputCacheRead")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
                cache_write: u
                    .get("inputCacheCreation")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
                output: u.get("output").and_then(|x| x.as_i64()).unwrap_or(0),
                reasoning: 0,
                total_raw: None, // 无 total 判据 → 声明互斥
            };
            let sem = resolve_semantics(default_policy(UsageSourceId::Kimi), &raw);
            if let Some(n) = normalize(&raw, sem) {
                let d = b.detail(key, ts);
                d.buckets.add(&n.buckets);
                d.request_total += n.request_total;
                d.requests += 1;
            }
        }
        // **R1 / 说明书 §4.3（GC 18-R1）**：kimi 属于「能读到用户文本」的三源之一
        // → 「用户输入(估)」必须在明细档与日档都出真值。文本在顶层
        // `turn.prompt.input[].text`（真机 242 条 = 每 turn 一条）。
        // 任务书代码块没有这一支 → 该指标永久为 NULL。
        "turn.prompt" => {
            if let Some(text) = text_of(v.get("input")) {
                let est = user_est_of(&text);
                let d = b.detail(key, ts);
                // 与 claude/codex 同口径：能读到文本就不算「不可得」，
                // 估算值为 0 也是**真值**
                d.user_est = Some(d.user_est.unwrap_or(0) + est);
            }
        }
        "turn.ended" => {
            // **回合失败层只取一个面**（评审裁决 C-Minor#6，选「保持与矩阵一致 + 写明」）：
            // spec §9.5 给 kimi 的判据是 `turn.ended.reason` **/** `agent.turn.ended.outcome=failed`；
            // 本实现只读前者。真机另有 1 条 `agent.turn.ended{outcome:"failed"}` 未计入
            // ——它与 `turn.ended{reason:"failed"}` 是**同一个 turn 的两个面**（D18 三层分列 =
            // 每层取一个面），两面都计会把这 1 轮算两次（2 → 3），反而偏离权威矩阵的 2。
            // 判据：`error.code` 在场（真机 2 条：provider.rate_limit / provider.api_error）
            // 或 `reason=="error"`。
            if v.pointer("/error/code").is_some()
                || v.get("reason").and_then(|r| r.as_str()) == Some("error")
            {
                b.detail(key.clone(), ts).counters.error_turn += 1;
            }
            // **原生「先到者」优先**（评审裁决 C-Minor#5 的措辞收敛；计划原话「有则采，
            // 缺则靠 time 推导」）：真机 `durationMs` 在**顶层**（11 条 / 覆盖率 5.5%），
            // 任务书读的是 `/event/durationMs`（嵌套对象里没有该字段）→ 一条都采不到。
            // 原生到场就用原生值结算该 turn（并摘掉推导条目），同一 turn 不双计。
            // **不是「原生一定优先」**：同 turn 的推导值若已在**更早的轮次**结算（文件在本轮
            // 只追加了 `turn.ended`），`dur_done` 会把它挡住 → 留下的是推导值。真机 11 条原生
            // 均紧跟在终结 `step.end` 之后、下一次结算之前（顺序友好）故不发作，
            // 但这条窗口**无覆盖**（登记）。
            if let Some(ms) = v.get("durationMs").and_then(|x| x.as_i64()) {
                let tid = turn_id_of(v);
                if !tid.is_empty() && !state.dur_done.iter().any(|t| t == &tid) {
                    state.open_turns.remove(&tid);
                    push_turn_ms(b, key.clone(), ts, ms);
                    mark_dur_done(state, tid);
                }
            }
        }
        "turn.step.retrying" => {
            b.detail(key.clone(), ts).counters.error_model += 1;
            // 429 重试（9 条）
        }
        "tool.call" => {
            let name = ev.get("name").and_then(|s| s.as_str()).unwrap_or("unknown");
            // `uuid` 是真机配对键；`toolCallId` 与之同值，作为形状漂移的兜底
            let uuid = ev
                .get("uuid")
                .or_else(|| ev.get("toolCallId"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let d = b.detail(key.clone(), ts);
            d.counters.tool_calls += 1;
            d.counters
                .tool_stats
                .entry(name.to_string())
                .or_insert((0, 0))
                .0 += 1;
            if !uuid.is_empty() {
                state.tools.start(uuid, ts, name);
            }
        }
        "tool.result" => {
            let parent = ev
                .get("parentUuid")
                .or_else(|| ev.get("toolCallId"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if let Some((dur, name)) = state.tools.finish(parent, ts) {
                let d = b.detail(key.clone(), ts);
                d.counters.tool_ms += dur;
                d.counters.tool_stats.entry(name).or_insert((0, 0)).1 += dur;
            }
            if ev
                .pointer("/result/isError")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                b.detail(key, ts).counters.error_tool += 1; // 168/2574
            }
        }
        _ => {}
    }
    true
}

/// 把「模型未知期间」缓存的行**按原序重放**，返回**重放后仍无法归属**的行数。
/// 顺序天然正确：缓存行都排在本文件首条 `usage.record` 之前，而重放发生在它之后的第一行处。
///
/// **返回值不许吞**（复审 Low #2）：`handle_line` 仍可能返回 `false` —— 缓存里若混进一条
/// `usage.record` 且它带空/缺 `model`，`state.model` 会被清空，**其后的缓存行又会变成「需要模型」**。
/// 真机 0 条这类记录（评审已扫：0 条缺字段、0 条空串），属健壮性边界；但**不静默**：
/// 调用方拿返回值计数并 `log::warn!`（见 `warn_unattributed`）。
fn replay_pending(
    pending: &mut Vec<String>,
    b: &mut DeltaBuilder,
    state: &mut KimiFileState,
    turns: &mut crate::services::usage::dedup::DedupSet,
    session_id: &str,
    work_dir: &str,
) -> i64 {
    let mut unattributed = 0i64;
    for pl in pending.drain(..) {
        // 缓存里只可能是**主循环解析成功过**的行（解析失败的在主循环就 `continue` 了、不进缓存），
        // 这里的 Err 纯属纵深防御：照样计数，不许装成「处理过了」。
        match serde_json::from_str::<Value>(&pl) {
            Ok(pv) => {
                if !handle_line(b, state, turns, session_id, work_dir, &pv) {
                    unattributed += 1;
                }
            }
            Err(_) => unattributed += 1,
        }
    }
    unattributed
}

/// 重放后仍无法归属的行 → **不静默**（真机 0 条；见 `replay_pending` 的文档）
fn warn_unattributed(wire: &std::path::Path, n: i64) {
    if n > 0 {
        log::warn!(
            "usage/kimi: {} 重放后有 {n} 行仍无法归属（缓存里混入了把 model 清空的 usage.record？）\
             —— 这些行不入账",
            wire.display()
        );
    }
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::collect::{CollectContext, UsageCollector};
    use serde_json::json;
    use std::collections::HashMap;

    /// fixture：main + 1 个子代理目录；子代理 output 合计精确占总量的 21.6%
    /// （矩阵 §3.5：只读 main 会系统性少算 output 的 21.6%）
    fn fixture(root: &std::path::Path) {
        let sess = root.join("sessions/wd_p/session_1");
        for (agent, out) in [("main", 784i64), ("agent-0", 216)] {
            let d = sess.join("agents").join(agent);
            std::fs::create_dir_all(&d).unwrap();
            let mut body = String::new();
            // metadata 行天然无 time（硬取 time 必崩）：必须存在且必须被跳过
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({"type":"metadata","model":"x"})).unwrap()
            ));
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type":"usage.record","model":"ollama-cloud/glm-5.3-flash","usageScope":"turn",
                    "usage":{"inputOther":100,"output":out,"inputCacheRead":50,"inputCacheCreation":0},
                    "time":1782301061159i64}))
                .unwrap()
            ));
            // 第二个 LLM step 同 turnId（usageScope=="turn" 虚高 9.14× 的陷阱）
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type":"usage.record","model":"ollama-cloud/glm-5.3-flash","usageScope":"turn",
                    "usage":{"inputOther":10,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                    "time":1782301061259i64}))
                .unwrap()
            ));
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type":"context.append_loop_event",
                    "event":{"turnId":"turn_1","type":"step.end","finishReason":"stop"},
                    "time":1782301061300i64}))
                .unwrap()
            ));
            // 工具调用 + 回执（tool.result.parentUuid → tool.call.uuid，配对 100%）
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type":"tool.call","uuid":"call_a","name":"Shell","time":1782301061000i64}))
                .unwrap()
            ));
            body.push_str(&format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type":"tool.result","parentUuid":"call_a","result":{"isError":false},
                    "time":1782301061050i64}))
                .unwrap()
            ));
            std::fs::write(d.join("wire.jsonl"), body).unwrap();
        }
        // state.json：子代理血缘（type/parentAgentId）
        std::fs::write(
            sess.join("state.json"),
            serde_json::to_string(&json!({
                "title":"t","agents":{"main":{"type":"main"},"agent-0":{"type":"sub","parentAgentId":"main"}}})).unwrap(),
        )
        .unwrap();
        // session_index.jsonl：workDir 是项目归属的唯一来源
        std::fs::write(
            root.join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&json!({"sessionId":"session_1","sessionDir":"sessions/wd_p/session_1","workDir":"/p/CodexPlusPlus"})).unwrap()
            ),
        )
        .unwrap();
    }

    /// 采集一轮（`KIMI_CODE_HOME` 读取放在采集器内部，调用方负责设置/清理该 env）
    fn run(
        home: &std::path::Path,
        cursors: &HashMap<String, crate::services::usage::delta::CursorDelta>,
    ) -> (
        crate::services::usage::delta::SourceDelta,
        crate::services::usage::collect::CollectStats,
    ) {
        let ctx = CollectContext::new(home, 1_700_000_000_000, cursors);
        let delta = KimiCollector.collect(&ctx).unwrap();
        (delta, ctx.stats())
    }

    /// 「设置 `KIMI_CODE_HOME` → 采集 → 清理」三段必须在**共用锁**内完成。
    ///
    /// 为什么必须有锁（实测，不是理论推导）：`KIMI_CODE_HOME` 是**进程级** env，而本模块的用例与
    /// `monitor::kimi_parser` 的解析器用例跑在**同一个 lib 测试二进制**里、默认并行。
    /// 第一版照任务书写（无锁）时 `globs_all_agent_dirs_and_counts_turns_by_turnid` 实测拿到
    /// 隔壁用例的 fixture（`output` 合计 30 而非 1020）。两处共用
    /// `crate::monitor::kimi_parser::HOME_LOCK`（Task 13 起提到模块级 `pub(crate)`）。
    /// 中毒处理：断言失败会让持锁用例 panic → 若用 `.unwrap()` 会把锁毒化、把**无关用例**
    /// 连带成 PoisonError 假红；这里取回内部值继续，失败仍然只由真正失败的那条报出。
    fn collect_with_env(
        home: &std::path::Path,
        cursors: &HashMap<String, crate::services::usage::delta::CursorDelta>,
    ) -> (
        crate::services::usage::delta::SourceDelta,
        crate::services::usage::collect::CollectStats,
    ) {
        let _guard = crate::monitor::kimi_parser::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("KIMI_CODE_HOME", home);
        let out = run(home, cursors);
        std::env::remove_var("KIMI_CODE_HOME");
        out
    }

    #[test]
    fn globs_all_agent_dirs_and_counts_turns_by_turnid() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let (delta, stats) = collect_with_env(dir.path(), &no_cursors);
        assert_eq!(stats.repeat_reads, 0);
        // 只读 main 会少算 21.6% 的 output —— 全量 output = 784 + 216 + 20（第二个 step 的 10+10）
        let out: i64 = delta.details.iter().map(|d| d.buckets.output).sum();
        assert_eq!(out, 1020);
        // 子代理用量与主会话共用同一 index 条目 → 全部归到 session_1（这正是"必须 glob 全部
        // agents/*/"的收益：漏掉 agents/agent-0 就会变成 784+20 = 804，少 21.2%）
        let by_session: i64 = delta
            .details
            .iter()
            .filter(|d| d.key.session_id == "session_1")
            .map(|d| d.buckets.output)
            .sum();
        assert_eq!(by_session, 1020);
        assert!(
            (216.0f64 / 1000.0 - 0.216).abs() < 1e-9,
            "矩阵实测：子代理占 output 21.6%"
        );
        // turn 数按 turnId 去重：每个 agent 目录各 1 个 turn，共 2（**不是**按 usage.record 数 4）
        let turns: i64 = delta.details.iter().map(|d| d.counters.turns).sum();
        assert_eq!(
            turns, 2,
            "usageScope=='turn' 是 per-LLM-step（虚高 9.14× 的陷阱）"
        );
        // 工具配对 100%
        let calls: i64 = delta.details.iter().map(|d| d.counters.tool_calls).sum();
        let ms: i64 = delta.details.iter().map(|d| d.counters.tool_ms).sum();
        assert_eq!(calls, 2, "main + 子代理各一次");
        assert_eq!(
            ms, 100,
            "50ms × 2（tool.result.parentUuid → tool.call.uuid）"
        );
        // 供应商来自模型名前缀（实测）
        let d0 = delta
            .details
            .iter()
            .find(|d| d.key.provider == "ollama-cloud")
            .unwrap();
        assert_eq!(d0.key.model, "glm-5.3-flash");
        // D21：项目键 = 小写 projectName（workDir 末段）
        let s = &delta.sessions[0];
        assert_eq!(s.project_key, "codexplusplus");
        assert_eq!(s.project_label, "CodexPlusPlus");
        assert!(
            !s.is_subagent,
            "子代理与主会话同 session_id → 会话不分层（turns 含子代理）"
        );
    }

    /// **B5 回归**：两个**同名** `wire.jsonl` 必须各自独立成游标行。
    /// 游标键若取文件 stem，两个文件都叫 "wire"（真机 82 个文件全叫 `wire.jsonl`）→
    /// `usage_cursor` 主键 `(source_id, session_id)` 上只剩 1 行（后写覆盖先写），
    /// 另一个文件每轮尾指纹不符 → `rescan = true` → 整文件重读重算并累加。
    #[test]
    fn same_named_wire_files_get_independent_cursors() {
        let dir = tempfile::tempdir().unwrap();
        // 两个会话目录，各自 `agents/main/wire.jsonl`（同名）+ 各自 index 条目
        for (n, out) in [("session_a", 10i64), ("session_b", 20)] {
            let d = dir.path().join(format!("sessions/wd_p/{n}/agents/main"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("wire.jsonl"),
                format!(
                    "{}\n",
                    serde_json::to_string(&json!({
                        "type":"usage.record","model":"ollama-cloud/glm-5.3-flash",
                        "usageScope":"turn",
                        "usage":{"inputOther":1,"output":out,"inputCacheRead":0,"inputCacheCreation":0},
                        "time":1782301061159i64}))
                    .unwrap()
                ),
            )
            .unwrap();
            std::fs::write(
                dir.path().join(format!("sessions/wd_p/{n}/state.json")),
                serde_json::to_string(&json!({"title":"t","agents":{"main":{"type":"main"}}}))
                    .unwrap(),
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&json!({"sessionId":"session_a","sessionDir":"sessions/wd_p/session_a","workDir":"/p/A"})).unwrap(),
                serde_json::to_string(&json!({"sessionId":"session_b","sessionDir":"sessions/wd_p/session_b","workDir":"/p/B"})).unwrap()
            ),
        )
        .unwrap();

        let (d1, _) = collect_with_env(dir.path(), &HashMap::new());
        assert_eq!(
            d1.cursors.len(),
            2,
            "两个同名 wire 必须两条游标（旧实现只有一条 'wire'）"
        );
        let keys: std::collections::BTreeSet<&str> =
            d1.cursors.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(keys.len(), 2, "两个游标键必须互不相同：{keys:?}");
        assert!(
            keys.iter().all(|k| k.contains("wire.jsonl")),
            "键是路径派生、可读：{keys:?}"
        );

        // 第二轮：两份游标都在 → 两个文件都不重算
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = collect_with_env(dir.path(), &cursors);
        assert_eq!(
            d2.new_records, 0,
            "第二轮不得有任何新增（旧实现会整文件重读）"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.buckets.output).sum::<i64>(),
            0,
            "不得重复计入"
        );
    }

    // ---- 以下 4 条是**真机语料对齐**补的回归锁（任务书代码块在这 4 处与真机形态不符，
    // ---- 错了不编译报错、只静默错数；每条都先跑出真红再实现，见报告「偏差申报」） ----

    /// 一行 wire 记录（与既有用例同形：走 serde_json 序列化，避免手拼转义）
    fn line(v: serde_json::Value) -> String {
        format!("{}\n", serde_json::to_string(&v).unwrap())
    }

    /// 单会话单 agent 目录的 fixture（`root/sessions/wd_p/<sid>/agents/main/wire.jsonl`）
    fn one_session(root: &std::path::Path, sid: &str, work_dir: &str, body: &str) {
        let d = root.join(format!("sessions/wd_p/{sid}/agents/main"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("wire.jsonl"), body).unwrap();
        std::fs::write(
            root.join(format!("sessions/wd_p/{sid}/state.json")),
            serde_json::to_string(&json!({"title":"t","agents":{"main":{"type":"main"}}})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(
                    &json!({"sessionId":sid,"sessionDir":format!("sessions/wd_p/{sid}"),"workDir":work_dir})
                )
                .unwrap()
            ),
        )
        .unwrap();
    }

    /// **真机形态（本机 82 文件 / 2,574 条）**：`tool.call` / `tool.result` **嵌在**
    /// `context.append_loop_event.event` 下（顶层只有 `usage.record` / `turn.prompt` /
    /// `turn.ended` 等）；行级 `time` 在外层。
    /// 任务书代码块只匹配**顶层** `"tool.call"` → 真机 `tool_calls` 恒为 **0**（静默少采 100%），
    /// 而 fixture 恰好把 tool.call 写成顶层（真机一条都没有）→ 假绿。
    ///
    /// 同一用例顺带锁住**模型继承**：嵌套事件里没有 `model` 字段，若不把最近一次
    /// `usage.record` 的模型带下去，turn/tool 计数会全部落到 `model=""` / `provider=""`
    /// 的独立明细行上——按模型/供应商分组时，回合数与工具调用**整块消失**
    /// （真机 12,317 条 loop 事件；且 368 条事件出现在本文件首条 usage.record 之前，见报告）。
    #[test]
    fn nested_loop_events_carry_tools_turns_and_durations() {
        let dir = tempfile::tempdir().unwrap();
        let body = [
            line(
                json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                "usageScope":"turn",
                "usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                "time":1782301060000i64}),
            ),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},
                "time":1782301061000i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.call","uuid":"Read_0","toolCallId":"Read_0",
                    "turnId":"0","step":1,"name":"Read","args":{}},
                "time":1782301062000i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.result","parentUuid":"Read_0","toolCallId":"Read_0",
                    "result":{"output":"x"}},
                "time":1782301062050i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.call","uuid":"Bash_1","toolCallId":"Bash_1",
                    "turnId":"0","step":1,"name":"Bash"},
                "time":1782301062100i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.result","parentUuid":"Bash_1",
                    "result":{"output":"boom","isError":true}},
                "time":1782301062200i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,
                    "finishReason":"end_turn"},
                "time":1782301067000i64})),
        ]
        .concat();
        one_session(dir.path(), "session_n", "/p/N", &body);

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        // **前提断言（防假绿）**：usage.record 那行必须真的成行，后面的断言才有对象
        let row = delta
            .details
            .iter()
            .find(|d| d.key.provider == "opencode-go")
            .expect("usage.record 必须成行（前提不成立时后续断言全是假绿）");
        assert_eq!(row.key.model, "kimi-k2.7-code");
        assert_eq!(
            row.counters.tool_calls, 2,
            "嵌套 tool.call 必须被采（真机 2,574 条全在 event 下）"
        );
        assert_eq!(row.counters.tool_ms, 150, "Read 50ms + Bash 100ms");
        assert_eq!(row.counters.tool_stats.get("Read"), Some(&(1, 50)));
        assert_eq!(row.counters.error_tool, 1, "result.isError 也在嵌套形态里");
        assert_eq!(
            row.counters.turns, 1,
            "turnId 去重（真机 main 201 / sub 41）"
        );
        assert_eq!(
            row.counters.turn_ms,
            vec![6000],
            "缺原生 durationMs 时按 loop 事件的 time 推导（首个 step.begin → 终结 step.end）"
        );
        // **注意**：这里原来还有一条 `all(|d| !model.is_empty() && !provider.is_empty())`——
        // 它在本 fixture 上是**恒真**的（`usage.record` 写在**首行**），而真机排序恰恰相反
        // → 锁不住「首条 usage.record 之前的事件」那个洞（评审裁决 A 点名）。该断言已搬到
        // `events_before_the_first_usage_record_are_attributed_to_that_model`（那里才是真锁）。
    }

    /// **原生时长读对了地方 + 不与推导值双计**。
    /// 真机：`turn.ended.durationMs` 在**顶层**（11 条 / 覆盖率 5.5% / max 147,556 ms），
    /// 而任务书代码块读的是 `/event/durationMs`（嵌套对象里没有该字段）→ 一条都采不到；
    /// 且真机 `turn.ended.turnId` 是**数字** `0`，而 `append_loop_event.event.turnId` 是
    /// **字符串** `"0"` —— 只认字符串会让原生值与推导值对不上同一个 turn。
    #[test]
    fn native_turn_duration_is_read_from_the_right_place_and_not_double_counted() {
        let dir = tempfile::tempdir().unwrap();
        let body = [
            line(
                json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                "usageScope":"turn",
                "usage":{"inputOther":1,"output":1,"inputCacheRead":0,"inputCacheCreation":0},
                "time":1782301060000i64}),
            ),
            // turn 0：只有原生时长（真机形态：durationMs 在顶层、turnId 是数字）
            line(json!({"type":"turn.ended","agentId":"main","turnId":0,
                "reason":"completed","durationMs":2802i64,"time":1782301065000i64})),
            // turn 1：无原生字段 → 靠 step.begin/step.end 推导
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.begin","uuid":"s2","turnId":"1","step":1},
                "time":1782301070000i64})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.end","uuid":"s2","turnId":"1","step":1,
                    "finishReason":"end_turn"},
                "time":1782301076000i64})),
        ]
        .concat();
        one_session(dir.path(), "session_d", "/p/D", &body);

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        let row = delta
            .details
            .iter()
            .find(|d| d.key.provider == "opencode-go")
            .expect("usage.record 必须成行");
        let mut ms = row.counters.turn_ms.clone();
        ms.sort_unstable();
        assert_eq!(
            ms,
            vec![2802, 6000],
            "原生 2,802（顶层 durationMs、数字 turnId）与推导 6,000 各一条，**不得双计**"
        );
    }

    /// **R1 / 说明书 §4.3**：kimi 属于「能读到用户文本」的三源之一（claude / codex / kimi），
    /// 「用户输入(估)」在**明细档与日档都必须出真值**（GC 18-R1）。文本在顶层
    /// `turn.prompt.input[].text`（真机 242 条 = 每 turn 一条，与 turnId 去重后的 turn 数相等）。
    /// 任务书代码块**完全没有这一支** → kimi 的 `user_est` 永久为 `NULL`，且没有任何用例会红。
    #[test]
    fn user_est_comes_from_turn_prompt() {
        // 前提断言（防假绿）：估算函数对这段文本真的 > 0，否则下面的等式退化成「都是 0」
        assert!(
            crate::services::usage::semantics::user_est_of("你好 world") > 0,
            "前提：CJK 每字 1 + ASCII 每词 1"
        );
        let dir = tempfile::tempdir().unwrap();
        let body = [
            line(
                json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                "usageScope":"turn",
                "usage":{"inputOther":1,"output":1,"inputCacheRead":0,"inputCacheCreation":0},
                "time":1782301060000i64}),
            ),
            line(json!({"type":"turn.prompt",
                "input":[{"type":"text","text":"你好 world"}],"time":1782301060001i64})),
        ]
        .concat();
        one_session(dir.path(), "session_u", "/p/U", &body);

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        let row = delta
            .details
            .iter()
            .find(|d| d.key.provider == "opencode-go")
            .expect("usage.record 必须成行");
        assert_eq!(
            row.user_est,
            Some(crate::services::usage::semantics::user_est_of("你好 world")),
            "kimi 的「用户输入(估)」必须由 turn.prompt 文本现算（现算=宿主/口径无关）"
        );
    }

    /// **W-06 / D-24（W-42 同族）**：单文件读失败**只有**「文件在枚举与读取之间消失」
    /// 才允许跳过；其余一律**整源响亮失败** `usage-source-io`（落进
    /// `UsageSourceStatus.errorCode`，Task 24 的验收面看得见），不得 `log::warn!` 后静默少算。
    /// 这里用**目录冒充 `wire.jsonl`**：`exists()` 为真、`std::fs::read` 必失败且
    /// `ErrorKind != NotFound` —— 跨平台、确定性的「读不了」样本（不依赖竞态窗口）。
    #[test]
    fn unreadable_wire_file_fails_the_whole_source_loudly() {
        let err = {
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path().join("sessions/wd_p/session_x/agents/main");
            std::fs::create_dir_all(d.join("wire.jsonl")).unwrap(); // 目录冒充文件
            std::fs::write(
                dir.path().join("session_index.jsonl"),
                format!(
                    "{}\n",
                    serde_json::to_string(&json!({"sessionId":"session_x",
                        "sessionDir":"sessions/wd_p/session_x","workDir":"/p/X"}))
                    .unwrap()
                ),
            )
            .unwrap();
            let no_cursors = HashMap::new();
            let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
            let _guard = crate::monitor::kimi_parser::HOME_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("KIMI_CODE_HOME", dir.path());
            let r = KimiCollector.collect(&ctx);
            std::env::remove_var("KIMI_CODE_HOME");
            r.expect_err("读不了的文件必须整源失败，不得静默跳过")
        };
        assert_eq!(
            err.code, "usage-source-io",
            "错误码必须落进 UsageSourceStatus.errorCode（细节：{}）",
            err.detail
        );
    }

    /// **裁决 A 的回归锁（**真机排序**）**：`turn.prompt` / `step.begin` / `tool.call` 排在
    /// 本文件**首条 `usage.record` 之前**——真机 61 个文件里 368 条 turn/tool 事件、65/242 条
    /// `turn.prompt` 如此，近半「用户输入(估)」(27,694/58,646 = **47.2%**) 会落到空模型行上
    /// （`provider.rs:92-93` 点名要避免的「明细行 model 与 provider 双空」= D9 分组退化）。
    /// 修法：模型未知期间**按行缓存**，解析出模型后原序重放（`handle_line` / `replay_pending`）。
    #[test]
    fn events_before_the_first_usage_record_are_attributed_to_that_model() {
        let dir = tempfile::tempdir().unwrap();
        let t = 1_782_301_060_000i64;
        let body = [
            // 前三条都排在首条 usage.record **之前**（模型此刻未知）
            line(
                json!({"type":"turn.prompt","input":[{"type":"text","text":"你好 world"}],
                "time":t + 10}),
            ),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},
                "time":t + 100})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.call","uuid":"Read_0","toolCallId":"Read_0",
                    "turnId":"0","step":1,"name":"Read"},
                "time":t + 200})),
            line(
                json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                "usageScope":"turn",
                "usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                "time":t + 300}),
            ),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.result","parentUuid":"Read_0","result":{"output":"x"}},
                "time":t + 400})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,
                    "finishReason":"end_turn"},
                "time":t + 900})),
        ]
        .concat();
        one_session(dir.path(), "session_pre", "/p/Pre", &body);

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        // **前提断言（防假绿）**：usage.record 必须成行，否则后面全是空转
        let row = delta
            .details
            .iter()
            .find(|d| d.key.provider == "opencode-go")
            .expect("usage.record 必须成行（前提不成立时后续断言全是假绿）");
        // **真锁**：明细里不得出现空 model / 空 provider 行（真机排序下才会咬）
        assert!(
            delta
                .details
                .iter()
                .all(|d| !d.key.model.is_empty() && !d.key.provider.is_empty()),
            "首条 usage.record 之前的事件不得另立空模型行：{:?}",
            delta
                .details
                .iter()
                .map(|d| (&d.key.model, &d.key.provider))
                .collect::<Vec<_>>()
        );
        // 缓存的三条事件必须按**解析出的模型**归行
        assert_eq!(
            row.user_est,
            Some(crate::services::usage::semantics::user_est_of("你好 world")),
            "turn.prompt 在 usage.record 之前也要落对行（真机 47.2% 的 user_est 走这条路）"
        );
        assert_eq!(row.counters.turns, 1, "step.begin 的 turn 计数不得丢");
        assert_eq!(row.counters.tool_calls, 1, "tool.call 不得丢");
        assert_eq!(row.counters.tool_ms, 200, "回执在同一文件稍后到达");
        assert_eq!(row.counters.turn_ms, vec![800], "时长推导同样要认缓存行");
    }

    /// **裁决 A（fix round 2）：本文件模型必须优先于「本轮首见模型」**。
    ///
    /// 触发条件（复审按代码路径穷举、真机已命中处理序第 72 个文件
    /// `wd_financialart…/session_cba38477…`：13 行，唯一 `usage.record` 在**末行**）：
    /// 文件本轮开始时 `state.model` 为空，而它首条带 model 的 `usage.record` **恰是本轮读到的最后一行**
    /// → 行循环里没机会 inline 重放 → 走到收尾结算时 `state.model` **已经是本文件自己的模型**。
    /// 此时若直接 `match round_model`（不看 `state.model`），就会被**另一个文件的**模型覆盖，
    /// 并把错模型写进游标 `state_json` 供后续轮次沿用。
    #[test]
    fn settlement_prefers_the_files_own_model_over_the_round_model() {
        let dir = tempfile::tempdir().unwrap();
        let t = 1_782_301_060_000i64;
        // 索引第 1 行：模型 A（它会把 `round_model` 占住）
        let a = dir.path().join("sessions/wd_p/session_first/agents/main");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            a.join("wire.jsonl"),
            line(json!({"type":"usage.record","model":"opencode-go/model-a",
                "usageScope":"turn",
                "usage":{"inputOther":1,"output":1,"inputCacheRead":0,"inputCacheCreation":0},
                "time":t})),
        )
        .unwrap();
        // 索引第 2 行：**record 在最后一行**、且前面有两条需要模型的事件
        let b = dir.path().join("sessions/wd_p/session_last/agents/main");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join("wire.jsonl"),
            [
                line(json!({"type":"turn.prompt","input":[{"type":"text","text":"你好 world"}],
                    "time":t + 10})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"tool.call","uuid":"Read_0","turnId":"0","name":"Read"},
                    "time":t + 20})),
                line(json!({"type":"usage.record","model":"ollama-cloud/model-b",
                    "usageScope":"turn",
                    "usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                    "time":t + 30})),
            ]
            .concat(),
        )
        .unwrap();
        for sid in ["session_first", "session_last"] {
            std::fs::write(
                dir.path().join(format!("sessions/wd_p/{sid}/state.json")),
                serde_json::to_string(&json!({"title":"t","agents":{"main":{"type":"main"}}}))
                    .unwrap(),
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&json!({"sessionId":"session_first",
                    "sessionDir":"sessions/wd_p/session_first","workDir":"/p/First"}))
                .unwrap(),
                serde_json::to_string(&json!({"sessionId":"session_last",
                    "sessionDir":"sessions/wd_p/session_last","workDir":"/p/Last"}))
                .unwrap()
            ),
        )
        .unwrap();

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        // 前提断言（防假绿）：第 1 行文件的模型必须真的占住了 `round_model`
        assert!(
            delta
                .details
                .iter()
                .any(|d| d.key.project_key == "first" && d.key.provider == "opencode-go"),
            "前提：索引第 1 行文件的模型必须先被解析出来"
        );
        // 末行 record 的那两条事件必须落在**本文件自己的模型 B** 上，而不是被 A 覆盖。
        // 先钉住「只许一行」：被 round_model 覆盖时，两条缓存事件会另立一行（真机的 M0 假行）
        let last_rows: Vec<(&str, &str)> = delta
            .details
            .iter()
            .filter(|d| d.key.project_key == "last")
            .map(|d| (d.key.provider.as_str(), d.key.model.as_str()))
            .collect();
        assert_eq!(
            last_rows,
            vec![("ollama-cloud", "model-b")],
            "本文件只应有一行、且是本文件自己的模型（多出来的就是被 round_model 覆盖出的假行）"
        );
        let last = delta
            .details
            .iter()
            .find(|d| d.key.project_key == "last")
            .expect("末行 record 的文件也必须成行");
        assert_eq!(
            (last.key.provider.as_str(), last.key.model.as_str()),
            ("ollama-cloud", "model-b"),
            "本文件模型优先！被 round_model 覆盖 = 真机 wd_financialart 那条 M0 假行"
        );
        assert_eq!(
            last.user_est,
            Some(crate::services::usage::semantics::user_est_of("你好 world")),
            "末行 record 之前的 turn.prompt 必须按**本文件**模型归行"
        );
        assert_eq!(last.counters.tool_calls, 1);
        assert!(
            delta
                .details
                .iter()
                .all(|d| !d.key.model.is_empty() && !d.key.provider.is_empty()),
            "任何情况下都不得出现空模型行"
        );
    }

    /// **裁决 A 的跨文件兜底（真机排序）**：真机有 **23/82** 个文件**通篇没有** `usage.record`
    /// （限流 / 被取消的空会话：只有 `turn.step.retrying` / `turn.ended` / `turn.prompt` / loop 事件）。
    /// 它们的计数**不能丢**——矩阵的 `error_model 9` / `error_turn 2` / `turns 242` 都含它们
    /// ——也**不能**写 `model=""` 行 → 用**本轮首见模型**兜底（裁决 A 的括号项）。
    ///
    /// 采集器按 `session_index.jsonl` 顺序 × `agents/*` 目录名序读文件；真机第 1 个文件就带
    /// model（宿主已用脚本核过：首个含 `usage.record` 的文件位于处理序第 0 位），故 23 个无模型
    /// 文件全都有兜底。**残留（登记）**：若某个无模型文件排在**所有**带模型文件之前，它仍会走
    /// 「丢 + warn」——真机 0 个文件命中。
    #[test]
    fn model_less_files_are_attributed_to_the_rounds_first_seen_model() {
        let dir = tempfile::tempdir().unwrap();
        let t = 1_782_301_060_000i64;
        // 会话 A（索引第 1 行）：带 model
        let a = dir.path().join("sessions/wd_p/session_alpha/agents/main");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            a.join("wire.jsonl"),
            line(
                json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                "usageScope":"turn",
                "usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                "time":t}),
            ),
        )
        .unwrap();
        // 会话 B（索引第 2 行）：**通篇没有** usage.record —— 真机 23 个这样的文件
        let b = dir.path().join("sessions/wd_p/session_beta/agents/main");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join("wire.jsonl"),
            [
                line(
                    json!({"type":"turn.step.retrying","agentId":"main","turnId":0,"step":1,
                    "errorName":"APIProviderRateLimitError","time":t + 10}),
                ),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},
                    "time":t + 100})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"tool.call","uuid":"Read_0","turnId":"0","name":"Read"},
                    "time":t + 200})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"tool.result","parentUuid":"Read_0","result":{"output":"x"}},
                    "time":t + 250})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,
                        "finishReason":"end_turn"},
                    "time":t + 900})),
            ]
            .concat(),
        )
        .unwrap();
        for sid in ["session_alpha", "session_beta"] {
            std::fs::write(
                dir.path().join(format!("sessions/wd_p/{sid}/state.json")),
                serde_json::to_string(&json!({"title":"t","agents":{"main":{"type":"main"}}}))
                    .unwrap(),
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&json!({"sessionId":"session_alpha",
                    "sessionDir":"sessions/wd_p/session_alpha","workDir":"/p/Alpha"}))
                .unwrap(),
                serde_json::to_string(&json!({"sessionId":"session_beta",
                    "sessionDir":"sessions/wd_p/session_beta","workDir":"/p/Beta"}))
                .unwrap()
            ),
        )
        .unwrap();

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        // 前提断言（防假绿）：A 的 usage 行必须成行（兜底模型就是它）
        assert!(
            delta
                .details
                .iter()
                .any(|d| d.key.project_key == "alpha" && d.key.provider == "opencode-go"),
            "前提：会话 A 的 usage.record 必须先成行"
        );
        assert!(
            delta
                .details
                .iter()
                .all(|d| !d.key.model.is_empty() && !d.key.provider.is_empty()),
            "任何情况下都不得出现空模型行：{:?}",
            delta
                .details
                .iter()
                .map(|d| (&d.key.project_key, &d.key.model, &d.key.provider))
                .collect::<Vec<_>>()
        );
        // 会话 B（无模型）的计数必须落在**本轮首见模型**上，而不是被丢掉
        let beta = delta
            .details
            .iter()
            .find(|d| d.key.project_key == "beta")
            .expect(
                "无模型文件的事件必须兜底成行（丢了就是矩阵口径少算 error_model 9 / turns 242）",
            );
        assert_eq!(beta.key.model, "kimi-k2.7-code");
        assert_eq!(beta.key.provider, "opencode-go");
        assert_eq!(beta.counters.error_model, 1, "turn.step.retrying 不得丢");
        assert_eq!(beta.counters.turns, 1);
        assert_eq!(beta.counters.tool_calls, 1);
        assert_eq!(beta.counters.tool_ms, 50);
        assert_eq!(beta.counters.turn_ms, vec![800]);
        // 会话维度：两个 index 条目 → 两条会话（登记已外提到 agent 目录循环之外）
        assert_eq!(delta.sessions.len(), 2, "一个 index 条目 = 一个会话");
    }

    /// **裁决 A 的丢弃路径**：整轮都拿不到模型（文件里根本没有 `usage.record`）→
    /// `log::warn!` + 计数 + **显式丢弃**，绝不再写 `model=""` 行。
    /// （这是明批裁决：宁可丢 + 留信号，也不让「按供应商/模型分组」的视图多出一行空模型。）
    #[test]
    fn events_without_any_usage_record_are_dropped_not_written_as_blank_rows() {
        let dir = tempfile::tempdir().unwrap();
        let t = 1_782_301_060_000i64;
        let body = [
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},
                "time":t + 100})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"tool.call","uuid":"Read_0","turnId":"0","name":"Read"},
                "time":t + 200})),
            line(json!({"type":"context.append_loop_event",
                "event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,
                    "finishReason":"end_turn"},
                "time":t + 900})),
        ]
        .concat();
        one_session(dir.path(), "session_nomodel", "/p/NoModel", &body);

        let (delta, _) = collect_with_env(dir.path(), &HashMap::new());
        assert!(
            delta.details.is_empty(),
            "模型不可知的整轮事件必须**丢弃**，不得写成空模型行：{:?}",
            delta
                .details
                .iter()
                .map(|d| (&d.key.model, &d.key.provider))
                .collect::<Vec<_>>()
        );
        // 前提断言：游标照常推进（否则下一轮会重读同一段字节 —— 丢弃与「卡住水位」是两件事）
        assert_eq!(delta.cursors.len(), 1, "水位必须照常推进");
    }

    /// **裁决 B 的回归锁：跨轮 `state_json` 续读**。
    ///
    /// 原先唯一的「两轮」用例第二轮**磁盘内容未变** → `read_incremental` 走快速路
    /// （`file_unchanged`，`stream.rs`）→ 采集器在 `lines.is_empty() && !rescan` 处提前 `continue`
    /// → `state_json` **既没被解析、也没被写回**：`turn_ids` 跨轮去重 / `PendingTools` 跨轮回执
    /// 配对 / `open_turns` 跨轮时长推导 —— **三条 W-11 纪律一次都没跑过**（任一处失效都是静默错数）。
    /// 本用例：R1 写「已知模型 + 未配对的 tool.call + 未闭合的 turn 0」，R2 **追加**回执与终结事件。
    #[test]
    fn two_round_append_resumes_state_instead_of_recounting() {
        let dir = tempfile::tempdir().unwrap();
        let t = 1_782_301_060_000i64;
        let d = dir.path().join("sessions/wd_p/session_r/agents/main");
        std::fs::create_dir_all(&d).unwrap();
        let wire = d.join("wire.jsonl");
        // R1
        std::fs::write(
            &wire,
            [
                line(json!({"type":"usage.record","model":"opencode-go/kimi-k2.7-code",
                    "usageScope":"turn",
                    "usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},
                    "time":t})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"tool.call","uuid":"Read_0","turnId":"0","name":"Read"},
                    "time":t + 100})),
                line(json!({"type":"context.append_loop_event",
                    "event":{"type":"step.begin","uuid":"s1","turnId":"0","step":1},
                    "time":t + 200})),
            ]
            .concat(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sessions/wd_p/session_r/state.json"),
            serde_json::to_string(&json!({"title":"t","agents":{"main":{"type":"main"}}})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("session_index.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&json!({"sessionId":"session_r",
                    "sessionDir":"sessions/wd_p/session_r","workDir":"/p/R"}))
                .unwrap()
            ),
        )
        .unwrap();

        let (d1, _) = collect_with_env(dir.path(), &HashMap::new());
        assert_eq!(d1.cursors.len(), 1, "前提：R1 必须产出游标");
        // **前提断言（防假绿）**：R1 里回执还没到、turn 还没闭合 —— 否则本用例证明不了跨轮
        assert_eq!(
            d1.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            0,
            "前提：R1 的回执未到（tool_ms 必须为 0）"
        );
        assert_eq!(
            d1.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            1,
            "前提：R1 计了 1 个 turn"
        );
        assert_eq!(
            d1.details
                .iter()
                .map(|d| d.counters.turn_ms.len())
                .sum::<usize>(),
            0,
            "前提：R1 的 turn 未闭合 → 无时长样本"
        );

        // R2：**追加**（磁盘内容变了 → 走追加路，不是快速路）
        let mut body = std::fs::read_to_string(&wire).unwrap();
        body.push_str(&line(json!({"type":"context.append_loop_event",
            "event":{"type":"tool.result","parentUuid":"Read_0","result":{"output":"x"}},
            "time":t + 250})));
        body.push_str(&line(json!({"type":"context.append_loop_event",
            "event":{"type":"step.end","uuid":"s1","turnId":"0","step":1,
                "finishReason":"end_turn"},
            "time":t + 900})));
        // 复审 Low #4：批准文本还要求「**下一 turn**」——验证新 turnId 在跨轮恢复后仍能被计入
        body.push_str(&line(json!({"type":"context.append_loop_event",
            "event":{"type":"step.begin","uuid":"s2","turnId":"1","step":1},
            "time":t + 1000})));
        std::fs::write(&wire, body).unwrap();

        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = collect_with_env(dir.path(), &cursors);
        assert!(
            d2.new_records > 0,
            "前提：R2 必须走**追加路**（快速路下 state 续读根本没跑，本用例就没有意义）"
        );
        // ① 跨轮回执配对（`PendingTools` 经 state_json 带过来）
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            150,
            "跨轮到达的 tool.result 必须配上上一轮的 tool.call"
        );
        // ② 同一 turnId 不得跨轮重复计；R2 只该计入**新出现的** turnId `"1"`（= 1 个）
        //    （若 turn 0 被重计，这里会是 2 —— 跨轮去重失效 = 静默多算）
        let r2_turns: i64 = d2.details.iter().map(|d| d.counters.turns).sum();
        assert_eq!(
            r2_turns, 1,
            "R2 只许计入新 turnId `1`；2 = turn 0 被跨轮重复计"
        );
        let r1_turns: i64 = d1.details.iter().map(|d| d.counters.turns).sum();
        assert_eq!(
            r1_turns + r2_turns,
            2,
            "两轮合计 = 去重后的 turnId 数（0 与 1 各一次）"
        );
        // ③ 时长样本：R1 的 step.begin → R2 的终结 step.end，**只出一条**
        let ms: Vec<i64> = d2
            .details
            .iter()
            .flat_map(|d| d.counters.turn_ms.clone())
            .collect();
        assert_eq!(ms, vec![700], "跨轮推导必须成立且只出一条");
        // ④ 写回的 `state_json` 必须能反序列化，且跨轮状态已收敛（读/写两侧都锁）
        let c = d2
            .cursors
            .iter()
            .find(|c| c.session_id.contains("wire.jsonl"))
            .expect("R2 必须回写游标");
        let st: KimiFileState =
            serde_json::from_str(&c.state_json).expect("游标 state_json 必须能反序列化");
        assert!(st.tools.open.is_empty(), "回执已配对 → 未配对表必须清空");
        assert_eq!(
            st.open_turns.keys().cloned().collect::<Vec<_>>(),
            vec!["1".to_string()],
            "turn 0 已结算应摘除；刚开的 turn 1 留在表里等下一轮"
        );
        assert!(
            st.turn_ids.contains(&"0".to_string()) && st.turn_ids.contains(&"1".to_string()),
            "turnIds 必须跨轮保留（否则下一轮重复计）"
        );
    }

    /// **登记锁（GC 20 / §3.2.2 阻塞 4）**：漏登记不是编译错——`run_collection`
    /// 只遍历 `collectors::all()`，这个源会被**静默跳过**（UI 上就是「kimi 一直没有用量」）。
    #[test]
    fn collector_is_registered_in_all() {
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::Kimi),
            "kimi 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
