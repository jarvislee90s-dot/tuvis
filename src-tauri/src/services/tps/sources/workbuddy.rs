//! WorkBuddy 原生 TPS 读取器（Spec §4 WorkBuddy 行；证据：
//! probes/2026-10-24-model-speed-supplement-macos.md §4.4/§11.2 —— Mac 5.8.3 正常路线
//! `qua.end` 单行全量载荷；probes/2026-10-24-model-speed-supplement-windows.md §7.5 ——
//! Windows 正常路线 `usage_update` 只有 `hasUsage=true` 而无数值 usage）。
//!
//! ## 数值归一与响应窗（Spec §9；V2 Task 2）
//!
//! 真机 Mac 5.8.3 的 `qua.end` 载荷里 `total_cost_ms` / `ttft_ms` / `completion_tokens`
//! 是**整数字符串**（如 `"6711"`）——旧实现按 JSON 数值读、整源诊断为「缺数值 usage」
//! 跳过。本版统一数字归一：**范围内 JSON 整数**（含能装进 i64 的 u64）与**合法十进制
//! 整数字符串**（可选单个前导 `-` + 纯 ASCII 数字）都接受；空串、布尔、null、浮点
//!（含 `6711.0`）、带空白/`+`/小数点/指数的串、NaN/Infinity 字样与 i64 溢出一律拒绝并
//! 按「字段:形态」诊断，**不默认 0、绝不经 f64 中转 Token**。终态、身份、重试与时间
//! 合理性等既有准入检查逐字保留，只换数值来源。
//!
//! 同一条 qua.end 记录同时构造两个时间窗（Spec §9「同时构造生成时间与响应时间」）：
//! `generation_ms = total − ttft`（原有口径不变）+ `response_elapsed_ms = total`
//! （含等待的响应窗事实；ttft 缺失时生成窗 None、响应窗仍为 Some(total)，响应口径
//! 单独可用由 Task 1 谓词把关）。失败/取消照发事实（outcome 判定不变），两个速度
//! 谓词天然判伪。
//!
//! ## 游标版本与旧 EOF 恢复（Spec §9；迁移纪律）
//!
//! 旧实现把 v1 游标推进到 EOF 时可能**零事实**（全部字符串数值行被跳过）。游标加
//! `format_version`（serde 字段默认 1：旧游标缺字段反序列化即 v1；本实现新写/推进恒为
//! 2）。读到**真实存在的 v1 游标**（空游标 = 全新安装，直接 v2 起步无历史可恢复）→
//! 触发**本来源一次性有界恢复**：枚举读取器已知的 qua 日志，**stat 预过滤**只留 mtime
//! 在 24h 新鲜窗内的文件（超窗不打开——AGENTS 反模式禁令：窗口判定先于打开），从字节
//! 0 重新经 `read_tail` 逐轮消费（单轮 ≤8MiB 字节、≤200 观察，残行不越过）；每轮把
//! 「事实 + 恢复进度（待恢复队列 + 各文件字节界）」随 ReadBatch 交给调用方同事务提交
//!（`store::commit_batch`），提交失败水位原样、下轮从检查点续跑；全部文件到 EOF 后
//! 游标升 v2，此后只消费追加字节、绝不反复全读。稳定键 UPSERT 使重放幂等；只恢复 TPS
//! 事实，不碰旧 Token/费用游标与其他来源游标。
//!
//! ## 读取对象与口径
//!
//! * 根 = `<home>/.workbuddy/logs/<日期>/*.log`（cli_host 日志，qua.* 行所在）+
//!   `<home>/.workbuddy/logs/<日期>/sdk/conversations/*.log`（Windows sdk 会话日志）。
//!   两条路线同根枚举、同一套字节界游标。
//! * **Mac 5.8.3 正常路线（已证）**：`qua.end` 单请求单行、同一身份，含
//!   `total_cost_ms` / `ttft_ms` / `completion_tokens` / `finish_flag` / `finish_reason` /
//!   `model_error` 与 `conversation_id / conversation_request_id / message_id / model /
//!   request_type` 身份字段（supplement-macos §4.4 字段清单）。
//!   生成窗 = `total_cost_ms − ttft_ms`（算式源码直读：ttft 以请求起点为锚、正文/思考窗
//!   天生分开，Spec §4「同请求 completion_tokens/(total_cost_ms−ttft_ms)」）；
//!   结束时刻 = 日志行前缀的 RFC3339 毫秒时刻（**绝不**拿文件 mtime 当事件时刻）；
//!   `started = ended − total_cost_ms`；`first_token = started + ttft_ms`；
//!   `native_duration = total_cost_ms`。
//! * **入速核对**（Spec §4「核对成功 finish、model/request 身份一致」）：finish_reason
//!   `stop` / `tool_calls` = 成功（工具调用后继续输出是正常收尾，supplement-macos §11.2
//!   实测）；`finish_flag=canceled` → Cancelled；`model_error` 非空 → Failed；
//!   未知 finish_reason → 跳过不猜。身份四元组（conversation_id / conversation_request_id /
//!   message_id / model）缺一 → 不入速跳过 + 诊断。
//! * **Windows 正常路线 = insufficient-evidence 缺口态**（supplement-windows §7.5 定论
//!   「有时刻、无 token」）：sdk/conversations 日志的 `usage_update` 只含 `hasUsage=true`
//!   标志、无任何数值 token → 该路线**不产观察、不入速**，来源状态明示缺口；
//!   **绝不借旧 CLI 样本或任何别的路线数据凑值**（Spec §4 WorkBuddy 行）。
//! * qua 行缺数值 `completion_tokens`（如只有 `hasUsage:true` 标志）：成功事实保留
//!   （output=None 记缺失不报 0），绝不入速。
//! * 输出语义：`completion_tokens` 为供应商总数（含思考与工具参数，supplement-macos §11.2
//!   语义注记）→ `IncludesReasoning`，不再相加；内容侧 `text/think_*_tokens` 子计数疑为
//!   字符侧估算，**绝不**加进速度分子（Spec §4「不加内容侧估算 Token」）。
//! * purpose：`request_type` 含 `topic` 的标题类请求（supplement-macos §11.2 样本 1
//!   `conversation*topic`）→ AssistTitle；其余 MainTask。
//! * 回合身份：`conversation_request_id`（同键下多次模型调用各自独立计时，Spec §1.4
//!   conversation-request）；稳定键 = `message_id`。
//!
//! ## 增量预算与已知限制
//!
//! * 逐文件游标 = 已消费字节界 + 行检查点，装在 SourceCursor.value 随 ReadBatch 返回；
//!   **残缺尾行不越过**；文件收缩重读；24h 新鲜窗内才打开无水位的日志；单批发射上限 200。
//! * 日志行的行前缀为日志框架文本（真机 `[Info] [pid=…]` 形态）：本读取器按
//!   `agent.sse.qua.end` 标记定位行、取行内 JSON 载荷、在前几个空白分隔 token 里解析
//!   RFC3339 时刻——对前缀文案的具体形态保持宽容（载荷字段名以证据清单为准）。
//! * `tool_ms` 恒空：qua 行无工具执行时长字段（工具等待在模型窗之外，无原生归属键）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::host_instance_tag;
use crate::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use crate::services::tps::store::TPS_BATCH_MAX_ROWS;
use crate::services::usage::error::UsageError;

/// 来源 id（与 `UsageSourceId::WorkBuddy.db_id()` 同串）
pub const SOURCE_ID: &str = "workbuddy";

/// 新鲜窗 = 事实保留窗（24h）
const FRESH_WINDOW_MS: i64 = crate::services::tps::store::TPS_RETENTION_MS;

/// 单批发射上限（与 commit_batch 的硬闸同源）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;

/// 来源限制说明（sourceState.detail，如实申报；Windows 路线缺口逐字含
/// insufficient-evidence，Spec §4「缺失时该路线返回 insufficient-evidence」）
const SOURCE_LIMITS: &str = "insufficient-evidence: Windows 正常桌面路线 usage_update 只有 hasUsage=true 而无数值 usage，该路线不入速，绝不借旧 CLI 样本凑值（需联到同请求数值 usage 后按新证据启用）；qua 路线覆盖 Mac 5.8.3 正常流式，tool_ms 恒空（无原生工具时长）";

// ==== 游标状态（装在 SourceCursor.value；反序列化失败归零重读——重读的观察以稳定键
//      UPSERT 幂等，不产生双计）====
//
// 版本语义（Spec §9 迁移纪律）：`format_version` 的 **serde 字段默认 = 1**——旧实现写下
// 的游标没有这个字段，反序列化即 v1，读到 v1 触发一次有界 24h 恢复；本实现新写/推进的
// 游标恒为 2。全新安装（空游标）走 [`WbCursorState::default`] 直接 v2 起步。两个默认值
// 刻意不同：字段级缺省=1（旧文件语义）、结构体级缺省=2（新起点语义）。

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WbCursorState {
    /// 游标格式版本：旧游标缺字段 → 1（触发有界恢复）；本实现写入恒为 2
    #[serde(default = "legacy_format_version")]
    format_version: u32,
    /// 日志文件（绝对路径串）→ 已消费字节界与行检查点
    #[serde(default)]
    files: BTreeMap<String, FilePos>,
    /// 恢复中（v1→v2 迁移）状态；恢复完成后置空并升 v2
    #[serde(default)]
    recovery: Option<RecoveryState>,
}

/// 旧游标（无版本字段）反序列化出的版本号
fn legacy_format_version() -> u32 {
    1
}

impl Default for WbCursorState {
    fn default() -> Self {
        Self {
            format_version: 2,
            files: BTreeMap::new(),
            recovery: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RecoveryState {
    /// 尚未恢复到 EOF 的 qua 日志（绝对路径串，首轮枚举序）；恢复字节界记在 files[key]
    pending: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FilePos {
    /// 已消费字节界（最后一个 '\n' 之后）
    #[serde(default)]
    offset: u64,
    /// 当前读取单元内已消费、待跳过的行数（预算在单元中部耗尽时的检查点）
    #[serde(default)]
    skip_lines: usize,
}

// ==== 入口 ====

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    let logs_root = home.join(".workbuddy").join("logs");
    // 未装 workbuddy = 正常空态（不是错误）：ok 批、游标原样
    if !logs_root.is_dir() {
        return Ok(empty_batch(cursor));
    }
    let host = host_instance_tag(&logs_root);
    let mut state: WbCursorState = parse_state(cursor);

    // 枚举（只 stat，不打开内容）：`<日期>/*.log`（cli_host）+ `<日期>/sdk/conversations/*.log`
    // （Windows sdk 路线）；mtime 降序 = 最新的文件优先吃预算
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_files: BTreeSet<String> = BTreeSet::new();
    let date_entries = match std::fs::read_dir(&logs_root) {
        Ok(e) => e,
        Err(e) => {
            return Err(UsageError::new(
                "usage-source-io",
                format!(
                    "tps/workbuddy: failed to read logs root {}: {e}",
                    logs_root.display()
                ),
            ))
        }
    };
    for date in date_entries.flatten() {
        let date_dir = date.path();
        if !date_dir.is_dir() {
            continue;
        }
        push_log_files(&date_dir, LogKind::Qua, &mut candidates, &mut seen_files);
        let sdk_conv = date_dir.join("sdk").join("conversations");
        if sdk_conv.is_dir() {
            push_log_files(
                &sdk_conv,
                LogKind::SdkUsage,
                &mut candidates,
                &mut seen_files,
            );
        }
    }
    candidates.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then(a.file_key.cmp(&b.file_key))
    });

    let mut observations: Vec<TpsObservation> = Vec::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    // 解析错误（json 载荷/数值归一拒绝），键 = "字段:形态"，只进 detail 前缀与诊断计数，
    // 绝不含载荷原文（Task 6 按 `parser-errors=<n>` 前缀识别）
    let mut parse_errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut budget = BATCH_CAP;

    // ==== v1→v2 一次性有界恢复（Spec §9；见模块头「游标版本与旧 EOF 恢复」）====
    // 触发条件 = 输入是一份真实存在的旧版游标（非空、反序列化为 v1）。恢复集在本轮选定：
    // 只取 qua 路线（sdk/conversations 本就不产观察），**stat mtime 24h 新鲜窗预过滤发生在
    // 打开内容之前**；选定文件的字节界归零（从字节 0 重读），之后每轮消费到哪、进度记到哪。
    let recovering = !cursor.value.is_empty() && state.format_version < 2;
    if recovering && state.recovery.is_none() {
        let mut pending: Vec<String> = Vec::new();
        for c in &candidates {
            if c.kind != LogKind::Qua || now_ms.saturating_sub(c.mtime_ms) > FRESH_WINDOW_MS {
                continue; // 非 qua 路线 / 超窗：不进恢复集（stat 预过滤，不打开）
            }
            let pos = state.files.entry(c.file_key.clone()).or_default();
            pos.offset = 0;
            pos.skip_lines = 0;
            pending.push(c.file_key.clone());
        }
        state.recovery = Some(RecoveryState { pending });
    }

    for c in candidates {
        if budget == 0 {
            diag(&mut tally, "单批预算用尽，余量下轮续读");
            break;
        }
        let had_pos = state.files.contains_key(&c.file_key);
        // L3 预算：超窗且无水位 → 不打开（有水位 = 以前读过，走便宜的字节界检查）。
        // 恢复集中的文件在恢复集选定时已过 stat 预过滤并建了水位，不受此门重拦。
        if !had_pos && now_ms.saturating_sub(c.mtime_ms) > FRESH_WINDOW_MS {
            continue;
        }
        let mut pos = state.files.remove(&c.file_key).unwrap_or_default();
        read_log(
            &c,
            &mut pos,
            &host,
            &mut observations,
            &mut tally,
            &mut parse_errors,
            &mut budget,
        )?;
        // 该文件本轮已消费到 EOF（字节界到位且无行检查点）→ 若在恢复队列则移出
        let done = pos.skip_lines == 0 && pos.offset >= c.size;
        state.files.insert(c.file_key.clone(), pos);
        if let Some(r) = state.recovery.as_mut() {
            if done && r.pending.contains(&c.file_key) {
                r.pending.retain(|k| *k != c.file_key);
            }
        }
    }
    // 消失文件的游标条目随之丢弃（代价 = 下轮重读，稳定键 UPSERT 幂等）；
    // 恢复队列同理（文件消失 = 该文件无从恢复，与轮转竞态同语义）
    state.files.retain(|k, _| seen_files.contains(k));
    if let Some(r) = state.recovery.as_mut() {
        r.pending.retain(|k| seen_files.contains(k));
    }
    // 恢复完成 → 升 v2（此后 read_incremental 不再进恢复分支，只消费追加字节）
    if state
        .recovery
        .as_ref()
        .is_some_and(|r| r.pending.is_empty())
    {
        state.recovery = None;
        state.format_version = 2;
    }

    let parse_error_total: usize = parse_errors.values().sum();
    let detail = if parse_error_total > 0 {
        let notes = parse_errors
            .iter()
            .map(|(k, n)| format!("{k}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        // 稳定前缀 `parser-errors=<n>`（Task 6 依赖）+ 紧凑的 类型/字段/计数 注记；
        // 只含类型与字段名与计数，绝不含整行载荷
        Some(format!(
            "parser-errors={parse_error_total}; {notes}; {SOURCE_LIMITS}"
        ))
    } else {
        Some(SOURCE_LIMITS.into())
    };

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
            detail,
        },
        diagnostics: render_diag(&tally),
        next_cursor,
    })
}

/// 日志种类（决定行解析器）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    /// cli_host 日志：qua.end 行（Mac 5.8.3 正常路线）
    Qua,
    /// sdk/conversations 日志：Windows 路线，usage_update 只有 hasUsage 标志
    SdkUsage,
}

struct Candidate {
    mtime_ms: i64,
    file_key: String,
    path: PathBuf,
    kind: LogKind,
    size: u64,
}

fn push_log_files(
    dir: &Path,
    kind: LogKind,
    candidates: &mut Vec<Candidate>,
    seen: &mut BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // 目录在枚举间消失/不可读：跳过（D-24 唯一静默形态）
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().map(|x| x == "log").unwrap_or(false) && path.is_file() {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let file_key = path.to_string_lossy().to_string();
            seen.insert(file_key.clone());
            candidates.push(Candidate {
                mtime_ms,
                file_key,
                path,
                kind,
                size: meta.len(),
            });
        }
    }
}

// ==== 单文件增量读取（plain 日志；残缺尾行不越过）====

#[allow(clippy::too_many_arguments)]
fn read_log(
    c: &Candidate,
    pos: &mut FilePos,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    parse_errors: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) -> Result<(), UsageError> {
    // 1) 文件收缩（截断/轮转）：按新文件重读
    if c.size < pos.offset {
        diag(tally, "日志文件收缩，按新文件重读");
        pos.offset = 0;
        pos.skip_lines = 0;
    }
    // 2) 无新增（便宜路径：不打开）
    if pos.offset >= c.size && pos.skip_lines == 0 {
        return Ok(());
    }
    // 3) 增量尾读（独立复核 R3）：从水位 seek，只读新增 ≤ 单批上限——读取量与已消费
    //    历史长度无关。W-06 同族：枚举与读取之间的轮转竞态（文件已消失）才静默跳过，
    //    其余读失败整源 Err——静默少算比响亮失败危险，且 Err 时水位不推进
    let base = pos.offset;
    let (bytes, _eof) = match super::read_tail(&c.path, base, super::TAIL_READ_MAX_BYTES) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            diag(
                tally,
                "log file vanished between enumerate and read (rotation race), skipping this round",
            );
            return Ok(());
        }
        Err(e) => {
            return Err(UsageError::new(
                "usage-source-io",
                format!("tps/workbuddy: failed to read {}: {e}", c.path.display()),
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
            "single log line exceeds tail read cap, not consumed this round",
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
        match c.kind {
            LogKind::Qua => {
                apply_qua_line(lines[i], host, observations, tally, parse_errors, budget)
            }
            LogKind::SdkUsage => apply_sdk_line(lines[i], tally),
        }
        i += 1;
    }
    if i == total {
        pos.offset = base + complete as u64; // 整段消费（上限截断的剩余部分下轮续读）
        pos.skip_lines = 0;
    } else {
        pos.skip_lines = i; // 预算在单元中部耗尽：检查点 =（起点, 已消费行数）
    }
    Ok(())
}

// ==== Windows sdk 路线（缺口态：不产观察，只记账）====

/// `usage_update` 只有 hasUsage=true 标志而无数值 token → 计数 + 诊断（该路线缺口态）
fn apply_sdk_line(line: &str, tally: &mut BTreeMap<String, usize>) {
    let Some(payload) = line_json_payload(line, "usage_update") else {
        return; // 非 usage_update 行：不做流时刻解析（该路线本版无数值 usage，不入速）
    };
    let has_usage_flag = payload.get("hasUsage").and_then(|h| h.as_bool());
    let numeric_tokens = ["completion_tokens", "output_tokens", "total_tokens"]
        .iter()
        .any(|k| payload.get(*k).and_then(|v| v.as_i64()).is_some());
    if has_usage_flag == Some(true) && !numeric_tokens {
        diag(
            tally,
            "insufficient-evidence: usage_update 只有 hasUsage=true 无数值 token → 该路线不入速",
        );
    }
}

// ==== qua.end 行 → 观察 ====

/// `qua.end` 行 → 观察。调用方保证 budget > 0；本函数至多发射一条。
fn apply_qua_line(
    line: &str,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    parse_errors: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    if !line.contains("qua.end") {
        return; // qua.enqueue / qua.start / 其它日志行：不消费
    }
    let Some(payload) = line_json_payload(line, "qua.end") else {
        diag(tally, "qua.end line-without-json-payload skipped");
        record_parse_error(parse_errors, "payload", "bad-json");
        return;
    };
    // 结束时刻 = 日志行前缀时刻（真机 `[M/D/YYYY, h:mm:ss AM|PM.mmm]` 本地时间，RFC3339 兼容；
    // 绝不拿文件 mtime 当事件时刻）
    let Some(ended_at_ms) = line_time_ms(line) else {
        diag(tally, "qua.end 行无可解析时刻前缀 → 跳过（不造时刻）");
        return;
    };
    // 身份四元组（核对 model/request 身份一致才入速；缺一 → 不入速跳过）
    let (Some(session_id), Some(creq), Some(message_id), Some(model)) = (
        str_of(&payload, "conversation_id"),
        str_of(&payload, "conversation_request_id"),
        str_of(&payload, "message_id"),
        str_of(&payload, "model").filter(|m| !m.is_empty()),
    ) else {
        diag(
            tally,
            "qua.end 身份不全（conversation/creq/message/model）→ 跳过（不入速）",
        );
        return;
    };
    // 结果判定：取消位 > 错误位 > finish_reason（stop/tool_calls 为成功收尾）
    let finish_flag = str_of(&payload, "finish_flag").unwrap_or_default();
    let finish_reason = str_of(&payload, "finish_reason").unwrap_or_default();
    let model_error = str_of(&payload, "model_error").unwrap_or_default();
    let (outcome, failure_reason) = if finish_flag == "canceled" {
        (Outcome::Cancelled, Some("finish_flag=canceled".to_string()))
    } else if !model_error.is_empty() {
        (Outcome::Failed, Some(format!("model_error={model_error}")))
    } else {
        match finish_reason.as_str() {
            "stop" | "tool_calls" => (Outcome::Success, None),
            "error" => (Outcome::Failed, Some("finish_reason=error".to_string())),
            "" => (Outcome::Success, None), // 字段缺席按成功收尾（qua.end 只在流末写）
            other => {
                diag(
                    tally,
                    &format!("unknown finish_reason={other:?} -> skip (no guessing)"),
                );
                return;
            }
        }
    };
    // 统一数字归一（Spec §9）：范围内 JSON 整数或合法十进制整数字符串；
    // 空串/布尔/null/浮点/非整数串/溢出按「字段:形态」诊断（不默认 0、不经 f64 中转）
    let total_cost_ms = native_integer_field(&payload, "total_cost_ms", tally, parse_errors);
    let ttft_ms = native_integer_field(&payload, "ttft_ms", tally, parse_errors);
    // 数值 usage：缺 = 记缺失不报 0（Windows hasUsage-only 变体在此落网，绝不入速）；
    // 在场但被归一拒绝同样不入速（上面已按字段诊断，不重复报「缺」）
    let completion_tokens =
        native_integer_field(&payload, "completion_tokens", tally, parse_errors);
    if payload.get("completion_tokens").is_none() {
        diag(
            tally,
            "qua.end 缺数值 usage（completion_tokens）→ 记缺失不报 0，不入速",
        );
    }
    let total = match total_cost_ms {
        Some(t) if t > 0 => t,
        _ => {
            diag(
                tally,
                "qua.end 缺 total_cost_ms（或被拒/非正）→ 无法定位起点，跳过（不造起点）",
            );
            return;
        }
    };
    // 起点护栏：total 若超过行时刻（宿主坏钟/坏数），started 会是负值 → 撞库表
    // CHECK(started_at_ms >= 0) 整批 Err → 游标不推进 → 同批重放死循环。此处保守跳过
    // 该行（既有跳过路径：游标照常越过），绝不造起点也不让单行拖死整源。
    let started_at_ms = match ended_at_ms.checked_sub(total) {
        Some(s) if s >= 0 => s,
        _ => {
            diag(tally, "negative-start field=total_cost_ms record-skipped");
            return;
        }
    };
    let first_token_at_ms = ttft_ms.map(|t| started_at_ms + t);
    // 生成窗 = total_cost_ms − ttft_ms（同请求两段；失败/取消不入速度）
    let generation_ms = match (outcome, ttft_ms) {
        (Outcome::Success, Some(ttft)) => {
            let g = total - ttft;
            if g > 0 {
                Some(g)
            } else {
                diag(tally, "生成窗非正 → 不入速度");
                None
            }
        }
        (Outcome::Success, None) => {
            diag(tally, "缺 ttft_ms → 无生成窗，不入速度");
            None
        }
        _ => None,
    };

    observations.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        stable_call_key: message_id, // 单请求单行、message_id 一请求一值（file-sources.md §6.2）
        started_at_ms,
        first_token_at_ms,
        ended_at_ms,
        // 同条记录同时构造两个窗（Spec §9）：生成窗见上；响应窗 = total（含等待，
        // started → 终态）。ttft 缺失时生成窗 None、响应窗仍在（响应口径单独可用）。
        native_duration_ms: Some(total),
        generation_ms,
        response_elapsed_ms: Some(total),
        output_tokens: completion_tokens,
        // completion_tokens 为供应商总数（含思考与工具参数）→ 已含思考，不再相加
        output_scope: OutputScope::IncludesReasoning,
        tool_ms: None, // qua 行无工具执行时长（见模块头），不填 0
        model,
        channel: None,
        session_key: Some(session_id),
        turn_key: Some(creq), // 原生 conversation-request 身份（Spec §1.4）
        // 标题类辅助请求（request_type 含 topic，supplement-macos §11.2 样本 1）
        purpose: match str_of(&payload, "request_type") {
            Some(rt) if rt.contains("topic") => Purpose::AssistTitle,
            _ => Purpose::MainTask,
        },
        outcome,
        method: TimingMethod::ClientNative,
        failure_reason,
    });
    *budget -= 1;
}

// ==== 统一数字归一（Spec §9）====

/// 数字归一：接受范围内 JSON 整数（含能装进 i64 的 u64）与合法十进制整数字符串
///（可选单个前导 `-` + 纯 ASCII 数字）；拒绝空串、布尔、null、浮点（含 `6711.0`）、
/// 带空白/`+`/小数点/指数的串、NaN/Infinity 字样与 i64 溢出，返回 None 由调用方按
/// 「字段:形态」诊断。**绝不默认 0，绝不经 f64 中转**。
fn parse_native_integer(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(i);
            }
            // 非 i64 的 Number：能装进 i64 的 u64 照收；其余（f64/溢出）拒绝
            n.as_u64().and_then(|u| i64::try_from(u).ok())
        }
        serde_json::Value::String(s) => {
            let digits = s.strip_prefix('-').unwrap_or(s);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None; // 空串/空白/+/. /e/NaN/Infinity 等一切非「-?纯数字」形态
            }
            s.parse::<i64>().ok() // 溢出在此落网（ParseIntError::PosOverflow/NegOverflow）
        }
        _ => None, // 布尔 / null / 数组 / 对象
    }
}

/// 载荷字段按 [`parse_native_integer`] 归一：字段缺席 → None（调用方按「缺」诊断）；
/// 在场但被拒 → 记一条「字段:形态」解析错误 + 诊断，返回 None（不默认 0）
fn native_integer_field(
    payload: &serde_json::Value,
    field: &str,
    tally: &mut BTreeMap<String, usize>,
    parse_errors: &mut BTreeMap<String, usize>,
) -> Option<i64> {
    let raw = payload.get(field)?;
    match parse_native_integer(raw) {
        Some(v) => Some(v),
        None => {
            let kind = reject_kind(raw);
            record_parse_error(parse_errors, field, kind);
            diag(
                tally,
                &format!("qua.end {field} unparsable-numeric kind={kind} rejected-no-zero-default"),
            );
            None
        }
    }
}

/// 被拒值的形态名（只进 detail 注记与诊断，绝不含载荷原文）
fn reject_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(n) => {
            if n.as_u64().is_some() {
                // 已排除 i64 的非负整数 = u64 装不进 i64 → 溢出
                "overflow"
            } else {
                "float" // 含 6711.0 与一切小数/指数形态（NaN/Infinity 本就非法 JSON）
            }
        }
        serde_json::Value::String(s) if s.is_empty() => "empty-string",
        serde_json::Value::String(s) => {
            // 纯整数字面量但装不进 i64 = 溢出；其余一律「非整数串」
            let digits = s.strip_prefix('-').unwrap_or(s);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                "overflow"
            } else {
                "bad-string"
            }
        }
        _ => "other",
    }
}

/// 记一条解析错误（键 = "字段:形态"，detail 前缀只聚合它）
fn record_parse_error(parse_errors: &mut BTreeMap<String, usize>, field: &str, kind: &str) {
    *parse_errors.entry(format!("{field}:{kind}")).or_insert(0) += 1;
}

// ==== 小工具 ====

/// 行内 JSON 载荷：定位 `marker` 后取首个 '{' 到最后一个 '}' 的子串解析
/// （行前缀是日志框架文本，形态宽容；载荷字段名以证据清单为准）
fn line_json_payload(line: &str, marker: &str) -> Option<serde_json::Value> {
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let open = rest.find('{')?;
    let close = rest.rfind('}')?;
    if close < open {
        return None;
    }
    serde_json::from_str(&rest[open..=close]).ok()
}

/// 行前缀时刻（独立复核 R4）：真机日志框架每行以 `[M/D/YYYY, h:mm:ss AM|PM.mmm]` 开头
///（如 `[9/3/2026, 10:03:57 PM.945]`——12 小时制、毫秒缀在 AM/PM 之后、**无时区偏移**），
/// 先按该形态解析并以**本地时区**解释（日志由本机进程按本地钟写入；DST 歧义取较早者）；
/// 再回退 RFC3339 兼容。两者都解不出 → None，调用方诊断跳过——绝不拿文件 mtime 冒充事件时刻。
fn line_time_ms(line: &str) -> Option<i64> {
    parse_bracketed_local_prefix(line).or_else(|| parse_rfc3339_prefix(line))
}

/// `[M/D/YYYY, h:mm:ss AM|PM.mmm]` → 本地时区毫秒时刻；任一字段形态不符 → None（不猜）
fn parse_bracketed_local_prefix(line: &str) -> Option<i64> {
    use chrono::TimeZone;
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let (date, time) = rest[..close].split_once(',')?;
    let mut dp = date.trim().split('/');
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    let year: i32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() {
        return None;
    }
    let (clock, ampm_ms) = time.trim().split_once(' ')?;
    let mut cp = clock.split(':');
    let hour12: u32 = cp.next()?.parse().ok()?;
    let minute: u32 = cp.next()?.parse().ok()?;
    let second: u32 = cp.next()?.parse().ok()?;
    if cp.next().is_some() || !(1..=12).contains(&hour12) {
        return None;
    }
    let (ampm, millis) = ampm_ms.split_once('.')?;
    let millis: u32 = millis.parse().ok()?;
    if millis > 999 {
        return None;
    }
    let hour = match ampm {
        "AM" => hour12 % 12,
        "PM" => hour12 % 12 + 12,
        _ => return None,
    };
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_milli_opt(hour, minute, second, millis)?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp_millis())
}

/// RFC3339 兼容：在前几个空白分隔 token 里尝试解析（容错 `[`/`]`/`,` 修饰）
fn parse_rfc3339_prefix(line: &str) -> Option<i64> {
    line.split_whitespace()
        .take(8)
        .filter_map(|tok| {
            let t = tok.trim_matches(|c| c == '[' || c == ']' || c == ',');
            chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|d| d.timestamp_millis())
        })
        .next()
}

fn str_of(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn parse_state(cursor: &SourceCursor) -> WbCursorState {
    if cursor.value.is_empty() {
        return WbCursorState::default();
    }
    serde_json::from_str(&cursor.value).unwrap_or_else(|e| {
        log::warn!("tps/workbuddy: 游标状态解析失败（{e}）→ 归零重读（稳定键 UPSERT 幂等）");
        WbCursorState::default()
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
