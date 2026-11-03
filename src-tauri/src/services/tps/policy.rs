//! TPS 采样口径层：build_point / snapshot 纯函数（Spec §2 固定采样规则、§3 有效性口径）。
//! 纪律：无 IO、无时钟、无全局态——时间一律由入参注入（Spec §7：用假时钟测试，不真等）。

use std::collections::BTreeMap;

use crate::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, Purpose, SeriesKey, SourceCursor, SourceState,
    TimingMethod, TpsMetrics, TpsObservation, TpsPoint, TpsQuality, TpsSelection,
    TpsSeriesSnapshot, TpsSnapshot,
};

/// 采样点回看窗长（Spec §2/§6：120 秒，半开 `(at−120000, at]`，按**完成**时刻落窗）
pub const TPS_WINDOW_MS: i64 = 120_000;
/// 每点每 series 最多取最近 N 条有效调用（Spec §2「最多取最近20条」）
pub const TPS_MAX_SAMPLES: usize = 20;
/// 当前值时效（Spec §2「最新读数超过10分钟不再当当前值」；恰好 10 分钟仍算新鲜）
pub const TPS_CURRENT_FRESH_MS: i64 = 600_000;
/// 小样本标记阈：入样输出 Token 合计低于此值标 smallSample（Spec §3「少量 Token 标小样本」；
/// **暂定阈值**——spec 未固定数值，后续 Task 如需收紧在此改一处）
pub const TPS_SMALL_SAMPLE_OUTPUT_TOKENS: i64 = 50;

/// 空值原因（Spec §6「返回…及空值原因」；`unknown-turn` 为 spec 逐字串）
pub const NULL_REASON_STALE: &str = "stale";
pub const NULL_REASON_UNKNOWN_TURN: &str = "unknown-turn";
pub const NULL_REASON_NO_VALID_SAMPLE: &str = "no-valid-sample";

/// 从观察提取 series 键（Spec §6：tool = 来源 id；model/channel/method 取观察原值）
fn series_key_of(o: &TpsObservation) -> SeriesKey {
    SeriesKey {
        tool: o.source.clone(),
        model: o.model.clone(),
        channel: o.channel.clone(),
        method: o.method,
    }
}

/// 点内去重：同主键（source+host_instance+stable_call_key）只保留一次（Spec §6「一个点内
/// 只一次」/ §7「同调用重复读取不翻倍」）。同键重复出现时保留**后到**的观察（重读更新鲜）。
fn dedup_by_call_key(observations: &[TpsObservation]) -> Vec<TpsObservation> {
    let mut dedup: BTreeMap<(&str, &str, &str), &TpsObservation> = BTreeMap::new();
    for o in observations {
        dedup.insert(o.call_key(), o);
    }
    dedup.into_values().cloned().collect()
}

/// TPS 入样池：有效调用按 (结束时刻, 稳定键) 升序（稳定键破平局保证确定性）。
/// `restrict_main_task` = true 时再滤掉非主任务用途（Spec §3「辅助标题/自动摘要……
/// 默认不进入主任务曲线」——点=主任务曲线数据面；snapshot 的最近调用/当前回合不受此限）。
fn sample_pool(candidates: &[TpsObservation], restrict_main_task: bool) -> Vec<&TpsObservation> {
    let mut pool: Vec<&TpsObservation> = candidates.iter().filter(|o| o.is_tps_valid()).collect();
    if restrict_main_task {
        pool.retain(|o| o.purpose == Purpose::MainTask);
    }
    pool.sort_by(|a, b| {
        (a.ended_at_ms, a.stable_call_key.as_str())
            .cmp(&(b.ended_at_ms, b.stable_call_key.as_str()))
    });
    pool
}

/// TTFT 样本池（独立复核 R9；Spec §6「TTFT 只平均成功且首点可对齐的样本，分母
/// ttftSampleCount 独立于 TPS 的 sampleCount」）：准入 = `ttft_ms()` 有值（成功 + 首点
/// 不倒挂），**不要求** TPS 有效（缺输出计数/生成窗的成功调用其首点仍计入）；用途过滤与
/// 排序同 `sample_pool`，调用方取最近 ≤20。
fn ttft_pool(candidates: &[TpsObservation], restrict_main_task: bool) -> Vec<&TpsObservation> {
    let mut pool: Vec<&TpsObservation> = candidates
        .iter()
        .filter(|o| o.ttft_ms().is_some())
        .collect();
    if restrict_main_task {
        pool.retain(|o| o.purpose == Purpose::MainTask);
    }
    pool.sort_by(|a, b| {
        (a.ended_at_ms, a.stable_call_key.as_str())
            .cmp(&(b.ended_at_ms, b.stable_call_key.as_str()))
    });
    pool
}

/// 一次聚合的完整产物：指标面 + 点级合计（TpsMetrics 不含合计字段，点才带）。
struct Agg {
    metrics: TpsMetrics,
    sum_output_tokens: i64,
    sum_generation_ms: i64,
    ttft_sum_ms: Option<i64>,
    /// 入样调用稳定键，最近在前（≤ TPS_MAX_SAMPLES）
    sampled_keys: Vec<String>,
}

/// 聚合候选集。耗时详情计数恒覆盖**全部**候选（Spec §3：含失败/取消；失败缺计时=缺失不填 0）；
/// TPS 样本面只取有效（按需限主任务）最近 ≤20 条（Spec §2）。
/// 无有效样本时样本面为空值、output_scope=Unknown（无事实不谎称 provider-reported）。
fn aggregate(
    candidates: &[TpsObservation],
    method: TimingMethod,
    now_ms: i64,
    restrict_main_task: bool,
) -> Agg {
    // —— 耗时详情计数：全部候选 ——
    let candidate_count = candidates.len() as i64;
    let mut failed_count = 0i64;
    let mut cancelled_count = 0i64;
    let mut timed_model_count = 0i64;
    let mut timed_tool_count = 0i64;
    let mut missing_timing_count = 0i64;
    let mut model_ms_sum = 0i64;
    let mut tool_ms_sum = 0i64;
    for o in candidates {
        match o.outcome {
            Outcome::Failed => failed_count += 1,
            Outcome::Cancelled => cancelled_count += 1,
            Outcome::Success => {}
        }
        match (o.timed_model_ms(), o.timed_tool_ms()) {
            (None, None) => missing_timing_count += 1, // 完全缺计时
            (m, t) => {
                if let Some(v) = m {
                    timed_model_count += 1;
                    model_ms_sum += v;
                }
                if let Some(v) = t {
                    timed_tool_count += 1;
                    tool_ms_sum += v;
                }
            }
        }
    }

    // —— TPS 样本面：最近 ≤20 条有效 ——
    let pool = sample_pool(candidates, restrict_main_task);
    let recent: Vec<&&TpsObservation> = pool.iter().rev().take(TPS_MAX_SAMPLES).collect();

    let sample_count = recent.len() as i64;
    let sum_output_tokens: i64 = recent.iter().map(|o| o.output_tokens.unwrap_or(0)).sum();
    let sum_generation_ms: i64 = recent.iter().map(|o| o.generation_ms.unwrap_or(0)).sum();
    // —— TTFT 样本面：独立分母（成功且首点可对齐的最近 ≤20 条，不绑 TPS 有效性）——
    let ttft_recent = ttft_pool(candidates, restrict_main_task);
    let ttfts: Vec<i64> = ttft_recent
        .iter()
        .rev()
        .take(TPS_MAX_SAMPLES)
        .filter_map(|o| o.ttft_ms())
        .collect();
    let ttft_sample_count = ttfts.len() as i64;
    let ttft_sum_ms: Option<i64> = (!ttfts.is_empty()).then(|| ttfts.iter().sum());
    let avg_first_token_ms =
        ttft_sum_ms.map(|s| (s as f64 / ttft_sample_count as f64).round() as i64);
    // 点内 TPS = 1000×Σ输出/Σ生成毫秒（Spec §2/§7；不平均单请求 TPS）
    let tps = (sum_generation_ms > 0)
        .then(|| 1000.0 * sum_output_tokens as f64 / sum_generation_ms as f64);
    let latest_ended_at = recent.first().map(|o| o.ended_at_ms);
    let age_ms = latest_ended_at.map(|e| (now_ms - e).max(0));
    // 头条样本的包含关系 = 最近一条入样调用的 scope（展示口径：最近值的事实归属）
    let output_scope = recent
        .first()
        .map(|o| o.output_scope)
        .unwrap_or(OutputScope::Unknown);
    let quality = if sample_count > 0 && sum_output_tokens < TPS_SMALL_SAMPLE_OUTPUT_TOKENS {
        TpsQuality::SmallSample
    } else {
        TpsQuality::Normal
    };
    let sampled_keys: Vec<String> = recent.iter().map(|o| o.stable_call_key.clone()).collect();

    Agg {
        metrics: TpsMetrics {
            model_ms: (timed_model_count > 0).then_some(model_ms_sum),
            tool_ms: (timed_tool_count > 0).then_some(tool_ms_sum),
            avg_first_token_ms,
            tps,
            sample_count,
            ttft_sample_count,
            candidate_count,
            timed_model_count,
            timed_tool_count,
            failed_count,
            cancelled_count,
            missing_timing_count,
            latest_ended_at,
            age_ms,
            method,
            output_scope,
            quality,
        },
        sum_output_tokens,
        sum_generation_ms,
        ttft_sum_ms,
        sampled_keys,
    }
}

/// 组装一个采样点（候选须已窗选 + 点内去重）。样本数为 0 → gap 点：
/// 不画 0、不沿用旧速度（Spec §2），但候选分类计数保留。
fn make_point(
    series: &SeriesKey,
    candidates: &[TpsObservation],
    run_id: &str,
    at_ms: i64,
    trigger: PointTrigger,
    cadence_ms: Option<i64>,
) -> TpsPoint {
    let agg = aggregate(candidates, series.method, at_ms, true);
    let m = &agg.metrics;
    let status = if m.sample_count > 0 {
        PointStatus::Value
    } else {
        PointStatus::Gap
    };
    TpsPoint {
        model_ms: m.model_ms,
        tool_ms: m.tool_ms,
        avg_first_token_ms: m.avg_first_token_ms,
        tps: m.tps,
        sample_count: m.sample_count,
        ttft_sample_count: m.ttft_sample_count,
        candidate_count: m.candidate_count,
        timed_model_count: m.timed_model_count,
        timed_tool_count: m.timed_tool_count,
        failed_count: m.failed_count,
        cancelled_count: m.cancelled_count,
        missing_timing_count: m.missing_timing_count,
        latest_ended_at: m.latest_ended_at,
        age_ms: m.age_ms,
        method: m.method,
        output_scope: m.output_scope,
        quality: m.quality,
        sum_output_tokens: agg.sum_output_tokens,
        sum_generation_ms: agg.sum_generation_ms,
        ttft_sum_ms: agg.ttft_sum_ms,
        // 点 id = tps_point 表 tick_id：run + 采样时刻 + series 内唯一
        point_id: format!("{}:{}:{}", run_id, at_ms, series.id()),
        series_key: series.clone(),
        sampled_at: at_ms,
        window_ms: TPS_WINDOW_MS,
        run_id: run_id.to_string(),
        trigger,
        status,
        cadence_ms,
        sampled_keys: agg.sampled_keys,
    }
}

/// 按固定采样规则生成一个采样时刻的全部点（Spec §2）：
/// 窗 = end ∈ `(at−120000, at]`（跨开始边界的调用整条纳入，不裁切不分摊）；每 series
/// 取最近 ≤20 条有效；空窗仍为 known_series 产出 gap 点；同 run 同 series 的稀疏点可相连，
/// gap 显式断线。点的 trigger 默认 Scheduled（startup/manual 由调度器经
/// [`build_point_with_trigger`] 固定）。
pub fn build_point(
    observations: &[TpsObservation],
    known_series: &[SeriesKey],
    run_id: &str,
    at_ms: i64,
) -> Vec<TpsPoint> {
    build_point_with_trigger(
        observations,
        known_series,
        run_id,
        at_ms,
        PointTrigger::Scheduled,
    )
}

/// [`build_point`] 的显式 trigger 版本（Spec §6：trigger(startup|scheduled|manual)）；
/// 不携带计划节奏（cadence_ms=None）——调度器建点走 [`build_point_with_schedule`]。
pub fn build_point_with_trigger(
    observations: &[TpsObservation],
    known_series: &[SeriesKey],
    run_id: &str,
    at_ms: i64,
    trigger: PointTrigger,
) -> Vec<TpsPoint> {
    build_point_with_schedule(observations, known_series, run_id, at_ms, trigger, None)
}

/// 完整版（独立复核 R11）：点携带**发点时生效的计划节奏** `cadence_ms`（可见 60000 / 后台
/// 600000；startup / manual 点同样带当时模式的节奏），前端据此判定「缺失的计划采样点」而
/// 非猜通用阈值；`None` 表示无节奏信息（纯函数测试 / 旧库行）。
pub fn build_point_with_schedule(
    observations: &[TpsObservation],
    known_series: &[SeriesKey],
    run_id: &str,
    at_ms: i64,
    trigger: PointTrigger,
    cadence_ms: Option<i64>,
) -> Vec<TpsPoint> {
    let window_lo = at_ms - TPS_WINDOW_MS; // 排他下界
                                           // series 桶 = known_series（空窗也要出 gap）∪ 窗内观察实际出现的 series
    let mut by_series: BTreeMap<String, (SeriesKey, Vec<TpsObservation>)> = BTreeMap::new();
    for k in known_series {
        by_series
            .entry(k.id())
            .or_insert_with(|| (k.clone(), Vec::new()));
    }
    for o in observations {
        if o.ended_at_ms <= window_lo || o.ended_at_ms > at_ms {
            continue; // 半开窗外（恰好左边界排除；晚于 at 排除）
        }
        let key = series_key_of(o);
        by_series
            .entry(key.id())
            .or_insert_with(|| (key, Vec::new()))
            .1
            .push(o.clone());
    }
    by_series
        .into_values()
        .map(|(series, cands)| {
            let deduped = dedup_by_call_key(&cands);
            make_point(&series, &deduped, run_id, at_ms, trigger, cadence_ms)
        })
        .collect()
}

/// 快照（Spec §6 + §2）：每 series 的「最近一次有效完成调用」与「当前回合已采样完成部分」，
/// 经 TpsSelection 过滤。最新读数超过 10 分钟不再当当前值（last_call/current_turn 为 null），
/// 但 latest_ended_at/age_ms 与空值原因仍返回（Spec §6「返回…ageMs 及空值原因」——null 不裸奔）；
/// 缺原生回合键 → current_turn 为 null 且原因 unknown-turn（只有 session 身份不称当前回合）。
///
/// sourceStates 为本层从观察来源推导的**占位形态**（ok=true、空游标）：真实来源状态由
/// 读取层 ReadBatch 携带（Spec §6），Task 7 命令层以真实值覆写 `TpsSnapshot::source_states`。
pub fn snapshot(
    observations: &[TpsObservation],
    selection: &TpsSelection,
    now_ms: i64,
) -> TpsSnapshot {
    // series 桶（不设 120s 窗：最近调用/当前回合不限窗，时效由 10 分钟新鲜度表达）
    let mut by_series: BTreeMap<String, (SeriesKey, Vec<TpsObservation>)> = BTreeMap::new();
    for o in dedup_by_call_key(observations) {
        let key = series_key_of(&o);
        by_series
            .entry(key.id())
            .or_insert_with(|| (key, Vec::new()))
            .1
            .push(o);
    }

    let mut series_out: Vec<TpsSeriesSnapshot> = by_series
        .into_values()
        .filter(|(series, _)| selection.matches_series(series))
        .map(|(series, cands)| series_snapshot(&series, &cands, now_ms))
        .collect();
    series_out.sort_by_key(|s| s.series_key.id());

    // sourceStates 占位推导（见函数注释）：观察来源去重，ok=true、空游标
    let mut sources: BTreeMap<String, SourceState> = BTreeMap::new();
    for o in observations {
        sources
            .entry(o.source.clone())
            .or_insert_with(|| SourceState {
                source: o.source.clone(),
                cursor: SourceCursor::empty(&o.source),
                ok: true,
                detail: None,
            });
    }

    TpsSnapshot {
        series: series_out,
        source_states: sources.into_values().collect(),
    }
}

/// 单 series 快照面。空值原因优先级：无有效样本 > 超 10 分钟 > 缺回合键。
fn series_snapshot(
    series: &SeriesKey,
    candidates: &[TpsObservation],
    now_ms: i64,
) -> TpsSeriesSnapshot {
    let pool = dedup_by_call_key(candidates);
    let latest = pool
        .iter()
        .filter(|o| o.is_tps_valid())
        .max_by_key(|o| (o.ended_at_ms, o.stable_call_key.clone()));

    let Some(latest) = latest else {
        return TpsSeriesSnapshot {
            series_key: series.clone(),
            last_call: None,
            current_turn: None,
            latest_ended_at: None,
            age_ms: None,
            null_reason: Some(NULL_REASON_NO_VALID_SAMPLE.to_string()),
        };
    };

    let latest_ended_at = latest.ended_at_ms;
    let age_ms = (now_ms - latest_ended_at).max(0);
    if age_ms > TPS_CURRENT_FRESH_MS {
        // Spec §2：超过 10 分钟不再当当前值——两个当前值都清空，历史点仍可查看
        return TpsSeriesSnapshot {
            series_key: series.clone(),
            last_call: None,
            current_turn: None,
            latest_ended_at: Some(latest_ended_at),
            age_ms: Some(age_ms),
            null_reason: Some(NULL_REASON_STALE.to_string()),
        };
    }

    // last_call = 单条最近有效调用的指标面（候选集=它自己，计数如实为 1）
    let last_call = aggregate(std::slice::from_ref(latest), series.method, now_ms, false).metrics;

    // 当前回合 = 最近有效调用的原生回合及其已采样完成部分（Spec §6）。回合身份是
    //「来源 / 宿主实例 / 会话 / 回合键」的联合身份（独立复核 R6）：裸回合键在 DSH 等来源
    // 只是会话内数字序号，两个会话都有回合 "1" 时若只比回合键会把它们合成同一轮；缺会话
    // 身份时保守降级为 unknown-turn（不拿裸回合号冒充回合身份，Spec §1.4）。
    let latest_identity = turn_identity(latest);
    let current_turn = latest_identity.map(|ident| {
        let same_turn: Vec<TpsObservation> = pool
            .iter()
            .filter(|o| turn_identity(o) == Some(ident))
            .cloned()
            .collect();
        aggregate(&same_turn, series.method, now_ms, false).metrics
    });

    TpsSeriesSnapshot {
        series_key: series.clone(),
        last_call: Some(last_call),
        current_turn,
        latest_ended_at: Some(latest_ended_at),
        age_ms: Some(age_ms),
        null_reason: if latest_identity.is_none() {
            Some(NULL_REASON_UNKNOWN_TURN.to_string())
        } else {
            None
        },
    }
}

/// 回合的可靠联合身份：(来源, 宿主实例, 会话, 回合键)；任一缺失 → None（保守降级）
type TurnIdentity<'a> = (&'a str, &'a str, &'a str, &'a str);

fn turn_identity(o: &TpsObservation) -> Option<TurnIdentity<'_>> {
    Some((
        o.source.as_str(),
        o.host_instance.as_str(),
        o.session_key.as_deref()?,
        o.turn_key.as_deref()?,
    ))
}

// ==== 以下为 Task 1 Step 1 的红测试（先于实现落笔；接口缺失即 FAIL）====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tps::model::*;

    /// 固定假时钟基准（纯整数，不读系统时钟）
    const BASE_MS: i64 = 1_700_000_000_000;

    /// 夹具 series：tool=kimi / model=kimi-k2 / channel=None / method=client-native
    fn series_kimi() -> SeriesKey {
        SeriesKey {
            tool: "kimi".into(),
            model: "kimi-k2".into(),
            channel: None,
            method: TimingMethod::ClientNative,
        }
    }

    /// 默认有效观察：Success / MainTask / client-native，end=ended_at，
    /// 生成窗 gen_ms，输出 output token；首点 = 开始 + gen_ms/4（可对齐）。
    fn valid_obs(key: &str, ended_at: i64, gen_ms: i64, output: i64) -> TpsObservation {
        TpsObservation {
            source: "kimi".into(),
            host_instance: "host-1".into(),
            stable_call_key: key.into(),
            started_at_ms: ended_at - gen_ms,
            first_token_at_ms: Some(ended_at - gen_ms + gen_ms / 4),
            ended_at_ms: ended_at,
            native_duration_ms: Some(gen_ms),
            generation_ms: Some(gen_ms),
            response_elapsed_ms: None,
            output_tokens: Some(output),
            output_scope: OutputScope::ExclusiveOutput,
            tool_ms: None,
            model: "kimi-k2".into(),
            channel: None,
            session_key: Some("s1".into()),
            turn_key: Some("t1".into()),
            purpose: Purpose::MainTask,
            outcome: Outcome::Success,
            method: TimingMethod::ClientNative,
            failure_reason: None,
        }
    }

    fn one_point(pts: &[TpsPoint]) -> &TpsPoint {
        assert_eq!(pts.len(), 1, "夹具只产一个 series 的点");
        &pts[0]
    }

    // ===== Step 1 红测试清单（brief 逐字值）=====

    /// 同 series 只取最近 20 条有效（Spec §2「最多取最近20条」）
    #[test]
    fn same_series_keeps_most_recent_20() {
        let obs: Vec<TpsObservation> = (0..25)
            .map(|i| valid_obs(&format!("c{i:02}"), BASE_MS - i * 1000, 1000, 1))
            .collect();
        let pts = build_point(&obs, &[], "run-1", BASE_MS);
        let p = one_point(&pts);
        assert_eq!(p.sample_count, 20);
        assert_eq!(p.sum_output_tokens, 20, "最旧 5 条不得入样");
        assert_eq!(p.sampled_keys.len(), 20);
        assert!(!p.sampled_keys.contains(&"c24".to_string()));
        assert_eq!(p.sampled_keys[0], "c00", "sampled_keys 最近在前");
        assert_eq!(p.sampled_keys[19], "c19");
    }

    /// 点内 TPS = 1000×Σ输出/Σ生成毫秒，不平均单请求 TPS（Spec §2/§7）
    #[test]
    fn point_tps_is_ratio_of_sums_not_mean_of_ratios() {
        let a = valid_obs("a", BASE_MS, 5000, 500); // 单请求 100 tok/s
        let b = valid_obs("b", BASE_MS - 1000, 1000, 50); // 单请求 50 tok/s
        let pts = build_point(&[a, b], &[], "run-1", BASE_MS);
        let point = one_point(&pts);
        assert!((point.tps.unwrap() - 550.0 / 6.0).abs() < 1e-9);
        assert_eq!(point.sample_count, 2);
        assert_eq!(point.sum_output_tokens, 550);
        assert_eq!(point.sum_generation_ms, 6000);
    }

    /// 失败不入速度（Spec §3）：100 token/1s 的失败调用不改变 TPS，
    /// 但有可靠时长 → 计入样本模型用时与分类计数
    #[test]
    fn failed_call_does_not_change_tps_but_enters_detail_counters() {
        let ok1 = valid_obs("a", BASE_MS, 5000, 500);
        let ok2 = valid_obs("b", BASE_MS - 1000, 1000, 50);
        let mut bad = valid_obs("x", BASE_MS - 2000, 1000, 100);
        bad.outcome = Outcome::Failed;
        bad.failure_reason = Some("boom".into());
        let pts = build_point(&[ok1, ok2, bad], &[], "run-1", BASE_MS);
        let point = one_point(&pts);
        assert!((point.tps.unwrap() - 550.0 / 6.0).abs() < 1e-9);
        assert_eq!(point.sample_count, 2);
        assert_eq!(point.failed_count, 1);
        assert_eq!(point.candidate_count, 3);
        assert_eq!(
            point.timed_model_count, 3,
            "失败但有可靠时长计入 timedModelCount"
        );
        assert!(
            point.model_ms.unwrap() > 6000,
            "失败调用的可靠时长计入样本模型用时"
        );
    }

    /// 不设全局 1 秒最小生成窗（Spec §3）：806ms 合法原生小窗不得被剔除
    #[test]
    fn native_806ms_window_is_not_dropped() {
        let o = valid_obs("fast", BASE_MS, 806, 100);
        let pts = build_point(&[o], &[], "run-1", BASE_MS);
        let point = one_point(&pts);
        assert_eq!(point.sample_count, 1);
        assert!((point.tps.unwrap() - 1000.0 * 100.0 / 806.0).abs() < 1e-9);
    }

    /// 空窗必须仍有 known_series 对应 gap（Spec §2：不画 0，不沿用旧速度）
    #[test]
    fn empty_window_yields_gap_for_known_series() {
        let known = vec![series_kimi()];
        let pts = build_point(&[], &known, "run-1", BASE_MS);
        assert_eq!(pts.len(), 1, "known_series 缺一不可");
        let empty = one_point(&pts);
        assert_eq!(empty.status, PointStatus::Gap);
        assert!(empty.tps.is_none());
        assert_eq!(empty.sample_count, 0);
        assert_eq!(empty.series_key, series_kimi());
        assert_eq!(
            empty.method,
            TimingMethod::ClientNative,
            "gap 仍带 series 方法"
        );
        assert_eq!(
            empty.output_scope,
            OutputScope::Unknown,
            "无样本时 output_scope 无事实，不得谎称 provider-reported"
        );
        // 有候选但全失败 → 仍是 gap，且分类计数保留
        let mut bad = valid_obs("x", BASE_MS, 1000, 100);
        bad.outcome = Outcome::Failed;
        let pts = build_point(&[bad], &known, "run-1", BASE_MS);
        let empty = one_point(&pts);
        assert_eq!(empty.status, PointStatus::Gap, "无有效样本即 gap");
        assert_eq!(empty.failed_count, 1, "gap 不丢候选分类计数");
    }

    // ===== Step 4 补充断言 =====

    /// 窗口规则（Spec §2）：end ∈ (at−120000, at]；跨开始边界整条纳入
    #[test]
    fn window_is_half_open_on_end_time_and_keeps_whole_call() {
        let at = BASE_MS;
        let in_boundary = valid_obs("in-at", at, 1000, 10);
        let inside = valid_obs("inside", at - 119_999, 1000, 10);
        let exact_left = valid_obs("exact-left", at - 120_000, 1000, 10); // 恰好左边界 → 排除
        let after = valid_obs("after", at + 1, 1000, 10); // 晚于 at → 排除
        let mut cross = valid_obs("cross", at - 1000, 130_000, 7); // 开始早于窗，完成在窗内 → 整条纳入
        cross.first_token_at_ms = Some(cross.started_at_ms + 500);
        let pts = build_point(
            &[in_boundary, inside, exact_left, after, cross],
            &[],
            "run-1",
            at,
        );
        let p = one_point(&pts);
        assert_eq!(p.sample_count, 3, "exact-left 与 after 均不在窗内");
        assert_eq!(p.sum_output_tokens, 27, "跨边界调用整条计入（7+10+10）");
        assert!(p.sampled_keys.contains(&"cross".to_string()));
        assert!(!p.sampled_keys.contains(&"exact-left".to_string()));
        assert!(!p.sampled_keys.contains(&"after".to_string()));
        assert_eq!(p.window_ms, 120_000);
    }

    /// 同一调用重复读取不翻倍（Spec §6/§7：一个点内只一次）
    #[test]
    fn duplicate_call_key_counted_once_per_point() {
        let a = valid_obs("same", BASE_MS, 5000, 500);
        let b = valid_obs("same", BASE_MS, 5000, 500);
        let pts = build_point(&[a, b], &[], "run-1", BASE_MS);
        let p = one_point(&pts);
        assert_eq!(p.sample_count, 1);
        assert_eq!(p.sum_output_tokens, 500);
        assert_eq!(p.candidate_count, 1, "重复键候选也只计一次");
    }

    /// 点的 trigger：默认 Scheduled；调度器可为 startup/manual 显式固定
    #[test]
    fn trigger_defaults_to_scheduled_and_can_be_pinned() {
        let o = valid_obs("a", BASE_MS, 1000, 10);
        // 首调用只读切片（o 还要复用）：from_ref 取引用切片，不克隆
        let pts = build_point(std::slice::from_ref(&o), &[], "run-1", BASE_MS);
        assert_eq!(pts[0].trigger, PointTrigger::Scheduled);
        let pts = build_point_with_trigger(&[o], &[], "run-1", BASE_MS, PointTrigger::Manual);
        assert_eq!(pts[0].trigger, PointTrigger::Manual);
    }

    /// snapshot：最近一次有效完成调用 + 当前回合（同 turnKey 的已采样完成部分）
    #[test]
    fn snapshot_returns_last_call_and_current_turn() {
        let c1 = valid_obs("c1", BASE_MS - 60_000, 1000, 100); // 旧回合 t1（valid_obs 默认 turn=t1）
        let mut c2 = valid_obs("c2", BASE_MS - 2000, 4000, 200);
        c2.turn_key = Some("t2".into());
        let mut c3 = valid_obs("c3", BASE_MS - 1000, 1000, 100);
        c3.turn_key = Some("t2".into());
        let mut fx = valid_obs("fx", BASE_MS - 1500, 500, 999);
        fx.turn_key = Some("t2".into());
        fx.outcome = Outcome::Failed;
        let snap = snapshot(&[c1, c2, c3, fx], &TpsSelection::default(), BASE_MS);
        assert_eq!(snap.series.len(), 1);
        let s = &snap.series[0];
        // last_call = 最近有效调用（c3 单条）
        let last = s.last_call.as_ref().expect("最近有效调用在 10 分钟内");
        assert!((last.tps.unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(last.sample_count, 1);
        assert_eq!(s.latest_ended_at, Some(BASE_MS - 1000));
        assert_eq!(s.age_ms, Some(1000));
        assert!(s.null_reason.is_none());
        // current_turn = t2 回合的有效完成调用（c2+c3），失败不入速度入计数
        let turn = s.current_turn.as_ref().expect("t2 有原生回合键");
        assert!((turn.tps.unwrap() - 1000.0 * 300.0 / 5000.0).abs() < 1e-9);
        assert_eq!(turn.sample_count, 2);
        assert_eq!(turn.failed_count, 1);
        // sourceStates 从观察来源推导（Task 1 占位语义）
        assert!(snap.source_states.iter().any(|st| st.source == "kimi"));
    }

    /// 缺原生回合键 → current_turn 为 null 且返回 unknown-turn（Spec §6）
    #[test]
    fn snapshot_unknown_turn_yields_null_current_turn_with_reason() {
        let mut o = valid_obs("solo", BASE_MS - 1000, 1000, 100);
        o.turn_key = None;
        let snap = snapshot(&[o], &TpsSelection::default(), BASE_MS);
        let s = &snap.series[0];
        assert!(s.last_call.is_some(), "最近有效调用本身不因缺回合键消失");
        assert!(s.current_turn.is_none(), "只有 session 身份不称当前回合");
        assert_eq!(s.null_reason.as_deref(), Some("unknown-turn"));
    }

    /// R6（独立复核 P1）：最近一轮按「来源/宿主/会话/回合」联合身份分组——两个会话复用裸回合号
    /// （DSH 把会话内数字回合号直接存成串）时不得合并成同一轮；宿主实例不同亦不合并；
    /// 缺会话身份时保守降级为 unknown-turn（不拿裸回合号冒充回合身份）。
    #[test]
    fn current_turn_groups_by_session_and_host_not_bare_turn_number() {
        // s1 回合 "1"：两条慢调用（10 tok/s）
        let mut a1 = valid_obs("s1-c1", BASE_MS - 50_000, 10_000, 100);
        a1.session_key = Some("s1".into());
        a1.turn_key = Some("1".into());
        let mut a2 = valid_obs("s1-c2", BASE_MS - 40_000, 10_000, 100);
        a2.session_key = Some("s1".into());
        a2.turn_key = Some("1".into());
        // 另一宿主实例、同会话名同回合号：也不是同一轮
        let mut h2 = valid_obs("h2-c1", BASE_MS - 30_000, 10_000, 100);
        h2.host_instance = "host-2".into();
        h2.session_key = Some("s2".into());
        h2.turn_key = Some("1".into());
        // s2 回合 "1"：一条快调用（100 tok/s），是最新调用
        let mut b1 = valid_obs("s2-c1", BASE_MS - 10_000, 1_000, 100);
        b1.session_key = Some("s2".into());
        b1.turn_key = Some("1".into());
        let snap = snapshot(&[a1, a2, h2, b1], &TpsSelection::default(), BASE_MS);
        assert_eq!(snap.series.len(), 1, "同 series");
        let ct = snap.series[0]
            .current_turn
            .as_ref()
            .expect("最新调用有完整回合身份");
        assert_eq!(
            ct.sample_count, 1,
            "最近一轮只含最新调用所属（宿主,会话,回合）的调用"
        );
        assert!(
            (ct.tps.unwrap() - 100.0).abs() < 1e-9,
            "不得混入另一会话/另一宿主同回合号的慢调用: {:?}",
            ct.tps
        );

        // 缺会话身份 → 保守降级：不称当前回合
        let mut c = valid_obs("no-session", BASE_MS - 5_000, 1_000, 100);
        c.session_key = None;
        c.turn_key = Some("1".into());
        let snap2 = snapshot(&[c], &TpsSelection::default(), BASE_MS);
        assert!(
            snap2.series[0].current_turn.is_none(),
            "缺会话身份不称当前回合"
        );
        assert_eq!(snap2.series[0].null_reason.as_deref(), Some("unknown-turn"));
    }

    /// R9（独立复核 P2）：TTFT 分母独立于 TPS 有效样本池（Spec §6「TTFT 只平均成功且首点可
    /// 对齐的样本，分母 ttftSampleCount 独立于 TPS 的 sampleCount」）——成功、有可靠 S/F
    /// 但缺输出计数（不入 TPS）的调用，其首点仍须进入 TTFT；两分母可不同。
    #[test]
    fn ttft_denominator_is_independent_of_tps_validity() {
        // 完整有效：ttft = gen/4 = 250ms
        let full = valid_obs("full", BASE_MS - 10_000, 1_000, 100);
        // 成功、有首点（ttft = 750ms）、但缺输出计数 → 不入 TPS，入 TTFT
        let mut no_output = valid_obs("no-output", BASE_MS - 5_000, 3_000, 100);
        no_output.output_tokens = None;
        no_output.generation_ms = None;
        assert!(!no_output.is_tps_valid());
        assert_eq!(no_output.ttft_ms(), Some(750));
        // 失败调用有首点也不入 TTFT（只平均成功样本）
        let mut failed = valid_obs("failed", BASE_MS - 2_000, 1_000, 100);
        failed.outcome = Outcome::Failed;

        let pts = build_point(&[full, no_output, failed], &[], "run", BASE_MS);
        let p = one_point(&pts);
        assert_eq!(p.sample_count, 1, "TPS 分母只含完整有效样本");
        assert_eq!(
            p.ttft_sample_count, 2,
            "TTFT 分母独立：成功且首点可对齐的样本都算"
        );
        assert_eq!(p.avg_first_token_ms, Some(500), "(250+750)/2");
    }

    /// 最新读数超过 10 分钟不再当当前值，但年龄仍须可见（Spec §2/§6）
    #[test]
    fn snapshot_stale_beyond_10min_clears_current_values_but_keeps_age() {
        let o = valid_obs("old", BASE_MS - 660_000, 1000, 100); // 11 分钟前
        let snap = snapshot(&[o], &TpsSelection::default(), BASE_MS);
        let s = &snap.series[0];
        assert!(s.last_call.is_none(), "超 10 分钟不再是当前值");
        assert!(s.current_turn.is_none());
        assert_eq!(s.latest_ended_at, Some(BASE_MS - 660_000), "年龄字段仍返回");
        assert_eq!(s.age_ms, Some(660_000));
        assert_eq!(s.null_reason.as_deref(), Some("stale"));
    }

    /// selection 过滤（Spec §6：tool/model/channel/method）
    #[test]
    fn selection_filters_series() {
        let kimi = valid_obs("k1", BASE_MS, 1000, 10);
        let mut cl = valid_obs("c1", BASE_MS, 1000, 10);
        cl.source = "claude".into();
        cl.model = "claude-sonnet".into();
        let mut st = valid_obs("s1", BASE_MS, 1000, 10);
        st.method = TimingMethod::StreamObserved;
        let sel = TpsSelection {
            tool_id: Some("kimi".into()),
            ..TpsSelection::default()
        };
        let snap = snapshot(&[kimi.clone(), cl], &sel, BASE_MS);
        assert_eq!(snap.series.len(), 1, "tool 过滤后只剩 kimi series");
        assert_eq!(snap.series[0].series_key.tool, "kimi");
        let sel = TpsSelection {
            method: Some(TimingMethod::StreamObserved),
            ..TpsSelection::default()
        };
        let snap = snapshot(&[kimi, st], &sel, BASE_MS);
        assert_eq!(
            snap.series.len(),
            1,
            "method 过滤后只剩 stream-observed series"
        );
        assert_eq!(
            snap.series[0].series_key.method,
            TimingMethod::StreamObserved
        );
    }

    /// source 与 provider 的 null 区别可表示（Spec §3「供应商输出已含思考…provider-reported」）：
    /// 四种 output_scope 必须互不可混淆
    #[test]
    fn output_scope_distinguishes_source_vs_provider_null() {
        let scopes = [
            OutputScope::ExclusiveOutput,   // 明确互斥：输出不含思考
            OutputScope::IncludesReasoning, // 供应商输出已含思考
            OutputScope::ProviderReported,  // provider-reported，是否含思考未声明
            OutputScope::Unknown,           // 无事实
        ];
        for (i, scope) in scopes.iter().enumerate() {
            let mut o = valid_obs(&format!("k{i}"), BASE_MS, 1000, 10);
            o.output_scope = *scope;
            let snap = snapshot(&[o], &TpsSelection::default(), BASE_MS);
            assert_eq!(
                snap.series[0].last_call.as_ref().unwrap().output_scope,
                *scope
            );
        }
        // 两两互异
        assert_ne!(scopes[0], scopes[1]);
        assert_ne!(scopes[1], scopes[2]);
        assert_ne!(scopes[2], scopes[3]);
    }

    /// 辅助标题/自动摘要默认不进入主任务曲线（Spec §3）：不入 TPS 分子分母，
    /// 但保留在候选与耗时详情计数里
    #[test]
    fn aux_purpose_excluded_from_point_tps_but_counted() {
        let main = valid_obs("m", BASE_MS, 1000, 100);
        let mut aux = valid_obs("a", BASE_MS - 1000, 1000, 400);
        aux.purpose = Purpose::AutoSummary;
        let pts = build_point(&[main, aux], &[], "run-1", BASE_MS);
        let p = one_point(&pts);
        assert_eq!(p.sample_count, 1, "辅助用途不入主任务曲线样本");
        assert!((p.tps.unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(p.candidate_count, 2, "候选计数不筛 purpose");
    }

    /// Spec 逐字枚举串形态锁（kebab/lowercase 逐字对齐 §3/§6）
    #[test]
    fn enum_serde_forms_match_spec() {
        assert_eq!(
            serde_json::to_string(&TimingMethod::StreamObserved).unwrap(),
            "\"stream-observed\""
        );
        assert_eq!(
            serde_json::to_string(&TimingMethod::ClientNative).unwrap(),
            "\"client-native\""
        );
        assert_eq!(
            serde_json::to_string(&TimingMethod::NativeEventApprox).unwrap(),
            "\"native-event-approx\""
        );
        for (v, s) in [
            (PointTrigger::Startup, "\"startup\""),
            (PointTrigger::Scheduled, "\"scheduled\""),
            (PointTrigger::Manual, "\"manual\""),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), s);
        }
        for (v, s) in [
            (PointStatus::Value, "\"value\""),
            (PointStatus::Gap, "\"gap\""),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), s);
        }
        for (v, s) in [
            (Outcome::Success, "\"success\""),
            (Outcome::Failed, "\"failed\""),
            (Outcome::Cancelled, "\"cancelled\""),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), s);
        }
    }
}
