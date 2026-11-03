//! V2 速度 dashboard 查询面（计划 Task 6：`query::dashboard(conn,query,now)`）。
//!
//! ## 职责与边界
//!
//! * **只读**：只查 `tps_observation` / `tps_state` / `settings`——绝不调任何来源读取器、
//!   旧采集（`tps_refresh`/`read_sources`/`collector`）或工具根文件系统（Spec §2「查询
//!   不触发源扫描或模型请求」）；`range_to = now_ms`（快照 asOf），`range_from =
//!   now_ms − window`。
//! * **完成时刻归属**：观察按 `ended_at_ms` 落进互不重叠的半开完成窗 `(from, to]`
//!   （Spec §7「按完成时刻归属，首尾裁剪」）——跨起点的整条调用按**完成**归属一次。
//! * **桶对齐**：桶边界按**本地墙钟**对齐到桶宽整倍数（Spec §7「滚动范围按本地墙钟
//!   对齐桶」）；[`bucket_spans`] 只解一次本机偏移（`time_price.rs` 先例），几何数学
//!   全在 offset 钉死的 [`bucket_spans_with_offset`]（测试不依赖机器时区）。
//! * **旧点无关**：只聚合 `tps_observation` 的稳定去重事实；`tps_point` 旧滚动点绝不
//!   进区间/桶求和，不截「最近 20 条」，不为空闲源清历史（Spec §7）。
//! * **选择**：`query.selection` 只在筛选后的 series 集中保留，否则取排序第一项，
//!   无匹配为 None；桶只为选中 series 计算。
//! * **来源状态**：每注册来源一条 [`SpeedSourceView`]——安装事实独立于样本；
//!   `NeedsCheck` = 最近读取失败（ok=false）或 `parser-errors=` 前缀（Task 2 契约，
//!   仅此精确前缀）；旧 Codex 流式缺口前缀**不**降级状态（Codex Ready + stream=false，
//!   Spec §8.6「不能继续把它整体标为不支持」）。
//! * **总授权**：`usage.enabled == false` ⇒ `disabled = true`、空 series/buckets、
//!   选择 None（历史不删；来源状态仍解析——安装事实不依赖样本）。

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::database::dao::speed as speed_dao;
use crate::services::tps::collector::REGISTERED_SOURCES;
use crate::services::tps::install;
use crate::services::tps::model::SeriesKey;
use crate::services::tps::store;
use crate::services::tps::v2::model::{
    BucketSpan, SpeedBucket, SpeedCapabilities, SpeedDashboardQuery, SpeedDashboardSnapshot,
    SpeedMetrics, SpeedSeriesSummary, SpeedSourceStatus, SpeedSourceView,
};
use crate::services::tps::v2::sums::{derive_metrics, SpeedSums};
use crate::services::usage::error::UsageError;

/// 解析失败原因的稳定机器 token（SpeedSourceView.reason；前端 i18n 映射）
const REASON_NOT_INSTALLED: &str = "not-installed";
const REASON_NEVER_READ: &str = "never-read";
const REASON_READ_FAILED: &str = "read-failed";
const REASON_PARSER_ERRORS: &str = "parser-errors";
const REASON_AWAIT_RESTART: &str = "await-restart";

/// 来源状态 detail 的解析错误前缀（Task 2 契约：workbuddy 以 `parser-errors=<n>` 起头
/// 表示本批有解析拒绝；只认这个精确前缀，其余 detail 一律不据此判 NeedsCheck）
const PARSER_ERRORS_PREFIX: &str = "parser-errors=";

// ==== 桶几何 ====

/// 本机偏移下把 t 向下对齐到「本地墙钟桶宽整倍数」：`(t + offset) mod width` 的
/// 整倍数回退。`offset_seconds` = 本机 UTC 偏移（`bucket_spans` 解析一次）。
fn align_down_ms(t_ms: i64, width_ms: i64, offset_seconds: i32) -> i64 {
    let off_ms = offset_seconds as i64 * 1000;
    (t_ms + off_ms).div_euclid(width_ms) * width_ms - off_ms
}

/// 本机 UTC 偏移秒（在 `at_ms` 时刻解析一次；歧义/极界回落 UTC=0，保守不偏移）
fn local_offset_seconds(at_ms: i64) -> i32 {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(at_ms)
        .single()
        .map(|dt| dt.offset().local_minus_utc())
        .unwrap_or(0)
}

/// 桶几何（公共入口）：在 `now` 时刻解析一次本机偏移后委托
/// [`bucket_spans_with_offset`]。测试应使用 offset 钉死的版本，不依赖机器时区。
pub fn bucket_spans(
    from_ms: i64,
    to_ms: i64,
    range: crate::services::tps::v2::model::SpeedRange,
) -> Vec<BucketSpan> {
    bucket_spans_with_offset(from_ms, to_ms, range, local_offset_seconds(to_ms))
}

/// 桶几何（offset 钉死的纯函数）：
///
/// * 桶宽 = `range.bucket_width_ms()`；边界对齐到本地墙钟整倍数（`(t+offset) ≡ 0 (mod width)`）；
/// * span 为半开区间 `(start, end]`，与完成时刻归属同口径（互不重叠、无缝覆盖）；
/// * 含 `from` 的首桶与含 `to` 的末桶**裁剪**到 `[from, to]` 并 carry
///   `is_partial_bucket = true`；中间桶恒满宽；
/// * 桶数 ≤ 名义桶数 + 1（边缘部分桶最多额外 1 个，Spec §7）；
/// * 绝不产出 `[from, to]` 之外的 span（不向范围外捞数据补满桶）；
/// * 退化区间（`to <= from`）返回空。
pub fn bucket_spans_with_offset(
    from_ms: i64,
    to_ms: i64,
    range: crate::services::tps::v2::model::SpeedRange,
    offset_seconds: i32,
) -> Vec<BucketSpan> {
    let width = range.bucket_width_ms();
    if to_ms <= from_ms {
        return Vec::new();
    }
    let anchor = align_down_ms(from_ms, width, offset_seconds);
    let mut spans = Vec::new();
    let mut start = anchor;
    while start < to_ms {
        let mut end = start + width;
        let mut partial = false;
        if start < from_ms {
            // 首桶：自然起点在 from 之前 → 裁剪到 from
            start = from_ms;
            partial = true;
        }
        if end > to_ms {
            // 末桶：自然终点在 to 之后 → 裁剪到 to
            end = to_ms;
            partial = true;
        }
        spans.push(BucketSpan {
            start_ms: start,
            end_ms: end,
            is_partial_bucket: partial,
        });
        start = end;
    }
    // 防御断言（数学不变量；违背即实现错了，宁可 panic 也不出脏几何）
    debug_assert!(
        spans.len() <= ((to_ms - from_ms) / width + 1) as usize,
        "部分桶最多 +1"
    );
    spans
}

// ==== 排序与选择 ====

/// 当前模式的速度值（rate 排序依据）
fn mode_value(
    m: &SpeedMetrics,
    mode: crate::services::tps::v2::model::SpeedMetricMode,
) -> Option<f64> {
    use crate::services::tps::v2::model::SpeedMetricMode as Mode;
    match mode {
        Mode::Stream => m.stream.value,
        Mode::Response => m.response.value,
    }
}

/// 稳定回退序：tool → model → channel（Spec §8.1「默认工具→模型→渠道稳定排序」）
/// 比较引擎备注（scoped 评审 ④）：Rust 按字节序、前端 TS `view.ts` 按 UTF-16 码元，
/// 仅对纯 ASCII 标识符等价——现实 tool/model/channel 键均为 ASCII；未来若出现非
/// ASCII 键，两引擎并列序可能不同（已知近似，注释即契约）。
fn fallback_cmp(
    a: (&str, &str, Option<&str>),
    b: (&str, &str, Option<&str>),
) -> std::cmp::Ordering {
    a.0.cmp(b.0)
        .then_with(|| a.1.cmp(b.1))
        .then_with(|| a.2.cmp(&b.2))
}

/// 列表排序（`sort` 逐字：tool | model | rate；rate 按当前模式值**降序**，无数值排末）
fn sort_series(
    entries: &mut [(SeriesKey, SpeedMetrics)],
    sort: crate::services::tps::v2::model::SpeedSort,
    mode: crate::services::tps::v2::model::SpeedMetricMode,
) {
    use crate::services::tps::v2::model::SpeedSort as Sort;
    entries.sort_by(|(ka, ma), (kb, mb)| match sort {
        Sort::Tool => fallback_cmp(
            (&ka.tool, &ka.model, ka.channel.as_deref()),
            (&kb.tool, &kb.model, kb.channel.as_deref()),
        ),
        Sort::Model => fallback_cmp(
            (&ka.model, &ka.tool, ka.channel.as_deref()),
            (&kb.model, &kb.tool, kb.channel.as_deref()),
        ),
        Sort::Rate => {
            let ord = match (mode_value(ma, mode), mode_value(mb, mode)) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater, // null 排末
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal), // 降序
            };
            ord.then_with(|| {
                fallback_cmp(
                    (&ka.tool, &ka.model, ka.channel.as_deref()),
                    (&kb.tool, &kb.model, kb.channel.as_deref()),
                )
            })
        }
    });
}

// ==== 来源能力与状态 ====

/// Codex stream 缺口的稳定机器 token（errcode 棘轮 + 评审 Minor：运行时串一律 ASCII
/// token，不写中文描述；人读解释在本注释——无同调用流式生成窗，仅日志近似响应窗
/// （log-boundary-approx，含等待）可用。前端 Task 9/11 把已知 token 映射进 i18n，
/// 未知 token 原样进技术详情）。
const CODEX_STREAM_GAP_REASON: &str = "stream-gap:insufficient-generation-timing";

/// 静态能力表（Spec §5.2「来源能力」逐行）：**结构性**计算路径，与安装/样本无关——
/// 「支持」是计算路径不是数据保证（未安装的来源也照报）；Codex 是唯一 stream 缺失源
/// （无同调用流式生成窗，仅日志近似响应窗）。
fn static_capabilities(source: &str) -> SpeedCapabilities {
    if source == "codex" {
        SpeedCapabilities {
            stream: false,
            response: true,
            stream_reason: Some(CODEX_STREAM_GAP_REASON.to_string()),
            response_reason: None,
        }
    } else {
        // dsh / zcode / kimi / workbuddy / claude / opencode：双指标均具备计算路径
        SpeedCapabilities {
            stream: true,
            response: true,
            stream_reason: None,
            response_reason: None,
        }
    }
}

/// 有安装机制的来源（仅 claude/opencode；其余只读来源结构上恒在册，无「未安装」形态）。
fn has_install_mechanism(source: &str) -> bool {
    source == "claude" || source == "opencode"
}

/// 安装记录是否在册（显式 null = 已了结 → false；仅 claude/opencode 有安装机制，
/// query.rs 同款判据）
fn install_recorded(conn: &Connection, source: &str) -> Result<bool, UsageError> {
    if !has_install_mechanism(source) {
        return Ok(false);
    }
    Ok(store::load_state(conn, &install::record_state_key(source))?.is_some_and(|v| !v.is_null()))
}

/// 状态折算判据（次序即优先级；矩阵锁在 tests/tps_speed_dashboard_test.rs）：
/// 未安装 → Unconfigured；最近状态 ok=false → NeedsCheck；detail 以 `parser-errors=`
/// 起头 → NeedsCheck（Task 2 契约，仅此精确前缀）；安装机制来源已装且区间零事实 →
/// AwaitRestart（「下次启动生效」等待态）；从未产出读取批 → Unconfigured（never-read）；
/// 其余 → Ready（有无数据均算——「无区间记录是数据空态，不自动等于采集故障」）。
/// 旧 Codex `insufficient-generation-timing` 前缀**不**在此折算（不降级整体状态：
/// Codex = Ready + stream=false，能力位承载缺口，Spec §8.6）。
/// 全部注册来源各出一条视图（安装事实不依赖样本，总授权关闭时照常解析）。
fn resolve_source_views(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<SpeedSourceView>, UsageError> {
    let stats = speed_dao::source_record_stats_conn(conn, from_ms, to_ms)?;
    let mut out = Vec::with_capacity(REGISTERED_SOURCES.len());
    for source in REGISTERED_SOURCES {
        let installed = if has_install_mechanism(source) {
            install_recorded(conn, source)?
        } else {
            // 无安装机制的只读来源：结构上恒在册（无「未安装」形态）
            true
        };
        let stored = store::load_source_state(conn, source)?;
        let (facts, last_record_at) = stats.get(*source).copied().unwrap_or((0, None));
        let (status, reason) = if !installed {
            (SpeedSourceStatus::Unconfigured, Some(REASON_NOT_INSTALLED))
        } else if stored.as_ref().is_some_and(|st| !st.ok) {
            (SpeedSourceStatus::NeedsCheck, Some(REASON_READ_FAILED))
        } else if stored.as_ref().is_some_and(|st| {
            st.detail
                .as_deref()
                .is_some_and(|d| d.starts_with(PARSER_ERRORS_PREFIX))
        }) {
            (SpeedSourceStatus::NeedsCheck, Some(REASON_PARSER_ERRORS))
        } else if has_install_mechanism(source) && facts == 0 {
            // 已装统计键但区间零事实 = 等待宿主重启（query.rs 同款判据，Spec §5）
            (SpeedSourceStatus::AwaitRestart, Some(REASON_AWAIT_RESTART))
        } else if stored.is_none() {
            (SpeedSourceStatus::Unconfigured, Some(REASON_NEVER_READ))
        } else {
            (SpeedSourceStatus::Ready, None)
        };
        out.push(SpeedSourceView {
            source: (*source).to_string(),
            status,
            installed,
            capabilities: static_capabilities(source),
            last_record_at,
            reason: reason.map(str::to_string),
            detail: stored.and_then(|st| st.detail),
        });
    }
    Ok(out)
}

// ==== dashboard 入口 ====

/// V2 dashboard（契约入口）：列表 / 总览 / 曲线来自**同一事实快照**——区间 series
/// 汇总与选中 series 的桶聚合都出自同一条有效性 CTE（dao/speed.rs），不被
/// 「最近 20 条」或旧 10 分钟规则截断（brief 验收逐字）。
pub fn dashboard(
    conn: &Connection,
    query: &SpeedDashboardQuery,
    now_ms: i64,
) -> Result<SpeedDashboardSnapshot, UsageError> {
    let range_to = now_ms;
    let range_from = (now_ms - query.range.window_ms()).max(0);
    // 来源状态先解析：安装事实不依赖样本（disabled 也照常返回全部视图）
    let source_states = resolve_source_views(conn, range_from, range_to)?;

    if !crate::services::usage::settings::load_from_conn(conn).enabled {
        return Ok(SpeedDashboardSnapshot {
            as_of: now_ms,
            range_from,
            range_to,
            disabled: true,
            source_states,
            series: Vec::new(),
            selected_series: None,
            buckets: Vec::new(),
        });
    }

    // 区间 series 汇总（MainTask 候选在 SQL 内全保留、各口径 CASE 分算）
    let search = (!query.search.is_empty()).then_some(query.search.as_str());
    let rows =
        speed_dao::series_sums_conn(conn, range_from, range_to, query.tool_id.as_deref(), search)?;
    let mut entries: Vec<(SeriesKey, SpeedMetrics)> = rows
        .into_iter()
        .map(|r| (r.key, derive_metrics(&r.sums)))
        .collect();
    sort_series(&mut entries, query.sort, query.metric_mode);

    // 选择：只在筛选后的 series 集保留，否则排序第一，无匹配 None
    let selected_series = match &query.selection {
        Some(sel) if entries.iter().any(|(k, _)| k == sel) => Some(sel.clone()),
        _ => entries.first().map(|(k, _)| k.clone()),
    };

    // 桶只为选中 series 计算（与区间同一 CTE、同一有效性口径）。
    // span 与桶序号一一对应：spans 从同一未裁剪锚点起按整除序连续编号
    // （首桶只是把自然起点裁剪到 from，SQL 的 `(ended−anchor)/width` 对
    // (from, 首自然边界] 的记录仍得 0），故按下标直取。
    let buckets = match &selected_series {
        Some(key) => {
            let width = query.range.bucket_width_ms();
            let spans = bucket_spans(range_from, range_to, query.range);
            let anchor = align_down_ms(range_from, width, local_offset_seconds(now_ms));
            let by_idx: BTreeMap<i64, SpeedSums> =
                speed_dao::bucket_sums_conn(conn, range_from, range_to, anchor, width, key)?
                    .into_iter()
                    .collect();
            let empty = SpeedSums::default();
            spans
                .into_iter()
                .enumerate()
                .map(|(i, span)| SpeedBucket {
                    start_ms: span.start_ms,
                    end_ms: span.end_ms,
                    is_partial_bucket: span.is_partial_bucket,
                    metrics: derive_metrics(by_idx.get(&(i as i64)).unwrap_or(&empty)),
                })
                .collect()
        }
        None => Vec::new(),
    };

    let series = entries
        .into_iter()
        .map(|(series_key, metrics)| SpeedSeriesSummary {
            series_key,
            metrics,
        })
        .collect();

    Ok(SpeedDashboardSnapshot {
        as_of: now_ms,
        range_from,
        range_to,
        disabled: false,
        source_states,
        series,
        selected_series,
        buckets,
    })
}
