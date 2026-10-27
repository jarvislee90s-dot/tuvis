//! DSH 原生 TPS 读取器（Spec §4 DSH 行；证据：dsh-kimi.md / dsh-output-scope-addendum.md）。
//!
//! ## 读取对象与口径
//!
//! * 根 = `<home>/.dsh/sessions/<项目>/<会话>/session[.vN].jsonl[.zstd]`，只读**最优代际**
//!   （版本号最大者，`monitor::dsh::log::generation_logs` 同款；代际并存即历史拷贝，
//!   只读一代天然满足「重复代际只计一次」）。
//! * **模型终结为终点**（Spec §4「最终成功消息」）：`assistant/message` 的结算时刻
//!   （事件 `time`）是调用终点；**不是**含工具执行的 `step/end`。
//! * 紧凑 dt 流展开（dsh-kimi.md「流时间确实持久保存」）：`reasoning-chunks` /
//!   `text-chunks` / `tool-call-chunks` 记录 = `time0` + `dt[]`（长度=成员数−1）+ 成员数组，
//!   成员 j 时刻 = time0 + Σdt[0..j]。**首个空 delta 跳过**：首 token = 首个非空成员时刻
//!   （思考/正文/参数都算输出成员，Spec §4「展开紧凑流首非空思考/正文/参数至结算」）；
//!   `chunk` 型记录（block-start/usage/finish）不参与首 token。
//! * 生成窗 = 首非空成员 → 消息结算；输出 = 同消息 `data.usage.outputTokens`
//!   （**同消息 Token 归同一次调用**，不跨消息累计、不按流文本重数）。
//! * 响应窗（V2 Spec §5.2「含等待响应」）= step 开始 → 消息结算（与起点/终点/整调用
//!   时长同边界），含首 token 等待、不含结算之后 step/end 的工具执行；重试对齐失败
//!   与流式口径一并剔除（无流生成窗的记录响应窗照常，response-only 由 Task 1 谓词
//!   把关）。
//! * **思考计入**（addendum 固定准入）：仅当 `data.message.source.replayState.response`
//!   为 `pi-ai / 2 / openai-completions` 且其 provider/model 与消息 source 一致时，
//!   `outputTokens` 标 `IncludesReasoning`（供应商总输出已含思考，不再相加）；
//!   未命中 profile → `ProviderReported`（思考计数未单独声明，不强行标已含）；生成窗**不**受
//!   profile 门控（Spec §4「不是只认旧稿的单一 replay profile」；独立复核 R8）。
//! * **重试对齐**（Spec §4「可定位最后成功尝试则用其起点，否则剔除重试速度」）：
//!   step 内无 `assistant/attempt`（无 step 重试）→ 对齐，起点 = `step/start`；
//!   有 attempt → 无法定位最后成功尝试的起点 → 消息**剔除速度**（generation/起点/
//!   单次时长全空，不猜），首个 attempt 以 step/start 为起点出 `Failed` 观察，
//!   第 2+ 次尝试起点不可定位、跳过并诊断。
//! * 流成员非空且分布 ≥2 个时刻才入速（addendum「流成员非空并分布在至少两个时刻」；
//!   单时刻 = 整块突发，排除）。
//! * 缓存只对账：本层不消费投影缓存（会话总量），逐调用事实全部来自原始事件
//!   （Spec §4「缓存只对账」）。
//!
//! ## 增量预算（Spec §2「文件只读新增部分」+ AGENTS.md L3）
//!
//! * 逐会话游标 = {最优代际, 已消费字节界, 单元内已消费行数, 会话 id, 未决 step 状态}，
//!   全部装在 SourceCursor.value（JSON）里随 ReadBatch 返回，本层不落库。
//! * zstd 多帧容器（每追加批次独立一帧）：只解码水位之后的新帧；**尾帧撕裂不越过**
//!   （下轮重解），中间坏帧跳过计数；plain 文件只读最后换行之后的新增。
//! * **整源共享解码预算**（F1）：一次 read_incremental 至多解码 8 MiB，跨会话、跨帧
//!   逐层传递，不在每文件重新初始化；被本轮余量截断的正常帧保留检查点（不消费不
//!   越过、不误判炸弹），下轮以新预算续读。
//! * 24h 新鲜窗：陈旧且无水位的会话文件**不打开不解码**（L3 预算；事实保留窗同 24h，
//!   更早完成不可能进任何未来 120s 采样窗）。
//! * 单批发射上限 = `store::TPS_BATCH_MAX_ROWS`（200）；预算在中途用尽时按**行粒度**
//!   留检查点（未消费内容下轮续读，不丢不重）。
//!
//! ## 已知限制（进 sourceState.detail，如实申报）
//!
//! * 工具用时未归属：`tool/call→result` 的 callId 配对要与消息观察跨事件回填，
//!   与「事实尽快出批进 120s 采样窗」冲突，本版 `tool_ms` 恒空（timedTool 无样本）。
//! * 重试语义以「step 内有无 attempt」为准；DSH 无稳定 requestId/attemptId
//!   （dsh-kimi.md），跨代际尝试序列不跨文件拼接。
//!
//! ## 与 monitor/dsh/decode.rs 的关系
//!
//! 帧扫描/解码逻辑与 `decode_zstd_frames` 同款但**本地镜像**：该模块的公开 API 只提供
//! 「整文件全帧解码」，选择性解码需要帧偏移（私有）；且 monitor/ 不在本任务改动范围内。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::host_instance_tag;
use crate::monitor::dsh::log as dsh_log;
use crate::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use crate::services::tps::store::TPS_BATCH_MAX_ROWS;
use crate::services::usage::error::UsageError;

/// 来源 id（与 `UsageSourceId::Dsh.db_id()` 同串）
pub const SOURCE_ID: &str = "dsh";

/// 新鲜窗 = 事实保留窗（24h）：更早完成不可能进任何未来 120s 采样窗
const FRESH_WINDOW_MS: i64 = crate::services::tps::store::TPS_RETENTION_MS;

/// 单批发射上限（与 commit_batch 的硬闸同源：读取层预算 = 存储层事务上限）
const BATCH_CAP: usize = TPS_BATCH_MAX_ROWS;

// ==== 游标状态（装在 SourceCursor.value；本层私有形状，反序列化失败归零重读——
//      重读的观察以稳定键 UPSERT 幂等，不产生双计）====

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DshCursorState {
    /// 会话目录（绝对路径串）→ 该会话的读取位置与未决状态
    #[serde(default)]
    sessions: BTreeMap<String, SessionPos>,
}

/// 无解可读时的坏文件标记：(mtime_ms, size)。未变化前不再重复解码（防 5s 轮询热循环）
type BadMark = (i64, i64);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionPos {
    /// 会话身份（文件头 `session` 行的 id；增量尾读不回头重读头，靠这里延续）
    #[serde(default)]
    id: String,
    /// 最优代际号
    #[serde(default)]
    gen: i64,
    /// 已消费字节界：zstd = 已消费帧尾；plain = 已消费行尾（'\n' 之后）
    #[serde(default)]
    offset: u64,
    /// 当前解码单元内已消费、待跳过的行数（预算在单元中部耗尽时的检查点）
    #[serde(default)]
    skip_lines: usize,
    /// 超大单行有界扫描位置（R3'）：plain 尾块读满上限仍无换行时，从这里续扫
    /// （0 = 无扫描进行中；> offset = 扫描中，每轮至多前扫 8 MiB，位置持久化防整段重读）
    #[serde(default)]
    oversized_upto: u64,
    /// 未决帧读界（M2）：跨读界被截断的普通帧 →「没读全」不是「解不动」，水位停帧
    /// 起点、读界逐轮翻倍（上限 [`MAX_PENDING_READ_BYTES`]）直到其压缩输入完整进入
    /// 单次读取；解析成功 / 判真炸弹 / 降级扫描态后归零。serde default = 默认读界
    /// （旧游标无此字段 = 无未决帧）；0 同样表示无未决帧（收缩/代际切换归零后读端
    /// 回默认读界，见 `effective_read_cap`）。
    #[serde(default = "default_pending_cap")]
    pending_cap: u64,
    /// 未决 step 起点："(turn,step)" → step/start 时刻（消息结算后清除）
    #[serde(default)]
    step_starts: BTreeMap<String, i64>,
    /// 未决 attempt 计数："(turn,step)" → 次数与末次时刻（消息结算后清除）
    #[serde(default)]
    attempts: BTreeMap<String, AttemptTally>,
    /// 解码失败标记（文件 (mtime,size) 未变前不再尝试）
    #[serde(default)]
    bad: Option<BadMark>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AttemptTally {
    count: i64,
    last_ms: i64,
}

/// 旧游标反序列化时 `pending_cap` 字段缺失的默认值 = 默认读界（无未决帧的等价态）
fn default_pending_cap() -> u64 {
    super::TAIL_READ_MAX_BYTES as u64
}

/// 未决帧读界上限（M2）：64 MiB（= 8 × 默认读界）仍读不全的帧按真巨帧降级（诚实
/// 诊断 + 有界扫描态），读界不得无限增长
const MAX_PENDING_READ_BYTES: u64 = 8 * super::TAIL_READ_MAX_BYTES as u64;

/// 本轮读取窗上限（M2）：未决帧读界翻倍序列的当前值；0/缺省 = 默认读界
fn effective_read_cap(pos: &SessionPos) -> u64 {
    if pos.pending_cap > super::TAIL_READ_MAX_BYTES as u64 {
        pos.pending_cap.min(MAX_PENDING_READ_BYTES)
    } else {
        super::TAIL_READ_MAX_BYTES as u64
    }
}

// ==== 入口 ====

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    let root = home.join(".dsh").join("sessions");
    let host = host_instance_tag(&root);
    let mut state: DshCursorState = parse_state(cursor);

    // 根目录读失败按 ErrorKind 区分（usage/dsh 采集器同款）：NotFound = 未装 dsh
    // 的正常空态；其余 = 这个源读不了 → 整源 Err（调用方保持原水位）。
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_batch(cursor));
        }
        Err(e) => {
            return Err(UsageError::new(
                "usage-source-io",
                format!(
                    "tps/dsh: failed to read session root {}: {e}",
                    root.display()
                ),
            ))
        }
    };

    // 枚举（只 stat，不打开内容）；mtime 降序 = 最新的会话优先吃预算
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_dirs: BTreeSet<String> = BTreeSet::new();
    for project in entries.flatten() {
        let ppath = project.path();
        if !ppath.is_dir() {
            continue;
        }
        let Ok(dirs) = std::fs::read_dir(&ppath) else {
            continue; // 项目目录在枚举间消失/不可读：跳过该项目（D-24 唯一静默形态）
        };
        for sd in dirs.flatten() {
            let spath = sd.path();
            if !spath.is_dir() {
                continue;
            }
            let Some((gen, path)) = dsh_log::generation_logs(&spath).pop() else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let dir_key = spath.to_string_lossy().to_string();
            seen_dirs.insert(dir_key.clone());
            candidates.push(Candidate {
                mtime_ms,
                dir_key,
                gen,
                path,
                size: meta.len(),
            });
        }
    }
    candidates.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then(a.dir_key.cmp(&b.dir_key)));

    let mut observations: Vec<TpsObservation> = Vec::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut budget = BATCH_CAP;
    // 整源共享解码预算（F1）：一次源采集（read_incremental）至多解码 8 MiB，跨会话、
    // 跨帧逐层传递，**不在每文件重新初始化**——否则 N 个「压缩体极小、解码量大、无
    // 观察」的合法会话把单轮解码放大到 N×8 MiB（观察条数预算拦不住它）
    let mut decode_budget = DECODE_BUDGET_BYTES;
    for c in candidates {
        if budget == 0 {
            diag(&mut tally, "单批预算用尽，余量下轮续读");
            break;
        }
        let had_pos = state.sessions.contains_key(&c.dir_key);
        // L3 预算：超窗且无水位 → 不打开不解码（有水位 = 以前读过，走便宜的字节界检查）
        if !had_pos && now_ms.saturating_sub(c.mtime_ms) > FRESH_WINDOW_MS {
            continue;
        }
        let mut pos = state.sessions.remove(&c.dir_key).unwrap_or_default();
        read_session(
            &c,
            &mut pos,
            now_ms,
            &host,
            &mut observations,
            &mut tally,
            &mut budget,
            &mut decode_budget,
        );
        state.sessions.insert(c.dir_key.clone(), pos);
    }
    // 消失目录的游标条目随之丢弃（枚举本身失败的目录本轮不在 seen 里，代价 = 下轮重读，幂等）
    state.sessions.retain(|k, _| seen_dirs.contains(k));

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

/// 来源限制说明（sourceState.detail，如实申报）
const SOURCE_LIMITS: &str = "DSH 工具用时未归属（tool_ms 恒空）；step 重试后成功与第2+次尝试因无法对齐起点剔除速度；非 pi-ai/2/openai-completions profile 的输出标 provider-reported（思考计数未单独声明），生成窗照常";

struct Candidate {
    mtime_ms: i64,
    dir_key: String,
    gen: i64,
    path: PathBuf,
    size: u64,
}

// ==== 单会话增量读取 ====

#[allow(clippy::too_many_arguments)]
fn read_session(
    c: &Candidate,
    pos: &mut SessionPos,
    now_ms: i64,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
    decode_budget: &mut usize,
) {
    let is_zstd = c.path.extension().map(|e| e == "zstd").unwrap_or(false);

    // 0) 坏文件未变 → 不再重复解码（防热循环）
    if pos.bad == Some((c.mtime_ms, c.size as i64)) {
        diag(tally, "上次解码失败的文件未变化，本轮跳过");
        return;
    }
    // 1) 代际切换：新代际文件是历史拷贝/重建 → 从头重读；稳定键去重保幂等，
    //    未决状态由新文件自身事件重建
    if pos.gen != 0 && pos.gen != c.gen {
        diag(
            tally,
            &format!(
                "generation {} -> {} switch, re-reading by new generation (stable-key dedup)",
                pos.gen, c.gen
            ),
        );
        *pos = SessionPos {
            id: pos.id.clone(),
            gen: c.gen,
            ..SessionPos::default()
        };
    }
    pos.gen = c.gen;
    // 2) 文件收缩（截断/重写）：按新文件重读。收缩到扫描位与水位**之间**同样算收缩
    //    （NB3）：否则扫描位恒在文件尾之外，每轮便宜返回，状态永久卡死。
    if c.size < pos.offset || c.size < pos.oversized_upto {
        diag(tally, "会话文件收缩，按新文件重读");
        pos.offset = 0;
        pos.skip_lines = 0;
        pos.oversized_upto = 0;
        pos.pending_cap = 0;
        pos.step_starts.clear();
        pos.attempts.clear();
    }
    // 3) 增量尾读（独立复核 R3/R3'）：水位恒落在帧/行边界（按整 unit 推进），从它 seek 只读
    //    新增 ≤ 单批上限——读取量与已消费历史长度无关。超大单行（plain 满上限无换行）走
    //    **有界分块扫描**（扫描位置持久化在 oversized_upto），绝不整余量重读。
    loop {
        let scanning_oversized = pos.oversized_upto > pos.offset;
        let base = if scanning_oversized {
            pos.oversized_upto
        } else {
            pos.offset
        };
        // 无新增 / 超大行扫描已到文件尾（便宜路径：不打开不解码；等文件增长后续扫）
        if base >= c.size && pos.skip_lines == 0 {
            return;
        }
        // 读取窗上限（M2）：未决帧读界翻倍序列的当前值（无未决帧 = 默认 8 MiB）
        let read_cap = effective_read_cap(pos);
        let Ok((bytes, eof)) = super::read_tail(
            &c.path,
            base,
            usize::try_from(read_cap).unwrap_or(usize::MAX),
        ) else {
            pos.bad = Some((c.mtime_ms, c.size as i64));
            diag(tally, "会话文件读取失败，本轮跳过");
            return;
        };
        pos.bad = None;
        // zstd 尾部短于帧魔数（新帧刚开始写）= 尚无完整帧，不是损坏：本轮无新增
        if is_zstd && bytes.len() < 4 {
            return;
        }
        // 超大单行 / 巨帧扫描中（R3'/NB2）：plain 找第一个 '\n'；zstd 找下一帧魔数
        //（帧体不再解码——进扫描态前已判定超预算/超窗，内容放弃解析）
        if scanning_oversized {
            if is_zstd {
                match frame_offsets(&bytes).first().copied() {
                    Some(m) => {
                        // 魔数 = 巨帧帧尾：水位越过整帧（内容不解析、不再重读），清扫描态
                        // 后回环一次常规尾读消费其后帧
                        diag(
                            tally,
                            "frame decompresses beyond decode budget, frame skipped (content not parsed)",
                        );
                        pos.oversized_upto = 0;
                        pos.offset = base + m as u64;
                        continue;
                    }
                    None => {
                        if bytes.is_empty() {
                            return; // 扫描位已在文件尾（stat/open 间隙收缩）：等下轮
                        }
                        // 块内无下一帧魔数：帧仍在写（或尚未到下一帧）——扫描位前移
                        // ≤ 8 MiB（持久化），下轮续扫（只扫魔数，不解码帧体）。
                        // 留 3 字节重叠（B1）：魔数骑在块尾最后 1–3 字节时本轮 4 字节
                        // 整窗检测不可见，步进若推到块尾，下轮从魔数序列内部起扫，
                        // 该帧将被永久吞掉。块 ≥ 4 字节时每轮仍至少前进 1 字节。
                        diag(
                            tally,
                            "giant frame end not found in chunk, scanning forward for the next frame magic",
                        );
                        pos.oversized_upto = base + (bytes.len() as u64).saturating_sub(3);
                        return;
                    }
                }
            }
            match bytes.iter().position(|b| *b == b'\n') {
                Some(p) => {
                    // 扫描完成：水位跳过整行（超大行内容不解析），清扫描状态后回环一次，
                    // 常规尾读消费其后的内容
                    diag(
                        tally,
                        "oversized line skipped, position advanced past it (content not parsed)",
                    );
                    pos.oversized_upto = 0;
                    pos.offset = base + p as u64 + 1;
                    continue;
                }
                None => {
                    if bytes.is_empty() {
                        return; // 扫描位已在文件尾（stat/open 间隙收缩）：等下轮
                    }
                    // 本块仍无换行：扫描位置前移 ≤ 8 MiB（持久化），下轮续扫
                    diag(
                        tally,
                        "single line exceeds tail read cap, scanning forward in bounded chunks",
                    );
                    pos.oversized_upto = base + bytes.len() as u64;
                    return;
                }
            }
        }
        // 4) 以相对偏移解码新增内容，再把单元边界加回绝对基址
        let mut tail = match decode_tail(&bytes, 0, is_zstd, eof, decode_budget) {
            Ok(t) => t,
            Err(reason) => {
                pos.bad = Some((c.mtime_ms, c.size as i64));
                diag(tally, &reason);
                return;
            }
        };
        if tail.deferred_count > 0 {
            // F1：本轮共享预算耗尽、余下帧未处理（不消费不越过，下轮以新预算续读）
            diag(
                tally,
                &format!(
                    "batch decode budget exhausted, {} {} deferred",
                    tail.deferred_count,
                    if tail.deferred_count == 1 {
                        "frame"
                    } else {
                        "frames"
                    }
                ),
            );
        }
        for item in &mut tail.items {
            match item {
                TailItem::Unit(u) => {
                    u.start += base;
                    u.end += base;
                }
                TailItem::BombFrame { start, end, .. } => {
                    *start += base;
                    if let Some(e) = end {
                        *e += base;
                    }
                }
                TailItem::PendingFrame { start } => {
                    *start += base;
                }
            }
        }
        if tail.torn_count > 0 {
            diag(tally, "损坏帧被跳过（中间帧，内容丢失）");
        }

        // 5) 逐项消费（预算按行粒度留检查点；炸弹/巨帧按帧粒度整体越过；未决帧停等）
        let mut work = SessionWork {
            id: std::mem::take(&mut pos.id),
            step_starts: std::mem::take(&mut pos.step_starts),
            attempts: std::mem::take(&mut pos.attempts),
        };
        let mut offset = pos.offset;
        let mut skip_lines = pos.skip_lines;
        // 本轮未决帧读界：默认 0 = 无未决（解析成功/降级即归零），PendingFrame 分支翻倍
        let mut pending_cap = 0u64;
        // 帧尾不可知的巨帧：本轮起进入有界帧扫描态（扫描位 = 块尾，持久化在 oversized_upto）
        let mut scan_from: Option<u64> = None;
        'items: for item in &tail.items {
            let unit = match item {
                TailItem::BombFrame { start, end } => {
                    match end {
                        Some(e) => {
                            // 帧尾已知（块内有下一帧魔数）：水位直接越过整帧
                            diag(
                                tally,
                                "frame decompresses beyond decode budget, frame skipped (content not parsed)",
                            );
                            offset = *e;
                        }
                        None => {
                            // 帧尾不可知（块内末帧、解压超预算且压缩流延到块尾或仍在写）：
                            // 水位停在帧起点，进入帧扫描态——绝不把水位落到压缩流中部
                            //（中部无帧魔数，下轮会误判整文件损坏而冻死）
                            diag(
                                tally,
                                "frame decompresses beyond decode budget, frame skipped (content not parsed)",
                            );
                            offset = *start;
                            // 同样留 3 字节重叠（B1）：首个扫描窗的边界上也骑着魔数
                            scan_from = Some(base + (bytes.len() as u64).saturating_sub(3));
                        }
                    }
                    continue;
                }
                TailItem::PendingFrame { start } => {
                    // 跨读界被截断的帧（M2）：「没读全」不是「解不动」——水位停帧起点，
                    // 读界翻倍下轮整帧重读；本帧不消费也不跳过。读界已到 64 MiB 上限仍
                    // 读不全 = 真巨帧 → 按炸弹扫描态降级（诚实诊断，内容放弃解析）。
                    offset = *start;
                    if read_cap >= MAX_PENDING_READ_BYTES {
                        diag(
                            tally,
                            "frame input still incomplete at the 64 MiB read cap, degrading to bounded frame scan (content not parsed)",
                        );
                        scan_from = Some(base + (bytes.len() as u64).saturating_sub(3));
                    } else {
                        pending_cap = (read_cap * 2).min(MAX_PENDING_READ_BYTES);
                    }
                    continue;
                }
                TailItem::Unit(u) => u,
            };
            let lines: Vec<&str> = unit.text.lines().collect();
            let total = lines.len();
            let mut i = skip_lines.min(total);
            skip_lines = 0;
            while i < total {
                if *budget == 0 {
                    break;
                }
                apply_line(lines[i], &mut work, host, observations, tally, budget);
                i += 1;
            }
            if i == total {
                offset = unit.end; // 整单元消费
                if *budget == 0 {
                    break 'items; // 停在单元边界，检查点最干净
                }
            } else {
                // 预算在单元中部耗尽：检查点 =（单元起点, 已消费行数），下轮续读
                offset = unit.start;
                skip_lines = i;
                break 'items;
            }
        }
        pos.id = work.id;
        pos.step_starts = work.step_starts;
        pos.attempts = work.attempts;
        pos.offset = offset;
        pos.skip_lines = skip_lines;
        pos.pending_cap = pending_cap;
        if let Some(p) = scan_from {
            pos.oversized_upto = p;
        }
        // 6) 未决状态 24h 剪枝（与事实保留窗一致；不死条目不无限膨胀）
        pos.step_starts
            .retain(|_, t| now_ms.saturating_sub(*t) <= FRESH_WINDOW_MS);
        pos.attempts
            .retain(|_, a| now_ms.saturating_sub(a.last_ms) <= FRESH_WINDOW_MS);

        if pos.id.is_empty() {
            diag(tally, "会话头未读到（无 id），事件无法归属会话");
        }
        // 7) 超大单行起扫（R3'/NB1）：plain 块读满上限且**整块从头到尾无任何换行**
        //    （offset == base = 什么都没消费到）才是真「单行 ≥ 8 MiB」。块内只要消费出
        //    完整行（offset > base），块尾的残行就只是**正在写的骑界普通行**——下轮从行
        //    边界有界续读、正常解析，绝不起扫（起扫会把普通行整体跳过 = 丢数据）。
        //    扫描位置持久化在游标，每轮扫描工作量 ≤ 8 MiB，绝不整余量重读；
        //    预算/单元中部检查点待决时不起扫（下轮先续读检查点）。
        if !is_zstd
            && bytes.len() >= super::TAIL_READ_MAX_BYTES
            && *budget > 0
            && skip_lines == 0
            && offset == base
        {
            diag(
                tally,
                "single line exceeds tail read cap, scanning forward in bounded chunks",
            );
            pos.oversized_upto = base + bytes.len() as u64;
            return;
        }
        // 常规路径每轮至多一次尾读（超大行跳过完成的那次回环除外）：消费即止
        return;
    }
}

// ==== 事件处理 ====

struct SessionWork {
    id: String,
    step_starts: BTreeMap<String, i64>,
    attempts: BTreeMap<String, AttemptTally>,
}

fn apply_line(
    line: &str,
    work: &mut SessionWork,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        diag(tally, "事件行 JSON 解析失败，跳过");
        return;
    };
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "session" => {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                work.id = id.to_string();
            }
        }
        "step/start" => {
            let Some((turn, step, time)) = turn_step_time(&v) else {
                diag(tally, "step/start 缺 turn/step/time，跳过");
                return;
            };
            work.step_starts.insert(step_key(turn, step), time);
        }
        "assistant/attempt" => apply_attempt(&v, work, host, observations, tally, budget),
        "assistant/message" => apply_message(&v, work, host, observations, tally, budget),
        // step/end（含工具的 step 终结）、tool/*、request/*、model/* 等：模型终结口径不消费
        _ => {}
    }
}

/// `assistant/attempt`：无表面消息的尝试结算（dsh-kimi.md）。
/// 首个 attempt 的起点 = step/start（可对齐）→ 出 Failed 观察；
/// 第 2+ 次 attempt 起点不可定位 → 跳过（不猜）。
fn apply_attempt(
    v: &serde_json::Value,
    work: &mut SessionWork,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    let Some((turn, step, time)) = turn_step_time(v) else {
        diag(tally, "assistant/attempt 缺 turn/step/time，跳过");
        return;
    };
    let key = step_key(turn, step);
    let entry = work.attempts.entry(key.clone()).or_default();
    entry.count += 1;
    entry.last_ms = time;
    if entry.count > 1 {
        diag(tally, "第2+次 attempt 无法定位起点，跳过（不猜）");
        return;
    }
    let Some(start) = work.step_starts.get(&key).copied() else {
        diag(tally, "attempt 缺同 step/start → 无法定位起点，跳过");
        return;
    };
    if *budget == 0 {
        return;
    }
    let kind = v
        .pointer("/data/finish/kind")
        .and_then(|k| k.as_str())
        .unwrap_or("unknown");
    observations.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        stable_call_key: format!("{}:attempt:{turn}:{step}:{}", work.id, entry.count),
        started_at_ms: start,
        first_token_at_ms: None, // 尝试自身流不参与计时（仅首尝试起点可对齐，TTFT 口径留空）
        ended_at_ms: time,
        native_duration_ms: (time - start > 0).then_some(time - start),
        generation_ms: None,       // 失败不入速度（Spec §3）
        response_elapsed_ms: None, // 失败不入速度：响应窗与生成窗同为 None（V2 口径）
        output_tokens: None,
        output_scope: OutputScope::Unknown,
        tool_ms: None,
        // attempt 事件不带 message.source（dsh-kimi.md），模型未知 → 显式未知桶
        model: "unknown".into(),
        channel: None,
        session_key: non_empty(work.id.clone()),
        turn_key: Some(turn.to_string()),
        purpose: Purpose::MainTask,
        outcome: Outcome::Failed,
        method: TimingMethod::StreamObserved,
        failure_reason: Some(format!("assistant/attempt finish.kind={kind}")),
    });
    *budget -= 1;
}

/// `assistant/message`：模型终结（结算即终点）。成功结算的完整事实观察。
fn apply_message(
    v: &serde_json::Value,
    work: &mut SessionWork,
    host: &str,
    observations: &mut Vec<TpsObservation>,
    tally: &mut BTreeMap<String, usize>,
    budget: &mut usize,
) {
    let Some((turn, step, time)) = turn_step_time(v) else {
        diag(tally, "assistant/message 缺 turn/step/time，跳过");
        return;
    };
    let key = step_key(turn, step);
    // 起点：必须能定位 step/start（消息从不凭空造起点）
    let Some(start) = work.step_starts.get(&key).copied() else {
        diag(
            tally,
            "消息缺同 step/start → 无法定位起点，跳过（不造起点）",
        );
        return;
    };
    // 模型：必须来自消息自身 source（不拿会话当前配置猜，addendum）
    let Some(model) = v
        .pointer("/data/message/source/model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
    else {
        diag(
            tally,
            "消息缺 data.message.source.model，跳过（不落无名行）",
        );
        return;
    };
    let msg_provider = v
        .pointer("/data/message/source/provider")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    // 同消息 usage（同消息 Token 归同一次调用）
    let output = v
        .pointer("/data/usage/outputTokens")
        .and_then(|o| o.as_i64());
    if output.is_none() {
        diag(tally, "缺 data.usage.outputTokens → 记缺失不报 0");
    }
    // 紧凑流展开：首非空成员 = 首 token（思考/正文/参数；首个空 delta 跳过）
    let members = expand_stream(v.pointer("/data/stream").and_then(|s| s.as_array()));
    let first_nonempty = members
        .iter()
        .filter(|m| m.nonempty)
        .map(|m| m.time_ms)
        .min();
    let distinct_times: usize = members
        .iter()
        .filter(|m| m.nonempty)
        .map(|m| m.time_ms)
        .collect::<BTreeSet<i64>>()
        .len();
    // replay profile（dsh-output-scope-addendum）只决定**输出包含关系**的标注：命中
    // pi-ai/2/openai-completions = 已证 completion_tokens 含 reasoning → IncludesReasoning；
    // 未命中 → provider-reported「思考计数未单独声明」。它**不**门控生成窗（独立复核 R8；
    // Spec §4「不是只认旧稿的单一 replay profile」）：同消息 outputTokens + 可靠生成窗即有效。
    let profile_ok = replay_profile_matches(v.pointer("/data/message/source"), msg_provider, model);
    if !profile_ok {
        diag(
            tally,
            "replay profile not pi-ai/2/openai-completions -> provider-reported scope (reasoning inclusion unspecified)",
        );
    }
    // 重试对齐：step 内有过 attempt → 最后成功尝试起点不可定位 → 剔除速度（不猜）
    let attempts = work.attempts.get(&key).map(|a| a.count).unwrap_or(0);
    let aligned = attempts == 0;

    let mut generation_ms = None;
    if !aligned {
        diag(
            tally,
            &format!(
                "step carries {attempts} retries, last successful attempt start not locatable -> call excluded from speed (no guessing)"
            ),
        );
    } else if first_nonempty.is_none() {
        diag(tally, "流无非空成员 → 无生成窗");
    } else if distinct_times < 2 {
        diag(tally, "非空流成员只分布在一个时刻（整块突发）→ 排除速度");
    } else {
        let g = time - first_nonempty.unwrap_or(time);
        if g > 0 {
            generation_ms = Some(g);
        } else {
            diag(tally, "生成窗非正 → 不入速度");
        }
    }
    // 起点可对齐才用其起点：TTFT 与单次时长都只在无重试时可信
    let first_token_at_ms = if aligned { first_nonempty } else { None };
    let native_duration_ms = if aligned {
        let d = time - start;
        if d > 0 {
            Some(d)
        } else {
            diag(tally, "整调用时长非正 → 记缺失不报 0");
            None
        }
    } else {
        None
    };
    // 响应窗（V2 Spec §5.2「含等待响应」）：step 开始 → 消息结算（与起点/终点/
    // 整调用时长同边界），含首 token 等待、不含结算之后 step/end 的工具执行；
    // 重试对齐失败 → 与流式口径一并剔除（跨 attempt 的窗不是单次调用）。
    let response_elapsed_ms = native_duration_ms;
    // 调用方保证 budget > 0 才进本函数；本函数至多发射一条（缺预算的路径到不了这里）
    observations.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        // 稳定键刻意**不含代际**：代际切换重读同一逻辑调用时键不变 → UPSERT 幂等
        stable_call_key: format!("{}:{turn}:{step}", work.id),
        started_at_ms: start,
        first_token_at_ms,
        ended_at_ms: time,
        native_duration_ms,
        generation_ms,
        // 同一次输出两个窗（Spec §5.1）：生成窗 = 首非空成员 → 结算；响应窗 =
        // step 开始 → 结算（含等待；见上）。对齐失败时两者同为 None。
        response_elapsed_ms,
        output_tokens: output,
        // 包含关系如实标注：命中 profile = 已证含思考；有输出数但未命中 = provider-reported
        //（是否含思考未单独声明，不强行标已含）；无输出数 = 无事实
        output_scope: match (profile_ok, output.is_some()) {
            (true, true) => OutputScope::IncludesReasoning,
            (false, true) => OutputScope::ProviderReported,
            (_, false) => OutputScope::Unknown,
        },
        tool_ms: None, // 已知限制：工具用时未归属（见模块头）
        model: model.to_string(),
        channel: None,
        session_key: non_empty(work.id.clone()),
        turn_key: Some(turn.to_string()),
        purpose: Purpose::MainTask,
        outcome: Outcome::Success,
        method: TimingMethod::StreamObserved,
        failure_reason: None,
    });
    *budget -= 1;
    // 消息已结算：清该 step 的未决状态（状态有界）
    work.attempts.remove(&key);
    work.step_starts.remove(&key);
}

// ==== 紧凑流展开（dsh-kimi.md 四种流记录）====

struct StreamMember {
    time_ms: i64,
    nonempty: bool,
}

/// `time0 + dt[]（长度=成员数−1）`：成员 j 时刻 = time0 + Σdt[0..j]
fn expand_stream(stream: Option<&Vec<serde_json::Value>>) -> Vec<StreamMember> {
    let Some(records) = stream else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rec in records {
        let kind = rec.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let member_key = match kind {
            "reasoning-chunks" | "text-chunks" => "texts",
            "tool-call-chunks" => "args",
            // chunk 型记录是 block-start/usage/finish 标记：不参与首 token
            _ => continue,
        };
        let Some(t0) = rec.get("time0").and_then(|t| t.as_i64()) else {
            continue;
        };
        let dt: Vec<i64> = rec
            .get("dt")
            .and_then(|d| d.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default();
        let members: Vec<String> = rec
            .get(member_key)
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut acc = 0i64;
        for (j, text) in members.iter().enumerate() {
            let time_ms = t0.saturating_add(acc);
            if let Some(gap) = dt.get(j) {
                acc = acc.saturating_add(*gap);
            }
            out.push(StreamMember {
                time_ms,
                nonempty: !text.trim().is_empty(),
            });
        }
    }
    out
}

/// replay profile 命中判定（dsh-output-scope-addendum.md 证得的形态；只决定包含关系标注，不门控速度）：
/// `pi-ai / 2 / openai-completions`，且 response 的 provider/model 与消息 source 一致
fn replay_profile_matches(
    source: Option<&serde_json::Value>,
    msg_provider: &str,
    msg_model: &str,
) -> bool {
    let Some(resp) = source.and_then(|s| s.pointer("/replayState/response")) else {
        return false;
    };
    resp.get("kind").and_then(|k| k.as_str()) == Some("pi-ai")
        && resp.get("version").and_then(|v| v.as_i64()) == Some(2)
        && resp.get("api").and_then(|a| a.as_str()) == Some("openai-completions")
        && !msg_provider.is_empty()
        && resp.get("provider").and_then(|p| p.as_str()) == Some(msg_provider)
        && resp.get("model").and_then(|m| m.as_str()) == Some(msg_model)
}

// ==== 尾部解码（只读新增）====

struct Unit {
    start: u64,
    end: u64,
    text: String,
}

/// 解码产物项：完整帧（单元）、解压超预算的巨帧（内容放弃解析，整体跳过）、
/// 或跨读界暂时读不全的未决帧（停等下一轮更大读窗）
enum TailItem {
    Unit(Unit),
    /// 巨帧（解压 ≥ 预算，已由完整/足够输入证实）：`end = Some(帧尾)` = 块内已知帧尾
    ///（其后还有帧魔数）→ 水位直接越过；`end = None` = 帧尾不可知（块内末帧、压缩流
    /// 延到块尾或仍在写）→ 进入有界帧扫描态（水位停帧起点，`oversized_upto` 承载
    /// 扫描位，后续轮只扫下一帧魔数）
    BombFrame {
        start: u64,
        end: Option<u64>,
    },
    /// 未决帧（M2）：解码失败且非文件尾 = 只是「没读全」，无法区分普通小帧与巨帧——
    /// 水位停帧起点、读界翻倍（`pending_cap`），下轮以更大读窗重新判定
    PendingFrame {
        start: u64,
    },
}

struct Tail {
    items: Vec<TailItem>,
    /// 中间坏帧计数（内容丢失，诊断）
    torn_count: usize,
    /// 因本轮共享解码预算耗尽而未处理的帧数（F1：正常帧不消费不越过，下轮以新预算
    /// 续读；与「炸弹跳过」「撕裂丢弃」是三种不同结局，诊断分开留痕）
    deferred_count: usize,
}

/// 与 `monitor/dsh/decode.rs` 的帧扫描同款魔数（该模块的公开 API 只有全帧解码，
/// 选择性解码需要帧偏移；monitor/ 不在本任务改动范围 → 本地镜像）
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// 单帧解码预算（R3'）：解压输出至多 8 MiB（= 尾读上限），超出即判解压炸弹、整帧跳过。
/// `decode_all` 会先全量解压再谈预算——12 MiB 压缩块可膨胀出任意大的内存与拷贝量。
const DECODE_BUDGET_BYTES: usize = super::TAIL_READ_MAX_BYTES;

fn frame_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == ZSTD_MAGIC {
            offsets.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }
    offsets
}

/// 单帧有界解码结果（`decoded` = 本帧已产出的解码字节量，计入整批解码预算）
enum FrameDecode {
    Complete {
        text: String,
        decoded: usize,
    },
    /// 解压输出超过预算（解压炸弹）：整帧跳过，水位越过帧尾
    BeyondBudget,
    /// 解码失败（撕裂/损坏帧；`decoded` = 失败前已产出量）
    Truncated {
        decoded: usize,
        reason: String,
    },
}

/// 有界解码单帧：输出截到 `cap`；恰好读满 `cap` 时向解码器再探 1 字节，
/// 有剩余输出 = 还有更多输出（是否判炸弹由调用方结合预算语义决定），恰好在 `cap`
/// 处 EOF = 合法帧。撕裂帧表现为读错误（Truncated）。
fn decode_frame_capped(input: &[u8], cap: usize) -> FrameDecode {
    use std::io::Read;
    let decoder = match zstd::stream::read::Decoder::new(input) {
        Ok(d) => d,
        Err(e) => {
            return FrameDecode::Truncated {
                decoded: 0,
                reason: e.to_string(),
            }
        }
    };
    let mut limited = decoder.take(cap as u64);
    let mut buf = Vec::new();
    if let Err(e) = limited.read_to_end(&mut buf) {
        return FrameDecode::Truncated {
            decoded: buf.len(),
            reason: e.to_string(),
        };
    }
    if buf.len() >= cap {
        // 预算耗尽：绕过 take 的限额直探解码器，判别「帧恰好在此结束」还是「还有输出」
        let mut probe = [0u8; 1];
        return match limited.get_mut().read(&mut probe) {
            Ok(0) => FrameDecode::Complete {
                text: String::from_utf8_lossy(&buf).to_string(),
                decoded: buf.len(),
            },
            Ok(_) => FrameDecode::BeyondBudget,
            Err(e) => FrameDecode::Truncated {
                decoded: buf.len(),
                reason: e.to_string(),
            },
        };
    }
    FrameDecode::Complete {
        text: String::from_utf8_lossy(&buf).to_string(),
        decoded: buf.len(),
    }
}

/// 自 `from` 起只解码新增内容：
/// * zstd：逐帧独立解码 `start >= from` 的帧；解码量计入 `decode_budget`（**整源共享**
///   预算，由 `read_incremental` 建立一次、跨会话逐层传递，不在每文件重新初始化——F1）。
///   单帧取 `min(单帧上限, 剩余余量)`：余量内解完 → 正常单元；被**余量**截断（take <
///   单帧上限且输入未尽）→ 不是炸弹，本帧不消费不越过（检查点停在最后已解码单元末尾），
///   余下帧计为 deferred（诊断），下轮以新预算重解；顶满**单帧上限**仍有剩余输出 →
///   真炸弹（既有越过/扫描态路径，分类解码同样计入预算）。**文件尾撕裂小帧不越过**
///   （下轮重解），中间帧失败跳过并计数（内容丢失，与 decode_zstd_frames 的容错口径
///   一致）；非文件尾的截断帧记 [`TailItem::PendingFrame`]（停等更大读窗，M2）；
/// * plain：读 `from` 之后最后一个 '\n'（含）为止，残行留待下轮（不解码，预算不受影响）。
fn decode_tail(
    bytes: &[u8],
    from: u64,
    is_zstd: bool,
    at_eof: bool,
    decode_budget: &mut usize,
) -> Result<Tail, String> {
    let from = (from as usize).min(bytes.len());
    if !is_zstd {
        let slice = &bytes[from..];
        let complete = slice
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let text = String::from_utf8_lossy(&slice[..complete]).to_string();
        return Ok(Tail {
            items: if text.is_empty() {
                Vec::new()
            } else {
                vec![TailItem::Unit(Unit {
                    start: from as u64,
                    end: (from + complete) as u64,
                    text,
                })]
            },
            torn_count: 0,
            deferred_count: 0,
        });
    }
    let offsets = frame_offsets(bytes);
    if offsets.is_empty() {
        return Err("DSH_LOG_MAGIC_MISSING: zstd 会话日志无帧魔数".into());
    }
    let mut items = Vec::new();
    let mut torn_count = 0usize;
    // 因本轮共享预算耗尽而未处理的帧数（正常帧，不消费不越过，下轮以新预算续读；
    // 诊断由 read_session 统一渲染）
    let mut deferred_count = 0usize;
    for (idx, &start) in offsets.iter().enumerate() {
        if start < from {
            continue; // 已消费帧
        }
        if *decode_budget == 0 {
            deferred_count += 1; // 本轮余量已尽：余下帧不解不跳，下轮续
            continue;
        }
        let end = offsets.get(idx + 1).copied().unwrap_or(bytes.len());
        // 单帧预算 = min(单帧上限, 剩余余量)：余量不足时按余量截断，截断 ≠ 超限
        let take = DECODE_BUDGET_BYTES.min(*decode_budget);
        match decode_frame_capped(&bytes[start..end], take) {
            FrameDecode::Complete { text, decoded } => {
                *decode_budget = decode_budget.saturating_sub(decoded);
                items.push(TailItem::Unit(Unit {
                    start: start as u64,
                    end: end as u64,
                    text,
                }));
            }
            FrameDecode::BeyondBudget => {
                if take < DECODE_BUDGET_BYTES {
                    // 被本轮余量截断的正常帧（F1）：不是炸弹——检查点停在最后已解码
                    // 单元末尾（items 就此止步），本帧不消费不越过，下轮以新预算重解；
                    // 「本轮余量不足」与「单帧超限」必须区分，正常帧绝不因轮次预算被
                    // 误判丢弃
                    *decode_budget = decode_budget.saturating_sub(take);
                    deferred_count += 1;
                    continue;
                }
                // 顶满单帧上限仍有剩余输出 = 真炸弹。块内有下一帧魔数 = 帧尾已知，
                // 水位直接越过；块内末帧 = 帧尾不可知（压缩流延到块尾——可能仍在写），
                // 交帧扫描态定位。分类解码（至多单帧上限）同样计入共享预算。
                *decode_budget = decode_budget.saturating_sub(DECODE_BUDGET_BYTES);
                let known_end = offsets.get(idx + 1).copied().map(|e| e as u64);
                items.push(TailItem::BombFrame {
                    start: start as u64,
                    end: known_end,
                });
            }
            FrameDecode::Truncated { decoded, reason } => {
                *decode_budget = decode_budget.saturating_sub(decoded);
                let is_last_frame = idx + 1 == offsets.len();
                if is_last_frame {
                    if at_eof {
                        // 文件尾撕裂小帧（写方 EOF 打断，设计内常态）：不越过，下轮整帧重解
                        log::debug!("tps/dsh: 尾帧待完成，下轮重解: {reason}");
                        break;
                    }
                    // 非文件尾 = 后面还有字节，本帧只是「没读全」而非「解不动」（M2）：
                    // 记未决帧，水位停帧起点、读界翻倍下轮重读；真巨帧由后续轮的更大
                    // 读窗判定（解压 ≥ 预算 → 炸弹扫描态；64 MiB 仍读不全 → 降级扫描态）
                    items.push(TailItem::PendingFrame {
                        start: start as u64,
                    });
                    continue;
                }
                torn_count += 1; // 中间坏帧：内容丢失，跳过
            }
        }
    }
    Ok(Tail {
        items,
        torn_count,
        deferred_count,
    })
}

// ==== 小工具 ====

fn parse_state(cursor: &SourceCursor) -> DshCursorState {
    if cursor.value.is_empty() {
        return DshCursorState::default();
    }
    serde_json::from_str(&cursor.value).unwrap_or_else(|e| {
        log::warn!("tps/dsh: 游标状态解析失败（{e}）→ 归零重读（稳定键 UPSERT 幂等）");
        DshCursorState::default()
    })
}

/// (turn, step, time) 三元组；任一缺失 → None
fn turn_step_time(v: &serde_json::Value) -> Option<(i64, i64, i64)> {
    let time = v.get("time").and_then(|t| t.as_i64())?;
    let turn = v.pointer("/data/turn").and_then(|t| t.as_i64())?;
    let step = v.pointer("/data/step").and_then(|s| s.as_i64())?;
    Some((turn, step, time))
}

fn step_key(turn: i64, step: i64) -> String {
    format!("{turn}:{step}")
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
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

/// 空批（根缺失等正常空态）：游标原样、ok、无诊断
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
