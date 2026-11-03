//! TPS 查询面（Spec §6「查询不发模型请求，不运行完整旧采集」；Task 7）。
//!
//! ## 职责与边界
//!
//! * **只读**：快照与曲线只读 `tps_observation` / `tps_point` / `tps_state`（索引尾查），
//!   不触发任何采集/扫描/模型请求（Spec §2「查询不触发采集」）。
//! * **总授权复用**：`usage.enabled` 关闭 → 返回 `disabled=true` 的空载荷，**不删历史**
//!   （Spec §2「返回disabled但不删历史」）；不新增第二个总开关。
//! * **真实来源状态**：快照的 `sourceStates` 一律来自存储（`source_state:<source>` 键）
//!   与注册来源枚举——**绝不**透出 policy::snapshot 的占位形态（那会伪造 ok=true）；
//!   从未读过 = 未配置（unconfigured），照实上板。
//! * **状态分类**：[`classify_source_status`] 把存储态折算成五态粗分类 + 读失败单列，
//!   以机器可解析的 `tps-status=` 前缀**前置**进 `SourceState.detail`——Spec §6 的
//!   `TpsSnapshot` 形状固定（`{series, sourceStates}`），不添加旁路字段；codex 的
//!   `insufficient-generation-timing` 与 opencode 的 `status=` 原文原样保留在前缀之后。
//! * **known_series**：近 24h 已落库点的 series 去重（Spec §2「对近24小时已知series
//!   …分别产点」）；曲线只按 view_range 尾查点表，runId/trigger/status 字段原样透出
//!   供前端断线（Spec §2「不同runId、显式gap、缺失的计划采样点均断线」）。

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::services::tps::install;
use crate::services::tps::model::{
    SeriesKey, SourceCursor, SourceState, TpsPoint, TpsSelection, TpsSnapshot,
};
use crate::services::tps::store;
use crate::services::usage::error::UsageError;

/// 当前值时效与窗长等口径常量在 policy 层（本层引用，不另设）
use crate::services::tps::store::TPS_RETENTION_MS;

/// 来源状态粗分类前缀（机器可解析；追加在 SourceState.detail 之首）
pub const TPS_STATUS_PREFIX: &str = "tps-status=";
/// 持久安装事实的机器 token（独立复核 R10）：可安装来源（claude / opencode）的 detail 在
/// 状态前缀之后追加 `tps-installed=<true|false>`——来自 tps_state 安装记录，不随短期采样
/// 状态（active / read-failed）或前端组件内存变化；前端据此给出卸载入口、隐藏安装入口。
pub const TPS_INSTALLED_PREFIX: &str = "tps-installed=";

/// 来源状态粗分类（Spec §2「源status分别保留未配置/待重启/无样本/字段不足/有效」；
/// 读失败/端口冲突按 Spec §5「报状态」单列 [`TpsSourceStatus::ReadFailed`]）。
/// 序列化 kebab-case，供 `tps-status=` 前缀与前端解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TpsSourceStatus {
    /// 从未产出读取批（源从未被读到 / 接收器或观察插件未配置）
    Unconfigured,
    /// 已安装统计键但尚未收到任何遥测（Spec §5「下次启动生效」的等待态）
    AwaitRestart,
    /// 已读到但没有任何有效样本（含：有事实全失败/缺计时）
    NoSample,
    /// 字段不足（codex 的 insufficient-generation-timing 族缺口）
    InsufficientTiming,
    /// 读取失败 / 接收器端口冲突（sourceState.ok=false，detail 携带定位前缀）
    ReadFailed,
    /// 有效（近 24h 存在 TPS 有效事实）
    Active,
}

impl TpsSourceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TpsSourceStatus::Unconfigured => "unconfigured",
            TpsSourceStatus::AwaitRestart => "await-restart",
            TpsSourceStatus::NoSample => "no-sample",
            TpsSourceStatus::InsufficientTiming => "insufficient-timing",
            TpsSourceStatus::ReadFailed => "read-failed",
            TpsSourceStatus::Active => "active",
        }
    }
}

/// 来源的分类输入快照（纯函数判据面；查询层从存储聚合，测试直构）
pub struct SourceFacts {
    /// tps_state 里该源的最近一次来源状态（从未成功读批 = None）
    pub state: Option<SourceState>,
    /// 近 24h 该源的事实总数（含失败/取消/缺计时）
    pub total_facts: i64,
    /// 近 24h 该源的 TPS 有效事实数
    pub valid_facts: i64,
    /// 安装记录是否存在（claude = 统计键已落；opencode = 观察插件已装）
    pub install_recorded: bool,
}

/// 状态分类判据（顺序即优先级；判据表锁在 tests/tps_sampling_test.rs）：
/// 1. `insufficient-generation-timing` 前缀 → InsufficientTiming（codex 恒空批，
///    必须先于「无事实」判定，否则缺口被吞成无样本）；
/// 2. ok=false → ReadFailed（端口冲突/读取失败如实单列，detail 已带定位前缀）；
/// 3. claude 已装且零遥测 → AwaitRestart（「下次启动生效」等待态；有无读取批都算——
///    接收器在跑而未收到遥测同样是等待态）；
/// 4. 无状态 → Unconfigured（从未产出读取批）；
/// 5. 无事实：claude/opencode 未装 → Unconfigured；原生只读源 → NoSample（已读无样本）；
/// 6. 有有效事实 → Active；有事实全无效 → NoSample。
pub fn classify_source_status(
    source: &str,
    state: &Option<SourceState>,
    total_facts: i64,
    valid_facts: i64,
    install_recorded: bool,
) -> TpsSourceStatus {
    if let Some(st) = state {
        if st
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("insufficient-generation-timing"))
        {
            return TpsSourceStatus::InsufficientTiming;
        }
        if !st.ok {
            return TpsSourceStatus::ReadFailed;
        }
    }
    // 已安装统计配置/观察插件但尚无任何事实 = 等待宿主重启（Spec §5「下次启动生效」）；
    // 对 opencode 同样适用（独立复核 R10/R2：此前只对 claude 判，opencode 落 unconfigured
    // → UI 继续显示安装按钮，重复安装覆盖所有权记录）
    if (source == "claude" || source == "opencode") && install_recorded && total_facts == 0 {
        return TpsSourceStatus::AwaitRestart;
    }
    if state.is_none() {
        return TpsSourceStatus::Unconfigured;
    }
    if total_facts == 0 {
        return match source {
            "claude" | "opencode" => TpsSourceStatus::Unconfigured,
            _ => TpsSourceStatus::NoSample,
        };
    }
    if valid_facts > 0 {
        TpsSourceStatus::Active
    } else {
        TpsSourceStatus::NoSample
    }
}

/// 用 `tps-status=` 前缀把粗分类前置进 detail；可安装来源再追加 `tps-installed=<bool>`
///（独立复核 R10）；原文全保留在后；空 detail 不留悬挂分隔符
fn detail_with_status(
    status: TpsSourceStatus,
    installed: Option<bool>,
    original: Option<&String>,
) -> Option<String> {
    let mut head = format!("{TPS_STATUS_PREFIX}{}", status.as_str());
    if let Some(installed) = installed {
        head.push_str(&format!("; {TPS_INSTALLED_PREFIX}{installed}"));
    }
    Some(match original {
        Some(d) if !d.is_empty() => format!("{head}; {d}"),
        _ => head,
    })
}

// ==== view_range（Spec §6「view_range仅2m/10m/1h/today」；IPC 串逐字）====

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TpsViewRange {
    #[serde(rename = "2m")]
    Last2m,
    #[serde(rename = "10m")]
    Last10m,
    #[serde(rename = "1h")]
    Last1h,
    #[serde(rename = "today")]
    Today,
}

impl TpsViewRange {
    /// 查询起点（`sampled_at > since` 的严格下界）：2m/10m/1h 固定回看；today = 本地当日零点
    pub fn since_ms(&self, now_ms: i64) -> i64 {
        match self {
            TpsViewRange::Last2m => now_ms - 120_000,
            TpsViewRange::Last10m => now_ms - 600_000,
            TpsViewRange::Last1h => now_ms - 3_600_000,
            TpsViewRange::Today => local_midnight_ms(now_ms),
        }
    }
}

/// 本地当日零点（epoch ms）。chrono Local 时区换算；极界时间戳拿不到本地时刻时
/// 回落 UTC 当日零点（保守近似：区间只会偏大，不会漏点）
fn local_midnight_ms(now_ms: i64) -> i64 {
    use chrono::TimeZone;
    let midnight = chrono::Local
        .timestamp_millis_opt(now_ms)
        .single()
        .and_then(|now| now.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|d| chrono::Local.from_local_datetime(&d).single());
    match midnight {
        Some(dt) => dt.timestamp_millis(),
        None => now_ms - now_ms.rem_euclid(86_400_000),
    }
}

// ==== 查询载荷 ====

/// tps_snapshot 的命令载荷（Spec §2「返回disabled」的载体：disabled=true 时
/// snapshot 为显式 null，历史不动）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsSnapshotResult {
    pub disabled: bool,
    pub snapshot: Option<TpsSnapshot>,
}

/// tps_points 的命令载荷（known_series 供前端对齐空窗；runId/trigger/status 在点内）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsPointsResult {
    pub disabled: bool,
    pub known_series: Vec<SeriesKey>,
    pub points: Vec<TpsPoint>,
    pub range_since_ms: i64,
}

/// 快照（Spec §6）：近 24h 事实的 series 快照 + 全部注册来源的真实状态。
/// 总授权关闭 → disabled 载荷（不读历史、不删历史）。
pub fn snapshot(
    conn: &Connection,
    selection: &TpsSelection,
    now_ms: i64,
) -> Result<TpsSnapshotResult, UsageError> {
    if !crate::services::usage::settings::load_from_conn(conn).enabled {
        return Ok(TpsSnapshotResult {
            disabled: true,
            snapshot: None,
        });
    }
    let facts = store::load_recent(conn, now_ms - TPS_RETENTION_MS)?;
    let mut snap = crate::services::tps::policy::snapshot(&facts, selection, now_ms);
    // 真实来源状态覆写（policy 层的占位 sourceStates 绝不上板——见模块头）
    snap.source_states = real_source_states(conn, &facts, now_ms)?;

    Ok(TpsSnapshotResult {
        disabled: false,
        snapshot: Some(snap),
    })
}

/// 全部注册来源的真实状态（存储态 + 分类前缀；从未读过 = unconfigured）
fn real_source_states(
    conn: &Connection,
    facts: &[crate::services::tps::model::TpsObservation],
    _now_ms: i64,
) -> Result<Vec<SourceState>, UsageError> {
    // 每源的事实计数（一次遍历分桶）
    let mut total_by: BTreeMap<&str, i64> = BTreeMap::new();
    let mut valid_by: BTreeMap<&str, i64> = BTreeMap::new();
    for o in facts {
        *total_by.entry(o.source.as_str()).or_insert(0) += 1;
        if o.is_tps_valid() {
            *valid_by.entry(o.source.as_str()).or_insert(0) += 1;
        }
    }
    let mut out = Vec::with_capacity(crate::services::tps::collector::REGISTERED_SOURCES.len());
    for source in crate::services::tps::collector::REGISTERED_SOURCES {
        let stored = store::load_source_state(conn, source)?;
        let install_recorded = install_recorded(conn, source)?;
        let status = classify_source_status(
            source,
            &stored,
            total_by.get(source).copied().unwrap_or(0),
            valid_by.get(source).copied().unwrap_or(0),
            install_recorded,
        );
        // 可安装来源带持久安装事实 token；其余来源不带（None）
        let installed = matches!(*source, "claude" | "opencode").then_some(install_recorded);
        let state = match stored {
            Some(mut st) => {
                st.detail = detail_with_status(status, installed, st.detail.as_ref());
                st
            }
            None => SourceState {
                source: (*source).to_string(),
                cursor: SourceCursor::empty(source),
                ok: true,
                detail: detail_with_status(status, installed, None),
            },
        };
        out.push(state);
    }
    Ok(out)
}

/// 安装记录是否在册（显式 null = 已了结 → false；安装记录键语义见 install.rs）
fn install_recorded(conn: &Connection, source: &str) -> Result<bool, UsageError> {
    if source != "claude" && source != "opencode" {
        return Ok(false);
    }
    Ok(store::load_state(conn, &install::record_state_key(source))?.is_some_and(|v| !v.is_null()))
}

/// 曲线查询（Spec §6）：view_range 尾查点表 + selection 过滤 + known_series（近 24h）。
/// 只读——不建点、不补点（gap 点由调度器建好落库后自然出现在曲线上）。
pub fn points(
    conn: &Connection,
    selection: &TpsSelection,
    range: TpsViewRange,
    now_ms: i64,
) -> Result<TpsPointsResult, UsageError> {
    if !crate::services::usage::settings::load_from_conn(conn).enabled {
        return Ok(TpsPointsResult {
            disabled: true,
            known_series: Vec::new(),
            points: Vec::new(),
            range_since_ms: range.since_ms(now_ms),
        });
    }
    let since = range.since_ms(now_ms);
    let points: Vec<TpsPoint> = store::load_points_since(conn, since)?
        .into_iter()
        .filter(|p| selection.matches_series(&p.series_key))
        .collect();
    // known_series = 近 24h 已落库点的 series 去重（BTreeMap 迭代序确定）
    let mut known: BTreeMap<String, SeriesKey> = BTreeMap::new();
    for p in store::load_points_since(conn, now_ms - TPS_RETENTION_MS)? {
        known.entry(p.series_key.id()).or_insert(p.series_key);
    }
    Ok(TpsPointsResult {
        disabled: false,
        known_series: known.into_values().collect(),
        points,
        range_since_ms: since,
    })
}

// ==== 以下为查询层的贴身单测（判据面锁在 tests/tps_sampling_test.rs，此处只锁换算）====
#[cfg(test)]
mod tests {
    use super::*;

    /// view_range 串形态逐字锁（IPC 契约「2m|10m|1h|today」）
    #[test]
    fn view_range_wire_forms_are_verbatim() {
        assert_eq!(
            serde_json::to_string(&TpsViewRange::Last2m).unwrap(),
            "\"2m\""
        );
        assert_eq!(
            serde_json::to_string(&TpsViewRange::Last10m).unwrap(),
            "\"10m\""
        );
        assert_eq!(
            serde_json::to_string(&TpsViewRange::Last1h).unwrap(),
            "\"1h\""
        );
        assert_eq!(
            serde_json::to_string(&TpsViewRange::Today).unwrap(),
            "\"today\""
        );
        // 反序列化（前端回传）同串可读回
        let back: TpsViewRange = serde_json::from_str("\"10m\"").unwrap();
        assert_eq!(back, TpsViewRange::Last10m);
    }

    /// 固定回看的起点换算
    #[test]
    fn fixed_ranges_subtract_verbatim_windows() {
        let now = 1_700_000_000_000;
        assert_eq!(TpsViewRange::Last2m.since_ms(now), now - 120_000);
        assert_eq!(TpsViewRange::Last10m.since_ms(now), now - 600_000);
        assert_eq!(TpsViewRange::Last1h.since_ms(now), now - 3_600_000);
    }

    /// tps-status= 前缀组装：原文保留、空 detail 不留悬挂分隔符；
    /// 独立复核 R10：可安装来源追加持久安装事实 token `tps-installed=<bool>`，非可安装来源不带
    #[test]
    fn status_prefix_keeps_original_detail() {
        let d = detail_with_status(
            TpsSourceStatus::Active,
            None,
            Some(&"status=supported; …".to_string()),
        )
        .unwrap();
        assert_eq!(d, "tps-status=active; status=supported; …");
        let d = detail_with_status(TpsSourceStatus::Unconfigured, None, None).unwrap();
        assert_eq!(d, "tps-status=unconfigured");
        let d = detail_with_status(
            TpsSourceStatus::Active,
            Some(true),
            Some(&"status=supported".to_string()),
        )
        .unwrap();
        assert_eq!(d, "tps-status=active; tps-installed=true; status=supported");
        let d = detail_with_status(TpsSourceStatus::Unconfigured, Some(false), None).unwrap();
        assert_eq!(d, "tps-status=unconfigured; tps-installed=false");
    }

    /// 独立复核 R10/R2：已安装但尚无事实的 OpenCode 与 Claude 一样是 await-restart，
    /// 不得再归 unconfigured（否则 UI 继续给安装按钮、重复安装）
    #[test]
    fn installed_without_facts_is_await_restart_for_opencode_too() {
        let st = Some(SourceState {
            source: "opencode".into(),
            cursor: SourceCursor::empty("opencode"),
            ok: true,
            detail: Some("status=supported".into()),
        });
        assert_eq!(
            classify_source_status("opencode", &st, 0, 0, true),
            TpsSourceStatus::AwaitRestart
        );
        assert_eq!(
            classify_source_status("opencode", &st, 0, 0, false),
            TpsSourceStatus::Unconfigured
        );
        assert_eq!(
            classify_source_status("claude", &None, 0, 0, true),
            TpsSourceStatus::AwaitRestart
        );
    }
}
