//! Kimi 原生 TPS 读取器（Spec §4 Kimi 行；证据：dsh-kimi.md + native-timing-addendum.md §4）。
//!
//! ## 读取对象与口径
//!
//! * 数据根复用 `monitor::kimi_parser::resolve_data_root(None, home)`（**刻意传 None**：
//!   不认 `KIMI_CODE_HOME` 环境覆盖——TPS 读取必须是入参 home 的纯函数，测试传 tempdir
//!   即天然隔离；与 Task 3 dsh/zcode 读取器同一纪律）。枚举走 `session_index.jsonl` 条目
//!   × `agents/*/wire.jsonl`（**必须 glob 全部 agent 目录**，只读 main 会系统性漏采，
//!   usage/kimi 采集器的实测教训）。
//! * **同 step 原生对齐**（dsh-kimi.md「关键证据」）：嵌套
//!   `context.append_loop_event.event` 的 `step.end` 自带 `usage.output` +
//!   `llmFirstTokenLatencyMs`（请求起点→首流片段）+ `llmStreamDurationMs`（首流片段→流结束）
//!   ——三者同事件，**不借相邻 `usage.record` 猜配**（2395 条顶层 usage.record 全无
//!   turnId，相邻配对存在未配对与不一致）。行级 `time` 在外层，事件在 `event` 下。
//! * 计时锚定：`ended = 行级事件时刻`；`native_duration = ttft + stream`（dsh-kimi.md
//!   指标表「模型累计用时 = sum(TTFT + streamDuration)」，两个原生时长相邻，stream 已排除
//!   首点等待，不再减 TTFT）；`started = ended − native`；`first_token = ended − stream`。
//!   生成窗 = `llmStreamDurationMs`，**正即有效**（806ms 合法，Spec §3 不设全局 1s 下限；
//!   supplement-assessment「Windows 把 806/776ms 原生生成窗按统一 1 秒阈值丢掉没有依据」）。
//! * 响应窗（V2 Spec §5.2「含等待响应」）= 同事件 `llmFirstTokenLatencyMs +
//!   llmStreamDurationMs`（请求起点→首片段→流结束的完整含等待窗，等待只计一次，绝不
//!   二次相加）；与整调用时长同一套对齐判据（重试不可对齐不填），失败不入响应速度。
//! * 模型归属（addendum §7.1）：同文件、同 `turnStep`（`turnId.step`）且**无歧义**对应的
//!   `llm.request.model`；旧形态无 request → 显式未知桶 `"unknown"`（usage+timing 仍同条
//!   对齐，速度照入）；同 turnStep 多条 request（重试）→ 保留全部尝试、**禁止 map 覆盖**，
//!   无法对齐哪次请求产出了该 step 的 usage → 剔除速度 + 模型进未知桶 + 诊断。
//! * 输出语义：kimi usage **无独立 reasoning 字段**，output 是否含思考未单独声明 →
//!   `ProviderReported`（留元数据，不按 think 文本另加估算、不宣称测得隐藏思考，Spec §3）。
//! * 失败/取消/缺失跳过：`finishReason=error` → Failed（不入速度，可靠起止保留）；
//!   缺 usage → output=None 记缺失不报 0；`llmStreamDurationMs<=0`（含 0 的整块突发）
//!   → 无生成窗；缺原生计时/uuid/turnId → 整条跳过 + 诊断（不造起点，D-23 同族）。
//!
//! ## 已知限制（sourceState.detail，如实申报）
//!
//! * **SDK 内部重试不可识别**（addendum §4.1）：再次 `llm.sent` 会重置 requestStartedAt /
//!   accumulator / usage 但**未重置 firstChunkAt**——首点来自旧尝试、usage 来自后续尝试的
//!   错配路径在代码上存在，字段为正不等于同批证据。本读取器只对**可见**的同 turnStep 多
//!   request 剔除速度；不可见的内部重试**不谎称零重试**，在来源状态明示。
//! * `tool_ms` 恒空：tool.call→tool.result 配对与 step 观察需跨事件回填，与「事实尽快出批
//!   进 120s 采样窗」冲突（与 Task 3 DSH 读取器同一取舍）。
//! * 首 part 是否非空有效输出未逐 provider 验证（TTFT 的内容形态边界，与 ZCode 同款申报）。
//!
//! ## 增量预算（Spec §2「文件只读新增部分」+ AGENTS.md L3）
//!
//! * 逐文件游标 = 已消费字节界 + 单元内已消费行数 + 未决 llm.request 表（turnStep → 尝试
//!   序列，带时刻用于剪枝），全部装在 SourceCursor.value 随 ReadBatch 返回，本层不落库。
//! * **残缺尾行不越过**：plain JSONL 只消费最后一个 '\n'（含）之前的完整行，残行下轮重读；
//!   文件收缩按新文件重读。
//! * 24h 新鲜窗：陈旧且无水位的 wire 文件**不打开**（L3 预算；事实保留窗同 24h）。
//! * 单批发射上限 = `store::TPS_BATCH_MAX_ROWS`（200），预算在行粒度留检查点。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::host_instance_tag;
use crate::monitor::kimi_parser::{parse_session_index, resolve_data_root, resolve_session_dir};
use crate::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use crate::services::tps::store::TPS_BATCH_MAX_ROWS;
use crate::services::usage::error::UsageError;

/// 来源 id（与 `UsageSourceId::Kimi.db_id()` 同串）
pub const SOURCE_ID: &str = "kimi";

/// 新鲜窗 = 事实保留窗（24h）
const FRESH_WINDOW_MS: i64 = crate::services::tps::store::TPS_RETENTION_MS;

/// 单批发射上限（与 commit_batch 的硬闸同源）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;

/// 单文件未决 `llm.request` 表的键数上限（有界：防长会话把游标状态撑爆；
/// 超限丢最旧键，同 turnStep 的 step.end 晚于 64 个 turn 出现属异常形态）
const REQUEST_KEYS_CAP: usize = 64;

/// 来源限制说明（sourceState.detail，如实申报）
const SOURCE_LIMITS: &str = "Kimi SDK 内部重试不可识别（llm.sent 重置 usage 不重置首点，首点/用量可能跨尝试错配），可见的同 turnStep 多 request 已剔除速度，不谎称零重试；tool_ms 恒空（工具配对未归属）；首 part 是否非空输出未逐 provider 验证";

// ==== 游标状态（装在 SourceCursor.value；反序列化失败归零重读——重读的观察以稳定键
//      UPSERT 幂等，不产生双计）====

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KimiCursorState {
    /// wire 文件（绝对路径串）→ 读取位置与未决 request 状态
    #[serde(default)]
    files: BTreeMap<String, KimiFilePos>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KimiFilePos {
    /// 已消费字节界（最后一个 '\n' 之后）
    #[serde(default)]
    offset: u64,
    /// 当前读取单元内已消费、待跳过的行数（预算在单元中部耗尽时的检查点）
    #[serde(default)]
    skip_lines: usize,
    /// 未决 llm.request：turnStep（`turnId.step`）→ 尝试序列（按出现序保留，不覆盖）
    #[serde(default)]
    requests: BTreeMap<String, Vec<LlmReq>>,
}

/// 一条 `llm.request` 的归属要素（provider 不选：SeriesKey 无 provider 位，与 zcode 同款）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LlmReq {
    model: String,
    time_ms: i64,
}

// ==== 入口 ====

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    // 数据根：入参 home 的纯函数（刻意不认 KIMI_CODE_HOME）；未装 kimi = 正常空态
    let Some(root) = resolve_data_root(None, home) else {
        return Ok(empty_batch(cursor));
    };
    let host = host_instance_tag(&root.sessions);
    let mut state: KimiCursorState = parse_state(cursor);

    // 枚举：session_index.jsonl 条目 × agents/*/wire.jsonl（只 stat，不打开内容）；
    // mtime 降序 = 最新的文件优先吃预算
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_files: BTreeSet<String> = BTreeSet::new();
    for entry in parse_session_index(&root) {
        let session_dir = resolve_session_dir(&root, &entry.session_dir);
        let agents_dir = session_dir.join("agents");
        let Ok(agents) = std::fs::read_dir(&agents_dir) else {
            continue; // agents 目录在枚举间消失/不可读：跳过该条目（D-24 唯一静默形态）
        };
        for agent in agents.flatten() {
            let agent_path = agent.path();
            if !agent_path.is_dir() {
                continue;
            }
            let wire = agent_path.join("wire.jsonl");
            let Ok(meta) = std::fs::metadata(&wire) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let file_key = wire.to_string_lossy().to_string();
            seen_files.insert(file_key.clone());
            candidates.push(Candidate {
                mtime_ms,
                file_key,
                path: wire,
                agent_name: agent_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string()),
                size: meta.len(),
                session_id: entry.session_id.clone(),
            });
        }
    }
    candidates.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then(a.file_key.cmp(&b.file_key))
    });

    let mut observations: Vec<TpsObservation> = Vec::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut budget = BATCH_CAP;
    for c in candidates {
        if budget == 0 {
            diag(&mut tally, "单批预算用尽，余量下轮续读");
            break;
        }
        let had_pos = state.files.contains_key(&c.file_key);
        // L3 预算：超窗且无水位 → 不打开（有水位 = 以前读过，走便宜的字节界检查）
        if !had_pos && now_ms.saturating_sub(c.mtime_ms) > FRESH_WINDOW_MS {
            continue;
        }
        let mut pos = state.files.remove(&c.file_key).unwrap_or_default();
        read_wire(
            &c,
            &mut pos,
            now_ms,
            &host,
            &mut observations,
            &mut tally,
            &mut budget,
        )?;
        state.files.insert(c.file_key.clone(), pos);
    }
    // 消失文件的游标条目随之丢弃（枚举失败的文件本轮不在 seen 里，代价 = 下轮重读，幂等）
    state.files.retain(|k, _| seen_files.contains(k));

    let next_cursor = SourceCursor {
        source: SOURCE_ID.into(),
        value: serde_json::to_string(&state).unwrap_or_default(),
    };
    Ok(ReadBatch {
        observations,
        source_state: SourceState {
            source: SOURCE_ID.into(),
            cursor: next_cursor.clone(),
            ok: true,
            detail: Some(SOURCE_LIMITS.into()),
        },
        diagnostics: render_diag(&tally),
        next_cursor,
    })
}

struct Candidate {
    mtime_ms: i64,
    file_key: String,
    path: std::path::PathBuf,
    agent_name: Option<String>,
    size: u64,
    session_id: String,
}

// ==== 单文件增量读取（plain JSONL；残缺尾行不越过）====

#[allow(clippy::too_many_arguments)]
fn read_wire(
    c: &Candidate,
    pos: &mut KimiFilePos,
    now_ms: i64,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) -> Result<(), UsageError> {
    // 1) 文件收缩（截断/重写）：按新文件重读
    if c.size < pos.offset {
        diag(tally, "wire 文件收缩，按新文件重读");
        pos.offset = 0;
        pos.skip_lines = 0;
        pos.requests.clear();
    }
    // 2) 无新增（便宜路径：不打开）
    if pos.offset >= c.size && pos.skip_lines == 0 {
        return Ok(());
    }
    // 3) 增量尾读（独立复核 R3）：从水位 seek，只读新增 ≤ 单批上限——读取量与已消费
    //    历史长度无关，1 GB 已消费 wire 追加一行时只读那一行。W-06 同族：枚举与读取
    //    之间的轮转竞态（文件已消失）才静默跳过，其余读失败整源 Err（Err 时水位不推进）
    let base = pos.offset;
    let (bytes, _eof) = match super::read_tail(&c.path, base, super::TAIL_READ_MAX_BYTES) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            diag(tally, "wire file vanished between enumerate and read (rotation race), skipping this round");
            return Ok(());
        }
        Err(e) => {
            return Err(UsageError::new(
                "usage-source-io",
                format!("tps/kimi: failed to read {}: {e}", c.path.display()),
            ));
        }
    };
    // 4) 只消费完整行：最后一个 '\n'（含）为止，残行留待下轮（不丢不重）。
    //    读满上限仍无换行 = 单行超过上限（病态）：诊断、本轮不越过，绝不改读整文件
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    if complete == 0 && bytes.len() >= super::TAIL_READ_MAX_BYTES {
        diag(
            tally,
            "single wire line exceeds tail read cap, not consumed this round",
        );
    }
    let text = String::from_utf8_lossy(&bytes[..complete]);

    // 5) 逐行消费（预算按行粒度留检查点）
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut i = pos.skip_lines.min(total);
    while i < total {
        if *budget == 0 {
            break;
        }
        apply_line(lines[i], c, pos, host, observations, tally, budget);
        i += 1;
    }
    if i == total {
        pos.offset = base + complete as u64; // 整段消费（上限截断的剩余部分下轮续读）
        pos.skip_lines = 0;
    } else {
        pos.skip_lines = i; // 预算在单元中部耗尽：检查点 =（起点, 已消费行数）
    }

    // 6) 未决 request 剪枝：超 24h 的尝试条目丢弃（与事实保留窗一致，状态有界）
    for reqs in pos.requests.values_mut() {
        reqs.retain(|r| now_ms.saturating_sub(r.time_ms) <= FRESH_WINDOW_MS);
    }
    pos.requests.retain(|_, r| !r.is_empty());
    while pos.requests.len() > REQUEST_KEYS_CAP {
        let first = pos.requests.keys().next().expect("非空才进循环").clone();
        pos.requests.remove(&first);
    }
    Ok(())
}

// ==== 事件处理 ====

#[allow(clippy::too_many_arguments)]
fn apply_line(
    line: &str,
    c: &Candidate,
    pos: &mut KimiFilePos,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        diag(tally, "wire 行 JSON 解析失败，跳过");
        return;
    };
    let line_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    // 真机形态（dsh-kimi.md「顶层 llm.request 共 811 条，全部有 provider/model/turnStep」）：
    // `llm.request` 是**顶层**行——provider/model/turnStep 与行级 time 直接在行上。
    // 独立复核 R5：此前只在循环事件的嵌套 event 里找它，真实请求全部被下面的 filter 丢弃
    // → 模型归属进 unknown、已可见的重试无法排除。嵌套形态证据为零，不为不存在的形态保留分支。
    if line_type == "llm.request" {
        let Some(time) = v.get("time").and_then(|t| t.as_i64()) else {
            diag(
                tally,
                "llm.request missing line-level time, skipping model attribution",
            );
            return;
        };
        apply_llm_request(&v, time, pos, tally);
        return;
    }
    // 其余真机形态：行级 `time` 在外层；结算事件嵌在 `event` 下（usage/kimi 采集器同款归一）
    if line_type != "context.append_loop_event" {
        return; // 顶层 usage.record / turn.prompt 等：TPS 口径不消费
    }
    let Some(time) = v.get("time").and_then(|t| t.as_i64()) else {
        diag(tally, "循环事件缺行级 time，跳过");
        return;
    };
    let null = serde_json::Value::Null;
    let ev = v.get("event").unwrap_or(&null);
    // 只消费 step.end；step.begin / content.part / tool.call / tool.result：TPS 口径不消费
    if ev.get("type").and_then(|t| t.as_str()) == Some("step.end") {
        apply_step_end(ev, time, c, pos, host, observations, tally, budget);
    }
}

/// `llm.request`：记录模型归属尝试（同 turnStep 多条 = 重试，按序保留、禁止覆盖）
fn apply_llm_request(
    ev: &serde_json::Value,
    time: i64,
    pos: &mut KimiFilePos,
    tally: &mut BTreeMap<String, usize>,
) {
    let Some(turn_step) = ev.get("turnStep").and_then(|t| t.as_str()) else {
        diag(tally, "llm.request 缺 turnStep，模型归属跳过");
        return;
    };
    let Some(model) = ev
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
    else {
        diag(tally, "llm.request 缺 model，模型归属跳过");
        return;
    };
    pos.requests
        .entry(turn_step.to_string())
        .or_default()
        .push(LlmReq {
            model: model.to_string(),
            time_ms: time,
        });
}

/// `step.end`：同事件 usage + 两个原生时长 → 完整事实观察。
/// 调用方保证 budget > 0 才进本函数；本函数至多发射一条。
#[allow(clippy::too_many_arguments)]
fn apply_step_end(
    ev: &serde_json::Value,
    time: i64,
    c: &Candidate,
    pos: &mut KimiFilePos,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    // 步骤身份（dsh-kimi.md：turnId + step 定位，uuid 是 step 自身身份）
    let Some(uuid) = ev
        .get("uuid")
        .and_then(|u| u.as_str())
        .filter(|u| !u.is_empty())
    else {
        diag(tally, "step.end 缺 uuid → 跳过（无稳定身份）");
        return;
    };
    let turn_id = turn_id_of(ev);
    if turn_id.is_empty() {
        diag(tally, "step.end 缺 turnId → 跳过（无原生回合身份）");
        return;
    }
    let Some(step) = ev.get("step").and_then(|s| s.as_i64()) else {
        diag(tally, "step.end 缺 step → 跳过");
        return;
    };
    let finish = ev
        .get("finishReason")
        .and_then(|f| f.as_str())
        .unwrap_or("");
    let outcome = match finish {
        "error" => Outcome::Failed,
        "end_turn" | "tool_use" => Outcome::Success,
        other => {
            diag(
                tally,
                &format!("unknown finishReason={other:?} -> skip (no guessing)"),
            );
            return;
        }
    };
    // 两个原生时长必须齐备才产观察（缺计时 = 无法定位起点，不造 0/1970，D-23 同族）
    let (Some(ttft), Some(stream)) = (
        ev.get("llmFirstTokenLatencyMs").and_then(|t| t.as_i64()),
        ev.get("llmStreamDurationMs").and_then(|t| t.as_i64()),
    ) else {
        diag(
            tally,
            "step.end 缺原生计时（ttft/stream）→ 跳过（不造起点）",
        );
        return;
    };
    // 同事件 usage（唯一原生输出字段；缺 = 记缺失不报 0）
    let output = ev.pointer("/usage/output").and_then(|o| o.as_i64());
    if output.is_none() {
        diag(tally, "step.end 缺 usage.output → 记缺失不报 0");
    }

    // 模型归属：同 turnStep 的未决 request 序列（消费后移除，状态有界）
    let turn_step_key = format!("{turn_id}.{step}");
    let reqs = pos.requests.remove(&turn_step_key).unwrap_or_default();
    let (aligned, model) = match reqs.len() {
        0 => (true, "unknown".to_string()), // 旧形态：模型未知桶，usage+timing 仍同条对齐
        1 => (true, reqs[0].model.clone()),
        n => {
            diag(
                tally,
                &format!(
                    "same turnStep has {n} llm.request entries (retries), cannot align which produced this usage -> excluded from speed"
                ),
            );
            (false, "unknown".to_string())
        }
    };

    // 计时锚定（见模块头）：ended = 行级时刻；native = ttft+stream；started = ended−native；
    // 首 token = ended−stream。重试不可对齐时起止/首点全空（跨尝试的时刻不是单次调用时刻）。
    let native_duration_ms = if aligned {
        let d = ttft.saturating_add(stream);
        (d > 0).then_some(d)
    } else {
        None
    };
    // 生成窗：失败不入速度（Spec §3）；重试不可对齐剔除；正窗即有效（806ms 合法，
    // 无全局 1s 下限）；非正窗（含 0 的整块突发）不入速
    let generation_ms = if outcome == Outcome::Success && aligned {
        if stream > 0 {
            Some(stream)
        } else {
            diag(tally, "llmStreamDurationMs 非正 → 无生成窗，不入速度");
            None
        }
    } else {
        None
    };
    // 响应窗（V2 Spec §5.2「含等待响应」）：同事件原生完整时长 ttft + stream（请求
    // 起点→首片段→流结束；dsh-kimi.md 指标表「模型累计用时 = TTFT + streamDuration」，
    // 等待只计一次，绝不二次相加）。与整调用时长同一套对齐判据：重试不可对齐不填；
    // 失败不入响应速度。
    let response_elapsed_ms = if outcome == Outcome::Success && aligned {
        native_duration_ms
    } else {
        None
    };
    let started_at_ms = match native_duration_ms {
        Some(d) => time - d,
        // 无可靠时长（重试对齐失败/时长非正）时起点不可定位：观察仍出（事实面），
        // started==ended 且时长/首点/生成窗全空——不产生任何伪造计时
        None => time,
    };
    let first_token_at_ms = (aligned && stream > 0).then(|| time - stream);

    observations.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        // 稳定键：session + agent 文件身份 + step 自身 uuid（dsh-kimi.md 关联键）
        stable_call_key: format!(
            "{}:{}:{uuid}",
            c.session_id,
            c.agent_name.as_deref().unwrap_or("main")
        ),
        started_at_ms,
        first_token_at_ms,
        ended_at_ms: time,
        native_duration_ms,
        generation_ms,
        // 同一次输出两个窗（Spec §5.1）：生成窗 = stream；响应窗 = ttft + stream
        //（含等待；见上）。
        response_elapsed_ms,
        output_tokens: output,
        // kimi usage 无独立 reasoning 字段 → 思考计数未单独声明（Spec §3 provider-reported）
        output_scope: OutputScope::ProviderReported,
        tool_ms: None, // 已知限制：工具配对未归属（见模块头）
        model,
        channel: None,
        session_key: non_empty(c.session_id.clone()),
        turn_key: Some(turn_id),
        purpose: Purpose::MainTask,
        outcome,
        method: TimingMethod::ClientNative,
        failure_reason: (outcome == Outcome::Failed).then(|| "finishReason=error".to_string()),
    });
    *budget -= 1;
}

// ==== 小工具 ====

/// turnId 的两种形态归一：`event.turnId` 是字符串，容忍数字变体
/// （usage/kimi 采集器同款：`turn.ended.turnId` 为数字、`event.turnId` 为字符串）
fn turn_id_of(ev: &serde_json::Value) -> String {
    match ev.get("turnId") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn parse_state(cursor: &SourceCursor) -> KimiCursorState {
    if cursor.value.is_empty() {
        return KimiCursorState::default();
    }
    serde_json::from_str(&cursor.value).unwrap_or_else(|e| {
        log::warn!("tps/kimi: 游标状态解析失败（{e}）→ 归零重读（稳定键 UPSERT 幂等）");
        KimiCursorState::default()
    })
}

/// 诊断计数（同原因聚合一条，避免刷屏）
fn diag(tally: &mut BTreeMap<String, usize>, reason: &str) {
    *tally.entry(reason.to_string()).or_insert(0) += 1;
}

fn render_diag(tally: &BTreeMap<String, usize>) -> Vec<String> {
    tally
        .iter()
        .map(|(reason, n)| format!("{SOURCE_ID}: {n}x {reason}"))
        .collect()
}

/// 空批（数据根缺失等正常空态）：游标原样、ok、无诊断
fn empty_batch(cursor: &SourceCursor) -> ReadBatch {
    ReadBatch {
        observations: Vec::new(),
        source_state: SourceState {
            source: SOURCE_ID.into(),
            cursor: cursor.clone(),
            ok: true,
            detail: None,
        },
        diagnostics: Vec::new(),
        next_cursor: cursor.clone(),
    }
}
