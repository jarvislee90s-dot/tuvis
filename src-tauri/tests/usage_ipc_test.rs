//! Task 20：6 条用量 IPC 的**真跑**验收（真 `#[tauri::command]` 函数 + 真 `spawn_blocking` +
//! 真账本 + 真采集调度）。
//!
//! 与 `tests/msw/tauriMocks.test.ts`（mock 形状）的分工：
//! * 本文件证明「**真跑通了**」——调的是 `multi_agents_manager_lib::commands::usage::*`，
//!   走 `tauri::async_runtime::spawn_blocking` 与全局 `DB`（`support::setup()` 把 `HOME`/`TUVIS_HOME`
//!   重定向到临时目录，七个采集器因此只扫空目录，D-07 的真实库泄漏在本文件不成立）；
//! * `tests/usage/usageMockParity.test.ts` 证明「**两端 mock 同形**」——它**不能**证明真实
//!   命令接通了，那正是本文件存在的理由（IPC 层最容易「看起来接上了其实没接」）。
//!
//! 断言一律打在 **serde 序列化后的 JSON** 上（真实 wire 形态），而不是 Rust 字段：
//! 字段改名 / 可空性写错（`skip_serializing_if` 漏了或多了）都只在 JSON 上可见，
//! Rust 侧 `assert_eq!` 对结构体是抓不住的。
//!
//! **测试隔离（Task 17 的教训）**：本文件所有用例共享进程级全局量（全局 `DB`、`COLLECT_*`、
//! 设置 KV）→ 每条用例都必须持 `serial()` 串行锁；自省用例
//! `every_test_in_this_file_takes_the_serial_lock` 把这条纪律变成机械锁。

mod support;

use std::sync::{Mutex, MutexGuard};

use multi_agents_manager_lib::commands::usage as ipc;
use multi_agents_manager_lib::commands::usage_v2 as v2ipc;
use multi_agents_manager_lib::database::connection::DB;
use multi_agents_manager_lib::database::dao::usage_billing as usage_dao;
use multi_agents_manager_lib::services::usage::collect;
use multi_agents_manager_lib::services::usage::error::USAGE_CODES;
use multi_agents_manager_lib::services::usage::model::{
    UsageBuckets, UsageFilters, UsageGroupBy, UsageMetrics, UsageRange, UsageRangePreset,
    UsageSettings, UsageSettingsPatch, UsageSourceId,
};
use multi_agents_manager_lib::services::usage::v2::model::{
    AttributionKind, BillingCommit, ChannelKind, ChannelOption, CostComponents, CostRow,
    CostStatus, DistributionGroup, DistributionRow, IdentityKey, ModelDisplayNameRequest,
    ObservedModelEntry, PriceCurrency, PriceOverrideRequest, PriceOverrideView, PriceRuleKind,
    PricingAutoMatch, PricingBasis, PricingCatalogEntry, PricingCatalogSearch,
    PricingIdentityRequest, PricingQuotePreview, PricingReference, ProviderAliasConfig,
    ProviderAliasKind, ProviderAliasRule, TimePriceRule, TimePriceTimezone, TimePriceWindow,
    TrendSelection,
};
use multi_agents_manager_lib::services::usage::v2::pricing;

static SERIAL: Mutex<()> = Mutex::new(());

/// 串行锁 + 环境重定向（`support::setup()` 是 `Once`：只生效一次，之后的调用是空操作）。
/// `MutexGuard` 必须绑到具名变量上（绑 `_` 会立刻 drop，锁形同虚设——`clippy::let_underscore_lock`）。
///
/// **额外隔离（本任务实测发现）**：`support::setup()` 只重定向了 `HOME`/`TUVIS_HOME`，但
/// * dsh 采集器的根走 `monitor::dsh::dsh_home_with()`，**`DSH_HOME` 环境变量优先于 `~/.dsh`**；
/// * kimi 采集器的根走 `KIMI_CODE_HOME`（优先于 `~/.kimi`）。`KIMI_CODE_HOME` 的注入
///   由 Task 13 的记录项目锁住（D-30）；两者若在本机被设置（本机 `DSH_HOME=~/.dsh`），
///   采集就会**扫真实家目录**——测试既慢又脏（实测：dashboard 用例因此读到别的用例
///   采进来的真实行 → 断言失败）。故取锁前把两者指到 fixture 根下的空目录。
/// * 采集扫描根本身走 `TUVIS_USAGE_FIXTURE_ROOT`（collect.rs 缝）：Windows 下
///   `dirs::home_dir()` 不认 HOME，不设它七个源会直捣真实 profile（2026-10-07 事故族）。
fn serial() -> MutexGuard<'static, ()> {
    support::setup();
    let root = support::usage_fixture_root();
    // 读源根 = 进程专用 fixture 根（空根）；dsh/kimi 摘掉本机值后若不显式指向
    // fixture 根，会回落到 `dirs::home_dir()/.dsh` / `/.kimi-code`——Windows 上即真实目录
    std::env::set_var("TUVIS_USAGE_FIXTURE_ROOT", root);
    std::env::set_var("DSH_HOME", root.join(".dsh"));
    std::env::set_var("KIMI_CODE_HOME", root.join(".kimi-code"));
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn today() -> UsageRange {
    UsageRange {
        preset: UsageRangePreset::Today,
        from: None,
        to: None,
    }
}

fn obj_keys(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v
        .as_object()
        .expect("wire 形态必须是 JSON 对象")
        .keys()
        .cloned()
        .collect();
    k.sort();
    k
}

/// 与 `commands/usage.rs` 的自省锁同款：只看代码骨架 / 只看生产半边。
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace(",)", ")")
}

fn prod_only(src: &str) -> String {
    let marker = concat!("#[cfg", "(test)]");
    match src.find(marker) {
        Some(index) => src[..index].to_string(),
        None => src.to_string(),
    }
}

/// 契约 §2 的 7 个源 id（**小写工具 id**；`workBuddy`/`zCode` 是 Task 1 抓过的坑）
const SOURCE_IDS: [&str; 7] = [
    "claude",
    "codex",
    "kimi",
    "opencode",
    "workbuddy",
    "zcode",
    "dsh",
];

// **直接引用权威码表**（P2-1：原先此处另存一份**镜像** `KNOWN_USAGE_CODES`，已删除）。
// 错误码的登记处是：① `services/usage/error.rs` 的 `USAGE_CODES`（**权威表**，`pub const`、
// **不是** `#[cfg(test)]`）；② 前端 `src/components/usage/usageErrors.ts` 的 `KNOWN_USAGE_CODES`；
// ③ zh.json + en.json 的 `usage.rpc.*` 模板。②③ 与权威表的同序同集合由
// `commands::usage` / `error` 的单测锁住，**集成构建不再需要也不得另存副本**
// （旧镜像的注释自称「Rust 表是 `#[cfg(test)]` 拿不到」——那是**假陈述**，见 `error.rs:38`）。
// 判据：`errorCode` 必须落在权威表内，否则前端 `usageErrMsg` 会把它**静默收敛成通用错误**。
// （改动前，本文件在旧 `:78-92` 处另存了那份 `[&str; 9]` 镜像——本说明取代它。）

/// ① `usage_collect`：真跑一轮采集（7 源，空 home）。
///
/// W-23：`new_records == 0` **必须与 `ok` / `errorCode` 配对**读——落库失败时 `new_records`
/// 也报 0，只看 `new_records` 会把「落库失败」读成「增量正确」。
#[test]
fn usage_collect_runs_seven_sources_and_pairs_ok_with_error_code() {
    let _g = serial();

    // **裁决 C（评审 Important #3）**：空 temp home 下 7 源**全部** `ok=true` → 配对断言的
    // `else`（ok=false → 必须有 errorCode）**永不执行**、`totalNewRecords == Σ` 退化成 `0 == 0`、
    // 本文件那份码表镜像**从不被查**（纸面断言）。故先造一个**必失败源**：
    // 往 `<temp home>/.workbuddy/workbuddy.db` 写**非 SQLite 内容** →
    // `workbuddy.rs` 的 `open_usage_db` 判 NOTADB 且 `db.exists()` → `Err("usage-source-io")`。
    // fixture 落点 = 进程专用 fixture 根（tempdir）。**不要**用 `dirs::home_dir()` 取 home：
    // Windows 下它不认 `support::setup()` 的 HOME 重定向，会拿到真实 home
    let home = support::usage_fixture_root().to_path_buf();
    // 安全前提：绝不在真实家目录里写夹具（fixture 根若不在 temp 下，这里当场红，
    // 而不是写坏用户目录）
    assert!(
        home.starts_with(std::env::temp_dir()),
        "安全前提：fixture 根必须是 temp 下的专用目录，实际 {}",
        home.display()
    );
    let wb_dir = home.join(".workbuddy");
    std::fs::create_dir_all(&wb_dir).expect("创建 .workbuddy 夹具目录失败");
    let wb_db = wb_dir.join("workbuddy.db");
    std::fs::write(&wb_db, b"this is not a sqlite database").expect("写失败源夹具失败");
    assert!(
        std::fs::metadata(&wb_db).map(|m| m.len()).unwrap_or(0) > 0,
        "前提：必失败源夹具必须真的落盘（否则本用例又退回纸面断言）"
    );

    let r = tauri::async_runtime::block_on(ipc::usage_collect(true))
        .expect("usage_collect 必须返回 Ok（单源故障走 status，不上抛整轮——W-06）");

    let v = serde_json::to_value(&r).expect("UsageCollectResult 必须可序列化");
    assert_eq!(
        obj_keys(&v),
        vec!["collectedAt", "durationMs", "sources", "totalNewRecords"],
        "UsageCollectResult 的 wire 字段逐字冻结（契约 §2）"
    );
    assert!(
        v["collectedAt"].as_i64().unwrap_or(0) > 0,
        "collectedAt 必须是真实毫秒时间戳"
    );
    assert!(
        v["durationMs"].as_i64().is_some(),
        "durationMs 必须是非空数字"
    );

    assert_eq!(r.sources.len(), 7, "七源必须齐（GC 20 / Task 17）");
    let mut seen: Vec<String> = Vec::new();
    for s in &r.sources {
        let sv = serde_json::to_value(s).unwrap();
        let keys = obj_keys(&sv);
        // `errorCode?: string` → **在场 ⟺ ok=false**（skip_serializing_if 的 wire 语义）
        if s.ok {
            assert!(
                s.error_code.is_none(),
                "源 {} ok=true 却带 errorCode（契约：errorCode 只在失败源出现）",
                s.source_id.db_id()
            );
        } else {
            let code = s.error_code.as_deref().unwrap_or_else(|| {
                panic!("源 {} ok=false 必须给出 errorCode", s.source_id.db_id())
            });
            assert!(
                USAGE_CODES.contains(&code),
                "源 {} 的错误码 `{code}` 不在权威表 USAGE_CODES 里 → 前端会静默显示成通用错误（W-23/W-28）",
                s.source_id.db_id()
            );
        }
        let want_keys = if s.ok {
            vec!["newRecords", "ok", "parsedFiles", "sourceId"]
        } else {
            vec!["errorCode", "newRecords", "ok", "parsedFiles", "sourceId"]
        };
        assert_eq!(
            keys, want_keys,
            "UsageSourceStatus 的 wire 字段逐字冻结（契约 §2：errorCode 在场 ⟺ ok=false）"
        );
        assert_eq!(
            sv["sourceId"].as_str().unwrap(),
            s.source_id.db_id(),
            "sourceId 必须是**小写**工具 id（不是 camelCase 的 workBuddy/zCode）"
        );
        assert!(
            SOURCE_IDS.contains(&sv["sourceId"].as_str().unwrap()),
            "未知源 id"
        );
        seen.push(s.source_id.db_id().to_string());
    }
    seen.sort();
    let mut want = SOURCE_IDS.to_vec();
    want.sort();
    assert_eq!(seen, want, "7 个源 id 必须各出现一次");

    // **裁决 C**：恰 1 个失败源（workbuddy）、码在权威表 `USAGE_CODES` 内、且该源 0 行——
    // 这条把上面的 `else` 分支从「纸面」变成真跑（此前 7/7 ok=true，白名单从不被查）。
    let failed: Vec<_> = r.sources.iter().filter(|s| !s.ok).collect();
    assert_eq!(
        failed.len(),
        1,
        "夹具应让恰好 1 个源失败，实际 {}",
        failed.len()
    );
    let bad = failed[0];
    assert_eq!(
        bad.source_id,
        multi_agents_manager_lib::services::usage::model::UsageSourceId::WorkBuddy,
        "失败源必须是被夹具打坏的 workbuddy"
    );
    assert_eq!(
        bad.error_code.as_deref(),
        Some("usage-source-io"),
        "workbuddy 的坏库必须报 usage-source-io（`workbuddy.rs` 的 NOTADB 路径）"
    );
    assert!(USAGE_CODES.contains(&bad.error_code.as_deref().unwrap()));
    assert_eq!(
        (bad.parsed_files, bad.new_records),
        (0, 0),
        "整源失败时该源必须 0/0（W-06 + W-23 配对口径）"
    );
    assert_eq!(
        r.sources.iter().filter(|s| s.ok).count(),
        6,
        "其余 6 源必须照常 ok（W-06 单源故障隔离）"
    );

    // totalNewRecords = 各源 newRecords 之和（配对口径：失败源已被置 0，不虚报计划行数）
    let sum: i64 = r.sources.iter().map(|s| s.new_records).sum();
    assert_eq!(r.total_new_records, sum);

    // 契约 §3：`force=false` 且距上次采集小于 `collectIntervalMin` → **直接返回上次结果，不扫描**。
    // 这条同时钉住 `force` 参数**真的被用上了**（命令层把 force 写死成 true 也会红）。
    let calls_after_force = collect::collect_calls();
    let again = tauri::async_runtime::block_on(ipc::usage_collect(false))
        .expect("usage_collect(false) 必须返回 Ok");
    assert_eq!(
        collect::collect_calls(),
        calls_after_force,
        "契约 §3：窗口内 force=false 不得再扫一轮（单飞 + 最小间隔保护）"
    );
    assert_eq!(
        again.collected_at, r.collected_at,
        "窗口内 force=false 必须复用同一次结果"
    );

    // **裁决 A（评审 Important #1）**：上面只锁了 `force=false` 复用；而首次调用时
    // `LAST_RESULT` 为空 → **任何** force 取值都会真扫 → 把命令层写成 `collect(false)`
    // 时本文件此前**全绿**（内核侧锁的是 `collect_with` 直调，不是 IPC 入口）。
    // `force` 是「立即采集」按钮的唯一开关，写成 false 后窗口内（默认 10min）点按钮静默无动作。
    // 这里用**计数**（不用同毫秒时间戳）钉住 force=true 必须绕过最小间隔真扫一轮。
    let calls_before_force = collect::collect_calls();
    let forced = tauri::async_runtime::block_on(ipc::usage_collect(true))
        .expect("usage_collect(true) 必须返回 Ok");
    assert_eq!(
        collect::collect_calls(),
        calls_before_force + 1,
        "契约 §3：force=true 必须**无视最小间隔**真扫一轮（否则「立即采集」按钮静默无动作）"
    );
    assert!(
        forced.collected_at >= again.collected_at,
        "force=true 必须给出新一轮的结果"
    );
}

/// ② `usage_dashboard`：wire 形状逐字冻结 + GC 7「不可得 = null 而不是 0」。
/// 空账本（临时 home 采不到任何行）下，`compare` / `recentSession` 与 `workSummary` 的
/// 不可得字段必须**在场且为 null**（缺键 = 前端 `undefined`，空态分支与 `| null` 类型都对不上）。
#[test]
fn usage_dashboard_wire_shape_is_frozen_contract() {
    let _g = serial();
    let d = tauri::async_runtime::block_on(ipc::usage_dashboard(today(), UsageGroupBy::Tool))
        .expect("usage_dashboard 必须返回 Ok");
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "availability",
            "collectedAt",
            "compare",
            "groupBy",
            "hero",
            "range",
            "recentSession",
            "rows",
            "totals",
            "totalsBuckets",
            "trend",
            "workSummary"
        ],
        "UsageDashboard 的 wire 字段逐字冻结（契约 §2）"
    );
    assert_eq!(v["groupBy"], "tool");
    assert_eq!(v["range"]["preset"], "today");
    // `from?: string` / `to?: string`：缺省即不出现（skip_serializing_if），不是 null
    let range = v["range"].as_object().unwrap();
    assert!(!range.contains_key("from") && !range.contains_key("to"));

    for k in ["compare", "recentSession"] {
        assert!(
            v.get(k).is_some(),
            "契约写 `{k}: T | null` → 必须**出现**（null 也要出现，不能缺键）"
        );
        assert!(v[k].is_null(), "空账本时 {k} 必须是 null（GC 7，不得填 0）");
    }

    let ws = v["workSummary"].as_object().expect("workSummary 必在");
    for k in [
        "sessions",
        "turnsPerTool",
        "errorModel",
        "errorTurn",
        "errorTool",
        "interrupted",
        "toolCalls",
        "toolAvgMs",
        "topTool",
        "topToolMs",
        "longestTurnPerTool",
    ] {
        assert!(
            ws.contains_key(k),
            "WorkSummary 缺字段 {k}（契约 §2 全字段必须出现）"
        );
    }
    // GC 7：不可得一律 null，绝不填 0
    for k in [
        "sessions",
        "errorModel",
        "errorTurn",
        "errorTool",
        "interrupted",
        "toolCalls",
        "toolAvgMs",
        "topTool",
        "topToolMs",
    ] {
        assert!(
            ws[k].is_null(),
            "空账本时 WorkSummary.{k} 必须是 null（GC 7：不可得 ≠ 0）"
        );
    }
    assert!(ws["turnsPerTool"].as_object().unwrap().is_empty());
    assert!(ws["longestTurnPerTool"].as_object().unwrap().is_empty());

    // hero = totals.requestTotal + totalsBuckets.output（说明书 P1 第 2 条）
    assert_eq!(
        d.hero,
        d.totals.request_total + d.totals_buckets.output,
        "hero 口径"
    );
    assert!(
        !v["availability"].as_array().unwrap().is_empty(),
        "availability 必须给非空表（「不可得就空态」的实现载体）"
    );
    assert!(
        v["collectedAt"].as_i64().is_some(),
        "collectedAt 是非空 number"
    );
    assert!(!USAGE_CODES.is_empty());
}

/// ③ `usage_records`：W6——记录页 `groupBy` **只接受 tool | project**；
/// 传 provider/model 必须是结构化错误 `usage-groupby-invalid`（不是 panic、不是静默当成 tool）。
#[test]
fn usage_records_accepts_only_tool_and_project() {
    let _g = serial();
    for bad in [UsageGroupBy::Provider, UsageGroupBy::Model] {
        let err = tauri::async_runtime::block_on(ipc::usage_records(
            today(),
            bad,
            UsageFilters::default(),
        ))
        .unwrap_err();
        assert_eq!(
            err.code, "usage-groupby-invalid",
            "非法卡片维度必须给结构化码"
        );
        let ev = serde_json::to_value(&err).unwrap();
        assert_eq!(
            obj_keys(&ev),
            vec!["code", "detail"],
            "UsageError 的 wire 形状"
        );
        // W-28：detail 要能定位（泛码之外还得说清是哪一处）
        assert!(
            err.detail.contains("groupBy"),
            "detail 必须能定位到出错参数，实际: {}",
            err.detail
        );
    }
    for good in [UsageGroupBy::Tool, UsageGroupBy::Project] {
        let r = tauri::async_runtime::block_on(ipc::usage_records(
            today(),
            good,
            UsageFilters::default(),
        ))
        .expect("tool | project 必须可用");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            obj_keys(&v),
            vec!["availability", "cards", "collectedAt", "groupBy", "range"],
            "UsageRecords 的 wire 字段逐字冻结（契约 §2）"
        );
        assert_eq!(
            v["groupBy"],
            match good {
                UsageGroupBy::Tool => "tool",
                UsageGroupBy::Project => "project",
                other => unreachable!("本循环只跑 tool | project，收到 {other:?}"),
            },
            "groupBy 必须原样回显（命令层把入参写死成常量也会被这条抓住）"
        );
        assert!(v["cards"].as_array().unwrap().is_empty(), "空账本无卡片");
    }
}

/// ④ `usage_export_csv`：返回 **CSV 文本**（11 列，无 BOM——BOM 是 Task 21 `export_save_text`
/// 的职责），且 `groupBy` **四值都接受**（与记录页的 W6 限制相反，契约 §3）。
#[test]
fn usage_export_csv_returns_header_text_for_all_four_groupby() {
    let _g = serial();
    let header =
        "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind";
    for g in [
        UsageGroupBy::Tool,
        UsageGroupBy::Project,
        UsageGroupBy::Provider,
        UsageGroupBy::Model,
    ] {
        let csv = tauri::async_runtime::block_on(ipc::usage_export_csv(
            today(),
            g,
            UsageFilters::default(),
        ))
        .unwrap_or_else(|e| panic!("groupBy={g:?} 必须被 CSV 接受，实际错误 {}", e.code));
        assert_eq!(csv.lines().next().unwrap(), header, "表头 11 列逐字冻结");
        assert_eq!(csv.lines().count(), 1, "空账本只有表头");
        assert!(
            !csv.starts_with('\u{feff}'),
            "本层不带 BOM（落盘命令负责前置，双 BOM 会让 Excel 首列变脏）"
        );
    }
}

/// ⑤⑥ 两条设置命令：读回契约默认值 + patch 合并**落库**（读回第二次必须看到新值——
/// 这是 mock 永远证明不了的一条），并锁定越界 patch 的结构化错误码。
#[test]
fn usage_settings_read_write_round_trip_through_ipc() {
    let _g = serial();
    // **A-5**：两条设置命令改成 `async` + `spawn_blocking`（P2-3），
    // 集成测试按仓内既有形式 `tauri::async_runtime::block_on` 驱动（与 usage_collect 同款）。
    let s0 = tauri::async_runtime::block_on(ipc::usage_get_settings());
    let v0 = serde_json::to_value(&s0).unwrap();
    assert_eq!(
        obj_keys(&v0),
        vec![
            "collectIntervalMin",
            "detailRetentionDays",
            "enabled",
            "exportPose",
            "exportQuote",
            "miniBarRange",
            "miniBarToolRows",
            "providerMapRules"
        ],
        "UsageSettings 的 wire 字段逐字冻结（契约 §2）"
    );
    assert_eq!(v0["miniBarRange"], "today");
    assert_eq!(s0.mini_bar_tool_rows, 3, "说明书 §P7：默认 3 条");

    // 合并 + 落库：patch 只带一个字段，其余保持原值（Partial<UsageSettings> 语义）
    let s1 = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        detail_retention_days: Some(30),
        ..Default::default()
    }))
    .expect("合法 patch 必须 Ok");
    assert_eq!(s1.detail_retention_days, 30);
    assert_eq!(
        s1.mini_bar_tool_rows, s0.mini_bar_tool_rows,
        "未出现在 patch 里的字段不得被清空"
    );
    let s2 = tauri::async_runtime::block_on(ipc::usage_get_settings());
    assert_eq!(
        s2.detail_retention_days, 30,
        "patch 必须真的落库（读回第二次要看到新值；mock 两端各自独立，证明不了这条）"
    );

    // 越界 patch → 结构化错误（不是静默夹取）
    let err = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        detail_retention_days: Some(0),
        ..Default::default()
    }))
    .unwrap_err();
    assert_eq!(err.code, "usage-settings-invalid");
    assert!(!err.detail.trim().is_empty(), "W-28：detail 不得为空");

    // 复位（同文件其它用例依赖默认口径）
    let back = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        detail_retention_days: Some(90),
        ..Default::default()
    }))
    .expect("复位必须 Ok");
    assert_eq!(back.detail_retention_days, 90);
    assert_eq!(
        tauri::async_runtime::block_on(ipc::usage_get_settings()),
        UsageSettings::default()
    );
}

/// Task 18 的既有结论之一：**总开关关闭 → 查询出空态（零扫描、零行）**，`collectedAt` = 0 哨兵。
/// 关闭期间 `usage_records` 对**非法** groupBy 仍必须报码（守卫在开关之前，Task 19 的口径）。
#[test]
fn master_switch_off_yields_empty_state_with_zero_collected_at() {
    let _g = serial();
    let off = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        enabled: Some(false),
        ..Default::default()
    }))
    .expect("关闭总开关必须 Ok");
    assert!(!off.enabled, "前提：总开关确已关闭");

    let d = tauri::async_runtime::block_on(ipc::usage_dashboard(today(), UsageGroupBy::Tool))
        .expect("关闭态查询不报错，出空态");
    assert!(d.rows.is_empty(), "关闭态零行");
    assert_eq!(
        d.collected_at, 0,
        "从未采集/关闭态：collectedAt = 0 哨兵（Task 18 结论）"
    );
    assert_eq!(d.hero, 0);

    let err = tauri::async_runtime::block_on(ipc::usage_records(
        today(),
        UsageGroupBy::Provider,
        UsageFilters::default(),
    ))
    .unwrap_err();
    assert_eq!(
        err.code, "usage-groupby-invalid",
        "非法维度守卫必须**先于**总开关（关闭态也要报码）"
    );

    let restored = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        enabled: Some(true),
        ..Default::default()
    }))
    .expect("恢复总开关必须 Ok");
    assert!(
        restored.enabled,
        "前提：总开关确已恢复（否则后续用例全体假绿）"
    );
}

/// **裁决 F（评审 Minor 4）**：`usage_set_settings` 的**用户可见**语义必须是
/// 「报成功 = 真的落库了」。DAO 的 `set_setting_conn`（Task 1 的 `dao/settings.rs:27-32`）
/// 把 `execute` 的错误丢进 `let _ =` → 写失败被报成成功（前端显示新值、重启回旧值）。
/// 本用例在 `settings` 表上挂 `RAISE(ABORT)` 触发器**造真实写失败**，断言命令层返回
/// **契约里已有的** `usage-db-failed`（不是新造码），且回读仍是旧值。
#[test]
fn set_settings_reports_persist_failure_instead_of_success() {
    let _g = serial();
    let mam_home = std::env::var("TUVIS_HOME").expect("support::setup() 必须设置 TUVIS_HOME");
    let db_path = std::path::Path::new(&mam_home)
        .join(".tuvis")
        .join("tuvis.db");
    assert!(
        db_path.exists(),
        "前提：全局 DB 文件必须在（{}）——否则本用例打不到真实写路径",
        db_path.display()
    );

    let before = tauri::async_runtime::block_on(ipc::usage_get_settings()).detail_retention_days;
    let blocker = rusqlite::Connection::open(&db_path).expect("第二条连接（造触发器）必须成功");
    blocker
        .execute_batch(
            "CREATE TRIGGER mam_test_block_settings_ins BEFORE INSERT ON settings \
               BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;
             CREATE TRIGGER mam_test_block_settings_upd BEFORE UPDATE ON settings \
               BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;
             CREATE TRIGGER mam_test_block_settings_del BEFORE DELETE ON settings \
               BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;",
        )
        .expect("建触发器必须成功（前提：settings 表存在）");

    let result = tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        // 保留期现上限 90：取一个仍在合法区间内的新值，确保失败来自写库而非参数校验。
        detail_retention_days: Some(before.saturating_sub(1).max(1)),
        ..Default::default()
    }));

    // 先撤触发器再断言：否则用例一旦失败会把触发器留给同二进制后面的用例（串味）
    blocker
        .execute_batch(
            "DROP TRIGGER IF EXISTS mam_test_block_settings_ins;
             DROP TRIGGER IF EXISTS mam_test_block_settings_upd;
             DROP TRIGGER IF EXISTS mam_test_block_settings_del;",
        )
        .expect("撤触发器必须成功");

    let err = result.expect_err("写失败必须报错——绝不能报成功（裁决 F 的底线）");
    assert_eq!(
        err.code, "usage-db-failed",
        "必须复用契约里已有的码，不新造码"
    );
    assert!(
        !err.detail.trim().is_empty(),
        "detail 要能定位（写后回读不一致）"
    );
    assert_eq!(
        tauri::async_runtime::block_on(ipc::usage_get_settings()).detail_retention_days,
        before,
        "前提：写真的没落库（回读仍是旧值）"
    );
}

/// **测试隔离自省锁**（Task 17 `dsh.rs` 先例的移植）：本文件每条用例都碰进程级全局量
/// （全局 `DB` / `COLLECT_*` / 设置 KV），漏一条串行锁就会间歇性互相串味。
/// 去掉任一条 `serial()` → 本用例真红。
#[test]
fn every_test_in_this_file_takes_the_serial_lock() {
    let _g = serial();
    let src = include_str!("usage_ipc_test.rs");
    let lines: Vec<&str> = src.lines().collect();
    let mut checked = 0;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() != "#[test]" {
            continue;
        }
        let end = (i + 5).min(lines.len());
        let window = lines[i..end].join("\n");
        // **Minor 7（第二版，第一版是假绿）**：`window.contains("let _g = serial();")`
        // 对 `// let _g = serial();` 这种**注释掉**的形态同样为真（实测：变异 M7 第一版 MISS）。
        // 必须逐行比对**确切语句**：窗口里得有一行 trim 后**恰好等于** `let _g = serial();`。
        let _ = window;
        let has_lock = lines[i..end]
            .iter()
            .any(|l| l.trim() == "let _g = serial();");
        assert!(
            has_lock,
            "第 {} 行的用例没有取 `let _g = serial();` 串行锁（Task 17 教训：进程级全局量必须串行；注释里的不算）",
            i + 1
        );
        checked += 1;
    }
    assert_eq!(
        checked, 18,
        "本文件应有 18 条用例（含本自省用例）；新增用例必须同步持有 serial()"
    );
}

// ── Task 5：V2 身份/Token IPC ────────────────────────────────────────────────

const V2_COMMAND_NAMES: [&str; 26] = [
    "usage_billing_get",
    "usage_billing_preview",
    "usage_billing_commit",
    "usage_billing_undo",
    "usage_identities",
    "usage_trend",
    "usage_distribution",
    "usage_pricing_get",
    "usage_pricing_set_enabled",
    "usage_pricing_refresh",
    "usage_pricing_catalog",
    "usage_costs",
    "usage_cost_export_csv",
    // Task 6：别名 / 渠道选项 / 手动覆盖价
    "usage_provider_aliases_get",
    "usage_provider_alias_upsert",
    "usage_provider_alias_delete",
    "usage_channel_options",
    "usage_pricing_overrides_get",
    "usage_pricing_override_set",
    "usage_pricing_override_delete",
    // 2026-10-20 计划 Task 5：观察条目 / 模型配置 / 精选目录
    "usage_observed_entries_get",
    "usage_model_display_name_set",
    "usage_pricing_identity_set",
    "usage_pricing_catalog_search",
    "usage_pricing_auto_match",
    "usage_pricing_quote_get",
];

/// Task 1 契约：供应商别名 / 渠道选项的 serde 形状逐字冻结。
/// 这些类型是后续 Task 2–9 的唯一 wire 契约；字段改名、枚举变体名写错、
/// `Option` 被 `skip_serializing_if` 吃掉，都只能在这一层抓住。
#[test]
fn v2_alias_and_channel_wire_shapes_are_frozen() {
    let _g = serial();

    // ChannelKind 一律 camelCase；ProviderAliasKind 的值域是 provider / toolModel。
    for (kind, want) in [
        (ChannelKind::BillingProfile, "billingProfile"),
        (ChannelKind::Alias, "alias"),
        (ChannelKind::Provider, "provider"),
        (ChannelKind::Unassigned, "unassigned"),
        (ChannelKind::Ambiguous, "ambiguous"),
    ] {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::json!(want),
            "ChannelKind::{kind:?} 的 wire 值必须是 `{want}`"
        );
    }
    assert_eq!(
        serde_json::to_value(ProviderAliasKind::Provider).unwrap(),
        serde_json::json!("provider")
    );
    assert_eq!(
        serde_json::to_value(ProviderAliasKind::ToolModel).unwrap(),
        serde_json::json!("toolModel")
    );

    let provider_rule = ProviderAliasRule {
        id: "a1".to_string(),
        kind: ProviderAliasKind::Provider,
        provider: Some("openrouter".to_string()),
        source_id: None,
        model_id: None,
        display_name: "My OpenRouter".to_string(),
    };
    let v = serde_json::to_value(&provider_rule).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "displayName",
            "id",
            "kind",
            "modelId",
            "provider",
            "sourceId"
        ],
        "ProviderAliasRule 的 wire 字段逐字冻结"
    );
    assert_eq!(v["kind"], "provider");
    assert!(
        v["sourceId"].is_null(),
        "kind=provider 时 sourceId 必须显式 null"
    );
    assert!(
        v["modelId"].is_null(),
        "kind=provider 时 modelId 必须显式 null"
    );

    let tool_rule = ProviderAliasRule {
        id: "a2".to_string(),
        kind: ProviderAliasKind::ToolModel,
        provider: None,
        source_id: Some(UsageSourceId::WorkBuddy),
        model_id: Some(String::new()),
        display_name: "WorkBuddy Unknown".to_string(),
    };
    let v = serde_json::to_value(&tool_rule).unwrap();
    assert_eq!(v["kind"], "toolModel");
    assert_eq!(v["sourceId"], "workbuddy", "sourceId 必须复用工具的 db id");
    assert_eq!(
        v["modelId"], "",
        "空串表示该工具的未知模型，不得折叠成 null"
    );
    assert!(
        v["provider"].is_null(),
        "kind=toolModel 时 provider 必须显式 null"
    );

    let config = ProviderAliasConfig {
        aliases: vec![provider_rule, tool_rule],
    };
    let v = serde_json::to_value(&config).unwrap();
    assert_eq!(obj_keys(&v), vec!["aliases"]);
    assert_eq!(v["aliases"].as_array().unwrap().len(), 2);

    let option = ChannelOption {
        key: "provider:My OpenRouter".to_string(),
        label: "My OpenRouter".to_string(),
        kind: ChannelKind::Alias,
        billing_profile_id: None,
        token_usage: 42,
    };
    let v = serde_json::to_value(&option).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec!["billingProfileId", "key", "kind", "label", "tokenUsage"],
        "ChannelOption 的 wire 字段逐字冻结"
    );
    assert!(v["billingProfileId"].is_null());
    assert_eq!(v["kind"], "alias");
    assert_eq!(v["tokenUsage"], 42);
}

/// Task 1 契约：手动覆盖价的请求/视图 + 行级渠道字段 + `PricingBasis=userOverride`。
#[test]
fn v2_price_override_and_channel_row_fields_are_frozen() {
    let _g = serial();

    // 2026-10-20 币种扩展：覆盖价请求/视图带 `currency`（serde `"USD"|"CNY"`，缺省 USD）。
    let request = PriceOverrideRequest {
        normalized_model_id: "gpt-4".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("1.5".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("7".to_string()),
        time_rule: None,
    };
    let v = serde_json::to_value(&request).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "cacheRead",
            "cacheWrite",
            "currency",
            "input",
            "normalizedModelId",
            "output",
            "timeRule"
        ],
        "PriceOverrideRequest 的 wire 字段逐字冻结"
    );
    assert_eq!(v["currency"], "USD", "PriceCurrency 序列化为大写 ISO 代码");
    assert!(v["timeRule"].is_null(), "固定价请求必须显式 null timeRule");
    assert!(v["cacheRead"].is_null(), "未覆盖的分量必须显式 null");
    assert!(v["cacheWrite"].is_null());
    assert_eq!(v["input"], "1.5");
    assert_eq!(v["output"], "7");
    // serde 缺省：旧客户端/旧夹具不带 currency ⇒ 反序列化按 USD。
    let legacy: PriceOverrideRequest =
        serde_json::from_str(r#"{"normalizedModelId":"gpt-4","input":"1"}"#).unwrap();
    assert_eq!(legacy.currency, PriceCurrency::Usd, "缺省币种必须是 USD");
    assert_eq!(legacy.time_rule, None, "缺省 timeRule 必须是固定价");
    let cny: PriceOverrideRequest =
        serde_json::from_str(r#"{"normalizedModelId":"m","currency":"CNY","input":"1"}"#).unwrap();
    assert_eq!(cny.currency, PriceCurrency::Cny);
    let bad: Result<PriceOverrideRequest, _> =
        serde_json::from_str(r#"{"normalizedModelId":"m","currency":"EUR","input":"1"}"#);
    assert!(bad.is_err(), "未知币种必须拒绝（不是静默 USD）");

    let view = PriceOverrideView {
        normalized_model_id: "gpt-4".to_string(),
        currency: PriceCurrency::Cny,
        input: Some("1.5".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("7".to_string()),
        time_rule: None,
        updated_at: 1_700_000_000_000,
        source_provider: None,
        source_model: None,
        release_date: None,
        snapshot_date: None,
    };
    let v = serde_json::to_value(&view).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "cacheRead",
            "cacheWrite",
            "currency",
            "input",
            "normalizedModelId",
            "output",
            "releaseDate",
            "snapshotDate",
            "sourceModel",
            "sourceProvider",
            "updatedAt"
        ],
        "PriceOverrideView 的 wire 字段逐字冻结"
    );
    assert_eq!(v["currency"], "CNY");
    for key in [
        "cacheRead",
        "cacheWrite",
        "releaseDate",
        "snapshotDate",
        "sourceModel",
        "sourceProvider",
    ] {
        assert!(
            v[key].is_null(),
            "PriceOverrideView.{key} 不可得时必须显式 null"
        );
    }
    assert_eq!(v["updatedAt"], 1_700_000_000_000i64);
    assert!(
        v.get("timeRule").is_none(),
        "固定价视图必须省略 timeRule（不是 null）"
    );

    assert_eq!(
        serde_json::to_value(PricingBasis::UserOverride).unwrap(),
        serde_json::json!("userOverride"),
        "PricingBasis 必须追加 userOverride（TS 侧同步）"
    );

    let buckets = UsageBuckets {
        input_fresh: 0,
        cache_read: 0,
        cache_write: 0,
        output: 0,
    };
    let metrics = UsageMetrics {
        request_total: 0,
        cache_hit_rate: 0.0,
        user_est: None,
        requests: 0,
    };

    // tool/project 维度：渠道字段全部显式 null（不得缺键）。
    let dist = DistributionRow {
        key: "tool:claude".to_string(),
        label: "claude".to_string(),
        source_ids: vec![UsageSourceId::Claude],
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        buckets,
        metrics,
        channel_key: None,
        channel_kind: None,
        display_provider: None,
        observed_provider: None,
        observed_providers: Vec::new(),
        has_empty_provider: false,
        model_id: None,
    };
    let v = serde_json::to_value(&dist).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "attributionKind",
            "billingProfileId",
            "buckets",
            "channelKey",
            "channelKind",
            "displayProvider",
            "hasEmptyProvider",
            "key",
            "label",
            "metrics",
            "modelId",
            "observedProvider",
            "observedProviders",
            "sourceIds"
        ],
        "DistributionRow 必须追加渠道/模型字段（tool/project 维度为 null）"
    );
    for key in [
        "channelKey",
        "channelKind",
        "displayProvider",
        "observedProvider",
        "modelId",
    ] {
        assert!(
            v[key].is_null(),
            "DistributionRow.{key} 在 tool/project 维度必须显式 null"
        );
    }
    assert_eq!(
        v["observedProviders"],
        serde_json::json!([]),
        "DistributionRow.observedProviders 必须是数组（tool/project 无证据时为空）"
    );

    // 渠道/模型维度：字段在场且有值。
    let dist = DistributionRow {
        key: "channel:provider:X".to_string(),
        label: "X".to_string(),
        source_ids: vec![UsageSourceId::Claude],
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        buckets,
        metrics,
        channel_key: Some("provider:X".to_string()),
        channel_kind: Some(ChannelKind::Alias),
        display_provider: Some("X".to_string()),
        observed_provider: Some("x-raw".to_string()),
        observed_providers: vec!["x-raw".to_string()],
        has_empty_provider: false,
        model_id: Some("m".to_string()),
    };
    let v = serde_json::to_value(&dist).unwrap();
    assert_eq!(v["channelKey"], "provider:X");
    assert_eq!(v["channelKind"], "alias");
    assert_eq!(v["displayProvider"], "X");
    assert_eq!(v["observedProvider"], "x-raw", "原始 provider 必须保留");
    assert_eq!(v["observedProviders"], serde_json::json!(["x-raw"]));
    assert_eq!(v["hasEmptyProvider"], false);
    assert_eq!(v["modelId"], "m");

    let cost = CostRow {
        key: "cost:k".to_string(),
        identity: IdentityKey {
            source_id: UsageSourceId::Claude,
            observed_provider: Some("x-raw".to_string()),
            model_id: "m".to_string(),
        },
        source_ids: vec![UsageSourceId::Claude],
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        token_usage: 0,
        cache_hit_rate: None,
        estimated_usd: None,
        estimated_cny: None,
        usd_converted: false,
        cny_converted: true,
        status: CostStatus::Unpriced,
        priced_tokens: 0,
        unpriced_tokens: 0,
        coverage_rate: None,
        status_cny: CostStatus::Partial,
        priced_tokens_cny: 0,
        unpriced_tokens_cny: 0,
        coverage_rate_cny: None,
        pricing_reference: None,
        pricing_snapshot_id: None,
        pricing_basis: PricingBasis::UserOverride,
        price_updated_at: None,
        reasons: vec![],
        components: CostComponents {
            input: None,
            cache_read: None,
            cache_write: None,
            output: None,
        },
        channel_key: "provider:X".to_string(),
        channel_kind: ChannelKind::Alias,
        display_provider: Some("X".to_string()),
        observed_providers: vec!["x-raw".to_string()],
        has_empty_provider: false,
        normalized_model_id: Some("m".to_string()),
        pricing_release_date: Some("2026-10".to_string()),
        price_override: true,
        price_rule_kind: PriceRuleKind::None,
        time_breakdown: None,
        display_model_name: None,
    };
    let v = serde_json::to_value(&cost).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "attributionKind",
            "billingProfileId",
            "cacheHitRate",
            "channelKey",
            "channelKind",
            "cnyConverted",
            "components",
            "coverageRate",
            "coverageRateCny",
            "displayModelName",
            "displayProvider",
            "estimatedCny",
            "estimatedUsd",
            "hasEmptyProvider",
            "identity",
            "key",
            "normalizedModelId",
            "observedProviders",
            "priceOverride",
            "priceRuleKind",
            "priceUpdatedAt",
            "pricedTokens",
            "pricedTokensCny",
            "pricingBasis",
            "pricingReference",
            "pricingReleaseDate",
            "pricingSnapshotId",
            "reasons",
            "sourceIds",
            "status",
            "statusCny",
            "timeBreakdown",
            "tokenUsage",
            "unpricedTokens",
            "unpricedTokensCny",
            "usdConverted"
        ],
        "CostRow 必须追加渠道/覆盖价/自定义模型名/双币种字段"
    );
    assert_eq!(v["channelKey"], "provider:X");
    assert_eq!(v["channelKind"], "alias");
    assert_eq!(v["displayProvider"], "X");
    assert_eq!(v["observedProviders"], serde_json::json!(["x-raw"]));
    assert_eq!(v["hasEmptyProvider"], false);
    assert_eq!(v["normalizedModelId"], "m");
    assert_eq!(v["pricingReleaseDate"], "2026-10");
    assert_eq!(v["priceOverride"], true);
    assert_eq!(v["pricingBasis"], "userOverride");
    // 双币种字段：CNY 口径独立 status/coverage；converted 布尔逐字上 wire。
    assert_eq!(v["status"], "unpriced");
    assert_eq!(v["statusCny"], "partial");
    assert_eq!(v["coverageRateCny"], serde_json::Value::Null);
    assert_eq!(v["cnyConverted"], true);
    assert_eq!(v["usdConverted"], false);
    assert!(
        v["displayModelName"].is_null(),
        "CostRow.displayModelName 未配置时必须显式 null（不是缺键）"
    );
}

/// Task 1 契约（2026-10-20 计划 §2.2）：模型条目配置与精选价目的 serde 形状逐字冻结。
/// `Option` 一律显式 null；枚举值域 official / openrouter / exact / prefix / none 写死。
#[test]
fn v2_model_config_and_curated_wire_shapes_are_frozen() {
    let _g = serial();

    // ObservedModelEntry：A 段与 B 段的共同数据源（全历史，来自 usage_daily）。
    // 2026-10-20 追加 recentTokens7d（最近 7 天含今日，条目 Top10 排序用）与
    // normalizedModelId（后端机械归一化，前端不重复实现）。
    let entry = ObservedModelEntry {
        provider_raw: Some("openai".to_string()),
        source_id: None,
        model_id: "gpt-5".to_string(),
        normalized_model_id: "gpt-5".to_string(),
        display_provider: Some("OpenAI".to_string()),
        display_model_name: None,
        pricing_provider: None,
        pricing_model: None,
        total_tokens: 42,
        recent_tokens_7d: 7,
    };
    let v = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "displayModelName",
            "displayProvider",
            "modelId",
            "normalizedModelId",
            "pricingModel",
            "pricingProvider",
            "providerRaw",
            "recentTokens7d",
            "sourceId",
            "totalTokens",
        ],
        "ObservedModelEntry 的 wire 字段逐字冻结"
    );
    for key in [
        "displayModelName",
        "pricingModel",
        "pricingProvider",
        "sourceId",
    ] {
        assert!(
            v[key].is_null(),
            "ObservedModelEntry.{key} 未配置时必须显式 null"
        );
    }
    assert_eq!(v["providerRaw"], "openai");
    assert_eq!(v["displayProvider"], "OpenAI");
    assert_eq!(v["totalTokens"], 42);
    assert_eq!(v["recentTokens7d"], 7);
    assert_eq!(v["normalizedModelId"], "gpt-5");
    assert!(
        v["normalizedModelId"].is_string(),
        "normalizedModelId 必须是字符串（归一化模型 ID 上 wire）"
    );

    // 空 provider 条目：providerRaw=null、sourceId 必有（「工具+模型」单列）。
    let empty_provider = ObservedModelEntry {
        provider_raw: None,
        source_id: Some(UsageSourceId::Codex),
        model_id: String::new(),
        normalized_model_id: String::new(),
        display_provider: None,
        display_model_name: Some("我的模型".to_string()),
        pricing_provider: Some("openai".to_string()),
        pricing_model: Some("gpt-5".to_string()),
        total_tokens: 0,
        recent_tokens_7d: 0,
    };
    let v = serde_json::to_value(&empty_provider).unwrap();
    assert!(
        v["providerRaw"].is_null(),
        "空 provider 条目 providerRaw=null"
    );
    assert_eq!(v["sourceId"], "codex");
    assert_eq!(v["displayModelName"], "我的模型");
    assert_eq!(v["pricingProvider"], "openai");
    assert_eq!(v["pricingModel"], "gpt-5");
    // 空模型 ⇒ 归一化空串，必须是字符串而不是 null（显式序列化纪律）。
    assert!(
        v["normalizedModelId"].is_string(),
        "空模型的 normalizedModelId 必须是空串类型，不是 null"
    );
    assert_eq!(
        v["normalizedModelId"], "",
        "空模型归一化为空串（前端显示「未知模型」）"
    );

    // 写请求：null = 清除（合法），不得被 skip 掉。
    let display = ModelDisplayNameRequest {
        provider_raw: Some("openai".to_string()),
        source_id: None,
        model_id: "gpt-5".to_string(),
        display_name: None,
    };
    let v = serde_json::to_value(&display).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec!["displayName", "modelId", "providerRaw", "sourceId"],
        "ModelDisplayNameRequest 的 wire 字段逐字冻结"
    );
    assert!(
        v["displayName"].is_null(),
        "displayName=null 必须显式序列化（清除语义）"
    );

    let identity = PricingIdentityRequest {
        provider_raw: None,
        source_id: Some(UsageSourceId::Codex),
        model_id: "m".to_string(),
        reference: Some(PricingReference {
            provider_id: "openai".to_string(),
            model_id: "gpt-5".to_string(),
        }),
    };
    let v = serde_json::to_value(&identity).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec!["modelId", "providerRaw", "reference", "sourceId"],
        "PricingIdentityRequest 的 wire 字段逐字冻结"
    );
    assert_eq!(v["reference"]["providerId"], "openai");
    assert_eq!(v["reference"]["modelId"], "gpt-5");

    // PricingCatalogEntry 追加 curatedKind；值域 official / openrouter / null。
    let catalog_entry = PricingCatalogEntry {
        provider_id: "openai".to_string(),
        model_id: "gpt-5".to_string(),
        release_date: None,
        has_rates: true,
        conditional: false,
        curated_kind: Some("official".to_string()),
    };
    let v = serde_json::to_value(&catalog_entry).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "conditional",
            "curatedKind",
            "hasRates",
            "modelId",
            "providerId",
            "releaseDate",
        ],
        "PricingCatalogEntry 必须追加 curatedKind"
    );
    assert_eq!(v["curatedKind"], "official");
    let openrouter = PricingCatalogEntry {
        curated_kind: Some("openrouter".to_string()),
        ..catalog_entry.clone()
    };
    assert_eq!(
        serde_json::to_value(&openrouter).unwrap()["curatedKind"],
        "openrouter",
        "curatedKind 值域含 openrouter"
    );
    let non_curated = PricingCatalogEntry {
        curated_kind: None,
        ..catalog_entry.clone()
    };
    assert!(
        serde_json::to_value(&non_curated).unwrap()["curatedKind"].is_null(),
        "非精选条目 curatedKind 显式 null"
    );

    // PricingCatalogSearch：entries + providerCount（「显示全部 N 个来源」的 N）。
    let search = PricingCatalogSearch {
        entries: vec![catalog_entry],
        provider_count: 232,
    };
    let v = serde_json::to_value(&search).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec!["entries", "providerCount"],
        "PricingCatalogSearch 的 wire 字段逐字冻结"
    );
    assert_eq!(v["providerCount"], 232);
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);

    // PricingQuotePreview：USD/CNY 双四价 + 发布日 + 条件价 + 快照日期 + 当日汇率；
    // Option 显式 null。2026-10-20 追加 inputCny/cacheReadCny/cacheWriteCny/outputCny/
    // cnyConverted/fxRate/fxFetchedAt/fxFallback 八字段。
    let quote = PricingQuotePreview {
        reference: PricingReference {
            provider_id: "openai".to_string(),
            model_id: "gpt-5".to_string(),
        },
        input: Some("2".to_string()),
        cache_read: Some("0.2".to_string()),
        cache_write: None,
        output: Some("10".to_string()),
        input_cny: Some("14.4".to_string()),
        cache_read_cny: Some("1.44".to_string()),
        cache_write_cny: None,
        output_cny: Some("72".to_string()),
        cny_converted: true,
        release_date: None,
        conditional: true,
        snapshot_date: Some(1_000),
        fx_rate: Some("7.2".to_string()),
        fx_fetched_at: None,
        fx_fallback: true,
    };
    let v = serde_json::to_value(&quote).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec![
            "cacheRead",
            "cacheReadCny",
            "cacheWrite",
            "cacheWriteCny",
            "cnyConverted",
            "conditional",
            "fxFallback",
            "fxFetchedAt",
            "fxRate",
            "input",
            "inputCny",
            "output",
            "outputCny",
            "reference",
            "releaseDate",
            "snapshotDate",
        ],
        "PricingQuotePreview 的 wire 字段逐字冻结（含 CNY 与 fx 面）"
    );
    assert!(v["cacheWrite"].is_null());
    assert!(v["cacheWriteCny"].is_null(), "CNY 缺价同样显式 null");
    assert!(v["releaseDate"].is_null());
    assert!(v["fxFetchedAt"].is_null(), "fxFetchedAt 不可得时显式 null");
    assert_eq!(v["input"], "2");
    assert_eq!(v["inputCny"], "14.4");
    assert_eq!(v["cnyConverted"], true);
    assert_eq!(v["fxFallback"], true);
    assert_eq!(v["fxRate"], "7.2");
    assert_eq!(v["conditional"], true);
    assert_eq!(v["snapshotDate"], 1_000);

    // PricingAutoMatch：kind 值域 exact / prefix / none；未命中 reference/quote 均 null。
    let miss = PricingAutoMatch {
        kind: "none".to_string(),
        reference: None,
        quote: None,
    };
    let v = serde_json::to_value(&miss).unwrap();
    assert_eq!(
        obj_keys(&v),
        vec!["kind", "quote", "reference"],
        "PricingAutoMatch 的 wire 字段逐字冻结"
    );
    assert!(v["reference"].is_null() && v["quote"].is_null());
    for kind in ["exact", "prefix"] {
        let hit = PricingAutoMatch {
            kind: kind.to_string(),
            reference: None,
            quote: None,
        };
        assert_eq!(
            serde_json::to_value(&hit).unwrap()["kind"],
            kind,
            "PricingAutoMatch.kind 值域含 {kind}"
        );
    }
}

/// 注册 + 锁纪律 + 服务生产入口：命令层一律 `spawn_blocking`、不取 DB 锁、不调采集；
/// 服务入口只持锁调 `*_with_conn`。
#[test]
fn v2_commands_are_registered_and_obey_the_lock_discipline() {
    let _g = serial();

    let command_src = code_only(&prod_only(include_str!("../src/commands/usage_v2.rs")));
    assert_eq!(
        command_src.matches("#[tauri::command").count(),
        26,
        "V2 必须有恰好 26 条 `#[tauri::command]`"
    );
    assert_eq!(
        command_src.matches("spawn_blocking").count(),
        26,
        "每条 V2 命令都必须经 spawn_blocking（命令层不在主线程做阻塞读）"
    );
    for name in V2_COMMAND_NAMES {
        assert!(
            command_src.contains(&format!("pubasyncfn{name}(")),
            "V2 命令签名缺失或被改动：`{name}`"
        );
    }
    for forbidden in [
        "DB.lock(",
        "collect::collect(",
        "run_collection(",
        "collect_with(",
        "load_ledger_rows(",
    ] {
        assert!(
            !command_src.contains(forbidden),
            "V2 命令层不得出现 `{forbidden}`（锁与采集都留在服务层）"
        );
    }
    for call in [
        "billing::load",
        "billing::preview(",
        "billing::commit(",
        "billing::undo(",
        "query::identities(",
        "query::trend(",
        "query::distribution(",
        "pricing::status(",
        "pricing::set_enabled(",
        "pricing::refresh(",
        "pricing::catalog_entries(",
        "cost_query::costs(",
        "cost_query::cost_csv(",
    ] {
        assert!(
            command_src.contains(call),
            "V2 命令必须调用服务入口 `{call}`"
        );
    }

    let lib = code_only(include_str!("../src/lib.rs"));
    for name in V2_COMMAND_NAMES {
        assert_eq!(
            lib.matches(&format!("commands::usage_v2::{name},")).count(),
            1,
            "generate_handler! 必须恰好登记一次 `commands::usage_v2::{name}`"
        );
    }
    assert_eq!(
        lib.matches("commands::usage_v2::").count(),
        26,
        "lib.rs 只应登记 26 条 V2 命令（无重复 / 无遗漏）"
    );

    let billing_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/billing.rs"
    )));
    for entry in ["pubfnload(", "pubfnpreview(", "pubfncommit(", "pubfnundo("] {
        assert!(
            billing_src.contains(entry),
            "billing 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        billing_src.contains("DB.lock("),
        "billing 生产入口必须自己取 DB 锁（命令层不取）"
    );
    assert!(
        billing_src.contains("load_with_conn(") && billing_src.contains("commit_with_conn("),
        "billing 生产入口必须复用 with_conn 实现"
    );

    let query_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/query.rs"
    )));
    for entry in [
        "pubfnidentities(",
        "pubfntrend(",
        "pubfndistribution(",
        "pubfnchannel_options(",
    ] {
        assert!(
            query_src.contains(entry),
            "query 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        query_src.contains("DB.lock("),
        "query 生产入口必须自己取 DB 锁"
    );
    for with_conn in [
        "identities_with_conn(",
        "trend_with_conn(",
        "distribution_with_conn(",
        "channel_options_with_conn(",
    ] {
        assert!(
            query_src.contains(with_conn),
            "query 生产入口必须复用 `{with_conn}`"
        );
    }
    for forbidden in ["collect::collect(", "run_collection(", "collect_with("] {
        assert!(
            !query_src.contains(forbidden),
            "查询服务不得触发采集：`{forbidden}`"
        );
    }

    let pricing_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/pricing.rs"
    )));
    for entry in [
        "pubfnstatus(",
        "pubfnset_enabled(",
        "pubfnrefresh(",
        "pubfnspawn_pricing_refresh_loop(",
        "pubfncatalog_entries(",
        "pubfnoverrides(",
        "pubfnset_override(",
        "pubfndelete_override(",
    ] {
        assert!(
            pricing_src.contains(entry),
            "pricing 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        pricing_src.contains("DB.lock("),
        "pricing 生产入口必须自己取 DB 锁"
    );
    for with_conn in [
        "catalog_entries_with_conn(",
        "catalog_search_scoped_with_conn(",
        "quote_preview_with_conn(",
        "auto_match_with_conn(",
        "overrides_with_conn(",
        "set_override_with_conn(",
        "delete_override_with_conn(",
    ] {
        assert!(
            pricing_src.contains(with_conn),
            "pricing 生产入口必须复用 `{with_conn}`"
        );
    }
    for entry in [
        "pubfncatalog_search_scoped(",
        "pubfnquote_preview(",
        "pubfnauto_match(",
    ] {
        assert!(
            pricing_src.contains(entry),
            "pricing 精选目录生产入口缺失：`{entry}`"
        );
    }

    let model_config_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/model_config.rs"
    )));
    for entry in [
        "pubfnobserved_entries(",
        "pubfnset_display_name(",
        "pubfnset_pricing_identity(",
    ] {
        assert!(
            model_config_src.contains(entry),
            "model_config 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        model_config_src.contains("DB.lock("),
        "model_config 生产入口必须自己取 DB 锁"
    );
    for with_conn in [
        "observed_entries_with_conn(",
        "set_display_name_with_conn(",
        "set_pricing_identity_with_conn(",
    ] {
        assert!(
            model_config_src.contains(with_conn),
            "model_config 生产入口必须复用 `{with_conn}`"
        );
    }
    for forbidden in ["collect::collect(", "run_collection(", "collect_with("] {
        assert!(
            !model_config_src.contains(forbidden),
            "模型配置服务不得触发采集：`{forbidden}`"
        );
    }
    assert!(
        pricing_src.contains("usage-pricing-updated") && pricing_src.contains("emit_ui("),
        "刷新完成必须 emit usage-pricing-updated（手动与后台循环共用 refresh）"
    );

    let channel_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/channel.rs"
    )));
    for entry in ["pubfnload(", "pubfnupsert(", "pubfndelete("] {
        assert!(
            channel_src.contains(entry),
            "channel 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        channel_src.contains("DB.lock("),
        "channel 生产入口必须自己取 DB 锁"
    );
    for with_conn in ["load_with_conn(", "upsert_with_conn(", "delete_with_conn("] {
        assert!(
            channel_src.contains(with_conn),
            "channel 生产入口必须复用 `{with_conn}`"
        );
    }

    let cost_query_src = code_only(&prod_only(include_str!(
        "../src/services/usage/v2/cost_query.rs"
    )));
    for entry in ["pubfncosts(", "pubfncost_csv("] {
        assert!(
            cost_query_src.contains(entry),
            "cost_query 服务生产入口缺失：`{entry}`"
        );
    }
    assert!(
        cost_query_src.contains("DB.lock("),
        "cost_query 生产入口必须自己取 DB 锁"
    );
}

/// 真跑 7 条命令，断言 wire 形状（serde 后的 JSON，而非 Rust 字段）。
/// 2026-10-20 计划 Task 5：6 条新命令真跑（观察条目 / 模型配置 / 精选目录）。
#[test]
fn v2_model_config_commands_round_trip() {
    let _g = serial();

    // 观察条目来自 usage_daily 全历史聚合；先开总开关（其它用例可能关过）。
    tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        enabled: Some(true),
        ..UsageSettingsPatch::default()
    }))
    .expect("打开总开关必须成功");
    {
        let mut conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch(
            "INSERT INTO usage_daily
                 (day_key, source_id, project_key, provider, provider_kind, model,
                  input_fresh, cache_read, cache_write, output, request_total, requests,
                  user_est, updated_at)
             VALUES
                 ('2026-10-16','codex','','openai','measured','deepseek-v4.1-flash',1,0,0,2,3,1,NULL,0),
                 ('2026-10-17','codex','','openai','measured','deepseek-v4.1-flash',1,0,0,4,5,1,NULL,0),
                 ('2026-10-17','workbuddy','','','measured','',1,0,0,2,3,1,NULL,0);",
        )
        .unwrap();
        // 精选目录（deepseek 直营官方 + 非精选聚合来源夹具）。
        pricing::set_enabled_with_conn(&conn, true).expect("启用定价必须成功");
        const CATALOG: &str = r#"{
            "deepseek": {"models": {
                "deepseek-v4.1-flash": {
                    "release_date": "2025-09-01",
                    "cost": {"input": 1, "output": 2},
                    "modalities": {"input": ["text"], "output": ["text"]}
                }
            }},
            "aggregator-x": {"models": {
                "deepseek-v4.1-flash": {
                    "release_date": "2025-12-01",
                    "cost": {"input": 99},
                    "modalities": {"input": ["text"], "output": ["text"]}
                }
            }}
        }"#;
        // force=true：本二进制的串行锁获取顺序不定，更早的用例可能已用同一 fetched_at
        // 激活过快照（不 stale ⇒ force=false 会静默跳过，拿到别人的目录）。
        pricing::refresh_with_conn(&mut conn, 1_000, true, || Ok(CATALOG.as_bytes().to_vec()))
            .expect("刷新快照必须成功");
    }

    // 观察条目 get：非空 provider + 空 provider（工具+模型）都在列，totalTokens 聚合。
    let entries = tauri::async_runtime::block_on(v2ipc::usage_observed_entries_get())
        .expect("usage_observed_entries_get 必须成功");
    assert_eq!(entries.len(), 2, "openai 与 workbuddy 空-provider 条目各一");
    let openai = entries
        .iter()
        .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
        .unwrap();
    assert_eq!(openai.total_tokens, 14, "requestTotal 8 + output 6");
    let workbuddy = entries
        .iter()
        .find(|entry| entry.provider_raw.is_none())
        .unwrap();
    assert_eq!(workbuddy.source_id, Some(UsageSourceId::WorkBuddy));

    // 显示名 set：写后返回刷新后的全量列表。
    let after_display = tauri::async_runtime::block_on(v2ipc::usage_model_display_name_set(
        ModelDisplayNameRequest {
            provider_raw: Some("openai".to_string()),
            source_id: None,
            model_id: "deepseek-v4.1-flash".to_string(),
            display_name: Some("DeepSeek 官方".to_string()),
        },
    ))
    .expect("usage_model_display_name_set 必须成功");
    let openai = after_display
        .iter()
        .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
        .unwrap();
    assert_eq!(openai.display_model_name.as_deref(), Some("DeepSeek 官方"));

    // 计价身份 set：指向精选目录条目。
    let after_identity =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_identity_set(PricingIdentityRequest {
            provider_raw: Some("openai".to_string()),
            source_id: None,
            model_id: "deepseek-v4.1-flash".to_string(),
            reference: Some(PricingReference {
                provider_id: "deepseek".to_string(),
                model_id: "deepseek-v4.1-flash".to_string(),
            }),
        }))
        .expect("usage_pricing_identity_set 必须成功");
    let openai = after_identity
        .iter()
        .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
        .unwrap();
    assert_eq!(openai.pricing_provider.as_deref(), Some("deepseek"));

    // 幽灵身份拒绝（经 IPC 错误码透传）。
    let err = tauri::async_runtime::block_on(v2ipc::usage_model_display_name_set(
        ModelDisplayNameRequest {
            provider_raw: Some("ghost".to_string()),
            source_id: None,
            model_id: "m".to_string(),
            display_name: Some("X".to_string()),
        },
    ))
    .unwrap_err();
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(err.detail, "model config entry not observed");

    // 作用域目录搜索：curated 过滤 aggregator-x；providerCount 恒为全量 2。
    let search = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog_search(
        Some("DEEPSEEK".to_string()),
        Some(20),
        Some("curated".to_string()),
    ))
    .expect("usage_pricing_catalog_search 必须成功");
    assert_eq!(search.provider_count, 2);
    assert_eq!(search.entries.len(), 1);
    assert_eq!(search.entries[0].provider_id, "deepseek");
    // 直营官方精选分类保持 official。
    assert_eq!(search.entries[0].curated_kind.as_deref(), Some("official"));
    let all = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog_search(
        None,
        Some(50),
        Some("all".to_string()),
    ))
    .unwrap();
    assert_eq!(all.entries.len(), 2, "all 作用域含非精选条目");
    assert!(all
        .entries
        .iter()
        .any(|entry| entry.provider_id == "aggregator-x" && entry.curated_kind.is_none()));
    let bad_scope = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog_search(
        None,
        None,
        Some("everything".to_string()),
    ))
    .unwrap_err();
    assert_eq!(bad_scope.code, "usage-settings-invalid");
    assert_eq!(bad_scope.detail, "pricing catalog scope is invalid");
    // scope 缺省 = curated。
    let default_scope =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog_search(None, None, None))
            .unwrap();
    assert!(
        default_scope
            .entries
            .iter()
            .all(|entry| entry.curated_kind.is_some()),
        "缺省作用域必须等价 curated"
    );

    // 自动匹配（固定精选）：归一化命中直营官方。
    // 本夹具 deepseek 是官方、aggregator-x 非精选（release_date 更新也不得翻盘）。
    let matched = tauri::async_runtime::block_on(v2ipc::usage_pricing_auto_match(
        "deepseek-v4.1-flash".to_string(),
        None,
    ))
    .expect("usage_pricing_auto_match 必须成功");
    assert_eq!(matched.kind, "exact");
    assert_eq!(
        matched.reference.as_ref().unwrap().provider_id,
        "deepseek",
        "官方优先（aggregator-x 的 release_date 更新也不得翻盘）"
    );
    let matched_quote = matched.quote.as_ref().unwrap();
    assert_eq!(matched_quote.snapshot_date, Some(1_000));
    // 双币种面：无原生人民币价 ⇒ CNY 由 USD 按当日汇率换算并标 converted；
    // fx 面来自快照缺失时的兜底链（source=default / fallback=true）。
    assert!(matched_quote.cny_converted);
    assert_eq!(matched_quote.input_cny.as_deref(), Some("7.2"));
    assert_eq!(matched_quote.output_cny.as_deref(), Some("14.4"));
    assert_eq!(matched_quote.fx_rate.as_deref(), Some("7.2"));
    assert!(matched_quote.fx_fetched_at.is_none());
    assert!(matched_quote.fx_fallback);

    // 价格预览：精确键四价 + 快照日期；无条目 null。
    let quote = tauri::async_runtime::block_on(v2ipc::usage_pricing_quote_get(
        "deepseek".to_string(),
        "deepseek-v4.1-flash".to_string(),
        None,
    ))
    .expect("usage_pricing_quote_get 必须成功")
    .expect("精确键必须命中");
    assert_eq!(quote.input.as_deref(), Some("1"));
    assert_eq!(quote.output.as_deref(), Some("2"));
    assert_eq!(quote.snapshot_date, Some(1_000));
    let miss = tauri::async_runtime::block_on(v2ipc::usage_pricing_quote_get(
        "aggregator-x".to_string(),
        "not-a-model".to_string(),
        None,
    ))
    .unwrap();
    assert!(miss.is_none(), "目录无该条目 ⇒ null（不是错误）");
}

/// 2026-10-21 精选两档（IPC 真跑）：自动匹配 glm-5.3 必须取直营官方 zhipuai
///（而非 OpenRouter）；Ollama Cloud 不再参与自动匹配（仅有 hosted 的模型落到
/// OpenRouter，纯 hosted 模型 kind=none）；种入原生人民币价后预览的 CNY 面改为
/// 原生价且不再标 converted。
#[test]
fn v2_auto_match_prefers_official_ignores_hosted_and_native_cny_skips_conversion() {
    let _g = serial();

    // 其它用例可能种过快照：先清 active，保证本用例从零种快照。
    {
        let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE usage_pricing_state SET active_snapshot_id = NULL WHERE singleton = 1",
            [],
        )
        .expect("清空 active 快照必须成功");
    }
    const CATALOG: &str = r#"{
        "zhipuai": {"models": {"glm-5.3": {
            "release_date": "2025-01-01",
            "cost": {"input": 1, "output": 8},
            "modalities": {"input": ["text"], "output": ["text"]}
        }}},
        "ollama-cloud": {"models": {
            "glm-5.3": {"release_date": "2025-09-01", "cost": {"input": 5, "output": 20},
                "modalities": {"input": ["text"], "output": ["text"]}},
            "deepseek-v4.1-flash": {"release_date": "2025-09-01", "cost": {"input": 1, "output": 2},
                "modalities": {"input": ["text"], "output": ["text"]}},
            "hosted-only": {"release_date": "2025-09-01", "cost": {"input": 2, "output": 4},
                "modalities": {"input": ["text"], "output": ["text"]}}
        }},
        "openrouter": {"models": {
            "glm-5.3": {"release_date": "2025-12-01", "cost": {"input": 9, "output": 90},
                "modalities": {"input": ["text"], "output": ["text"]}},
            "deepseek-v4.1-flash": {"release_date": "2025-12-01", "cost": {"input": 7, "output": 70},
                "modalities": {"input": ["text"], "output": ["text"]}}
        }}
    }"#;
    let snapshot_id = {
        let mut conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        pricing::set_enabled_with_conn(&conn, true).expect("启用定价必须成功");
        pricing::refresh_with_conn(&mut conn, 1_000, true, || Ok(CATALOG.as_bytes().to_vec()))
            .expect("刷新快照必须成功");
        conn.query_row(
            "SELECT active_snapshot_id FROM usage_pricing_state WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("读取 active 快照 id 必须成功")
    };

    // 自动匹配：glm-5.3（大小写不敏感）取直营官方 zhipuai。
    let matched = tauri::async_runtime::block_on(v2ipc::usage_pricing_auto_match(
        "GLM-5.3".to_string(),
        None,
    ))
    .expect("usage_pricing_auto_match 必须成功");
    assert_eq!(matched.kind, "exact");
    assert_eq!(
        matched.reference.as_ref().unwrap().provider_id,
        "zhipuai",
        "直营官方优先于 OpenRouter（后者 release_date 更新也不翻盘）"
    );
    let quote = matched.quote.as_ref().unwrap();
    assert_eq!(quote.input.as_deref(), Some("1"));
    // 无原生人民币价 ⇒ CNY 面按当日汇率换算并标 converted；fx 走内置兜底。
    assert!(quote.cny_converted, "无原生人民币价必须标 converted");
    assert_eq!(quote.input_cny.as_deref(), Some("7.2"));
    assert_eq!(quote.output_cny.as_deref(), Some("57.6"));
    assert_eq!(quote.fx_rate.as_deref(), Some("7.2"));
    assert!(quote.fx_fallback);
    assert!(quote.fx_fetched_at.is_none());

    // 无直营官方：Ollama Cloud 已不参与精选 ⇒ 落到 OpenRouter。
    let fallback = tauri::async_runtime::block_on(v2ipc::usage_pricing_auto_match(
        "deepseek-v4.1-flash".to_string(),
        None,
    ))
    .expect("usage_pricing_auto_match 必须成功");
    assert_eq!(fallback.kind, "exact");
    assert_eq!(
        fallback.reference.as_ref().unwrap().provider_id,
        "openrouter"
    );
    assert_eq!(fallback.quote.as_ref().unwrap().input.as_deref(), Some("7"));

    // 仅 hosted、无 OpenRouter 候选 ⇒ 精选作用域无命中（ollama-cloud 被忽略）。
    let hosted_only = tauri::async_runtime::block_on(v2ipc::usage_pricing_auto_match(
        "hosted-only".to_string(),
        None,
    ))
    .expect("usage_pricing_auto_match 必须成功");
    assert_eq!(hosted_only.kind, "none");
    assert!(hosted_only.reference.is_none());

    // 种原生人民币价（zhipuai/glm-5.3，`List price: CNY 8/2/28` 的解析形态）⇒
    // 预览 CNY 面改为原生价，converted=false；USD 面不变。
    let normalized = pricing::normalize_model_id("glm-5.3");
    {
        let mut conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        usage_dao::replace_native_prices_conn(
            &mut conn,
            &snapshot_id,
            &[usage_dao::NativePriceRecord {
                provider_id: "zhipuai".to_string(),
                model_id: "glm-5.3".to_string(),
                normalized_model_id: normalized,
                rates: [
                    Some(8_000_000_000),
                    Some(2_000_000_000),
                    Some(0),
                    Some(28_000_000_000),
                ],
                source_url: "https://example.test/zhipuai/glm-5.3.toml".to_string(),
                fetched_at: 1_000,
            }],
        )
        .expect("种原生人民币价必须成功");
    }
    let native_quote = tauri::async_runtime::block_on(v2ipc::usage_pricing_quote_get(
        "zhipuai".to_string(),
        "glm-5.3".to_string(),
        None,
    ))
    .expect("usage_pricing_quote_get 必须成功")
    .expect("精确键必须命中");
    assert_eq!(
        native_quote.input.as_deref(),
        Some("1"),
        "USD 面用目录原生价"
    );
    assert_eq!(native_quote.input_cny.as_deref(), Some("8"));
    assert_eq!(native_quote.cache_read_cny.as_deref(), Some("2"));
    assert_eq!(native_quote.cache_write_cny.as_deref(), Some("0"));
    assert_eq!(native_quote.output_cny.as_deref(), Some("28"));
    assert!(
        !native_quote.cny_converted,
        "原生人民币价不得标 converted（换算面才标）"
    );
    // fallbackRate 透传：有原生人民币价时 CNY 不受汇率影响，fx 面仍如实回传。
    let with_fallback = tauri::async_runtime::block_on(v2ipc::usage_pricing_quote_get(
        "zhipuai".to_string(),
        "glm-5.3".to_string(),
        Some("7.5".to_string()),
    ))
    .expect("usage_pricing_quote_get(fallbackRate) 必须成功")
    .expect("精确键必须命中");
    assert_eq!(with_fallback.input_cny.as_deref(), Some("8"));
    assert_eq!(with_fallback.fx_rate.as_deref(), Some("7.5"));
}

/// 2026-10-20 币种扩展：`usage_costs(fallbackRate)`、`PricingStatus`/`UsageCosts` 的
/// fx 面 wire 形状（serde 后的 JSON）。
#[test]
fn v2_commands_serialize_contract_shapes() {
    let _g = serial();

    // 其它用例可能改过总开关；本用例显式打开。
    tauri::async_runtime::block_on(ipc::usage_set_settings(UsageSettingsPatch {
        enabled: Some(true),
        ..UsageSettingsPatch::default()
    }))
    .expect("打开总开关必须成功");

    let config = tauri::async_runtime::block_on(v2ipc::usage_billing_get())
        .expect("usage_billing_get 必须成功");
    assert_eq!(
        obj_keys(&serde_json::to_value(&config).unwrap()),
        vec!["pricingRules", "profiles", "revision", "rules"],
        "BillingConfig 的 wire 字段逐字冻结"
    );

    let preview =
        tauri::async_runtime::block_on(v2ipc::usage_billing_preview(config.clone(), today()))
            .expect("usage_billing_preview 必须成功");
    let preview_value = serde_json::to_value(&preview).unwrap();
    assert_eq!(
        obj_keys(&preview_value),
        vec![
            "ambiguousTokens",
            "baseRevision",
            "boundaryTokens",
            "candidate",
            "matchedIdentities",
            "matchedTokens",
        ],
        "BillingPreview 的 wire 字段逐字冻结"
    );
    assert_eq!(preview.candidate.revision, preview.base_revision + 1);
    assert!(preview_value["matchedIdentities"].is_array());

    let committed = tauri::async_runtime::block_on(v2ipc::usage_billing_commit(BillingCommit {
        expected_revision: config.revision,
        candidate: config.clone(),
    }))
    .expect("usage_billing_commit 必须成功");
    assert_eq!(committed.revision, config.revision + 1);
    let undone = tauri::async_runtime::block_on(v2ipc::usage_billing_undo(
        committed.revision,
        config.revision,
    ))
    .expect("usage_billing_undo 必须成功");
    assert_eq!(undone.revision, committed.revision + 1);

    let identities =
        tauri::async_runtime::block_on(v2ipc::usage_identities(today(), UsageFilters::default()))
            .expect("usage_identities 必须成功");
    assert!(
        serde_json::to_value(identities).unwrap().is_array(),
        "IdentityUsage[] 必须是数组"
    );

    let trend = tauri::async_runtime::block_on(v2ipc::usage_trend(today(), TrendSelection::All))
        .expect("usage_trend 必须成功");
    assert_eq!(
        obj_keys(&serde_json::to_value(trend).unwrap()),
        vec![
            "available",
            "compare",
            "hero",
            "reason",
            "selection",
            "trend"
        ],
        "UsageTrend 的 wire 字段逐字冻结"
    );

    let distribution =
        tauri::async_runtime::block_on(v2ipc::usage_distribution(today(), DistributionGroup::Tool))
            .expect("usage_distribution 必须成功");
    assert_eq!(
        obj_keys(&serde_json::to_value(distribution).unwrap()),
        vec!["groupBy", "rows"],
        "UsageDistribution 的 wire 字段逐字冻结"
    );

    let pricing = tauri::async_runtime::block_on(v2ipc::usage_pricing_get())
        .expect("usage_pricing_get 必须成功");
    assert_eq!(
        obj_keys(&serde_json::to_value(&pricing).unwrap()),
        vec![
            "enabled",
            "fetchedAt",
            "fxFallback",
            "fxFetchedAt",
            "fxRate",
            "fxSource",
            "lastError",
            "refreshing",
            "snapshotId",
            "stale",
        ],
        "PricingStatus 的 wire 字段逐字冻结（含 2026-10-20 fx 面）"
    );
    // 关闭分支不会触发网络；本用例只验证命令接通与 wire 形状。
    let disabled = tauri::async_runtime::block_on(v2ipc::usage_pricing_set_enabled(false))
        .expect("usage_pricing_set_enabled 必须成功");
    assert!(!disabled.enabled);
    let refreshed = tauri::async_runtime::block_on(v2ipc::usage_pricing_refresh())
        .expect("usage_pricing_refresh 必须成功");
    assert!(!refreshed.enabled);
    assert_eq!(
        obj_keys(&serde_json::to_value(&refreshed).unwrap()).len(),
        10
    );

    // usage_costs 新增可选 fallbackRate（设置页备用汇率）；UsageCosts 带 fx 面。
    let costs = tauri::async_runtime::block_on(v2ipc::usage_costs(today(), None))
        .expect("usage_costs 必须成功");
    let costs_value = serde_json::to_value(&costs).unwrap();
    assert_eq!(
        obj_keys(&costs_value),
        vec![
            "fxFallback",
            "fxFetchedAt",
            "fxRate",
            "fxSource",
            "pricing",
            "range",
            "rows"
        ]
    );
    assert!(costs_value["rows"].is_array());
    assert_eq!(costs_value["fxRate"], "7.2", "无快照无缓存 ⇒ 内置兜底汇率");
    assert_eq!(costs_value["fxSource"], "default");
    assert_eq!(costs_value["fxFallback"], true);
    assert!(costs_value["fxFetchedAt"].is_null());
    // fallbackRate 显式传入且有效 ⇒ source=settings（仍是 fallback）。
    let with_fallback =
        tauri::async_runtime::block_on(v2ipc::usage_costs(today(), Some("7.5".to_string())))
            .expect("usage_costs(fallbackRate) 必须成功");
    assert_eq!(
        serde_json::to_value(&with_fallback).unwrap()["fxSource"],
        "settings"
    );
    assert_eq!(
        with_fallback.fx_rate.as_deref(),
        Some("7.5"),
        "设置页备用汇率参与换算"
    );

    let csv = tauri::async_runtime::block_on(v2ipc::usage_cost_export_csv(today()))
        .expect("usage_cost_export_csv 必须成功");
    assert!(csv.starts_with(
        "billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at,channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override,price_rule_kind\n"
    ));
    assert!(!csv.to_lowercase().contains("total"));
}

/// 手工测试回归：参考 API 模型下拉必须能搜索目录，而不是让用户手打 provider/model。
///
/// 真跑 `usage_pricing_catalog`（真命令 + 真全局库 + 真快照）：无快照返回空数组；
/// 有快照时大小写不敏感命中、limit clamp、wire 字段逐字冻结。
#[test]
fn v2_pricing_catalog_command_searches_active_snapshot() {
    let _g = serial();

    // 无快照：空数组（不是错误；前端用 usage_pricing_get 区分「无快照」）。
    // 注意：其它用例可能先种过快照，故先显式清空 active 指针。
    {
        let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE usage_pricing_state SET active_snapshot_id = NULL WHERE singleton = 1",
            [],
        )
        .expect("清空 active 快照必须成功");
    }
    let empty = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog(None, None))
        .expect("usage_pricing_catalog 必须成功");
    assert!(empty.is_empty(), "无快照必须返回空数组");

    const FIXTURE: &str = r#"{
        "ollama-cloud": {"models": {
            "deepseek-v4.1-flash": {
                "release_date": "2025-09-01",
                "cost": {"input": 1},
                "modalities": {"input": ["text"], "output": ["text"]}
            },
            "kimi-k2": {
                "release_date": "2025-08-01",
                "modalities": {"input": ["text"], "output": ["text"]}
            }
        }},
        "zhipuai-coding-plan": {"models": {
            "glm-5.3": {
                "release_date": "2025-10-01",
                "cost": {"input": 2},
                "modalities": {"input": ["text"], "output": ["text"]}
            }
        }}
    }"#;
    {
        let mut conn = DB.lock().unwrap_or_else(|e| e.into_inner());
        pricing::set_enabled_with_conn(&conn, true).expect("启用定价必须成功");
        pricing::refresh_with_conn(&mut conn, 1_000, false, || Ok(FIXTURE.as_bytes().to_vec()))
            .expect("刷新快照必须成功");
    }

    let entries = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog(
        Some("DEEPSEEK".to_string()),
        Some(20),
    ))
    .expect("usage_pricing_catalog 必须成功");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.provider_id, "ollama-cloud");
    assert_eq!(entry.model_id, "deepseek-v4.1-flash");
    let wire = serde_json::to_value(entry).unwrap();
    assert_eq!(
        obj_keys(&wire),
        vec![
            "conditional",
            "curatedKind",
            "hasRates",
            "modelId",
            "providerId",
            "releaseDate"
        ],
        "PricingCatalogEntry 的 wire 字段逐字冻结（含 curatedKind）"
    );
    assert_eq!(wire["providerId"], "ollama-cloud");
    assert_eq!(wire["hasRates"], true);
    assert_eq!(wire["conditional"], false);
    assert_eq!(wire["releaseDate"], "2025-09-01");

    // 空 query 按下限/排序；limit=0 clamp 到 1。
    let clamped =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog(None, Some(0))).unwrap();
    assert_eq!(clamped.len(), 1, "limit=0 必须 clamp 到 1");
    let all = tauri::async_runtime::block_on(v2ipc::usage_pricing_catalog(None, Some(50))).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(
        all.iter()
            .map(|entry| (entry.provider_id.as_str(), entry.model_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("ollama-cloud", "deepseek-v4.1-flash"),
            ("ollama-cloud", "kimi-k2"),
            ("zhipuai-coding-plan", "glm-5.3"),
        ]
    );
    assert!(
        !all[1].has_rates,
        "缺价条目必须如实 hasRates=false（不是 0 价）"
    );
}

/// Task 6：7 条别名 / 渠道选项 / 覆盖价命令真跑（真命令 + 真锁 + 真临时库）。
///
/// 覆盖契约行为：按 id upsert / 键冲突拒绝 / 删除幂等；覆盖价四价至少一项、
/// 非法费率拒绝、归一化模型键与来源字段的 wire 形状。
#[test]
fn v2_alias_and_override_commands_round_trip() {
    let _g = serial();

    let initial = tauri::async_runtime::block_on(v2ipc::usage_provider_aliases_get())
        .expect("usage_provider_aliases_get 必须成功");
    assert!(
        initial.aliases.is_empty(),
        "前提：临时库初始无别名（若有，说明前序用例漏清理）"
    );

    let provider_rule = ProviderAliasRule {
        id: "ipc-alias-openai".to_string(),
        kind: ProviderAliasKind::Provider,
        provider: Some("openai".to_string()),
        source_id: None,
        model_id: None,
        display_name: "OpenAI".to_string(),
    };
    let after_provider =
        tauri::async_runtime::block_on(v2ipc::usage_provider_alias_upsert(provider_rule.clone()))
            .expect("provider 别名 upsert 必须成功");
    assert_eq!(after_provider.aliases.len(), 1);
    assert_eq!(after_provider.aliases[0], provider_rule);
    let wire = serde_json::to_value(&after_provider).unwrap();
    assert_eq!(obj_keys(&wire), vec!["aliases"]);
    assert_eq!(
        obj_keys(&wire["aliases"][0]),
        vec![
            "displayName",
            "id",
            "kind",
            "modelId",
            "provider",
            "sourceId"
        ],
        "ProviderAliasRule 的 wire 字段逐字冻结"
    );
    assert_eq!(wire["aliases"][0]["kind"], "provider");
    assert!(wire["aliases"][0]["sourceId"].is_null());
    assert!(wire["aliases"][0]["modelId"].is_null());

    let tool_rule = ProviderAliasRule {
        id: "ipc-alias-workbuddy".to_string(),
        kind: ProviderAliasKind::ToolModel,
        provider: None,
        source_id: Some(UsageSourceId::WorkBuddy),
        model_id: Some(String::new()),
        display_name: "WorkBuddy".to_string(),
    };
    let after_tool =
        tauri::async_runtime::block_on(v2ipc::usage_provider_alias_upsert(tool_rule.clone()))
            .expect("toolModel 别名 upsert 必须成功");
    assert_eq!(after_tool.aliases.len(), 2);
    let wire = serde_json::to_value(
        after_tool
            .aliases
            .iter()
            .find(|rule| rule.id == tool_rule.id)
            .expect("刚写入的规则必须出现在返回配置里"),
    )
    .unwrap();
    assert_eq!(wire["kind"], "toolModel");
    assert_eq!(wire["sourceId"], "workbuddy");
    assert_eq!(wire["modelId"], "", "空串表示该工具的未知模型");

    // 同 id 覆盖式更新（不是重复键）
    let renamed = ProviderAliasRule {
        display_name: "OpenAI API".to_string(),
        ..provider_rule.clone()
    };
    let after_rename = tauri::async_runtime::block_on(v2ipc::usage_provider_alias_upsert(renamed))
        .expect("同 id 更新必须成功");
    assert_eq!(
        after_rename
            .aliases
            .iter()
            .find(|rule| rule.id == provider_rule.id)
            .map(|rule| rule.display_name.as_str()),
        Some("OpenAI API")
    );

    // 目标键被另一条规则占用 → 结构化拒绝，不静默合并
    let duplicate = ProviderAliasRule {
        id: "ipc-alias-openai-copy".to_string(),
        ..provider_rule.clone()
    };
    let err = tauri::async_runtime::block_on(v2ipc::usage_provider_alias_upsert(duplicate))
        .expect_err("重复键必须拒绝");
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(
        err.detail,
        "provider alias key already exists: provider:openai"
    );
    assert_eq!(
        tauri::async_runtime::block_on(v2ipc::usage_provider_aliases_get())
            .expect("回读必须成功")
            .aliases
            .len(),
        2,
        "被拒绝的写入不得留下半条规则"
    );

    let options = tauri::async_runtime::block_on(v2ipc::usage_channel_options(today()))
        .expect("usage_channel_options 必须成功");
    let wire = serde_json::to_value(&options).unwrap();
    assert!(wire.is_array(), "ChannelOption[] 必须是数组");
    for option in wire.as_array().unwrap() {
        assert_eq!(
            obj_keys(option),
            vec!["billingProfileId", "key", "kind", "label", "tokenUsage"],
            "ChannelOption 的 wire 字段逐字冻结"
        );
        let kind = option["kind"].as_str().unwrap();
        assert!(
            ["billingProfile", "alias", "provider"].contains(&kind),
            "趋势选项不得包含 {kind}（未指定/混合不可作为选项）"
        );
    }

    let overrides_before = tauri::async_runtime::block_on(v2ipc::usage_pricing_overrides_get())
        .expect("usage_pricing_overrides_get 必须成功");
    assert!(
        overrides_before.is_empty(),
        "前提：临时库初始无覆盖价（若有，说明前序用例漏清理）"
    );

    let request = PriceOverrideRequest {
        normalized_model_id: "GPT-5".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("1.5".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("7".to_string()),
        time_rule: None,
    };
    let after_set = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(request))
        .expect("覆盖价写入必须成功");
    let view = after_set
        .iter()
        .find(|view| view.normalized_model_id == "gpt-5")
        .expect("模型 ID 必须被机械归一化为 gpt-5");
    assert_eq!(view.input.as_deref(), Some("1.5"));
    assert_eq!(view.output.as_deref(), Some("7"));
    assert!(view.cache_read.is_none() && view.cache_write.is_none());
    assert_eq!(view.currency, PriceCurrency::Usd);
    assert!(view.updated_at > 0);
    let wire = serde_json::to_value(view).unwrap();
    assert_eq!(
        obj_keys(&wire),
        vec![
            "cacheRead",
            "cacheWrite",
            "currency",
            "input",
            "normalizedModelId",
            "output",
            "releaseDate",
            "snapshotDate",
            "sourceModel",
            "sourceProvider",
            "updatedAt"
        ],
        "PriceOverrideView 的 wire 字段逐字冻结"
    );
    assert!(
        wire["sourceProvider"].is_null(),
        "无目录时来源必须显式 null"
    );
    assert!(
        wire["snapshotDate"].is_null(),
        "无目录时快照日期必须显式 null"
    );

    // CNY 覆盖价：币种按请求落库并回读（显示另一币种时的换算由费用查询负责）。
    let cny_request = PriceOverrideRequest {
        normalized_model_id: "cny-model".to_string(),
        currency: PriceCurrency::Cny,
        input: Some("8".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("28".to_string()),
        time_rule: None,
    };
    let after_cny = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(cny_request))
        .expect("CNY 覆盖价写入必须成功");
    let cny_view = after_cny
        .iter()
        .find(|view| view.normalized_model_id == "cny-model")
        .expect("CNY 覆盖价条目必须在列");
    assert_eq!(cny_view.currency, PriceCurrency::Cny);
    assert_eq!(
        serde_json::to_value(cny_view).unwrap()["currency"],
        "CNY",
        "CNY 币种逐字上 wire"
    );

    let empty = PriceOverrideRequest {
        normalized_model_id: "gpt-5".to_string(),
        currency: PriceCurrency::Usd,
        input: None,
        cache_read: None,
        cache_write: None,
        output: None,
        time_rule: None,
    };
    let err = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(empty))
        .expect_err("四价全空必须拒绝");
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(err.detail, "price override must set at least one rate");

    let bad_rate = PriceOverrideRequest {
        normalized_model_id: "gpt-5".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("abc".to_string()),
        cache_read: None,
        cache_write: None,
        output: None,
        time_rule: None,
    };
    let err = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(bad_rate))
        .expect_err("非法费率必须拒绝");
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(err.detail, "price override rate is invalid: input");

    let after_delete =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_override_delete("GPT-5".to_string()))
            .expect("覆盖价删除必须成功");
    assert!(
        after_delete
            .iter()
            .all(|view| view.normalized_model_id != "gpt-5"),
        "删除后该模型必须不在列"
    );
    // 幂等：删除不存在的模型不报错
    let after_noop =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_override_delete("gpt-5".to_string()))
            .expect("重复删除必须幂等");
    assert!(after_noop
        .iter()
        .all(|view| view.normalized_model_id != "gpt-5"));

    // 清理覆盖价，避免污染同二进制后续用例
    tauri::async_runtime::block_on(v2ipc::usage_pricing_override_delete(
        "cny-model".to_string(),
    ))
    .expect("清理 CNY 覆盖价必须成功");

    // 清理别名，避免污染同二进制后续用例
    for id in ["ipc-alias-openai", "ipc-alias-workbuddy"] {
        tauri::async_runtime::block_on(v2ipc::usage_provider_alias_delete(id.to_string()))
            .expect("清理别名必须成功");
    }
    assert!(
        tauri::async_runtime::block_on(v2ipc::usage_provider_aliases_get())
            .expect("最终回读必须成功")
            .aliases
            .is_empty(),
        "用例结束必须恢复空别名状态"
    );
}

/// 2026-10-21 分时价格（IPC 真跑）：固定价 → 分时价 → 回到固定价的完整 roundtrip；
/// 分时规则保留 timezone / 跨午夜窗口 / 低谷四价；非法窗口与空低谷经真命令拒绝。
#[test]
fn v2_time_price_override_commands_round_trip() {
    let _g = serial();

    // 固定价：timeRule 在视图 wire 上省略（不是 null）。
    let fixed = PriceOverrideRequest {
        normalized_model_id: "time-roundtrip".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("5".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("20".to_string()),
        time_rule: None,
    };
    let after_fixed =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(fixed)).unwrap();
    let fixed_view = after_fixed
        .iter()
        .find(|view| view.normalized_model_id == "time-roundtrip")
        .expect("固定价条目必须在列");
    assert!(fixed_view.time_rule.is_none());
    assert!(
        serde_json::to_value(fixed_view)
            .unwrap()
            .get("timeRule")
            .is_none(),
        "固定价视图必须省略 timeRule"
    );

    // 分时价：跨午夜窗口合法；时区/窗口/低谷价完整保留。
    let timed = PriceOverrideRequest {
        normalized_model_id: "time-roundtrip".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("5".to_string()),
        cache_read: None,
        cache_write: None,
        output: Some("20".to_string()),
        time_rule: Some(TimePriceRule {
            timezone: TimePriceTimezone::Utc,
            windows: vec![
                TimePriceWindow {
                    start: "09:00".to_string(),
                    end: "12:00".to_string(),
                },
                TimePriceWindow {
                    start: "22:00".to_string(),
                    end: "02:00".to_string(),
                },
            ],
            offpeak_input: Some("2".to_string()),
            offpeak_cache_read: None,
            offpeak_cache_write: None,
            offpeak_output: Some("8".to_string()),
        }),
    };
    let after_timed =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(timed)).unwrap();
    let timed_view = after_timed
        .iter()
        .find(|view| view.normalized_model_id == "time-roundtrip")
        .expect("分时价条目必须在列");
    let rule = timed_view.time_rule.as_ref().expect("timeRule 必须回读");
    assert_eq!(rule.timezone, TimePriceTimezone::Utc);
    assert_eq!(
        rule.windows,
        vec![
            TimePriceWindow {
                start: "09:00".to_string(),
                end: "12:00".to_string(),
            },
            TimePriceWindow {
                start: "22:00".to_string(),
                end: "02:00".to_string(),
            }
        ]
    );
    assert_eq!(rule.offpeak_input.as_deref(), Some("2"));
    assert_eq!(rule.offpeak_output.as_deref(), Some("8"));
    let wire = serde_json::to_value(timed_view).unwrap();
    assert_eq!(wire["timeRule"]["timezone"], "utc");
    assert_eq!(wire["timeRule"]["windows"][1]["start"], "22:00");
    assert_eq!(wire["timeRule"]["windows"][1]["end"], "02:00");
    assert_eq!(wire["timeRule"]["offpeakInput"], "2");

    // 非法窗口（非整点）拒绝。
    let bad_window = PriceOverrideRequest {
        normalized_model_id: "time-roundtrip".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("5".to_string()),
        cache_read: None,
        cache_write: None,
        output: None,
        time_rule: Some(TimePriceRule {
            timezone: TimePriceTimezone::Local,
            windows: vec![TimePriceWindow {
                start: "09:30".to_string(),
                end: "12:00".to_string(),
            }],
            offpeak_input: Some("2".to_string()),
            offpeak_cache_read: None,
            offpeak_cache_write: None,
            offpeak_output: None,
        }),
    };
    let err = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(bad_window))
        .expect_err("非整点窗口必须拒绝");
    assert_eq!(err.code, "usage-settings-invalid");
    assert!(err.detail.contains("whole hours"), "detail={}", err.detail);

    // 空低谷拒绝。
    let empty_offpeak = PriceOverrideRequest {
        normalized_model_id: "time-roundtrip".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("5".to_string()),
        cache_read: None,
        cache_write: None,
        output: None,
        time_rule: Some(TimePriceRule {
            timezone: TimePriceTimezone::Local,
            windows: vec![TimePriceWindow {
                start: "09:00".to_string(),
                end: "12:00".to_string(),
            }],
            offpeak_input: None,
            offpeak_cache_read: None,
            offpeak_cache_write: None,
            offpeak_output: None,
        }),
    };
    let err = tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(empty_offpeak))
        .expect_err("低谷四价全空必须拒绝");
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(
        err.detail,
        "time price rule must set at least one offpeak rate"
    );

    // 回到固定价必须清空分时规则。
    let fixed_again = PriceOverrideRequest {
        normalized_model_id: "time-roundtrip".to_string(),
        currency: PriceCurrency::Usd,
        input: Some("3".to_string()),
        cache_read: None,
        cache_write: None,
        output: None,
        time_rule: None,
    };
    let after_fixed_again =
        tauri::async_runtime::block_on(v2ipc::usage_pricing_override_set(fixed_again)).unwrap();
    assert!(after_fixed_again
        .iter()
        .find(|view| view.normalized_model_id == "time-roundtrip")
        .unwrap()
        .time_rule
        .is_none());

    // 清理，避免污染同二进制后续用例。
    tauri::async_runtime::block_on(v2ipc::usage_pricing_override_delete(
        "time-roundtrip".to_string(),
    ))
    .expect("清理分时覆盖价必须成功");
}
