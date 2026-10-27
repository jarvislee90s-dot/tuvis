//! TPS 采集编排（Spec §6 ReadBatch 契约的消费侧；2026-10-24 计划 Task 7）。
//!
//! ## 职责与边界
//!
//! * **跑源**：对每个**扫描类**来源调 `sources::X::read_incremental(home, cursor, now)`
//!   （claude 是接收器喂入的形态例外，见 sources/mod.rs——不在此扫描）；`Ok` 批经
//!   [`store::commit_batch`] **同事务**落「事实 + 游标 + 来源状态」，`Err` 整源跳过——
//!   单源失败绝不丢其他源的 series，也绝不推进失败源的水位（Spec §6「读取失败不改变
//!   水位」）；V2 起（Task 6）失败源额外落一条 ok=false 的**失败健康态**（detail =
//!   错误码+定位、无载荷原文），查询面据此报 NeedsCheck，下一成功批自然恢复健康。
//! * **建点**：[`produce_point`] 尾读 120 秒窗内事实 + 近 24h 已知 series →
//!   `build_point_with_trigger` → 落库。空窗仍为已知 series 产 gap（Spec §2）。
//! * **清理**：[`maybe_purge`] 慢节奏（1 小时一次）做 24h 保留清理，锚点落
//!   `sched:last_purge_ms`（tps_state 键前缀注册表见 store.rs）。
//! * **单飞**：轮级 [`try_begin_round`] 闸——运行循环与手动刷新并发时只有一方进采集，
//!   源级单飞由「同一时刻至多一轮」蕴含。
//! * **运行接线**（[`spawn_runtime`]）：lib.rs setup 里 spawn 的后台线程，每秒一拍：
//!   复用 `usage.enabled` 总授权（**不设第二个开关**，Spec §2）——关闭时停调度/停接收
//!   （查询面如实报 disabled 但不删历史），重开换新 runId 先采一次。**惯性保证**：
//!   总授权关闭时本线程只读一行 settings，不写任何账本、不改变旧 usage 采集的启动
//!   时序（Spec §6）。
//!
//! ## 预算纪律（AGENTS.md 采集域契约的 TPS 面）
//!
//! 文件读取的有界性由各来源读取器自带（Task 3–6：只读新增、尾 200 行）；本层不做
//! 全量 COUNT、不为建点重读全窗——[`produce_point`] 的 `load_recent` 只取
//! `(at−120s, at]`（ended 索引尾查），known_series 只取近 24h 的点（sampled 索引尾查）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use rusqlite::Connection;

use crate::services::tps::model::{PointTrigger, ReadBatch, SourceCursor, SourceState};
use crate::services::tps::scheduler::{Sampler, TickAction};
use crate::services::tps::store;
use crate::services::usage::error::UsageError;

/// 扫描类来源（`read_incremental(home,…)` 形态；claude 是接收器喂入，不在此列）
pub const SCANNED_SOURCES: &[&str] = &["dsh", "zcode", "kimi", "workbuddy", "codex", "opencode"];
/// 全部注册来源（查询面 sourceStates 的枚举口径；含接收器喂入的 claude）
pub const REGISTERED_SOURCES: &[&str] = &[
    "dsh",
    "zcode",
    "kimi",
    "workbuddy",
    "codex",
    "opencode",
    "claude",
];
/// 保留清理慢节奏（Spec §2「按结束时间/采样时间清理」——不在每拍做）
const PURGE_CADENCE_MS: i64 = 60 * 60 * 1000;
/// 清理节奏锚点键（tps_state `sched:` 前缀族，注册表见 store.rs）
const LAST_PURGE_KEY: &str = "sched:last_purge_ms";

/// 单源执行报告（隔离失败的可观测面：本轮每源各自 ok / 提交数 / 错误）
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRunReport {
    pub source: String,
    pub ok: bool,
    pub committed: usize,
    pub detail: Option<String>,
    pub error_code: Option<String>,
}

/// 一轮采集的汇总
#[derive(Debug, Clone, PartialEq)]
pub struct CollectRound {
    pub reports: Vec<SourceRunReport>,
    pub total_committed: usize,
}

/// 采样段 id（每次 Tuvis 启动 / 每次重开总授权时新生成；进程内纳秒串，与
/// `app_run_id` 同形态——纯进程内值，不落盘）
pub fn generate_run_id() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default()
}

// ==== 采集轮 ====
//
// 一轮 = 三阶段（独立复核 R3：源 IO 不得持全局 DB 锁）：
//   1. [`load_cursors`]（持锁，毫秒级）：读各源已提交游标；
//   2. [`read_sources`]（**不持锁**）：按游标做源 IO——文件尾读 / 宿主 SQLite 尾查；
//   3. [`commit_reads`]（持锁，短）：Ok 批 `commit_batch` 同事务落事实+游标+状态，Err 只记报告。
// 生产 runtime 路径按三段分别取锁（见 [`runtime_step`] / [`manual_refresh_execute`]）；
// 轮级单飞闸保证阶段 1 读到的游标在阶段 3 之前不会被另一轮改写。[`run_round`] 是三阶段
// 的串行便捷入口（测试与同步调用）。

/// 阶段 1（持锁）：加载全部扫描类来源的已提交游标（读失败按空游标重读，UPSERT 幂等）
pub fn load_cursors(conn: &Connection) -> Vec<(&'static str, SourceCursor)> {
    SCANNED_SOURCES
        .iter()
        .map(|source| {
            let cursor = store::load_cursor(conn, source)
                .unwrap_or_else(|e| {
                    log::warn!(
                        "tps/{source}: 游标读取失败（按空游标重读，UPSERT 幂等）：{} / {}",
                        e.code,
                        e.detail
                    );
                    None
                })
                .unwrap_or_else(|| SourceCursor::empty(source));
            (*source, cursor)
        })
        .collect()
}

/// 阶段 2（**不持 DB 锁**）：按游标读各源。签名刻意不含 Connection——源 IO 只依赖
/// 入参 home 与游标；任何需要数据库的操作都不属于本阶段（文件尾读 / 宿主 SQLite 尾查
/// 可能耗时，必须在全局锁之外进行，否则阻塞查询面）。
pub fn read_sources(
    home: &Path,
    now_ms: i64,
    cursors: Vec<(&'static str, SourceCursor)>,
) -> Vec<(&'static str, Result<ReadBatch, UsageError>)> {
    cursors
        .into_iter()
        .map(|(source, cursor)| {
            let read = match source {
                "dsh" => {
                    crate::services::tps::sources::dsh::read_incremental(home, &cursor, now_ms)
                }
                "zcode" => {
                    crate::services::tps::sources::zcode::read_incremental(home, &cursor, now_ms)
                }
                "kimi" => {
                    crate::services::tps::sources::kimi::read_incremental(home, &cursor, now_ms)
                }
                "workbuddy" => crate::services::tps::sources::workbuddy::read_incremental(
                    home, &cursor, now_ms,
                ),
                "codex" => {
                    crate::services::tps::sources::codex::read_incremental(home, &cursor, now_ms)
                }
                "opencode" => {
                    crate::services::tps::sources::opencode::read_incremental(home, &cursor, now_ms)
                }
                other => Err(UsageError::internal(format!(
                    "unknown scanned source {other}"
                ))),
            };
            (source, read)
        })
        .collect()
}

/// 阶段 3（持锁，短）：Ok 批 `commit_batch`（事实/游标/状态同事务）；Err 原水位不动，只记报告
pub fn commit_reads(
    conn: &mut Connection,
    reads: Vec<(&'static str, Result<ReadBatch, UsageError>)>,
) -> CollectRound {
    let mut reports = Vec::with_capacity(reads.len());
    let mut total = 0usize;
    for (source, read) in reads {
        let report = match read {
            Err(e) => {
                // 整源失败：水位**不动**（Spec §6「读取失败不改变水位」，cursor 键绝不写）；
                // V2（Task 6）：失败健康态照报——落一条 ok=false 的 SourceState（detail =
                // 错误码+定位，绝无载荷原文），查询面据此给 NeedsCheck；下一成功批自然恢复。
                let detail = format!("{}: {}", e.code, e.detail);
                log::warn!(
                    "tps/{source}: 本轮读取失败（水位不动，落失败健康态）：{}",
                    detail
                );
                let state = SourceState {
                    source: source.to_string(),
                    // SourceState 内嵌的 cursor 只是状态载荷的镜像字段：
                    // 取当前已提交水位（无则空游标），真正的 cursor:<source> 键不写
                    cursor: store::load_cursor(conn, source)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| SourceCursor::empty(source)),
                    ok: false,
                    detail: Some(detail.clone()),
                };
                match serde_json::to_value(&state) {
                    Ok(value) => {
                        if let Err(se) =
                            store::save_state(conn, &store::source_state_key(source), &value)
                        {
                            log::warn!(
                                "tps/{source}: 失败健康态落库失败（下轮重写）：{} / {}",
                                se.code,
                                se.detail
                            );
                        }
                    }
                    Err(e) => {
                        log::warn!("tps/{source}: 失败健康态序列化失败（下轮重写）：{e}");
                    }
                }
                SourceRunReport {
                    source: source.to_string(),
                    ok: false,
                    committed: 0,
                    detail: Some(detail),
                    error_code: Some(format!("{}: {}", e.code, e.detail)),
                }
            }
            Ok(batch) => {
                let committed = batch.observations.len();
                let detail = batch.source_state.detail.clone();
                match store::commit_batch(conn, &batch) {
                    Ok(()) => SourceRunReport {
                        source: source.to_string(),
                        ok: true,
                        committed,
                        detail,
                        error_code: None,
                    },
                    Err(e) => {
                        // 提交失败 = 读取成功但没落库：下一轮靠稳定键 UPSERT 重读补齐
                        log::warn!(
                            "tps/{source}: 提交失败（下轮重读补齐）：{} / {}",
                            e.code,
                            e.detail
                        );
                        SourceRunReport {
                            source: source.to_string(),
                            ok: false,
                            committed: 0,
                            detail,
                            error_code: Some(format!("{}: {}", e.code, e.detail)),
                        }
                    }
                }
            }
        };
        if report.ok {
            total += report.committed;
        }
        reports.push(report);
    }
    CollectRound {
        reports,
        total_committed: total,
    }
}

/// 三阶段串行便捷入口（测试与同步调用）。注意它持锁跨越全部三段——生产 runtime 路径
/// 不走这里，而是分段取锁（源 IO 期间不占全局 DB 锁）。
pub fn run_round(conn: &mut Connection, home: &Path, now_ms: i64) -> CollectRound {
    let cursors = load_cursors(conn);
    let reads = read_sources(home, now_ms, cursors);
    commit_reads(conn, reads)
}

// ==== 建点 ====

/// 在 at_ms 生成全部 series 的采样点并落库（Spec §2 一次建点的完整编排）：
/// * 窗内事实：`ended > at−120s` 的**尾查**（窗口过滤在 policy 层，半开口径在那边锁）；
/// * known_series：近 24h **事实**的 series ∪ 近 24h **点**的 series 去重——事实是
///   主记录（首点之前没有点可依，只看点会漏「有事实无点」的 series），点补足
///   「事实已清、gap 点仍在」的对齐面（两边都走索引尾查）；
/// * 空窗仍为已知 series 出 gap；无任何已知 series 返回 0（不造点、不造 series）。
pub fn produce_point(
    conn: &mut Connection,
    run_id: &str,
    at_ms: i64,
    trigger: PointTrigger,
) -> Result<usize, UsageError> {
    produce_point_with_cadence(conn, run_id, at_ms, trigger, None)
}

/// [`produce_point`] 的完整版：点携带发点时的计划节奏 `cadence_ms`（独立复核 R11；
/// 运行接线从 `TickAction::EmitPoint` 取得；纯函数测试可传 None）。
pub fn produce_point_with_cadence(
    conn: &mut Connection,
    run_id: &str,
    at_ms: i64,
    trigger: PointTrigger,
    cadence_ms: Option<i64>,
) -> Result<usize, UsageError> {
    use crate::services::tps::model::SeriesKey;
    let facts = store::load_recent(conn, at_ms - crate::services::tps::policy::TPS_WINDOW_MS)?;
    let mut known: std::collections::BTreeMap<String, SeriesKey> =
        std::collections::BTreeMap::new();
    for o in store::load_recent(conn, at_ms - store::TPS_RETENTION_MS)? {
        let key = SeriesKey {
            tool: o.source.clone(),
            model: o.model,
            channel: o.channel,
            method: o.method,
        };
        known.entry(key.id()).or_insert(key);
    }
    for p in store::load_points_since(conn, at_ms - store::TPS_RETENTION_MS)? {
        known.entry(p.series_key.id()).or_insert(p.series_key);
    }
    let known: Vec<SeriesKey> = known.into_values().collect();
    let points = crate::services::tps::policy::build_point_with_schedule(
        &facts, &known, run_id, at_ms, trigger, cadence_ms,
    );
    let n = points.len();
    store::save_points(conn, &points)?;
    Ok(n)
}

// ==== 保留清理 ====

/// 慢节奏 24h 清理（1 小时一次；锚点 `sched:last_purge_ms` 落 tps_state）。
/// 节奏未到 → Ok(0)（不动库）；到达 → 清理并把锚点前移。**只**删 tps 两张表。
pub fn maybe_purge(conn: &Connection, now_ms: i64) -> Result<usize, UsageError> {
    let last = store::load_state(conn, LAST_PURGE_KEY)?.and_then(|v| v.as_i64());
    if let Some(last) = last {
        if now_ms - last < PURGE_CADENCE_MS {
            return Ok(0);
        }
    }
    let deleted = store::purge(conn, now_ms)?;
    store::save_state(conn, LAST_PURGE_KEY, &serde_json::json!(now_ms))?;
    Ok(deleted)
}

// ==== 轮级单飞 ====

static ROUND_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 尝试进入采集轮（已在飞行 → false）。与 [`end_round`] 配对使用。
pub fn try_begin_round() -> bool {
    !ROUND_FLIGHT.swap(true, Ordering::SeqCst)
}

/// 退出采集轮（调用方必须保证配对，含早退路径）
pub fn end_round() {
    ROUND_FLIGHT.store(false, Ordering::SeqCst);
}

/// 生产 home（真实用户目录；测试传影子目录，本函数不出现在测试路径）
pub(crate) fn runtime_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

// ==== 运行接线（lib.rs setup spawn；总授权复用 usage.enabled）====

/// 共享调度器：运行循环与 IPC 命令（心跳 / manual 刷新）共用同一份状态机。
/// `None` = 总授权从未开启（未构造）。锁序：**持 SAMPLER 锁期间绝不取 DB 锁**——
/// 命令层与循环都先摘动作/状态再放锁做 IO，杜绝 ABBA。
static SAMPLER: Lazy<std::sync::Mutex<Option<Sampler>>> = Lazy::new(|| std::sync::Mutex::new(None));

/// lib.rs setup 调用：spawn 调度线程（每秒一拍）。总授权关闭时本线程是**惯性**的：
/// 每拍只读一行 settings，不写账本、不影响旧 usage 采集的启动时序。
pub fn spawn_runtime() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(1));
        if let Err(e) = runtime_step() {
            log::warn!("tps: 调度拍失败（下拍重试）：{}", e.detail);
        }
    });
}

/// 一拍：总授权边沿处理 + 租约驱动 + 到期动作执行 + 慢节奏清理。
/// 单拍失败只记日志（采集/建点都有幂等重试路径）。
fn runtime_step() -> Result<(), UsageError> {
    let enabled = crate::services::usage::settings::load().enabled;
    let now = crate::services::usage::now_ms();

    // —— 边沿 + 到期动作摘取（持 SAMPLER 锁，无 IO）——
    let outcome = {
        let mut guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
        if enabled && guard.is_none() {
            *guard = Some(Sampler::new(generate_run_id(), true));
            log::info!(
                "tps: 调度器启动（runId={}）",
                guard.as_ref().unwrap().run_id()
            );
        }
        let sampler = match guard.as_mut() {
            Some(s) => s,
            None => return Ok(()), // 从未开启：惯性拍
        };
        sampler.set_enabled(enabled, &generate_run_id());
        // 视图可见性完全由租约承载（前端 heartbeat 决定续约/交还；tick 不另设全局旗标）
        sampler.tick(now, false)
    }; // SAMPLER 锁在此释放（接收器启停有 DB IO，绝不持 SAMPLER 锁做）

    // —— 接收器生命周期跟总授权走（Spec §2「关闭时停止TPS调度/接收」）——
    if enabled {
        crate::services::tps::receiver::ensure_started();
    } else {
        crate::services::tps::receiver::shutdown();
        return Ok(());
    }

    if !enabled {
        return Ok(());
    }

    // —— 动作执行（短临界区持 DB 锁；单飞失败让路，下拍重试）——
    let run_id = {
        let guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|s| s.run_id().to_string())
            .unwrap_or_default()
    };
    for action in &outcome.actions {
        match action {
            TickAction::Read => {
                if try_begin_round() {
                    // 三段取锁：源 IO（阶段 2）期间不占全局 DB 锁（独立复核 R3）
                    let cursors = {
                        let conn = crate::database::connection::DB
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        load_cursors(&conn)
                    };
                    let reads = read_sources(&runtime_home(), now, cursors);
                    {
                        let mut conn = crate::database::connection::DB
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        commit_reads(&mut conn, reads);
                    }
                    end_round();
                } else {
                    log::debug!("tps: 采集轮已在飞行（手动刷新占用），本拍让路");
                }
            }
            TickAction::EmitPoint {
                at_ms,
                trigger,
                cadence_ms,
            } => {
                let mut conn = crate::database::connection::DB
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let n = produce_point_with_cadence(
                    &mut conn,
                    &run_id,
                    *at_ms,
                    *trigger,
                    Some(*cadence_ms),
                )?;
                if n > 0 {
                    log::debug!("tps: {} 点落库（{} series）", trigger_str(*trigger), n);
                }
            }
        }
    }

    // —— 慢节奏清理（与动作同一拍顺手做；节奏闸在 maybe_purge 内部）——
    let conn = crate::database::connection::DB
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let deleted = maybe_purge(&conn, now)?;
    if deleted > 0 {
        log::info!("tps: 24h 保留清理删除 {deleted} 行");
    }
    Ok(())
}

fn trigger_str(t: PointTrigger) -> &'static str {
    match t {
        PointTrigger::Startup => "startup",
        PointTrigger::Scheduled => "scheduled",
        PointTrigger::Manual => "manual",
    }
}

// ==== IPC 命令的消费口（薄封装；命令层见 commands/tps.rs）====

/// 视图心跳落进共享调度器（命令层直通；无 IO、纯状态）
pub fn heartbeat_view(view_id: &str, visible: bool) {
    let now = crate::services::usage::now_ms();
    let mut guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = guard.as_mut() {
        s.heartbeat(view_id, visible, now);
    }
}

/// 当前调度模式（无调度器 = Disabled 语义）
pub fn current_mode() -> crate::services::tps::scheduler::SamplerMode {
    let now = crate::services::usage::now_ms();
    let guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .map(|s| s.mode_at(now))
        .unwrap_or(crate::services::tps::scheduler::SamplerMode::Disabled)
}

/// manual 刷新的执行编排：限频摘动作 → 采集轮（单飞让路）→ 建点。
/// 返回 (是否执行, 未执行原因, 写入 series 数)。
pub fn manual_refresh_execute() -> Result<(bool, Option<&'static str>, usize), UsageError> {
    let now = crate::services::usage::now_ms();
    let (run_id, actions) = {
        let mut guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = guard.as_mut() else {
            return Ok((false, Some("disabled"), 0));
        };
        match s.manual_refresh(now) {
            None => return Ok((false, Some("rate-limited"), 0)),
            Some(actions) => (s.run_id().to_string(), actions),
        }
    };
    let mut written = 0usize;
    for action in actions {
        match action {
            TickAction::Read => {
                if !try_begin_round() {
                    return Ok((false, Some("busy"), 0));
                }
                // 三段取锁：源 IO 期间不占全局 DB 锁（独立复核 R3）
                let cursors = {
                    let conn = crate::database::connection::DB
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    load_cursors(&conn)
                };
                let reads = read_sources(&runtime_home(), now, cursors);
                {
                    let mut conn = crate::database::connection::DB
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    commit_reads(&mut conn, reads);
                }
                end_round();
            }
            TickAction::EmitPoint {
                at_ms,
                trigger,
                cadence_ms,
            } => {
                let mut conn = crate::database::connection::DB
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                written += produce_point_with_cadence(
                    &mut conn,
                    &run_id,
                    at_ms,
                    trigger,
                    Some(cadence_ms),
                )?;
            }
        }
    }
    Ok((true, None, written))
}

// ==== 以下为编排层的贴身单测（隔离失败 / 单飞闸的纯函数面；集成面在 tests/）====
#[cfg(test)]
mod tests {
    use super::*;

    /// 轮级单飞闸：进入后他人进不来，退出后恢复
    #[test]
    fn round_flight_gate_is_mutually_exclusive() {
        assert!(try_begin_round(), "空闲时必须能进入");
        assert!(!try_begin_round(), "飞行中他人不得进入（单飞）");
        end_round();
        assert!(try_begin_round(), "退出后可再次进入");
        end_round();
    }
}
