//! 用量账本 DAO（计划①）：4 张表的增量 upsert 与区间查询。
//! 全部 `*_conn` 形态（调用方持连接/事务传入，本层不自取全局锁）——与 dao/archive.rs 同款，
//! 让 ledger.rs 能把「一批行 = 一个短事务」捏在手里（写库不得阻塞 3 秒轮询，见 ledger.rs）。
//! **所有聚合列一律 `col = col + excluded.col`**（增量账语义：采集器上报的是增量），
//! **唯一例外是 `user_est`**：它是可空列，「不可得」必须是 NULL 而不是 0，故走
//! `CASE WHEN 两边都 NULL THEN NULL ELSE COALESCE(..,0) + COALESCE(..,0) END`（R1）。
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashMap};

// 全局连接包装（Task 10 追加的 `load_cursors` / `load_sessions`）用：调度与查询层零锁代码。
use crate::database::connection::DB;

// `SourceKind` 必须显式导入：`DailyAggRow.provider_kind` / `DetailAggRow.provider_kind` 两个行类型
// 都用它，`SourceKind::from_db` 也在两处查询里调用——只导 `UsageBuckets` 会 4×E0425/E0433（照抄计划即失败）。
// 同理 `SessionCounters` **不在**本文件作用域里出现（只在 `delta.rs` 的类型定义与测试里用到），
// 计划正文的导入清单把它一并列了进来 → `unused_imports` 告警（门禁 `-D warnings` 下是 error）。
use crate::services::usage::delta::{CursorDelta, DailyDelta, DetailDelta, SessionDimDelta};
use crate::services::usage::model::{SourceKind, UsageBuckets};
// `CacheSemantics` **不需要**导入：写入走 `r.cache_semantics.as_db()`（方法按字段类型解析，
// 名字本身从不在本文件出现），查询侧刻意不读该列（诊断用列，不进任何行类型）→ 计划正文的
// `use ...::semantics::CacheSemantics;` 是 `unused_imports`（门禁 `-D warnings` 下是 error）。

// 游标**没有**独立的行类型：`load_cursors_conn` 直接返回 `HashMap<String, CursorDelta>`。
// 旧版这里另有一个 `CursorRow`（字段与 `CursorDelta` 逐字相同、但两 struct 无 `From`/别名）——
// 于是 Task 10 的装配行 `collect_source(c, &home, now_ms, &cursors)` 必然 E0308（§3.2.2 阻塞 1）。
// 教训：**同型行只留一个类型**；DAO 的行类型要么就是域类型，要么有一个显式映射函数。
// （回归锁：Task 3 的 `cursor_round_trip_and_session_dim_layering` 逐字段守恒 +
//  Task 10 的 `dao_cursor_map_type_matches_collect_source_signature` 编译期同型。）

#[derive(Debug, Clone, PartialEq)]
pub struct DailyAggRow {
    pub day_key: String,
    pub source_id: String,
    pub project_key: String,
    pub provider: String,
    pub provider_kind: SourceKind,
    pub model: String,
    pub buckets: UsageBuckets,
    pub request_total: i64,
    pub requests: i64,
    /// 用户输入(估)：`None` = 该（日 × 源 × 项目 × 供应商 × 模型）组内全不可得（R1）
    pub user_est: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetailAggRow {
    pub hour_key: String,
    pub day_key: String,
    pub source_id: String,
    pub session_id: String,
    /// **记录级**项目键（D21 规则④）：查询层按项目分组直接用这一列，不回查会话维度表（R2）
    pub project_key: String,
    pub model: String,
    pub provider: String,
    pub provider_kind: SourceKind,
    pub buckets: UsageBuckets,
    pub request_total: i64,
    pub requests: i64,
    /// 用户输入(估)：`None` = 不可得（R1；不得在查询层补 0）
    pub user_est: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CounterAggRow {
    pub source_id: String,
    pub session_id: String,
    pub is_subagent: bool,
    pub turns: i64,
    pub error_model: i64,
    pub error_turn: i64,
    pub error_tool: i64,
    pub interrupted: i64,
    pub tool_calls: i64,
    pub tool_ms: i64,
    pub turn_ms: Vec<i64>,
    pub tool_stats: BTreeMap<String, (i64, i64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionDimRow {
    pub source_id: String,
    pub session_id: String,
    pub project_key: String,
    pub project_label: String,
    /// 记录内 cwd 原文（D21：仅留档，UI 不用）
    pub project_path_raw: String,
    /// realpath 规范化路径（**仅留档**，不作 UI 用途）
    pub project_realpath: String,
    pub title: Option<String>,
    pub is_subagent: bool,
    pub parent_session_id: Option<String>,
    pub originator: Option<String>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

/// 时长样本每行上限：单行样本超限时截断（本机最大单会话 turn 数 347，实际远达不到；
/// 截断只影响 p50 精度，且会 warn 留痕）
pub const TURN_SAMPLE_CAP: usize = 1024;

/// `SourceKind` 三态全集的**枚举清单**（只是把三个变体列全，**不承载任何顺序语义**：
/// 顺序一律由 `SourceKind::strongest` 现算，见 `strongest_case`）。
const ALL_SOURCE_KINDS: [SourceKind; 3] = [
    SourceKind::Measured,
    SourceKind::Inferred,
    SourceKind::Unknown,
];

/// 「两边取最强」的 SQL 片段生成器（`provider_kind` 的 ON CONFLICT 用，`detail` 与 `daily` 共用）。
///
/// **不另造第三套三态顺序**（评审 Important #2 的要求）：分支优先级由 `SourceKind::strongest`
/// **现算**——对每个变体数一遍「strongest 判定胜过自己」的个数即得 rank
/// （measured=2 / inferred=1 / unknown=0），落库串由 `as_db` 给。
/// 将来 `strongest` 改序（或加第四个变体），本片段自动跟随，SQL 一个字都不用动。
fn strongest_case(col: &str, other: &str) -> String {
    let rank_of = |k: SourceKind| {
        ALL_SOURCE_KINDS
            .iter()
            .filter(|o| k.strongest(**o) == k)
            .count()
            - 1
    };
    let mut kinds = ALL_SOURCE_KINDS;
    kinds.sort_by_key(|k| std::cmp::Reverse(rank_of(*k)));
    let arms = kinds
        .iter()
        .map(|k| {
            let v = k.as_db();
            format!("WHEN {col} = '{v}' OR {other} = '{v}' THEN '{v}'")
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("CASE {arms} ELSE '{}' END", SourceKind::Unknown.as_db())
}

/// `provider_kind` 的「取最强」CASE（进程内只生成一次，避免每批重复拼串）。
fn strongest_provider_kind_case() -> &'static str {
    static CASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CASE.get_or_init(|| strongest_case("provider_kind", "excluded.provider_kind"))
}

// ---- 写入 ----

pub fn upsert_detail_conn(conn: &Connection, rows: &[DetailDelta], now_ms: i64) -> usize {
    // 三态合并的 CASE 由 `SourceKind::strongest` 现算（见 `strongest_case`）；整批只拼一次 SQL
    let sql = format!(
        // 列清单是**显式列名绑定**（顺序与物理列序无关，映射按名字不按位置）；
        // 主键/冲突键的六列顺序见下方 ON CONFLICT（与 DDL、契约 §4 同序）
        "INSERT INTO usage_detail
                 (source_id, session_id, hour_key, project_key, day_key, model, provider, provider_kind,
                  input_fresh, cache_read, cache_write, output, request_total, requests,
                  user_est, cache_semantics, turns, error_model, error_turn, error_tool,
                  interrupted, tool_calls, tool_ms, turn_ms, tool_stats,
                  first_seen_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,
                         ?20,?21,?22,?23,?24,?25,?26,?26)
                 -- 冲突键 = 契约 §4 的六列主键（顺序也照抄，便于与 DDL 对照）
                 ON CONFLICT(source_id, session_id, project_key, hour_key, model, provider) DO UPDATE SET
                  -- D8/R1：三态**取最强**（measured > inferred > unknown），后写的弱三态**不得**把已落库的
                  -- 强三态降级——日聚合永久保留、明细按保留期删除，降级是**不可逆**的口径损坏
                  -- （旧正文写 `= excluded.provider_kind`，与 `DailyDelta.provider_kind` 的注释
                  --  「三态取本组内最强」自相矛盾，且会让 `SourceKind::strongest` 永无调用者）
                  provider_kind = {},
                  input_fresh   = input_fresh   + excluded.input_fresh,
                  cache_read    = cache_read    + excluded.cache_read,
                  cache_write   = cache_write   + excluded.cache_write,
                  output        = output        + excluded.output,
                  request_total = request_total + excluded.request_total,
                  requests      = requests      + excluded.requests,
                  -- R1：可空列不得用 `+`（NULL + x = NULL 会把已有值抹掉，COALESCE 两侧又会把
                  -- 「不可得」变成 0）→ 只有「两边都不可得」才保持 NULL，其余情况按 0 补位累加
                  user_est      = CASE
                                    WHEN user_est IS NULL AND excluded.user_est IS NULL THEN NULL
                                    ELSE COALESCE(user_est, 0) + COALESCE(excluded.user_est, 0)
                                  END,
                  cache_semantics = excluded.cache_semantics,
                  turns       = turns       + excluded.turns,
                  error_model = error_model + excluded.error_model,
                  error_turn  = error_turn  + excluded.error_turn,
                  error_tool  = error_tool  + excluded.error_tool,
                  interrupted = interrupted + excluded.interrupted,
                  tool_calls  = tool_calls  + excluded.tool_calls,
                  tool_ms     = tool_ms     + excluded.tool_ms,
                  turn_ms     = excluded.turn_ms,
                  tool_stats  = excluded.tool_stats,
                  updated_at  = excluded.updated_at",
        strongest_provider_kind_case()
    );
    let mut written = 0usize;
    for r in rows {
        let (turn_ms, tool_stats) = merge_samples(conn, r);
        match conn.execute(
            &sql,
            params![
                r.source_id.as_str(),
                r.key.session_id.as_str(),
                r.key.hour_key.as_str(),
                r.key.project_key.as_str(),
                r.key.day_key.as_str(),
                r.key.model.as_str(),
                r.key.provider.as_str(),
                r.provider_kind.as_db(),
                r.buckets.input_fresh,
                r.buckets.cache_read,
                r.buckets.cache_write,
                r.buckets.output,
                r.request_total,
                r.requests,
                // R1：`Option<i64>` 直传（None → SQL NULL）；**不得** `unwrap_or(0)`
                r.user_est,
                r.cache_semantics.as_db(),
                r.counters.turns,
                r.counters.error_model,
                r.counters.error_turn,
                r.counters.error_tool,
                r.counters.interrupted,
                r.counters.tool_calls,
                r.counters.tool_ms,
                turn_ms,
                tool_stats,
                r.ts_ms.max(now_ms)
            ],
        ) {
            Ok(_) => written += 1,
            // 评审 Important #1：写路径**不得**吞错。原写法 `.is_ok()` 把「正常写入」与「SQL/数据出错」
            // 混同（列名写错的表现会是「少写几行」且零痕迹）。保持 `-> usize` 冻结签名不变，
            // 只在失败时留一条 warn（返回值语义与调用方 Task 4/10/18 依赖的形状都不变）。
            Err(e) => log::warn!(
                "usage: 明细行写入失败（源 {} 会话 {} 小时 {} 项目 {} 模型 {} 供应商 {}）：{e}",
                r.source_id,
                r.key.session_id,
                r.key.hour_key,
                r.key.project_key,
                r.key.model,
                r.key.provider
            ),
        }
    }
    written
}

/// 时长样本合并：旧值 + 本批 → 换行分隔文本 / JSON 对象文本（截断到上限）。
/// WHERE 与 upsert 的冲突键**逐列同序**（契约 §4 的六列主键，含 project_key）：
/// 少了 project_key 就会跨项目串样本，顺序抄错则等于换了列——两处都与 DDL 的 PRIMARY KEY 对照着写。
fn merge_samples(conn: &Connection, r: &DetailDelta) -> (String, String) {
    // 评审 Important #1：`ok()` 会把「本行还没有旧值」（首次写入，正常）与「SQL 出错」混同。
    // 后者意味着本批会把该行的时长样本**当成空旧值重写**（旧样本被覆盖）→ 必须留痕，不能只靠 `.ok()`。
    let old: Option<(String, String)> = match conn.query_row(
        // WHERE 与 upsert 的冲突键**逐列同序**（契约 §4 的六列主键）
        "SELECT turn_ms, tool_stats FROM usage_detail
             WHERE source_id=?1 AND session_id=?2 AND project_key=?3 AND hour_key=?4
               AND model=?5 AND provider=?6",
        params![
            r.source_id,
            r.key.session_id,
            r.key.project_key,
            r.key.hour_key,
            r.key.model,
            r.key.provider
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(v) => Some(v),
        // 首次写入该行：没有旧样本，属正常路径（不打日志）
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            log::warn!(
                "usage: 读取明细行时长样本失败（源 {} 会话 {} 小时 {} 项目 {} 模型 {} 供应商 {}）：{e}；\
                 本批将按「无旧样本」继续，该行旧 turn_ms/tool_stats 可能被覆盖",
                r.source_id,
                r.key.session_id,
                r.key.hour_key,
                r.key.project_key,
                r.key.model,
                r.key.provider
            );
            None
        }
    };
    let (old_turn, old_tools) = old.unwrap_or_default();
    let mut samples: Vec<i64> = old_turn
        .split('\n')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();
    samples.extend(r.counters.turn_ms.iter().copied());
    if samples.len() > TURN_SAMPLE_CAP {
        log::warn!(
            "usage: 会话 {} 的 turn 时长样本超上限 {}，截断（p50 精度受影响）",
            r.key.session_id,
            TURN_SAMPLE_CAP
        );
        samples.truncate(TURN_SAMPLE_CAP);
    }
    let mut tools: BTreeMap<String, (i64, i64)> = parse_tool_stats(&old_tools);
    for (name, (calls, ms)) in &r.counters.tool_stats {
        let e = tools.entry(name.clone()).or_insert((0, 0));
        e.0 += calls;
        e.1 += ms;
    }
    let turn_text = samples
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    (
        turn_text,
        serde_json::to_string(&tools).unwrap_or_else(|_| "{}".into()),
    )
}

fn parse_tool_stats(s: &str) -> BTreeMap<String, (i64, i64)> {
    // 空串 = 「还没有任何统计」，属正常路径（不打日志）
    if s.trim().is_empty() {
        return BTreeMap::new();
    }
    // 评审 Important #1：非空却解析失败 = **数据损坏或列被写坏**（`unwrap_or_default()` 会把它
    // 静默降级成「没有工具调用」，与真的没有调用于是无法区分）→ 留痕后仍返回空表（保持原返回语义）。
    // 只记错误与长度，**不落文本内容**（GC 10：账本与日志都不得出现原文）。
    match serde_json::from_str::<BTreeMap<String, (i64, i64)>>(s) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "usage: tool_stats JSON 解析失败（{} 字节）：{e}；本处按空统计继续",
                s.len()
            );
            BTreeMap::new()
        }
    }
}

pub fn upsert_daily_conn(conn: &Connection, rows: &[DailyDelta], now_ms: i64) -> usize {
    // 与明细表同一个「取最强」CASE（同源生成，见 `strongest_case`）
    let sql = format!(
        "INSERT INTO usage_daily
                 (day_key, source_id, project_key, provider, provider_kind, model,
                  input_fresh, cache_read, cache_write, output, request_total, requests,
                  user_est, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(day_key, source_id, project_key, provider, model) DO UPDATE SET
                  -- D8：三态取最强（日聚合**永久保留**，一旦被后写的弱三态降级就不可逆）
                  provider_kind = {},
                  input_fresh   = input_fresh   + excluded.input_fresh,
                  cache_read    = cache_read    + excluded.cache_read,
                  cache_write   = cache_write   + excluded.cache_write,
                  output        = output        + excluded.output,
                  request_total = request_total + excluded.request_total,
                  requests      = requests      + excluded.requests,
                  -- R1：与明细表同一条 CASE 语义（全不可得才是 NULL）——日档不得恒为 null
                  user_est      = CASE
                                    WHEN user_est IS NULL AND excluded.user_est IS NULL THEN NULL
                                    ELSE COALESCE(user_est, 0) + COALESCE(excluded.user_est, 0)
                                  END,
                  updated_at    = excluded.updated_at",
        strongest_provider_kind_case()
    );
    let mut written = 0usize;
    for r in rows {
        match conn.execute(
            &sql,
            params![
                r.day_key,
                r.source_id,
                r.project_key,
                r.provider,
                r.provider_kind.as_db(),
                r.model,
                r.buckets.input_fresh,
                r.buckets.cache_read,
                r.buckets.cache_write,
                r.buckets.output,
                r.request_total,
                r.requests,
                r.user_est,
                now_ms
            ],
        ) {
            Ok(_) => written += 1,
            // 评审 Important #1：原 `.is_ok()` 吞错 → 改为留痕 + 保持返回值语义
            Err(e) => log::warn!(
                "usage: 日聚合行写入失败（日 {} 源 {} 项目 {} 供应商 {} 模型 {}）：{e}",
                r.day_key,
                r.source_id,
                r.project_key,
                r.provider,
                r.model
            ),
        }
    }
    written
}

/// 会话维度 upsert（**四参**：`source_id` 由调用方给——`SessionDimDelta` 里没有它，
/// 见 Interfaces 的说明）。`usage_session` 的主键是 `(source_id, session_id)`，
/// 所以"这批会话登记到哪个源"必须显式传，不能靠行内字段猜
/// （旧版写的是 `r.source_id` → 该字段在 `SessionDimDelta` 上不存在，E0609）；
/// 调用方 `ledger::write_batch(tx, b, source_id, now_ms)` 手里正好有 `sid`。
pub fn upsert_session_conn(
    conn: &Connection,
    source_id: &str,
    rows: &[SessionDimDelta],
    now_ms: i64,
) -> usize {
    let mut written = 0usize;
    for r in rows {
        // last_seen_at 取 max（乱序批次不得把活跃时间改小）；first_seen_at 只在插入时写
        let seen = if r.last_seen_at > 0 {
            r.last_seen_at
        } else {
            now_ms
        };
        match conn.execute(
            "INSERT INTO usage_session
                 (source_id, session_id, project_key, project_label, project_path_raw,
                  project_realpath, title, is_subagent, parent_session_id, originator,
                  first_seen_at, last_seen_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                 ON CONFLICT(source_id, session_id) DO UPDATE SET
                  -- R2：本表的项目列是**会话级近似**（该会话首个**带 cwd** 的记录），
                  -- 记录级归属在 usage_detail/usage_daily 的 project_key 上；空 cwd 的记录不得覆盖它。
                  -- （SQLite 的 SET 表达式一律读旧行值，四个 CASE 之间不会互相污染）
                  project_key       = CASE WHEN project_path_raw = '' AND excluded.project_path_raw <> ''
                                           THEN excluded.project_key ELSE project_key END,
                  project_label     = CASE WHEN project_path_raw = '' AND excluded.project_path_raw <> ''
                                           THEN excluded.project_label ELSE project_label END,
                  project_realpath  = CASE WHEN project_path_raw = '' AND excluded.project_path_raw <> ''
                                           THEN excluded.project_realpath ELSE project_realpath END,
                  project_path_raw  = CASE WHEN project_path_raw = ''
                                           THEN excluded.project_path_raw ELSE project_path_raw END,
                  title = COALESCE(excluded.title, title),
                  -- 一旦判定为子代理就保持（逐行标记的源不得因后一行非 sidechain 而回退）
                  is_subagent = MAX(is_subagent, excluded.is_subagent),
                  parent_session_id = COALESCE(excluded.parent_session_id, parent_session_id),
                  originator = COALESCE(excluded.originator, originator),
                  last_seen_at = MAX(last_seen_at, excluded.last_seen_at)",
            params![
                source_id, r.session_id, r.project_key, r.project_label,
                r.project_path_raw, r.project_realpath, r.title,
                r.is_subagent as i64, r.parent_session_id, r.originator, seen
            ],
        ) {
            Ok(_) => written += 1,
            // 评审 Important #1：原 `.is_ok()` 吞错 → 留痕 + 保持返回值语义
            Err(e) => log::warn!(
                "usage: 会话维度行写入失败（源 {source_id} 会话 {}）：{e}",
                r.session_id
            ),
        }
    }
    written
}

/// 游标 upsert（**四参**：`source_id` 由调用方给——`CursorDelta` 里没有它，见 Interfaces 的说明）。
/// `usage_cursor` 的主键是 `(source_id, session_id=游标键)`，所以"写给哪个源"必须显式传，
/// 不能靠行内字段猜（旧版写的是 `r.source_id` → 该字段在 `CursorDelta` 上不存在，E0609）。
pub fn save_cursors_conn(
    conn: &Connection,
    source_id: &str,
    rows: &[CursorDelta],
    now_ms: i64,
) -> usize {
    let mut written = 0usize;
    for r in rows {
        match conn.execute(
            "INSERT INTO usage_cursor
                 (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
                  mtime_ms, file_size, state_json, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(source_id, session_id) DO UPDATE SET
                  fingerprint = excluded.fingerprint,
                  byte_offset = excluded.byte_offset,
                  ordinal = excluded.ordinal,
                  last_cumulative = excluded.last_cumulative,
                  mtime_ms = excluded.mtime_ms,
                  file_size = excluded.file_size,
                  state_json = excluded.state_json,
                  updated_at = excluded.updated_at",
            params![
                source_id,
                r.session_id,
                r.fingerprint,
                r.byte_offset,
                r.ordinal,
                r.last_cumulative,
                r.mtime_ms,
                r.file_size,
                r.state_json,
                now_ms
            ],
        ) {
            Ok(_) => written += 1,
            // 评审 Important #1：原 `.is_ok()` 吞错。**游标写失败尤其危险**：水位没推进而调用方
            // 以为推进了 → 要么丢数据要么每轮全量重扫，且零痕迹。留痕是这里唯一的可观测手段。
            Err(e) => log::warn!(
                "usage: 游标写入失败（源 {source_id} 会话 {}）：{e}；水位未推进，下轮可能重扫该文件",
                r.session_id
            ),
        }
    }
    written
}

// ---- 读取 ----

/// 读回本源的游标表（键 = 游标键/文件标识）。返回类型就是 `CursorDelta`
/// ——**与 `collect_source` 的第 4 参同型**（§3.2.2 阻塞 1：不再有第二个 `CursorRow`）。
pub fn load_cursors_conn(conn: &Connection, source_id: &str) -> HashMap<String, CursorDelta> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT session_id, fingerprint, byte_offset, ordinal, last_cumulative, mtime_ms,
                file_size, state_json
         FROM usage_cursor WHERE source_id = ?1",
    ) else {
        return HashMap::new();
    };
    stmt.query_map([source_id], |row| {
        Ok(CursorDelta {
            session_id: row.get(0)?,
            fingerprint: row.get(1)?,
            byte_offset: row.get(2)?,
            ordinal: row.get(3)?,
            last_cumulative: row.get(4)?,
            mtime_ms: row.get(5)?,
            file_size: row.get(6)?,
            state_json: row.get(7)?,
        })
    })
    .map(|rows| {
        rows.filter_map(Result::ok)
            .map(|r| (r.session_id.clone(), r))
            .collect()
    })
    .unwrap_or_default()
}

pub fn query_daily_conn(conn: &Connection, from_day: &str, to_day: &str) -> Vec<DailyAggRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT day_key, source_id, project_key, provider, provider_kind, model, input_fresh,
                cache_read, cache_write, output, request_total, requests, user_est
         FROM usage_daily WHERE day_key BETWEEN ?1 AND ?2",
    ) else {
        return Vec::new();
    };
    stmt.query_map([from_day, to_day], |row| {
        Ok(DailyAggRow {
            day_key: row.get(0)?,
            source_id: row.get(1)?,
            project_key: row.get(2)?,
            provider: row.get(3)?,
            provider_kind: SourceKind::from_db(&row.get::<_, String>(4)?),
            model: row.get(5)?,
            buckets: UsageBuckets {
                input_fresh: row.get(6)?,
                cache_read: row.get(7)?,
                cache_write: row.get(8)?,
                output: row.get(9)?,
            },
            request_total: row.get(10)?,
            requests: row.get(11)?,
            // R1：可空读回 Option（`row.get::<_, i64>` 会把 NULL 变成 Err → 整行被丢）
            user_est: row.get(12)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

pub fn query_detail_conn(conn: &Connection, from_hour: &str, to_hour: &str) -> Vec<DetailAggRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT hour_key, day_key, source_id, session_id, project_key, model, provider, provider_kind,
                input_fresh, cache_read, cache_write, output, request_total, requests, user_est
         FROM usage_detail WHERE hour_key BETWEEN ?1 AND ?2",
    ) else {
        return Vec::new();
    };
    stmt.query_map([from_hour, to_hour], |row| {
        Ok(DetailAggRow {
            hour_key: row.get(0)?,
            day_key: row.get(1)?,
            source_id: row.get(2)?,
            session_id: row.get(3)?,
            // R2：记录级项目键随行读回（查询层按项目分组直接用它）
            project_key: row.get(4)?,
            model: row.get(5)?,
            provider: row.get(6)?,
            provider_kind: SourceKind::from_db(&row.get::<_, String>(7)?),
            buckets: UsageBuckets {
                input_fresh: row.get(8)?,
                cache_read: row.get(9)?,
                cache_write: row.get(10)?,
                output: row.get(11)?,
            },
            request_total: row.get(12)?,
            requests: row.get(13)?,
            // R1：可空读回 Option（不是 0）
            user_est: row.get(14)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// 计数类聚合（工作小结区）：**按 (源, 会话) 分组**，is_subagent 由会话维度表带出。
/// D17「计数类分层」= 会话数与 **turn 数都只计父会话**：过滤在调用方 `work_summary_with`
/// （turns_per_tool 求和前剔 `is_subagent`）——本函数保留全部会话，好让 token 口径不受影响。
///
/// **A-4 诊断（不改变行为）**：调用前先跑一次**只读**的孤儿明细行计数（`warn_orphan_details_conn`）
/// —— `COALESCE(s.is_subagent, 0)` 的语义与返回值**一字未动**，只是把「会话维度没跟上明细」
/// 变成可观测的告警数字。
pub fn query_counters_conn(
    conn: &Connection,
    from_hour: &str,
    to_hour: &str,
) -> Vec<CounterAggRow> {
    warn_orphan_details_conn(conn, from_hour, to_hour);
    let Ok(mut stmt) = conn.prepare(
        "SELECT d.source_id, d.session_id, COALESCE(s.is_subagent, 0),
                SUM(d.turns), SUM(d.error_model), SUM(d.error_turn), SUM(d.error_tool),
                SUM(d.interrupted), SUM(d.tool_calls), SUM(d.tool_ms),
                GROUP_CONCAT(d.turn_ms, char(10)), GROUP_CONCAT(d.tool_stats, char(10))
         FROM usage_detail d
         LEFT JOIN usage_session s ON s.source_id = d.source_id AND s.session_id = d.session_id
         WHERE d.hour_key BETWEEN ?1 AND ?2
         GROUP BY d.source_id, d.session_id",
    ) else {
        return Vec::new();
    };
    stmt.query_map([from_hour, to_hour], |row| {
        let turn_text: Option<String> = row.get(10)?;
        let stats_text: Option<String> = row.get(11)?;
        Ok(CounterAggRow {
            source_id: row.get(0)?,
            session_id: row.get(1)?,
            is_subagent: row.get::<_, i64>(2)? != 0,
            turns: row.get(3)?,
            error_model: row.get(4)?,
            error_turn: row.get(5)?,
            error_tool: row.get(6)?,
            interrupted: row.get(7)?,
            tool_calls: row.get(8)?,
            tool_ms: row.get(9)?,
            turn_ms: turn_text
                .unwrap_or_default()
                .split('\n')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect(),
            tool_stats: merge_stats_text(stats_text.as_deref().unwrap_or("")),
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// **A-4 诊断计数（只读、只告警、不改任何返回值）**：本轮窗口内「孤儿明细行」条数 ——
/// 明细行在 `usage_session` 里查不到对应 (源, 会话) 的那些行。
///
/// 为什么值得一条告警：这类行会被 `COALESCE(s.is_subagent, 0)` **按父会话计**（保守方向，
/// 用户裁决保持不变），于是 D17 的分层计数会**少算子代理**却**不报任何错**——用户看到的是
/// 「子代理数偏少」，而不是「会话维度没跟上」。>0 即说明台账的两半不同步
/// （会话登记失败 / 被保留期清理 / 明细先行落库），是本层唯一的探针。
fn warn_orphan_details_conn(conn: &Connection, from_hour: &str, to_hour: &str) {
    const SQL: &str = "SELECT COUNT(*)
         FROM usage_detail d
         LEFT JOIN usage_session s ON s.source_id = d.source_id AND s.session_id = d.session_id
         WHERE d.hour_key BETWEEN ?1 AND ?2 AND s.session_id IS NULL";
    match conn.query_row(SQL, [from_hour, to_hour], |r| r.get::<_, i64>(0)) {
        Ok(0) => {}
        Ok(n) => log::warn!(
            "usage/dao: 窗口 {from_hour}..{to_hour} 内有 {n} 条孤儿明细行（(源, 会话) 在 \
             usage_session 里查不到）→ COALESCE(s.is_subagent,0) 按父会话计（保守方向，行为不变，\
             A-4 诊断计数）；子代理分层可能因此偏少"
        ),
        // 诊断本身失败**不得**影响主查询（也不得静默）：主查询照跑，这里留一条痕
        Err(e) => log::warn!("usage/dao: 孤儿明细诊断查询失败（不影响主查询结果）：{e}"),
    }
}

fn merge_stats_text(text: &str) -> BTreeMap<String, (i64, i64)> {
    let mut out: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    // GROUP_CONCAT 把每行的 JSON 对象用换行连起来 → 逐行 parse 后合并
    for line in text.split('\n').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        for (k, v) in parse_tool_stats(line) {
            let e = out.entry(k).or_insert((0, 0));
            e.0 += v.0;
            e.1 += v.1;
        }
    }
    out
}

pub fn load_sessions_conn(conn: &Connection, source_id: Option<&str>) -> Vec<SessionDimRow> {
    let cols = "SELECT source_id, session_id, project_key, project_label, project_path_raw,
                       project_realpath, title, is_subagent, parent_session_id, originator,
                       first_seen_at, last_seen_at FROM usage_session";
    let sql = match source_id {
        Some(_) => format!("{} WHERE source_id = ?1", cols),
        None => cols.to_string(),
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SessionDimRow> {
        Ok(SessionDimRow {
            source_id: row.get(0)?,
            session_id: row.get(1)?,
            project_key: row.get(2)?,
            project_label: row.get(3)?,
            project_path_raw: row.get(4)?,
            project_realpath: row.get(5)?,
            title: row.get(6)?,
            is_subagent: row.get::<_, i64>(7)? != 0,
            parent_session_id: row.get(8)?,
            originator: row.get(9)?,
            first_seen_at: row.get(10)?,
            last_seen_at: row.get(11)?,
        })
    };
    let rows = match source_id {
        Some(s) => stmt.query_map([s], map),
        None => stmt.query_map([], map),
    };
    rows.map(|r| r.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// 保留期清理：**只删明细**（日聚合在写入时同事务维护，此处绝不重算、绝不叠加）
pub fn purge_detail_before_conn(conn: &Connection, cutoff_day: &str) -> usize {
    match conn.execute("DELETE FROM usage_detail WHERE day_key < ?1", [cutoff_day]) {
        Ok(n) => n,
        // 评审 Important #1：原 `.unwrap_or(0)` 会让「清理失败」长得和「没有需要清理的行」一样
        // （调用方 Task 4 的 purge_expired 会把它当成「删了 0 行」正常返回）→ 留痕 + 保持返回值语义
        Err(e) => {
            log::warn!("usage: 保留期清理失败（cutoff_day={cutoff_day}）：{e}");
            0
        }
    }
}

pub fn count_rows_conn(conn: &Connection, table: &str) -> i64 {
    // 表名来自 crate 内常量（非用户输入），无注入面
    conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
        .unwrap_or(0)
}

// ---- 全局连接包装（调度与查询层用；Task 10 追加） ----

/// 全局连接包装（调度/查询层零锁代码；采集落库**不走这里**——它必须走 ledger 的分批事务）
/// 返回类型 = `HashMap<String, CursorDelta>`：**与 `collect_source` 的第 4 参同型**，
/// 调度侧零转换直通（独立验证复核 §3.2.2 阻塞 1）。
/// 锁中毒 → 空表。**但绝不静默**（评审 M-5）：空游标表 = 整源全量重读，而本域是**增量累加**账
/// （DAO 的 upsert 一律 `col = col + excluded.col`）→ 重复计入。这一族是 W-05/D-23 点名的
/// 「把失败伪装成合法值」，至少必须留下痕迹。
pub fn load_cursors(source_id: &str) -> HashMap<String, CursorDelta> {
    match DB.lock() {
        Ok(conn) => load_cursors_conn(&conn, source_id),
        Err(_) => {
            log::warn!(
                "usage: DB 锁中毒，源 {source_id} 本轮读不到游标（将回退全量重读，增量账可能重复计入）"
            );
            HashMap::new()
        }
    }
}

pub fn load_sessions(source_id: Option<&str>) -> Vec<SessionDimRow> {
    match DB.lock() {
        Ok(conn) => load_sessions_conn(&conn, source_id),
        Err(_) => {
            // 同 M-5：空表会把「读不到」伪装成「没有会话」——查询侧看到的是空态而不是故障。
            log::warn!("usage: DB 锁中毒，会话维度表本轮读不到（查询侧会显示空态）");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::delta::{
        CursorDelta, DailyDelta, DetailDelta, DetailKey, SessionCounters, SessionDimDelta,
    };
    use crate::services::usage::UsageBuckets;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    fn detail(hour: &str, model: &str, input: i64) -> DetailDelta {
        DetailDelta {
            source_id: "claude".into(),
            provider_kind: crate::services::usage::model::SourceKind::Inferred,
            key: DetailKey {
                session_id: "s1".into(),
                hour_key: hour.into(),
                day_key: hour[..10].to_string(),
                // D21/R2：项目键在**明细行键**里（记录级，进明细表主键）——一个文件可跨多个项目，
                // 它是行身份的一部分，不能只在日聚合折叠时才用（否则同会话两个 cwd 会折叠成一行）
                project_key: "proj".into(),
                model: model.into(),
                provider: "p1".into(),
            },
            buckets: UsageBuckets {
                input_fresh: input,
                ..Default::default()
            },
            request_total: input,
            requests: 1,
            user_est: Some(3),
            cache_semantics: crate::services::usage::semantics::CacheSemantics::Exclusive,
            counters: SessionCounters::default(),
            ts_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn detail_upsert_is_additive_and_keyed_by_hour() {
        let conn = mem();
        upsert_detail_conn(&conn, &[detail("2026-10-03T09", "m1", 100)], 1);
        // 同一行再次增量 → 累加（增量账语义），不是覆盖
        upsert_detail_conn(&conn, &[detail("2026-10-03T09", "m1", 50)], 2);
        // 不同小时 → 独立行
        upsert_detail_conn(&conn, &[detail("2026-10-03T10", "m1", 7)], 3);
        let rows = query_detail_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        assert_eq!(rows.len(), 2);
        let nine = rows.iter().find(|r| r.hour_key == "2026-10-03T09").unwrap();
        assert_eq!(nine.buckets.input_fresh, 150, "增量必须累加");
        assert_eq!(nine.day_key, "2026-10-03", "day_key 由 upsert 侧派生");
        assert_eq!(
            nine.project_key, "proj",
            "R2：记录级项目键必须随明细行落库并读回"
        );
        assert_eq!(nine.user_est, Some(6), "有值累加");
    }

    /// R2：同一会话同一小时里的**两个 cwd** 必须是两行、各自归属自己的 projectKey
    /// （模拟 claude 那种一个 jsonl 内出现两个 cwd 的实例）。
    /// 日聚合按同一记录级键折叠的断言在 Task 4 `daily_rows_fold_by_project`（本任务还没有 ledger.rs）。
    #[test]
    fn two_project_keys_in_one_session_hour_stay_separate() {
        let conn = mem();
        let mut a = detail("2026-10-03T09", "m1", 100);
        a.key.project_key = "alpha".into();
        let mut b = detail("2026-10-03T09", "m1", 40);
        b.key.project_key = "beta".into();
        upsert_detail_conn(&conn, &[a, b], 1);
        let rows = query_detail_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        assert_eq!(rows.len(), 2, "两个项目键必须各自成行（不得折叠）");
        let alpha = rows.iter().find(|r| r.project_key == "alpha").unwrap();
        let beta = rows.iter().find(|r| r.project_key == "beta").unwrap();
        assert_eq!(alpha.buckets.input_fresh, 100);
        assert_eq!(beta.buckets.input_fresh, 40);
    }

    /// R1：「拿不到就是 NULL」在 **明细表与日聚合表** 都要成立——
    /// 写入 NULL 读回 NULL（不是 0），有值才累加；两端混写时「全 NULL 才是 NULL」
    #[test]
    fn user_est_null_stays_null_in_detail_and_daily() {
        let conn = mem();
        // zcode 那种「读不到用户文本」的源：user_est = None，写两批
        let mut z1 = detail("2026-10-03T09", "m1", 10);
        z1.source_id = "zcode".into();
        z1.user_est = None;
        let mut z2 = z1.clone();
        z2.buckets.input_fresh = 5;
        upsert_detail_conn(&conn, &[z1.clone(), z2.clone()], 1);
        upsert_detail_conn(&conn, &[z1.clone(), z2], 2);
        // claude 那种有值的源：None 与 Some 混写 → 有值就累加（不是 0，也不是 NULL）。
        // 与上面 zcode 的「写两批」对称，这里同样写**两轮**：期望值 `Some(6) = 3 + 3` 只有在
        // 第二轮才同时验证「后一批的 NULL 行**不得**把已累加的值抹成 NULL」——这正是 CASE 的
        // 另一半语义（不是「碰见 NULL 就 NULL」，而是「两边都 NULL 才 NULL」）。
        // 计划正文此处只写了一轮调用（一轮只能得 3），与它自己的期望值 6 不自洽，
        // 见报告「偏差申报」第 4 条：**期望值一字未改**，补上对称的第二轮。
        let mut c1 = detail("2026-10-03T09", "m1", 1);
        c1.user_est = None;
        upsert_detail_conn(&conn, &[c1.clone(), detail("2026-10-03T09", "m1", 1)], 3);
        upsert_detail_conn(&conn, &[c1, detail("2026-10-03T09", "m1", 1)], 4);
        let rows = query_detail_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        let zcode = rows.iter().find(|r| r.source_id == "zcode").unwrap();
        assert_eq!(
            zcode.user_est, None,
            "R1：不可得的源在明细里必须是 NULL，不是 0"
        );
        let claude = rows.iter().find(|r| r.source_id == "claude").unwrap();
        assert_eq!(
            claude.user_est,
            Some(6),
            "混写时只累加有值的那些（全 NULL 才是 NULL）"
        );
        // 日聚合同规则
        upsert_daily_conn(
            &conn,
            &[
                DailyDelta {
                    day_key: "2026-10-03".into(),
                    source_id: "zcode".into(),
                    project_key: "proj".into(),
                    provider: "p1".into(),
                    provider_kind: crate::services::usage::model::SourceKind::Unknown,
                    model: "m1".into(),
                    buckets: UsageBuckets {
                        input_fresh: 10,
                        ..Default::default()
                    },
                    request_total: 10,
                    requests: 1,
                    user_est: None,
                },
                DailyDelta {
                    day_key: "2026-10-03".into(),
                    source_id: "claude".into(),
                    project_key: "proj".into(),
                    provider: "p1".into(),
                    provider_kind: crate::services::usage::model::SourceKind::Inferred,
                    model: "m1".into(),
                    buckets: UsageBuckets {
                        input_fresh: 1,
                        ..Default::default()
                    },
                    request_total: 1,
                    requests: 1,
                    user_est: Some(7),
                },
            ],
            1,
        );
        let daily = query_daily_conn(&conn, "2026-10-01", "2026-10-31");
        let zd = daily.iter().find(|r| r.source_id == "zcode").unwrap();
        assert_eq!(
            zd.user_est, None,
            "R1：日聚合的不可得源同样是 NULL（不是 0）"
        );
        let cd = daily.iter().find(|r| r.source_id == "claude").unwrap();
        assert_eq!(
            cd.user_est,
            Some(7),
            "R1：日聚合必须出真值（否则日档恒显示「—」）"
        );
    }

    #[test]
    fn daily_upsert_is_additive() {
        let conn = mem();
        let d = DailyDelta {
            day_key: "2026-10-03".into(),
            source_id: "claude".into(),
            project_key: "/p".into(),
            provider: "p1".into(),
            provider_kind: crate::services::usage::model::SourceKind::Inferred,
            model: "m1".into(),
            buckets: UsageBuckets {
                output: 10,
                ..Default::default()
            },
            request_total: 10,
            requests: 1,
            user_est: Some(2),
        };
        // 同一行写两批 → 累加。单元素切片用 `std::slice::from_ref`：计划正文的 `&[d.clone()]`
        // 触发 `clippy::cloned_ref_to_slice_refs`（本计划门禁 `-D warnings` 下是 error）。
        upsert_daily_conn(&conn, std::slice::from_ref(&d), 1);
        upsert_daily_conn(&conn, &[d], 2);
        let rows = query_daily_conn(&conn, "2026-10-01", "2026-10-31");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].buckets.output, 20);
        assert_eq!(rows[0].user_est, Some(4), "R1：日聚合的 userEst 同样累加");
    }

    #[test]
    fn counters_and_duration_samples_round_trip() {
        let conn = mem();
        let mut d = detail("2026-10-03T09", "m1", 1);
        d.counters.turns = 2;
        d.counters.error_tool = 1;
        d.counters.tool_calls = 3;
        d.counters.tool_ms = 120;
        d.counters.turn_ms = vec![1000, 3000];
        d.counters.tool_stats.insert("Bash".into(), (3, 120));
        upsert_detail_conn(&conn, &[d.clone()], 1);
        upsert_detail_conn(&conn, &[d], 2); // 第二批：样本追加
        let c = query_counters_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].turns, 4);
        // **A-4 诊断的行为不变量**：本夹具**没有**写 `usage_session` 行 ⇒ 这就是一条
        // 「孤儿明细行」（诊断会因此告警，但**不得**改变任何返回值）。
        // 前提断言（防假绿）：会话维度表确实为空，否则下面就测不到孤儿分支。
        assert_eq!(
            load_sessions_conn(&conn, None).len(),
            0,
            "前提：本夹具是孤儿明细（无会话维度行）"
        );
        assert!(
            !c[0].is_subagent,
            "孤儿明细按**父会话**计（`COALESCE(s.is_subagent,0)` 的保守方向，A-4 行为不变）"
        );
        assert_eq!(
            c[0].turn_ms,
            vec![1000, 3000, 1000, 3000],
            "时长样本必须追加"
        );
        assert_eq!(c[0].tool_stats.get("Bash").copied(), Some((6, 240)));
    }

    /// **游标 8 字段守恒（独立验证复核 §3.2.2 阻塞 1 的回归锁）**：`usage_cursor` 的每一列都必须
    /// 原样往返——**含 `fingerprint` / `mtime_ms` / `session_id`**（旧断言只点了 5 个字段，漏掉的
    /// 两个恰好是增量快速路 `file_unchanged()` 的两维输入）。DAO 的行类型与域类型**同一个**
    /// `CursorDelta`（没有平行 struct、没有映射函数），所以这里再用整结构相等兜一层：
    /// 将来谁再加一个平行类型或漏映射一列，这条立刻红。
    #[test]
    fn cursor_round_trip_and_session_dim_layering() {
        let conn = mem();
        let written = CursorDelta {
            session_id: "s1".into(),
            fingerprint: "ab".into(),
            byte_offset: 4096,
            ordinal: 42,
            last_cumulative: Some(999),
            mtime_ms: 123,
            file_size: 8192,
            state_json: "{\"model\":\"m1\"}".into(),
        };
        assert_eq!(
            save_cursors_conn(&conn, "codex", std::slice::from_ref(&written), 7),
            1,
            "写一条游标"
        );
        let m: std::collections::HashMap<String, CursorDelta> = load_cursors_conn(&conn, "codex");
        assert_eq!(m.len(), 1);
        let got = m.get("s1").expect("键（游标键/session_id）必须原样读回");
        // ① 逐字段点名（失败信息能直接指认丢的是哪一列）
        assert_eq!(got.session_id, "s1");
        assert_eq!(
            got.fingerprint, "ab",
            "尾指纹是 file_unchanged() 的判据，必须守恒"
        );
        assert_eq!(got.byte_offset, 4096);
        assert_eq!(got.ordinal, 42);
        assert_eq!(got.last_cumulative, Some(999));
        assert_eq!(got.mtime_ms, 123, "mtime 是增量快速路的第一维，必须守恒");
        assert_eq!(got.file_size, 8192);
        assert_eq!(got.state_json, "{\"model\":\"m1\"}", "续读状态必须往返");
        // ② 整结构相等 = 8/8 全守恒（新增列却忘了改 SELECT/映射时在这里红）
        assert_eq!(got, &written, "8 个字段必须一一守恒");
        // ③ 键必须按 source_id 隔离（写给 codex 的游标不得出现在别的源名下）
        assert!(
            load_cursors_conn(&conn, "claude").is_empty(),
            "游标按源隔离"
        );
        // 会话维度：子代理分层字段（**四参**：源由调用方给——`SessionDimDelta` 里没有 `source_id`）
        upsert_session_conn(
            &conn,
            "codex",
            &[SessionDimDelta {
                session_id: "sub1".into(),
                project_key: "proj".into(),
                project_label: "Proj".into(),
                project_path_raw: "/Users/x/Proj".into(),
                project_realpath: "/users/x/proj".into(),
                title: Some("t".into()),
                is_subagent: true,
                parent_session_id: Some("s1".into()),
                originator: Some("codex-tui".into()),
                last_seen_at: 5,
            }],
            5,
        );
        let subs = load_sessions_conn(&conn, Some("codex"));
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].source_id, "codex");
        assert!(subs[0].is_subagent);
        assert_eq!(subs[0].parent_session_id.as_deref(), Some("s1"));
        assert_eq!(subs[0].originator.as_deref(), Some("codex-tui"));
        assert_eq!(subs[0].project_key, "proj", "D21：键 = projectName 小写");
        assert_eq!(
            subs[0].project_label, "Proj",
            "D21：显示名 = projectName 原文"
        );
        assert_eq!(subs[0].project_path_raw, "/Users/x/Proj");
        assert_eq!(
            subs[0].project_realpath, "/users/x/proj",
            "realpath 仅留档，不作 UI 用途"
        );
        assert_eq!(subs[0].first_seen_at, 5, "首次登记 first_seen_at=now");
    }

    /// 裁决 A 的回归锁（评审 Important #2）：`provider_kind` 必须按**最强**合并——
    /// 后写的弱三态**不得**把已落库的强三态降级（日聚合永久保留，降级不可逆）。
    /// `usage_detail` 与 `usage_daily` 两张表都**真的写两遍**（第二遍必走 ON CONFLICT），
    /// 期望值由 `SourceKind::strongest` **现算**——本用例不另列一套三态顺序。
    #[test]
    fn provider_kind_conflict_takes_strongest_on_both_tables() {
        use crate::services::usage::model::SourceKind;
        let kinds = [
            SourceKind::Measured,
            SourceKind::Inferred,
            SourceKind::Unknown,
        ];
        let conn = mem();
        for (i, first) in kinds.iter().enumerate() {
            for (j, second) in kinds.iter().enumerate() {
                let pk = format!("d{i}{j}");
                // 明细：同一 (源,会话,项目,小时,模型,供应商) 键写两轮，第二轮三态更弱或相同
                let mut r1 = detail("2026-10-03T09", "m1", 1);
                r1.provider_kind = *first;
                r1.key.project_key = pk.clone();
                let mut r2 = r1.clone();
                r2.provider_kind = *second;
                assert_eq!(
                    upsert_detail_conn(&conn, std::slice::from_ref(&r1), 1),
                    1,
                    "第一轮 INSERT"
                );
                assert_eq!(upsert_detail_conn(&conn, &[r2], 2), 1, "第二轮 ON CONFLICT");
                // 日聚合：同一 (day, source, project, provider, model) 键写两轮
                let d1 = DailyDelta {
                    day_key: "2026-10-03".into(),
                    source_id: "claude".into(),
                    project_key: pk.clone(),
                    provider: "p1".into(),
                    provider_kind: *first,
                    model: "m1".into(),
                    buckets: UsageBuckets {
                        output: 1,
                        ..Default::default()
                    },
                    request_total: 1,
                    requests: 1,
                    user_est: Some(1),
                };
                let mut d2 = d1.clone();
                d2.provider_kind = *second;
                assert_eq!(
                    upsert_daily_conn(&conn, std::slice::from_ref(&d1), 1),
                    1,
                    "第一轮 INSERT"
                );
                assert_eq!(upsert_daily_conn(&conn, &[d2], 2), 1, "第二轮 ON CONFLICT");
            }
        }
        let rows = query_detail_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        let daily = query_daily_conn(&conn, "2026-10-03", "2026-10-03");
        // 前提断言：9 个有序对真的各落了一行（否则下面的 find 会 panic 或退化成空对空的假绿）
        assert_eq!(rows.len(), 9, "明细：9 个有序对各一行");
        assert_eq!(daily.len(), 9, "日聚合：9 个有序对各一行");
        for (i, first) in kinds.iter().enumerate() {
            for (j, second) in kinds.iter().enumerate() {
                let pk = format!("d{i}{j}");
                let want = first.strongest(*second);
                let d = rows
                    .iter()
                    .find(|r| r.project_key == pk)
                    .expect("明细行存在");
                assert_eq!(
                    d.provider_kind, want,
                    "明细：先 {first:?} 后 {second:?} 必须留最强"
                );
                let a = daily
                    .iter()
                    .find(|r| r.project_key == pk)
                    .expect("日聚合行存在");
                assert_eq!(
                    a.provider_kind, want,
                    "日聚合：先 {first:?} 后 {second:?} 必须留最强"
                );
                // 顺带锁住「改三态不能带坏累加」（两轮各 1）
                assert_eq!(a.buckets.output, 2, "两轮累加");
                assert_eq!(a.requests, 2);
            }
        }
    }

    /// 裁决 C 的另一半：`usage_session` 的 ON CONFLICT 四 CASE 与 `is_subagent`/`last_seen_at`
    /// 的 MAX 也必须**真的走两次以上冲突路径**才算被覆盖（原有三处都只在单次写入后读回）。
    /// 语义（计划正文注释）：会话级项目列取「首个**带 cwd** 的记录」；一旦判定为子代理就保持；
    /// `last_seen_at` 取 max（乱序批次不得把活跃时间改小）；`first_seen_at` 只在插入时写。
    #[test]
    fn session_dim_conflict_never_downgrades_cwd_subagent_or_last_seen() {
        let conn = mem();
        let mk = |path_raw: &str, key: &str, label: &str, sub: bool, seen: i64| SessionDimDelta {
            session_id: "s1".into(),
            project_key: key.into(),
            project_label: label.into(),
            project_path_raw: path_raw.into(),
            project_realpath: if path_raw.is_empty() {
                String::new()
            } else {
                "/users/x/proj".into()
            },
            title: None,
            is_subagent: sub,
            parent_session_id: None,
            originator: None,
            last_seen_at: seen,
        };
        // ① 首条是「无 cwd」的近似行（workbuddy 那种空串），非子代理，last_seen=10
        let bare = mk("", "unknown", "Unknown", false, 10);
        assert_eq!(
            upsert_session_conn(&conn, "codex", std::slice::from_ref(&bare), 10),
            1
        );
        // ② 第二条带 cwd + 子代理血缘 + 更大 last_seen → 会话级项目列被「提升」到带 cwd 的那一份
        let mut rich = mk("/Users/x/Proj", "proj", "Proj", true, 30);
        rich.title = Some("t".into());
        rich.parent_session_id = Some("p1".into());
        rich.originator = Some("codex-tui".into());
        assert_eq!(
            upsert_session_conn(&conn, "codex", std::slice::from_ref(&rich), 30),
            1
        );
        // ③ 第三条又是空 cwd + 非子代理 + 更小 last_seen（now_ms 也更大）→ 一切**不得回退**
        let late_bare = mk("", "unknown", "Unknown", false, 5);
        assert_eq!(
            upsert_session_conn(&conn, "codex", std::slice::from_ref(&late_bare), 99),
            1
        );
        let rows = load_sessions_conn(&conn, Some("codex"));
        assert_eq!(rows.len(), 1, "同一 (源,会话) 只应有一行");
        let r = &rows[0];
        assert_eq!(
            r.project_key, "proj",
            "空 cwd 的后写行不得覆盖带 cwd 的会话级近似"
        );
        assert_eq!(r.project_label, "Proj");
        assert_eq!(r.project_path_raw, "/Users/x/Proj");
        assert_eq!(r.project_realpath, "/users/x/proj");
        assert_eq!(
            r.title.as_deref(),
            Some("t"),
            "COALESCE：NULL 不得抹掉已有标题"
        );
        assert!(
            r.is_subagent,
            "MAX：一旦判定子代理就保持（R3 父/子分层依赖它）"
        );
        assert_eq!(r.parent_session_id.as_deref(), Some("p1"));
        assert_eq!(r.originator.as_deref(), Some("codex-tui"));
        assert_eq!(r.last_seen_at, 30, "MAX：乱序批次不得把活跃时间改小");
        assert_eq!(
            r.first_seen_at, 10,
            "first_seen_at 只在 INSERT 时写（后续批次 30/99 都不得改写它）"
        );
    }

    /// 【fix 轮偏差新增】评审 Important #1 的回归锁：写路径失败**不得**再被当成「正常无事」——
    /// 失败行不计入 `written` 且不 panic（`log::warn!` 本身在 libtest 里没有 logger、无法断言，
    /// 只能靠 diff 审阅）。
    /// 手法：先建表再把四张表 DROP 掉 → 四条写路径必然走**新增的 `Err(_)` 分支**
    /// （不可能是 `merge_samples` 里「首次写入无旧行」那条正常路径，因为表都不存在了）。
    #[test]
    fn write_failures_are_zero_and_never_panic() {
        let conn = mem();
        conn.execute_batch(
            "DROP TABLE usage_detail;
             DROP TABLE usage_daily;
             DROP TABLE usage_cursor;
             DROP TABLE usage_session;",
        )
        .expect("测试自备前置：四张表先建后删");
        assert_eq!(
            upsert_detail_conn(&conn, &[detail("2026-10-03T09", "m1", 1)], 1),
            0,
            "明细写失败不得计入 written"
        );
        assert_eq!(
            upsert_daily_conn(
                &conn,
                &[DailyDelta {
                    day_key: "2026-10-03".into(),
                    source_id: "claude".into(),
                    project_key: "proj".into(),
                    provider: "p1".into(),
                    provider_kind: crate::services::usage::model::SourceKind::Inferred,
                    model: "m1".into(),
                    buckets: UsageBuckets::default(),
                    request_total: 1,
                    requests: 1,
                    user_est: None,
                }],
                1
            ),
            0,
            "日聚合写失败不得计入 written"
        );
        assert_eq!(
            upsert_session_conn(
                &conn,
                "codex",
                &[SessionDimDelta {
                    session_id: "s1".into(),
                    project_key: "proj".into(),
                    project_label: "Proj".into(),
                    project_path_raw: "/Users/x/Proj".into(),
                    project_realpath: "/users/x/proj".into(),
                    title: None,
                    is_subagent: false,
                    parent_session_id: None,
                    originator: None,
                    last_seen_at: 5,
                }],
                5
            ),
            0,
            "会话维度写失败不得计入 written"
        );
        assert_eq!(
            save_cursors_conn(&conn, "codex", &[CursorDelta::default()], 7),
            0,
            "游标写失败不得计入 written（水位没推进必须能被调用方看见）"
        );
        assert_eq!(
            purge_detail_before_conn(&conn, "2026-10-01"),
            0,
            "保留期清理失败不得 panic（返回值与「删除 0 行」同为 0，但会留一条 warn）"
        );
        // 损坏的 tool_stats 文本：留痕后按空统计继续（不得 panic）
        assert!(parse_tool_stats("{ 不是 JSON").is_empty());
        assert!(parse_tool_stats("").is_empty());
    }

    #[test]
    fn purge_detail_keeps_daily() {
        let conn = mem();
        upsert_detail_conn(&conn, &[detail("2026-01-01T09", "m1", 5)], 1);
        upsert_daily_conn(
            &conn,
            &[DailyDelta {
                day_key: "2026-01-01".into(),
                source_id: "claude".into(),
                project_key: "/p".into(),
                provider: "p1".into(),
                // 计划正文此处漏写 `provider_kind`（`DailyDelta` 的必需字段）→ E0063：
                // 同文件另两处 `DailyDelta` 字面量都带它，这里按 claude 的惯例补 Inferred，
                // 期望值（删除条数）一字未改。
                provider_kind: crate::services::usage::model::SourceKind::Inferred,
                model: "m1".into(),
                buckets: UsageBuckets {
                    input_fresh: 5,
                    ..Default::default()
                },
                request_total: 5,
                requests: 1,
                user_est: None,
            }],
            1,
        );
        let deleted = purge_detail_before_conn(&conn, "2026-10-01");
        assert_eq!(deleted, 1);
        assert_eq!(count_rows_conn(&conn, "usage_detail"), 0);
        assert_eq!(count_rows_conn(&conn, "usage_daily"), 1, "日聚合永久保留");
    }

    /// 【偏差新增，见报告「偏差申报」第 7 条】覆盖 `load_sessions_conn(None)` 的**无源过滤**分支：
    /// 它是动态拼 SQL + 空参数绑定（`query_map([], ..)`），一旦运行期出错会被 `unwrap_or_default()`
    /// **静默吞成空表**——本任务其余用例只走 `Some(src)` 分支，这条生产路径在原计划里零覆盖。
    #[test]
    fn load_sessions_without_source_filter_spans_all_sources() {
        let conn = mem();
        let mk = |sid: &str| SessionDimDelta {
            session_id: sid.into(),
            project_key: "proj".into(),
            project_label: "Proj".into(),
            project_path_raw: "/Users/x/Proj".into(),
            project_realpath: "/users/x/proj".into(),
            title: None,
            is_subagent: false,
            parent_session_id: None,
            originator: None,
            last_seen_at: 9,
        };
        upsert_session_conn(&conn, "codex", &[mk("s-codex")], 9);
        upsert_session_conn(&conn, "claude", &[mk("s-claude")], 9);
        // 前提断言：行确实落库了（否则下面的 None 分支会「空对空」假绿）
        assert_eq!(load_sessions_conn(&conn, Some("codex")).len(), 1);
        let all = load_sessions_conn(&conn, None);
        assert_eq!(all.len(), 2, "无源过滤必须读到全部源的会话行");
        assert!(all
            .iter()
            .any(|r| r.source_id == "codex" && r.session_id == "s-codex"));
        assert!(all
            .iter()
            .any(|r| r.source_id == "claude" && r.session_id == "s-claude"));
    }
}
