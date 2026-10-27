//! ZCode 原生 TPS 读取器（Spec §4 ZCode 行；证据：sqlite-sources.md）。
//!
//! ## 读取对象与口径
//!
//! * 库 = `<home>/.zcode/cli/db/db.sqlite`，经 `monitor::sqlite::open_usage_db`
//!   **只读打开**（ro 优先、探针、immutable 回退；**绝不 checkpoint、绝不写**，
//!   尊重 WAL——与 usage/zcode 采集器同入口）。
//! * 生成窗 = 原生 `first_token_at` → `completed_at`（Spec §4「原生model_usage首点至
//!   completed」）。`first_token_at` 只当正文/思考首点用，**不推造参数首点**
//!   （Spec §4「标首点仅正文/思考」；首点内容形态待实测，见 sqlite-sources.md
//!   「日后必须定向实测」题 1）。
//! * 响应窗（V2 Spec §5.2「含等待响应」）= 行级 verified `completed_at − started_at`
//!   （与行自身 `duration_ms` 同窗，不含后续工具执行）；成功且 retry_count=0 才填实
//!   （与生成窗同一套同调用验证），缺 F 的完整调用 response-only 有效。
//! * 入速条件（Spec §4「成功/retry_count=0/正窗」）：`status='completed'` 且
//!   `retry_count=0` 且窗为正。失败/取消不入速度（保留候选与起止事实）；
//!   **缺 F 的完整调用为 response-only 有效，流式口径跳过**（响应窗照入，见
//!   `build_observation_inner`）；retry>0 无法对齐内部尝试 → 剔除速度并诊断。
//! * 输出语义（sqlite-sources.md「token、重试、取消」）：`computed_total = input+output`
//!   全量成立、reasoning ⊆ output → reasoning>0 标 `IncludesReasoning`（不再相加），
//!   reasoning=0 标 `ExclusiveOutput`。output 列缺失记 None（不报 0）。
//! * **重复代际只计一次**：稳定键 = `logical_request_id`（缺失回退 `session:rowid`）；
//!   批内同键重放只保留最新一行（与 UPSERT 覆盖语义一致）+ 诊断。
//! * 工具用时：`tool_usage` 与 `model_usage` **无原生关联键**
//!   （sqlite-sources.md「粒度和关联」）→ `tool_ms` 恒空（工具归属不唯一为 null，
//!   不填 0，Spec §6）。
//! * purpose：`task_type` 的 `session_title` → AssistTitle、`goal_summary_title` →
//!   AutoSummary（证据文档点名的辅助请求类型），其余默认 MainTask。
//!
//! ## 增量预算（Spec §2「单次原生SQL每宿主最多尾200行」）
//!
//! * 游标 = {rowid 水位, 完成时间复合机会游标 (completed_at, rowid)}（JSON；旧纯数字
//!   串映射 watermark、旧 JSON 未知字段忽略——旧值不被解释成新语义；**独立游标、不复用
//!   旧 usage_cursor**，Spec §6）。
//! * **水位不跨在飞行行**（usage/zcode 采集器裁决 B 镜像）：`status='running'` 的行是
//!   插入后原地 UPDATE 收尾的，跨过即永久丢失该行终态；只消费连续终态前缀。
//!   僵尸安全阀：running 行老于 24h（超真机最长样本一个数量级）强制跨过，防水位冻死；
//!   **僵尸行是唯一被跨过的非终态形态**——僵尸放行后窗口内更靠后的新鲜 running 行
//!   仍会停住（消费循环第二道闸），水位永不越过任何还可能收尾的行。
//! * **机会采样**（水位被新鲜 running 行挡住时的旁路）：对 rowid > 水位 且
//!   (completed_at, rowid) > 机会游标的终态行做有界读取（最新完成事实优先）——原地
//!   完成带更晚 completed_at，下轮必被发现；机会行不推进水位（收尾后正常路径重读一次，
//!   稳定键幂等）。
//! * 单次 `LIMIT 200` 尾读，余量下轮续读；不做全表 COUNT 轮询。
//! * 列/行级读失败整源 `Err`（D-38 同族：静默少算比响亮失败危险），水位不推进。

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::host_instance_tag;
use crate::monitor::sqlite::open_usage_db;
use crate::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use crate::services::tps::store::TPS_BATCH_MAX_ROWS;
use crate::services::usage::error::UsageError;

/// 来源 id（与 `UsageSourceId::ZCode.db_id()` 同串）
pub const SOURCE_ID: &str = "zcode";

/// 僵尸 `running` 行阈值（usage/zcode 采集器同款：超真机最长在飞行一个数量级；
/// 超过仍 running = 崩溃残留，强制跨过水位，否则该源永久冻死）
const ZOMBIE_RUNNING_MS: i64 = 24 * 60 * 60 * 1000;

/// 单次尾读上限（Spec §2「单次原生SQL每宿主最多尾200行」；与 commit_batch 硬闸同源）
const BATCH_CAP_ROWS: i64 = TPS_BATCH_MAX_ROWS as i64;

/// 游标状态（F2：机会采样游标从「最大已见终态 rowid」升级为 (completed_at, rowid)
/// **完成时间复合游标**）：
/// * `watermark` = 已提交的 rowid 水位（绝不越过新鲜 running 行）；
/// * `op_completed_at` / `op_rowid` = 机会采样已覆盖到的最新完成事实——水位被新鲜
///   running 行挡住时，机会采样窗的复合下界（同刻完成行按 rowid 决胜；0/0 = 无未决
///   机会窗）。rowid 单调下界会把「更小 rowid、更晚完成」的原地收尾行永久排除在采样
///   外（四轮复审反例）；复合游标只排除「完成事实已覆盖」的行——原地完成带更晚的
///   completed_at，下轮必被发现。
///
/// 旧格式升级：
/// * 纯 rowid 十进制串（升级自前版二进制）解析回退为
///   `{watermark: n, op_completed_at: 0, op_rowid: 0}`——行为与升级前逐字等价；
/// * 上一版 JSON 游标里的 `opportunistic_until`（旧「最大已见 rowid」下界）是本结构的
///   未知字段，serde 默认忽略——旧值**绝不**被解释成新语义（既不作 rowid 下界、也不作
///   完成时间下界）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ZcodeCursorState {
    #[serde(default)]
    watermark: i64,
    #[serde(default)]
    op_completed_at: i64,
    #[serde(default)]
    op_rowid: i64,
}

fn parse_cursor_state(value: &str) -> ZcodeCursorState {
    if value.is_empty() {
        return ZcodeCursorState::default();
    }
    if let Ok(state) = serde_json::from_str::<ZcodeCursorState>(value) {
        return state;
    }
    if let Ok(watermark) = value.parse::<i64>() {
        return ZcodeCursorState {
            watermark,
            op_completed_at: 0,
            op_rowid: 0,
        };
    }
    log::warn!("tps/zcode: 游标 `{value}` 非法 → 归零重读（稳定键 UPSERT 幂等）");
    ZcodeCursorState::default()
}

pub fn read_incremental(
    home: &Path,
    cursor: &SourceCursor,
    now_ms: i64,
) -> Result<ReadBatch, UsageError> {
    let db = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    let host = host_instance_tag(&db);
    // 未装 zcode = 正常空态（不是错误）：ok 批、游标原样
    if !db.exists() {
        return Ok(ReadBatch {
            observations: Vec::new(),
            source_state: SourceState {
                source: SOURCE_ID.into(),
                cursor: cursor.clone(),
                ok: true,
                detail: None,
            },
            diagnostics: Vec::new(),
            next_cursor: cursor.clone(),
        });
    }
    let Some(conn) = open_usage_db(&db) else {
        return Err(UsageError::new(
            "usage-source-db-open",
            format!(
                "tps/zcode: failed to open {} (both ro and immutable failed)",
                db.display()
            ),
        ));
    };
    // 游标 = {rowid 水位, 完成时间复合机会游标}（旧纯数字串向后兼容、旧 JSON 未知字段
    // 忽略；坏值归零重读——观察以稳定键 UPSERT 幂等）
    let state = parse_cursor_state(&cursor.value);
    let after_committed = state.watermark;
    // 机会采样游标 (completed_at, rowid)：只排除「完成事实已覆盖」的行，rowid 序不再
    // 构成排除面（F2）
    let mut op_cursor = (state.op_completed_at, state.op_rowid);
    let mut tally: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    // 冷启动 / 积压（独立复核 R7/R7'）：不从最旧记录按 rowid 追赶——跳到近期尾部；
    // 水位目标只受「新鲜 running 行」约束（近期完成行改由补采查询找回，不再拦跳转），
    // 否则数千条历史要跑多轮（后台 600 秒/轮）才读到新调用，近期调用早已错过 120 秒采样窗。
    let (after, skipped_stale) = backlog_start(&conn, after_committed, now_ms)?;
    // 水位不跨在飞行行（裁决 B 镜像 + 僵尸安全阀）
    let until = inflight_boundary(&conn, after, now_ms)?;
    // R7'：被跳过区域 (after_committed, after] 里「保护窗内完成」的行同轮补采——
    // 新→旧取（最有用的优先），升序回拼在尾读之前；补采 + 尾读合计 ≤ 200。
    let mut rows: Vec<ModelRow> = Vec::new();
    let mut net_skipped = skipped_stale;
    let mut tail_limit = BATCH_CAP_ROWS;
    if after > after_committed {
        let recovered = load_recent_completed(
            &conn,
            after_committed,
            after,
            now_ms - BACKLOG_PROTECT_MS,
            RECOVER_BUDGET_ROWS,
        )?;
        if !recovered.is_empty() {
            net_skipped = net_skipped.saturating_sub(recovered.len() as i64);
            diag(
                &mut tally,
                &format!(
                    "backlog recovery: ingested {} recently completed rows from the skipped region",
                    recovered.len()
                ),
            );
            tail_limit -= recovered.len() as i64;
            rows.extend(recovered);
        }
    }
    if net_skipped > 0 {
        diag(
            &mut tally,
            &format!("cold start/backlog: skipped {net_skipped} stale rows before the recent tail"),
        );
    }
    rows.extend(load_rows(&conn, after, until, tail_limit)?);

    // 机会采样（M3 收尾：近期完成事实恢复与未终态水位保护解耦的后半边；F2 改复合
    // 完成时间游标）：水位被新鲜 running 行挡住（watermark < until，补采门在 jump 被
    // 锁死时也不触发）时，其后已完成的调用不得全部错过采样窗——对 rowid > 水位 且
    // (completed_at, rowid) > 机会游标的**终态行**做一次有界读取（最新完成事实优先，
    // k = 200 − 本批已取行数，反转升序拼接）。原地收尾的行带更晚的 completed_at →
    // 下轮必被复合游标发现（rowid 序不构成排除面）。这些行照常入观察（稳定键 UPSERT
    // 幂等）但**不推进水位**；机会游标推进到本轮取得的最新 (completed_at, rowid)。
    // running 行收尾后，正常路径从水位恢复推进，其间被机会采过的行重读一次（幂等，
    // 有界自愈）。
    let main_rows = rows.len();
    if let Some(until_rowid) = until {
        if after_committed < until_rowid {
            let k = BATCH_CAP_ROWS - rows.len() as i64;
            if k > 0 {
                let opportunistic =
                    load_opportunistic_completed(&conn, after_committed, op_cursor, now_ms, k)?;
                if !opportunistic.is_empty() {
                    diag(
                        &mut tally,
                        &format!(
                            "opportunistic sample beyond fresh-running head: {} rows",
                            opportunistic.len()
                        ),
                    );
                    // 机会游标推进到本轮取得的最新完成事实（完成时间优先、同刻 rowid
                    // 决胜——与查询排序一致）
                    if let Some(newest) = opportunistic
                        .iter()
                        .max_by_key(|r| (r.completed_at.unwrap_or(i64::MIN), r.rowid))
                    {
                        op_cursor = (newest.completed_at.unwrap_or(0), newest.rowid);
                    }
                    rows.extend(opportunistic);
                }
            }
        }
    }

    let mut observations: Vec<TpsObservation> = Vec::new();
    // 批内按稳定键去重（重复代际重放只计一次；保留最新一行，与 UPSERT 覆盖语义一致）
    let mut index_of_key: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut max_rowid = after;
    for (idx, r) in rows.iter().enumerate() {
        // **水位不变量**：新鲜 running 行（正在生成、将原地 UPDATE 收尾）之前必须停住——
        // 越过它，该行的完成态就永远不会被重读（sqlite-sources.md「不能单靠 rowid
        // 推进跳过未终态」）。`inflight_boundary` 探针只认新鲜 running 行（窗口内不
        // 应出现），此处是第二道闸；机会采样行不带水位推进，不构成越过面。
        if r.status == "running" && !is_zombie_running(r, now_ms) {
            diag(
                &mut tally,
                "新鲜 running 行 → 水位停在其之前（原地 UPDATE 收尾后下轮补采）",
            );
            break; // 该行不进 max_rowid、不产观察，整体停住
        }
        // 机会采样行**不推进水位**（绝不越过新鲜 running 行；水位不动它们下轮可被
        // 正常路径重读一次，稳定键幂等）
        if idx < main_rows {
            max_rowid = max_rowid.max(r.rowid);
        }
        if r.status == "running" {
            // 僵尸行（started_at 缺失或 ≥24h 无终态 = 崩溃残留）：唯一被跨过的非终态
            // 形态——安全阀强制跨过，否则水位永久冻死；该行无终态，不入观察也不会补采
            diag(
                &mut tally,
                "僵尸 running 行 → 强制跨过水位（该行无终态，不入观察）",
            );
            continue;
        }
        // 形状检查对重流行同样生效（缺起点/缺模型的重流行不得顶掉已有合格观察）
        let Some(obs) = build_observation(r, &host, &mut tally) else {
            continue; // 形状不合格（缺起点/缺模型）：跳过 + 诊断，不产观察
        };
        let key = stable_call_key(r);
        if let Some(slot) = index_of_key.get(&key).copied() {
            observations[slot] = obs;
            diag(
                &mut tally,
                "同一 stable call key 重放多行，只计一次（保留最新）",
            );
            continue;
        }
        index_of_key.insert(key, observations.len());
        observations.push(obs);
    }

    let cursor_state = ZcodeCursorState {
        watermark: max_rowid,
        op_completed_at: op_cursor.0,
        op_rowid: op_cursor.1,
    };
    let next_cursor = SourceCursor {
        source: SOURCE_ID.into(),
        value: serde_json::to_string(&cursor_state).unwrap_or_default(),
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
const SOURCE_LIMITS: &str = "ZCode tool_usage 与 model_usage 无原生关联（tool_ms 恒空）；first_token_at 的内容形态（正文/思考/参数）待实测，仅当正文/思考首点使用；retry>0 的内部尝试不可对齐，剔除速度";

// ==== SQL 侧 ====

/// model_usage 行（列名逐字对齐 sqlite-sources.md 的脱敏 schema）
struct ModelRow {
    rowid: i64,
    logical_request_id: String,
    session_id: String,
    turn_id: Option<String>,
    model_id: String,
    status: String,
    /// None = 缺字段（不得伪装成 0）
    started_at: Option<i64>,
    first_token_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    output_tokens: Option<i64>,
    reasoning: i64,
    retry_count: i64,
    task_type: String,
}

/// model_usage 行的 SELECT 列清单（列名逐字对齐 sqlite-sources.md 的脱敏 schema；
/// 尾读与补采两路查询共用，列位对齐 `read_model_row`）
const MODEL_ROW_COLUMNS: &str = "rowid, COALESCE(logical_request_id,''), session_id, turn_id,
                      COALESCE(model_id,''), COALESCE(status,''),
                      started_at, first_token_at, completed_at, duration_ms,
                      output_tokens, COALESCE(reasoning_tokens,0), COALESCE(retry_count,0),
                      COALESCE(task_type,'')";

/// 行 → ModelRow（列位对齐 `MODEL_ROW_COLUMNS`）
fn read_model_row(r: &rusqlite::Row) -> rusqlite::Result<ModelRow> {
    Ok(ModelRow {
        rowid: r.get(0)?,
        logical_request_id: r.get(1)?,
        session_id: r.get(2)?,
        turn_id: r.get(3)?,
        model_id: r.get(4)?,
        status: r.get(5)?,
        started_at: r.get(6)?,
        first_token_at: r.get(7)?,
        completed_at: r.get(8)?,
        duration_ms: r.get(9)?,
        output_tokens: r.get(10)?,
        reasoning: r.get(11)?,
        retry_count: r.get(12)?,
        task_type: r.get(13)?,
    })
}

fn map_err_io(context: &str, e: rusqlite::Error) -> UsageError {
    UsageError::new("usage-source-io", format!("tps/zcode: {context}: {e}"))
}

/// rowid 尾读（M3：**DESC 取最新 `limit` 行再反转回升序**）：`rowid > after AND
/// rowid < until ORDER BY rowid DESC LIMIT ?`。升序尾读会把窗口内**最新**的行挤出
/// 当轮（补采占去名额后，唯一在 120 秒窗内的新鲜样本要等下一轮——后台 600 秒/轮
/// 节奏下已错过一切采样点）；DESC 永远优先最新完成事实，被挤出的只是尾窗外更陈旧
/// 的行（跳过语义内）。反转后按 rowid 升序拼接，保因果序。不做全表 COUNT 轮询。
/// 刻意不 select `provider_id`：SeriesKey 无 provider 位（Task 1 固定形状），
/// TPS 观察不落供应商列——以免读取层做没有任何消费者的字段搬运。
fn load_rows(
    conn: &Connection,
    after: i64,
    until: Option<i64>,
    limit: i64,
) -> Result<Vec<ModelRow>, UsageError> {
    let sql = format!(
        "SELECT {MODEL_ROW_COLUMNS} FROM model_usage
         WHERE rowid > ?1 AND rowid < ?2 ORDER BY rowid DESC LIMIT ?3"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_err_io("model_usage query failed (schema mismatch)", e))?;
    let rows = stmt
        .query_map(
            rusqlite::params![after, until.unwrap_or(i64::MAX), limit],
            read_model_row,
        )
        .map_err(|e| map_err_io("model_usage read failed", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| {
            // 行级失败整源 Err（D-38 同族）：本轮作废、下轮重试、水位不推进
            map_err_io("model_usage row read failed", e)
        })?);
    }
    out.reverse(); // 新→旧取出 → 升序回拼（保因果序）
    Ok(out)
}

/// 单次补采上限（R7'）：被跳过区域的近期完成行新→旧取，最有用的优先；与尾读合计 ≤ 200
/// （commit_batch 的硬闸），超出的近期行按「余量下轮续读」的既有检查点语义延后。
const RECOVER_BUDGET_ROWS: i64 = 100;

/// R7'：被跳过区域 `(after, target]` 里「保护窗内完成」的行补采——新→旧取 `limit` 行
///（M3：按 `completed_at` DESC = 最有用优先，落实），返回按完成时刻升序（rowid 序的
/// 最佳近似，调用方直接拼接在尾读之前保因果序）。只补 `completed_at` 落在保护窗内的
/// 行：running 行 `completed_at` 为 NULL 天然落选（僵尸行无终态可补，新鲜行由水位闸
/// 拦住等它收尾），陈旧行（结束于保护窗之前）不补——它们进不了任何采样窗。
fn load_recent_completed(
    conn: &Connection,
    after: i64,
    target: i64,
    floor_ms: i64,
    limit: i64,
) -> Result<Vec<ModelRow>, UsageError> {
    let sql = format!(
        "SELECT {MODEL_ROW_COLUMNS} FROM model_usage
         WHERE rowid > ?1 AND rowid <= ?2 AND completed_at > ?3
         ORDER BY completed_at DESC LIMIT ?4"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_err_io("backlog recovery query failed (schema mismatch)", e))?;
    let rows = stmt
        .query_map(
            rusqlite::params![after, target, floor_ms, limit],
            read_model_row,
        )
        .map_err(|e| map_err_io("backlog recovery read failed", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_err_io("backlog recovery row read failed", e))?);
    }
    out.reverse(); // 新→旧取出 → 升序回拼（与尾读衔接保因果）
    Ok(out)
}

/// 机会采样读取（M3 收尾引入；F2 改**完成时间复合游标**）：水位被新鲜 running 行挡住
/// 时，对其后「已完成」的行做有界读取。谓词三段：
/// * `rowid > watermark`——水位之下的行归正常路径（或已跳过），机会采样不重复扫；
/// * `(completed_at, rowid) > 机会游标`（字典序，完成时间优先、同刻 rowid 决胜）——
///   只排除「完成事实已覆盖」的行。rowid 单调下界会把「更小 rowid、更晚完成」的原地
///   收尾行永久排除（四轮复审反例），完成时间游标不会：原地完成带更晚 completed_at →
///   下轮必被发现；
/// * `completed_at IS NOT NULL` 与终态过滤合取：新鲜 running 行（completed_at 为 NULL）
///   天然落选——机会采样绝不越过它；僵尸 running 行与「终态但缺 completed_at」的异常
///   行同样落选——谓词收紧后消费循环安全阀不再经机会窗处置它们，统一归主水位路径
///   （running 头收尾后从水位扫回，稳定键幂等）。
///   取最新 `limit` 行（(completed_at, rowid) DESC），反转升序拼接保因果序。这些行只入
///   观察，水位推进仍由正常路径负责（收尾后重读一次，幂等）。
fn load_opportunistic_completed(
    conn: &Connection,
    watermark: i64,
    op_cursor: (i64, i64),
    now_ms: i64,
    limit: i64,
) -> Result<Vec<ModelRow>, UsageError> {
    let sql = format!(
        "SELECT {MODEL_ROW_COLUMNS} FROM model_usage
         WHERE rowid > ?1 AND completed_at IS NOT NULL
           AND (completed_at > ?2 OR (completed_at = ?2 AND rowid > ?3))
           AND (status <> 'running' OR started_at IS NULL OR ?4 - started_at >= ?5)
         ORDER BY completed_at DESC, rowid DESC LIMIT ?6"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_err_io("opportunistic sample query failed (schema mismatch)", e))?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                watermark,
                op_cursor.0,
                op_cursor.1,
                now_ms,
                ZOMBIE_RUNNING_MS,
                limit
            ],
            read_model_row,
        )
        .map_err(|e| map_err_io("opportunistic sample read failed", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_err_io("opportunistic sample row read failed", e))?);
    }
    out.reverse(); // 新→旧取出 → 升序回拼（保因果序）
    Ok(out)
}

/// 僵尸 `running` 行判定（与 `inflight_boundary` 的安全阀同一规则，单一语义两处引用）：
/// `started_at` 缺失（无法证明它还在跑）或已等待 ≥ `ZOMBIE_RUNNING_MS`
/// （超真机最长在飞行一个数量级 = 崩溃残留）。
fn is_zombie_running(r: &ModelRow, now_ms: i64) -> bool {
    debug_assert_eq!(r.status, "running");
    match r.started_at {
        Some(started_at) => now_ms.saturating_sub(started_at) >= ZOMBIE_RUNNING_MS,
        None => true,
    }
}

/// 积压保护窗（独立复核 R7/R7'）：与 Spec §2「最新读数超过 10 分钟不再当当前值」同口径。
/// 冷启动/积压跳到近期尾部时，落在被跳过区域里的「保护窗内完成」行由补采查询同轮找回
/// （`load_recent_completed`）；水位目标本身**只**受新鲜 running 行约束。
const BACKLOG_PROTECT_MS: i64 = 10 * 60 * 1000;

/// 冷启动 / 积压的起始水位：当 (after, max] 的积压超过单批尾窗（200 行）时，跳到
/// `max − 200`。R7'（评审追记）：水位目标只受第一条「新鲜 running 行」约束——单条 SQL
/// 聚合探测（`MIN(rowid)`，新鲜度在 SQL 内判定，天然完整无 LIMIT 截断；此前
/// `... OR status='running' ... ORDER BY rowid LIMIT 64` 的探针会被僵尸行占满，漏探
/// 其后的新鲜 running 行而越过它）。被跳过的只有「结束于保护窗之前、且位于尾窗之外」
/// 的历史行（保护窗内完成的行由补采找回，见 `load_recent_completed`）——它们进不了
/// 任何采样点（点只看前 120 秒），损失仅限 24h 事实面的 series 发现，换来近期调用
/// 立即可采。返回 (起始水位, 跳过的陈旧行数)。
fn backlog_start(conn: &Connection, after: i64, now_ms: i64) -> Result<(i64, i64), UsageError> {
    let max_rowid: i64 = conn
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM model_usage", [], |r| {
            r.get(0)
        })
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps/zcode: max rowid probe failed (schema mismatch): {e}"),
            )
        })?;
    let jump = max_rowid - BATCH_CAP_ROWS;
    if jump <= after {
        return Ok((after, 0));
    }
    // 新鲜 running 行探测（一条 SQL，无截断）：僵尸行（缺起点或 ≥24h）不构成保护，
    // 由安全阀跨过；保护窗内的「近期完成」行不在此列——它们改由补采查询找回（R7'）。
    let fresh_running: Option<i64> = conn
        .query_row(
            "SELECT MIN(rowid) FROM model_usage
             WHERE rowid > ?1 AND rowid <= ?2 AND status = 'running'
               AND started_at IS NOT NULL AND ?3 - started_at < ?4",
            rusqlite::params![after, jump, now_ms, ZOMBIE_RUNNING_MS],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps/zcode: fresh running probe failed (schema mismatch): {e}"),
            )
        })?;
    let target = match fresh_running {
        Some(rowid) if rowid <= jump => rowid - 1, // 停在新鲜 running 行之前（原地 UPDATE 收尾后补采）
        _ => jump,
    };
    let target = target.max(after);
    Ok((target, target - after))
}

/// 水位边界（usage/zcode 采集器「裁决 B + 僵尸安全阀」镜像）：返回必须停在其**之前**
/// 的 rowid；None = 无在飞行行，可读到尾。探测只认**新鲜** running 行（M3：与
/// `backlog_start` 的跳转探针同一新鲜度谓词）——僵尸行不再构成边界，由消费循环的
/// 安全阀强制跨过；此前「首条 running 行是僵尸行 → None → 放开整个窗口」的语义在
/// DESC 尾读下会把窗口内更后方的新鲜 running 行暴露给消费循环（DESC 序里它前面的
/// 完成行先被消费、水位随之越过它），故窗口边界统一按新鲜度判定：窗口内永远不含
/// 新鲜 running 行，第二道闸只作纵深防御。
fn inflight_boundary(
    conn: &Connection,
    after: i64,
    now_ms: i64,
) -> Result<Option<i64>, UsageError> {
    let blocked: Option<i64> = conn
        .query_row(
            "SELECT MIN(rowid) FROM model_usage
             WHERE rowid > ?1 AND status='running'
               AND started_at IS NOT NULL AND ?2 - started_at < ?3",
            rusqlite::params![after, now_ms, ZOMBIE_RUNNING_MS],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("tps/zcode: inflight row probe failed (schema mismatch): {e}"),
            )
        })?;
    Ok(blocked)
}

// ==== 行 → 观察 ====

fn stable_call_key(r: &ModelRow) -> String {
    if r.logical_request_id.is_empty() {
        format!("{}:{}", r.session_id, r.rowid)
    } else {
        r.logical_request_id.clone()
    }
}

/// 形状检查 + 观察构建；不合格 → None + 诊断（缺字段返回诊断，不报 0）。
/// 调用前置：running 行已在消费循环被拦下（新鲜=停水位、僵尸=跨过），到不了这里。
fn build_observation(
    r: &ModelRow,
    host: &str,
    tally: &mut std::collections::BTreeMap<String, usize>,
) -> Option<TpsObservation> {
    debug_assert_ne!(r.status, "running");
    // 终态判定
    let outcome = match r.status.as_str() {
        "completed" => Outcome::Success,
        "error" => Outcome::Failed,
        "cancelled" => Outcome::Cancelled,
        other => {
            diag(
                tally,
                &format!("unknown status={other:?} -> skip (no guessing)"),
            );
            return None;
        }
    };
    // 起点缺失：不造 0/1970（D-23 同族）
    let Some(started_at) = r.started_at else {
        diag(tally, "started_at 缺失 → 跳过该行（不造起点）");
        return None;
    };
    let Some(completed_at) = r.completed_at else {
        diag(tally, "终态行缺 completed_at → 跳过该行（不造终点）");
        return None;
    };
    // 模型：绝不落无名行（D-32 同族）
    if r.model_id.is_empty() {
        diag(tally, "缺 model_id → 跳过该行（不落无名行）");
        return None;
    }
    if r.output_tokens.is_none() && outcome == Outcome::Success {
        diag(tally, "output_tokens 缺失 → 记缺失不报 0");
    }
    Some(build_observation_inner(
        r,
        host,
        tally,
        outcome,
        started_at,
        completed_at,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_observation_inner(
    r: &ModelRow,
    host: &str,
    tally: &mut std::collections::BTreeMap<String, usize>,
    outcome: Outcome,
    started_at: i64,
    completed_at: i64,
) -> TpsObservation {
    // 生成窗 = 首点 → completed（Spec §4）；只有成功 + retry=0 + 正窗入速
    let generation_ms = if outcome != Outcome::Success {
        None // 失败/取消不入速度（Spec §3）
    } else if r.retry_count > 0 {
        diag(
            tally,
            &format!(
                "retry_count={} > 0 and internal attempts not alignable -> excluded from speed",
                r.retry_count
            ),
        );
        None
    } else {
        match r.first_token_at {
            None => {
                diag(
                    tally,
                    "completed 行缺 first_token_at → 无生成窗，跳过流式速度，响应窗照入",
                );
                None
            }
            Some(f) => {
                let g = completed_at - f;
                if g > 0 {
                    Some(g)
                } else {
                    diag(tally, "生成窗非正 → 不入速度");
                    None
                }
            }
        }
    };
    // 响应窗（V2 Spec §5.2「含等待响应」）：同调用完整窗 completed − started（行级
    // verified 起止，与行自身 duration_ms 同窗；模型调用行不含后续工具执行）。与生成窗
    // 同一套同调用验证：成功 + retry=0 才填实——失败/取消不入速度，retry>0 的内部
    // 尝试不可对齐（跨尝试的窗不是单次调用）。缺 F 的完整调用 response-only 有效
    //（正负/短于生成窗的不一致由 Task 1 谓词把关，绝不修复）。
    let response_elapsed_ms = if outcome == Outcome::Success && r.retry_count == 0 {
        let d = completed_at - started_at;
        if d > 0 {
            Some(d)
        } else {
            diag(tally, "响应窗非正 → 不入响应速度");
            None
        }
    } else {
        None
    };
    // 输出语义（sqlite-sources.md）：reasoning ⊆ output → >0 即已含思考，不再相加
    let output_scope = if r.reasoning > 0 {
        OutputScope::IncludesReasoning
    } else {
        OutputScope::ExclusiveOutput
    };
    TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: host.into(),
        stable_call_key: stable_call_key(r),
        started_at_ms: started_at,
        first_token_at_ms: r.first_token_at,
        ended_at_ms: completed_at,
        native_duration_ms: r.duration_ms,
        generation_ms,
        // 同一次输出两个窗（Spec §5.1）：生成窗 = 首点 → completed；响应窗 =
        // completed − started（含等待；见上）。
        response_elapsed_ms,
        output_tokens: r.output_tokens,
        output_scope,
        tool_ms: None, // tool_usage 与 model_usage 无原生关联（见模块头），不填 0
        model: r.model_id.clone(),
        channel: None,
        session_key: Some(r.session_id.clone()),
        turn_key: r.turn_id.clone(),
        purpose: purpose_of(&r.task_type),
        outcome,
        method: TimingMethod::ClientNative,
        failure_reason: (outcome != Outcome::Success).then(|| format!("status={}", r.status)),
    }
}

/// 辅助请求标 purpose（证据文档点名的两类；其余默认主任务）
fn purpose_of(task_type: &str) -> Purpose {
    match task_type {
        "session_title" => Purpose::AssistTitle,
        "goal_summary_title" => Purpose::AutoSummary,
        _ => Purpose::MainTask,
    }
}

/// 诊断计数（同原因聚合一条）
fn diag(tally: &mut std::collections::BTreeMap<String, usize>, reason: &str) {
    *tally.entry(reason.to_string()).or_insert(0) += 1;
}

fn render_diag(tally: &std::collections::BTreeMap<String, usize>) -> Vec<String> {
    tally
        .iter()
        .map(|(reason, n)| format!("{SOURCE_ID}: {n}× {reason}"))
        .collect()
}
