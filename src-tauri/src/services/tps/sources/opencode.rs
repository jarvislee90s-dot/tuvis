//! OpenCode 观察插件事实读取器（Spec §4 OpenCode 行；2026-10-24 计划 Task 6）。
//!
//! ## 读取对象与口径
//!
//! * 事实文件 = 插件落盘的 `<home>/.tuvis/tps/opencode/facts.jsonl`（append-only JSONL；
//!   路径与插件同式推导：home + `.tuvis/tps/opencode/facts.jsonl`，见插件
//!   `FACT_RELATIVE_DIR`/`FACT_FILE_NAME`）。插件宿主（opencode 2.0.22）侧只观察、
//!   取数字后丢正文；本读取器只解析数字与键，绝不触碰用户 opencode 配置/会话数据。
//! * 计时方法固定 [`TimingMethod::StreamObserved`]（Spec §3 三法之一；series_key 含
//!   method 即隔离）。生成窗 = 首个非空 delta 时刻 → `session.step.ended` 时刻
//!   （Spec §4「开始、非空delta、结束tokens同键」；开始 = 插件 start 行的宿主
//!   `started` 字段，宿主惰性发布时回退为事件时刻——上游同款校准）。
//! * 响应窗（V2 Spec §5.2「含等待响应」）= 完整 step 窗（start 行宿主 `started` →
//!   终行 `created`，同 session/message/step 身份，不含后续 step 的工具时间）；成功
//!   且无已识别未对齐重试才填实（与生成窗同一套判据）。
//! * 入速条件（Spec §3）：outcome=Success、output>0、生成窗>0、无「识别而不能对齐
//!   重试」。已识别的重试（`session.retry.scheduled` / 2.1.x `session.next.retried`，
//!   由插件折叠进终行 `attempt` 或读取器在飞计数）**剔除速度、保留候选与起止事实**
//!   （Spec §3「能识别而不能对齐的重试排除」）。
//! * 取消单列：插件把 `MessageAbortedError` 的 step.failed 标成 `cancelled` 行 →
//!   [`Outcome::Cancelled`]（取消不入速度，有可靠起止仍进耗时详情，Spec §3）。
//! * usage 只来自 `session.step.ended` 自带数值（插件的累计面 `session.usage.updated`/
//!   `message.updated` 永不入行）→ 累计 usage 不重复入账（Spec §4 OpenCode 行）。
//!   reasoning 由宿主单列上报但与 output 的包含关系未经实测 → 标
//!   [`OutputScope::ProviderReported`]（「思考计数未单独声明」，不并入、不估算）。
//! * purpose 一律 MainTask：辅助标题/compaction 事件不在观察词表内（如实申报于
//!   [`SOURCE_LIMITS`]，不猜不凑）。
//! * turn_key 恒 None：step 事件无原生回合键（execution 事件未消费）→ 查询面按
//!   Spec §6 返回 unknown-turn 的降级态，不按 session 全部请求冒充一轮。
//!
//! ## 增量预算（Spec §2「文件只读新增部分」+ kimi/dsh 同族纪律）
//!
//! * 游标 = 已消费字节界 + 单元内已消费行数 + 在飞调用/step 计数状态，全部装在
//!   `SourceCursor.value` 随 ReadBatch 返回，本层不落库；解析失败归零重读——观察以
//!   稳定键 UPSERT 幂等，不产生双计。
//! * **残缺尾行不越过**：只消费最后一个 '\n'（含）之前的完整行；文件收缩（轮转/
//!   截断）按新文件重读（插件 4MiB 轮转一次 → 最多重读 8MiB，幂等）。
//! * 单批预算：观察 ≤ [`BATCH_CAP`]（200，与 commit_batch 硬闸同源），行处理 ≤
//!   [`ROW_BUDGET`]（有界工作量；超出部分 `skip_lines` 检查点下轮续读）。
//! * 在飞调用状态进游标：start 行与其 end 行可跨批合流（字节界游标不存在 zcode
//!   rowid 水位的「越过在飞行」问题；在飞调用按 24h 保留窗剪枝，僵尸自灭）。
//! * 列/行级读失败整源 `Err`（静默少算比响亮失败危险），水位不推进。
//!
//! ## 适配状态（Task 7 注册面消费，机器可读）
//!
//! [`ADAPTER_STATUS`] 声明本适配的验收状态：`needs-live-validation`（缺一条真实
//! 正常宿主 start→delta→end 正例）或 `supported`（正例已捕获）。loaded ≠ 支持
//! （Spec §7「不以loaded充数」）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::host_instance_tag;
use crate::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use crate::services::tps::store::{TPS_BATCH_MAX_ROWS, TPS_RETENTION_MS};
use crate::services::usage::error::UsageError;

/// 来源 id（与 `UsageSourceId::OpenCode.db_id()` 同串）
pub const SOURCE_ID: &str = "opencode";

/// 适配验收状态（机器可读；sourceState.detail 前缀）：
/// * `supported` = 已有一条**真实宿主**的 start→delta→end 正例（Spec §7：
///   「真实正常宿主一条开始→delta→结束为该适配验收」，loaded ≠ 支持）；
/// * `needs-live-validation` = 插件已交付但无正例。
///
/// 正例出处（2026-10-24，R8 裁决的隔离运行——tempdir HOME、零用户凭证、零用户配置）：
/// opencode v2.0.22 `run --standalone`（内置免费模型 longcat-2.5-preview-free），
/// 交付字节原样捕获 start(created=1792857532020, started=1792857529940) → 非空 delta
/// (created=1792857532031, deltaChars=2) → end(created=1792857532036, tokens=3,
/// finish=stop)，facts.jsonl 全文与复核记录见 task-6-report.md。
/// 用户日常路径仍是：Tuvis 设置 → 点击安装观察插件 → 自然重启 OpenCode → 正常请求。
pub const ADAPTER_STATUS: &str = "supported";

/// 来源限制说明（sourceState.detail，如实申报；前缀供 Task 7 机器解析）
pub(crate) const SOURCE_LIMITS: &str = concat!(
    "status=",
    "supported",
    "; 正例=隔离运行 opencode 2.0.22 run --standalone（免费内置模型）一条 start→delta→end; \
     usage 只取 step.ended 自带 output（累计面已排除，若宿主把 message 级累计放进 \
     step.ended 需实测复核）；reasoning 单列但包含关系未实测 → provider-reported 不并入；\
     无原生回合键（unknown-turn）；辅助标题/compaction 不在观察词表，purpose 恒主任务；\
     tool_ms 恒空（工具配对未归属）；2.1.x 宿主 session.next.* 词表按别名兼容、\
     subscription 返回形态需实机复核；插件分发为纯 JS 的 .ts（宿主不装载 .mjs，隔离实测）"
);

/// 单批观察上限（与 commit_batch 硬闸同源）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;
/// 单批行处理上限（有界工作量；5s 可见期轮询的最坏路径）
const ROW_BUDGET: usize = 8000;
/// 游标状态键数上限（step 计数/在飞调用；超限先按保留窗剪枝，仍超则丢最旧）
const STATE_KEYS_CAP: usize = 512;
/// 单行字节上限（插件同款；超长行 = 异常形态，跳过并诊断）
const MAX_LINE_BYTES: usize = 8192;

// ==== 游标状态（装在 SourceCursor.value；反序列化失败归零重读——观察以稳定键
//      UPSERT 幂等，不产生双计）====

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorState {
    /// 已消费字节界（最后一个 '\n' 之后）
    #[serde(default)]
    offset: u64,
    /// 当前读取单元内已消费、待跳过的行数（行预算在中部耗尽时的检查点）
    #[serde(default)]
    skip_lines: usize,
    /// (sessionKey,messageKey) → step 计数与最近活跃时刻（稳定 call 键的 stepIndex 来源）
    #[serde(default)]
    steps: BTreeMap<String, KeyMeta>,
    /// 在飞调用（已见 start 未见终行）
    #[serde(default)]
    open: BTreeMap<String, OpenCall>,
}

/// step 计数条目
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyMeta {
    step: u64,
    last_seen_ms: i64,
}

/// 一次在飞调用（start 行开出的生成窗）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenCall {
    /// (session,message) 内的 step 序号（稳定 call 键的末段）
    step: u64,
    started_at: i64,
    first_non_empty: Option<i64>,
    model: String,
    provider: Option<String>,
    /// 本调用内观察到的未对齐重试次数（retry 行折叠；>0 → 剔除速度）
    retries: u64,
}

// ==== 插件事实行（固定 15 键 camelCase；缺值 null → Option）====

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FactRow {
    #[serde(default)]
    event_key: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    message_key: Option<String>,
    /// 插件侧每键序数（键集与插件产出逐字对齐的文档性字段；本层排序用文件序，
    /// 不消费——交错不串键的证明面在插件单测里，故只读不用的 dead_code 豁免）
    #[serde(default)]
    #[allow(dead_code)]
    ordinal: i64,
    #[serde(default)]
    attempt: u64,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    started: Option<i64>,
    #[serde(default)]
    tokens: Option<i64>,
    #[serde(default)]
    finish: Option<String>,
    #[serde(default)]
    has_non_empty_delta: bool,
    #[serde(default)]
    delta_chars: i64,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    tokens_reasoning: Option<i64>,
}

impl FactRow {
    fn row_type(&self) -> &str {
        self.r#type.as_deref().unwrap_or("")
    }
}

// ==== 入口 ====

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    // 事实文件：入参 home 的纯函数（插件侧同式推导；未装插件 = 正常空态）
    let fact_file = home
        .join(".tuvis")
        .join("tps")
        .join("opencode")
        .join("facts.jsonl");
    let host = host_instance_tag(&fact_file);
    if !fact_file.exists() {
        return Ok(empty_batch(cursor));
    }
    let mut state: CursorState = parse_state(cursor);
    let meta = std::fs::metadata(&fact_file).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("tps/opencode: failed to stat {}: {e}", fact_file.display()),
        )
    })?;
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    // 文件收缩（插件轮转/外部截断）：按新文件重读（观察以稳定键 UPSERT 幂等）
    if meta.len() < state.offset {
        diag(&mut tally, "事实文件收缩（轮转/截断），按新文件重读");
        state.offset = 0;
        state.skip_lines = 0;
        state.open.clear();
    }
    // 便宜路径：无新增完整内容
    if state.offset >= meta.len() && state.skip_lines == 0 {
        return Ok(finish_batch(state, Vec::new(), tally));
    }

    // 增量尾读（独立复核 R3，与 kimi/workbuddy/dsh 同一入口）：从水位 seek 只读新增
    // ≤ 单批上限；插件 4MiB 轮转使整文件本就有界，但读取量仍只随新增量走
    let base = state.offset;
    let (bytes, _eof) =
        super::read_tail(&fact_file, base, super::TAIL_READ_MAX_BYTES).map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps/opencode: failed to read {}: {e}", fact_file.display()),
            )
        })?;
    // 只消费完整行（残缺尾行不越过，下轮重读）
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    if complete == 0 && bytes.len() >= super::TAIL_READ_MAX_BYTES {
        diag(
            &mut tally,
            "single fact line exceeds tail read cap, not consumed this round",
        );
    }
    let text = String::from_utf8_lossy(&bytes[..complete]);

    let mut observations: Vec<TpsObservation> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut i = state.skip_lines.min(total);
    let mut row_budget = ROW_BUDGET;
    // 同批 eventKey 去重（重复投递的行只折进状态一次；跨批重复由 UPSERT 幂等兜底）
    let mut seen_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    while i < total {
        if row_budget == 0 || observations.len() >= BATCH_CAP {
            break;
        }
        row_budget -= 1;
        apply_line(
            lines[i],
            &mut state,
            &host,
            now_ms,
            &mut seen_keys,
            &mut observations,
            &mut tally,
        );
        i += 1;
    }
    if i == total {
        state.offset = base + complete as u64; // 整段消费
        state.skip_lines = 0;
    } else {
        state.skip_lines = i; // 预算在中部耗尽：检查点 =（起点, 已消费行数）
    }

    prune_state(&mut state, now_ms, &mut tally);
    Ok(finish_batch(state, observations, tally))
}

// ==== 行处理（状态机：start 开窗 → delta 定首点 → end/failed/cancelled 收窗）====

#[allow(clippy::too_many_arguments)]
fn apply_line(
    line: &str,
    state: &mut CursorState,
    host: &str,
    now_ms: i64,
    seen_keys: &mut std::collections::BTreeSet<String>,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
) {
    if line.len() > MAX_LINE_BYTES {
        diag(tally, "超长事实行（异常形态），跳过");
        return;
    }
    let Ok(row) = serde_json::from_str::<FactRow>(line) else {
        diag(tally, "事实行 JSON 解析失败，跳过");
        return;
    };
    if let Some(key) = row.event_key.clone() {
        if !seen_keys.insert(key) {
            diag(tally, "同批重复 eventKey 行，跳过（幂等）");
            return;
        }
    }
    let Some(sk) = row.session_key.clone() else {
        diag(tally, "事实行缺 sessionKey，跳过");
        return;
    };
    let created = match row.created {
        Some(c) if c > 0 => c,
        // 诊断行允许 created 为占位；观察行缺时刻无法计时 → 跳过（不造 0 时刻）
        _ if row.row_type().starts_with("diag:") => return,
        _ => {
            diag(tally, "事实行缺 created，跳过（不造时刻）");
            return;
        }
    };
    match row.row_type() {
        "start" => apply_start(row, sk, created, state, now_ms, tally),
        "delta" => apply_delta(row, sk, state, tally),
        "end" | "failed" | "cancelled" => {
            apply_terminal(row, sk, created, state, host, now_ms, observations, tally)
        }
        "retry" => apply_retry(row, sk, state, tally),
        t if t.starts_with("diag:") => {
            // 观察器诊断行（loaded/订阅失败等）：计一条诊断，不入观察
            diag(tally, &format!("observer diagnostic line {t}"));
        }
        other => diag(tally, &format!("unknown fact row type={other:?}, skipping")),
    }
}

fn pair_key(sk: &str, mk: &str) -> String {
    format!("{sk}\u{0}{mk}")
}

fn apply_start(
    row: FactRow,
    sk: String,
    created: i64,
    state: &mut CursorState,
    now_ms: i64,
    tally: &mut BTreeMap<String, usize>,
) {
    let Some(mk) = row.message_key.clone() else {
        diag(tally, "start 行缺 messageKey，跳过");
        return;
    };
    let key = pair_key(&sk, &mk);
    let meta = state.steps.entry(key.clone()).or_insert(KeyMeta {
        step: 0,
        last_seen_ms: now_ms,
    });
    meta.step += 1;
    meta.last_seen_ms = now_ms;
    if meta.step == 0 {
        // u64 回绕防御（现实不可达）
        meta.step = 1;
    }
    // 已有在飞同键调用 = 上一次 end 丢失：弃旧开新（旧调用无终态，不入观察）
    if state.open.contains_key(&key) {
        diag(
            tally,
            "同键已有在飞调用又被 start 开窗（缺失终行），弃旧开新",
        );
    }
    state.open.insert(
        key,
        OpenCall {
            step: meta.step,
            started_at: row.started.unwrap_or(created),
            first_non_empty: None,
            model: row.model.unwrap_or_default(),
            provider: row.provider,
            retries: row.attempt,
        },
    );
}

fn apply_delta(
    row: FactRow,
    sk: String,
    state: &mut CursorState,
    tally: &mut BTreeMap<String, usize>,
) {
    let Some(mk) = row.message_key.clone() else {
        diag(tally, "delta 行缺 messageKey，跳过");
        return;
    };
    let Some(open) = state.open.get_mut(&pair_key(&sk, &mk)) else {
        // 无在飞调用（起点缺失/游标从调用中部进入）：delta 无归属，不入（不造起点）
        diag(tally, "delta 行无在飞调用（缺起点），跳过");
        return;
    };
    if row.has_non_empty_delta && row.delta_chars > 0 && open.first_non_empty.is_none() {
        open.first_non_empty = row.created;
    }
}

fn apply_retry(
    row: FactRow,
    sk: String,
    state: &mut CursorState,
    tally: &mut BTreeMap<String, usize>,
) {
    match row.message_key.clone() {
        Some(mk) => {
            if let Some(open) = state.open.get_mut(&pair_key(&sk, &mk)) {
                open.retries += 1;
            }
        }
        // 2.1.x retried 无 messageID：会话级重试无法对齐 → 会话内全部在飞调用剔除速度
        None => {
            let prefix = format!("{sk}\u{0}");
            let mut hit = 0;
            for (key, open) in state.open.iter_mut() {
                if key.starts_with(&prefix) {
                    open.retries += 1;
                    hit += 1;
                }
            }
            if hit > 0 {
                diag(tally, "会话级重试（无 messageKey）→ 该会话在飞调用剔除速度");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_terminal(
    row: FactRow,
    sk: String,
    created: i64,
    state: &mut CursorState,
    host: &str,
    now_ms: i64,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
) {
    let Some(mk) = row.message_key.clone() else {
        diag(tally, "终行缺 messageKey，跳过");
        return;
    };
    let key = pair_key(&sk, &mk);
    let Some(open) = state.open.remove(&key) else {
        diag(tally, "终行无在飞调用（缺 start 起点），跳过（不落无名行）");
        return;
    };
    // 键活跃时刻以**读取时刻**记账（事件时刻可能是历史回放/夹具；保留窗按事实窗对齐）
    if let Some(meta) = state.steps.get_mut(&key) {
        meta.last_seen_ms = now_ms;
    }
    // 模型：绝不落无名行（D-32 同族；起点行缺失时 model 为空）
    if open.model.is_empty() {
        if row.row_type() == "end" {
            diag(tally, "调用缺 model（起点行不完整）→ 跳过（不落无名行）");
        }
        return;
    }
    let outcome = match row.row_type() {
        "end" => Outcome::Success,
        "failed" => Outcome::Failed,
        _ => Outcome::Cancelled,
    };
    // 生成窗 = 首非空 delta → 终行；已识别未对齐重试剔除速度（Spec §3）
    let retries = open.retries.max(row.attempt);
    let generation_ms = if outcome != Outcome::Success {
        None // 失败/取消不入速度（Spec §3）
    } else if retries > 0 {
        diag(
            tally,
            &format!("{retries} retries detected and not alignable -> excluded from speed"),
        );
        None
    } else {
        match open.first_non_empty {
            None => {
                if row.row_type() == "end" {
                    diag(tally, "成功调用无首非空 delta → 无生成窗，不入速度");
                }
                None
            }
            Some(first) => {
                let g = created - first;
                if g > 0 {
                    Some(g)
                } else {
                    diag(tally, "生成窗非正 → 不入速度");
                    None
                }
            }
        }
    };
    if row.row_type() == "end" && row.tokens.is_none() {
        diag(tally, "end 行缺 tokens → 记缺失不报 0");
    }
    if row.tokens_reasoning.is_some() {
        diag(
            tally,
            "reasoning 由宿主单列 → provider-reported，不并入 output",
        );
    }
    let native_duration_ms = (created - open.started_at > 0).then_some(created - open.started_at);
    // 响应窗（V2 Spec §5.2「含等待响应」）：完整 step 窗（start 行宿主 started → 终行
    // created，同 session/message/step 身份；绝不借消息间墙钟拼窗、不延伸到后续 step
    // 的工具时间）。与生成窗同一套判据：成功且无已识别未对齐重试才填实；起止差与
    // 生成窗的同基准一致性由 Task 1 谓词把关（绝不修复）。
    let response_elapsed_ms = if outcome == Outcome::Success && retries == 0 {
        native_duration_ms
    } else {
        None
    };
    observations.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        stable_call_key: format!("oc:{}:{}:{}", sk, mk, open.step),
        started_at_ms: open.started_at,
        first_token_at_ms: open.first_non_empty,
        ended_at_ms: created,
        native_duration_ms,
        generation_ms,
        // 同一次输出两个窗（Spec §5.1）：生成窗 = 首非空 delta → 终行；响应窗 =
        // 完整 step 窗 started → 终行（含等待；见上）。
        response_elapsed_ms,
        output_tokens: row.tokens,
        output_scope: OutputScope::ProviderReported,
        tool_ms: None, // 工具配对未归属（不填 0，Spec §6）
        model: open.model,
        channel: open.provider,
        session_key: Some(sk),
        turn_key: None, // step 事件无原生回合键 → unknown-turn（模块头申报）
        purpose: Purpose::MainTask,
        outcome,
        method: TimingMethod::StreamObserved,
        failure_reason: match outcome {
            Outcome::Success => None,
            // 宿主 finish 语义随行（如 content-filter）；中止行无 finish → 固定标记
            Outcome::Failed => Some(match &row.finish {
                Some(f) => format!("step.failed:{f}"),
                None => "step.failed".into(),
            }),
            Outcome::Cancelled => Some("message-aborted".into()),
        },
    });
}

// ==== 状态有界化（保留窗剪枝 + 键数硬上限）====

fn prune_state(state: &mut CursorState, now_ms: i64, tally: &mut BTreeMap<String, usize>) {
    // 在飞调用超保留窗 = 宿主崩溃残留（start 后 24h 无终态）：丢弃（僵尸自灭）
    let before = state.open.len();
    state
        .open
        .retain(|_, call| now_ms.saturating_sub(call.started_at) <= TPS_RETENTION_MS);
    if state.open.len() != before {
        diag(tally, "超保留窗的在飞调用丢弃（宿主残留，无终态不入观察）");
    }
    // step 计数超保留窗剪枝（事实保留窗同款；键撞回 step=1 时旧行已过保留窗被清理）
    state
        .steps
        .retain(|_, meta| now_ms.saturating_sub(meta.last_seen_ms) <= TPS_RETENTION_MS);
    if state.steps.len() > STATE_KEYS_CAP {
        let mut by_age: Vec<_> = state.steps.iter().collect::<Vec<_>>();
        by_age.sort_by_key(|(_, meta)| meta.last_seen_ms);
        let drop: std::collections::BTreeSet<String> = by_age
            .into_iter()
            .take(state.steps.len() - STATE_KEYS_CAP)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &drop {
            state.steps.remove(k);
        }
        diag(tally, "游标 step 计数超上限，丢最旧键（撞键行为幂等）");
    }
}

// ==== 批组装 ====

fn parse_state(cursor: &SourceCursor) -> CursorState {
    if cursor.value.is_empty() {
        return CursorState::default();
    }
    serde_json::from_str(&cursor.value).unwrap_or_else(|e| {
        log::warn!("tps/opencode: 游标状态解析失败（{e}）→ 归零重读（UPSERT 幂等）");
        CursorState::default()
    })
}

fn diag(tally: &mut BTreeMap<String, usize>, reason: &str) {
    *tally.entry(reason.to_string()).or_insert(0) += 1;
}

fn render_diag(tally: &std::collections::BTreeMap<String, usize>) -> Vec<String> {
    tally
        .iter()
        .map(|(reason, n)| format!("{SOURCE_ID}: {n}× {reason}"))
        .collect()
}

fn empty_batch(cursor: &SourceCursor) -> ReadBatch {
    ReadBatch {
        observations: Vec::new(),
        source_state: SourceState {
            source: SOURCE_ID.into(),
            cursor: cursor.clone(),
            ok: true,
            detail: Some(SOURCE_LIMITS.into()),
        },
        diagnostics: Vec::new(),
        next_cursor: cursor.clone(),
    }
}

fn finish_batch(
    state: CursorState,
    observations: Vec<TpsObservation>,
    tally: BTreeMap<String, usize>,
) -> ReadBatch {
    let next_cursor = SourceCursor {
        source: SOURCE_ID.into(),
        value: serde_json::to_string(&state).unwrap_or_default(),
    };
    ReadBatch {
        observations,
        source_state: SourceState {
            source: SOURCE_ID.into(),
            cursor: next_cursor.clone(),
            ok: true,
            detail: Some(SOURCE_LIMITS.into()),
        },
        diagnostics: render_diag(&tally),
        next_cursor,
    }
}
