//! 用量查询层：只读本地账本，**零文件扫描**（契约 §3 要点 1 / GC 3）。
//! 取数口径（本节的实现约定，逐条可测）：
//! * **小时粒度视图**（近 5 小时 / 当日）→ 明细表 `usage_detail`（小时桶）；项目键**取自明细行**
//!   （记录级 `project_key`，D21 规则④/R2——**不再**按 session 反查会话维度表）；
//! * **日粒度视图**（近 7 天 / 近 30 天 / 自定义）→ 日聚合表 `usage_daily`（永久保留），
//!   项目键同样取自日聚合行（与小时档同一记录级键，两档口径一致）；
//! * **计数类/工作小结** 一律取明细表（保留期内可得），超出保留期 → 相应字段 `null`（空态）；
//!   其中**会话数与 turn 数只计父会话**（D17 计数类分层，R3）；
//! * **`userEst` 可空**：`None` = 该源读不到用户文本 = 不可得（两档都出 `null`，不填 0，R1）；
//! * 分组行**返回全部**（组数天然有界：8 工具 / ~40 项目 / 12 供应商 / ~40 模型），
//!   「10 行 + 等N」的折叠是展示层的事（UI 自己知道总行数）；
//! * 环比：上一等长周期区间同法取数；**上一周期无数据 → `compare: None`**（不显示对比段）。
//! * **跨小时的 `tool_stats` `(0, ms>0)` 条目**（W-39 / Task 17 实测：次数落在 `tool/call` 那一小时、
//!   耗时落在 `result` 那一小时）**如实呈现**——本层不删、不归零；0 次调用时「平均耗时」按
//!   **不可得**给 `null`（不做除零，也不编造 0）。
//!
//! ## ✅ 时区口径：窗口键与落库键**同为宿主本地**（原 R-32 分叉已由 Q-4 裁决消除）
//!
//! **一律以宿主本地时区为基准**（spec §P6，2026-10-05 用户裁决）：本层的窗口键由
//! `range::resolve_range` 用 `hour_key_of_host` / `day_key_of_host` 生成，采集侧的**落库键走同一对入口**
//! （A-1 已把 codex 从命名时区改到宿主本地）⇒ **两侧同钟，结构上不可能分叉**。
//!
//! **实现纪律**（spec §P6 明文要求「实现处必须写明这一点」）：各源自带的时区信息
//! （如 codex 的 `turn_context.timezone`，本机 `Asia/Shanghai`）**可以读入备查**
//! （`CodexFileState::tz_name` 随 `state_json` 持久化），**但不得参与任何键计算**。
//! **「读得到却不用」是有意的**——后来者**不得**把它当成 bug「修回去」（改回命名时区即重造两套时钟）。
//! 封棺钉：`collectors::codex::tests::detail_hour_key_is_host_local_even_when_source_timezone_is_present`
//! （`turn_context.timezone` 在场、两源时区相差 26 小时，仍必须落**同一个宿主本地桶**）
//! + 源码级自省锁 `collect::tests::no_source_timezone_is_fed_into_any_key_function`。
//!
//! **旧 R-32 登记作废**：原先「窗口键按宿主本地、codex 落库键按其自带命名时区」的分叉，在
//! δ = 源时区偏移 − 宿主偏移 ≠ 0 时会让边界行落到查询窗口之外 → **静默少算**（跨整点 ≤1h、跨日 ≤1d）。
//! 该裁决把落库键一并统一到宿主本地 ⇒ **分叉与其失败模式同时消除，不再是开放分叉**；
//! 原先设想的「真修法」（把每条记录的时区持久化、改契约 §4 加列/加表）**随之失去前提，不再需要**。
//! 代价如实记录（spec §P6）：跨时区使用时日键按**宿主本地**日切分，与「按会话发生地当地日切分」
//! 不同——这是**为可复现性付的代价**，不是待办。
//!
//! ## 已知口径差异（**如实登记，本层不修**；均为上游/契约级决定，带进 ①→② 对齐复核）
//!
//! * **日档无会话维度**（评审判决 A）：日聚合行**不带 `session_id`**（粒度是「日 × 源 × 项目 ×
//!   供应商 × 模型」）⇒ 日档里任何**按会话**的判定都**不可得**。本层按 GC 7 的处理：
//!   **`recentSession` 日档直接给 `null`**（契约里该块本身可空；**不得**再聚合出空集，否则 UI 会
//!   显示「本会话 **0**」这个**看起来像真实测量值的 0**，`miniBarRange = last7d` 时可见）；
//!   **分组行的 `is_subagent` 按档位分叉**（**已按 2026-10-03 用户裁决落地**：契约 §2 现为
//!   `boolean | null`）：**日档给 `None`**（不可得）、**小时档给 `Some(true/false)`**（真值）。
//!   「不知道」**不得**写成 `false`——那等于谎称「不含子代理」（与 `userEst` 的 R1 同一逻辑）。
//! * **kimi 的模型/供应商兜底**（W-44）：真机 23/82 个文件通篇无 `usage.record`，其 model/provider
//!   来自**同轮其它文件**，落库 `provider_kind = measured` 是**兜底猜出来的**（不是实测）——本层只透传该列。
//! * **kimi 明细行数口径**（W-45）：84（含 28 空模型行）→ 59 → **58**。
//! * **opencode V1 的 `error_turn` 恒 0**（W-46）：`caps` 仍声明该源可得 → 本层 `availability`
//!   也照 caps 报 `available = true`，与 GC 7 存在张力，**待 ①→② 裁决，不得自行改 caps**。
//! * **`dropped_no_model`（W-51）** 只在采集器私有 `state_json` 里，不进本层任何字段。
//! * **`is_subagent` 语义**（W-30/W-31）= 「**本会话自身就是**子代理会话」（D17 分层的唯一依据），
//!   不是「文件里出现过侧链」；DAO 侧是只升不降的粘滞位（`MAX`）。
//! * **D-39**：zcode 的 `fork` / `selection_side_chat` **也带 `parent_id`** ⇒ 属**子会话**一侧，
//!   不计入父会话数 / turn 数（采集侧已按 `parent_id` 非空置位，本层只消费）。
//!
//! 本层**不做任何口径发明**（GC 6）：命中率与 hero 走 `semantics` 的两个纯函数，
//! 其余只是把 DAO 读回的行按组/按桶重新聚合。
//!
//! ## 记录页与 CSV（Task 19）
//!
//! * **记录页 `records`**（W6/D6/D7）：`groupBy` **只接受 `tool` | `project`**——它只驱动**卡片**维度，
//!   卡内行**恒为「供应商 / 模型」**（供应商不可得时只呈现模型名，§8.3）。传 `provider` / `model`
//!   一律结构化错误 `usage-groupby-invalid`；**守卫在总开关之前**（关闭态也报错，锁在用例里）。
//!   **大看板的 `model` 维度用同一形状**（**2026-10-06 用户裁决**：分布维度合并为「供应商 / 模型」）
//!   ——不再是裸模型名；同一模型挂在两个供应商下 = **两组**（键必须含供应商，否则两个供应商的
//!   同名模型会互相吃掉）。两处共用同一个私有实现 `route_label`，**不得各写一份**。
//!   卡内行的 `isSubagent` **恒 `None`**（契约 §2 用户裁决）：该字段的语义是 D17 的计数分层，
//!   而这一层的行维度是「供应商 / 模型」、**不含 `session_id`** → 本档位算不出
//!   （**小时档也一样**；与小时档**分组行**的真值不是同一层，别写混）——不得写真 `false`。
//! * **CSV `export_csv`**（契约 §3）：**只导结构化账本列**（11 列 = 表头常量 `CSV_HEADER`），
//!   零会话正文（GC 10 白名单）：会话标题、`project_path_raw` / `project_realpath`（原始路径）
//!   一律**不进**任何输出（有哨兵用例锁住）。列值纪律：`userEst` 不可得 = **空单元格**（不是 0，
//!   GC 7 的两面）、命中率 `{:.6}`（避免 `0.30000000000000004` 这类浮点尾巴）、`sourceKind` 用
//!   落库字面值（`measured` / `inferred` / `unknown`）。W5：供应商不可得时 `label` 列**留空**
//!   ——CSV 是导出文件、不落 i18n 键（UI 侧才 `t(label)`）。
//!   **本层只出文本、不带 BOM**：BOM 是 Task 21 落盘命令（`export_save_text`）的职责。
//! * 两条入口都**先持 `DB` 锁、guard 存活期间只调 `collect::cached_result()`**（W-26 的唯一安全形式：
//!   它只碰 `LAST_RESULT`、不碰飞行锁；持锁调采集会构成 ABBA 死锁）。
//! * **`collectedAt` 的 0 哨兵**（契约把该字段定为非空 `number`）：由 `cached_result()` 给，
//!   从未采集过即 0；关闭态走早退分支恒 0（确定性可断言）。与 Task 18 同一口径，登记 ①→②。
//! * **`load_ledger_rows()` 已删除**（Task 20 执行 Task 19 的裁定）：它 `pub(crate)` 且
//!   **内部自取 `DB` 锁**，而唯一可能的接线位置（三条 IPC 入口）**都在已持锁路径内** →
//!   `std::sync::Mutex` 不可重入 → **自死锁**；自省锁只禁 ABBA 族、管不到重入。
//!   属**接口变更**（Task 18 brief:13 声明的接口），详见 `task-20-report.md` 偏差申报。

use std::collections::BTreeMap;

use super::caps::{caps_of, turn_semantics};
use super::collect;
use super::error::UsageError;
use super::model::*;
use super::range::{
    day_label, granularity_of, hour_label, resolve_range, RangeGranularity, ResolvedRange,
};
use super::semantics::{cache_hit_rate, hero_of};
use crate::database::connection::DB;

/// 一行账（已按当前视图粒度取数）。`Clone` 便于按组/按桶重切。
/// `project_key` / `project_label` 是**记录级**键与标签（R2）；`user_est` 可空（R1）。
#[derive(Debug, Clone)]
pub(crate) struct LedgerRow {
    pub bucket_key: String,
    pub source_id: String,
    /// **日档恒为空串**：日聚合的粒度是「日 × 源 × 项目 × 供应商 × 模型」，**不带会话 id**
    /// ⇒ 日档里任何**按会话**的判定都不可得。本层两个消费点的处理（GC 7 / 评审判决 A）：
    /// * `recent_session_with` → 日档直接返回 `None`（不可得给 `null`，**不给 0**）；
    /// * 分组行的 `is_subagent` → 日档 `None`（不可得 = `null`，**不是 `false`**；契约 §2
    ///   2026-10-03 用户裁决），小时档才有真值 `Some(bool)`。
    pub session_id: String,
    pub project_key: String,
    pub project_label: String,
    pub provider: String,
    pub provider_kind: SourceKind,
    pub model: String,
    pub buckets: UsageBuckets,
    pub request_total: i64,
    pub requests: i64,
    pub user_est: Option<i64>,
}

/// 连接注入版：按视图粒度取数（Hour → **明细行自带的记录级项目键**；Day → 日聚合）
pub(crate) fn load_ledger_rows_with(
    conn: &rusqlite::Connection,
    resolved: &ResolvedRange,
) -> Vec<LedgerRow> {
    load_rows_for_keys(conn, resolved.cur.granularity, &resolved.cur.keys)
}

/// 按「粒度 + 桶键集合」取数（当期与上一周期共用同一条取数路径，保证口径一致）。
/// **R2 口径**：小时档与日档的项目键**都直接取记录级列**（`usage_detail.project_key` /
/// `usage_daily.project_key`），两档不得分叉；会话维度表只用于**标签字典**（键 → 显示名原文），
/// 不充当归属来源。日档的 `user_est` 同样直接取列（R1：不可得 = `None`）。
///
/// 窗口键的时钟假设见模块文档（R-32）：这里是**宿主本地**生成的窗口键与落库键做字符串比较，
/// δ ≠ 0 时边界行会落到窗口外 → 少算；本机 δ = 0 不发作，冲突已上报 ①→②。
pub(crate) fn load_rows_for_keys(
    conn: &rusqlite::Connection,
    granularity: RangeGranularity,
    keys: &[String],
) -> Vec<LedgerRow> {
    let Some(first) = keys.first() else {
        return Vec::new();
    };
    let last = keys.last().cloned().unwrap_or_default();
    let labels = key_label_dict(conn);
    match granularity {
        RangeGranularity::Hour => {
            // 记录级项目键**就在明细行上**（不再按 session_id 反查会话维度表——那会丢记录级归属）
            crate::database::dao::usage::query_detail_conn(conn, first, &last)
                .into_iter()
                .filter(|r| keys.contains(&r.hour_key))
                .map(|r| {
                    let pk = r.project_key;
                    let pl = labels.get(&pk).cloned().unwrap_or_default();
                    LedgerRow {
                        bucket_key: r.hour_key,
                        source_id: r.source_id,
                        session_id: r.session_id,
                        project_key: pk,
                        project_label: pl,
                        provider: r.provider,
                        provider_kind: r.provider_kind,
                        model: r.model,
                        buckets: r.buckets,
                        request_total: r.request_total,
                        requests: r.requests,
                        user_est: r.user_est, // R1：可空直传，绝不 `Some(unwrap_or(0))`
                    }
                })
                .collect()
        }
        RangeGranularity::Day => crate::database::dao::usage::query_daily_conn(conn, first, &last)
            .into_iter()
            .filter(|r| keys.contains(&r.day_key))
            .map(|r| {
                let pk = r.project_key;
                let pl = labels.get(&pk).cloned().unwrap_or_default();
                LedgerRow {
                    bucket_key: r.day_key,
                    source_id: r.source_id,
                    session_id: String::new(), // 日聚合不带会话 id（会话数走明细表）
                    project_key: pk,
                    project_label: pl, // 与小时档同一本标签字典（D21 显示名 = 原文）
                    provider: r.provider,
                    provider_kind: r.provider_kind,
                    model: r.model,
                    buckets: r.buckets,
                    request_total: r.request_total,
                    requests: r.requests,
                    user_est: r.user_est, // R1：日档必须出真值（缺列就会恒显示「—」）
                }
            })
            .collect(),
    }
}

/// 项目键 → 显示名原文的**标签字典**（键 = 小写 projectName）。
/// **只用于给记录级项目键配显示名，不是归属来源**（归属看明细/日聚合的 `project_key`）。
/// 同一键可能对应多个原文（Windows 上 claude 已小写、zcode 保留原文）→
/// 取最近活跃会话的原文（先按 `last_seen_at` 倒序，再 `or_insert` 即确定性首选）。
pub(crate) fn key_label_dict(
    conn: &rusqlite::Connection,
) -> std::collections::HashMap<String, String> {
    let mut sessions = crate::database::dao::usage::load_sessions_conn(conn, None);
    // `sort_by_key(Reverse(..))` 与 `sort_by(|a, b| b.x.cmp(&a.x))` 语义相同（都是稳定排序），
    // 改写只为过门禁 `clippy::unnecessary_sort_by`（`-D warnings` 下是 error，D-05/D-18 同族）
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_seen_at));
    let mut out = std::collections::HashMap::new();
    for s in sessions {
        if s.project_key.is_empty() || s.project_label.is_empty() {
            continue;
        }
        out.entry(s.project_key).or_insert(s.project_label);
    }
    out
}

/// (源, 会话) → 是否子代理（D17 分层判据；明细表不带该列，按会话维度查）
pub(crate) fn session_subagent_flags(
    conn: &rusqlite::Connection,
) -> std::collections::HashMap<(String, String), bool> {
    crate::database::dao::usage::load_sessions_conn(conn, None)
        .into_iter()
        .map(|s| ((s.source_id, s.session_id), s.is_subagent))
        .collect()
}

/// 过滤器（契约 §2 `UsageFilters`）：工具 / 项目 / 供应商 / 模型 + 子代理模式。
/// `parentsOnly` 把子代理会话的行**整体剔除**（含 token；页脚口径文案相应改「不含子代理」）；
/// `include`（默认）只影响计数分层。
/// 语义细节（**都有用例锁**）：**空列表 = 不过滤**（`Some(vec![])` 与 `None` 同义）、
/// **A-2 的筛选位「算得出吗」判据（单一入口，两条命令共用）**。
///
/// `Some(code)` = 该筛选组合在**当前档位算不出**，命令必须报结构化错误；`None` = 可判。
/// 目前**唯一**不可判的组合：**日档 + `subagentMode="parentsOnly"`**（日聚合行不带
/// `session_id`）。同族扫描结论（详见 FINAL-FIX-report §B）：其余成员两档都可判——
/// `toolIds` / `projects` / `providers` / `models` 在日档取 `usage_daily` 的**同名列**，
/// `subagentMode="include"` 恒不过滤 ⇒ 它们**不得**走这条早退（否则是误报，有反向锁）。
///
/// 放在这里（而不是命令层）的原因：与 `usage-groupby-invalid` 的守卫**同层**——
/// 两条命令共用同一判据，漏改一条必红（两条命令各有一条用例）。
pub(crate) fn filter_unavailable_code(
    preset: UsageRangePreset,
    filters: &UsageFilters,
) -> Option<&'static str> {
    let parents_only = matches!(filters.subagent_mode, Some(SubagentMode::ParentsOnly));
    if parents_only && granularity_of(preset) == RangeGranularity::Day {
        return Some("usage-filter-unavailable");
    }
    None
}

/// 多维度之间是 **AND**、表里查不到的 `(源, 会话)` 对按**非子代理**处理（`unwrap_or(false)`）。
/// 注：`projects` / `providers` / `models` 三个筛选**仅供 CSV 导出与内部查询**——
/// 记录页 UI 不暴露它们（说明书 §6 P3【默认裁定】：维度已由卡片分组与卡内行直接可见）。
pub(crate) fn apply_filters(
    rows: Vec<LedgerRow>,
    filters: &UsageFilters,
    subs: &std::collections::HashMap<(String, String), bool>,
) -> Vec<LedgerRow> {
    let hit = |list: &Option<Vec<String>>, v: &str| match list {
        Some(l) if !l.is_empty() => l.iter().any(|x| x == v),
        _ => true,
    };
    let parents_only = matches!(filters.subagent_mode, Some(SubagentMode::ParentsOnly));
    rows.into_iter()
        .filter(|r| hit(&filters.tool_ids, &r.source_id))
        .filter(|r| hit(&filters.projects, &r.project_key))
        .filter(|r| hit(&filters.providers, &r.provider))
        .filter(|r| hit(&filters.models, &r.model))
        .filter(|r| {
            if !parents_only {
                return true;
            }
            !subs
                .get(&(r.source_id.clone(), r.session_id.clone()))
                .copied()
                .unwrap_or(false)
        })
        .collect()
}

/// 「供应商 / 模型」路由标签（**唯一实现**，两处消费：记录页卡内行 + 大看板 `model` 维度）。
/// 供应商不可得（空串，workbuddy 实测形态）时只回**裸模型名**（§8.3：归因不到就只按模型维度呈现）；
/// 否则回 `"{provider} / {model}"`——**键必须含供应商**，否则两个供应商下的同名模型会被折叠成一组。
/// **2026-10-06 用户裁决**：分布维度合并为「供应商 / 模型」，故大看板的 `model` 维度与记录页卡内行
/// 形状**逐字一致**（此前大看板只给裸模型名）。
fn route_label(row: &LedgerRow) -> String {
    if row.provider.is_empty() {
        row.model.clone()
    } else {
        format!("{} / {}", row.provider, row.model)
    }
}

/// 分组键/标签/三态（D21：项目键 = 小写 projectName，标签 = projectName 原文）。
/// 供应商不可得（空串）时标签回 **i18n 键** `UNKNOWN_PROVIDER_LABEL_KEY`（W5：不得硬编码中文，
/// 前端 `t(label)` 渲染；CSV 是导出文件、由 Task 19 把该键还原成空串）。
pub(crate) fn group_of(row: &LedgerRow, group_by: UsageGroupBy) -> (String, String, SourceKind) {
    match group_by {
        UsageGroupBy::Tool => (
            row.source_id.clone(),
            row.source_id.clone(),
            row.provider_kind,
        ),
        UsageGroupBy::Project => (
            row.project_key.clone(),
            if row.project_label.is_empty() {
                row.project_key.clone()
            } else {
                row.project_label.clone()
            },
            row.provider_kind,
        ),
        UsageGroupBy::Provider => {
            if row.provider.is_empty() {
                // W5：不可得 = i18n 键（zh/en 同父同键），不是硬编码「未知」
                (
                    String::new(),
                    super::UNKNOWN_PROVIDER_LABEL_KEY.to_string(),
                    row.provider_kind,
                )
            } else {
                let key = row.provider.clone();
                (key.clone(), key, row.provider_kind)
            }
        }
        // 大看板 `model` 维度 = 与记录页卡内行**同一形状**「供应商 / 模型」（2026-10-06 用户裁决，
        // 见 `route_label` 的文档）：键含供应商 ⇒ 同一模型挂在两个供应商下出**两行**。
        UsageGroupBy::Model => {
            let label = route_label(row);
            (label.clone(), label, row.provider_kind)
        }
    }
}

/// 汇总一组行为 (四桶, 派生口径)。`user_est`：**只要有任一行有值就汇总，全不可得才是 None**
/// （R1：不可得的源不填 0；小时档与日档走同一个 `aggregate`，口径天然一致）
pub(crate) fn aggregate(rows: &[LedgerRow]) -> (UsageBuckets, UsageMetrics) {
    let mut b = UsageBuckets::default();
    let mut request_total = 0i64;
    let mut requests = 0i64;
    let mut user_est: Option<i64> = None;
    for r in rows {
        b.add(&r.buckets);
        request_total += r.request_total;
        requests += r.requests;
        if let Some(v) = r.user_est {
            user_est = Some(user_est.unwrap_or(0) + v);
        }
    }
    (
        b,
        UsageMetrics {
            request_total,
            cache_hit_rate: cache_hit_rate(b.cache_read, request_total),
            user_est,
            requests,
        },
    )
}

/// 组级 / 行级三态折叠（**单一实现，三处共用**）：取组内所有行的**最强**三态
/// （measured > inferred > unknown，`SourceKind::strongest`）。
///
/// **为什么必须是折叠、不能是「首行胜出」**（评审 Minor #2）：`UsageRow.sourceKind` 与 CSV 的
/// `sourceKind` 列是**同一个契约字段**的三个出口（看板分组行 / 记录页卡内行 / CSV 分组行）。
/// 「首行胜出」会**依赖 DAO 返回序**——混合三态时同一份数据换个行序就可能显示更弱甚至 `unknown`
/// 的状态，而 §4.3 要求三态必须**可见地区分**（弱态冒充强态是**静默错标**，正是 GC 7 要防的那类）。
/// 折叠后三处口径一致，且**与行序无关**（有用例把「较弱的那行排在前面」当夹具，三处一起锁）。
pub(crate) fn aggregate_source_kind(rows: &[LedgerRow]) -> SourceKind {
    rows.iter()
        .map(|r| r.provider_kind)
        .fold(SourceKind::Unknown, |a, b| a.strongest(b))
}

/// 逐源可得性表（D15/D16/D19：不可得写 false + reason，绝不填 0）。
/// **项目归属不进本表**：小时档与日档都取记录级 `project_key`，不存在「只能拿会话级」的档位
/// （会话维度表的项目列是会话级近似、不参与分组，见 Task 2 的 DDL 注释与 GC 18-R2）；
/// 若将来真的出现会话级降级档位，必须先在这里登记条目再上线（`UsageMetric` 是契约冻结枚举）。
pub fn availability_table() -> Vec<UsageAvailability> {
    let per = |pick: fn(&super::caps::SourceCaps) -> bool, reason: &str| {
        let mut map = BTreeMap::new();
        for s in UsageSourceId::ALL {
            map.insert(s.db_id().to_string(), pick(&caps_of(s)));
        }
        let any = map.values().any(|v| *v);
        UsageAvailability {
            metric: UsageMetric::Turn, // 调用处覆盖
            available: any,
            reason: Some(reason.to_string()),
            per_source: Some(map),
        }
    };
    let mut out = Vec::new();
    let mk = |metric: UsageMetric, pick: fn(&super::caps::SourceCaps) -> bool, reason: &str| {
        let mut a = per(pick, reason);
        a.metric = metric;
        a
    };
    out.push(mk(
        UsageMetric::Turn,
        |c| c.turn,
        &format!(
            "逐源口径不可比（D16），只在按工具维度显示、不做合计；{}",
            UsageSourceId::ALL
                .iter()
                .map(|s| turn_semantics(*s))
                .collect::<Vec<_>>()
                .join("；")
        ),
    ));
    out.push(mk(
        UsageMetric::ErrorModel,
        |c| c.error_model,
        "模型/传输层错误（D18 三层分列，互不包含、不得相加）",
    ));
    out.push(mk(
        UsageMetric::ErrorTurn,
        |c| c.error_turn,
        "回合失败层（claude 无回合级失败字段）",
    ));
    out.push(mk(
        UsageMetric::ErrorTool,
        |c| c.error_tool,
        "工具执行层（opencode 的 part.state.status 全 completed → 不可得）",
    ));
    out.push(mk(
        UsageMetric::Interrupted,
        |c| c.interrupted,
        "用户主动打断（单列，不计入错误；kimi 未取得判据）",
    ));
    out.push(mk(
        UsageMetric::LongestTurn,
        |c| c.longest_turn,
        "逐工具 p50 + 最长，不跨工具合计；**含挂机、未剔 idle**（D19）；dsh 仅覆盖有原始日志的会话（覆盖率随机器与清理策略变化，2026-10-06 实测账本口径已接近全量）、workbuddy 无 duration 字段",
    ));
    out.push(mk(
        UsageMetric::ToolCalls,
        |c| c.tool_calls,
        "工具调用次数/耗时；dsh 逐工具维度仅覆盖有原始日志的会话",
    ));
    out.push(mk(
        UsageMetric::UserEst,
        |c| c.user_est,
        "用户输入(估)：CJK 每字 1 + ASCII 每词 1，只对能读到用户文本的源计算（claude / codex / kimi）",
    ));
    out
}

/// 大看板（契约 §3）
pub fn dashboard(
    range: &UsageRange,
    group_by: UsageGroupBy,
    now_ms: i64,
) -> Result<UsageDashboard, UsageError> {
    if let Ok(conn) = DB.lock() {
        let d = dashboard_with_conn(&conn, range, group_by, now_ms)?;
        return Ok(d);
    }
    Err(UsageError::new("usage-db-failed", "db poisoned lock"))
}

/// 连接注入版（单测直连内存库）
pub fn dashboard_with_conn(
    conn: &rusqlite::Connection,
    range: &UsageRange,
    group_by: UsageGroupBy,
    now_ms: i64,
) -> Result<UsageDashboard, UsageError> {
    if !super::settings::load_from_conn(conn).enabled {
        // 总开关关闭：不采集不落库 → 查询出空态（零扫描、零行）。
        // 用连接注入版读设置：单测直连内存库时不触碰真实 ~/.tuvis/tuvis.db（W-24：
        // 单测**不得**调进程级 `settings::load()`，那会被别的模块注入的值串味）
        return Ok(empty_dashboard(range, group_by));
    }
    let resolved = resolve_range(range, now_ms)?;
    let rows = load_ledger_rows_with(conn, &resolved);
    let (totals_buckets, totals) = aggregate(&rows);
    let subs = session_subagent_flags(conn);
    // 分组行（全量返回；折叠交 UI）
    let mut groups: BTreeMap<String, (String, Vec<LedgerRow>)> = BTreeMap::new();
    for r in &rows {
        let (k, label, _kind) = group_of(r, group_by);
        groups
            .entry(k)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(r.clone());
    }
    let mut out_rows: Vec<UsageRow> = groups
        .into_iter()
        .map(|(key, (label, rs))| {
            let (b, m) = aggregate(&rs);
            // 组级三态 = 组内所有行的**最强**三态（与记录页卡内行 / CSV 分组行同一折叠，
            // 评审 Minor #2；**不得**首行胜出——那会依赖 DAO 返回序）
            let kind = aggregate_source_kind(&rs);
            // 该组是否只由子代理会话贡献（供 UI「单列一层可展开」，D17）。
            //
            // **按档位分叉**（契约 §2，2026-10-03 用户裁决：`boolean | null`）：
            // * **小时档**：明细行带 `session_id`，能真算 → `Some(true/false)`；
            // * **日档**：日聚合不带 `session_id`（见 `LedgerRow::session_id`），`subs` 必然查不到
            //   → 该位**不可得** = `None`（**不是 `Some(false)`**：那等于谎称「不含子代理」）。
            let is_sub = match resolved.cur.granularity {
                RangeGranularity::Hour => Some(
                    !rs.is_empty()
                        && rs.iter().all(|r| {
                            subs.get(&(r.source_id.clone(), r.session_id.clone()))
                                .copied()
                                .unwrap_or(false)
                        }),
                ),
                RangeGranularity::Day => None,
            };
            UsageRow {
                key,
                label,
                buckets: b,
                metrics: m,
                source_kind: kind,
                is_subagent: is_sub,
            }
        })
        .collect();
    out_rows.sort_by_key(|r| std::cmp::Reverse(r.metrics.request_total));
    // 趋势：逐桶
    let label_of = |k: &str| match resolved.cur.granularity {
        RangeGranularity::Hour => hour_label(k),
        RangeGranularity::Day => day_label(k),
    };
    let trend = resolved
        .cur
        .keys
        .iter()
        .map(|k| {
            let rs: Vec<LedgerRow> = rows
                .iter()
                .filter(|r| &r.bucket_key == k)
                .cloned()
                .collect();
            let (b, m) = aggregate(&rs);
            TrendPoint {
                key: k.clone(),
                label: label_of(k),
                buckets: b,
                metrics: m,
            }
        })
        .collect();
    // 环比：上一等长周期（完整四桶 + 完整口径；无数据 → None，不得显示 0% / NaN）
    let compare = resolved.prev.as_ref().and_then(|p| {
        let prev_rows = load_rows_for_keys(conn, p.granularity, &p.keys);
        let (pb, pm) = aggregate(&prev_rows);
        if pm.requests == 0 && pb.total() == 0 {
            return None;
        }
        Some(CompareBlock {
            prev_buckets: pb,
            prev_metrics: pm,
        })
    });
    let recent_session = recent_session_with(conn, &rows, resolved.cur.granularity);
    let work_summary = work_summary_with(conn, &resolved);
    Ok(UsageDashboard {
        range: range.clone(),
        group_by,
        rows: out_rows,
        totals,
        totals_buckets,
        hero: hero_of(totals.request_total, totals_buckets.output),
        trend,
        compare,
        recent_session,
        work_summary,
        availability: availability_table(),
        collected_at: collect::cached_result()
            .map(|r| r.collected_at)
            .unwrap_or(0),
    })
}

fn empty_dashboard(range: &UsageRange, group_by: UsageGroupBy) -> UsageDashboard {
    UsageDashboard {
        range: range.clone(),
        group_by,
        rows: Vec::new(),
        totals: UsageMetrics {
            request_total: 0,
            cache_hit_rate: 0.0,
            user_est: None,
            requests: 0,
        },
        totals_buckets: UsageBuckets::default(),
        hero: 0,
        trend: Vec::new(),
        compare: None,
        recent_session: None,
        work_summary: WorkSummary::default(),
        availability: availability_table(),
        collected_at: 0,
    }
}

/// 记录页（契约 §3；D6/D7/W6）：**卡片**维度由 `groupBy` 决定（`tool` → 每工具一卡、
/// `project` → 每项目一卡），**卡内行恒为「供应商 / 模型」**（供应商不可得时只呈现模型名，§8.3）。
/// `groupBy` 传 `provider` / `model` → `usage-groupby-invalid`（契约：记录页只接受这两值）。
/// `UsageCard.tool_id` / `tool_label` 承载**卡片分组键与显示名**（字段名沿用契约；
/// `groupBy=project` 时它们是项目键与项目名——② 按 `groupBy` 决定怎么渲染卡片标题）。
///
/// **W-26**：本入口先持 `DB` 锁，guard 存活期间只调 `collect::cached_result()`（它只碰 `LAST_RESULT`、
/// 不碰飞行锁）；**绝不在持锁期间调采集入口**（`collect` 与 `run_collection` 都只能在没有持有 `DB`
/// 时调用，否则与在飞扫描的「飞行锁 → DB」构成 ABBA 死锁）。
/// 注：上面的措辞刻意**不写函数调用的括号形态**——本文件的
/// `query_layer_never_calls_collection_and_reads_only_cached_result` 是**源码级自省锁**，
/// 它会扫描整份源码（含注释）里有没有采集入口的调用形态。
pub fn records(
    range: &UsageRange,
    group_by: UsageGroupBy,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<UsageRecords, UsageError> {
    let conn = DB
        .lock()
        .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
    records_with_conn(&conn, range, group_by, filters, now_ms)
}

/// 连接注入版（单测直连内存库）
pub fn records_with_conn(
    conn: &rusqlite::Connection,
    range: &UsageRange,
    group_by: UsageGroupBy,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<UsageRecords, UsageError> {
    // W6：记录页只接受 tool | project（其余维度由大看板的分组切换承担）。
    // **守卫刻意在总开关之前**：关闭态下非法维度也必须报结构化错误码（有用例锁）。
    if !matches!(group_by, UsageGroupBy::Tool | UsageGroupBy::Project) {
        return Err(UsageError::new(
            "usage-groupby-invalid",
            format!("records groupBy accepts tool | project only, got {group_by:?}"),
        ));
    }
    // **A-2（契约 §2 `UsageFilters` 通用条款 / §3 要点 4，错误码第 9 个）**：筛选条件在
    // **当前档位算不出**时必须报错，不得静默忽略。首个实例：**日档 + `parentsOnly`** ——
    // 日聚合行（`usage_daily`）**不带 `session_id`** ⇒ 无从判断会话身份 ⇒
    // `apply_filters` 的 `unwrap_or(false)` 会把**全部行放行**（"当没有子代理"），
    // 于是合计含子代理、而页脚文案写「不含子代理」= **界面谎言**（GC 7 / 说明书 §8.2 不可得不得假装）。
    // 与 `usage-groupby-invalid` **同层**：同样**在总开关之前**（关闭态也必须报该码）。
    if let Some(code) = filter_unavailable_code(range.preset, filters) {
        return Err(UsageError::new(
            code,
            "daily buckets (last7d/last30d/custom) lack session_id; \
             subagentMode=\"parentsOnly\" cannot be evaluated"
                .to_string(),
        ));
    }
    if !super::settings::load_from_conn(conn).enabled {
        // 总开关关闭：不采集不落库 → 查询出空态（零扫描、零行）。
        // 用连接注入版读设置：单测直连内存库时不触碰真实 `~/.tuvis/tuvis.db`（W-24：
        // 单测**不得**调进程级 `settings::load()`，那会被别的模块注入的值串味）。
        // `collected_at` 取 **0 哨兵**（契约把该字段定为非空 `number`，没有 null 可选）。
        return Ok(UsageRecords {
            range: range.clone(),
            group_by,
            cards: Vec::new(),
            availability: availability_table(),
            collected_at: 0,
        });
    }
    let resolved = resolve_range(range, now_ms)?;
    let subs = session_subagent_flags(conn);
    let rows = apply_filters(load_ledger_rows_with(conn, &resolved), filters, &subs);
    // 卡片分组键/显示名：tool → 工具 id；project → 项目键 + 标签字典给出的原文
    let mut cards: BTreeMap<String, (String, Vec<LedgerRow>)> = BTreeMap::new();
    for r in &rows {
        let (key, label) = match group_by {
            UsageGroupBy::Project => (
                r.project_key.clone(),
                if r.project_label.is_empty() {
                    r.project_key.clone()
                } else {
                    r.project_label.clone()
                },
            ),
            UsageGroupBy::Tool => (r.source_id.clone(), r.source_id.clone()),
            // 守卫在函数开头，这里不可能到达；显式写出来，避免「悄悄当成 tool」
            other => unreachable!("records 的 groupBy 已在入口校验，不接受 {other:?}"),
        };
        cards
            .entry(key)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(r.clone());
    }
    let mut out: Vec<UsageCard> = cards
        .into_iter()
        .map(|(group_key, (group_label, rs))| {
            let (buckets, metrics) = aggregate(&rs);
            // 卡内行**恒为「供应商 / 模型」**（D6）：键取这一对，避免同供应商的多个模型互相吃掉；
            // 供应商不可得（workbuddy）时只呈现模型名（§8.3：归因不到就只按模型维度呈现）。
            // 形状由 `route_label` **独占**（大看板 `model` 维度走同一个函数，2026-10-06 用户裁决）。
            // `is_subagent` **恒 `None`**（契约 §2，2026-10-03 用户裁决）：该字段的语义是 D17 的
            // 计数分层，而这一层的行维度是「供应商 / 模型」、**不含 `session_id`**
            // → 本档位算不出（**小时档也一样**：档位判定不落在这一层），不得写真 `false`。
            let mut inner: BTreeMap<String, (String, Vec<LedgerRow>)> = BTreeMap::new();
            for r in &rs {
                let (k, label) = (route_label(r), route_label(r));
                inner
                    .entry(k)
                    .or_insert_with(|| (label, Vec::new()))
                    .1
                    .push(r.clone());
            }
            let mut inner_rows: Vec<UsageRow> = inner
                .into_iter()
                .map(|(key, (label, rs))| {
                    let (b, m) = aggregate(&rs);
                    let kind = aggregate_source_kind(&rs);
                    UsageRow {
                        key,
                        label,
                        buckets: b,
                        metrics: m,
                        source_kind: kind,
                        is_subagent: None,
                    }
                })
                .collect();
            // `sort_by_key(Reverse(..))` 与 `sort_by(|a, b| b.x.cmp(&a.x))` 语义相同（都是稳定排序），
            // 改写只为过门禁 `clippy::unnecessary_sort_by`（`-D warnings` 下是 error，D-05/D-18/D-25 同族）
            inner_rows.sort_by_key(|r| std::cmp::Reverse(r.metrics.request_total));
            UsageCard {
                tool_id: group_key,
                tool_label: group_label, // 展示名由前端解析（工具走 AGENT_BADGE；项目用本字段原文）
                buckets,
                metrics,
                rows: inner_rows,
            }
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.metrics.request_total)); // 同上：过 `unnecessary_sort_by`
    Ok(UsageRecords {
        range: range.clone(),
        group_by,
        cards: out,
        availability: availability_table(),
        // W-26 安全形式：guard 存活期间只调 `cached_result()`（不碰飞行锁）。
        // 契约把 `collectedAt` 定为非空 `number` → 从未采集过给 **0 哨兵**（不是 null），登记 ①→②
        collected_at: collect::cached_result()
            .map(|r| r.collected_at)
            .unwrap_or(0),
    })
}

/// CSV 表头（**列顺序是冻结形态**：11 列；改它必须按契约变更纪律回写契约 / spec §9.7）。
/// 前四列是四桶、中间四列是派生口径、末列是供应商三态（落库字面值）。
const CSV_HEADER: &str = "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind\n";

/// CSV 导出（契约 §3）：**只导结构化账本列**，零会话正文（GC 10 隐私白名单）。
/// 前端拿到文本后经 `export_save_text` 落盘（该命令负责加 BOM 与路径校验）——
/// 因此**本层输出的文本不带 BOM**（否则落盘会再前置一次 → 双 BOM，有用例锁）。
/// 列值纪律：`userEst` 不可得 = **空单元格**（不是 0）、命中率 `{:.6}`、`sourceKind` 用落库字面值。
///
/// **W-26**：与 `records` 同一安全形式（先持 `DB` 锁，guard 存活期间只调 `cached_result()`）。
pub fn export_csv(
    range: &UsageRange,
    group_by: UsageGroupBy,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<String, UsageError> {
    let conn = DB
        .lock()
        .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
    csv_with_conn(&conn, range, group_by, filters, now_ms)
}

/// 连接注入版（单测直连内存库）。`groupBy` **四值都接受**（契约 §3：只有记录页限 `tool | project`）。
///
/// **如实登记（登记 ①→②）**：本函数**不判总开关**（任务书代码如此），与 `dashboard` / `records`
/// 的 `enabled` 早退**不对称**——关闭态下 CSV 仍会导出账本里的历史数据。用例
/// `master_switch_gates_records_and_csv_behavior_is_registered_as_is` 把这一现状钉住。
pub fn csv_with_conn(
    conn: &rusqlite::Connection,
    range: &UsageRange,
    group_by: UsageGroupBy,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<String, UsageError> {
    // **A-2（契约 §2 通用条款 / §3 要点 4）**：与 `records_with_conn` **同一判据**
    // （`filter_unavailable_code`）——CSV 也吃 `filters`，日档 + `parentsOnly` 同样算不出，
    // 不得导出一份"声称不含子代理、实则含"的账本。CSV 本就不判总开关（登记 ①→② 的不对称），
    // 故这里没有"守卫在总开关之前"的问题，但判据必须与记录页**逐字同源**（漏改一条即红）。
    if let Some(code) = filter_unavailable_code(range.preset, filters) {
        return Err(UsageError::new(
            code,
            "daily buckets (last7d/last30d/custom) lack session_id; \
             subagentMode=\"parentsOnly\" cannot be evaluated"
                .to_string(),
        ));
    }
    let resolved = resolve_range(range, now_ms)?;
    let subs = session_subagent_flags(conn);
    let rows = apply_filters(load_ledger_rows_with(conn, &resolved), filters, &subs);
    let mut groups: BTreeMap<String, (String, Vec<LedgerRow>)> = BTreeMap::new();
    for r in &rows {
        let (k, label, _kind) = group_of(r, group_by);
        // W5：CSV 是导出文件、不落 i18n 键——供应商不可得时该列留空（UI 侧才显示 t(label)）
        let label = if label == crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY {
            String::new()
        } else {
            label
        };
        groups
            .entry(k)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(r.clone());
    }
    let mut out = String::from(CSV_HEADER);
    let mut lines: Vec<(i64, String)> = groups
        .into_iter()
        .map(|(key, (label, rs))| {
            let (b, m) = aggregate(&rs);
            // 组级三态 = 组内所有行的**最强**三态（与看板分组行 / 记录页卡内行同一折叠，
            // 评审 Minor #2；**不得**首行胜出——那会依赖 DAO 返回序）
            let kind = aggregate_source_kind(&rs);
            (
                m.request_total,
                format!(
                    "{},{},{},{},{},{},{},{:.6},{},{},{}",
                    csv_escape(&key),
                    csv_escape(&label),
                    b.input_fresh,
                    b.cache_read,
                    b.cache_write,
                    b.output,
                    m.request_total,
                    m.cache_hit_rate,
                    m.requests,
                    // GC 7：不可得 = **空单元格**（`unwrap_or_default()`），**不是 0**——
                    // 真的是 0 的 `Some(0)` 会照常出 `0`（两态在 CSV 里可区分）
                    m.user_est.map(|v| v.to_string()).unwrap_or_default(),
                    kind.as_db(),
                ),
            )
        })
        .collect();
    lines.sort_by_key(|l| std::cmp::Reverse(l.0)); // 同上：过 `unnecessary_sort_by`
    for (_, l) in lines {
        out.push_str(&l);
        out.push('\n');
    }
    Ok(out)
}

/// CSV 字段转义（RFC 4180）+ **公式注入中和**。两件事都要做，**顺序**如下：
/// ① **先中和**：内容以 `=` `+` `-` `@` `\t` `\r` `\n` 开头 ⇒ 最前面补一个单引号 `'`（Excel /
///    Sheets / LibreOffice 通用的「当作文本」前缀，不会被显示成内容的一部分）。`groupKey` /
///    `label` 两列直接写原始文本，而**项目维度下 `label` 就是项目文件夹名** ⇒ 不中和时，一个
///    clone 到 `=cmd|'/C calc'!A0` 的仓库目录名会让打开导出 CSV 的人**执行命令**。
///    只处理 `=` 不够：`+` `-` `@` 同样触发公式；**`\t` / `\r` / `\n` 是「隐形前缀」**——Excel /
///    Sheets 会先剥掉行首空白（含换行）再判公式，故它们等价于把公式藏在第二个字符。
///    中和只按**首字符**判 ⇒ 加 `\n` 的代价为零：`a\nb` 这类「换行不在首位」的内容逐字不变。
/// ② **再按 RFC 4180 包裹**：含 `,` `"` `\n` `\r` 时用双引号包裹，并把 `"` 翻倍。
///    顺序不可颠倒：**加完前缀的串才是要被包裹的内容**（`'` 本身不是特殊字符，两件事互不干扰，
///    但先包裹后加前缀会产出 `"'…"` 之外的错形态）。
///
/// **防过度转义**：不含上述任何字符的内容（正常项目名 / 模型名 / 空串）**逐字不变**。
///
/// **对任务书代码的最小偏离（F-1，已申报）**：多判一个 `\r`。RFC 4180 里 **CR 也是记录分隔符**，
/// 只判 `\n` 时「含孤立 CR 的字段」会以裸形态导出 → 一条记录被 Excel 劈成两条（静默错行）。
/// `\r\n` 本就会因 `\n` 命中，所以这条只影响孤立 CR；判据与期望值**一字未改**，回退面 1 行。
pub(crate) fn csv_escape(s: &str) -> String {
    // ① 公式注入中和（**先**做）：`chars().next()` 而不是 `starts_with` × 7——一次取首字符即可
    let neutralize = matches!(
        s.chars().next(),
        Some('=') | Some('+') | Some('-') | Some('@') | Some('\t') | Some('\r') | Some('\n')
    );
    let body = if neutralize {
        format!("'{s}")
    } else {
        s.to_string()
    };
    // ② RFC 4180 引号包裹（**后**做，判的是加完前缀的串）
    if body.contains(',') || body.contains('"') || body.contains('\n') || body.contains('\r') {
        format!("\"{}\"", body.replace('"', "\"\""))
    } else {
        body
    }
}

/// 浮窗第 4 行「本会话 X」= **最近有活动的会话**（`lastSeenAt` 最大者，**且本期窗口内真的量到过
/// 请求输入**；**不是**当前打开的会话），其用量按**当前 range** 统计；无这种会话 → `None`。
///
/// **日档一律 `None`**（GC 7 / 评审判决 A）：日聚合行**不带 `session_id`**（见 `LedgerRow::session_id`），
/// 日档无法按会话取数。若照旧让它在空集上聚合，UI 会拿到一个 `buckets` 全 0 的 `recentSession`，
/// 显示成「**本会话 0**」——一个**看起来像真实测量值的 0**；正确语义是「不可得」。
/// 契约里该块本身可空（`RecentSessionUsage | null`），故日档给 `null` 是契约内的正确表达。
/// `miniBarRange = last7d` 时由 UI 显示空态。
///
/// **小时档同样要防这个形状（2026-10-06 评审 C1 修复）**：候选必须**限定在「本期窗口内真的
/// 量到过请求输入（`request_total > 0`）」的会话**里 ——注意是「**有量**」、不是仅仅「有行」：
/// 四桶全零的**真空回合**行同样不算量（见下）。旧实现取**全时全量**会话里 `last_seen_at`
/// 最大者、再拿窗口内的行去过滤它 ——
/// 那个会话在窗口内一行都没有时，`aggregate(&[])` 给出**全 0**，而 `Some` 照样返回 ⇒ 浮窗
/// 显示「本会话 X 0」，与日档被禁掉的**是同一个假测量值**。真机高频可达：`miniBarRange`
/// 默认 `today`，而「最近被看到的会话」完全可以是昨天的（`last_seen_at` 由会话板上板刷新，
/// 与「本期有没有用量」无关）。
fn recent_session_with(
    conn: &rusqlite::Connection,
    rows: &[LedgerRow],
    granularity: RangeGranularity,
) -> Option<RecentSessionUsage> {
    if granularity == RangeGranularity::Day {
        return None;
    }
    // **候选只认「真的量到过请求输入」的行**（2026-10-06 修复轮①；只判「有行」是不够的）。
    // 这一层同时挡住三种形状：
    //  ① 窗口内一行都没有（`rows` 空 ⇒ 候选空 ⇒ 下面的 `?` 给 `None`）；
    //  ② 孤儿明细行（会话维度表里查不到 ⇒ 拿不到 title）；
    //  ③ **真空回合**：四桶全零、但 `requests ≥ 1` 的行（claude / opencode / zcode 都会落这种行，
    //     真机账本实测 **15 行 / 50 requests**；`semantics.rs` 刻意保留它并计一次请求）。
    //     它不是「本会话有用量」—— 只看「有行」的话仍会选中它，第④行照样印「本会话 X 0」，
    //     而那正是本方法要根除的假测量值。
    let present: std::collections::HashSet<(&str, &str)> = rows
        .iter()
        .filter(|r| r.request_total > 0)
        .map(|r| (r.source_id.as_str(), r.session_id.as_str()))
        .collect();
    let sessions = crate::database::dao::usage::load_sessions_conn(conn, None);
    // `?`：候选为空（窗口内没有任何会话量到过请求输入）或在会话维度表里都查不到 ⇒ 如实给 `None`。
    let latest = sessions
        .iter()
        .filter(|s| present.contains(&(s.source_id.as_str(), s.session_id.as_str())))
        .max_by_key(|s| s.last_seen_at)?;
    let mine: Vec<LedgerRow> = rows
        .iter()
        .filter(|r| r.source_id == latest.source_id && r.session_id == latest.session_id)
        .cloned()
        .collect();
    let (buckets, metrics) = aggregate(&mine);
    Some(RecentSessionUsage {
        source_id: latest.source_id.clone(),
        session_id: latest.session_id.clone(),
        title: latest.title.clone(),
        buckets,
        metrics,
    })
}

/// 工作小结（D15–D19）。**一律取明细表**（保留期内可得）：
/// * `sessions`：父会话数（D17 分层：`is_subagent = 0`）；
/// * `turns_per_tool`：逐工具，**不合计**（D16）；**只累加父会话的 turn**（D17 计数类分层，
///   与 `sessions` 同一口径，R3）；不可得的工具值 = `None`；
/// * 三层报错 + 打断：按可得源求和，逐源可得性进 `availability.per_source`；
/// * `longest_turn_per_tool`：逐工具 p50 + max，**不跨工具合计**（D19），无样本 → `None`；
/// * `top_tool` / `top_tool_ms`：合并各源 tool_stats 取第一（**如实呈现 `(0, ms>0)` 的跨小时条目**，W-39）。
fn work_summary_with(conn: &rusqlite::Connection, resolved: &ResolvedRange) -> WorkSummary {
    let from = match resolved.cur.granularity {
        RangeGranularity::Hour => resolved.cur.keys.first().cloned().unwrap_or_default(),
        RangeGranularity::Day => format!(
            "{}T00",
            resolved.cur.keys.first().cloned().unwrap_or_default()
        ),
    };
    let to = match resolved.cur.granularity {
        RangeGranularity::Hour => resolved.cur.keys.last().cloned().unwrap_or_default(),
        RangeGranularity::Day => format!(
            "{}T23",
            resolved.cur.keys.last().cloned().unwrap_or_default()
        ),
    };
    // W-02：先按时间窗过滤（`WHERE hour_key BETWEEN ?1 AND ?2`）再由 DAO 做 GROUP_CONCAT——
    // 30 天档下拼接量随「窗口内的行数」而非全表行数增长；本机量级（保留期 90 天）可接受，
    // 若将来样本暴涨，退路是 SQL 外聚合（属 DAO 改动，不在本层）。
    let rows = crate::database::dao::usage::query_counters_conn(conn, &from, &to);
    if rows.is_empty() {
        return WorkSummary::default(); // 全部 null（空态），绝不填 0
    }
    let mut per_source: BTreeMap<String, Vec<&crate::database::dao::usage::CounterAggRow>> =
        BTreeMap::new();
    for r in &rows {
        per_source.entry(r.source_id.clone()).or_default().push(r);
    }
    let mut turns_per_tool: BTreeMap<String, Option<i64>> = BTreeMap::new();
    let mut longest_turn_per_tool: BTreeMap<String, Option<LongestTurn>> = BTreeMap::new();
    let mut tool_stats: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let (mut em, mut et, mut e_tool, mut intr, mut calls, mut ms) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    let mut sessions = 0i64;
    for s in UsageSourceId::ALL {
        let Some(rs) = per_source.get(s.db_id()) else {
            if !caps_of(s).turn {
                turns_per_tool.insert(s.db_id().to_string(), None);
            }
            if !caps_of(s).longest_turn {
                longest_turn_per_tool.insert(s.db_id().to_string(), None);
            }
            continue;
        };
        let caps = caps_of(s);
        if caps.turn {
            // R3/D17：**计数类分层**——turn 数与会话数一样只计父会话（token 总量仍含子代理）
            turns_per_tool.insert(
                s.db_id().to_string(),
                Some(rs.iter().filter(|r| !r.is_subagent).map(|r| r.turns).sum()),
            );
        } else {
            turns_per_tool.insert(s.db_id().to_string(), None);
        }
        if caps.longest_turn {
            let mut samples: Vec<i64> = rs.iter().flat_map(|r| r.turn_ms.clone()).collect();
            longest_turn_per_tool.insert(s.db_id().to_string(), percentiles(&mut samples));
        } else {
            longest_turn_per_tool.insert(s.db_id().to_string(), None);
        }
        if caps.error_model {
            em += rs.iter().map(|r| r.error_model).sum::<i64>();
        }
        if caps.error_turn {
            et += rs.iter().map(|r| r.error_turn).sum::<i64>();
        }
        if caps.error_tool {
            e_tool += rs.iter().map(|r| r.error_tool).sum::<i64>();
        }
        if caps.interrupted {
            intr += rs.iter().map(|r| r.interrupted).sum::<i64>();
        }
        if caps.tool_calls {
            calls += rs.iter().map(|r| r.tool_calls).sum::<i64>();
            ms += rs.iter().map(|r| r.tool_ms).sum::<i64>();
        }
        // D17：会话数分层——只数父会话
        sessions += rs.iter().filter(|r| !r.is_subagent).count() as i64;
        // W-39：`(0, ms>0)` 的跨小时条目在此**原样合并**（不丢、不归零）
        for r in rs {
            for (name, (c, m)) in &r.tool_stats {
                let e = tool_stats.entry(name.clone()).or_insert((0, 0));
                e.0 += c;
                e.1 += m;
            }
        }
    }
    let any = |pick: fn(&super::caps::SourceCaps) -> bool| {
        UsageSourceId::ALL.iter().any(|s| pick(&caps_of(*s)))
    };
    WorkSummary {
        // 「会话数」= 明细表里出现过的父会话数（D17 分层后的会话数）
        sessions: Some(sessions),
        turns_per_tool,
        error_model: any(|c| c.error_model).then_some(em),
        error_turn: any(|c| c.error_turn).then_some(et),
        error_tool: any(|c| c.error_tool).then_some(e_tool),
        interrupted: any(|c| c.interrupted).then_some(intr),
        tool_calls: any(|c| c.tool_calls).then_some(calls),
        // calls == 0 时均值**不可得**（null），不得编造 0、不得除零
        tool_avg_ms: (calls > 0).then(|| ms / calls),
        top_tool: tool_stats
            .iter()
            .max_by_key(|(_, (c, _))| *c)
            .map(|(n, (c, _))| TopTool {
                name: n.clone(),
                count: *c,
            }),
        top_tool_ms: tool_stats
            .iter()
            .max_by_key(|(_, (_, m))| *m)
            .map(|(n, (_, m))| TopToolMs {
                name: n.clone(),
                ms: *m,
            }),
        longest_turn_per_tool,
    }
}

/// p50 + max（样本已聚合到逐工具；**含挂机**，D19 要求在 UI 标注）。
/// 形参是 `&mut [i64]`（不是 `&mut Vec<i64>`）：调用方传 `&mut samples`，deref 强转即可，
/// 只为过门禁 `clippy::ptr_arg`（`-D warnings` 下是 error；判据与期望值一字未改）。
fn percentiles(samples: &mut [i64]) -> Option<LongestTurn> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let n = samples.len();
    let median = if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2
    };
    Some(LongestTurn {
        p50: median,
        max: *samples.last().unwrap(),
    })
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::delta::{DetailDelta, DetailKey, SessionCounters, SessionDimDelta};
    // 计划① 在这里还列了 `use crate::services::usage::model::*;` —— 它是**多余**的：
    // `use super::*` 已经带进 `query` 自己的 `use super::model::*;` 绑定（D-01 的口诀：
    // 看 `super` 是谁——这里是 `query`，而 `query` 自己就 `use` 了 model），
    // 保留它会触发 `unused_imports`，在门禁 `-D warnings` 下是 error（D-10 同族）。
    // 计划① 的测试代码块漏了这一行：`chrono::Local.with_ymd_and_hms` 是 `TimeZone` trait 的方法，
    // 不导入就是 E0599（`query.rs` 模块级没有 `use chrono::TimeZone;`）。**已批准的最小修复**
    // （D-01/D-05/D-18 同族：任务书代码块不总是自洽，判据与期望值一字未改）。
    use chrono::TimeZone;

    fn local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("测试时刻必须存在（避开 DST 跳变）")
            .timestamp_millis()
    }

    fn range(preset: UsageRangePreset) -> UsageRange {
        UsageRange {
            preset,
            from: None,
            to: None,
        }
    }

    fn mem_empty() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 明细 + **日聚合**成对写入（真实路径是 `ledger::apply_delta`；单测直连 DAO，
    /// 必须显式折叠一次——只写 usage_detail 的话，日档用例会读到空表）
    fn write_pair(conn: &rusqlite::Connection, d: DetailDelta) {
        let daily = crate::services::usage::ledger::daily_rows_of(
            &d.source_id,
            &crate::services::usage::delta::SourceDelta {
                details: vec![d.clone()],
                ..Default::default()
            },
        );
        // 两个 DAO 都返回 `usize`（写入行数），**不是 `Result`** —— 对它调 `.unwrap()` 是
        // E0599（§3.2.3 FIX-2）。写入行数顺手当断言用：应写入 1 行。
        assert_eq!(
            crate::database::dao::usage::upsert_detail_conn(conn, &[d], 1),
            1,
            "明细应写入 1 行"
        );
        assert_eq!(
            crate::database::dao::usage::upsert_daily_conn(conn, &daily, 1),
            1,
            "日聚合应写入 1 行（与明细成对）"
        );
    }

    /// 完整行描述（新增用例用；`add_detail` 只覆盖「只给 input_fresh」的形态）
    #[derive(Clone)]
    struct RowSpec {
        buckets: UsageBuckets,
        request_total: i64,
        requests: i64,
        counters: SessionCounters,
        user_est: Option<i64>,
        model: String,
        provider: String,
        /// 供应商三态（Task 19 补的字段：CSV 的 `sourceKind` 列与卡内行的「最强三态」
        /// 都需要逐值可控；默认值与 `add_spec` 原先写死的 `Inferred` 一致，既有用例零影响）
        kind: SourceKind,
    }

    impl Default for RowSpec {
        fn default() -> Self {
            Self {
                buckets: UsageBuckets::default(),
                request_total: 0,
                requests: 1,
                counters: SessionCounters::default(),
                user_est: None,
                model: "m".into(),
                provider: "p".into(),
                kind: SourceKind::Inferred,
            }
        }
    }

    /// 行描述 → 明细 delta（`add_spec` / 「只写明细」/「只写日聚合」三个夹具共用）
    fn delta_of(
        source: &str,
        session: &str,
        project: &str,
        hour: &str,
        spec: RowSpec,
    ) -> DetailDelta {
        DetailDelta {
            source_id: source.into(),
            provider_kind: spec.kind,
            key: DetailKey {
                session_id: session.into(),
                hour_key: hour.into(),
                day_key: hour[..10].into(),
                project_key: project.into(),
                model: spec.model,
                provider: spec.provider,
            },
            buckets: spec.buckets,
            request_total: spec.request_total,
            requests: spec.requests,
            user_est: spec.user_est,
            cache_semantics: crate::services::usage::semantics::CacheSemantics::Exclusive,
            counters: spec.counters,
            ts_ms: 1,
        }
    }

    fn add_spec(
        conn: &rusqlite::Connection,
        source: &str,
        session: &str,
        project: &str,
        hour: &str,
        spec: RowSpec,
    ) {
        write_pair(conn, delta_of(source, session, project, hour, spec));
    }

    /// **只写明细**（不折叠日聚合）：用于锁「小时档读 `usage_detail`」这条取数路径
    /// ——若实现把小时档改成读日聚合，用例会真红（`write_pair` 会把两张表都写满，锁不住）。
    fn write_detail_only(conn: &rusqlite::Connection, d: DetailDelta) {
        assert_eq!(
            crate::database::dao::usage::upsert_detail_conn(conn, &[d], 1),
            1,
            "只写明细应写入 1 行"
        );
    }

    /// **只写日聚合**（不写明细）：用于锁「日档读 `usage_daily`」这条取数路径
    /// （真实路径是 `ledger::apply_delta`；这里复用它的折叠函数 `daily_rows_of`，不另造口径）
    fn write_daily_only(conn: &rusqlite::Connection, d: DetailDelta) {
        // 先取出 source_id（`daily_rows_of` 借它 + 同时要 move `d` → 不能同时借用同一结构体）
        let source = d.source_id.clone();
        let daily = crate::services::usage::ledger::daily_rows_of(
            &source,
            &crate::services::usage::delta::SourceDelta {
                details: vec![d],
                ..Default::default()
            },
        );
        assert_eq!(
            crate::database::dao::usage::upsert_daily_conn(conn, &daily, 1),
            1,
            "只写日聚合应写入 1 行（明细不写）"
        );
    }

    /// 直接构造一行账（`apply_filters` 这类纯函数用例用；**不走库**，所以不受去重/累加影响）
    fn lrow(source: &str, session: &str, project: &str, provider: &str, model: &str) -> LedgerRow {
        LedgerRow {
            bucket_key: "2026-10-03T10".into(),
            source_id: source.into(),
            session_id: session.into(),
            project_key: project.into(),
            project_label: project.to_uppercase(),
            provider: provider.into(),
            provider_kind: SourceKind::Inferred,
            model: model.into(),
            buckets: UsageBuckets {
                input_fresh: 1,
                ..Default::default()
            },
            request_total: 1,
            requests: 1,
            user_est: None,
        }
    }

    /// 简版：只给 `input_fresh`（其余桶为 0）——任务书 Step 1 那张 fixture 表用的形态。
    /// `#[allow(clippy::too_many_arguments)]`：任务书给的 helper 就是 8 参，而 clippy 默认阈值是 7
    /// （`-D warnings` 下是 error，D-05/D-18 同族的最小修复；本仓已有 8 处同款 allow）。
    #[allow(clippy::too_many_arguments)]
    fn add_detail(
        conn: &rusqlite::Connection,
        source: &str,
        session: &str,
        project: &str,
        hour: &str,
        input: i64,
        turns: i64,
        user_est: Option<i64>,
    ) {
        add_spec(
            conn,
            source,
            session,
            project,
            hour,
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: input,
                    ..Default::default()
                },
                request_total: input,
                counters: SessionCounters {
                    turns,
                    ..Default::default()
                },
                user_est,
                ..Default::default()
            },
        );
    }

    /// 会话维度登记（父会话）——同时充当「项目键 → 显示名原文」标签字典的来源。
    /// **四参**：源由调用方给（`SessionDimDelta` 里没有 `source_id`，见 Task 3 Interfaces）；
    /// 返回值是 `usize`（写入行数），不是 `Result`。
    fn add_session(
        conn: &rusqlite::Connection,
        source: &str,
        session: &str,
        project: &str,
        label: &str,
    ) {
        add_session_at(conn, source, session, project, label, 1, None, false);
    }

    /// 完整控制版（新增用例用）：可控 `last_seen_at` / `title` / `is_subagent`。
    /// 同款 8 参 allow（见 `add_detail`）。
    #[allow(clippy::too_many_arguments)]
    fn add_session_at(
        conn: &rusqlite::Connection,
        source: &str,
        session: &str,
        project: &str,
        label: &str,
        last_seen_at: i64,
        title: Option<String>,
        is_subagent: bool,
    ) {
        assert_eq!(
            crate::database::dao::usage::upsert_session_conn(
                conn,
                source,
                &[SessionDimDelta {
                    session_id: session.into(),
                    project_key: project.into(),
                    project_label: label.into(),
                    project_path_raw: format!("/p/{project}"),
                    project_realpath: format!("/p/{project}"),
                    title,
                    is_subagent,
                    parent_session_id: is_subagent.then(|| "parent".to_string()),
                    originator: None,
                    last_seen_at,
                }],
                1
            ),
            1,
            "会话维度应登记 1 行"
        );
    }

    fn mem_with_rows() -> rusqlite::Connection {
        let conn = mem_empty();
        // 明细：今日 10 点 claude 100/800/0/20（请求输入 900）、11 点 codex 500/0/0/100（请求输入 500）
        add_spec(
            &conn,
            "claude",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 100,
                    cache_read: 800,
                    output: 20,
                    ..Default::default()
                },
                request_total: 900,
                requests: 1,
                counters: SessionCounters {
                    turns: 2,
                    ..Default::default()
                },
                user_est: Some(7),
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "codex",
            "s2",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 500,
                    output: 100,
                    ..Default::default()
                },
                request_total: 500,
                requests: 1,
                counters: SessionCounters {
                    turns: 3,
                    ..Default::default()
                },
                user_est: None,
                ..Default::default()
            },
        );
        // 会话维度：s2 是子代理（D17 分层：会话数与 turn 数都不含它）。
        // **一批 = 一个源**（`upsert_session_conn` 的 `source_id` 是批级参数）→ 父子两个会话
        // 分属 claude / codex，必须分两次登记（合并成一次会把 s2 记到 claude 名下，
        // `query_counters_conn` 的 `LEFT JOIN usage_session` 按 (source_id, session_id) 连接，
        // 于是 codex 的 s2 拿不到 is_subagent=1 → D17 分层失效）。
        add_session(&conn, "claude", "s1", "proj", "Proj");
        add_session_at(&conn, "codex", "s2", "proj", "Proj", 1, None, true);
        conn
    }

    #[test]
    fn dashboard_hero_trend_and_layered_session_count() {
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        // hero = 请求输入 + 产出 = (900 + 500) + (20 + 100)
        assert_eq!(d.totals.request_total, 1400);
        assert_eq!(d.totals_buckets.output, 120);
        assert_eq!(d.hero, 1520);
        // 命中率 = 800 / 1400
        assert!((d.totals.cache_hit_rate - 800.0 / 1400.0).abs() < 1e-9);
        // 用户输入(估)：只有 claude 有值 → 汇总 7（不可得的源不填 0）
        assert_eq!(d.totals.user_est, Some(7));
        // 趋势：当日 = 小时桶（0 时到当前 12 时 → 13 个桶），命中两个点
        assert_eq!(d.trend.len(), 13, "当日 0 时到当前小时共 13 个桶");
        assert_eq!(d.trend.iter().filter(|t| t.buckets.total() > 0).count(), 2);
        assert_eq!(d.trend.first().unwrap().key, "2026-10-03T00");
        assert_eq!(d.trend.last().unwrap().key, "2026-10-03T12");
        // 按工具分组：claude / codex 各一行；按请求输入降序（900 > 500）
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[0].key, "claude");
        assert_eq!(d.rows[1].key, "codex");
        // D17：该组是否**只由子代理会话贡献**（供 UI 单列一层可展开）。
        // 本用例是**小时档**（Today → 小时桶）→ 该位有真值 `Some(bool)`（契约 §2 2026-10-03 裁决）
        assert_eq!(
            d.rows[0].is_subagent,
            Some(false),
            "小时档：claude 组含父会话 s1 → 不是「只由子代理贡献」"
        );
        assert_eq!(
            d.rows[1].is_subagent,
            Some(true),
            "小时档：codex 组只有子代理 s2 → D17 分层标记必须为真"
        );
        // D17：会话数分层（s2 是子代理，不计入）
        assert_eq!(d.work_summary.sessions, Some(1));
        // D16：turn 数只逐工具给、不合计；**R3：也只计父会话** → codex 的唯一会话是子代理 → 0
        assert_eq!(d.work_summary.turns_per_tool.get("claude"), Some(&Some(2)));
        assert_eq!(
            d.work_summary.turns_per_tool.get("codex"),
            Some(&Some(0)),
            "R3：子代理会话的 turn 不计入（强断言见 turns_per_tool_counts_parent_sessions_only）"
        );
        // D16/GC 7：workbuddy 无回合概念（caps 恒 false）→ 显式 null（不是 0，也不是「条目消失」）
        assert_eq!(
            d.work_summary.turns_per_tool.get("workbuddy"),
            Some(&None),
            "不可得的工具必须显式为 null"
        );
        // D19：最长单 turn 逐工具。workbuddy 无 duration 字段 → 显式 null（不得填 0）；
        // kimi 本区间**无样本** → 不出条目（「无数据」与「不可得」是两件事）
        assert_eq!(
            d.work_summary.longest_turn_per_tool.get("workbuddy"),
            Some(&None),
            "D19/GC 7：不可得 = 显式 null（不是 0）"
        );
        assert_eq!(
            d.work_summary.longest_turn_per_tool.get("kimi"),
            None,
            "本区间无样本的工具不出条目"
        );
        // 环比：上一周期（昨日同一时段）无数据 → 整体 None（不得显示 0% / NaN）
        assert!(d.compare.is_none());
        // 契约字段回显（GC 1）
        assert_eq!(d.range.preset, UsageRangePreset::Today);
        assert_eq!(d.group_by, UsageGroupBy::Tool);
    }

    #[test]
    fn availability_marks_unavailable_metrics_instead_of_zero() {
        let table = availability_table();
        let turn = table
            .iter()
            .find(|a| a.metric == UsageMetric::Turn)
            .unwrap();
        assert!(turn.available);
        assert!(turn.reason.as_deref().unwrap().contains("口径") || turn.per_source.is_some());
        let per = turn.per_source.as_ref().unwrap();
        assert_eq!(
            per.get("workbuddy"),
            Some(&false),
            "workbuddy 无回合概念 → false 而不是 0"
        );
        let user = table
            .iter()
            .find(|a| a.metric == UsageMetric::UserEst)
            .unwrap();
        let per = user.per_source.as_ref().unwrap();
        assert_eq!(per.get("claude"), Some(&true));
        assert_eq!(per.get("zcode"), Some(&false));
    }

    /// 可得性表的**形状锁**（新增）：8 条指标逐条覆盖、每条都带七源可得性、
    /// `available` 必须等于「至少一个源可得」（防「available 恒 true/false」的假绿）；
    /// 并把 D16/D19 点名的不可得源与文案纪律钉住。
    #[test]
    fn availability_table_covers_all_eight_metrics_with_per_source_truth() {
        let table = availability_table();
        let metrics: Vec<UsageMetric> = table.iter().map(|a| a.metric).collect();
        assert_eq!(
            metrics,
            vec![
                UsageMetric::Turn,
                UsageMetric::ErrorModel,
                UsageMetric::ErrorTurn,
                UsageMetric::ErrorTool,
                UsageMetric::Interrupted,
                UsageMetric::LongestTurn,
                UsageMetric::ToolCalls,
                UsageMetric::UserEst,
            ],
            "8 条逐源可得性条目，一条都不能少（D15–D19）"
        );
        for a in &table {
            let per = a
                .per_source
                .as_ref()
                .expect("逐源可得性必须逐条给 per_source（D16/D19 的逐源口径）");
            assert_eq!(per.len(), 7, "{:?} 的 per_source 必须覆盖七源", a.metric);
            assert_eq!(
                a.available,
                per.values().any(|v| *v),
                "{:?} 的 available 必须等于「至少一个源可得」（不得恒 true/false）",
                a.metric
            );
            assert!(
                a.reason.is_some(),
                "{:?} 必须给原因文案（UI tooltip）",
                a.metric
            );
        }
        let pick = |m: UsageMetric, k: &str| {
            table
                .iter()
                .find(|a| a.metric == m)
                .unwrap()
                .per_source
                .as_ref()
                .unwrap()
                .get(k)
                .copied()
        };
        assert_eq!(pick(UsageMetric::ErrorTurn, "claude"), Some(false));
        assert_eq!(pick(UsageMetric::ErrorTool, "opencode"), Some(false));
        assert_eq!(pick(UsageMetric::Interrupted, "kimi"), Some(false));
        assert_eq!(pick(UsageMetric::LongestTurn, "workbuddy"), Some(false));
        assert_eq!(pick(UsageMetric::UserEst, "kimi"), Some(true));
        assert_eq!(pick(UsageMetric::UserEst, "dsh"), Some(false));
        // 文案纪律（Task 17 已补逐工具维度）：dsh 的覆盖面必须如实写成「逐工具维度仅覆盖有原始日志的会话」，
        // 不得写成「全量可得」（也不得改成相反说法）。
        let tc = table
            .iter()
            .find(|a| a.metric == UsageMetric::ToolCalls)
            .unwrap();
        assert!(
            tc.reason.as_deref().unwrap().contains("逐工具维度"),
            "dsh 逐工具维度的覆盖面必须写明：{}",
            tc.reason.as_deref().unwrap()
        );
        let lt = table
            .iter()
            .find(|a| a.metric == UsageMetric::LongestTurn)
            .unwrap();
        assert!(
            lt.reason
                .as_deref()
                .unwrap()
                .contains("workbuddy 无 duration"),
            "D19：workbuddy 无 duration 字段必须写明"
        );
    }

    /// R3/D17：**turn 数只计父会话**（与会话数同一分层口径）。
    /// fixture：codex 的 s2 是子代理（turns=3）+ 新加父会话 s3（turns=5）→ 只算 5；
    /// 会话数 = s1(claude 父) + s3(codex 父) = 2（s2 不计）。
    #[test]
    fn turns_per_tool_counts_parent_sessions_only() {
        let conn = mem_with_rows();
        add_detail(&conn, "codex", "s3", "proj", "2026-10-03T12", 10, 5, None);
        add_session(&conn, "codex", "s3", "proj", "Proj");
        let now = local_ms(2026, 10, 3, 13, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert_eq!(
            d.work_summary.turns_per_tool.get("codex"),
            Some(&Some(5)),
            "R3：只累加父会话的 turn（3 属子代理，必须被剔除）"
        );
        assert_eq!(d.work_summary.sessions, Some(2), "会话数同样只数父会话");
        // token 总量**含子代理**（不受分层影响）
        assert_eq!(d.totals_buckets.input_fresh, 100 + 500 + 10);
        // 分层标记也要跟着变：该组现在同时含父会话 s3 → 不再是「只由子代理贡献」
        // （本用例是小时档 → 有真值；契约 §2 2026-10-03 裁决）
        let codex = d.rows.iter().find(|r| r.key == "codex").unwrap();
        assert_eq!(
            codex.is_subagent,
            Some(false),
            "R3 只改计数类；codex 组含父会话 s3 → Some(false)"
        );
    }

    /// R1：`userEst` 的「不可得就是 null」在**小时档与日档**都成立（不是 0）；
    /// 有值的源在两档都出真值（日聚合必须有该列，否则日档恒显示「—」）
    #[test]
    fn user_est_is_null_not_zero_in_both_hour_and_day_tier() {
        let conn = mem_with_rows();
        // zcode：读不到用户文本 → 两批写入都是 None（累加后仍是 None）
        add_detail(&conn, "zcode", "z1", "proj", "2026-10-03T10", 10, 0, None);
        add_detail(&conn, "zcode", "z1", "proj", "2026-10-03T10", 5, 0, None);
        add_session(&conn, "zcode", "z1", "proj", "Proj");
        let now = local_ms(2026, 10, 3, 13, 0);
        // 前提断言（§4.1 防假绿）：两批**同一行键**必须合并成 1 行，且 `user_est` 列在两张表里
        // 都真的是 NULL——若库里是 0，下面两条 `None` 断言就不可能成立，用例会红在更早的位置
        let (sum, rows): (i64, i64) = conn
            .query_row(
                "SELECT SUM(input_fresh), COUNT(*) FROM usage_detail WHERE source_id = 'zcode'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (sum, rows),
            (15, 1),
            "两批同键必须合并成 1 行（15 = 10 + 5）"
        );
        for table in ["usage_detail", "usage_daily"] {
            let n: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE source_id = 'zcode' AND user_est IS NULL"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                n, 1,
                "前提：{table} 里 zcode 的 user_est 必须是 NULL（不是 0）"
            );
        }
        // 小时档（今日）
        let hour = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert_eq!(
            hour.totals.user_est,
            Some(7),
            "汇总只累加有值的源（zcode 不贡献 0）"
        );
        let zrow = hour.rows.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(
            zrow.metrics.user_est, None,
            "R1：小时档里不可得的源是 null，不是 0"
        );
        let crow = hour.rows.iter().find(|r| r.key == "claude").unwrap();
        assert_eq!(crow.metrics.user_est, Some(7));
        // 日档（近 7 天 → 走 usage_daily）
        let day = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert_eq!(
            day.totals.user_est,
            Some(7),
            "R1：日档必须出真值（缺列会恒为 null）"
        );
        let zrow = day.rows.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(
            zrow.metrics.user_est, None,
            "R1：日档里不可得的源同样是 null，不是 0"
        );
        let crow = day.rows.iter().find(|r| r.key == "claude").unwrap();
        assert_eq!(crow.metrics.user_est, Some(7), "R1：日档与小时档同口径");
    }

    /// R2：同一会话内两个 cwd → **小时档与日档都**按记录级 projectKey 分成两组
    /// （两档口径不得分叉），且显示名走标签字典（projectName 原文）
    #[test]
    fn two_project_keys_in_one_session_are_grouped_separately() {
        let conn = mem_with_rows();
        // 同一会话 s1、同一小时、同一模型/供应商，只有 cwd 不同
        add_detail(&conn, "claude", "s1", "alpha", "2026-10-03T10", 30, 0, None);
        add_detail(&conn, "claude", "s1", "beta", "2026-10-03T10", 70, 0, None);
        // 标签字典（D21 显示名 = projectName 原文）：两个项目各自另有会话
        add_session(&conn, "claude", "s-alpha", "alpha", "Alpha");
        add_session(&conn, "claude", "s-beta", "beta", "Beta");
        let now = local_ms(2026, 10, 3, 13, 0);
        let hour = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            now,
        )
        .unwrap();
        let hkey: Vec<(&str, i64)> = hour
            .rows
            .iter()
            .map(|r| (r.key.as_str(), r.buckets.input_fresh))
            .collect();
        assert!(
            hkey.contains(&("alpha", 30)),
            "R2：小时档必须按记录级键分组，实测 {hkey:?}"
        );
        assert!(
            hkey.contains(&("beta", 70)),
            "R2：同一会话的第二个 cwd 必须自成一组，实测 {hkey:?}"
        );
        let alpha = hour.rows.iter().find(|r| r.key == "alpha").unwrap();
        assert_eq!(
            alpha.label, "Alpha",
            "D21：显示名取 projectName 原文（标签字典）"
        );
        // 日档：同一记录级键折叠，两档分组键一致
        let day = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Project,
            now,
        )
        .unwrap();
        let dkey: Vec<(&str, i64)> = day
            .rows
            .iter()
            .map(|r| (r.key.as_str(), r.buckets.input_fresh))
            .collect();
        assert!(
            dkey.contains(&("alpha", 30)),
            "R2：日档同一组，实测 {dkey:?}"
        );
        assert!(
            dkey.contains(&("beta", 70)),
            "R2：日档同一组，实测 {dkey:?}"
        );
        let alpha = day.rows.iter().find(|r| r.key == "alpha").unwrap();
        assert_eq!(
            alpha.label, "Alpha",
            "D21/R2：日档与小时档共用同一本标签字典"
        );
        let beta = day.rows.iter().find(|r| r.key == "beta").unwrap();
        assert_eq!(beta.label, "Beta");
    }

    /// W5：供应商不可得时的行标签必须是 **i18n 键**（zh/en 同父同键），不得硬编码中文
    #[test]
    fn unknown_provider_label_is_an_i18n_key_present_in_both_locales() {
        let row = LedgerRow {
            bucket_key: "2026-10-03T10".into(),
            source_id: "workbuddy".into(),
            session_id: "s1".into(),
            project_key: "alpha".into(),
            project_label: "ALPHA".into(),
            provider: String::new(), // 供应商不可得（workbuddy 实测）
            provider_kind: SourceKind::Unknown,
            model: "hy3".into(),
            buckets: UsageBuckets::default(),
            request_total: 0,
            requests: 0,
            user_est: None,
        };
        let (key, label, _) = group_of(&row, UsageGroupBy::Provider);
        assert_eq!(key, "", "供应商不可得 → key 为空串");
        assert_eq!(label, crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY);
        assert!(
            !label
                .chars()
                .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "W5：Rust 侧不得硬编码中文标签"
        );
        for locale in ["zh.json", "en.json"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../src/i18n/locales")
                .join(locale);
            let text = std::fs::read_to_string(&path).unwrap();
            let root: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                root.pointer("/usage/label/unknownProvider")
                    .and_then(|v| v.as_str())
                    .is_some(),
                "{locale} 缺 usage.label.unknownProvider（zh/en 同父同键）"
            );
        }
    }

    /// 环比（D14 / 契约 `CompareBlock`）的**另一半**：上一周期**有**数据时必须出完整聚合。
    /// 任务书原用例只断言「无数据 → None」——把 `compare` 改成**恒 None** 也能全绿，
    /// 这是本轮必须补上的真锁。
    #[test]
    fn compare_is_populated_from_the_previous_period() {
        let conn = mem_with_rows(); // 当日：claude 10 时（900 请求输入）+ codex 11 时（500）
                                    // 昨日同一时段（Today 档的上一周期 = 昨日 0 时到昨日 12 时）
        add_spec(
            &conn,
            "claude",
            "p1",
            "proj",
            "2026-10-02T05",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 200,
                    cache_read: 100,
                    output: 40,
                    ..Default::default()
                },
                request_total: 300,
                requests: 2,
                user_est: Some(3),
                ..Default::default()
            },
        );
        add_session(&conn, "claude", "p1", "proj", "Proj");
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let c = d
            .compare
            .expect("上一周期有数据 → 必须出对比段（否则 UI 永远看不到环比）");
        assert_eq!(c.prev_buckets.input_fresh, 200);
        assert_eq!(c.prev_buckets.cache_read, 100);
        assert_eq!(c.prev_buckets.cache_write, 0);
        assert_eq!(c.prev_buckets.output, 40);
        assert_eq!(c.prev_metrics.request_total, 300);
        assert_eq!(c.prev_metrics.requests, 2);
        assert!(
            (c.prev_metrics.cache_hit_rate - 100.0 / 300.0).abs() < 1e-9,
            "命中率必须与当期同一条纯函数（semantics::cache_hit_rate）"
        );
        assert_eq!(
            c.prev_metrics.user_est,
            Some(3),
            "R1：上一周期同样按「不可得不填 0」累加"
        );
        // 当期不得被上一周期污染，且两段窗口必须是不同的数据
        assert_eq!(d.totals.request_total, 1400);
        assert_eq!(d.hero, 1520);
        assert_eq!(c.prev_buckets.total(), 340);
        assert_ne!(
            c.prev_buckets.total(),
            d.totals_buckets.total(),
            "上一周期与当期必须是两段不同的窗口"
        );
        assert!(
            d.trend.iter().all(|t| t.key.starts_with("2026-10-03")),
            "趋势桶只能是当期窗口（不得混入上一周期）"
        );
    }

    /// 日档（7d/30d/自定义）的上一周期也必须走**日粒度**取数路径（`p.granularity`）；
    /// 若实现把上一周期一律按小时档取（或按当期取），这条会红。
    #[test]
    fn day_tier_compare_uses_the_previous_day_window() {
        let conn = mem_with_rows();
        // 近 7 天的上一周期 = 再往前 7 天：2026-09-20 ~ 2026-09-26
        add_spec(
            &conn,
            "codex",
            "old1",
            "proj",
            "2026-09-24T08",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 50,
                    output: 5,
                    ..Default::default()
                },
                request_total: 50,
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 13, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        // 当期 7 个日桶：09-27 ~ 10-03（含今日）
        assert_eq!(d.trend.len(), 7);
        assert_eq!(d.trend.first().unwrap().key, "2026-09-27");
        assert_eq!(
            d.trend.first().unwrap().label,
            "9/27",
            "日档标签 M/D（去前导零）"
        );
        assert_eq!(d.trend.last().unwrap().key, "2026-10-03");
        let c = d.compare.expect("前 7 天有数据 → 出对比段");
        assert_eq!(c.prev_buckets.input_fresh, 50, "上一周期走日聚合表");
        assert_eq!(c.prev_buckets.output, 5);
        assert_eq!(c.prev_metrics.request_total, 50);
        assert_eq!(c.prev_metrics.requests, 1);
        assert_eq!(
            d.totals_buckets.input_fresh,
            100 + 500,
            "当期不得把 09-24 的行算进来（否则就是「当期=上一周期」口径错）"
        );
    }

    /// 空账本（stub 边界：一行都没有 / 某源一行都没有 / 时间段无数据）→ **全 null 空态**，
    /// 绝不填 0；趋势仍按窗口出满桶（UI 自己判「暂无数据」）。
    #[test]
    fn empty_ledger_yields_null_empty_state_not_zero() {
        let conn = mem_empty();
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert!(d.rows.is_empty());
        assert_eq!(d.totals.request_total, 0);
        assert_eq!(d.totals.requests, 0);
        assert_eq!(d.totals_buckets.total(), 0);
        assert_eq!(d.hero, 0);
        assert_eq!(
            d.totals.cache_hit_rate, 0.0,
            "分母为 0 记 0（§4.3），空态判定交给 request_total"
        );
        // GC 7：不可得一律 null（绝不 0）——空账本下没有任何源有值
        assert_eq!(d.totals.user_est, None);
        assert_eq!(
            d.work_summary,
            WorkSummary::default(),
            "空账本的「工作小结」必须整块空态（全 null），不得填 0"
        );
        assert_eq!(d.work_summary.sessions, None);
        assert_eq!(d.work_summary.tool_calls, None);
        assert_eq!(d.work_summary.top_tool, None);
        assert!(d.compare.is_none());
        assert!(
            d.recent_session.is_none(),
            "无任何会话 → recentSession = null"
        );
        assert_eq!(d.trend.len(), 13, "空数据也要出满窗口的桶");
        assert!(d
            .trend
            .iter()
            .all(|t| t.buckets.total() == 0 && t.metrics.user_est.is_none()));
        assert_eq!(d.availability.len(), 8, "可得性表与数据无关（恒返回）");
    }

    /// 总开关关闭 → 空态；重新打开 → 同一库数据回来。
    /// 证明①门真的生效；②门读的是**传入连接**的设置（W-24：不碰进程级 `settings::load()`）。
    #[test]
    fn disabled_setting_returns_empty_dashboard_and_reenabling_restores_rows() {
        use crate::services::usage::settings::SETTINGS_KEY;
        let conn = mem_with_rows(); // 库里**有**数据
        let now = local_ms(2026, 10, 3, 12, 0);
        let off = serde_json::to_string(&UsageSettings {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        crate::database::dao::settings::set_setting_conn(&conn, SETTINGS_KEY, &off);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert!(d.rows.is_empty(), "总开关关闭 → 零行");
        assert_eq!(d.totals.request_total, 0);
        assert_eq!(d.totals.user_est, None, "空态仍是 null，不是 0");
        assert!(
            d.trend.is_empty(),
            "关闭时连桶都不出（与「开着但无数据」不同）"
        );
        assert!(d.recent_session.is_none());
        assert_eq!(d.work_summary, WorkSummary::default());
        assert_eq!(d.availability.len(), 8, "可得性说明照常返回");
        // **顺序效应**（如实固定，见报告「自审发现」）：本层先判总开关、后解析区间 →
        // 关闭态下「非法区间」不报错（空态优先）。打开态下同一非法区间**必须**回结构化错误码。
        let bad = UsageRange {
            preset: UsageRangePreset::Custom,
            from: None,
            to: None,
        };
        assert!(
            dashboard_with_conn(&conn, &bad, UsageGroupBy::Tool, now).is_ok(),
            "关闭态不解析区间（空态优先）"
        );
        // 对照（防假绿）：重新打开后同一个库必须出数据，否则上面的空态只是因为库空
        let on = serde_json::to_string(&UsageSettings {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        crate::database::dao::settings::set_setting_conn(&conn, SETTINGS_KEY, &on);
        let d2 = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert_eq!(d2.rows.len(), 2, "开关打开后同一库必须出 2 行");
        assert_eq!(d2.totals.request_total, 1400);
        // 打开态：非法区间 → 结构化错误码（契约 §1「采集类错误用结构化错误码」+ §3 要点）
        let err = dashboard_with_conn(&conn, &bad, UsageGroupBy::Tool, now)
            .expect_err("打开态下非法区间必须报错（不得静默回落成空态）");
        assert_eq!(err.code, "usage-range-invalid");
    }

    /// GC 3（只读账本、零扫描）+ W-26（持 `DB` 锁的路径**不得**调采集 → ABBA 死锁）的**结构性自省锁**。
    ///
    /// 为什么不用行为式断言（`collect::collect_calls()` 前后不变）：那个原子量与 `collect.rs` 的
    /// 采集用例**在同一个 lib 测试二进制里并行**共享，别人一 `fetch_add` 本用例就假红
    /// （W-22 已把这条行为断言留给 Task 20 的集成构建）。自省锁（`include_str!` 读自己的源码，
    /// Task 17 的 `every_test_in_this_file_takes_the_serial_lock` 同款）变异即可真红。
    ///
    /// **针必须拆开写**（`concat!`）：否则本用例自己的字符串字面量就会命中自己 → 恒真/恒假。
    #[test]
    fn query_layer_never_calls_collection_and_reads_only_cached_result() {
        let src = include_str!("query.rs");
        // 正向：数据截止时间只能走 W-26 的唯一安全形式（它只碰 LAST_RESULT、不碰飞行锁）
        let safe = concat!("collect::", "cached_result()");
        assert!(
            src.contains(safe),
            "查询层取「数据截止时间」必须经 cached_result()（W-26 安全形式）"
        );
        // 反向：采集入口一律不得出现在查询层（持 DB 锁调采集 = ABBA 死锁）
        for forbidden in [
            concat!("collect", "_with("),
            concat!("run", "_collection("),
            concat!("collect::", "collect("),
        ] {
            assert!(
                !src.contains(forbidden),
                "查询层不得出现 `{forbidden}`：它会扫描并取 DB → 与持 DB 锁的查询路径构成 ABBA 死锁（W-26）"
            );
        }
        // GC 3：零文件扫描——不得碰增量读入口与采集上下文
        for forbidden in [
            concat!("read_", "incremental"),
            concat!("Collect", "Context"),
        ] {
            assert!(
                !src.contains(forbidden),
                "查询层不得出现 `{forbidden}`（GC 3：只读账本、零文件扫描）"
            );
        }
    }

    /// D21 标签字典：同一 `project_key` 可能对应多个原文（Windows 上 claude 路径已小写、
    /// zcode 保留原文）→ 取**最近活跃**会话的原文（`last_seen_at` 倒序 + `or_insert`，确定性）。
    #[test]
    fn key_label_dict_prefers_the_most_recently_seen_session_label() {
        let conn = mem_empty();
        add_session_at(&conn, "claude", "old", "mam", "OldName", 100, None, false);
        add_session_at(&conn, "zcode", "new", "mam", "NewName", 900, None, false);
        // 前提：同一 project_key 必须真有两行不同原文，才谈得上「取最近」（否则假绿）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_session WHERE project_key = 'mam'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "前提：同一 project_key 应有两行");
        let labels = key_label_dict(&conn);
        assert_eq!(
            labels.get("mam").map(String::as_str),
            Some("NewName"),
            "同一项目键必须取最近活跃会话的原文（不得随查询顺序漂移）"
        );
        // 空 project_key / 空 label 不得进字典（W-01：Unknown 桶只有 "unknown" 一个键）
        add_session_at(&conn, "dsh", "blank", "", "", 999, None, false);
        let labels = key_label_dict(&conn);
        assert!(!labels.contains_key(""), "空 project_key 的会话不得进字典");
    }

    /// W-39 / Task 17 实测：`tool_stats` 里存在 `(0, ms>0)` 条目（跨小时调用的产物：次数落在
    /// `tool/call` 那一小时的行、耗时落在 `result` 那一小时的行）。
    /// 查询层**必须如实呈现**（不得删掉、不得归零）：Top 工具按耗时第一名要能看到它；
    /// 而「平均耗时」在 0 次调用时**不可得**（null，不是 0、不许除零）。
    #[test]
    fn zero_call_tool_stats_entry_keeps_its_duration() {
        let conn = mem_empty();
        let mut counters = SessionCounters {
            tool_ms: 500,
            ..Default::default()
        };
        counters.tool_stats.insert("Agent".to_string(), (0, 500));
        add_spec(
            &conn,
            "dsh",
            "h1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                counters,
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        // 前提断言：库里确实是 `(0 次调用, 500ms)` 那一形态（不是夹具写成 (1, 500)）
        let text: String = conn
            .query_row(
                "SELECT tool_stats FROM usage_detail WHERE source_id = 'dsh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            text, r#"{"Agent":[0,500]}"#,
            "前提：落库条目就是 (0 次调用, 500ms)"
        );
        assert_eq!(
            d.work_summary.top_tool_ms,
            Some(TopToolMs {
                name: "Agent".into(),
                ms: 500
            }),
            "W-39：有耗时无次数的条目不得被归零/删除"
        );
        assert_eq!(
            d.work_summary.top_tool,
            Some(TopTool {
                name: "Agent".into(),
                count: 0
            }),
            "W-39：0 次调用如实呈现（不得伪造成 1 次）"
        );
        assert_eq!(d.work_summary.tool_calls, Some(0));
        assert_eq!(
            d.work_summary.tool_avg_ms, None,
            "0 次调用 → 平均耗时不可得（null，不得 0、不得除零）"
        );
    }

    /// `recentSession`（契约 §2 / P2 第 4 行）：「本会话」= **最近有活动的会话**（不是当前打开的会话），
    /// 用量按**当前 range** 统计；`title` 直传（可空）。
    #[test]
    fn recent_session_is_the_most_recently_seen_session() {
        let conn = mem_with_rows();
        add_session_at(
            &conn,
            "claude",
            "s-late",
            "proj",
            "Proj",
            9_999,
            Some("最近会话".into()),
            false,
        );
        add_spec(
            &conn,
            "claude",
            "s-late",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 42,
                    output: 8,
                    ..Default::default()
                },
                request_total: 42,
                user_est: Some(1),
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let rs = d.recent_session.expect("有会话 → recentSession 必须有值");
        assert_eq!(
            (rs.source_id.as_str(), rs.session_id.as_str()),
            ("claude", "s-late"),
            "取 last_seen_at 最大者，不是当前打开的会话"
        );
        assert_eq!(rs.title.as_deref(), Some("最近会话"), "title 直传（可空）");
        assert_eq!(
            rs.buckets.input_fresh, 42,
            "只算**该会话**在当期窗口内的用量（不含同源其它会话）"
        );
        assert_eq!(rs.buckets.output, 8);
        assert_eq!(rs.metrics.request_total, 42);
        assert_eq!(rs.metrics.requests, 1);
        assert_eq!(rs.metrics.user_est, Some(1));
        assert_ne!(
            rs.metrics.request_total, d.totals.request_total,
            "本会话行不得等于全体总量"
        );
        // **已知分叉（如实断言现状，不视为正确口径；见模块文档「已知口径差异」）**：
        // 日聚合行不带 session_id → 日档 recentSession 恒 0。若 ② 裁决为「不可得 → null」
        // 或补会话级取数，本断言会红——这正是它存在的意义。
        let day = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert!(day.totals_buckets.total() > 0, "前提：日档总量有数据");
        // GC 7（评审判决 A）：日档无会话维度 → `recentSession` 必须**整体为 null**，
        // 不得给一个 `buckets` 全 0 的块（那会被 UI 显示成「本会话 0」＝假测量值）
        assert!(
            day.recent_session.is_none(),
            "日档不可得 → null（GC 7）；给 Some(全 0) 就是把「不可得」伪装成「真的是 0」"
        );
    }

    /// **C1 回归（2026-10-06 评审）**：小时档里 `last_seen_at` 最大的那个会话**在窗口内一行都没有**
    /// 时，**不得**返回 `Some(全 0)`——那是与日档被禁掉的**同一个假测量值**（UI 会显示「本会话 X 0」）。
    /// 判据两条：① 退到「窗口内**真的有行**」的会话里最新的那个；② 窗口内一个会话都没有 ⇒ 整体 `null`。
    ///
    /// 真机可达且高频：`miniBarRange` 默认 `today`，而「最近被看到的会话」完全可以是昨天的
    /// （`last_seen_at` 由会话板上板刷新，与「本期有没有用量」无关）。
    #[test]
    fn recent_session_never_fakes_a_zero_for_a_session_outside_the_window() {
        let conn = mem_empty();
        // ① 窗口内**有**行的会话（被看到的时刻更早）
        add_session_at(
            &conn,
            "claude",
            "s-in-window",
            "proj",
            "Proj",
            1_000,
            Some("窗口内".into()),
            false,
        );
        add_spec(
            &conn,
            "claude",
            "s-in-window",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 42,
                    output: 8,
                    ..Default::default()
                },
                request_total: 42,
                user_est: Some(1),
                ..Default::default()
            },
        );
        // ② 被看到得**更晚**、但只发生在窗口**之外**（另一个月）的会话 —— 它就是旧实现会选中的那个
        add_session_at(
            &conn,
            "codex",
            "s-outside",
            "proj",
            "Proj",
            9_999,
            Some("窗口外".into()),
            false,
        );
        add_spec(
            &conn,
            "codex",
            "s-outside",
            "proj",
            "2026-09-01T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 999,
                    ..Default::default()
                },
                request_total: 999,
                ..Default::default()
            },
        );

        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let rs = d
            .recent_session
            .expect("窗口内有会话（claude/s-in-window）⇒ 必须有值");
        assert_eq!(
            (rs.source_id.as_str(), rs.session_id.as_str()),
            ("claude", "s-in-window"),
            "必须退到**窗口内有行**的会话，而不是 `last_seen_at` 更大但窗口内一行为空的那个 \
             （旧实现会选 codex/s-outside 并返回全 0 ⇒ 浮窗印「本会话 Codex 0」）"
        );
        assert_eq!(
            rs.buckets.input_fresh, 42,
            "且绝不能是全 0 的假测量值（假 0 正是本条要防的形状）"
        );
        assert_eq!(rs.title.as_deref(), Some("窗口内"));

        // ③ 窗口内**一个会话都没有**（只有窗口外的会话）⇒ 整体 `null`，而不是 `Some(全 0)`
        let later = local_ms(2026, 10, 20, 12, 0);
        let d2 = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            later,
        )
        .unwrap();
        assert!(
            d2.recent_session.is_none(),
            "窗口内没有任何会话行 ⇒ `null`（不得给 `Some(全 0)`，那会被浮窗显示成「本会话 0」）"
        );
    }

    /// **真空回合不算「本会话有用量」**（2026-10-06 修复轮①）。
    ///
    /// 「窗口内**有行**」不足以排除假零：claude / opencode / zcode 都会落「四桶全零、但 `requests ≥ 1`」
    /// 的**真空回合**行（`semantics.rs` 刻意保留并计一次请求；真机账本实测 **15 行 / 50 requests**）。
    /// 只按「有行」选候选，仍会选中一个四桶全零的会话 ⇒ 浮窗第④行印「本会话 X **0**」——
    /// 与本次要根除的假测量值是**同一个形状**。故候选必须限定在「`request_total > 0`」的行上。
    #[test]
    fn vacuum_turn_rows_do_not_count_as_in_window_usage() {
        let conn = mem_empty();
        // ① 只有**真空回合**的会话，且 `last_seen_at` 最新（只判「有行」的实现会选中它）
        add_session_at(
            &conn,
            "claude",
            "s-vacuum",
            "proj",
            "Proj",
            9_999,
            Some("真空".into()),
            false,
        );
        add_spec(
            &conn,
            "claude",
            "s-vacuum",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets::default(), // 四桶全零
                request_total: 0,
                requests: 3, // 但确实计了 3 次请求（这就是真空回合）
                ..Default::default()
            },
        );
        // ② 窗口内**真有**请求输入的另一个会话（被看到的时刻更早）
        add_session_at(
            &conn,
            "codex",
            "s-real",
            "proj",
            "Proj",
            1_000,
            Some("真用量".into()),
            false,
        );
        add_spec(
            &conn,
            "codex",
            "s-real",
            "proj",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 500,
                    ..Default::default()
                },
                request_total: 500,
                ..Default::default()
            },
        );

        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert!(
            d.totals.requests >= 3 && !d.rows.is_empty(),
            "前提：真空回合把窗口撑成「非空」⇒ 看板不判空、浮窗 `live` 为真（假零正是在这个面上现形）"
        );
        let rs = d.recent_session.expect("窗口内有真有量的会话 ⇒ 必须有值");
        assert_eq!(
            (rs.source_id.as_str(), rs.session_id.as_str()),
            ("codex", "s-real"),
            "**真空回合不算用量**：不得因 `last_seen_at` 更大就选中 `s-vacuum`（那样第④行印「本会话 Claude 0」）"
        );
        assert_eq!(rs.buckets.input_fresh, 500);

        // ③ 整窗**只有**真空回合 ⇒ 整体 `null`，而不是 `Some(全 0)`
        let only_vacuum = mem_empty();
        add_session_at(
            &only_vacuum,
            "claude",
            "s-vacuum",
            "proj",
            "Proj",
            9_999,
            Some("真空".into()),
            false,
        );
        add_spec(
            &only_vacuum,
            "claude",
            "s-vacuum",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets::default(),
                request_total: 0,
                requests: 3,
                ..Default::default()
            },
        );
        let d3 = dashboard_with_conn(
            &only_vacuum,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert!(
            d3.recent_session.is_none(),
            "整窗只有真空回合 ⇒ `null`（`requests ≥ 1` 会让它看起来「有数据」，但对「本会话」而言没有量）"
        );
    }

    /// 取数辅助的边界：**空键集 → 零行**（不查表）；**非连续键集 → 只回键集内的行**
    /// （`keys.contains` 过滤不能省：区间查询按 `BETWEEN first..last` 取数，键集中间缺桶时
    /// 那些桶的行**必须**被剔掉，否则环比/趋势会混入窗口外的数据）。
    #[test]
    fn load_rows_for_keys_respects_the_key_set_and_empty_keys() {
        let conn = mem_with_rows();
        assert!(
            load_rows_for_keys(&conn, RangeGranularity::Hour, &[]).is_empty(),
            "空键集不得回任何行"
        );
        // 键集刻意**跳过 11 时**（该小时在库里有一行 codex 明细）→ 只有 10 时的 claude 行能回
        let keys: Vec<String> = vec!["2026-10-03T10".to_string(), "2026-10-03T12".to_string()];
        let picked = load_rows_for_keys(&conn, RangeGranularity::Hour, &keys);
        assert_eq!(
            picked.len(),
            1,
            "键集外的桶必须被剔除（区间查询的 BETWEEN 会把它带出来）"
        );
        assert_eq!(picked[0].bucket_key, "2026-10-03T10");
        assert_eq!(picked[0].source_id, "claude");
        assert_eq!(picked[0].project_key, "proj", "记录级项目键随行读回（R2）");
        let dkeys: Vec<String> = vec!["2026-10-03".to_string()];
        let onlyday = load_rows_for_keys(&conn, RangeGranularity::Day, &dkeys);
        assert_eq!(onlyday.len(), 2, "10-03 的两条日聚合（claude / codex）");
        assert!(onlyday.iter().all(|r| r.bucket_key == "2026-10-03"));
        assert!(
            onlyday.iter().all(|r| r.session_id.is_empty()),
            "**根因前提**（不是对输出的背认）：日聚合行不带会话 id ⇒ 日档任何按会话的判定都不可得；\
             它对输出的两个后果由 `recent_session_is_the_most_recently_seen_session`（日档 → null）\
             与 `subagent_flag_is_only_meaningful_in_the_hour_tier`（日档 → `None`）正向锁定"
        );
        // **日档也要有非连续键集**（否则 Day 分支的 `keys.contains` 过滤同样是零覆盖：
        // 单日键集时 `BETWEEN 2026-10-03..2026-10-03` 恰好只覆盖那一天，删掉过滤也抓不住）。
        // 库里 10-02 另有 1 行 → 键集取 {10-02, 10-04}，中间那天（10-03）的两行必须被剔除。
        add_spec(
            &conn,
            "codex",
            "prev",
            "proj",
            "2026-10-02T08",
            RowSpec::default(),
        );
        let dgap: Vec<String> = vec!["2026-10-02".to_string(), "2026-10-04".to_string()];
        let picked = load_rows_for_keys(&conn, RangeGranularity::Day, &dgap);
        assert_eq!(
            picked.len(),
            1,
            "日档：键集外的日（10-03）必须被剔除（BETWEEN 会把它带出来）"
        );
        assert_eq!(picked[0].bucket_key, "2026-10-02");
        assert_eq!(picked[0].source_id, "codex");
    }

    /// 评审判决 A 的第三半（**outcome 断言**，非根因断言）：行级 `is_subagent` 的**分叉结果**——
    /// 小时档能分层（codex 组只由子代理 s2 贡献 → `true`），**日档不可得 → `null`**
    /// （契约 §2 2026-10-03 用户裁决：`boolean | null`；「不知道」**不得**写成 `false`）。
    /// 本断言让口径**可见**：日档若回退成 `false`（`Some(false)`）会立刻变红。
    #[test]
    fn subagent_flag_is_only_meaningful_in_the_hour_tier() {
        let conn = mem_with_rows(); // codex 的 s2 是子代理；claude 的 s1 是父会话
        let now = local_ms(2026, 10, 3, 12, 0);
        let hour = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let hsub = |k: &str| hour.rows.iter().find(|r| r.key == k).unwrap().is_subagent;
        assert_eq!(
            hsub("codex"),
            Some(true),
            "小时档：codex 组只由子代理会话贡献 → Some(true)（D17 分层的依据可用）"
        );
        assert_eq!(
            hsub("claude"),
            Some(false),
            "小时档：claude 组含父会话 s1 → Some(false)"
        );
        let day = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        // 前提断言：日档**确实**有 codex 组（否则下面两条只是「组不存在」的假绿）
        assert!(
            day.rows.iter().any(|r| r.key == "codex"),
            "前提：日档必须有 codex 组"
        );
        let dsub = |k: &str| day.rows.iter().find(|r| r.key == k).unwrap().is_subagent;
        assert_eq!(
            dsub("codex"),
            None,
            "日档：日聚合不带 session_id → 该位**不可得** = None，**不是 false**\
             （2026-10-03 用户裁决；契约 §2 已改 `boolean | null`）"
        );
        assert_eq!(dsub("claude"), None, "日档：不可得 ≠ 「非子代理」");

        // —— 新口径锁（契约 §2 2026-10-03 用户裁决：日档 `null`，不是 `false`）——
        // 变异敏感性：把日档分支改回 `Some(false)` → 下面第一条断言真红（MA-1）。
        let dv = serde_json::to_value(&day).unwrap();
        let di = day.rows.iter().position(|r| r.key == "codex").unwrap();
        assert!(
            dv["rows"][di]
                .as_object()
                .unwrap()
                .contains_key("isSubagent"),
            "isSubagent 键必须在（`null` 也要出现，不得被 skip_serializing_if 省略）"
        );
        assert!(
            dv["rows"][di]["isSubagent"].is_null(),
            "日档：日聚合不带 session_id → 该位**不可得** = null（不是 false）；\
             契约 §2 已改 `boolean | null`"
        );
        // 反向对照：小时档在 wire 上是**真布尔**（`Option` 只在 `None` 时出 `null`）
        let hv = serde_json::to_value(&hour).unwrap();
        let hi = hour.rows.iter().position(|r| r.key == "codex").unwrap();
        assert_eq!(
            hv["rows"][hi]["isSubagent"],
            serde_json::json!(true),
            "小时档在 wire 上必须是真布尔 true（不得被可空化牵连）"
        );
    }

    /// D19 的**数值锁**（评审 Important #2）：`longestTurnPerTool` 的 p50 与 max。
    /// 四条边界各锁一个命门：
    /// * **奇数样本**（claude 100/200/1000 → p50=200、max=1000）——样本刻意让**均值(433)≠中位数**，
    ///   把「拿均值冒充 p50」的退化实现抓住；
    /// * **偶数样本**（codex 10/20/30/100 → p50=(20+30)/2=**25**、max=100）；
    /// * **caps 不可得**（workbuddy `longest_turn=false`，库里**却真有**样本 5/7）→ 必须 `None`
    ///   （不得 0，也不得把别的工具的样本混进来）；
    /// * **有行无样本**（opencode）→ `None`；**无行的源**（kimi）→ 不出条目。
    #[test]
    fn longest_turn_p50_max_odd_even_and_unavailable() {
        let conn = mem_empty();
        add_spec(
            &conn,
            "claude",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                counters: SessionCounters {
                    turns: 3,
                    turn_ms: vec![100, 200, 1000],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "codex",
            "s2",
            "proj",
            "2026-10-03T10",
            RowSpec {
                counters: SessionCounters {
                    turns: 4,
                    turn_ms: vec![10, 20, 30, 100],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "workbuddy",
            "s3",
            "proj",
            "2026-10-03T10",
            RowSpec {
                counters: SessionCounters {
                    turn_ms: vec![5, 7],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "opencode",
            "s4",
            "proj",
            "2026-10-03T10",
            RowSpec::default(),
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        // 前提断言：三行样本确已落库（否则下面全是假绿）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE turn_ms <> ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "前提：三行带 turn_ms 样本");
        let lt = |k: &str| d.work_summary.longest_turn_per_tool.get(k).cloned();
        assert_eq!(
            lt("claude"),
            Some(Some(LongestTurn {
                p50: 200,
                max: 1000
            })),
            "奇数样本：p50=200、max=1000（均值 433 ≠ p50，防「均值冒充中位数」）"
        );
        assert_eq!(
            lt("codex"),
            Some(Some(LongestTurn { p50: 25, max: 100 })),
            "偶数样本：p50=(20+30)/2=25、max=100"
        );
        assert_eq!(
            lt("workbuddy"),
            Some(None),
            "caps.longest_turn=false → 不可得（库里真有 5/7 也不得显示；D19/GC 7）"
        );
        assert_eq!(lt("opencode"), Some(None), "有行但无样本 → None（不得 0）");
        assert_eq!(
            lt("kimi"),
            None,
            "无行的源不出条目（「无数据」≠「不可得」）"
        );
    }

    /// D18 三层报错 + 打断 + 工具计数的**数值锁**（评审 Important #2）。
    /// 命门：① 四层都必须出**非零真值**（空账本用例走早退分支，`根本到不了求和`）；
    /// ② **caps 为 false 的层不得计入**（夹具在那些层故意填 `99`，漏 caps 门数值立刻炸）；
    /// ③ `tool_avg_ms > 0`；④ `topTool`/`topToolMs` 的**非零选择**；
    /// ⑤ `tool_stats` 的**跨源累加**；⑥ **并列时的确定性**。
    #[test]
    fn work_summary_error_layers_tool_counters_and_top_tools() {
        let conn = mem_empty();
        // 每源的 tool_stats 刻意设计（合并后见断言）：
        //   Alpha = claude(2,500) + codex(2,10) = **(4, 510)** ← 跨源累加；耗时第一名
        //   Zeta  = claude(4,5)                   ← 与 Alpha 次数并列（4），确定性取字典序最大者
        //   Weird = workbuddy(0,300)              ← W-39 形态：0 次调用却有耗时，**不得**被选为第一名
        let mut claude = SessionCounters {
            turns: 2,
            error_model: 3,
            error_turn: 99, // claude 的 caps.error_turn = false → 必须被剔除
            error_tool: 5,
            interrupted: 1,
            tool_calls: 4,
            tool_ms: 800,
            ..Default::default()
        };
        claude.tool_stats = [
            ("Alpha".to_string(), (2, 500)),
            ("Zeta".to_string(), (4, 5)),
        ]
        .into_iter()
        .collect();
        let mut codex = SessionCounters {
            turns: 1,
            error_model: 2,
            error_turn: 4,
            error_tool: 1,
            tool_calls: 2,
            tool_ms: 400,
            ..Default::default()
        };
        codex.tool_stats = [
            ("Alpha".to_string(), (2, 10)),
            ("Beta".to_string(), (2, 100)),
        ]
        .into_iter()
        .collect();
        let mut workbuddy = SessionCounters {
            error_model: 99, // workbuddy 的 caps.error_model = false → 剔除
            error_turn: 99,  // 同上（caps.error_turn = false）
            error_tool: 7,
            interrupted: 2,
            tool_calls: 1,
            tool_ms: 100,
            ..Default::default()
        };
        workbuddy.tool_stats = [("Weird".to_string(), (0, 300))].into_iter().collect();
        let opencode = SessionCounters {
            turns: 1,
            error_model: 1,
            error_turn: 1,
            error_tool: 99, // opencode 的 caps.error_tool = false → 剔除
            interrupted: 3,
            tool_calls: 0, // 0 次调用 + 50ms 耗时（W-39 形态）：`tool_avg_ms` 仍按**总** calls 算
            tool_ms: 50,
            ..Default::default()
        };
        // kimi 只用来锁**第四层**（打断）的 caps 门：它唯一 `caps.interrupted = false`。
        // 其余计数全 0（不影响上面任何一条期望值），只有 `interrupted: 99` 必须被剔除。
        let kimi = SessionCounters {
            interrupted: 99,
            ..Default::default()
        };
        for (src, sess, counters) in [
            ("claude", "s1", claude),
            ("codex", "s2", codex),
            ("workbuddy", "s3", workbuddy),
            ("opencode", "s4", opencode),
            ("kimi", "s5", kimi),
        ] {
            add_spec(
                &conn,
                src,
                sess,
                "proj",
                "2026-10-03T10",
                RowSpec {
                    counters,
                    ..Default::default()
                },
            );
            add_session(&conn, src, sess, "proj", "Proj");
        }
        // **日档窗口的「首日早期小时」样本**（下面日档断言专锁 `from = 首日 T00`）：
        // 近 7 天的首日是 09-27，这一行落在 09-27T01。若把日档窗口的 `from` 写成 `T23`，
        // 这一行会被整体丢出窗口 → 日档数值静默变少（评审后补的真锁，见变异 O1）。
        add_spec(
            &conn,
            "dsh",
            "s6",
            "proj",
            "2026-09-27T01",
            RowSpec {
                counters: SessionCounters {
                    error_model: 10,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        add_session(&conn, "dsh", "s6", "proj", "Proj");
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let w = &d.work_summary;
        assert_eq!(
            w.error_model,
            Some(6),
            "模型/传输层：claude 3 + codex 2 + opencode 1（workbuddy caps=false，其 99 必须被剔除）"
        );
        assert_eq!(
            w.error_turn,
            Some(5),
            "回合失败层：codex 4 + opencode 1（claude / workbuddy 该层 caps=false，各自的 99 被剔除）"
        );
        assert_eq!(
            w.error_tool,
            Some(13),
            "工具执行层：claude 5 + codex 1 + workbuddy 7（opencode caps=false，其 99 被剔除）"
        );
        assert_eq!(
            w.interrupted,
            Some(6),
            "用户打断单列：1+0+2+3（不计入任何错误层）；kimi 该层 caps=false，其 99 必须被剔除"
        );
        assert_eq!(w.tool_calls, Some(7), "工具调用总数（kimi 全 0，不改变）");
        assert_eq!(
            w.tool_avg_ms,
            Some(192),
            "1350ms / 7 次 = 192（>0；只有**总**调用为 0 才算不可得）"
        );
        assert_eq!(w.sessions, Some(5), "五个父会话（含 kimi 的空回合会话）");
        assert_eq!(w.turns_per_tool.get("claude"), Some(&Some(2)));
        assert_eq!(
            w.turns_per_tool.get("workbuddy"),
            Some(&None),
            "无回合概念 → null"
        );
        assert_eq!(w.turns_per_tool.get("zcode"), None, "无行的源不出条目");
        // 前提断言：`Alpha` **确实**出现在两个源的行里（否则「跨源累加」根本没被考到）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE tool_stats LIKE '%Alpha%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "前提：Alpha 分布在两个源的行里");
        assert_eq!(
            w.top_tool,
            Some(TopTool {
                name: "Zeta".into(),
                count: 4
            }),
            "按次数第一名：Alpha(2+2=4) 与 Zeta(4) 并列 → 取 BTreeMap 序最后者（确定性；换规则必须是有意为之）"
        );
        assert_eq!(
            w.top_tool_ms,
            Some(TopToolMs {
                name: "Alpha".into(),
                ms: 510
            }),
            "按耗时第一名：Alpha 510ms（跨源累加 500+10）；**不得**取零次调用的 Weird(300)"
        );
        // 日档同口径：工作小结的窗口是**整日**（`from=首日 T00` / `to=末日 T23`）——两端都要锁：
        // * 首日早期小时（09-27T01，dsh 的 error_model=10）→ 锁 `from = T00`（写成 `T23` 会丢掉它）；
        // * 末日的小时（10-03T10 那批）→ 锁 `to = T23`（写成 `T00` 会把它们全丢掉）。
        let dday = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        assert_eq!(
            dday.work_summary.error_model,
            Some(6 + 10),
            "日档：工作小结按整日窗口取数（from=首日 T00；首日 01 时的 10 必须在窗内）"
        );
        assert_eq!(
            dday.work_summary.error_tool,
            Some(13),
            "日档：末日 10 时的四行必须在窗内（to=末日的 T23）"
        );
        assert_eq!(dday.work_summary.tool_calls, Some(7));
        assert_eq!(dday.work_summary.tool_avg_ms, Some(192));
        assert_eq!(
            dday.work_summary.sessions,
            Some(6),
            "日档六个父会话（含 09-27 的 dsh）"
        );
    }

    /// 环比「无数据」判据的**边界**：上一周期**有量但 0 次请求**（累计型源的差值轮可能如此）
    /// 仍算「有数据」→ 必须出 `Some`（否则 UI 会静默不显示环比段）。
    /// 判据是 `requests == 0` **且** `total() == 0` **两个条件同时成立**，不是只看请求数。
    #[test]
    fn compare_distinguishes_zero_buckets_from_zero_requests() {
        let conn = mem_empty();
        // 昨日 08 时：7 token、0 次请求
        add_spec(
            &conn,
            "codex",
            "p1",
            "proj",
            "2026-10-02T08",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 7,
                    ..Default::default()
                },
                request_total: 7,
                requests: 0,
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            now,
        )
        .unwrap();
        let c = d
            .compare
            .expect("上一周期有量（哪怕 0 次请求）→ 必须出对比段");
        assert_eq!(c.prev_buckets.input_fresh, 7);
        assert_eq!(c.prev_metrics.request_total, 7);
        assert_eq!(c.prev_metrics.requests, 0);
        // 对照：当期确实无数据 → 总量为 0（`compare` 的 None 语义只针对**上一周期**）
        assert_eq!(d.totals_buckets.total(), 0);
        assert_eq!(d.totals.requests, 0);
    }

    /// 记录页（W6/D6/D7）：`groupBy` **只接受 `tool` | `project`**（决定**卡片**维度），
    /// 卡内行**恒为「供应商 / 模型」**；`provider` / `model` 入参必须报 `usage-groupby-invalid`
    #[test]
    fn records_groupby_accepts_only_tool_and_project() {
        let conn = mem_with_rows();
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let range = UsageRange {
            preset: UsageRangePreset::Today,
            from: None,
            to: None,
        };
        for bad in [UsageGroupBy::Provider, UsageGroupBy::Model] {
            let e =
                records_with_conn(&conn, &range, bad, &UsageFilters::default(), now).unwrap_err();
            assert_eq!(e.code, "usage-groupby-invalid", "记录页不接受 {bad:?}");
        }
        // 两值都必须能用
        for ok in [UsageGroupBy::Tool, UsageGroupBy::Project] {
            assert!(records_with_conn(&conn, &range, ok, &UsageFilters::default(), now).is_ok());
        }
    }

    #[test]
    fn records_make_one_card_per_group_with_provider_model_rows() {
        let conn = mem_with_rows();
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let range = UsageRange {
            preset: UsageRangePreset::Today,
            from: None,
            to: None,
        };
        // tool：每工具一卡；卡内行恒为「供应商 / 模型」（D6）
        let r = records_with_conn(
            &conn,
            &range,
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 2, "每工具一个卡片（D6）");
        let claude = r.cards.iter().find(|c| c.tool_id == "claude").unwrap();
        assert_eq!(claude.rows.len(), 1);
        assert_eq!(
            claude.rows[0].key, "p / m",
            "卡内行的键 = 供应商/模型对（不是只有供应商）"
        );
        assert_eq!(claude.rows[0].label, "p / m");
        assert_eq!(claude.buckets.input_fresh, 100);
        // project：每项目一卡（D7：记录页卡片分组），卡内行仍是「供应商 / 模型」
        let rp = records_with_conn(
            &conn,
            &range,
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(rp.cards.len(), 1, "本 fixture 全部落同一项目 → 一张卡");
        assert_eq!(
            rp.cards[0].tool_id, "proj",
            "groupBy=project 时 toolId 承载卡片分组键（契约字段名不变）"
        );
        assert_eq!(rp.cards[0].tool_label, "Proj");
        assert_eq!(
            rp.cards[0].rows.len(),
            1,
            "两工具的 (p, m) 对相同 → 合并成一行"
        );
        assert_eq!(rp.cards[0].buckets.input_fresh, 600, "100 + 500");
        // 筛选：只看 codex → 只剩一张卡
        let only = UsageFilters {
            tool_ids: Some(vec!["codex".into()]),
            ..Default::default()
        };
        let r2 = records_with_conn(&conn, &range, UsageGroupBy::Tool, &only, now).unwrap();
        assert_eq!(r2.cards.len(), 1);
        // parentsOnly：s2 是子代理 → 它的 token 一并剔除
        let po = UsageFilters {
            subagent_mode: Some(SubagentMode::ParentsOnly),
            ..Default::default()
        };
        let r3 = records_with_conn(&conn, &range, UsageGroupBy::Tool, &po, now).unwrap();
        assert!(
            r3.cards.iter().all(|c| c.tool_id != "codex"),
            "子代理会话整卡剔除"
        );
    }

    #[test]
    fn csv_has_stable_columns_and_escapes() {
        let conn = mem_with_rows();
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let csv = csv_with_conn(
            &conn,
            &UsageRange {
                preset: UsageRangePreset::Today,
                from: None,
                to: None,
            },
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind");
        assert!(lines.len() >= 3);
        assert!(lines[1].starts_with("claude,"), "按 requestTotal 倒序");
        assert!(csv.ends_with('\n'), "行尾必须有换行（Excel 兼容）");
        // 含逗号的标签必须被引号包裹（CSV 转义）
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    /// W5：CSV 是**导出文件**（给 Excel 看），不落 i18n 键——
    /// 供应商不可得时 label 列留空（UI 侧才用 `usage.label.unknownProvider`）
    ///
    /// **任务书勘误（已申报 F-2）**：原 fixture 用 `add_detail`（其默认 `provider = "p"`）造 workbuddy 行
    /// ——**根本没造出「供应商不可得」形态**，`group_of(Provider)` 的键会是 `"p"`，
    /// 于是 `find(starts_with("workbuddy"))` **取不到行、用例只能 panic**（它想锁的 W5 行为一次都没走到）。
    /// 判据（「CSV 不含 i18n 键」「不可得供应商的 label 列留空」）**一字未改**，只把夹具换成
    /// 真的不可得形态（`provider` 空串），并把整行逐字钉住（顺带成为一处数值锁）。
    #[test]
    fn csv_keeps_i18n_keys_out_of_the_export() {
        let conn = mem_with_rows();
        add_spec(
            &conn,
            "workbuddy",
            "w1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                provider: String::new(), // GC 7：不可得 = 空串（不是 "unknown"、不是 i18n 键）
                buckets: UsageBuckets {
                    input_fresh: 5,
                    ..Default::default()
                },
                request_total: 5,
                ..Default::default()
            },
        );
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let csv = csv_with_conn(
            &conn,
            &UsageRange {
                preset: UsageRangePreset::Today,
                from: None,
                to: None,
            },
            UsageGroupBy::Provider,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            !csv.contains(crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY),
            "CSV 不得出现 i18n 键：{csv}"
        );
        // 前提断言：库里真有一行 provider = ''（否则「没出现 i18n 键」只是因为没走到那条分支）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE provider = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "前提：必须真有一行「供应商不可得」");
        let row = csv
            .lines()
            .find(|l| l.starts_with(",,"))
            .unwrap_or_else(|| panic!("必须有空分组键（供应商不可得）的行：{csv}"));
        assert!(
            row.starts_with(",,5,"),
            "不可得供应商的 label 列留空：{row}"
        );
        assert_eq!(
            row, ",,5,0,0,0,5,0.000000,1,,inferred",
            "整行逐字（groupKey 与 label 都空 + 三态照出）：{row}"
        );
        // 反向对照（防「label 列恒空」的退化实现）：有供应商的那组必须照旧出 `p,p,...`
        let p_row = csv
            .lines()
            .find(|l| l.starts_with("p,p,"))
            .unwrap_or_else(|| panic!("有供应商的行丢了：{csv}"));
        assert_eq!(
            p_row, "p,p,600,800,0,120,1400,0.571429,2,7,inferred",
            "同一份 CSV 里正常供应商行不得被牵连置空"
        );
    }

    /// **CSV 的逐列数值锁**：整段文本逐字断言（表头 + 两行），把
    /// ① 表头与**列顺序**（11 列）② 每个数值列的取值 ③ 命中率 `{:.6}` 形态
    /// ④ `userEst` 不可得 = **空单元格**（不是 0）⑤ 行尾换行 ⑥ **无 BOM**（BOM 属 Task 21 落盘）
    /// 一次全锁住——任一列改序 / 漏列 / 换格式化 / 把 null 填 0 / 少一个换行，都会真红。
    #[test]
    fn csv_rows_carry_every_numeric_column_in_contract_order() {
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(
            csv,
            "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind\n\
             claude,claude,100,800,0,20,900,0.888889,1,7,inferred\n\
             codex,codex,500,0,0,100,500,0.000000,1,,inferred\n",
            "CSV 文本逐字锁：列顺序 / 逐列数值 / {{:.6}} 命中率 / null→空 / 行尾换行"
        );
        // BOM 是 Task 21（落盘命令）的事：本层文本**不得**带（否则落盘会再前置一次 → 双 BOM）
        assert!(
            !csv.starts_with('\u{feff}'),
            "CSV 文本本层不得带 BOM（落盘时才加）"
        );
        assert!(!csv.contains('\u{feff}'));
    }

    /// GC 7 的 CSV 侧边界：`userEst` **不可得 = 空单元格**，**真的是 0 = `0`**，有值 = 数值本身。
    /// 三者必须可区分（把 null 填 0、或把 0 也留空、或整列恒空，都会真红）。
    #[test]
    fn csv_user_est_cell_distinguishes_unavailable_from_a_real_zero() {
        let conn = mem_empty();
        for (src, input, ue) in [
            ("claude", 10i64, None),
            ("codex", 20, Some(0)),
            ("kimi", 30, Some(12)),
        ] {
            add_spec(
                &conn,
                src,
                "s1",
                "proj",
                "2026-10-03T10",
                RowSpec {
                    buckets: UsageBuckets {
                        input_fresh: input,
                        ..Default::default()
                    },
                    request_total: input,
                    user_est: ue,
                    ..Default::default()
                },
            );
        }
        // 前提断言（防假绿）：库里真的是「NULL / 0 / 12」三种形态（ORDER BY source_id = claude, codex, kimi）
        let cells: Vec<Option<i64>> = {
            let mut stmt = conn
                .prepare("SELECT user_est FROM usage_detail ORDER BY source_id")
                .unwrap();
            let rows = stmt.query_map([], |x| x.get(0)).unwrap();
            rows.map(|v| v.unwrap()).collect()
        };
        assert_eq!(
            cells,
            vec![None, Some(0), Some(12)],
            "前提：NULL / 0 / 12 三种形态都要真在库里"
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let cell_of = |k: &str| {
            csv.lines()
                .find(|l| l.starts_with(&format!("{k},")))
                .unwrap_or_else(|| panic!("缺 {k} 行：{csv}"))
                .split(',')
                .nth(9)
                .unwrap()
                .to_string()
        };
        assert_eq!(cell_of("claude"), "", "不可得 → **空单元格**（不是 0）");
        assert_eq!(
            cell_of("codex"),
            "0",
            "真的是 0 → 出 `0`（在场的 0 是数据）"
        );
        assert_eq!(cell_of("kimi"), "12");
    }

    /// **转义真能变红**：`groupKey`/`label` 含 `,` / `"` / 换行时，必须按 RFC 4180 引号包裹 + 双写引号；
    /// 不含特殊字符时**不得**包裹（防「一律加引号」的退化实现）；超长值**不得截断**。
    #[test]
    fn csv_escapes_commas_quotes_and_newlines_from_labels() {
        let conn = mem_empty();
        // 五个项目（requestTotal 递减 → 行序确定）：逗号 / 引号 / 换行 / 无特殊字符 / 超长
        let long_label = format!("{},tail", "L".repeat(5000));
        for (key, label, input) in [
            ("alpha", "A, B", 40i64),
            ("beta", "q\"uote", 30),
            ("gamma", "multi\nline", 20),
            ("delta", "plain", 10),
            ("longkey", long_label.as_str(), 5),
        ] {
            add_spec(
                &conn,
                "claude",
                key,
                key,
                "2026-10-03T10",
                RowSpec {
                    buckets: UsageBuckets {
                        input_fresh: input,
                        ..Default::default()
                    },
                    request_total: input,
                    ..Default::default()
                },
            );
            add_session(&conn, "claude", key, key, label);
        }
        // 前提：标签字典真的把四个特殊原文交了出来（否则下面只是「标签都等于键」的假绿）
        let dict = key_label_dict(&conn);
        assert_eq!(dict.get("alpha").map(String::as_str), Some("A, B"));
        assert_eq!(dict.get("beta").map(String::as_str), Some("q\"uote"));
        assert_eq!(dict.get("gamma").map(String::as_str), Some("multi\nline"));
        assert_eq!(dict.get("delta").map(String::as_str), Some("plain"));
        assert_eq!(
            dict.get("longkey").map(String::as_str),
            Some(long_label.as_str())
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(csv.contains("alpha,\"A, B\",40,"), "逗号 → 引号包裹：{csv}");
        assert!(csv.contains("beta,\"q\"\"uote\",30,"), "引号 → 双写：{csv}");
        assert!(
            csv.contains("gamma,\"multi\nline\",20,"),
            "换行 → 引号包裹（仍是一条记录）：{csv}"
        );
        assert!(
            csv.contains("delta,plain,10,"),
            "无特殊字符 → **不得**加引号：{csv}"
        );
        assert!(
            csv.contains(&format!("longkey,\"{long_label}\",5,")),
            "超长标签必须逐字透传（本层不截断、不省略）"
        );
        // `csv_escape` 自身的边界（含空串；**CR 也是记录分隔符**——见报告偏差申报 F-1）
        assert_eq!(csv_escape(""), "");
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("a\nb"), "\"a\nb\"");
        assert_eq!(
            csv_escape("a\rb"),
            "\"a\rb\"",
            "CR 不包裹会把一条记录劈成两条"
        );
    }

    /// **CSV 公式注入中和（2026-10-06 第三轮安全修复）**：导出的 `groupKey` / `label` 两列直接写原始
    /// 文本，而**项目维度下 `label` 就是项目文件夹名** ⇒ 一个 clone 到 `=cmd|'/C calc'!A0` 的仓库
    /// 目录名会让用 Excel / Sheets / LibreOffice 打开 CSV 的人**执行命令**（以 `=` `+` `-` `@`
    /// 开头的单元格都被当公式；`\t` / `\r` 是「隐形前缀」——Excel 先剥前导空白再判公式）。
    ///
    /// 修法 = 最前面加一个**单引号** `'`（三家用通用的「当作文本」前缀，不会显示成内容的一部分），
    /// **然后**再按 RFC 4180 判要不要双引号包裹——**两件事都要做**且顺序如此（加完前缀的串才是
    /// 被包裹的内容）。**能变红**：去掉中和那几行 → 本用例红（见报告「变异验证」）。
    #[test]
    fn csv_neutralizes_formula_prefixes_so_names_are_not_executed() {
        // ① 单测：五种前缀**逐个**中和（`=` 不是唯一触发字符；只处理 `=` 的实现会在②那一组红）
        assert_eq!(csv_escape("=1+1"), "'=1+1");
        assert_eq!(csv_escape("+A1"), "'+A1", "`+` 同样触发公式");
        assert_eq!(csv_escape("-A1"), "'-A1", "`-` 同样触发公式");
        assert_eq!(csv_escape("@SUM(A1)"), "'@SUM(A1)", "`@` 同样触发公式");
        assert_eq!(
            csv_escape("=cmd|'/C calc'!A0"),
            "'=cmd|'/C calc'!A0",
            "真机形态的目录名"
        );
        assert_eq!(
            csv_escape("\tTAB"),
            "'\tTAB",
            "制表符是「隐形前缀」：Excel 剥掉它之后照样判公式"
        );
        // **`\n` 同属「隐形前缀」**（2026-10-06 第四轮补）：加它的**代价为零** —— 中和判据只看
        // **首字符**，故既有 `a\nb`（首字符是 `a`）逐字不变（下一组的回归锚），只有**以 `\n` 开头**
        // 的标签才多一个 `'`。同时它自己又是 RFC 4180 的记录分隔符 ⇒ 这一例**两种处理同时生效**
        // （先加 `'`、再双引号包裹）。
        assert_eq!(csv_escape("\n=1+1"), "\"'\n=1+1\"");
        assert_eq!(csv_escape("\nPLAIN"), "\"'\nPLAIN\"");
        // ③ 两种处理**同时生效**：先加 `'`，再按 RFC 4180 包裹（`'=a,b` → `"'=a,b"`）
        assert_eq!(csv_escape("=a,b"), "\"'=a,b\"");
        assert_eq!(csv_escape("=a\"b"), "\"'=a\"\"b\"");
        assert_eq!(
            csv_escape("\rCR"),
            "\"'\rCR\"",
            "CR 既是隐形前缀又是记录分隔符"
        );
        // ④⑤ **防过度转义**：不含特殊字符的正常标签、以及空串，逐字不变
        assert_eq!(csv_escape("multiagents-manager"), "multiagents-manager");
        assert_eq!(csv_escape("DeepSeek-plugins"), "DeepSeek-plugins");
        assert_eq!(csv_escape(""), "");
        // ⑥ 既有边界（逗号 / 引号 / 换行）不带前缀时仍是老形态（回归锚）
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("a\nb"), "\"a\nb\"");
        assert_eq!(csv_escape("a\rb"), "\"a\rb\"");

        // ② 端到端：真机路径（`csv_with_conn` 的 `groupKey` / `label` 两列）里必须是中和后的形态
        let conn = mem_empty();
        add_spec(
            &conn,
            "claude",
            "evil",
            "evil",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 30,
                    ..Default::default()
                },
                request_total: 30,
                ..Default::default()
            },
        );
        add_session(&conn, "claude", "evil", "evil", "=cmd|'/C calc'!A0");
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            csv.contains("evil,'=cmd|'/C calc'!A0,30,"),
            "项目目录名以 `=` 开头时必须被中和成 `'=…`：{csv}"
        );
        assert!(
            !csv.contains("\nevil,=cmd") && !csv.starts_with("evil,=cmd"),
            "裸的公式前缀不得出现在 label 列：{csv}"
        );
    }

    /// 五个维度逐个可断言、组合是 AND、**空列表 = 不过滤**（不是「全滤掉」）、
    /// `parentsOnly` 只剔**真为子代理**的 `(源, 会话)` 对（表里查不到的一律按 false 保留）。
    #[test]
    fn apply_filters_covers_every_dimension_and_empty_list_means_no_filter() {
        let rows = vec![
            lrow("claude", "s1", "alpha", "p1", "m1"),
            lrow("claude", "s2", "beta", "p2", "m2"),
            lrow("codex", "s3", "alpha", "p1", "m2"),
            lrow("dsh", "s4", "beta", "p2", "m1"),
        ];
        let subs: std::collections::HashMap<(String, String), bool> =
            [(("claude".to_string(), "s2".to_string()), true)]
                .into_iter()
                .collect();
        let ids = |rs: &[LedgerRow]| {
            rs.iter()
                .map(|r| format!("{}/{}", r.source_id, r.session_id))
                .collect::<Vec<_>>()
        };
        let f = |filters: UsageFilters| apply_filters(rows.clone(), &filters, &subs);
        assert_eq!(ids(&f(UsageFilters::default())).len(), 4, "无筛选 → 全保留");
        assert_eq!(
            ids(&f(UsageFilters {
                tool_ids: Some(vec!["codex".into()]),
                ..Default::default()
            })),
            vec!["codex/s3"],
            "tool_ids"
        );
        assert_eq!(
            ids(&f(UsageFilters {
                projects: Some(vec!["beta".into()]),
                ..Default::default()
            })),
            vec!["claude/s2", "dsh/s4"],
            "projects"
        );
        assert_eq!(
            ids(&f(UsageFilters {
                providers: Some(vec!["p1".into()]),
                ..Default::default()
            })),
            vec!["claude/s1", "codex/s3"],
            "providers"
        );
        assert_eq!(
            ids(&f(UsageFilters {
                models: Some(vec!["m2".into()]),
                ..Default::default()
            })),
            vec!["claude/s2", "codex/s3"],
            "models"
        );
        assert_eq!(
            ids(&f(UsageFilters {
                tool_ids: Some(vec!["claude".into()]),
                models: Some(vec!["m2".into()]),
                ..Default::default()
            })),
            vec!["claude/s2"],
            "多维度是 AND"
        );
        // **空列表 = 不过滤**（`Some(vec![])` 与 `None` 同义）：写成 `Some(l) => l.iter().any(..)`
        // 会把全部行滤掉 → 这条真红。`include` 同样不得过滤任何行。
        assert_eq!(
            ids(&f(UsageFilters {
                tool_ids: Some(vec![]),
                projects: Some(vec![]),
                providers: Some(vec![]),
                models: Some(vec![]),
                subagent_mode: Some(SubagentMode::Include),
            }))
            .len(),
            4,
            "空列表与 include 都不得过滤"
        );
        assert!(
            f(UsageFilters {
                tool_ids: Some(vec!["nope".into()]),
                ..Default::default()
            })
            .is_empty(),
            "未知取值 → 空集（不是「忽略该筛选」）"
        );
        assert_eq!(
            ids(&f(UsageFilters {
                subagent_mode: Some(SubagentMode::ParentsOnly),
                ..Default::default()
            })),
            vec!["claude/s1", "codex/s3", "dsh/s4"],
            "parentsOnly：只剔真为子代理的 (源, 会话) 对；表里没有的对按 false 保留"
        );
    }

    /// `session_subagent_flags` 的键是 **(源, 会话) 对**（契约 §4 的会话维度主键就是二元组），
    /// 不是会话 id 单键。夹具刻意让 `claude/s2` 是父会话、`codex/s2` 是子代理：
    /// **只按 session_id 建表的实现会把 claude/s2 误判成子代理** → D17 分层静默出错。
    #[test]
    fn session_subagent_flags_keys_by_source_and_session() {
        let conn = mem_empty();
        add_session(&conn, "claude", "s2", "alpha", "Alpha"); // 父会话
        add_session_at(&conn, "codex", "s2", "alpha", "Alpha", 1, None, true); // 子代理（同名会话）
        add_session_at(
            &conn,
            "dsh",
            "s9",
            "beta",
            "Beta",
            1,
            Some("t".into()),
            true,
        );
        let flags = session_subagent_flags(&conn);
        assert_eq!(flags.len(), 3, "三个会话三行");
        assert_eq!(
            flags.get(&("claude".to_string(), "s2".to_string())),
            Some(&false),
            "只按 session_id 建表会在这里假真（两个源可以有同名会话）"
        );
        assert_eq!(
            flags.get(&("codex".to_string(), "s2".to_string())),
            Some(&true)
        );
        assert_eq!(
            flags.get(&("dsh".to_string(), "s9".to_string())),
            Some(&true)
        );
        assert_eq!(
            flags.get(&("dsh".to_string(), "s2".to_string())),
            None,
            "不存在的对必须查不到（不是 false）"
        );
    }

    /// **卡片聚合与排序的数值锁**（任务书只断言了 `inputFresh` 一个桶）：
    /// 卡的四桶 / 四口径必须是**卡内行的聚合**（不是首行、不是全局总量），
    /// 卡与卡内行都按 `requestTotal` 降序（并列时按分组键 = BTreeMap 序 → 确定性）。
    #[test]
    fn records_card_aggregates_and_sorting_are_locked() {
        let conn = mem_empty();
        // 三个工具，requestTotal 刻意**非单调**插入：codex 500 / claude 900 / dsh 700
        add_spec(
            &conn,
            "codex",
            "s2",
            "proj",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 500,
                    output: 100,
                    ..Default::default()
                },
                request_total: 500,
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "claude",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 100,
                    cache_read: 800,
                    output: 20,
                    ..Default::default()
                },
                request_total: 900,
                requests: 2,
                user_est: Some(7),
                ..Default::default()
            },
        );
        // dsh 一卡两行（两个「供应商 / 模型」对）：p2/m2 = 200、p1/m1 = 500
        add_spec(
            &conn,
            "dsh",
            "s3",
            "proj",
            "2026-10-03T09",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 200,
                    ..Default::default()
                },
                request_total: 200,
                model: "m2".into(),
                provider: "p2".into(),
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "dsh",
            "s3",
            "proj",
            "2026-10-03T09",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 500,
                    cache_write: 30,
                    output: 50,
                    ..Default::default()
                },
                request_total: 500,
                requests: 3,
                model: "m1".into(),
                provider: "p1".into(),
                ..Default::default()
            },
        );
        add_session(&conn, "dsh", "s3", "proj", "Proj");
        // 前提断言：dsh 的两行键不同、必须真落成两行（否则「卡内两行」根本没被考到）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE source_id = 'dsh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "前提：dsh 的两行必须是两行（model/provider 不同）");
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let keys: Vec<&str> = r.cards.iter().map(|c| c.tool_id.as_str()).collect();
        assert_eq!(
            keys,
            vec!["claude", "dsh", "codex"],
            "卡片按 requestTotal 降序（900 / 700 / 500）"
        );
        let dsh = r.cards.iter().find(|c| c.tool_id == "dsh").unwrap();
        assert_eq!(
            dsh.buckets,
            UsageBuckets {
                input_fresh: 700,
                cache_read: 0,
                cache_write: 30,
                output: 50
            },
            "卡四桶 = 卡内行之和"
        );
        assert_eq!(dsh.metrics.request_total, 700);
        assert_eq!(dsh.metrics.requests, 4, "1 + 3");
        assert_eq!(
            dsh.metrics.user_est, None,
            "两行都不可得 → 卡也是 null（不是 0）"
        );
        assert!(
            dsh.metrics.cache_hit_rate.abs() < 1e-12,
            "cache_read = 0 → 0.0（§4.3 分母为 0 记 0）"
        );
        let inner: Vec<(&str, i64)> = dsh
            .rows
            .iter()
            .map(|x| (x.key.as_str(), x.metrics.request_total))
            .collect();
        assert_eq!(
            inner,
            vec![("p1 / m1", 500), ("p2 / m2", 200)],
            "卡内行按 requestTotal 降序"
        );
        let claude = r.cards.iter().find(|c| c.tool_id == "claude").unwrap();
        assert!(
            (claude.metrics.cache_hit_rate - 800.0 / 900.0).abs() < 1e-9,
            "卡级命中率与逐行同一条纯函数（semantics::cache_hit_rate）"
        );
        assert_eq!(claude.metrics.user_est, Some(7));
        // **卡内行级**的口径断言（评审 Minor #4）：卡级断言用的是同一个 `aggregate`，抓不到
        // 「只改内层」的定向改写 → 这里把内层行的 `userEst` / `cacheHitRate` 直接钉住
        // （GC 7 在记录页内层行的静默假值落点：不可得不得变 0、有值源不得被填 0 牵连）
        assert_eq!(
            dsh.rows[0].metrics.user_est, None,
            "卡内行级 userEst：不可得就是 null（不是 0）"
        );
        assert_eq!(dsh.rows[1].metrics.user_est, None, "另一内层行同样是 null");
        assert!(
            (claude.rows[0].metrics.cache_hit_rate - 800.0 / 900.0).abs() < 1e-9,
            "**卡内行级**命中率（只改内层的定向变异在这里红）"
        );
        assert_eq!(
            claude.rows[0].metrics.user_est,
            Some(7),
            "**卡内行级** userEst 出真值"
        );
        // 一致性不变量：卡 == 卡内行之和（改任一行的聚合口径都会红）；且不得出现空卡
        for c in &r.cards {
            let mut b = UsageBuckets::default();
            let (mut rt, mut req) = (0i64, 0i64);
            for x in &c.rows {
                b.add(&x.buckets);
                rt += x.metrics.request_total;
                req += x.metrics.requests;
            }
            assert_eq!(b, c.buckets, "卡 {} 的四桶必须等于卡内行之和", c.tool_id);
            assert_eq!(
                (rt, req),
                (c.metrics.request_total, c.metrics.requests),
                "卡 {} 的请求输入/次数必须等于卡内行之和",
                c.tool_id
            );
            assert!(!c.rows.is_empty(), "不得出现空卡（某源无行就不出卡）");
        }
    }

    /// `groupBy = project`（D7）的数值锁：卡 = 项目（`toolId` 承载项目键、`toolLabel` 是原文），
    /// 卡内行仍是「供应商 / 模型」——**跨工具同 (供应商, 模型) 必须合并成一行**，
    /// 且卡的四桶/四口径是**合并值**（任务书那条只断言了 `inputFresh` 一个桶）。
    #[test]
    fn records_project_cards_merge_tools_within_one_project() {
        let conn = mem_empty();
        add_spec(
            &conn,
            "claude",
            "s1",
            "alpha",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 100,
                    cache_read: 800,
                    output: 20,
                    ..Default::default()
                },
                request_total: 900,
                user_est: Some(7),
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "codex",
            "s2",
            "alpha",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 500,
                    output: 100,
                    ..Default::default()
                },
                request_total: 500,
                requests: 2,
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "dsh",
            "s3",
            "beta",
            "2026-10-03T11",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 77,
                    ..Default::default()
                },
                request_total: 77,
                model: "m2".into(),
                ..Default::default()
            },
        );
        add_session(&conn, "claude", "s1", "alpha", "Alpha");
        add_session(&conn, "dsh", "s3", "beta", "Beta");
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let keys: Vec<(&str, &str)> = r
            .cards
            .iter()
            .map(|c| (c.tool_id.as_str(), c.tool_label.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![("alpha", "Alpha"), ("beta", "Beta")],
            "卡 = 项目（按 requestTotal 降序），toolLabel 是项目原文（D21）"
        );
        let alpha = &r.cards[0];
        assert_eq!(alpha.rows.len(), 1, "跨工具同 (供应商, 模型) 合并成一行");
        assert_eq!(alpha.rows[0].key, "p / m");
        assert_eq!(
            alpha.buckets,
            UsageBuckets {
                input_fresh: 600,
                cache_read: 800,
                cache_write: 0,
                output: 120
            }
        );
        assert_eq!(alpha.metrics.request_total, 1400);
        assert_eq!(alpha.metrics.requests, 3, "1 + 2");
        assert_eq!(
            alpha.metrics.user_est,
            Some(7),
            "只有 claude 有值 → 合并后是 7（不可得的源不贡献 0）"
        );
        assert!((alpha.metrics.cache_hit_rate - 800.0 / 1400.0).abs() < 1e-9);
        assert_eq!(
            r.cards[1].rows[0].key, "p / m2",
            "另一项目的行照常按「供应商 / 模型」"
        );
    }

    /// **三处 `sourceKind` 折叠口径统一**（评审 Minor #2 / 裁决 A）：同一个契约字段的三个出口
    /// （看板分组行 / 记录页卡内行 / CSV 分组行）**都必须取组内最强三态**，不得「首行胜出」。
    /// 夹具刻意让**较弱**的那行**排在前面**（先插入；`query_detail_conn` 无 `ORDER BY` → 按 rowid 序返回）
    /// —— 这一点用**前提断言**钉住，否则「首行恰好是强态」会让用例变成假绿。
    #[test]
    fn source_kind_folding_is_strongest_in_all_three_exits() {
        let conn = mem_empty();
        // 先插弱态（unknown）、后插强态（measured）：同一 source / 同一 (供应商, 模型) → 三处都合成一组
        add_spec(
            &conn,
            "claude",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                kind: SourceKind::Unknown,
                buckets: UsageBuckets {
                    input_fresh: 1,
                    ..Default::default()
                },
                request_total: 1,
                ..Default::default()
            },
        );
        add_spec(
            &conn,
            "claude",
            "s2",
            "proj",
            "2026-10-03T10",
            RowSpec {
                kind: SourceKind::Measured,
                buckets: UsageBuckets {
                    input_fresh: 2,
                    ..Default::default()
                },
                request_total: 2,
                ..Default::default()
            },
        );
        // 前提断言：DAO 返回序真的是「弱在前」（不是 → 夹具失效，用例必须响亮地红）
        let raw =
            crate::database::dao::usage::query_detail_conn(&conn, "2026-10-03T00", "2026-10-03T23");
        assert_eq!(raw.len(), 2, "前提：两行明细都在时间窗内");
        assert_eq!(
            (raw[0].provider_kind, raw[1].provider_kind),
            (SourceKind::Unknown, SourceKind::Measured),
            "前提：**较弱的那行排在最前**（否则「首行胜出」的退化实现也会绿 = 假绿）"
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let today = range(UsageRangePreset::Today);
        // ① 看板分组行（Task 18 的 dashboard_with_conn）
        let d = dashboard_with_conn(&conn, &today, UsageGroupBy::Tool, now).unwrap();
        assert_eq!(d.rows.len(), 1, "前提：两行合成一组");
        assert_eq!(
            d.rows[0].source_kind,
            SourceKind::Measured,
            "看板分组行：取最强"
        );
        // ② 记录页卡内行
        let r = records_with_conn(
            &conn,
            &today,
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 1);
        assert_eq!(r.cards[0].rows.len(), 1, "前提：卡内合成一行");
        assert_eq!(
            r.cards[0].rows[0].source_kind,
            SourceKind::Measured,
            "记录页卡内行：取最强"
        );
        // ③ CSV 分组行
        let csv = csv_with_conn(
            &conn,
            &today,
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(
            csv.lines().nth(1).unwrap().rsplit(',').next().unwrap(),
            "measured",
            "CSV 分组行：取最强：{csv}"
        );
        // 对照（防「三态恒 measured」的退化实现）：只有弱态时必须是 unknown
        let conn2 = mem_empty();
        add_spec(
            &conn2,
            "claude",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                kind: SourceKind::Unknown,
                buckets: UsageBuckets {
                    input_fresh: 1,
                    ..Default::default()
                },
                request_total: 1,
                ..Default::default()
            },
        );
        let d2 = dashboard_with_conn(&conn2, &today, UsageGroupBy::Tool, now).unwrap();
        assert_eq!(
            d2.rows[0].source_kind,
            SourceKind::Unknown,
            "只有弱态 → unknown（不得恒 measured）"
        );
    }

    /// **D-43 的披露扩到记录页的强制卡内行**（评审 Minor #3 / 裁决 C）：dsh 真机 **63/170** 个投影文件
    /// **结构性**无 `modelSelection`，采集侧按 D-43 **保留** `model=""` 行（「数据 > 分组整洁」）。
    /// 而记录页的卡内行维度**恒为「供应商 / 模型」** → 这些行产出 **key / label 双空**的卡内行
    /// （② 会渲染成一行空白）。契约把 `key` / `label` 定为**非空 `string`**，GC 7 的 `null` 在此**不可表达**；
    /// locale 里只有 `usage.label.unknownProvider`、**没有** `unknownModel` → 本层**不改契约、不发明 i18n 键**，
    /// 只把现状钉住（② 若要兜底文案，须按契约变更纪律先回写）。
    /// 另一形态（**防御性写清，真机当前不可达**）：`provider` 非空 + `model` 空 → `"p / "` **尾分隔符**标签。
    #[test]
    fn records_inner_row_keeps_structurally_model_less_rows_as_empty_labels() {
        let conn = mem_empty();
        // dsh：结构性无模型（连供应商也不可得）
        add_spec(
            &conn,
            "dsh",
            "s1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                provider: String::new(),
                model: String::new(),
                kind: SourceKind::Unknown,
                buckets: UsageBuckets {
                    input_fresh: 10,
                    ..Default::default()
                },
                request_total: 10,
                ..Default::default()
            },
        );
        // codex：供应商在、模型空（防御性形态）
        add_spec(
            &conn,
            "codex",
            "s2",
            "proj",
            "2026-10-03T10",
            RowSpec {
                provider: "p".into(),
                model: String::new(),
                buckets: UsageBuckets {
                    input_fresh: 20,
                    ..Default::default()
                },
                request_total: 20,
                ..Default::default()
            },
        );
        // 前提断言：两条**空模型**明细真落库了（否则下面只是「没有这种行」的假绿）
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE model = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "前提：两行空模型必须真在库里（D-43 的形态）");
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let dsh = r.cards.iter().find(|c| c.tool_id == "dsh").unwrap();
        assert_eq!(
            dsh.rows[0].key, "",
            "结构性无模型 → 卡内行键为空串（契约非空 string，null 不可表达）"
        );
        assert_eq!(
            dsh.rows[0].label, "",
            "标签同样为空串（locale 无 unknownModel；本层不发明键）"
        );
        assert_eq!(
            dsh.rows[0].source_kind,
            SourceKind::Unknown,
            "三态照实给 unknown"
        );
        assert_eq!(
            dsh.rows[0].buckets.input_fresh, 10,
            "四桶照实呈现（D-43：数据 > 分组整洁）"
        );
        let codex = r.cards.iter().find(|c| c.tool_id == "codex").unwrap();
        assert_eq!(
            codex.rows[0].key, "p / ",
            "provider 非空 + model 空 → **尾分隔符**标签（真机当前不可达，防御性钉住）"
        );
        // CSV 侧同一形态（**2026-10-06 裁决后**）：`Model` 分组的键**含供应商** ⇒ 这两个
        // 结构性无模型的行**不再合并成一组**——`provider="p" / model=""` 自成一组（键与标签都是
        // 尾分隔符形态 `"p / "`，与上面卡内行那条防御性钉子同形），`provider="" / model=""` 单独
        // 落空键组；两组各自照实给三态（inferred / unknown）。
        // ⚠️ 旧断言钉的是「两行落同一个空键组、合成 30」——那正是本次要改掉的行为（键不含供应商
        // 时，两家供应商的同名模型会互相吃掉）。故这里**正向断言两组各自的整行**，并**反向断言**
        // 旧的合并组已不存在（只改正向断言的话，实现若退回旧行为仍可能蒙混过关）。
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Model,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            csv.contains("\np / ,p / ,20,0,0,0,20,0.000000,1,,inferred\n"),
            "provider 非空 + model 空 → **自成一组**且键含供应商（尾分隔符形态，与卡内行同形）：{csv}"
        );
        assert!(
            csv.contains("\n,,10,0,0,0,10,0.000000,1,,unknown\n"),
            "供应商与模型双空 → 单独落空键组，三态照实给 unknown：{csv}"
        );
        assert!(
            !csv.contains("\n,,30,"),
            "旧的「两行合并成空键组 30」行为已被 2026-10-06 裁决取代（键必须含供应商）：{csv}"
        );
    }

    /// §8.3：供应商不可得（空串）时，**卡内行只呈现模型名**（键与标签都是模型名），
    /// 且**不得**落 i18n 键（i18n 键是 dashboard 分组标签的用法；这一行的维度是模型）。
    #[test]
    fn records_inner_row_falls_back_to_model_name_when_provider_is_unavailable() {
        let conn = mem_empty();
        add_spec(
            &conn,
            "workbuddy",
            "w1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                provider: String::new(),
                model: "hy3".into(),
                kind: SourceKind::Unknown,
                buckets: UsageBuckets {
                    input_fresh: 5,
                    ..Default::default()
                },
                request_total: 5,
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 1);
        assert_eq!(r.cards[0].rows[0].key, "hy3", "供应商不可得 → 键就是模型名");
        assert_eq!(r.cards[0].rows[0].label, "hy3");
        assert_ne!(
            r.cards[0].rows[0].label,
            crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY,
            "记录页这一行不是 i18n 键的载体"
        );
        assert_eq!(
            r.cards[0].rows[0].source_kind,
            SourceKind::Unknown,
            "三态照实传"
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains(crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY),
            "记录页卡内行不得落 i18n 键：{json}"
        );
    }

    /// **2026-10-06 用户裁决**：大看板的 `model` 维度 = 「供应商 / 模型」（与记录页卡内行**同一形状**，
    /// 两处共用 `route_label`）。两条硬判据：
    ///  ① **键必须含供应商**：同一个模型挂在两个供应商下必须是**两组**——键里没有供应商，两家的同名
    ///     模型就会被折叠成一组（本次要堵的缺陷）。判据用**各自的量**（10 / 20）而非只数行数：
    ///     折叠成一组时那一组的量是 30，能真红；
    ///  ② 供应商不可得（空串，workbuddy 实测形态）→ 回**裸模型名**（§8.3），不得出「 / 模型」这种
    ///     以分隔符开头的标签。
    /// 另加**交叉锁**：同一条明细在记录页卡内行与看板 `model` 维度下的键**逐字相同**——两处一旦
    /// 各写一份实现（或改了一处忘了另一处），本用例红。
    #[test]
    fn model_dimension_key_is_provider_slash_model_and_falls_back_to_bare_model_name() {
        const SHARED: &str = "deepseek-v4.1-flash";
        let conn = mem_empty();
        // 同模型、两个供应商：量分别 10 / 20（被折叠时会是 30）
        for (source, provider, input) in
            [("claude", "anthropic", 10i64), ("codex", "volcengine", 20)]
        {
            add_spec(
                &conn,
                source,
                "s1",
                "proj",
                "2026-10-03T10",
                RowSpec {
                    provider: provider.into(),
                    model: SHARED.into(),
                    kind: SourceKind::Measured,
                    buckets: UsageBuckets {
                        input_fresh: input,
                        ..Default::default()
                    },
                    request_total: input,
                    ..Default::default()
                },
            );
        }
        // 供应商不可得（workbuddy 实测形态）：键与标签都是裸模型名
        add_spec(
            &conn,
            "workbuddy",
            "w1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                provider: String::new(),
                model: "hy3".into(),
                kind: SourceKind::Unknown,
                buckets: UsageBuckets {
                    input_fresh: 5,
                    ..Default::default()
                },
                request_total: 5,
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);

        let d = dashboard_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Model,
            now,
        )
        .unwrap();
        let by_key: Vec<(&str, i64)> = d
            .rows
            .iter()
            .map(|r| (r.key.as_str(), r.buckets.input_fresh))
            .collect();
        assert_eq!(
            by_key.len(),
            3,
            "同模型的两种供应商不得被折叠成一组：{by_key:?}"
        );
        assert!(
            by_key.contains(&("anthropic / deepseek-v4.1-flash", 10)),
            "键必须含供应商（且量与行对应）：{by_key:?}"
        );
        assert!(
            by_key.contains(&("volcengine / deepseek-v4.1-flash", 20)),
            "键必须含供应商（且量与行对应）：{by_key:?}"
        );
        assert!(
            !by_key.iter().any(|(k, _)| *k == SHARED),
            "键不得只是裸模型名（该模型挂了两个供应商）：{by_key:?}"
        );
        assert!(
            by_key.contains(&("hy3", 5)),
            "供应商不可得 → 键回裸模型名，不得带分隔符：{by_key:?}"
        );
        for r in &d.rows {
            assert_eq!(
                r.label, r.key,
                "model 维度的标签与键同形（都由 route_label 一处产出）：{r:?}"
            );
        }

        // 交叉锁：记录页卡内行（恒「供应商 / 模型」）与看板 model 维度**逐字同形**
        let rec = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let inner: Vec<&str> = rec
            .cards
            .iter()
            .flat_map(|c| c.rows.iter())
            .map(|r| r.key.as_str())
            .collect();
        for k in [
            "anthropic / deepseek-v4.1-flash",
            "volcengine / deepseek-v4.1-flash",
            "hy3",
        ] {
            assert!(
                inner.contains(&k),
                "记录页卡内行也必须是 {k}（同一 route_label）：{inner:?}"
            );
        }
        assert_eq!(inner.len(), 3, "三个 (供应商, 模型) 组合 = 三行：{inner:?}");
    }

    /// 卡内行的 `sourceKind` 是**该行所有记录的「最强」三态**（measured > inferred > unknown）：
    /// 同一 (供应商, 模型) 由不同会话给出不同三态时取最强——**不得只取首行/末行**。
    #[test]
    fn records_inner_row_source_kind_takes_the_strongest_state() {
        let conn = mem_empty();
        for (sess, kind, input) in [
            ("s1", SourceKind::Unknown, 1i64),
            ("s2", SourceKind::Measured, 2),
            ("s3", SourceKind::Inferred, 4),
        ] {
            add_spec(
                &conn,
                "claude",
                sess,
                "proj",
                "2026-10-03T10",
                RowSpec {
                    kind,
                    buckets: UsageBuckets {
                        input_fresh: input,
                        ..Default::default()
                    },
                    request_total: input,
                    ..Default::default()
                },
            );
        }
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 1);
        assert_eq!(
            r.cards[0].rows.len(),
            1,
            "前提：三条记录同 (供应商, 模型) → 合并成一行"
        );
        assert_eq!(
            r.cards[0].rows[0].source_kind,
            SourceKind::Measured,
            "取最强三态"
        );
        assert_eq!(r.cards[0].rows[0].buckets.input_fresh, 7, "1 + 2 + 4");
    }

    /// **两档取数路径锁**：小时档读 `usage_detail`、日档读 `usage_daily`——两张表各只写一行
    /// （只明细 / 只日聚合），两个档位必须各自只认自己那张表（若日档走明细或反之，真红）。
    /// 顺带：窗口外的日聚合行不得被 `BETWEEN` 带进来。
    /// 并把**新口径**钉成可见断言：卡内行 `is_subagent` 在**两档**都必须是 `None`（不可得 = `null`）
    /// ——行维度是「供应商 / 模型」、不含 `session_id`（契约 §2 2026-10-03 用户裁决），不得写真 `false`。
    #[test]
    fn records_hour_tier_reads_detail_and_day_tier_reads_daily() {
        let conn = mem_empty();
        let spec = |input: i64| RowSpec {
            buckets: UsageBuckets {
                input_fresh: input,
                ..Default::default()
            },
            request_total: input,
            ..Default::default()
        };
        write_detail_only(
            &conn,
            delta_of("claude", "s1", "proj", "2026-10-03T10", spec(100)),
        );
        write_daily_only(
            &conn,
            delta_of("codex", "s2", "proj", "2026-10-03T10", spec(500)),
        );
        write_daily_only(
            &conn,
            delta_of("dsh", "s3", "proj", "2026-08-01T10", spec(999)),
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let hour = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let h: Vec<(&str, i64)> = hour
            .cards
            .iter()
            .map(|c| (c.tool_id.as_str(), c.buckets.input_fresh))
            .collect();
        assert_eq!(
            h,
            vec![("claude", 100)],
            "小时档只认明细表：日聚合独有的 codex 不得出现，窗口外的 dsh 也不得出现"
        );
        let day = records_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let d: Vec<(&str, i64)> = day
            .cards
            .iter()
            .map(|c| (c.tool_id.as_str(), c.buckets.input_fresh))
            .collect();
        assert_eq!(
            d,
            vec![("codex", 500)],
            "日档只认日聚合表：明细独有的 claude 不得出现"
        );
        // CSV 走同一条 `load_ledger_rows_with`：两档各自只认自己那张表
        let csv_day = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            csv_day.contains("codex,codex,500,"),
            "CSV 日档同样读日聚合：{csv_day}"
        );
        assert!(
            !csv_day.contains("claude"),
            "CSV 日档不得混入明细独有的行：{csv_day}"
        );
        assert!(
            !csv_day.contains("dsh"),
            "窗口外的日聚合行不得出现：{csv_day}"
        );
        let csv_hour = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            csv_hour.contains("claude,claude,100,"),
            "CSV 小时档读明细：{csv_hour}"
        );
        assert!(
            !csv_hour.contains("codex"),
            "CSV 小时档不得混入日聚合独有的行：{csv_hour}"
        );
        // 契约 §2（2026-10-03 用户裁决）的可见化：卡内行**两档都是 `None`**（行维度是
        // 「供应商 / 模型」、不含 `session_id` → 本档位算不出）；若回退成 `Some(false)` 这两条会红。
        assert!(day
            .cards
            .iter()
            .all(|c| c.rows.iter().all(|x| x.is_subagent.is_none())));
        assert!(hour
            .cards
            .iter()
            .all(|c| c.rows.iter().all(|x| x.is_subagent.is_none())));

        // —— 新口径锁（契约 §2 2026-10-03 用户裁决：记录页卡内行 `null`，不是 `false`）——
        // 变异敏感性：把卡内行改回 `Some(false)` → 下面的断言真红（MA-2）。
        // 卡内行的行维度是「供应商 / 模型」、**不含 `session_id`** ⇒ 该位在本层算不出（两档一致）。
        for (tier, rec) in [("日档", &day), ("小时档", &hour)] {
            let rv = serde_json::to_value(rec).unwrap();
            for c in rv["cards"].as_array().unwrap() {
                for r in c["rows"].as_array().unwrap() {
                    assert!(
                        r["isSubagent"].is_null(),
                        "{tier}：记录页卡内行不含 session_id → isSubagent 必须是 null（不是 false）"
                    );
                }
            }
        }
    }

    /// **总开关的门 + 错误传播**：关闭 → `records` 出空卡、`collectedAt` 是 **0 哨兵**
    /// （确定性可断言）；重开 → 同一库数据回来（防「空是因为夹具本来就空」的假绿）；
    /// 非法 `groupBy` 与非法区间都必须回结构化错误码（不得静默出空表）。
    /// 另：CSV **不受总开关门控**（任务书代码如此：`csv_with_conn` 无 `enabled` 早退）——
    /// 本层**如实锁住现状**并登记 ①→②（与 `dashboard` / `records` 的门控不对称）。
    #[test]
    fn master_switch_gates_records_and_csv_behavior_is_registered_as_is() {
        use crate::services::usage::settings::SETTINGS_KEY;
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 2, "前提：打开态有 2 张卡");
        assert_eq!(
            r.range.preset,
            UsageRangePreset::Today,
            "入参回显（契约字段）"
        );
        let off = serde_json::to_string(&UsageSettings {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        crate::database::dao::settings::set_setting_conn(&conn, SETTINGS_KEY, &off);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(r.cards.is_empty(), "总开关关闭 → 零卡");
        assert_eq!(
            r.availability.len(),
            8,
            "可得性说明照常返回（与 Task 18 同口径）"
        );
        assert_eq!(
            r.collected_at, 0,
            "关闭态走早退分支：`collectedAt` 是 **0 哨兵**（契约只有非空 number，没有 null）"
        );
        assert_eq!(r.group_by, UsageGroupBy::Tool, "入参回显不受门控影响");
        // 顺序：W6 的 groupBy 守卫在总开关**之前** → 关闭态下非法维度仍必须报错
        let e = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Provider,
            &UsageFilters::default(),
            now,
        )
        .unwrap_err();
        assert_eq!(
            e.code, "usage-groupby-invalid",
            "关闭态也必须报 W6 的结构化错误码"
        );
        // CSV：任务书未给它总开关门 —— 如实锁住现状（将来要改口径必须是有意为之）
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            csv.contains("claude,claude,100,"),
            "现状：CSV 不看总开关（登记 ①→② 的不对称）：{csv}"
        );
        // 对照：重新打开 → 卡回来
        let on = serde_json::to_string(&UsageSettings {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        crate::database::dao::settings::set_setting_conn(&conn, SETTINGS_KEY, &on);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 2, "重开 → 同一库必须出 2 张卡");
        // 打开态：非法区间 → 结构化错误码（两条新入口都要传，不得静默出空表）
        let bad = UsageRange {
            preset: UsageRangePreset::Custom,
            from: None,
            to: None,
        };
        assert_eq!(
            records_with_conn(
                &conn,
                &bad,
                UsageGroupBy::Tool,
                &UsageFilters::default(),
                now
            )
            .unwrap_err()
            .code,
            "usage-range-invalid",
            "记录页必须上抛区间错误"
        );
        assert_eq!(
            csv_with_conn(
                &conn,
                &bad,
                UsageGroupBy::Tool,
                &UsageFilters::default(),
                now
            )
            .unwrap_err()
            .code,
            "usage-range-invalid",
            "CSV 必须上抛区间错误"
        );
    }

    /// **A-2 锁（契约 §2 `UsageFilters` 通用条款 / §3 要点 4；错误码第 9 个
    /// `usage-filter-unavailable`）**：**日档 + `subagentMode="parentsOnly"` 算不出**——
    /// 日聚合行（`usage_daily`）**不带 `session_id`** ⇒ 无从判断会话身份 ⇒ 两条命令
    /// **必须返回结构化错误**，不得把该条件当成"没有子代理"**放行**。
    ///
    /// 放行的后果是**界面谎言**：合计里仍含子代理，而页脚口径文案写着「不含子代理」
    /// ——违反「不可得不得假装」（GC 7 / 说明书 §8.2）。本条款此前只约束展示位，
    /// 2026-10-03 用户裁决把它扩展到**筛选位**。
    ///
    /// 三档日粒度（last7d / last30d / custom）**逐一**断言（`granularity_of` 单点判据
    /// 若与 `resolve_range` 漂移，这里会红）+ 两条命令都断言 + **守卫在总开关之前**
    /// （关闭态也必须报该码，与 `usage-groupby-invalid` 同纪律）。
    #[test]
    fn day_tier_with_parents_only_filter_reports_filter_unavailable() {
        use crate::services::usage::settings::SETTINGS_KEY;
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let parents_only = UsageFilters {
            subagent_mode: Some(SubagentMode::ParentsOnly),
            ..Default::default()
        };
        let day_ranges = [
            range(UsageRangePreset::Last7d),
            range(UsageRangePreset::Last30d),
            UsageRange {
                preset: UsageRangePreset::Custom,
                from: Some("2026-09-20".into()),
                to: Some("2026-10-03".into()),
            },
        ];
        for r in &day_ranges {
            let e = records_with_conn(&conn, r, UsageGroupBy::Tool, &parents_only, now)
                .expect_err("日档 + parentsOnly 必须报错，不得静默放行");
            assert_eq!(
                e.code, "usage-filter-unavailable",
                "{:?} 的行来自日聚合表（不带 session_id）→ 必须是第 9 个码",
                r.preset
            );
            assert!(
                e.detail.contains("parentsOnly"),
                "错误详情要点名是哪个筛选算不出：{}",
                e.detail
            );
            let e = csv_with_conn(&conn, r, UsageGroupBy::Tool, &parents_only, now)
                .expect_err("CSV 同款：日档 + parentsOnly 必须报错");
            assert_eq!(e.code, "usage-filter-unavailable", "CSV 侧的码必须一致");
        }
        // **守卫在总开关之前**（与 W6 的 `usage-groupby-invalid` 同一层语义）：
        // 关闭态下也必须报该码——否则"关了采集就静默放行"会成为新的界面谎言入口。
        let off = serde_json::to_string(&UsageSettings {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        crate::database::dao::settings::set_setting_conn(&conn, SETTINGS_KEY, &off);
        // **前提回读（P2-3 / B-3，第三轮复审后收尾修正）**：`set_setting_conn` 是 **`let _ =` 吞错**
        // （`dao/settings.rs:27-32`），而下面的守卫**在总开关之前** ⇒ **开/关返回同一个码**
        // ⇒ 「前提没建立」时这一段**照样绿**（W-56 登记的潜在假绿）。这里回读一次，把
        // 「关闭态」变成**本用例自证**（不再只靠 `:1942` / `:3963` 的套件级跨用例背书）。
        let stored = crate::database::dao::settings::get_setting_conn(&conn, SETTINGS_KEY)
            .expect("关闭态前提：设置必须真的落库（set_setting_conn 吞错，不回读就可能假绿）");
        let stored: UsageSettings = serde_json::from_str(&stored)
            .expect("关闭态前提：落库的值必须能反序列化回 UsageSettings");
        assert!(
            !stored.enabled,
            "关闭态前提未建立（回读 enabled = {}）：本段断言对开/关无判别力，必须先把前提钉住",
            stored.enabled
        );
        let e = records_with_conn(
            &conn,
            &day_ranges[0],
            UsageGroupBy::Tool,
            &parents_only,
            now,
        )
        .expect_err("关闭态 + 日档 + parentsOnly：筛选守卫仍必须在总开关之前报错");
        assert_eq!(e.code, "usage-filter-unavailable");
    }

    /// **A-2 反向锁（防误报）**：**小时档 + `parentsOnly` 必须正常返回**——
    /// 小时档的行在明细表上、**带 `session_id`**，会话身份算得出 ⇒ 报错就是误报
    /// （误报会让契约承诺的功能直接不可用，比静默更坏：用户看到"筛选不可用"却明明可用）。
    ///
    /// 同时钉住反向的另一半：**日档 + `include`（默认）必须正常**——只有
    /// `parentsOnly` 这一种组合算不出，不得把整个日档打死。
    #[test]
    fn hour_tier_and_default_mode_still_return_normally_with_parents_only_available() {
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let parents_only = UsageFilters {
            subagent_mode: Some(SubagentMode::ParentsOnly),
            ..Default::default()
        };
        for r in [
            range(UsageRangePreset::Last5h),
            range(UsageRangePreset::Today),
        ] {
            let rec = records_with_conn(&conn, &r, UsageGroupBy::Tool, &parents_only, now)
                .unwrap_or_else(|e| {
                    panic!(
                        "{:?} 是小时档（明细表带 session_id），parentsOnly 算得出，误报即红：{}",
                        r.preset, e.code
                    )
                });
            assert!(
                !rec.cards.is_empty(),
                "{:?} 小时档 + parentsOnly 必须照常出卡（夹具里今日有 2 个源）",
                r.preset
            );
            let csv = csv_with_conn(&conn, &r, UsageGroupBy::Tool, &parents_only, now)
                .expect("CSV 的小时档 + parentsOnly 同样算得出");
            assert!(
                csv.lines().count() > 1,
                "CSV 必须有数据行（只有表头 = 误伤）：{csv}"
            );
        }
        // 日档 + 默认 include：必须正常（不得把整个日档打死）
        let rec = records_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .expect("日档 + 默认 include 必须正常返回");
        assert!(!rec.cards.is_empty(), "日档 + include 必须照常出卡");
        assert!(csv_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now
        )
        .is_ok());
    }

    /// **A-2 同族穷尽扫描的运行期锁**：`UsageFilters` 的**每一个成员**都要"问一句算得出吗"。
    /// 本轮扫描结论（详见 FINAL-FIX-report §B）：
    /// * `toolIds` / `projects` / `providers` / `models` —— **两档都算得出**：小时档取
    ///   `usage_detail` 的记录级列（`source_id` / `project_key` / `provider` / `model`），
    ///   日档取 `usage_daily` 的**同名列**（`load_rows_for_keys` 两档都直接取列、不分叉）
    ///   ⇒ `apply_filters` 的四个 `hit(...)` 在日档**不是**"空表恒不命中"，而是真过滤；
    /// * `subagentMode="include"` —— 恒真（不过滤），两档都算得出；
    /// * `subagentMode="parentsOnly"` —— **唯一算不出的组合**（只有它需要 `session_id`，
    ///   而日聚合行没有该列）。
    ///
    /// 本用例把上面前四条**在日档上**逐条变红式验证（若哪天日档真丢了这些列，
    /// `apply_filters` 会静默变成"全部滤掉/全部放行"，这里会红）。
    #[test]
    fn every_other_filter_member_is_decidable_in_both_tiers() {
        let conn = mem_with_rows();
        let now = local_ms(2026, 10, 3, 12, 0);
        let day = range(UsageRangePreset::Last7d);
        let hour = range(UsageRangePreset::Today);
        // 日档上四个"列筛选"都必须真命中（不是空结果、也不是全放行）
        for (f, want_cards, what) in [
            (
                UsageFilters {
                    tool_ids: Some(vec!["claude".into()]),
                    ..Default::default()
                },
                1,
                "toolIds",
            ),
            (
                UsageFilters {
                    projects: Some(vec!["proj".into()]),
                    ..Default::default()
                },
                2,
                "projects",
            ),
            (
                UsageFilters {
                    providers: Some(vec!["p".into()]),
                    ..Default::default()
                },
                2,
                "providers",
            ),
            (
                UsageFilters {
                    models: Some(vec!["m".into()]),
                    ..Default::default()
                },
                2,
                "models",
            ),
        ] {
            let d =
                records_with_conn(&conn, &day, UsageGroupBy::Tool, &f, now).unwrap_or_else(|e| {
                    panic!(
                        "日档下 {what} 算得出（日聚合表有同名列），不得报 {} ",
                        e.code
                    )
                });
            assert_eq!(
                d.cards.len(),
                want_cards,
                "日档下 {what} 必须是**真过滤**（列在场 → 算得出）"
            );
            let h = records_with_conn(&conn, &hour, UsageGroupBy::Tool, &f, now)
                .unwrap_or_else(|e| panic!("小时档下 {what} 算得出，不得报 {}", e.code));
            assert_eq!(
                h.cards.len(),
                want_cards,
                "小时档下 {what} 与日档同口径（两档不得分叉）"
            );
        }
        // `subagentMode="include"`：两档都算得出（恒不过滤）
        let inc = UsageFilters {
            subagent_mode: Some(SubagentMode::Include),
            ..Default::default()
        };
        assert_eq!(
            records_with_conn(&conn, &day, UsageGroupBy::Tool, &inc, now)
                .unwrap()
                .cards
                .len(),
            2,
            "日档 + include 不过滤"
        );
        assert_eq!(
            records_with_conn(&conn, &hour, UsageGroupBy::Tool, &inc, now)
                .unwrap()
                .cards
                .len(),
            2,
            "小时档 + include 不过滤"
        );
    }

    /// **0 值行不得被丢**（W-35「真空回合保留」/ W-39「如实呈现」在本层的落点）：
    /// 四桶全 0、0 次请求、`userEst = Some(0)` 的行在记录页与 CSV 里都必须各占一行/一条卡内行
    /// ——「看起来像空行就跳过」会让源如实上报的零值回合静默消失（正是 GC 7 要防的那类）。
    #[test]
    fn all_zero_rows_are_still_rendered_in_records_and_csv() {
        let conn = mem_empty();
        add_spec(
            &conn,
            "opencode",
            "z1",
            "proj",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets::default(),
                request_total: 0,
                requests: 0,
                user_est: Some(0),
                ..Default::default()
            },
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(r.cards.len(), 1, "0 值行必须出卡（不得跳过）");
        assert_eq!(r.cards[0].tool_id, "opencode");
        assert_eq!(r.cards[0].buckets, UsageBuckets::default());
        assert_eq!(r.cards[0].metrics.request_total, 0);
        assert_eq!(r.cards[0].metrics.requests, 0);
        assert_eq!(
            r.cards[0].metrics.user_est,
            Some(0),
            "在场且为 0 的 userEst 是**数据**（GC 7 的两面）"
        );
        assert_eq!(r.cards[0].rows.len(), 1, "卡内行同样不得被跳过");
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(
            csv,
            "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind\n\
             opencode,opencode,0,0,0,0,0,0.000000,0,0,inferred\n",
            "0 值行必须逐列如实出（含 userEst=0 与命中率 0.000000）"
        );
    }

    /// CSV `sourceKind` 列的三态（契约 §2 的 `measured | inferred | unknown`，落库列 `provider_kind`）：
    /// 逐源单行 → 三行必须出三个**不同**的字面值（用 Rust `Debug` 的 `Measured`、或恒 `unknown`，真红）。
    /// 同时锁：`usage_export_csv` 接受 **provider / model**（契约 §3：只有记录页限两值）。
    #[test]
    fn csv_source_kind_column_keeps_the_three_states() {
        let conn = mem_empty();
        for (src, kind, input) in [
            ("claude", SourceKind::Measured, 30i64),
            ("codex", SourceKind::Inferred, 20),
            ("workbuddy", SourceKind::Unknown, 10),
        ] {
            add_spec(
                &conn,
                src,
                "s1",
                "proj",
                "2026-10-03T10",
                RowSpec {
                    kind,
                    buckets: UsageBuckets {
                        input_fresh: input,
                        ..Default::default()
                    },
                    request_total: input,
                    provider: if kind == SourceKind::Unknown {
                        String::new()
                    } else {
                        "p".into()
                    },
                    ..Default::default()
                },
            );
        }
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let kinds: Vec<&str> = csv
            .lines()
            .skip(1)
            .map(|l| l.rsplit(',').next().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec!["measured", "inferred", "unknown"],
            "三态逐字（不是 Debug 形态）：{csv}"
        );
        for g in [UsageGroupBy::Provider, UsageGroupBy::Model] {
            assert!(
                csv_with_conn(
                    &conn,
                    &range(UsageRangePreset::Today),
                    g,
                    &UsageFilters::default(),
                    now
                )
                .is_ok(),
                "CSV 必须接受 {g:?}（契约 §3：只有记录页限 tool|project）"
            );
        }
        // 供应商不可得分组：groupKey 与 label 留空（W5）但三态仍如实出 unknown
        let byp = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Provider,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(
            byp.contains(",,10,0,0,0,10,0.000000,1,,unknown"),
            "不可得供应商：groupKey+label 留空、三态照出：{byp}"
        );
    }

    /// **CSV 侧也要过筛**（任务书只测了 records 的筛选）：五个筛选维度在 CSV 路径上都真生效
    /// （若 CSV 忘了 `apply_filters`，全红）。
    #[test]
    fn csv_applies_every_filter_dimension() {
        let conn = mem_with_rows(); // claude/s1（父，proj，p/m）+ codex/s2（子代理，proj，p/m）
        let now = local_ms(2026, 10, 3, 12, 0);
        let today = range(UsageRangePreset::Today);
        let csv =
            |f: UsageFilters| csv_with_conn(&conn, &today, UsageGroupBy::Tool, &f, now).unwrap();
        let base = csv(UsageFilters::default());
        assert!(
            base.contains("claude,claude,100,") && base.contains("codex,codex,500,"),
            "前提：两源都在：{base}"
        );
        let only = csv(UsageFilters {
            tool_ids: Some(vec!["codex".into()]),
            ..Default::default()
        });
        assert!(
            only.contains("codex,codex,500,") && !only.contains("claude,claude,"),
            "tool_ids 生效：{only}"
        );
        assert_eq!(only.lines().count(), 2, "表头 + 1 行");
        let po = csv(UsageFilters {
            subagent_mode: Some(SubagentMode::ParentsOnly),
            ..Default::default()
        });
        assert!(
            !po.contains("codex,codex,"),
            "parentsOnly：子代理会话整行剔除（token 一并剔）：{po}"
        );
        assert!(po.contains("claude,claude,100,"));
        assert_eq!(
            csv(UsageFilters {
                projects: Some(vec!["nope".into()]),
                ..Default::default()
            })
            .lines()
            .count(),
            1,
            "projects 不匹配 → 只剩表头"
        );
        assert_eq!(
            csv(UsageFilters {
                providers: Some(vec!["p".into()]),
                ..Default::default()
            })
            .lines()
            .count(),
            3,
            "providers 匹配 → 两行都在"
        );
        assert_eq!(
            csv(UsageFilters {
                models: Some(vec!["nope".into()]),
                ..Default::default()
            })
            .lines()
            .count(),
            1,
            "models 不匹配 → 只剩表头"
        );
    }

    /// 空账本边界：CSV 只出表头 + 行尾换行（**不是空串、不是没有表头**）；
    /// 记录页零卡但照常出可得性说明（8 条）——「账本空」与「总开关关」是两种空态。
    #[test]
    fn empty_ledger_csv_is_header_only_and_records_have_no_cards() {
        let conn = mem_empty();
        let now = local_ms(2026, 10, 3, 12, 0);
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert_eq!(
            csv, "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind\n",
            "空账本也要出表头（Excel 打开是一张只有表头的表）"
        );
        assert_eq!(csv.lines().count(), 1);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Tool,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        assert!(r.cards.is_empty());
        assert_eq!(r.availability.len(), 8);
    }

    /// **GC 10 隐私白名单的可执行断言**：记录页与 CSV 的输出**只允许**契约给的结构化数字与静态文本。
    /// 会话标题、`project_path_raw`、`project_realpath`（原始路径）**一律不得出现**——用哨兵串验证。
    /// 先断言哨兵**真的在库里**（否则「没出现」只是因为夹具里没有 = 假绿），
    /// 再断言契约允许的字段**真的在**（否则「输出恒空」也是假绿）。
    #[test]
    fn records_and_csv_never_leak_session_text_or_raw_paths() {
        const TITLE: &str = "PROMPT_SENTINEL_DO_NOT_LEAK";
        const RAW_PATH: &str = "/Users/secret/SECRET_PATH_SENTINEL";
        let conn = mem_empty();
        add_spec(
            &conn,
            "claude",
            "s1",
            "sentinelproj",
            "2026-10-03T10",
            RowSpec {
                buckets: UsageBuckets {
                    input_fresh: 42,
                    ..Default::default()
                },
                request_total: 42,
                ..Default::default()
            },
        );
        assert_eq!(
            crate::database::dao::usage::upsert_session_conn(
                &conn,
                "claude",
                &[SessionDimDelta {
                    session_id: "s1".into(),
                    project_key: "sentinelproj".into(),
                    project_label: "SentinelProj".into(),
                    project_path_raw: RAW_PATH.into(),
                    project_realpath: RAW_PATH.into(),
                    title: Some(TITLE.into()),
                    is_subagent: false,
                    parent_session_id: None,
                    originator: None,
                    last_seen_at: 1,
                }],
                1
            ),
            1
        );
        // 前提断言：哨兵真的落库了
        let (t, p): (String, String) = conn
            .query_row(
                "SELECT title, project_path_raw FROM usage_session WHERE session_id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (t.as_str(), p.as_str()),
            (TITLE, RAW_PATH),
            "前提：哨兵必须在库里"
        );
        let now = local_ms(2026, 10, 3, 12, 0);
        let r = records_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let csv = csv_with_conn(
            &conn,
            &range(UsageRangePreset::Today),
            UsageGroupBy::Project,
            &UsageFilters::default(),
            now,
        )
        .unwrap();
        for out in [&json, &csv] {
            for forbidden in [TITLE, RAW_PATH, "SECRET_PATH_SENTINEL"] {
                assert!(
                    !out.contains(forbidden),
                    "GC 10：{forbidden} 不得出现在查询输出里：{out}"
                );
            }
        }
        // 反向对照（防「输出恒空」的假绿）：契约允许的字段必须真的在
        assert!(
            json.contains("SentinelProj"),
            "契约允许的项目显示名必须在：{json}"
        );
        assert!(json.contains("sentinelproj"), "分组键必须在：{json}");
        assert!(
            csv.contains("sentinelproj,SentinelProj,42,"),
            "CSV 同样只出契约列：{csv}"
        );
    }

    /// **真机账本冒烟**（`#[ignore]`，需开发机 `~/.tuvis/tuvis.db` 已有真机数据）：
    /// `cargo test --lib -- --ignored --nocapture real_ledger_dashboard_smoke`
    ///
    /// 为什么值得单列：本文件其余用例都在**内存库**上跑（口径可断言、可复现），而真机数据的
    /// **形状**（多源混存、空模型行、跨小时 `(0, ms>0)`、`user_est` 为 NULL）只有真库才有。
    /// 本用例**只读**打开真库（`SQLITE_OPEN_READ_ONLY`，**绝不写**），只断言**与具体数字无关的
    /// 不变量**；真机数字一律 `println!` 作报告证据，不钉死（否则换台机器就红）。
    ///
    /// **⚠️ 本用例是「刻意的只读例外」（Task 16 / 2026-10-03 真机数据隔离守卫：勿改、勿删、勿"统一"）**：
    /// 它是全仓**唯一**需要真机数据**形状**的用例，天然安全——因为它**绕开了全部三条真机可达路径**：
    /// ① 用**显式路径**自开连接（`TUVIS_HOME` 优先、否则 `dirs::home_dir()`）——**不走**
    ///    `database::connection::app_data_home()`（Step 2 那个 `cfg(test)` 重定向分支在它这里
    ///    **根本不存在**，它也没被重定向影响）；
    /// ② **不走** `database::open()`、也**不碰**进程级 `DB`（设置读的是 `settings::load_from_conn`）；
    /// ③ 连接以 `SQLITE_OPEN_READ_ONLY` 打开，物理上写不进去。
    /// 再加 `#[ignore]`（不在常规门禁里），它既不会被 `cargo test` 顺带跑到，也不可能写账本。
    /// 后来者**不要**「顺手统一」成 `app_data_home()`——那会把它变成 Step 2 的测试对象，
    /// 同时毁掉它存在的唯一理由（读真机数据的**形状**）。
    #[test]
    #[ignore = "真机冒烟：需 ~/.tuvis/tuvis.db（只读打开）"]
    fn real_ledger_dashboard_smoke() {
        let home = std::env::var_os("TUVIS_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .expect("无法定位用户目录");
        let db = home.join(".tuvis").join("tuvis.db");
        if !db.exists() {
            eprintln!("跳过：真机账本 {} 不存在", db.display());
            return;
        }
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("只读打开真机账本失败（若 兔维斯 正在写，可能 SQLITE_BUSY）");
        let enabled = crate::services::usage::settings::load_from_conn(&conn).enabled;
        let now = crate::services::usage::now_ms();
        println!(
            "[real] db={} usage.enabled={enabled} now_ms={now}",
            db.display()
        );
        for (name, r) in [
            ("today", range(UsageRangePreset::Today)),
            ("last7d", range(UsageRangePreset::Last7d)),
        ] {
            let d = dashboard_with_conn(&conn, &r, UsageGroupBy::Tool, now).unwrap();
            // 与数据无关的不变量（真机数字只在下面打印）
            assert_eq!(
                d.hero,
                d.totals.request_total + d.totals_buckets.output,
                "hero = 请求输入 + 产出（说明书 P1 第 2 条）"
            );
            let row_rt: i64 = d.rows.iter().map(|r| r.metrics.request_total).sum();
            assert_eq!(
                row_rt, d.totals.request_total,
                "分组行必须不重不漏地划分总量"
            );
            let row_b = d.rows.iter().fold(UsageBuckets::default(), |mut a, r| {
                a.add(&r.buckets);
                a
            });
            assert_eq!(row_b, d.totals_buckets, "四桶同样不重不漏");
            let trend_b = d.trend.iter().fold(UsageBuckets::default(), |mut a, t| {
                a.add(&t.buckets);
                a
            });
            assert_eq!(
                trend_b, d.totals_buckets,
                "趋势各桶之和必须等于总量（当期窗口自洽）"
            );
            println!(
                "[{name}] totals={:?} hero={} requests={} user_est={:?} rows={} trend={} \
                 compare_prev_request_total={:?} sessions={:?} turns_per_tool={:?} \
                 error_model={:?} error_turn={:?} error_tool={:?} interrupted={:?} \
                 tool_calls={:?} tool_avg_ms={:?} top_tool={:?} top_tool_ms={:?} \
                 recent_session={:?} collected_at={}",
                d.totals_buckets,
                d.hero,
                d.totals.requests,
                d.totals.user_est,
                d.rows.len(),
                d.trend.len(),
                d.compare.as_ref().map(|c| c.prev_metrics.request_total),
                d.work_summary.sessions,
                d.work_summary.turns_per_tool,
                d.work_summary.error_model,
                d.work_summary.error_turn,
                d.work_summary.error_tool,
                d.work_summary.interrupted,
                d.work_summary.tool_calls,
                d.work_summary.tool_avg_ms,
                d.work_summary.top_tool,
                d.work_summary.top_tool_ms,
                d.recent_session.as_ref().map(|r| (
                    r.source_id.clone(),
                    r.session_id.clone(),
                    r.buckets
                )),
                d.collected_at,
            );
            // 逐工具明细（报告用）：按请求输入降序
            for row in d.rows.iter().take(10) {
                println!(
                    "  - {} / {} : buckets={:?} request_total={} requests={} user_est={:?} \
                     kind={:?} is_subagent={:?}",
                    row.key,
                    row.label,
                    row.buckets,
                    row.metrics.request_total,
                    row.metrics.requests,
                    row.metrics.user_est,
                    row.source_kind,
                    row.is_subagent
                );
            }
        }
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
