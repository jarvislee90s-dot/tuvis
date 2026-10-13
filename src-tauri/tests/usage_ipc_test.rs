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
use multi_agents_manager_lib::services::usage::collect;
use multi_agents_manager_lib::services::usage::error::USAGE_CODES;
use multi_agents_manager_lib::services::usage::model::{
    UsageFilters, UsageGroupBy, UsageRange, UsageRangePreset, UsageSettings, UsageSettingsPatch,
};

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
        detail_retention_days: Some(before + 7),
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
        checked, 8,
        "本文件应有 8 条用例（含本自省用例）；新增用例必须同步持有 serial()"
    );
}
