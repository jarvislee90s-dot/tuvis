//! Codex 原生 rollout 有界增量读取器（2026-10-26 桌面工作区与输出速度 V2 计划 Task 5；
//! Spec §6「Codex 响应观测接入」）。
//!
//! ## 读取对象与口径
//!
//! * 根 = `<home>/.codex/sessions/**/rollout-*.jsonl`（与 usage 采集器
//!   `services/usage/collectors/codex.rs` 同一根定位约定；`archived_sessions` 的旧
//!   归档不进 TPS——事实保留窗 24h，归档不新鲜）。游标键 = **相对根的路径**。
//! * 事实来自原生 `token_usage_record` 封口的日志近似响应窗（Task 4 解码器：
//!   `decoder::push_record`）；`generation_ms` / `first_token_at_ms` /
//!   `native_duration_ms` 恒缺失——Codex 无同调用流式窗，stream 仍是真实缺口
//!   （`insufficient-generation-timing`），绝不以回合墙钟补窗。
//! * **每文件一份解码器状态**（模型/渠道/会话事实与未决封口绝不跨文件泄漏），
//!   随游标持久化跨批次续算；`session_meta` 的 `cli_version` / `model_provider`
//!   作为头事实入游标（Spec §6.2.6 记录宿主版本）。
//!
//! ## 增量、冷启动与预算（Spec §6.3 + AGENTS.md 采集域纪律）
//!
//! * **游标 v1**（`SourceCursor.value` 内的 [`CodexSourceCursor`]）：逐文件
//!   offset（完整行字节界）/ size / 冷启动标记 / 解码器状态 / 头事实。
//!   `format_version` 是**必填**字段：任何旧「不提供流式 TPS」占位游标（`Null`、
//!   自由字符串、缺版本字段的对象）反序列化失败 = 非本源游标 → **全新起步**
//!   （`files: []`），绝不把旧值解释成文件位置或 offset。
//! * **枚举与预过滤（窗口判定先于打开，AGENTS 反模式禁令）**：递归枚举 rollout
//!   文件、逐个 `stat`；**从未读过**（游标无条目）且 mtime 超出 24h 新鲜窗的文件
//!   不打开；游标在案的文件**永远续读**（其 offset/未决状态才是权威——超窗活跃
//!   会话不停卡）。访问序 = 续读文件优先（mtime 新→旧），新文件次之（最新优先）。
//! * **冷启动（首次读一个文件）**：从 `max(0, size − 8MiB)` 进入、对齐到下一行
//!   行首；读取起点**绝不当请求起点**——窗前开始的响应不补窗（解码器保证：无窗内
//!   输入边界必不发射，其事实丢失而非伪造），首个真实输入/回合边界见过即转热
//!   （`cold_start=false`）。
//! * **残行不越过**：`read_tail` 块尾的不完整行不消费，offset 只推进到完整行边界
//!   ——下轮从行界续读（不丢不重）。文件收缩（截断/重写）按新文件重读。
//! * **超大行（>1MiB）/ 坏行**：整行越过**不解析其正文**，且使解码器在途未决
//!   片段失效（`decoder::invalidate_inflight`）——等下一个可靠输入边界再计速，
//!   绝不把损坏区间混进任何响应窗。「越过」只发生在**证得**行超阈值时（读窗
//!   ≥ 1MiB 且读满仍无换行）；共享余量不足几十 KB 时的无换行块 = 可能只是合法
//!   大行没读全，原位返回等下轮足额预算，绝不拦腰越过（评审 Important 2）。
//! * **封口行发射余量闸**：`token_usage_record` 行的发射上界为 2（本行可先结算
//!   更早的延迟未决封口、再提交本封口）。发射余量 < 2 时停在该行行界**之前**——
//!   不消费、不计预算、解码器零状态变更，下轮足额结算；发射绝不 take 截断（被截
//!   观察的摘要已在已发射集合内，截断 = 永久丢失）（评审 Important 1）。门控停批
//!   直接返回本轮（含「封口行是块首」的 pos == 0 情形），绝不落入无换行块的越过
//!   判定——那里此刻是完整可解析行，越过即整窗吞掉（评审修复轮 2）。
//! * **整源单轮共享预算**（跨文件、不逐文件重置；DSH `decode_budget` 同风格）：
//!   读取字节 ≤ 8 MiB、完整 JSON 行处理 ≤ 4096、发射观察 ≤ 200。预算在中途用尽
//!   停在**行边界检查点**，余量下轮续读。
//! * **已发射摘要集有界**：解码器状态内 **128** 个 FIFO 驱逐（裁决 R2：游标每个
//!   采集轮整体重写——可见节奏 5s——上限压住单文件状态体积；同流重复封口相邻，
//!   128 远超真实去重距离）；驱逐后的同文件同 id 再现会重复发射一次，由 DB 对
//!   `stable_call_key` 的 UPSERT（同 host_instance）收敛；跨文件去重本就是 DB 的
//!   职责，不归解码器。
//! * **安静文件游标剪枝**（裁决 R2b）：文件 mtime 超出 24h 新鲜窗**且**已读尽
//!   （`offset == size`）**且**无未决封口 → 从游标剪除（游标每 5s 重写，体积与
//!   「24h 内见过的文件数」挂钩而非历史总量）。若该文件日后追加，mtime 回到窗内 →
//!   按新文件冷启动重进（回看窗内重叠事实重发一次，UPSERT 幂等）；仍挂着未决封口
//!   的安静文件不剪（跨轮未决状态绝不随剪枝丢失）。
//!
//! ## 隐私与边界
//!
//! * 游标/诊断只存 **ID 摘要、时刻、计数、模型/渠道与头事实**——绝无 prompt、
//!   reasoning、工具正文、原始 response_id 或 session id（Task 4 serde 测试 +
//!   本任务游标级测试双重把关）。
//! * **不读** `~/.codex/auth.json` 或任何凭证；**不启动**任何 Codex/app-server/
//!   模型进程；旧 token/费用采集（`services/usage/collectors/codex.rs` 及其游标）
//!   不读、不写、不碰。
//! * `read_incremental` 是入参的纯函数：home 显式注入（测试传影子目录），本层
//!   不落库——next_cursor / source_state 随 ReadBatch 返回，由采集编排经
//!   `store::commit_batch` 同事务提交。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::host_instance_tag;
use crate::services::tps::model::{ReadBatch, SourceCursor, SourceState, TpsObservation};
use crate::services::tps::store::TPS_BATCH_MAX_ROWS;
use crate::services::usage::error::UsageError;

/// Codex 纯响应分段与原生 ID 去重解码器（V2 计划 Task 4；纯事件状态机，无文件/DB/
/// 原文持久化；读取器编排与游标接线归 Task 5）
pub mod decoder;

use decoder::CodexDecoderState;

/// 来源 id（与 `UsageSourceId::Codex.db_id()` 同串）
pub const SOURCE_ID: &str = "codex";

/// 游标格式版本：本实现写入恒为 1；无此字段的任何旧占位（`Null`/字符串/缺键对象）
/// 反序列化失败 = 非本源游标 → 全新起步，绝不重释为文件位置
pub const FORMAT_VERSION: u32 = 1;

/// 新鲜窗 = 事实保留窗（24h）：只对**从未读过**的新文件做 stat 预过滤
const FRESH_WINDOW_MS: i64 = crate::services::tps::store::TPS_RETENTION_MS;

/// 单批发射上限（与 commit_batch 的硬闸同源：读取层预算 = 存储层事务上限）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;

/// 冷启动回看窗（Spec §6.3「首次从近期文件有界定位最新完整边界」）：从
/// `max(0, size − 8MiB)` 进入，窗前历史的事实丢失而非伪造
const COLD_START_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// 整源单轮读取字节预算（跨文件共享，含冷启动窗口本身）
const PASS_READ_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// 整源单轮完整 JSON 行处理预算
const PASS_LINE_BUDGET: usize = 4096;

/// 超大单行阈值：越过不解析其正文（并使在途未决失效）
const OVERSIZED_LINE_BYTES: usize = 1024 * 1024;

/// 来源限制说明（sourceState.detail）：响应口径已接入、stream 仍是真实缺口
///（「stream-capability 语言保持真实」：Codex 无同调用流式窗，不提供 stream/TTFT）
const SOURCE_LIMITS: &str = "response-only: 响应事实来自原生 token_usage_record 封口的日志近似响应窗（log-boundary-approx，含等待）；无同调用流式窗——stream/TTFT/生成窗不提供（insufficient-generation-timing），generation/首 Token 恒缺失；工具用时未归属";

// ==== 游标状态（装在 SourceCursor.value；本层私有形状）====
//
// 版本语义（Spec §6.3 迁移纪律）：`format_version` 为**必填**字段——旧占位游标没有
// 它，反序列化整体失败 = 「非本源游标」，按全新起步处理（files: []），绝不把旧值
// 读成 offset/文件位置。全新安装（空游标）同样走 default 直接 v1 起步。

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexSourceCursor {
    /// 游标格式版本：旧占位缺字段 → 整体解析失败 → 全新起步；本实现写入恒为 1
    format_version: u32,
    /// 逐文件读取位置与未决状态（按访问序重建，落盘前按 path 排序定形）
    #[serde(default)]
    files: Vec<CodexFileCursor>,
}

impl Default for CodexSourceCursor {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            files: Vec::new(),
        }
    }
}

/// 单文件游标：相对路径 + 完整行字节界 + 本文件自己的解码器状态 + 头事实
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexFileCursor {
    /// 相对 sessions 根的路径（游标键；跨 home 移动仍稳定）
    path: String,
    /// 已消费字节界：恒落在完整行边界（残行/超大行按各自规则越过时也记录在案）
    offset: u64,
    /// 上次见到的文件大小（审计参考；收缩判定以当轮 stat 为准）
    size: u64,
    /// 冷启动未完成：窗内首个真实输入/回合边界尚未见过（窗前开始的响应不补窗）
    cold_start: bool,
    /// 本文件自己的解码器状态（模型/渠道/会话事实绝不跨文件泄漏；已发射摘要集
    /// 有界 128（decoder 的 DIGEST_CAP），FIFO 驱逐）
    #[serde(default)]
    decoder: CodexDecoderState,
    /// 头事实：首个 `session_meta.payload.cli_version`（Spec §6.2.6 记录宿主版本）
    #[serde(default)]
    cli_version: Option<String>,
    /// 头事实：首个 `session_meta.payload.model_provider`
    #[serde(default)]
    model_provider: Option<String>,
}

/// 整源单轮共享预算（跨文件；任一耗尽即停批，检查点恒在行边界）
#[derive(Debug, Clone, Copy)]
struct PassBudget {
    bytes: usize,
    lines: usize,
    emits: usize,
}

impl PassBudget {
    fn new() -> Self {
        Self {
            bytes: PASS_READ_BUDGET_BYTES,
            lines: PASS_LINE_BUDGET,
            emits: BATCH_CAP,
        }
    }

    fn exhausted(&self) -> bool {
        self.bytes == 0 || self.lines == 0 || self.emits == 0
    }
}

/// 枚举产物（只 stat，未打开内容）
struct Candidate {
    /// 相对 sessions 根的路径（游标键）
    rel: String,
    path: PathBuf,
    mtime_ms: i64,
    size: u64,
}

// ==== 入口 ====

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    let root = home.join(".codex").join("sessions");
    // 未装 codex = 正常空态（不是错误）：ok 空批、游标逐字原样（不升级不解释）
    if !root.is_dir() {
        return Ok(empty_batch(cursor));
    }
    let host = host_instance_tag(&root);
    let mut state = parse_state(cursor);

    // 枚举（只 stat，不打开内容）；mtime 降序 = 最新文件优先吃预算
    let mut candidates: Vec<Candidate> = Vec::new();
    collect_rollouts(&root, &root, &mut candidates, 0);
    candidates.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then(a.rel.cmp(&b.rel)));

    let mut observations: Vec<TpsObservation> = Vec::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut budget = PassBudget::new();

    // 游标条目按 rel 建索引；访问序 = 续读文件优先（游标在案，超窗也读），新文件
    // 次之（24h stat 预过滤后按 mtime 新→旧）
    let mut existing: BTreeMap<String, CodexFileCursor> =
        state.files.drain(..).map(|f| (f.path.clone(), f)).collect();
    let mut next_files: Vec<CodexFileCursor> = Vec::new();
    let fresh: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| !existing.contains_key(&c.rel))
        .filter(|c| now_ms.saturating_sub(c.mtime_ms) <= FRESH_WINDOW_MS)
        .collect();
    let ordered: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| existing.contains_key(&c.rel))
        .chain(fresh.iter().copied())
        .collect();

    for c in &ordered {
        if budget.exhausted() {
            diag(&mut tally, "单批预算用尽，余量下轮续读");
            break;
        }
        let cold = !existing.contains_key(&c.rel);
        let mut fc = existing.remove(&c.rel).unwrap_or_else(|| CodexFileCursor {
            path: c.rel.clone(),
            offset: 0,
            size: 0,
            cold_start: true,
            decoder: CodexDecoderState::default(),
            cli_version: None,
            model_provider: None,
        });
        if cold {
            // 冷启动：从 max(0, size − 8MiB) 进入（首个块按「块首残行」对齐跳过）
            fc.offset = c.size.saturating_sub(COLD_START_WINDOW_BYTES);
        }
        let head_partial = cold && fc.offset > 0;
        read_file(
            c,
            &mut fc,
            head_partial,
            &mut budget,
            &host,
            &mut observations,
            &mut tally,
        )?;
        fc.size = c.size;
        next_files.push(fc);
    }
    // 游标条目保留规则：本轮枚举到的文件（无论是否被预算截到）都保留条目——预算
    // 截断的续读文件不得因「本轮没轮到」丢水位；未枚举到的（已消失）随之丢弃，
    // 代价 = 该文件下轮重读，稳定键 UPSERT 幂等
    let enumerated: BTreeSet<&str> = candidates.iter().map(|c| c.rel.as_str()).collect();
    for (_, fc) in existing {
        if enumerated.contains(fc.path.as_str()) {
            next_files.push(fc);
        }
    }
    // 安静文件游标剪枝（裁决 R2b）：mtime 超 24h 窗 + 已读尽 + 无未决封口 → 剪除。
    // 游标每个采集轮（可见节奏 5s）经 commit_batch 整体重写，条目数与「24h 内见过
    // 的文件数」挂钩而非历史总量。被剪文件日后追加 → mtime 回窗内 → 按新文件冷启动
    // 重进，重叠事实 UPSERT 幂等；有未决封口的安静文件不剪（跨轮未决状态绝不丢）。
    let by_rel: BTreeMap<&str, &Candidate> =
        candidates.iter().map(|c| (c.rel.as_str(), c)).collect();
    next_files.retain(|fc| {
        let Some(c) = by_rel.get(fc.path.as_str()) else {
            return true; // 不可达（上面已按枚举集过滤）；保守保留
        };
        let quiet_and_aged = now_ms.saturating_sub(c.mtime_ms) > FRESH_WINDOW_MS;
        let fully_consumed = fc.offset >= c.size;
        !(quiet_and_aged && fully_consumed && !decoder::has_pending_seal(&fc.decoder))
    });
    next_files.sort_by(|a, b| a.path.cmp(&b.path));
    state.files = next_files;

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

// ==== 枚举（只 stat）====

/// 递归收集 `rollout-*.jsonl`（真实根约定 `sessions/<y>/<m>/<d>/rollout-*.jsonl`；
/// 与 usage 采集器同款文件名判据）。深度上限防御符号链接环/异常嵌套。
fn collect_rollouts(dir: &Path, root: &Path, out: &mut Vec<Candidate>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // 目录在枚举间消失/不可读：跳过（D-24 唯一静默形态）
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rollouts(&p, root, out, depth + 1);
        } else if is_rollout(&p) {
            let Ok(meta) = std::fs::metadata(&p) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            out.push(Candidate {
                rel,
                path: p,
                mtime_ms,
                size: meta.len(),
            });
        }
    }
}

/// rollout 文件判据（与 `services/usage/collectors/codex.rs::is_rollout` 同款）
fn is_rollout(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("rollout") && n.ends_with(".jsonl"))
        .unwrap_or(false)
}

// ==== 单文件有界增量读取 ====

#[allow(clippy::too_many_arguments)]
fn read_file(
    c: &Candidate,
    fc: &mut CodexFileCursor,
    head_partial: bool,
    budget: &mut PassBudget,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
) -> Result<(), UsageError> {
    // 1) 文件收缩（截断/重写）：按新文件重读（稳定键 UPSERT 幂等；冷启动规则重生效）
    if c.size < fc.offset {
        diag(tally, "rollout 文件收缩，按新文件重读");
        fc.offset = 0;
        fc.cold_start = true;
        fc.decoder = CodexDecoderState::default();
        fc.cli_version = None;
        fc.model_provider = None;
    }
    // 2) 无新增（便宜路径：不打开）
    if fc.offset >= c.size {
        return Ok(());
    }
    // 3) 有界增量读取：每块至多 min(字节余量, 单批读上限)，逐块消费到行边界检查点
    let mut head_partial = head_partial;
    loop {
        if budget.bytes == 0 {
            diag(tally, "整源读取字节预算用尽，余量下轮续读");
            return Ok(());
        }
        if budget.lines == 0 {
            diag(tally, "整源解析行预算用尽，余量下轮续读");
            return Ok(());
        }
        if budget.emits == 0 {
            diag(tally, "单批观察上限用尽，余量下轮续读");
            return Ok(());
        }
        let want = budget.bytes.min(super::TAIL_READ_MAX_BYTES);
        let (bytes, eof) = match super::read_tail(&c.path, fc.offset, want) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                diag(
                    tally,
                    "rollout file vanished between enumerate and read (rotation race), skipping this round",
                );
                return Ok(());
            }
            Err(e) => {
                return Err(UsageError::new(
                    "usage-source-io",
                    format!("tps/codex: failed to read {}: {e}", c.path.display()),
                ))
            }
        };
        if bytes.is_empty() {
            return Ok(());
        }
        budget.bytes = budget.bytes.saturating_sub(bytes.len());

        // 冷启动块首对齐：seek 点（size − 8MiB）可能落在行中部——首个残行不消费，
        // 对齐到下一行行首；整窗无一行完整时只有**证得**该行超过超大行阈值（窗口
        // ≥ 1MiB 且读满仍无换行）才整窗越过不解析——否则原位返回（可能只是余量
        // 不足的合法行），下轮足额预算从同一 seek 重读
        let mut pos = 0usize;
        if head_partial {
            head_partial = false;
            match bytes.iter().position(|b| *b == b'\n') {
                Some(nl) => pos = nl + 1,
                None => {
                    if want >= OVERSIZED_LINE_BYTES && bytes.len() == want {
                        diag(
                            tally,
                            "cold-start window contains no complete line, advancing past it (content not parsed)",
                        );
                        fc.offset += bytes.len() as u64;
                        if eof {
                            return Ok(());
                        }
                        continue;
                    }
                    return Ok(()); // 余量不足，读不全：等下轮足额预算（水位不动）
                }
            }
        }

        // 4) 逐行消费（只处理完整行；预算检查点恒在行边界）。
        //    `gate_stopped` 区分 while 的中断原因：只有「块内确无换行」才允许进入
        //    循环后的无换行块判定——封口闸的停批（哪怕停在块首、pos == 0）必须直接
        //    返回，绝不落入 mid-line 越过分支（评审修复轮 2）。
        let mut gate_stopped = false;
        while pos < bytes.len() {
            if budget.lines == 0 || budget.emits == 0 {
                break;
            }
            let Some(nl) = bytes[pos..].iter().position(|b| *b == b'\n') else {
                break; // 块尾残行（正在写/撕裂）：不消费，下轮从行界续
            };
            let line_end = pos + nl + 1;
            let line = &bytes[pos..line_end - 1];
            if line.len() > OVERSIZED_LINE_BYTES {
                budget.lines -= 1;
                // 超大行：越过不解析其正文；在途未决片段失效（等下个可靠输入边界）
                let dropped = decoder::invalidate_inflight(&mut fc.decoder);
                diag(
                    tally,
                    &format!("oversized line skipped (>1MiB, content not parsed), pending invalidated ({dropped})"),
                );
                pos = line_end;
                continue;
            }
            let Ok(row) = serde_json::from_slice::<Value>(line) else {
                budget.lines -= 1;
                // 坏行：诊断跳过；在途未决片段失效（同超大行）
                let dropped = decoder::invalidate_inflight(&mut fc.decoder);
                diag(
                    tally,
                    &format!("malformed JSON line skipped, pending invalidated ({dropped})"),
                );
                pos = line_end;
                continue;
            };
            // 封口行是发射上界最大的一行（可在本行先结算更早的延迟未决封口、再提交
            // 本封口——两条观察的摘要在解码器发射点内部即已入已发射集合）。发射余量
            // 不足上界时**停在本行行界之前**：不消费该行、不计行预算、解码器零状态
            // 变更，下轮以足额余量结算——绝不 take 截断发射（被截掉那条的摘要已入
            // 集合，跨轮重复检查会挡住它，UPSERT 也救不回 = 永久丢失）
            if is_seal_row(&row) && budget.emits < 2 {
                gate_stopped = true;
                diag(tally, "封口行发射余量不足，停在该行行界之前，下轮足额结算");
                break;
            }
            budget.lines -= 1;
            capture_header_fact(fc, &row);
            if fc.cold_start && is_boundary_row(&row) {
                // 冷启动转热：窗内首个真实输入/回合边界已见（此前的封口因无窗内
                // 边界本就发不出——解码器保证读取起点绝不当请求起点）
                fc.cold_start = false;
            }
            let r = decoder::push_record(&mut fc.decoder, &row, host);
            let take = r.observations.len().min(budget.emits);
            observations.extend(r.observations.into_iter().take(take));
            budget.emits -= take;
            for d in r.diagnostics {
                *tally.entry(d).or_insert(0) += 1;
            }
            pos = line_end;
        }
        fc.offset += pos as u64;

        if gate_stopped {
            // 封口闸停批：检查点已落在封口行行界（含「封口行是块首、pos == 0」的
            // 情形）——本轮该文件到此为止，下轮足额结算。**绝不**落入下面的无换行
            // 块判定：此刻块内是完整可解析的行，mid-line 越过会整窗吞掉未解析内容
            // 并误使未决状态失效（评审修复轮 2 的回归点）。
            return Ok(());
        }
        if pos == 0 && !eof {
            // 非 EOF 且整块无一行完整（也没对齐可跳）：只有**证得**该行超过超大行
            // 阈值（窗口 ≥ 1MiB 且读满仍无换行）才越过本窗不解析（字节已计入预算，
            // 不会死循环）——共享预算被前面的文件/行消耗后窗口可能只剩几十 KB，一个
            // 完全合法的 <1MiB 大行绝不能因此被拦腰越过
            if want >= OVERSIZED_LINE_BYTES && bytes.len() == want {
                diag(
                    tally,
                    "single line exceeds read window, advancing past it (content not parsed)",
                );
                let dropped = decoder::invalidate_inflight(&mut fc.decoder);
                if dropped > 0 {
                    diag(tally, &format!("pending invalidated ({dropped})"));
                }
                fc.offset += bytes.len() as u64;
                continue;
            }
            // 窗口不足或块未读满：本轮不消费（可能是合法行只是没读全），下轮足额再读
            return Ok(());
        }
        // 未到 EOF 或块内尚有未消费内容：预算耗尽 → 诊断 + 行边界检查点（本轮该文件到此）
        let work_remains = pos < bytes.len() || !eof;
        if work_remains && budget.lines == 0 {
            diag(tally, "整源解析行预算用尽，余量下轮续读");
            return Ok(());
        }
        if work_remains && budget.emits == 0 {
            diag(tally, "单批观察上限用尽，余量下轮续读");
            return Ok(());
        }
        if eof || pos < bytes.len() {
            // EOF（含撕裂尾行：等写方补完，残行不消费）或块尾残行：本轮该文件到此
            return Ok(());
        }
        // 本块消费尽且预算尚余：续读下一块
    }
}

// ==== 行级辅助 ====

/// 头事实捕获：首个 `session_meta` 的 cli_version / model_provider（首见者 = 建档
/// 宿主；同文件多 session_meta（父子线程）不覆盖）
fn capture_header_fact(fc: &mut CodexFileCursor, row: &Value) {
    if row.get("type").and_then(Value::as_str) != Some("session_meta") {
        return;
    }
    let Some(p) = row.get("payload") else {
        return;
    };
    if fc.cli_version.is_none() {
        if let Some(v) = p.get("cli_version").and_then(Value::as_str) {
            fc.cli_version = Some(v.to_string());
        }
    }
    if fc.model_provider.is_none() {
        if let Some(v) = p.get("model_provider").and_then(Value::as_str) {
            fc.model_provider = Some(v.to_string());
        }
    }
}

/// 封口行判定（发射余量闸用）：`token_usage_record` 是发射上界最大的一行——可在本行
/// 先结算更早的延迟未决封口、再提交本封口，发射余量不足上界（2）时读取器停在本行
/// 行界之前，绝不 take 截断
fn is_seal_row(row: &Value) -> bool {
    row.get("type").and_then(Value::as_str) == Some("token_usage_record")
}

/// 冷启动转热判定：真实输入/回合边界（与解码器的边界清单一致——user/developer
/// 消息、工具返回、`task_started`、`turn_context`）
fn is_boundary_row(row: &Value) -> bool {
    let row_type = row.get("type").and_then(Value::as_str);
    if row_type == Some("turn_context") {
        return true;
    }
    if row_type == Some("event_msg") {
        return row
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            == Some("task_started");
    }
    if row_type == Some("response_item") {
        let Some(p) = row.get("payload") else {
            return false;
        };
        let item_type = p.get("type").and_then(Value::as_str);
        let role = p.get("role").and_then(Value::as_str);
        return matches!(
            (item_type, role),
            (Some("message"), Some("user" | "developer"))
                | (Some("function_call_output" | "custom_tool_call_output"), _)
        );
    }
    false
}

// ==== 游标解析与小工具 ====

/// 非 v1 形态（空串/`Null`/字符串/缺 `format_version` 的对象/未知版本）= 非本源
/// 游标 → 全新起步（files: []）。**绝不**把旧占位值解释成文件位置或 offset。
fn parse_state(cursor: &SourceCursor) -> CodexSourceCursor {
    if cursor.value.is_empty() {
        return CodexSourceCursor::default();
    }
    match serde_json::from_str::<CodexSourceCursor>(&cursor.value) {
        Ok(s) if s.format_version == FORMAT_VERSION => s,
        _ => {
            log::warn!(
                "tps/codex: 游标不是本源 v{FORMAT_VERSION} 形态（旧占位/外源值）→ 全新起步，绝不解释为文件位置"
            );
            CodexSourceCursor::default()
        }
    }
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

// ==== 本模块贴身单测（纯函数面；集成面在 tests/tps_codex_source_test.rs）====
#[cfg(test)]
mod tests {
    use super::*;

    /// 旧占位游标全形态 → 全新起步（不 panic、files 空、版本 v1）
    #[test]
    fn placeholder_cursors_parse_as_fresh() {
        for value in [
            "",
            "null",
            "insufficient-generation-timing",
            "{\"files\":[{\"path\":\"x\",\"offset\":99}]}",
            "{\"format_version\":2,\"files\":[]}",
        ] {
            let state = parse_state(&SourceCursor {
                source: SOURCE_ID.into(),
                value: value.into(),
            });
            assert!(state.files.is_empty(), "{value}");
            assert_eq!(state.format_version, FORMAT_VERSION, "{value}");
        }
        // 本源 v1 游标正常解析
        let state = parse_state(&SourceCursor {
            source: SOURCE_ID.into(),
            value: serde_json::to_string(&CodexSourceCursor {
                format_version: 1,
                files: vec![CodexFileCursor {
                    path: "2026/10/26/rollout-a.jsonl".into(),
                    offset: 42,
                    size: 100,
                    cold_start: false,
                    decoder: CodexDecoderState::default(),
                    cli_version: Some("0.164.0".into()),
                    model_provider: Some("openai".into()),
                }],
            })
            .unwrap(),
        });
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.files[0].offset, 42);
    }

    /// rollout 文件名判据与 usage 采集器同款
    #[test]
    fn rollout_predicate_matches_usage_collector_convention() {
        let yes = ["rollout-2026-10-26.jsonl", "rollout-x.jsonl"];
        let no = ["session.jsonl", "rollout.jsonl.tmp", "xrollout-a.jsonl"];
        for n in yes {
            assert!(is_rollout(Path::new(n)), "{n}");
        }
        for n in no {
            assert!(!is_rollout(Path::new(n)), "{n}");
        }
    }

    /// 冷启动转热判据覆盖解码器的边界清单
    #[test]
    fn boundary_classifier_covers_decoder_boundary_list() {
        let user = serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "user"}});
        let tool_out = serde_json::json!({"type": "response_item", "payload": {"type": "custom_tool_call_output"}});
        let started = serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}});
        let ctx = serde_json::json!({"type": "turn_context", "payload": {"model": "m"}});
        let assistant = serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "assistant"}});
        let seal = serde_json::json!({"type": "token_usage_record", "payload": {}});
        for row in [&user, &tool_out, &started, &ctx] {
            assert!(is_boundary_row(row), "{row}");
        }
        for row in [&assistant, &seal] {
            assert!(!is_boundary_row(row), "生成项/封口不是转热边界：{row}");
        }
    }
}
