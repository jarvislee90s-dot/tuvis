//! 账本落库：**分批短事务**（宿主硬约束，说明书 §7.2/§10）。
//!
//! 为什么必须分批：兔维斯 的 SQLite 未启用 WAL、未设 busy_timeout，全局只有一把
//! `Mutex<Connection>`（database/connection.rs:21），而 3 秒的 `get_all_sessions`
//! 轮询要频繁取同一把锁。若一个事务写完 20 万行，会话卡/通知/宠物会一起卡住数秒。
//! 因此：每事务 ≤ LEDGER_BATCH_ROWS 行 → drop 锁 → sleep(LEDGER_YIELD_MS) 让出，
//! 单批锁持有窗口稳定在毫秒级（Task 4 测试与 Task 22 并发验收双重锁住）。
//!
//! **不启用 WAL / 不换独立连接**：那会改变整个 tuvis.db 的既有行为，属【默认裁定】，
//! 只做评估与提请（Task 22），不在本计划实施范围内。
use std::collections::HashMap;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::database::connection::DB;
use crate::database::dao::usage as dao;
use crate::services::usage::delta::{DailyDelta, DetailDelta, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;

/// 单事务最大行数（明细 + 日聚合 + 会话维度 + 游标合计）
pub const LEDGER_BATCH_ROWS: usize = 200;
/// 批间让出时长：给 3 秒轮询的取锁请求留出确定性窗口
pub const LEDGER_YIELD_MS: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerWriteStats {
    pub rows: usize,
    pub batches: usize,
    pub max_batch_rows: usize,
    pub max_hold_ms: u128,
}

/// 明细 → 日聚合（纯函数）：按 (day, source, **记录级 project_key**, provider, model) 折叠。
/// 项目键取自 `DetailDelta.key.project_key`（D21 规则④：一个文件可跨多个项目；小时档与日档
/// 用的是**同一个键**，两档口径不得分叉）；`user_est` 按 R1 折叠（全不可得才是 None）。
pub fn daily_rows_of(source_id: &str, delta: &SourceDelta) -> Vec<DailyDelta> {
    let mut out: Vec<DailyDelta> = Vec::new();
    let mut index: HashMap<(String, String, String, String), usize> = HashMap::new();
    for d in &delta.details {
        let k = (
            d.key.day_key.clone(),
            d.key.project_key.clone(),
            d.key.provider.clone(),
            d.key.model.clone(),
        );
        match index.get(&k) {
            Some(&i) => {
                out[i].buckets.add(&d.buckets);
                out[i].request_total += d.request_total;
                out[i].requests += d.requests;
                out[i].provider_kind = out[i].provider_kind.strongest(d.provider_kind);
                // R1：只有「全组都不可得」才保持 None；任一条有值就按 0 补位累加
                if out[i].user_est.is_some() || d.user_est.is_some() {
                    out[i].user_est = Some(out[i].user_est.unwrap_or(0) + d.user_est.unwrap_or(0));
                }
            }
            None => {
                index.insert(k, out.len());
                out.push(DailyDelta {
                    day_key: d.key.day_key.clone(),
                    source_id: source_id.to_string(),
                    project_key: d.key.project_key.clone(),
                    provider: d.key.provider.clone(),
                    provider_kind: d.provider_kind,
                    model: d.key.model.clone(),
                    buckets: d.buckets,
                    request_total: d.request_total,
                    requests: d.requests,
                    user_est: d.user_est,
                });
            }
        }
    }
    out
}

/// 分批落库：每批一个独立事务、批间释放全局锁并让出。
/// **采集器的全部计算已在内存完成**——本函数只做写，不做任何文件/网络 IO。
///
/// **B3 修复点**：批次计划由 `plan_batches` 产出——四类行（明细 / 日聚合 / 会话维度 / 游标）
/// **各自**按 `LEDGER_BATCH_ROWS` 切片。早期版本把全部 sessions + cursors 塞进**最后一个**
/// 批次：codex 真机 425 个 rollout 游标 + ~450 条 `session_meta` = 875 行 > 200 →
/// 必然 `Err`，而明细/日聚合前几批**已提交** → 账本半写、游标不落库 → 下一轮整源重扫重算。
pub fn apply_delta(
    source_id: UsageSourceId,
    delta: &SourceDelta,
) -> Result<LedgerWriteStats, UsageError> {
    let sid = source_id.db_id();
    let daily = daily_rows_of(sid, delta);
    let batches = plan_batches(delta, &daily);
    validate_batches(&batches)?;

    // **先校验后写**（`validate_batches`，见其文档）→ 逐批短事务提交
    // （每批 ≤ LEDGER_BATCH_ROWS 行）→ drop 锁 → 让出
    let mut stats = LedgerWriteStats::default();
    for b in batches {
        let rows = b.rows();
        let started = Instant::now();
        {
            // 只在事务期间持有全局 DB 锁——这段就是 3 秒轮询可能被挡住的最坏窗口
            let mut conn = DB.lock().map_err(|e| {
                UsageError::new("usage-db-failed", format!("db poisoned lock: {e}"))
            })?;
            write_one_batch(&mut conn, &b, sid)?;
        } // ← 锁在此释放，随后主动让出
        record_batch(&mut stats, rows, started);
        if LEDGER_YIELD_MS > 0 {
            std::thread::sleep(Duration::from_millis(LEDGER_YIELD_MS));
        }
    }
    Ok(stats)
}

/// **调用方持连接的落库入口**（与 `apply_delta` 同一主体，只是连接由调用方给）。
///
/// 存在的理由（独立验证复核 §3.2.2 阻塞 2）：集成测试二进制里 `support::setup()` 用 `Once`
/// 只重定向一次 `HOME`/`TUVIS_HOME` → **同一个测试文件里的所有用例共用一个库**，
/// 于是 `count_rows_conn(..., "usage_detail") == 1` 这类**绝对行数**断言会随用例数量与执行顺序
/// 变化（旧计划里 Task 11/13 往同文件加了两条落库用例之后，Task 4 的 round_trip 在任何顺序下
/// 都不可能成立）。有了这个入口，每条集成用例可以拿 `support::open_ledger_db(tag)` 开一个
/// **自己的库文件**，断言回到绝对行数也依然稳定、可并行。
///
/// 与全局入口的差异：**不做批间 sleep**（`LEDGER_YIELD_MS` 是给全局锁让路的，私有连接没有
/// 竞争者；生产路径的让出语义由 `apply_delta` 保留）。批次切片与行数完全一致。
pub fn apply_delta_conn(
    conn: &mut Connection,
    source_id: UsageSourceId,
    delta: &SourceDelta,
) -> Result<LedgerWriteStats, UsageError> {
    let sid = source_id.db_id();
    let daily = daily_rows_of(sid, delta);
    let batches = plan_batches(delta, &daily);
    validate_batches(&batches)?;

    let mut stats = LedgerWriteStats::default();
    for b in batches {
        let rows = b.rows();
        let started = Instant::now();
        write_one_batch(conn, &b, sid)?;
        record_batch(&mut stats, rows, started);
    }
    Ok(stats)
}

/// **先校验后写**：全部批次在**开启第一个事务之前**逐批校验上限。任何一批超限都在这里
/// 返回 Err ——不会留下「前几批已提交、后几批没写」的半批状态。
/// （切片本身已保证不超限，这一步是纵深防御：将来有人改坏切片会立刻在这里炸，而不是半写。）
fn validate_batches(batches: &[Batch<'_>]) -> Result<(), UsageError> {
    for b in batches {
        if b.rows() > LEDGER_BATCH_ROWS {
            return Err(UsageError::internal(format!(
                "账本批次超上限：{} 行 > {}",
                b.rows(),
                LEDGER_BATCH_ROWS
            )));
        }
    }
    Ok(())
}

/// 一批 = 一个短事务（调用方已持连接/锁）
fn write_one_batch(
    conn: &mut Connection,
    b: &Batch<'_>,
    source_id: &str,
) -> Result<(), UsageError> {
    let tx = conn
        .transaction()
        .map_err(|e| UsageError::new("usage-db-failed", format!("begin tx: {e}")))?;
    write_batch(&tx, b, source_id, crate::services::usage::now_ms())?;
    tx.commit()
        .map_err(|e| UsageError::new("usage-db-failed", format!("commit tx: {e}")))?;
    Ok(())
}

fn record_batch(stats: &mut LedgerWriteStats, rows: usize, started: Instant) {
    stats.rows += rows;
    stats.batches += 1;
    stats.max_batch_rows = stats.max_batch_rows.max(rows);
    stats.max_hold_ms = stats.max_hold_ms.max(started.elapsed().as_millis());
}

#[derive(Default)]
struct Batch<'a> {
    /// 明细批次承载**引用**而非切片：明细按 (source_id, session_id) 分组后不再是输入数组的
    /// 连续区间（W-03，见 `plan_batches`）；日聚合/会话/游标不分组，仍是连续切片。
    details: Vec<&'a DetailDelta>,
    daily: &'a [DailyDelta],
    sessions: &'a [crate::services::usage::delta::SessionDimDelta],
    cursors: &'a [crate::services::usage::delta::CursorDelta],
}

impl Batch<'_> {
    /// 本批行数（= 该事务会写的行数上限；空批跳过与超限校验都用它）
    fn rows(&self) -> usize {
        self.details.len() + self.daily.len() + self.sessions.len() + self.cursors.len()
    }
}

/// 会话组内部的「同批单元」键：会话内 `source_id` / `session_id` 恒定，行键退化成四列
type SessionRowKey<'a> = (&'a str, &'a str, &'a str, &'a str);

/// 批次计划（**纯函数**，可零 DB 单测）：四类行**各自**按 `LEDGER_BATCH_ROWS` 切片，
/// 空批直接丢弃（不得凭空开事务）。顺序 = 明细 → 日聚合 → 会话维度 → 游标。
///
/// 为什么不混装：混装会让「明细 200 行 + 同批的日聚合/会话/游标」超限；按类切片后每批恒
/// ≤ `LEDGER_BATCH_ROWS`，代价只是事务数略多（每批之间 1ms 让出）。
///
/// **明细必须先按 `(source_id, session_id)` 分组再切片（W-03）**：`upsert_detail_conn` 的
/// `turn_ms` / `tool_stats` 走 `merge_samples`（先读旧值、合并、写回），是读改写。
/// 计划正文的 4 条用例（行数守恒 / 每批 ≤200）在「按行数硬切」下**全绿**，
/// 所以这个坑只有 `same_session_rows_never_cross_batches` 才锁得住。
///
/// 分组后批次不再是输入数组的连续切片 → `Batch.details` 存引用表（见 `write_batch`）。
/// 装批策略：以「必须同批的单元」为粒度贪心装填（整单元入批，装不下就另起一批），
/// 因此批次里的行数仍然 ≤ `LEDGER_BATCH_ROWS`，且**没有任何单元被拆开**。
fn plan_batches<'a>(delta: &'a SourceDelta, daily: &'a [DailyDelta]) -> Vec<Batch<'a>> {
    let mut batches: Vec<Batch<'a>> = Vec::new();

    // ① 明细 → 第一层单元：按 (source_id, session_id) 分组（顺序 = 首次出现顺序，组内保序）
    let mut session_index: HashMap<(&str, &str), usize> = HashMap::new();
    let mut session_units: Vec<Vec<&'a DetailDelta>> = Vec::new();
    for r in &delta.details {
        let k = (r.source_id.as_str(), r.key.session_id.as_str());
        match session_index.get(&k) {
            Some(&i) => session_units[i].push(r),
            None => {
                session_index.insert(k, session_units.len());
                session_units.push(vec![r]);
            }
        }
    }

    // ② 单元超上限的退让：单会话 > LEDGER_BATCH_ROWS（首轮全量扫描的长会话就是这个形状）时
    //    「单事务 ≤200」（GC 2）与「同会话同批」（W-03）不可兼得。**退让点在六列行键边界上**——
    //    `merge_samples` 的 WHERE 正是这六列，同一行键绝不跨批；若在这里硬拆行键，
    //    或让整组 >200 行进批（→ validate_batches 给整轮 Err、该源再也采不到数），都更坏。
    let mut units: Vec<Vec<&'a DetailDelta>> = Vec::new();
    for u in session_units {
        if u.len() <= LEDGER_BATCH_ROWS {
            units.push(u);
            continue;
        }
        let mut key_index: HashMap<SessionRowKey<'_>, usize> = HashMap::new();
        for r in u {
            let k = (
                r.key.project_key.as_str(),
                r.key.hour_key.as_str(),
                r.key.model.as_str(),
                r.key.provider.as_str(),
            );
            match key_index.get(&k) {
                Some(&i) => units[i].push(r),
                None => {
                    key_index.insert(k, units.len());
                    units.push(vec![r]);
                }
            }
        }
    }

    // ③ 贪心装批：整单元入批；本批已有行且装不下就结算，刚好装满也结算（不留半空批）
    let mut cur: Vec<&'a DetailDelta> = Vec::new();
    for u in units {
        if !cur.is_empty() && cur.len() + u.len() > LEDGER_BATCH_ROWS {
            batches.push(Batch {
                details: std::mem::take(&mut cur),
                ..Default::default()
            });
        }
        cur.extend(u);
        if cur.len() >= LEDGER_BATCH_ROWS {
            batches.push(Batch {
                details: std::mem::take(&mut cur),
                ..Default::default()
            });
        }
    }
    if !cur.is_empty() {
        batches.push(Batch {
            details: cur,
            ..Default::default()
        });
    }

    // ④ 日聚合 / 会话维度 / 游标：不做读改写（纯 upsert），按行数切片即可
    for chunk in daily.chunks(LEDGER_BATCH_ROWS) {
        batches.push(Batch {
            daily: chunk,
            ..Default::default()
        });
    }
    for chunk in delta.sessions.chunks(LEDGER_BATCH_ROWS) {
        batches.push(Batch {
            sessions: chunk,
            ..Default::default()
        });
    }
    for chunk in delta.cursors.chunks(LEDGER_BATCH_ROWS) {
        batches.push(Batch {
            cursors: chunk,
            ..Default::default()
        });
    }
    batches.retain(|b| b.rows() > 0);
    batches
}

/// 一个批次 → 四类表的 upsert。`source_id` **显式传入**（不取自行内字段）：
/// `usage_cursor` 的主键是 `(source_id, session_id)` 而 `CursorDelta` 里没有源字段，
/// `usage_session` 的主键同样是 `(source_id, session_id)` 而 `SessionDimDelta` 里也没有源字段
/// （见 Task 3 Interfaces 与 §3.2.2 阻塞 1 / 次要 6 / §3.2.3 FIX-1）。
/// 两个 upsert 都必须收到 `source_id`——漏传是编译错（E0061），行内取源是 E0609。
///
/// **四类行都必须核对 DAO 的写入行数（评审 Important #1）**：DAO 的写路径在**行级**失败时只
/// `log::warn!` 然后继续（`dao/usage.rs:225/377/441`），返回值就是「实际写成功的行数」。
/// 早期版本把四个返回值全丢了，于是 `write_one_batch` 的 `Result` 只剩 `transaction()` /
/// `commit()` 两个失败点：某行 INSERT 失败 → 本批照常 `commit` → 游标批照常提交 →
/// `apply_delta` 返回 `Ok`、`stats.rows` 还按**计划行数**上报 → 采集器推进水位 →
/// **该行永久缺失**（本任务要消灭的「账本半写」只是降到了行粒度）。
/// 现在任何一行没写进去都在 `tx.commit()` **之前** `Err`：该批整体回滚、整轮 `Err`、水位不推进。
fn write_batch(
    tx: &Connection,
    b: &Batch<'_>,
    source_id: &str,
    now_ms: i64,
) -> Result<(), UsageError> {
    if !b.details.is_empty() {
        // 明细批次分组后**不再是连续切片**（W-03），而 DAO 收 `&[DetailDelta]`：
        // 这里拷成连续切片再交出去（每批 ≤200 行；拷贝成本远小于一次事务的落库成本，
        // 也保住 DAO「整批只拼一次 SQL」的形态）。
        let rows: Vec<DetailDelta> = b.details.iter().map(|r| (*r).clone()).collect();
        let written = dao::upsert_detail_conn(tx, &rows, now_ms);
        if written != rows.len() {
            return Err(UsageError::internal(format!(
                "明细写入 {written}/{} 行",
                rows.len()
            )));
        }
    }
    if !b.daily.is_empty() {
        let written = dao::upsert_daily_conn(tx, b.daily, now_ms);
        if written != b.daily.len() {
            return Err(UsageError::internal(format!(
                "日聚合写入 {written}/{} 行",
                b.daily.len()
            )));
        }
    }
    if !b.sessions.is_empty() {
        let written = dao::upsert_session_conn(tx, source_id, b.sessions, now_ms);
        if written != b.sessions.len() {
            return Err(UsageError::internal(format!(
                "会话维度写入 {written}/{} 行",
                b.sessions.len()
            )));
        }
    }
    if !b.cursors.is_empty() {
        let written = dao::save_cursors_conn(tx, source_id, b.cursors, now_ms);
        if written != b.cursors.len() {
            return Err(UsageError::internal(format!(
                "游标写入 {written}/{} 行",
                b.cursors.len()
            )));
        }
    }
    Ok(())
}

/// 保留期清理（「聚合后删明细」）：日聚合在写入时同事务维护，这里**只删明细**。
/// 返回删除行数。retention_days < 1 时按 1 处理（设置层已校验，这里是纵深防御）。
pub fn purge_expired(retention_days: i64, now_ms: i64) -> Result<usize, UsageError> {
    let cutoff = cutoff_day(retention_days, now_ms);
    let conn = DB
        .lock()
        .map_err(|e| UsageError::new("usage-db-failed", format!("db poisoned lock: {e}")))?;
    Ok(dao::purge_detail_before_conn(&conn, &cutoff))
}

/// **调用方持连接的清理入口**（集成测试的私有库用；语义与 `purge_expired` 完全一致）
pub fn purge_expired_conn(conn: &Connection, retention_days: i64, now_ms: i64) -> usize {
    let cutoff = cutoff_day(retention_days, now_ms);
    dao::purge_detail_before_conn(conn, &cutoff)
}

/// 保留期边界日键（本地时区）：早于该日的明细可删
///
/// 口径 = `range::day_key_of_host`（Task 8）：本地时区整日 `YYYY-MM-DD`。
/// **本处不再自算**（GC 6「口径层唯一」）：Task 4 落地时 `range.rs` 尚不存在，曾内联一份等价实现；
/// Task 8 已把那次内联副本删掉、改为直接调用，避免仓里长期存有两份本地日实现。
/// 降级行为一致：时间戳超出可表示范围时 `range::day_key_of_host` 同样给空串
/// （锁：本文件 `cutoff_day_delegates_to_range_day_key_of_host`）。
pub fn cutoff_day(retention_days: i64, now_ms: i64) -> String {
    let days = retention_days.max(1);
    let ts = now_ms - days * 24 * 3600 * 1000;
    crate::services::usage::range::day_key_of_host(ts)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::delta::{
        CursorDelta, DetailDelta, DetailKey, SessionCounters, SessionDimDelta,
    };
    use crate::services::usage::model::UsageBuckets;
    use crate::services::usage::semantics::CacheSemantics;
    // 显式导入（不靠 `use super::*` 的 glob）：本模块在 Step 1 阶段 `super` 里还没有任何名字，
    // 且 glob 只带进父模块**自己** `use` 进来的名字（D-01 的判定口诀）。
    use std::collections::HashMap;

    fn delta_rows(n: usize) -> SourceDelta {
        let details = (0..n)
            .map(|i| DetailDelta {
                source_id: "claude".into(),
                provider_kind: crate::services::usage::model::SourceKind::Inferred,
                key: DetailKey {
                    session_id: format!("s{}", i % 50),
                    hour_key: "2026-10-03T09".into(),
                    day_key: "2026-10-03".into(),
                    project_key: "proj".into(),
                    model: "m".into(),
                    provider: "p".into(),
                },
                buckets: UsageBuckets {
                    input_fresh: 1,
                    ..Default::default()
                },
                request_total: 1,
                requests: 1,
                user_est: None,
                cache_semantics: CacheSemantics::Exclusive,
                counters: SessionCounters::default(),
                ts_ms: 1,
            })
            .collect();
        SourceDelta {
            details,
            ..Default::default()
        }
    }

    /// 单条明细行构造器：`hour` = `"YYYY-MM-DDTHH"`（`day_key` 取前 10 字符，与采集器同一派生规则）
    fn detail_row(session_id: &str, hour: &str) -> DetailDelta {
        DetailDelta {
            source_id: "claude".into(),
            provider_kind: crate::services::usage::model::SourceKind::Inferred,
            key: DetailKey {
                session_id: session_id.into(),
                hour_key: hour.into(),
                day_key: hour.chars().take(10).collect(),
                project_key: "proj".into(),
                model: "m".into(),
                provider: "p".into(),
            },
            buckets: UsageBuckets {
                input_fresh: 1,
                ..Default::default()
            },
            request_total: 1,
            requests: 1,
            user_est: None,
            cache_semantics: CacheSemantics::Exclusive,
            counters: SessionCounters::default(),
            ts_ms: 1,
        }
    }

    fn session_row(i: usize) -> SessionDimDelta {
        SessionDimDelta {
            session_id: format!("sess-{i}"),
            project_key: "proj".into(),
            project_label: "Proj".into(),
            project_path_raw: "/p/Proj".into(),
            project_realpath: "/p/proj".into(),
            title: None,
            is_subagent: false,
            parent_session_id: None,
            originator: None,
            last_seen_at: 1,
        }
    }

    fn cursor_row(i: usize) -> CursorDelta {
        CursorDelta {
            session_id: format!("rollout-{i}"),
            fingerprint: "ab".into(),
            byte_offset: 1,
            ordinal: 1,
            last_cumulative: None,
            mtime_ms: 1,
            file_size: 1,
            state_json: "{}".into(),
        }
    }

    /// 硬约束（说明书 §7.2 / §10）：**单事务写入行数 ≤ LEDGER_BATCH_ROWS**——
    /// 「一个事务写完一整轮采集」被结构性禁止。5000 条明细 → ≥25 个批次。
    #[test]
    fn writes_are_split_into_small_transactions() {
        // 修（D-13 同族的最小修正）：原样 `plan_batches(&delta_rows(5_000), &[])` 让批次计划
        // 借用**临时值**，临时值在该语句末尾即析构 → E0716（DISPATCH-PREAMBLE §3 点名的坑）。
        // 先绑变量再借用；断言一字未改。
        let delta = delta_rows(5_000);
        let plan = plan_batches(&delta, &[]);
        assert_eq!(plan.iter().map(|b| b.rows()).sum::<usize>(), 5_000);
        assert!(
            plan.iter().all(|b| b.rows() <= LEDGER_BATCH_ROWS),
            "单事务最多 {} 行，实测最大 {}",
            LEDGER_BATCH_ROWS,
            plan.iter().map(|b| b.rows()).max().unwrap_or(0)
        );
        assert!(
            plan.len() >= 5_000 / LEDGER_BATCH_ROWS,
            "5000 行至少要 {} 个事务，实测 {}",
            5_000 / LEDGER_BATCH_ROWS,
            plan.len()
        );
    }

    /// 空增量：零批次（不得凭空开事务）
    #[test]
    fn empty_delta_has_no_batches() {
        let delta = SourceDelta::default(); // 同上：先绑变量，避免 E0716
        let plan = plan_batches(&delta, &[]);
        assert!(plan.is_empty(), "空增量不得开任何事务");
    }

    /// **B3 回归（纯函数面）**：`sessions` / `cursors` 必须与 details/daily 一样按
    /// `LEDGER_BATCH_ROWS` 切片。真机形态：codex 425 个 rollout → 425 条游标，25 个文件含
    /// 父子双线程 → 约 450 条 `session_meta`；旧实现把两类**一起塞进最后一个批次** =
    /// 875 行 > 200 → `apply_delta` 必然 `Err`，而明细/日聚合前几批**已提交** → 账本半写
    /// （游标不落库 → 下一轮整源重扫重算，明细按累加语义膨胀）。
    #[test]
    fn sessions_and_cursors_are_batched_in_the_plan() {
        let delta = SourceDelta {
            sessions: (0..450).map(session_row).collect(),
            cursors: (0..425).map(cursor_row).collect(),
            ..Default::default()
        };
        let plan = plan_batches(&delta, &[]);
        assert_eq!(
            plan.iter().map(|b| b.rows()).sum::<usize>(),
            875,
            "一行都不能丢"
        );
        assert!(
            plan.iter().all(|b| b.rows() <= LEDGER_BATCH_ROWS),
            "任何批次都不得超上限（旧实现在这里就是 875 行一批）"
        );
        // 450 会话 → 3 批（200/200/50）；425 游标 → 3 批（200/200/25）
        assert_eq!(plan.len(), 6, "会话与游标各自分批");
        assert_eq!(plan.iter().map(|b| b.sessions.len()).sum::<usize>(), 450);
        assert_eq!(plan.iter().map(|b| b.cursors.len()).sum::<usize>(), 425);
    }

    /// 日聚合折叠：明细按 (day, **记录级 project_key**, provider, model) 求和（D21/R2）；
    /// `user_est` 同规则折叠——有值才累加、全不可得才是 `None`（R1，日档不得恒为 null）
    #[test]
    fn daily_rows_fold_by_project() {
        let mut d = delta_rows(4);
        for (i, r) in d.details.iter_mut().enumerate() {
            r.key.project_key = if i < 2 { "alpha".into() } else { "beta".into() };
            r.buckets.output = 10;
            r.request_total = 10;
            // 前两条有值（3 + 5），后两条不可得 → alpha 得 8、beta 保持 None
            r.user_est = if i < 2 {
                Some(if i == 0 { 3 } else { 5 })
            } else {
                None
            };
        }
        let daily = daily_rows_of("claude", &d);
        assert_eq!(daily.len(), 2, "两个项目 → 两行（记录级键折叠）");
        assert!(daily.iter().all(|r| r.buckets.output == 20));
        assert!(daily.iter().any(|r| r.project_key == "alpha"));
        let alpha = daily.iter().find(|r| r.project_key == "alpha").unwrap();
        let beta = daily.iter().find(|r| r.project_key == "beta").unwrap();
        assert_eq!(alpha.user_est, Some(8), "R1：有值的项目在日档必须出真值");
        assert_eq!(
            beta.user_est, None,
            "R1：全不可得的项目在日档是 None，不是 0"
        );
    }

    /// **W-03 锁（计划正文 4 条用例全绿也抓不到）**：`upsert_detail_conn` 的
    /// `turn_ms` / `tool_stats` 走 `merge_samples`（先读旧值、合并、写回）——
    /// **同一 `(source_id, session_id)` 的行不得跨批**。
    /// 输入形态 = 真机的多文件交错（`delta_rows` 就是 `s{i % 50}` 这种交错）：
    /// 按行数硬切必然把同一会话的行切进两个批次。
    #[test]
    fn same_session_rows_never_cross_batches() {
        let delta = delta_rows(600);
        let plan = plan_batches(&delta, &[]);
        let mut first_batch: HashMap<&str, usize> = HashMap::new();
        let mut total = 0usize;
        for (bi, b) in plan.iter().enumerate() {
            total += b.details.len();
            for r in b.details.iter() {
                match first_batch.get(r.key.session_id.as_str()) {
                    Some(&prev) => assert_eq!(
                        prev, bi,
                        "会话 {} 的行跨批（批 {} → 批 {}）：merge_samples 的读改写会跨事务（W-03）",
                        r.key.session_id, prev, bi
                    ),
                    None => {
                        first_batch.insert(r.key.session_id.as_str(), bi);
                    }
                }
            }
        }
        assert_eq!(total, 600, "一行都不能丢");
        assert!(plan.iter().all(|b| b.rows() <= LEDGER_BATCH_ROWS));
        assert_eq!(first_batch.len(), 50, "50 个会话各自落在一个批次里");
    }

    /// 单会话 > `LEDGER_BATCH_ROWS`（首轮全量扫描的长会话真实形状）：GC 2 的「单事务 ≤200」
    /// 与「同会话同批」在此**不可兼得**。本实现的退让点 = **六列行键**边界
    /// （`merge_samples` 的 WHERE 就是这六列）：同一行键绝不跨批，单会话只在行键之间切开。
    #[test]
    fn oversized_session_splits_on_row_key_boundaries() {
        // 250 行同会话、各自不同的小时键；再追加 50 行**复用**前 50 个行键（同键两份样本）
        let mut details: Vec<DetailDelta> = (0..250)
            .map(|i| detail_row("long", &format!("2026-10-{:02}T{:02}", 1 + i / 24, i % 24)))
            .collect();
        for i in 0..50 {
            details.push(detail_row(
                "long",
                &format!("2026-10-{:02}T{:02}", 1 + i / 24, i % 24),
            ));
        }
        let delta = SourceDelta {
            details,
            ..Default::default()
        };
        let plan = plan_batches(&delta, &[]);
        assert!(
            plan.iter().all(|b| b.rows() <= LEDGER_BATCH_ROWS),
            "退让后仍须守住 ≤{} 行硬上限",
            LEDGER_BATCH_ROWS
        );
        assert_eq!(
            plan.iter().map(|b| b.rows()).sum::<usize>(),
            300,
            "一行都不能丢"
        );
        let mut first_batch: HashMap<&str, usize> = HashMap::new();
        for (bi, b) in plan.iter().enumerate() {
            for r in b.details.iter() {
                match first_batch.get(r.key.hour_key.as_str()) {
                    Some(&prev) => assert_eq!(
                        prev, bi,
                        "行键 {} 跨批（同键的两份样本会被拆开）",
                        r.key.hour_key
                    ),
                    None => {
                        first_batch.insert(r.key.hour_key.as_str(), bi);
                    }
                }
            }
        }
        assert_eq!(first_batch.len(), 250, "250 个行键各只落一批");
    }

    /// `cutoff_day` = **本地时区**边界日键（早于该日可删）；`retention_days < 1` 按 1 处理
    /// （纵深防御：0 天 = 删到今天，绝不允许）。
    ///
    /// 注：Task 4 落地时 `range::day_key_of_host` 还不存在（`range.rs` 由 Task 8 创建），
    /// 故当时本文件的本地日换算为内联实现；**Task 8 已按 D-13 删掉那份内联副本**、
    /// 改为直接调 `range::day_key_of_host`（见 `cutoff_day_delegates_to_range_day_key_of_host`
    /// 这条结构锁：两处一旦分叉即红）。
    #[test]
    fn cutoff_day_is_the_local_day_key_floored_at_one_day() {
        use chrono::TimeZone;
        // 固定本地正午（2026-10-03；常见时区此日无夏令时切换）→ 1 天前的日键必须是 10-02
        let noon = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .expect("构造本地时间 2026-10-03 12:00 失败");
        let t = noon.timestamp_millis();
        assert_eq!(cutoff_day(1, t), "2026-10-02");
        // < 1 一律按 1（不得出现「删到今天」）
        assert_eq!(cutoff_day(0, t), "2026-10-02");
        assert_eq!(cutoff_day(-7, t), "2026-10-02");
        // 天数越大边界越早（日键字典序 = 时间序）
        assert!(cutoff_day(30, t) < cutoff_day(1, t));
    }

    /// **口径层唯一**（GC 6 / D-13）：`cutoff_day` 必须**就是** `range::day_key_of_host`，
    /// 不是「又一份等价的本地日实现」。逐值对照 + 越界降级对齐（两处都给空串）——
    /// 谁把内联副本改回来并写歪一格，这条即红。
    ///
    /// **探针时刻刻意取本地 00:30 / 正午 / 23:30**：本地正午探针**抓不住**「用 UTC 日界替代
    /// 本地日界」这种口径分叉（UTC+8 的正午 = 04:00Z，两侧日期相同，变异测试实证过这条假绿），
    /// 故必须取**贴着本地日界**的时刻（东时区 00:30 分叉、西时区 23:30 分叉）；
    /// 日期 2026-05-10 当天全球各时区均无夏令时切换。
    #[test]
    fn cutoff_day_delegates_to_range_day_key_of_host() {
        use crate::services::usage::range::day_key_of_host;
        use chrono::TimeZone;
        let probe = |h, min| {
            chrono::Local
                .with_ymd_and_hms(2026, 5, 10, h, min, 0)
                .single()
                .expect("构造本地时间 2026-05-10 失败（该日全球无夏令时切换）")
                .timestamp_millis()
        };
        for t in [probe(0, 30), probe(12, 0), probe(23, 30)] {
            for days in [1_i64, 7, 30, 90, 365] {
                assert_eq!(
                    cutoff_day(days, t),
                    day_key_of_host(t - days * 24 * 3600 * 1000),
                    "cutoff_day({days}) 必须等于 range::day_key_of_host（口径层唯一）"
                );
            }
        }
        // 越界时间戳：两处降级形态必须一致（都为空串，不是 panic、不是回落当日）
        assert_eq!(cutoff_day(1, i64::MAX), "");
        assert_eq!(
            cutoff_day(1, i64::MAX),
            day_key_of_host(i64::MAX - 24 * 3600 * 1000)
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
