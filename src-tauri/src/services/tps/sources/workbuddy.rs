//! WorkBuddy 原生 TPS 读取器（Spec §4 WorkBuddy 行；证据：
//! probes/2026-10-24-model-speed-supplement-macos.md §4.4/§11.2 —— Mac 5.8.3 正常路线
//! `qua.end` 单行全量载荷；probes/2026-10-24-model-speed-supplement-windows.md §7.5 ——
//! Windows sdk/conversations 路线 `usage_update` 只有 `hasUsage=true` 而无数值 usage；
//! probes/2026-11-02-workbuddy-qua-zhcn-prefix.md —— Windows qua 路线数值齐全、
//! 行前缀时刻随系统区域变化（zh-CN 为 `YYYY/M/D 24h` 形态））。
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
//! ## 游标版本与解析口径修复后的旧 EOF 恢复（Spec §9；迁移纪律）
//!
//! 旧实现把游标推进到 EOF 时可能**零事实或劣质事实**（v1：整数字符串数值行被归一拒绝
//! 而跳过；v2：zh-CN 区域的 qua 行前缀时刻解析不出而整行跳过——2026-11-02 实机缺口；
//! v3：突发交付观察带退化生成窗入库——2026-11-03 实机缺口，需改写为无流式口径）。游标加
//! `format_version`（serde 字段默认 1：旧游标缺字段反序列化即 v1；v2 = 数值归一修复、
//! v3 = 行前缀区域形态修复、v4 = 突发交付闸；本实现新写/推进恒为
//! [`CURSOR_FORMAT_VERSION`]）。读到
//! **版本号低于当前的真实游标**（空游标 = 全新安装，直接当前版本起步无历史可恢复）→
//! 触发**本来源一次性有界恢复**：枚举读取器已知的 qua 日志，**stat 预过滤**只留 mtime
//! 在 24h 新鲜窗内的文件（超窗不打开——AGENTS 反模式禁令：窗口判定先于打开），从字节
//! 0 重新经 `read_tail` 逐轮消费（单轮 ≤8MiB 字节、≤200 观察，残行不越过）；每轮把
//! 「事实 + 恢复进度（待恢复队列 + 各文件字节界）」随 ReadBatch 交给调用方同事务提交
//!（`store::commit_batch`），提交失败水位原样、下轮从检查点续跑；全部文件到 EOF 后
//! 游标升至 [`CURSOR_FORMAT_VERSION`]，此后只消费追加字节、绝不反复全读。稳定键 UPSERT
//! 使重放幂等；只恢复 TPS 事实，不碰旧 Token/费用游标与其他来源游标。
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
//!   结束时刻 = 日志行前缀时刻（已证形态见 `line_time_ms`；**绝不**拿文件 mtime 当事件时刻）；
//!   `started = ended − total_cost_ms`；`first_token = started + ttft_ms`；
//!   `native_duration = total_cost_ms`。
//! * **入速核对**（Spec §4「核对成功 finish、model/request 身份一致」）：finish_reason
//!   `stop` / `tool_calls` = 成功（工具调用后继续输出是正常收尾，supplement-macos §11.2
//!   实测）；`finish_flag=canceled` → Cancelled；`model_error` 非空 → Failed；
//!   未知 finish_reason → 跳过不猜。身份四元组（conversation_id / conversation_request_id /
//!   message_id / model）缺一 → 不入速跳过 + 诊断。
//! * **Windows sdk 路线 = insufficient-evidence 缺口态**（supplement-windows §7.5 定论
//!   「有时刻、无 token」，2026-11-02 复测仍成立）：sdk/conversations 日志的 `usage_update`
//!   只含 `hasUsage=true` 标志、无任何数值 token → 该路线**不产观察、不入速**，来源状态
//!   明示缺口；**绝不借旧 CLI 样本或任何别的路线数据凑值**（Spec §4 WorkBuddy 行）。
//! * **Windows qua 路线 = 已证带数值**（probes/2026-11-02-workbuddy-qua-zhcn-prefix.md）：
//!   桌面端 qua.end 与 Mac 同构（整数字符串数值，归一已兼容）；行前缀时刻形态随系统
//!   区域变化（en-US 12h / zh-CN 24h），解析只认已证形态（见 `line_time_ms`），未证形态
//!   跳过不猜。
//! * **非流式（整包突发）交付 = 无流式口径**（probes/2026-11-03-workbuddy-qua-burst-delivery.md，
//!   2026-11-03 实机）：第三方中转路线会把整段补全在末尾 1~2 个 chunk 一次性送达——
//!   `ttft_ms ≈ total_cost_ms`、`total_chunks` 1~2、`interval_*` 分桶几乎全空（实测
//!   custom-local 670 条中 349 条生成窗 <50ms，官方流式路线 0/59、最短 3761ms）。生成窗
//!   退化到无法构成速率时（窗口 <50ms——该量级窗实测仅见于整包突发；或分桶字段在场而
//!   可分间隔 <3——少于 3 个间隔无法构成速率估计），`generation_ms = None`（同 Codex
//!   缺口语义）：生成窗与首点同时置 None——首 token 与末包同时到达，TTFT 池只收真
//!   流式请求（突发那段时间是全文生成/中转排队，总等待保留在响应窗 started→ended），
//!   观察仅保留响应口径，绝不把突发交付当流式速率入速；分桶字段整体缺席的旧载荷只按
//!   窗口规则兜底。残余缺口：13~20 个可分间隔压缩在短窗的切片式突发（实测仅数条、
//!   TPS 数百）不在此闸，按遥测如实入账。
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
//!   `qua.end` 标记定位行、取行内 JSON 载荷、按已证区域形态解析行前缀时刻（见
//!   `line_time_ms`；未证形态跳过并诊断，不做宽容猜测）。
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

/// 游标格式版本：v1 = 旧实现（无数值归一，EOF 即终点）；v2 = 整数字符串归一修复后的
/// 有界恢复起点；v3 = qua 行前缀区域形态（zh-CN `YYYY/M/D 24h`）修复后的有界恢复起点；
/// v4 = 突发交付闸（退化生成窗改写为无流式口径）后的有界恢复起点。
/// serde 字段缺省恒为 1（旧文件语义），结构体缺省跟随本常量（新起点语义）。
const CURSOR_FORMAT_VERSION: u32 = 4;

/// 突发交付判定阈值（证据：probes/2026-11-03-workbuddy-qua-burst-delivery.md；依据是
/// 经验分布而非机制断言——分桶记的是 chunk 间隔时长，与窗口总长无关）：
/// 生成窗 <50ms 实测仅出现于整包突发（custom-local 349/670 条；官方流式路线 0/59、
/// 最短 3761ms），该量级窗内至多 1~2 个 chunk、速率分母失真；可分间隔 <3 则覆盖
/// 「窗口稍大但交付仍是整包突发」的形态（50~1000ms 桶 80 条中 34 条 chunks≤2）。
/// 残余缺口：13~20 个可分间隔压缩在短窗的切片式突发（实测仅数条、TPS 数百）不在此闸，
/// 按遥测如实入账。
const MIN_STREAMING_WINDOW_MS: i64 = 50;
const MIN_MEASURABLE_INTERVALS: i64 = 3;

/// qua 载荷的 chunk 间隔分桶字段名（可分间隔总数 = 各桶 count 之和）
const INTERVAL_BUCKETS: [&str; 8] = [
    "0_50",
    "50_100",
    "100_500",
    "500_1000",
    "1000_3000",
    "3000_10000",
    "10000_60000",
    "gt_60000",
];

/// 新鲜窗 = 事实保留窗（24h）
const FRESH_WINDOW_MS: i64 = crate::services::tps::store::TPS_RETENTION_MS;

/// 单批发射上限（与 commit_batch 的硬闸同源）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;

/// 来源限制说明（sourceState.detail，如实申报；sdk 路线缺口逐字含
/// insufficient-evidence，Spec §4「缺失时该路线返回 insufficient-evidence」）
const SOURCE_LIMITS: &str = "insufficient-evidence: sdk/conversations 路线 usage_update 只有 hasUsage=true 无数值 usage，不入速，绝不借旧 CLI 样本凑值；qua 路线 Mac/Windows 均已证带数值（2026-11-02 zh-CN 实机），行前缀时刻仅认已证形态（RFC3339 / [M/D/YYYY, h:mm:ss AM|PM.mmm] / [YYYY/M/D H:MM:SS.mmm]），未证形态跳过不猜；突发交付（ttft≈total、chunks 1~2）生成窗与首点均置 None、仅保留响应口径（2026-11-03 实机；生成窗<50ms 或分桶在场而可分间隔<3，切片式突发残余不在闸内）；tool_ms 恒空（无原生工具时长）";

// ==== 游标状态（装在 SourceCursor.value；反序列化失败归零重读——重读的观察以稳定键
//      UPSERT 幂等，不产生双计）====
//
// 版本语义（Spec §9 迁移纪律）：`format_version` 的 **serde 字段默认 = 1**——旧实现写下
// 的游标没有这个字段，反序列化即 v1，版本号低于 [`CURSOR_FORMAT_VERSION`] 即触发一次
// 有界 24h 恢复；本实现新写/推进的游标恒为 [`CURSOR_FORMAT_VERSION`]。全新安装（空游标）
// 走 [`WbCursorState::default`] 直接当前版本起步。两个默认值刻意不同：字段级缺省=1（旧
// 文件语义）、结构体级缺省=当前版本（新起点语义）。

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WbCursorState {
    /// 游标格式版本：旧游标缺字段 → 1（触发有界恢复）；本实现写入恒为 [`CURSOR_FORMAT_VERSION`]
    #[serde(default = "legacy_format_version")]
    format_version: u32,
    /// 日志文件（绝对路径串）→ 已消费字节界与行检查点
    #[serde(default)]
    files: BTreeMap<String, FilePos>,
    /// 恢复中（旧版→当前版本迁移）状态；恢复完成后置空并升至当前版本
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
            format_version: CURSOR_FORMAT_VERSION,
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

    // ==== 旧版游标一次性有界恢复（Spec §9；见模块头「游标版本」）====
    // 触发条件 = 输入是一份真实存在的旧版游标（非空、版本号低于当前）。恢复集在本轮选定：
    // 只取 qua 路线（sdk/conversations 本就不产观察），**stat mtime 24h 新鲜窗预过滤发生在
    // 打开内容之前**；选定文件的字节界归零（从字节 0 重读），之后每轮消费到哪、进度记到哪。
    let recovering = !cursor.value.is_empty() && state.format_version < CURSOR_FORMAT_VERSION;
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
    // 恢复完成 → 升至当前版本（此后 read_incremental 不再进恢复分支，只消费追加字节）
    if state
        .recovery
        .as_ref()
        .is_some_and(|r| r.pending.is_empty())
    {
        state.recovery = None;
        state.format_version = CURSOR_FORMAT_VERSION;
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
    // 结束时刻 = 日志行前缀时刻（已证区域形态见 `line_time_ms` 文档；
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
    let candidate_first_token = ttft_ms.map(|t| started_at_ms + t);
    // 突发交付证据（见模块头「非流式交付」节）：interval 分桶之和 = 可分间隔总数。
    // 各桶是遥测不是身份字段，单个缺席/被拒按 0 计；字段整体缺席 = 旧载荷无法判定，
    // 闸门只在窗口规则上兜底（intervals_present = false）。
    let mut intervals_present = false;
    let mut interval_total: i64 = 0;
    for bucket in INTERVAL_BUCKETS {
        if let Some(v) = payload.get(format!("interval_{bucket}_count").as_str()) {
            intervals_present = true;
            // 遥测字段与身份字段纪律不同：被拒不进 parse_errors（保 Task 6 契约）、
            // 不阻断观察，但计一条 diag 让腐化可见，不静默归 0。
            match parse_native_integer(v) {
                Some(n) => interval_total = interval_total.saturating_add(n),
                None => diag(
                    tally,
                    "qua.end interval 分桶值不可解析，该桶按 0 计（遥测）",
                ),
            }
        }
    }
    // 生成窗 = total_cost_ms − ttft_ms（同请求两段；失败/取消不入速度）。
    // 退化生成窗（整包突发交付）→ 无流式口径：生成窗与首点同时置 None——首 token 与
    // 末包同时到达，「首 Token 等待」的流式语义不存在（那段时间是全文生成/中转排队，
    // 不是等首个输出），总等待仍完整保留在响应窗 started→ended 里。同 Codex 缺口语义。
    let mut burst_delivery = false;
    let generation_ms = match (outcome, ttft_ms) {
        (Outcome::Success, Some(ttft)) => {
            let g = total - ttft;
            if g <= 0 {
                // ttft >= total：首点在结束之后，流式首点同样不可观测（ttft_ok 无上界闸，
                // 不置位会以 total 级 ttft 混进 TTFT 池）
                burst_delivery = true;
                diag(tally, "生成窗非正 → 不入速度");
                None
            } else if g < MIN_STREAMING_WINDOW_MS {
                burst_delivery = true;
                diag(
                    tally,
                    &format!(
                        "突发交付：生成窗<{MIN_STREAMING_WINDOW_MS}ms（该量级窗实测仅见于整包突发）→ 无流式口径，首点不可观测"
                    ),
                );
                None
            } else if intervals_present && interval_total < MIN_MEASURABLE_INTERVALS {
                burst_delivery = true;
                diag(
                    tally,
                    &format!(
                        "突发交付：可分间隔<{MIN_MEASURABLE_INTERVALS} → 无流式口径，首点不可观测"
                    ),
                );
                None
            } else {
                Some(g)
            }
        }
        (Outcome::Success, None) => {
            diag(tally, "缺 ttft_ms → 无生成窗，不入速度");
            None
        }
        _ => None,
    };
    let first_token_at_ms = if burst_delivery {
        None
    } else {
        candidate_first_token
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

/// 行前缀时刻（独立复核 R4）：真机日志框架每行以 `[…]` 开头，时刻形态随系统区域变化，
/// 已证两种——en-US `[M/D/YYYY, h:mm:ss AM|PM.mmm]`（12 小时制、毫秒缀在 AM/PM 之后）、
/// zh-CN `[YYYY/M/D H:MM:SS.mmm]`（24 小时制，2026-11-02 实机证据）。先按已证形态解析并以
/// **本地时区**解释（日志由本机进程按本地钟写入；DST 歧义取较早者）；再回退 RFC3339 兼容。
/// 全部解不出 → None，调用方诊断跳过——绝不拿文件 mtime 冒充事件时刻、绝不猜未证形态。
fn line_time_ms(line: &str) -> Option<i64> {
    parse_bracketed_local_prefix(line).or_else(|| parse_rfc3339_prefix(line))
}

/// `[…]` 内文本按已证区域形态解析（en-US 在前、zh-CN 兜底；两形态互斥不会误吸）
fn parse_bracketed_local_prefix(line: &str) -> Option<i64> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let inner = &rest[..close];
    parse_mdy_ampm_local(inner).or_else(|| parse_ymd24_local(inner))
}

/// `[M/D/YYYY, h:mm:ss AM|PM.mmm]` → 本地时区毫秒时刻；任一字段形态不符 → None（不猜）
fn parse_mdy_ampm_local(inner: &str) -> Option<i64> {
    use chrono::TimeZone;
    let (date, time) = inner.split_once(',')?;
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

/// `[YYYY/M/D H:MM:SS.mmm]` → 本地时区毫秒时刻（zh-CN Windows 实机形态，2026-11-02
/// 取证：24 小时制、毫秒必有）；任一字段形态不符 → None（不猜）
fn parse_ymd24_local(inner: &str) -> Option<i64> {
    use chrono::TimeZone;
    let (date, clock_ms) = inner.split_once(' ')?;
    let mut dp = date.split('/');
    let year: i32 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() {
        return None;
    }
    let (clock, millis) = clock_ms.split_once('.')?;
    let mut cp = clock.split(':');
    let hour: u32 = cp.next()?.parse().ok()?;
    let minute: u32 = cp.next()?.parse().ok()?;
    let second: u32 = cp.next()?.parse().ok()?;
    if cp.next().is_some() || hour > 23 {
        return None;
    }
    let millis: u32 = millis.parse().ok()?;
    if millis > 999 {
        return None;
    }
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

// ==== 本模块贴身单测（主断言走 read_incremental 公开入口，与真实消费路径一致）====
//
// 覆盖面：行前缀时刻三形态（zh-CN / en-US / RFC3339）+ 无日期前缀跳过闸 +
// 旧版 EOF 游标（v1/v2/v3）的有界恢复与 v4 升版 + 全新安装 v4 起步 +
// qua.end 突发交付闸门（单 chunk 窄窗 / 大窗少桶 / 生成窗非正 ttft≥total → 生成窗与
// 首 Token 等待 None；真流式 → 两者保留；旧载荷无分桶字段 → 只按窗口规则，首点随
// 窗口闸同进退）。
// 临时 home 用 std::env::temp_dir() 下的唯一子目录，绝不触真机 ~/.tuvis / ~/.workbuddy。
#[cfg(test)]
mod tests {
    use super::*;

    /// 真机 qua.end 载荷字段清单（数值字段为整数字符串形态，证据见模块头）；
    /// message_id 为占位值，由 qua_end_line 按测试替换，保证稳定键各用例唯一
    const QUA_END_PAYLOAD: &str = r#"{"conversation_id":"c0bb5964-d5c7-4b9d-9d45-264741ccc144","conversation_request_id":"01a19b1b800475aab1c452c34d30f7db","prompt_request_id":"1a9b748ebc55426db1132dd0ce92b589","message_id":"01a19b1b88517505b26a0fde09620cad","model":"minimax-m3.1-flash","agent_name":"cli","scene":"craft","request_type":"chat","finish_flag":"done","finish_reason":"tool_calls","model_error":"","completion_tokens":"1188","total_cost_ms":"17541","ttft_ms":"7500","prompt_tokens":"3200"}"#;

    /// 独立临时 home：std::env::temp_dir() 下按测试名 + 纳秒建唯一目录（并行不串扰）
    fn temp_home(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("workbuddy-tps-{name}-{nanos}"));
        std::fs::create_dir_all(home.join(".workbuddy").join("logs").join("2026-11-02")).unwrap();
        home
    }

    /// 写 cli_host 日志（每行以 '\n' 结尾 = 整行完整消费），返回文件路径
    fn write_log(home: &Path, lines: &[String]) -> PathBuf {
        let path = home
            .join(".workbuddy")
            .join("logs")
            .join("2026-11-02")
            .join("DM_test.log");
        let mut data = String::new();
        for l in lines {
            data.push_str(l);
            data.push('\n');
        }
        std::fs::write(&path, data.as_bytes()).unwrap();
        path
    }

    /// 真机形态 qua.end 行：行前缀时刻 + 日志框架文本 + qua.end 标记 + JSON 载荷
    fn qua_end_line(prefix: &str, message_id: &str) -> String {
        let payload = QUA_END_PAYLOAD.replace("01a19b1b88517505b26a0fde09620cad", message_id);
        format!(
            "{prefix} [Info] [pid=30436] [ModelProvider]  [SseMonitor#01a19b1b] qua.end: {payload}"
        )
    }

    /// 突发交付样例载荷基座（2026-11-03 实机取证：第三方中转路线 custom-local 的整包
    /// 突发交付形态——ttft ≈ total、total_chunks 1~2、8 个 interval 分桶几乎全空；
    /// 实测 670 条中 349 条生成窗 <50ms，官方流式路线 minimax 59 条 0 条如此）。
    /// message_id 用 MSGIDUNIQUE 占位，由 burst_qua_line 按测试替换保证稳定键唯一
    const BURST_BASE_PAYLOAD: &str = r#"{"conversation_id":"c0bb5964-d5c7-4b9d-9d45-264741ccc144","conversation_request_id":"01a19b1b800475aab1c452c34d30f7db","message_id":"MSGIDUNIQUE","model":"custom-local:glm-5.3-flash","request_type":"chat","finish_flag":"done","finish_reason":"tool_calls","model_error":"","completion_tokens":"659","total_cost_ms":"87962","ttft_ms":"87959"}"#;

    /// qua 载荷的 8 个 interval 分桶字段名（流式间隔分桶，突发交付闸门的读取面）
    const INTERVAL_KEYS: [&str; 8] = [
        "interval_0_50_count",
        "interval_50_100_count",
        "interval_100_500_count",
        "interval_500_1000_count",
        "interval_1000_3000_count",
        "interval_3000_10000_count",
        "interval_10000_60000_count",
        "interval_gt_60000_count",
    ];

    /// 8 桶字段段（前导逗号 + `"键":"值"` 对，值与 [`INTERVAL_KEYS`] 一一对应）
    fn interval_fields(values: [&str; 8]) -> String {
        let mut s = String::new();
        for (k, v) in INTERVAL_KEYS.iter().zip(values.iter()) {
            s.push_str(&format!(r#","{k}":"{v}""#));
        }
        s
    }

    /// 突发交付用例的 qua.end 行：BASE 基座替换 message_id（稳定键唯一），completion /
    /// total / ttft 按用例覆写，载荷末尾追加 `extra_fields`（前导逗号的字段段；空串不加）
    fn burst_qua_line(
        prefix: &str,
        message_id: &str,
        completion_tokens: &str,
        total_cost_ms: &str,
        ttft_ms: &str,
        extra_fields: &str,
    ) -> String {
        let mut payload = BURST_BASE_PAYLOAD
            .replace("MSGIDUNIQUE", message_id)
            .replace(
                r#""completion_tokens":"659""#,
                &format!(r#""completion_tokens":"{completion_tokens}""#),
            )
            .replace(
                r#""total_cost_ms":"87962""#,
                &format!(r#""total_cost_ms":"{total_cost_ms}""#),
            )
            .replace(r#""ttft_ms":"87959""#, &format!(r#""ttft_ms":"{ttft_ms}""#));
        if !extra_fields.is_empty() {
            // 去掉闭括号、追加字段段后补回（BASE 以 '}' 结尾）
            let body = payload.trim_end_matches('}');
            payload = format!("{body}{extra_fields}}}");
        }
        format!(
            "{prefix} [Info] [pid=30436] [ModelProvider]  [SseMonitor#01a19b1b] qua.end: {payload}"
        )
    }

    /// 本地时区毫秒时刻（与实现同式解释行前缀 naive 时刻，不硬编码 epoch）
    fn local_ms(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32, ms: u32) -> i64 {
        use chrono::TimeZone;
        let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_milli_opt(hour, min, sec, ms)
            .unwrap();
        chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    /// 公开入口读取（now_ms = 当前时刻；文件刚写 = 24h 新鲜窗内）
    fn read(home: &Path, cursor: &SourceCursor) -> ReadBatch {
        read_incremental(home, cursor, chrono::Utc::now().timestamp_millis()).unwrap()
    }

    /// 单条观察完整事实面断言（三形态前缀用例共用同一口径）：
    /// total=17541 / ttft=7500 / completion=1188 的算式逐字段对齐
    fn assert_full_observation(obs: &TpsObservation, message_id: &str, ended_at_ms: i64) {
        let started = ended_at_ms - 17541;
        assert_eq!(obs.source, "workbuddy");
        assert_eq!(obs.stable_call_key, message_id);
        assert_eq!(obs.ended_at_ms, ended_at_ms);
        assert_eq!(obs.started_at_ms, started);
        assert_eq!(obs.first_token_at_ms, Some(started + 7500));
        assert_eq!(obs.native_duration_ms, Some(17541));
        assert_eq!(obs.generation_ms, Some(17541 - 7500)); // 生成窗 = total − ttft
        assert_eq!(obs.response_elapsed_ms, Some(17541)); // 响应窗 = total
        assert_eq!(obs.output_tokens, Some(1188));
        assert_eq!(obs.output_scope, OutputScope::IncludesReasoning);
        assert_eq!(obs.tool_ms, None);
        assert_eq!(
            obs.session_key.as_deref(),
            Some("c0bb5964-d5c7-4b9d-9d45-264741ccc144")
        );
        assert_eq!(
            obs.turn_key.as_deref(),
            Some("01a19b1b800475aab1c452c34d30f7db")
        );
        assert_eq!(obs.model, "minimax-m3.1-flash");
        assert_eq!(obs.purpose, Purpose::MainTask);
        assert_eq!(obs.outcome, Outcome::Success);
        assert_eq!(obs.method, TimingMethod::ClientNative);
        assert_eq!(obs.failure_reason, None);
    }

    /// zh-CN 区域前缀（`[YYYY/M/D H:MM:SS.mmm]`：24 小时制、无逗号、毫秒必有）→
    /// 产出恰好一条完整观察。真机 zh-CN Windows 的 qua.end 全部落此形态，
    /// 修复前全部被「行无可解析时刻前缀 → 跳过」闸丢弃（速度观察 0 条）。
    #[test]
    fn zh_cn_prefix_line_produces_observation() {
        use chrono::TimeZone;
        let home = temp_home("zh-cn");
        let line = qua_end_line(
            "[2026/11/2 15:14:12.917]",
            "01a19b1b88517505b26a0fde09620cad",
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(
            batch.observations.len(),
            1,
            "zh-CN 前缀行必须产出观察，不得被时刻前缀闸丢弃"
        );
        // 期望结束时刻 = 行前缀 naive 时刻的本地时区解释（与实现同式，不依赖硬编码 epoch）
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_milli_opt(15, 14, 12, 917)
            .unwrap();
        let ended = chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .timestamp_millis();
        assert_full_observation(
            &batch.observations[0],
            "01a19b1b88517505b26a0fde09620cad",
            ended,
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// en-US 区域前缀（`[M/D/YYYY, h:mm:ss AM|PM.mmm]` 12 小时制）回归护栏：
    /// 修复前就该绿；引入 zh-CN 形态后必须仍绿（不得破坏既有解析）。
    #[test]
    fn en_us_prefix_line_produces_observation() {
        use chrono::TimeZone;
        let home = temp_home("en-us");
        let line = qua_end_line(
            "[11/2/2026, 3:14:12 PM.917]",
            "02a19b1b88517505b26a0fde09620cad",
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(
            batch.observations.len(),
            1,
            "en-US 前缀行（既有形态）必须产出观察"
        );
        // 3:14:12 PM = 15:14:12.917，与 zh-CN 用例同一本地时刻（期望值同式计算）
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_milli_opt(15, 14, 12, 917)
            .unwrap();
        let ended = chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .timestamp_millis();
        assert_full_observation(
            &batch.observations[0],
            "02a19b1b88517505b26a0fde09620cad",
            ended,
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// RFC3339 前缀回归护栏：修复前就该绿；改后必须仍绿。
    /// 期望结束时刻 = RFC3339 字符串的 epoch 毫秒（无时区假设，逐字解析）。
    #[test]
    fn rfc3339_prefix_line_produces_observation() {
        let home = temp_home("rfc3339");
        let line = qua_end_line(
            "2026-11-02T07:14:12.917Z",
            "03a19b1b88517505b26a0fde09620cad",
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(
            batch.observations.len(),
            1,
            "RFC3339 前缀行（既有形态）必须产出观察"
        );
        let ended = chrono::DateTime::parse_from_rfc3339("2026-11-02T07:14:12.917Z")
            .unwrap()
            .timestamp_millis();
        assert_full_observation(
            &batch.observations[0],
            "03a19b1b88517505b26a0fde09620cad",
            ended,
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 只有时刻没有日期的前缀（`[15:14:12.917]`）→ 无法定位事件时刻，行被跳过：
    /// 0 条观察、来源状态 ok（正常跳过而非读取失败——绝不造时刻）。
    #[test]
    fn unparseable_prefix_line_skipped() {
        let home = temp_home("no-date");
        let line = qua_end_line("[15:14:12.917]", "04a19b1b88517505b26a0fde09620cad");
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert!(
            batch.observations.is_empty(),
            "无日期前缀不得产出观察（不猜时刻）"
        );
        assert!(batch.source_state.ok, "跳过是正常空态，来源状态必须 ok");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// v2 EOF 游标（offset 已到文件尾）→ 触发一次有界恢复：字节 0 重读两条 zh-CN 行
    /// 全部产出观察；恢复完成后游标升 v4 且 recovery 清空（复用旧版迁移机制）。
    #[test]
    fn v2_cursor_eof_triggers_bounded_rescan_and_upgrades_v4() {
        let home = temp_home("v2-rescan");
        let lines = [
            qua_end_line(
                "[2026/11/2 15:14:12.917]",
                "05a19b1b88517505b26a0fde09620cad",
            ),
            qua_end_line(
                "[2026/11/2 15:15:30.123]",
                "06a19b1b88517505b26a0fde09620cad",
            ),
        ];
        let path = write_log(&home, &lines);
        let size = std::fs::metadata(&path).unwrap().len();
        // v2 EOF 游标：offset = 文件字节数（全部历史已在旧版本消费过）
        let file_key = path.to_string_lossy().to_string();
        let cursor = SourceCursor {
            source: SOURCE_ID.into(),
            value: serde_json::json!({
                "format_version": 2,
                "files": { file_key: {"offset": size, "skip_lines": 0} }
            })
            .to_string(),
        };
        let batch = read(&home, &cursor);
        assert_eq!(
            batch.observations.len(),
            2,
            "EOF 的 v2 游标必须触发有界恢复（退回字节 0 重读），不得把事实锁死在旧版游标里"
        );
        let next: serde_json::Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
        assert_eq!(
            next["format_version"].as_u64(),
            Some(4),
            "恢复完成后游标必须升 v4"
        );
        assert!(next["recovery"].is_null(), "恢复完成后 recovery 必须清空");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 全新安装（空游标）+ 一条 zh-CN 行 → 直接 v4 起步、无恢复（无历史可恢复）、
    /// 恰好 1 条观察。
    #[test]
    fn fresh_install_starts_v4_without_recovery() {
        let home = temp_home("fresh-v4");
        let line = qua_end_line(
            "[2026/11/2 15:14:12.917]",
            "07a19b1b88517505b26a0fde09620cad",
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(batch.observations.len(), 1, "全新安装首读必须产出观察");
        let next: serde_json::Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
        assert_eq!(
            next["format_version"].as_u64(),
            Some(4),
            "全新安装必须直接以 v4 游标起步"
        );
        assert!(next["recovery"].is_null(), "全新安装无历史，不得触发恢复");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 旧版游标恢复后的第二轮 = append-only 稳定（恢复幂等）：v2 EOF 游标第一轮触发
    /// 有界恢复（两条 zh-CN 行全部产出、升 v4），返回的 v4 游标**原样**喂回第二轮——
    /// 0 条观察（不再全读）、返回游标与输入逐字节一致（版本不回退、状态无抖动）。
    #[test]
    fn recovery_second_round_is_append_only_and_stable() {
        let home = temp_home("recovery-stable");
        let lines = [
            qua_end_line(
                "[2026/11/2 15:14:12.917]",
                "08a19b1b88517505b26a0fde09620cad",
            ),
            qua_end_line(
                "[2026/11/2 15:15:30.123]",
                "09a19b1b88517505b26a0fde09620cad",
            ),
        ];
        let path = write_log(&home, &lines);
        let size = std::fs::metadata(&path).unwrap().len();
        let file_key = path.to_string_lossy().to_string();
        // v2 EOF 游标：offset = 文件字节数（全部历史已在旧版本消费过）
        let v2_cursor = SourceCursor {
            source: SOURCE_ID.into(),
            value: serde_json::json!({
                "format_version": 2,
                "files": { file_key: {"offset": size, "skip_lines": 0} }
            })
            .to_string(),
        };
        // 第一轮：恢复 + 升 v4
        let batch1 = read(&home, &v2_cursor);
        assert_eq!(
            batch1.observations.len(),
            2,
            "v2 EOF 游标必须触发有界恢复并产出全部两条观察"
        );
        let recovered: serde_json::Value = serde_json::from_str(&batch1.next_cursor.value).unwrap();
        assert_eq!(
            recovered["format_version"].as_u64(),
            Some(4),
            "恢复完成后游标必须升 v4"
        );
        assert!(
            recovered["recovery"].is_null(),
            "恢复完成后 recovery 必须清空"
        );
        // 第二轮：v4 游标原样喂回 → 只消费追加字节（本文件无追加 = 0 条）
        let batch2 = read(&home, &batch1.next_cursor);
        assert!(
            batch2.observations.is_empty(),
            "v4 游标第二轮不得再产出观察（append-only：不得反复全读）"
        );
        assert_eq!(
            batch2.next_cursor.value, batch1.next_cursor.value,
            "第二轮返回游标必须与输入逐字节一致（版本不回退、状态无抖动）"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// zh-CN 形态三条越界/缺毫秒前缀（月 13 / 时 24 / 毫秒缺失）→ 各自被诊断跳过：
    /// 0 条观察、来源 ok、诊断恰好聚合为一条 3x「无可解析时刻前缀」（不造时刻、
    /// 毫秒缺失不得默认 0 放行）。诊断计数同时防负向用例假绿（因其他原因被跳过）。
    #[test]
    fn zh_cn_malformed_prefixes_are_skipped_without_guessing() {
        let home = temp_home("zh-cn-bad");
        let lines = [
            // 月越界（13 月）：
            qua_end_line(
                "[2026/13/1 15:14:12.917]",
                "10a19b1b88517505b26a0fde09620cad",
            ),
            // hour 越界（24 点）：
            qua_end_line(
                "[2026/11/2 24:14:12.917]",
                "11a19b1b88517505b26a0fde09620cad",
            ),
            // 毫秒缺失（已证形态毫秒必有，不得默认 0 放行）：
            qua_end_line("[2026/11/2 15:14:12]", "12a19b1b88517505b26a0fde09620cad"),
        ];
        write_log(&home, &lines);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert!(
            batch.observations.is_empty(),
            "越界/缺毫秒前缀不得产出观察（不造值）"
        );
        assert!(
            batch.source_state.ok,
            "逐行诊断跳过是正常空态，来源状态必须 ok"
        );
        assert_eq!(
            batch.diagnostics.len(),
            1,
            "三条行的跳过必须按同原因聚合为一条诊断，不得有其他诊断混入"
        );
        assert!(
            batch.diagnostics[0].contains("3x")
                && batch.diagnostics[0].contains("无可解析时刻前缀"),
            "诊断必须计数 3（每行各自跳过）且原因落在时刻前缀闸上：{}",
            batch.diagnostics[0]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// v1 旧游标（无 format_version 字段 → serde 字段缺省 = 1）与 v2 同路触发有界恢复：
    /// 两条 zh-CN 行全部产出观察，游标升至 v4。
    #[test]
    fn legacy_v1_cursor_without_version_field_also_recovers() {
        let home = temp_home("v1-rescan");
        let lines = [
            qua_end_line(
                "[2026/11/2 15:14:12.917]",
                "13a19b1b88517505b26a0fde09620cad",
            ),
            qua_end_line(
                "[2026/11/2 15:15:30.123]",
                "14a19b1b88517505b26a0fde09620cad",
            ),
        ];
        let path = write_log(&home, &lines);
        let size = std::fs::metadata(&path).unwrap().len();
        let file_key = path.to_string_lossy().to_string();
        // 旧实现写下的游标没有 format_version 字段（字段缺省反序列化即 v1）
        let cursor = SourceCursor {
            source: SOURCE_ID.into(),
            value: serde_json::json!({
                "files": { file_key: {"offset": size, "skip_lines": 0} }
            })
            .to_string(),
        };
        let batch = read(&home, &cursor);
        assert_eq!(
            batch.observations.len(),
            2,
            "无版本字段的旧游标必须按 v1 处理并触发有界恢复，不得把事实锁死在旧游标里"
        );
        let next: serde_json::Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
        assert_eq!(
            next["format_version"].as_u64(),
            Some(4),
            "恢复完成后游标必须升 v4"
        );
        assert!(next["recovery"].is_null(), "恢复完成后 recovery 必须清空");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 突发交付闸门·单 chunk 窄窗（整包突发形态，2026-11-03 实机取证）：生成窗仅 3ms
    ///（< qua 最小分桶粒度 50ms，不可能有可分间隔）→ 生成窗与首 Token 等待同闸置 None
    ///（ttft ≈ total 是全文生成/中转排队耗时，不是「等首个输出」，入 TTFT 池会污染
    /// 首Token平均等待）；观察照发，响应窗（= total）保留，outcome 不改判。
    /// 修复前按流式速率口径给 Some(3)（预期红）。
    #[test]
    fn burst_delivery_single_chunk_has_no_stream_window() {
        let home = temp_home("burst-single-chunk");
        let mut extra = r#","total_chunks":"1""#.to_string();
        extra.push_str(&interval_fields(["0"; 8]));
        let line = burst_qua_line(
            "[2026/11/3 12:12:22.000]",
            "15a19b1b800475aab1c452c34d30f7db",
            "659",
            "87962",
            "87959",
            &extra,
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(
            batch.observations.len(),
            1,
            "突发交付只禁生成窗，事实观察必须照发"
        );
        let obs = &batch.observations[0];
        assert_eq!(
            obs.generation_ms, None,
            "生成窗 3ms < 50ms 且单 chunk → 不得按流式速率给生成窗"
        );
        assert_eq!(
            obs.response_elapsed_ms,
            Some(87962),
            "响应窗 = total（含等待），必须保留"
        );
        assert_eq!(
            obs.first_token_at_ms, None,
            "突发交付首点同闸置 None：ttft ≈ total 不得入 TTFT 池"
        );
        assert_eq!(obs.outcome, Outcome::Success, "突发交付不改判 outcome");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 突发交付闸门·大窗少桶：生成窗 103ms（≥ 最小分桶粒度 50ms）但 8 桶可分间隔
    /// 之和只有 1（< 3，无法构成速率）→ 生成窗置 None、响应窗保留。
    /// 修复前按窗口规则给 Some(103)（预期红）。
    #[test]
    fn burst_delivery_few_intervals_over_larger_window_has_no_stream_window() {
        let home = temp_home("burst-few-intervals");
        let mut extra = r#","total_chunks":"2""#.to_string();
        let mut buckets = ["0"; 8];
        buckets[0] = "1"; // 仅 interval_0_50_count=1：可分间隔和 = 1 < 3
        extra.push_str(&interval_fields(buckets));
        let line = burst_qua_line(
            "[2026/11/3 12:12:22.000]",
            "16a19b1b800475aab1c452c34d30f7db",
            "659",
            "15409",
            "15306",
            &extra,
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(batch.observations.len(), 1, "观察照发");
        assert_eq!(
            batch.observations[0].generation_ms, None,
            "可分间隔和 1 < 3 → 无法构成速率，生成窗必须置 None"
        );
        assert_eq!(
            batch.observations[0].response_elapsed_ms,
            Some(15409),
            "响应窗 = total，必须保留"
        );
        assert_eq!(
            batch.observations[0].first_token_at_ms, None,
            "间隔闸触发的突发交付首点同置 None，不得污染 TTFT 池"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 护栏·真流式不得误伤：生成窗 10041ms（≥50）、8 桶可分间隔和 57（≥3）→
    /// 生成窗 Some(10041)、首点 Some(started + 7500) 全保留（突发交付置 None 的
    /// 对照面）。修复前就该绿，修复后必须仍绿。
    #[test]
    fn streaming_delivery_keeps_generation_window() {
        let home = temp_home("streaming-keep");
        let mut extra = r#","total_chunks":"58""#.to_string();
        let mut buckets = ["0"; 8];
        buckets[0] = "21"; // interval_0_50_count
        buckets[2] = "33"; // interval_100_500_count
        buckets[3] = "3"; // interval_500_1000_count
        extra.push_str(&interval_fields(buckets));
        let line = burst_qua_line(
            "[2026/11/3 12:12:22.000]",
            "17a19b1b800475aab1c452c34d30f7db",
            "1188",
            "17541",
            "7500",
            &extra,
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(batch.observations.len(), 1, "真流式行观察照发");
        assert_eq!(
            batch.observations[0].generation_ms,
            Some(17541 - 7500),
            "真流式（间隔和 57 ≥ 3、窗 ≥ 50ms）生成窗必须保留"
        );
        let started = local_ms(2026, 11, 3, 12, 12, 22, 0) - 17541;
        assert_eq!(
            batch.observations[0].first_token_at_ms,
            Some(started + 7500),
            "真流式首点必须保留（突发交付置 None 的对照面）"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 旧载荷（无任何 interval/chunks 字段）不触发间隔闸，只按窗口规则（首点随窗口
    /// 闸同进退）：窗 10041ms ≥50 → 生成窗 Some(10041)、首点 Some（现在就该绿）；
    /// 窗 10ms <50 → 生成窗 None、首点 None（修复前分别给 Some(10041)/Some(10)，
    /// 首点断言预期红）。
    #[test]
    fn legacy_payload_without_interval_fields_uses_window_rule_only() {
        let home = temp_home("legacy-no-interval");
        // 行 A：旧载荷形态（无 interval/chunks 字段）+ 健康窗（total=17541, ttft=7500）
        let line_a = burst_qua_line(
            "[2026/11/3 12:12:22.000]",
            "18a19b1b800475aab1c452c34d30f7db",
            "659",
            "17541",
            "7500",
            "",
        );
        // 行 B：同样无 interval 字段，生成窗 10ms（total=2000, ttft=1990）→ 低于粒度
        let line_b = burst_qua_line(
            "[2026/11/3 12:12:30.000]",
            "19a19b1b800475aab1c452c34d30f7db",
            "659",
            "2000",
            "1990",
            "",
        );
        write_log(&home, &[line_a, line_b]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(batch.observations.len(), 2, "两条旧载荷行观察照发");
        let key_a = "18a19b1b800475aab1c452c34d30f7db";
        let key_b = "19a19b1b800475aab1c452c34d30f7db";
        let a = batch
            .observations
            .iter()
            .find(|o| o.stable_call_key == key_a)
            .unwrap();
        let b = batch
            .observations
            .iter()
            .find(|o| o.stable_call_key == key_b)
            .unwrap();
        assert_eq!(
            a.generation_ms,
            Some(10041),
            "旧载荷无分桶字段 → 只按窗口规则：窗 10041ms ≥ 50 保留"
        );
        // 行 A 健康窗非突发：首点 = started + ttft = (ended − 17541) + 7500，必须保留
        assert_eq!(
            a.first_token_at_ms,
            Some(local_ms(2026, 11, 3, 12, 12, 22, 0) - 17541 + 7500),
            "旧载荷健康窗（非突发）首点保留（started + ttft）"
        );
        assert_eq!(
            b.generation_ms, None,
            "旧载荷无分桶字段 → 只按窗口规则：窗 10ms < 50 置 None"
        );
        assert_eq!(
            b.first_token_at_ms, None,
            "窗口规则（10ms < 50）同闸置 None 首点，不得污染 TTFT 池"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// v3 EOF 游标（v4 语义下的旧版游标）与 v2 同路触发有界恢复：两条 zh-CN 行全部
    /// 产出观察，游标升 v4。修复前 v3 = 当前版本（不触发恢复：0 条且停在 v3，预期红）。
    #[test]
    fn v3_cursor_eof_triggers_rescan_upgrade_to_v4() {
        let home = temp_home("v3-rescan");
        let lines = [
            qua_end_line(
                "[2026/11/2 15:14:12.917]",
                "20a19b1b88517505b26a0fde09620cad",
            ),
            qua_end_line(
                "[2026/11/2 15:15:30.123]",
                "21a19b1b88517505b26a0fde09620cad",
            ),
        ];
        let path = write_log(&home, &lines);
        let size = std::fs::metadata(&path).unwrap().len();
        let file_key = path.to_string_lossy().to_string();
        // v3 EOF 游标：offset = 文件字节数（全部历史已在旧版本消费过）
        let cursor = SourceCursor {
            source: SOURCE_ID.into(),
            value: serde_json::json!({
                "format_version": 3,
                "files": { file_key: {"offset": size, "skip_lines": 0} }
            })
            .to_string(),
        };
        let batch = read(&home, &cursor);
        assert_eq!(
            batch.observations.len(),
            2,
            "EOF 的 v3 旧游标必须触发有界恢复（退回字节 0 重读）"
        );
        let next: serde_json::Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
        assert_eq!(
            next["format_version"].as_u64(),
            Some(4),
            "恢复完成后游标必须升 v4"
        );
        assert!(next["recovery"].is_null(), "恢复完成后 recovery 必须清空");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 退化生成窗·g ≤ 0（ttft ≥ total，宿主坏数形态）与非正窗同闸：生成窗 None 且
    /// 首点 None——ttft_ok 缺「首点 ≤ 结束」上界闸，不置位时该分支首点会以 total 级
    /// ttft 混进 TTFT 池（与 87.96s 同形污染）；观察照发、响应窗保留、outcome 不改判。
    #[test]
    fn nonpositive_generation_window_also_clears_first_token() {
        let home = temp_home("nonpositive-window");
        // BASE 载荷改 total=10000 / ttft=10500（g = −500），不加 interval 字段
        let line = burst_qua_line(
            "[2026/11/3 12:12:22.000]",
            "22a19b1b800475aab1c452c34d30f7db",
            "659",
            "10000",
            "10500",
            "",
        );
        write_log(&home, &[line]);
        let batch = read(&home, &SourceCursor::empty(SOURCE_ID));
        assert_eq!(
            batch.observations.len(),
            1,
            "g ≤ 0 只是退化口径，事实观察必须照发"
        );
        let obs = &batch.observations[0];
        assert_eq!(obs.generation_ms, None, "g = −500 → 无生成窗");
        assert_eq!(
            obs.first_token_at_ms, None,
            "g ≤ 0 分支同置位突发交付：total 级 ttft 不得入 TTFT 池"
        );
        assert_eq!(
            obs.response_elapsed_ms,
            Some(10000),
            "响应窗 = total（含等待），必须保留"
        );
        assert_eq!(obs.outcome, Outcome::Success, "g ≤ 0 不改判 outcome");
        let _ = std::fs::remove_dir_all(&home);
    }
}
