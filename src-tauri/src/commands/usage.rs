//! 用量域 IPC（契约 §3）。纪律：
//! * **只有 `usage_collect` 会扫描**，且它有单飞 + 最小间隔保护（Task 10）；三个查询命令
//!   只读本地账本（零文件扫描）；
//! * 采集经 `spawn_blocking` 出主线程（同步命令在主线程执行，codex 全量可达 1–3 分钟）；
//! * 错误一律结构化 `UsageError`（前端按 `usage.rpc.<code>` 翻译）。
//!
//! ## ⚠️ 锁序（W-26，本层最容易踩的一条）
//! 本层**不得**自己取 `DB` 锁：三个查询命令把锁定留给 `query::dashboard` / `records` /
//! `export_csv`（它们的安全形式是「先持 `DB` 锁、guard 存活期间只调 `collect::cached_result()`」），
//! 而**持 `DB` 锁时绝不能再调采集入口**（`collect()` / `collect_with()` / `run_collection()`）——
//! 那会与在飞扫描的「飞行锁 → DB」构成 ABBA 死锁，挂死**整个 app 的 DB 访问**。
//! 采集入口在本文件里只有一处、且在**不持任何锁**的命令入口上（`usage_collect`）。
//! 锁：本模块 `mod tests` 的 `command_layer_never_locks_db_or_calls_collection_entries`。
//!
//! ## ⚠️ GC 3 / AGENTS.md 扫描预算契约
//! 用量采集**绝不进入 3 秒会话轮询热路径**：启动采集是 `lib.rs` setup 里
//! `spawn_initial_collection()` 的**延迟 3s 后台线程**（刻意如此，**不要**改成同步调用）。
//! 锁：`collect::collect_calls()` 的行为式用例在 `tests/usage_polling_budget_test.rs`。

use crate::services::usage::error::UsageError;
use crate::services::usage::model::*;
use crate::services::usage::{collect, query, settings};

#[tauri::command]
pub async fn usage_collect(force: bool) -> Result<UsageCollectResult, UsageError> {
    tauri::async_runtime::spawn_blocking(move || collect::collect(force))
        .await
        .map_err(|e| UsageError::internal(format!("collect task failed: {e}")))
}

#[tauri::command]
pub async fn usage_dashboard(
    range: UsageRange,
    group_by: UsageGroupBy,
) -> Result<UsageDashboard, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::dashboard(&range, group_by, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| UsageError::internal(format!("query task failed: {e}")))?
}

#[tauri::command]
pub async fn usage_records(
    range: UsageRange,
    group_by: UsageGroupBy,
    filters: UsageFilters,
) -> Result<UsageRecords, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::records(&range, group_by, &filters, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| UsageError::internal(format!("query task failed: {e}")))?
}

#[tauri::command]
pub async fn usage_export_csv(
    range: UsageRange,
    group_by: UsageGroupBy,
    filters: UsageFilters,
) -> Result<String, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::export_csv(&range, group_by, &filters, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| UsageError::internal(format!("export task failed: {e}")))?
}

/// 读用量设置。
///
/// **A-5（P2-3）**：改成 `async` + `spawn_blocking` —— `settings::load()` 会取全局
/// `DB.lock()` 并读 `tuvis.db`，同步命令在**主线程**上执行 ⇒ 最坏 0.5s UI 冻结
/// （与 `commands/session.rs:10-12` 同一个坑）。**JS 侧签名与返回形状一字未动**
/// （命令名 / 入参 / 返回都不变），契约 §3 冻结的是这三样、不含 sync/async。
#[tauri::command]
pub async fn usage_get_settings() -> UsageSettings {
    tauri::async_runtime::spawn_blocking(settings::load)
        .await
        .unwrap_or_else(|e| {
            // 任务 panic：不得让命令静默无返回。设置是**可空态友好**的读路径（前端有默认值
            // 兜底），这里响亮记一笔后回落 `Default` —— 与 `commands/session.rs` 的
            // `get_all_sessions` 同款取舍。
            ::log::error!("用量设置读取任务异常: {e}");
            Default::default()
        })
}

/// 写用量设置。
///
/// **A-5（P2-3）**：同 `usage_get_settings` —— `merge_patch` + `save`（写后回读）整段都在
/// `spawn_blocking` 里，主线程只 await。错误语义不变：补丁越界 →
/// `usage-settings-invalid`、写失败 → `usage-db-failed`（裁决 F）。
#[tauri::command]
pub async fn usage_set_settings(patch: UsageSettingsPatch) -> Result<UsageSettings, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let merged = settings::merge_patch(&settings::load(), &patch)?;
        // 裁决 F：写失败**不得**报成成功（DAO 的 `let _ =` 会吞掉 execute 错误 →
        // `settings::save` 做写后回读，不一致即以 `usage-db-failed` 报错）。
        settings::save(&merged)?;
        Ok(merged)
    })
    .await
    .map_err(|e| UsageError::internal(format!("settings write task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    // **D-01**：`use super::*;` 拿不到父模块那条 `use crate::services::usage::model::*;`
    // 的 glob 再导出（glob 不传递 glob，实测 E0433/E0422 一片），故显式导入所需值对象。
    use crate::services::usage::model::{
        CompareBlock, LongestTurn, RecentSessionUsage, SourceKind, TopTool, TopToolMs, TrendPoint,
        UsageAvailability, UsageBuckets, UsageCard, UsageDashboard, UsageGroupBy, UsageMetric,
        UsageMetrics, UsageRange, UsageRangePreset, UsageRecords, UsageRow, WorkSummary,
    };

    /// 本文件源码（自省锁的输入）
    const SELF_SRC: &str = include_str!("usage.rs");
    /// 命令注册表所在文件（`generate_handler!` 与 `setup` 闭包）
    const LIB_SRC: &str = include_str!("../lib.rs");

    /// 去掉注释行与全部空白后的「代码骨架」：自省锁只看代码、不看注释——
    /// 否则把文档注释里的签名抄一遍就能骗过签名锁（假绿）。
    /// 顺带把参数表尾逗号 `,)` 归一成 `)`，于是「一行写」与「多行写」等价（fmt 不制造假红）。
    fn code_only(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")")
    }

    /// 本文件的**生产半边**（`#[cfg(test)] mod tests` 之前）。
    /// 自省锁必须只看生产代码，否则断言里写的字面量（`"#[tauri::command]"`、`"DB.lock("`……）
    /// 会被自己数进去 → 反向锁恒红 / 正向锁恒绿（两种假性）。
    fn prod_only(src: &str) -> String {
        let marker = concat!("#[cfg", "(test)]");
        match src.find(marker) {
            Some(i) => src[..i].to_string(),
            None => src.to_string(),
        }
    }

    /// 契约 §3 的 6 条命令签名（**逐字冻结**）。参数名即 Tauri 的 JS 侧键名
    /// （默认 camelCase 映射）：`group_by` → `groupBy`、`patch` → `patch`……改一个字母，
    /// 前端 `invoke` 就会静默 reject（只有 mock 会掩盖它）。
    ///
    /// **A-5 追记**：末两行由 `pub fn` 改为 `pub async fn`（P2-3：同步命令在主线程等
    /// `DB.lock()`）。契约 §3 冻结的是**命令名 + 入参 + 返回形状**（不含 sync/async），
    /// ⇒ 本改动**不违约**，属**计划级冻结签名的显式偏离**（plan:11659-11660），
    /// 已在 FINAL-FIX-report 的「偏离申报」点名；前端键名/参数/返回一字未动。
    const FROZEN_SIGNATURES: [&str; 6] = [
        "pub async fn usage_collect(force: bool) -> Result<UsageCollectResult, UsageError>",
        "pub async fn usage_dashboard(range: UsageRange, group_by: UsageGroupBy) -> Result<UsageDashboard, UsageError>",
        "pub async fn usage_records(range: UsageRange, group_by: UsageGroupBy, filters: UsageFilters) -> Result<UsageRecords, UsageError>",
        "pub async fn usage_export_csv(range: UsageRange, group_by: UsageGroupBy, filters: UsageFilters) -> Result<String, UsageError>",
        "pub async fn usage_get_settings() -> UsageSettings",
        "pub async fn usage_set_settings(patch: UsageSettingsPatch) -> Result<UsageSettings, UsageError>",
    ];

    const COMMAND_NAMES: [&str; 6] = [
        "usage_collect",
        "usage_dashboard",
        "usage_records",
        "usage_export_csv",
        "usage_get_settings",
        "usage_set_settings",
    ];

    /// 契约 §3 的 6 条命令**逐字冻结**：少一条 / 改名 / 改参数名与类型 → 本用例真红。
    /// （Tauri 命令名 = 函数名；参数名 = JS 侧 camelCase 键名。两者都不会有编译错，
    /// 只会在运行时静默 reject —— 所以必须用自省锁钉住。）
    #[test]
    fn six_commands_keep_frozen_signatures() {
        let code = code_only(&prod_only(SELF_SRC));
        // 按**前缀**计数（`#[tauri::command` 而非 `#[tauri::command]`）：这样
        // `#[tauri::command(rename_all = …)]` 仍算「一条命令」，于是下面的 rename_all 反锁
        // 才是**独立承重**的第二道（否则计数断言先红，反锁形同虚设、永远到不了）。
        assert_eq!(
            code.matches("#[tauri::command").count(),
            6,
            "用量域必须恰好 6 条 `#[tauri::command]`（契约 §3）"
        );
        for sig in FROZEN_SIGNATURES {
            let want = code_only(sig);
            assert!(
                code.contains(&want),
                "契约 §3 的命令签名被改动或缺失：`{sig}`\n（参数名 = 前端 invoke 的键名，改它前端会静默 reject）"
            );
        }
        for name in COMMAND_NAMES {
            let pat = format!("fn{name}(");
            assert_eq!(
                code.matches(&pat).count(),
                1,
                "命令 `{name}` 必须恰好声明一次（契约 §3）"
            );
        }
        // **Minor 5**：签名锁挡不住 `#[tauri::command(rename_all = "snake_case")]`——
        // 加上它签名一字不变，而前端 `groupBy` / `filters` / `patch` 会**静默 reject**
        // （mock 仍然全绿，正是最阴的一类）。契约 §1 明文：命令名 snake_case、
        // 前端 invoke 用同名；**参数一律走 Tauri 默认的 camelCase 映射**，故此处禁掉 rename_all。
        assert!(
            !code.contains("rename_all"),
            "6 条命令不得使用 `rename_all`：前端按默认 camelCase 传 groupBy/filters/patch，改了就是静默 reject（契约 §1）"
        );
    }

    /// **A-5 结构锁（P2-3）**：两条设置命令必须是 **`pub async fn` + `spawn_blocking`**
    /// —— 同步命令在**主线程**上执行，会等全局 `DB.lock()`（最坏 0.5s UI 冻结）；
    /// 仓内 `commands/session.rs:10-12` 正是踩过这个坑才把轮询命令改成 async。
    ///
    /// **JS 侧签名与返回形状不变**（本次只动 Rust 侧），契约 §3 冻结的是**命令名 + 入参 + 返回**
    /// —— 不含 sync/async ⇒ 本改动**不违约**，属**计划级冻结签名的显式偏离**（plan:11659-11660），
    /// 已在 FINAL-FIX-report 的「偏离申报」里点名。
    ///
    /// 断言方式（沿用本文件既有自省锁模式）：
    /// ① 两条命令的声明必须是 `pubasyncfn<name>(`（`code_only` 去掉全部空白）；
    /// ② 函数体内必须出现 `spawn_blocking`（阻塞工作已移出主线程）；
    /// ③ **`spawn_blocking` 之前不得出现 `settings::`** —— 否则阻塞调用仍在主线程上跑，
    ///    ② 就成了装样子（"把 await 写在后面"骗不过这一条）；
    /// ④ `settings::load` / `settings::save` 必须都在函数体里（别为了过锁把逻辑删了）。
    ///
    /// **能变红**：把命令改回同步（或把 `settings::load()` 提到 `spawn_blocking` 之外）→ 红。
    #[test]
    fn settings_commands_are_async_and_do_not_block_the_main_thread() {
        let code = code_only(&prod_only(SELF_SRC));
        for name in ["usage_get_settings", "usage_set_settings"] {
            let sig = format!("pubasyncfn{name}(");
            assert!(
                code.contains(&sig),
                "`{name}` 必须是 `pub async fn`（同步命令在主线程等 DB 锁 → UI 冻结，P2-3）"
            );
            // 取函数体：从签名起到下一个 `#[tauri::command]`（或文件末尾）
            let start = code.find(&sig).expect("签名在场（上一条已断言）");
            let rest = &code[start..];
            let end = rest.find("#[tauri::command").unwrap_or(rest.len());
            let body = &rest[..end];
            let Some(off) = body.find("spawn_blocking") else {
                panic!("`{name}` 必须把阻塞工作放进 `spawn_blocking`（否则仍在主线程上跑）");
            };
            assert!(
                !body[..off].contains("settings::"),
                "`{name}` 在 `spawn_blocking` **之前**就碰了 `settings::` —— \
                 阻塞调用仍在主线程上执行（把 load/save 移进闭包）"
            );
            assert!(
                body[off..].contains("settings::"),
                "`{name}` 的 `spawn_blocking` 闭包里必须真的做设置读写（别为了过锁掏空逻辑）"
            );
        }
    }

    /// 三处登记之一：`lib.rs` 的 `generate_handler!` 必须逐条登记 6 条命令**各一次**。
    /// 漏登记 = 前端 `invoke` 静默 reject（mock 会掩盖），不是编译错。
    #[test]
    fn six_commands_are_registered_in_generate_handler_once() {
        let lib = code_only(LIB_SRC);
        let handler_start = lib
            .find("tauri::generate_handler![")
            .expect("lib.rs 必须存在 generate_handler! 块");
        let handler = &lib[handler_start..];
        for name in COMMAND_NAMES {
            let pat = format!("commands::usage::{name},");
            assert_eq!(
                handler.matches(&pat).count(),
                1,
                "`generate_handler!` 必须登记 `{pat}`（契约 §1：三处登记）"
            );
        }
        // 反向：不得把用量命令登记到别处（例如误放进 setup 闭包）
        assert_eq!(
            lib.matches("commands::usage::").count(),
            6,
            "lib.rs 全文件只应有 6 处 `commands::usage::` 引用（逐条登记、无重复）"
        );
    }

    /// GC 3 / AGENTS.md：启动采集是**延迟 3s 的后台线程**（`spawn_initial_collection`），
    /// 且 `lib.rs` 里**不得**出现任何直接采集调用（`collect()` / `run_collection()`）——
    /// 那会把采集塞进启动路径，或（更糟）被误接进 3 秒轮询。
    #[test]
    fn initial_collection_is_the_only_launch_time_entry() {
        let lib = code_only(LIB_SRC);
        assert_eq!(
            lib.matches("crate::services::usage::collect::spawn_initial_collection();")
                .count(),
            1,
            "setup 闭包必须恰好调用一次 spawn_initial_collection()（延迟入口，GC 3）"
        );
        for forbidden in ["collect::collect(", "run_collection(", "collect_with("] {
            assert!(
                !lib.contains(forbidden),
                "lib.rs 不得出现 `{forbidden}`：启动路径只能走 spawn_initial_collection 的延迟线程（GC 3）"
            );
        }
        // 位置锚定：接在既有 remote 恢复之后（计划① Step 4 的落点）
        let restore = lib
            .find("crate::remote::restore_on_launch();")
            .expect("lib.rs setup 里应有 restore_on_launch()");
        let spawn = lib
            .find("crate::services::usage::collect::spawn_initial_collection();")
            .expect("已断言存在");
        assert!(
            spawn > restore,
            "spawn_initial_collection() 必须接在 restore_on_launch() 之后（计划① Step 4 的落点）"
        );
    }

    /// W-26：命令层**不得**自己取 `DB` 锁、也不得调采集内核；采集入口只允许一处。
    /// （取 `DB` 锁后再调采集 = ABBA 死锁；`std::sync::Mutex` 不可重入，整个 app 的 DB 访问挂死。）
    #[test]
    fn command_layer_never_locks_db_or_calls_collection_entries() {
        let code = code_only(&prod_only(SELF_SRC));
        for forbidden in [
            "DB.lock(",
            "cached_result(",
            "run_collection(",
            "collect_with(",
            "load_ledger_rows(",
        ] {
            assert!(
                !code.contains(forbidden),
                "命令层不得出现 `{forbidden}`（W-26：查询层负责持锁，采集入口只能在不持锁时调）"
            );
        }
        assert_eq!(
            code.matches("collect::collect(").count(),
            1,
            "`collect::collect(` 只允许出现在 `usage_collect` 一处（单飞 + 最小间隔的唯一入口）"
        );
    }

    /// 码表**前端那一处**的一致性：`KNOWN_USAGE_CODES`
    /// 必须与 Rust 权威表 `USAGE_CODES` **同序同集合**——漏一个码，前端会把它收敛成
    /// `usage-internal`（静默显示成通用错误，用户看到的 detail 也对不上）。
    /// （码表的登记处 = ① Rust `USAGE_CODES` ② 前端本数组 ③ zh/en locale 模板；
    /// 第 4 处 = `usage_ipc_test.rs` 的镜像副本**已删除**，该集成测试直接 `use` ①。）
    #[test]
    fn usage_codes_match_frontend_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../src/components/usage/usageErrors.ts");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
        let start = text
            .find("KNOWN_USAGE_CODES = [")
            .expect("usageErrors.ts 必须声明 KNOWN_USAGE_CODES 数组");
        let tail = &text[start..];
        let end = tail
            .find("] as const;")
            .expect("KNOWN_USAGE_CODES 数组必须以 `] as const;` 收尾");
        let body = &tail[..end];
        let codes: Vec<String> = body
            .split('"')
            .skip(1)
            .step_by(2)
            .map(|s| s.to_string())
            .collect();
        assert!(
            !codes.is_empty(),
            "KNOWN_USAGE_CODES 必须非空（解析失败就是空表）"
        );
        assert_eq!(
            codes,
            crate::services::usage::error::USAGE_CODES,
            "前端码表 KNOWN_USAGE_CODES 必须与 Rust 的 USAGE_CODES 同序同集合"
        );
    }

    /// **生产半边**的代码骨架（无整行注释、无空白）—— `usage_disabled_stays_reserved_…` 的判据面。
    /// **三支，按路径分派**（2026-10-06 第五轮定稿）：
    ///
    /// * **`services/usage/**`**：走 **`self_lock::scan_face`**（BEGIN/END 哨兵）——那是**全仓唯一**
    ///   为「生产半边」设计的机制，且标记缺失 / 重复 / 顺序反了 / 面为空会**响亮 panic**
    ///   （不静默退化）。它返回的行已经剥掉整行 `//` 注释并去掉行内空白 ⇒ 与另外两支形态一致。
    ///   **这一支是可靠的，不要再动**。
    /// * **`commands/**`（唯一例外）**：剥整行注释 + 去空白后，**切在最后一个 `#[cfg(test)] mod `
    ///   （测试模块）之前**。**为什么必须切**：本锁自己的 pattern 字面量就写在 `commands/usage.rs`
    ///   的测试模块里（`"new(\"usage-disabled\""` 等）⇒ 整文件扫会**自匹配**（锁把自己的断言串
    ///   当成发射点，恒红）。
    /// * **其余文件**：**不切，整文件扫**（2026-10-06 第五轮收紧；第四轮那一版对它们也切在最后一个
    ///   测试模块）。**为什么收紧**：「测试模块边界」是文本启发式，而 `#[cfg(test)] mod` 之外/之后
    ///   仍可能有代码 —— 本仓实测就有 `#[test] fn`（不在 `#[cfg(test)] mod` 里）、
    ///   `#[cfg(all(test, windows))] mod …`（复合 cfg 的测试模块，`last_test_mod` 认不出）、
    ///   以及**真生产代码**（`bin/tuvis-marker.rs` 的 `#[cfg(windows)] mod win`）落在「最后一个测试
    ///   模块之后」的形态（共 5 个文件，见台账第五轮全表）。整文件扫把非 usage / 非 commands 域的
    ///   **全部**代码纳入判据面，代价只是**测试代码也进面**（偏严 = 保守方向：宁可误报，不可漏报）。
    ///   全仓零误报（出现该码字面量的只有 `services/usage/error.rs` 的登记与 `commands/usage.rs`
    ///   自己的 pattern 串）。
    ///
    /// ⚠️ **不要退回「第一个 `#[cfg(test)]` 字面量」**（第三/四轮的盲区）：实测按该口径切会**致盲
    /// 34 个文件**。判据是「**切在首个字面量会不会丢掉生产代码**」，不是「首个字面量后面是不是
    /// `mod`」：
    ///   * `services/usage/collect.rs:70` 是**生产闸**（`#[cfg(test)] fn default_rules_override()`）
    ///     ⇒ 切在那里丢掉 71..532 的全部生产代码；
    ///   * `remote/mod.rs` 的 `#[cfg(test)] pub mod attachment_fixtures;` 是**测试门控的模块声明**
    ///     ⇒ 切在那里丢掉后面全部（同类：`monitor/hooks.rs`、`remote/api.rs`、`inject/question.rs`、
    ///     `linker/mod.rs` …）；
    ///   * 反过来，`services/usage/error.rs:41` **不是**盲区：那一处在**文档注释**里，而
    ///     `code_only` 先剥注释 ⇒ 它的首个**代码**字面量就是真测试模块（旧口径把它多算了 1 个）。
    ///
    /// 34 个文件的全表见台账 `.superpowers/sdd/2026-10-03-usage-surfaces-and-export/progress.md`
    /// 第五轮那一节（此处不复述，免得又一次手写数腐烂）。
    ///
    /// **残余盲区（如实、具体，不说过头）**：
    ///   * `services/usage/**`：**无**（哨兵面把 BEGIN 之前、END 之后的生产代码都收进来）。
    ///   * **非 usage / 非 commands**：**无**（整文件扫，不切）。
    ///   * **`commands/**`（仅此一支）**：切在最后一个测试模块 ⇒ 若某 `commands/**` 文件在**最后
    ///     一个测试模块之后**又写代码，那部分不在面内。**当前无实例**（实测：落在这条线之后的只有
    ///     `adapter/mod.rs` 的复合 cfg 测试模块、`inject/question.rs` 的测试段、
    ///     `remote/content.rs` 的顶层 `#[test] fn`、`services/mcp/jsonc.rs` 与
    ///     `bin/tuvis-marker.rs` 的 `#[cfg(windows)] mod win` —— **没有一个是 `commands/` 下的**）。
    ///     真要收紧，把这一支也改成整文件扫 + 让 pattern 匹配**跳过本文件自己的测试模块**即可；
    ///     本轮**刻意不做**（超出派单，且会改动锁自身的断言串布局）。
    ///
    /// `rel` = 相对 `src-tauri/src` 的路径（`/` 分隔）。
    fn prod_code_only(rel: &str, src: &str) -> String {
        if let Some(name) = rel.strip_prefix("services/usage/") {
            return crate::services::usage::self_lock::scan_face(name, src)
                .into_iter()
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n");
        }
        if rel.starts_with("commands/") {
            let code = code_only(src);
            return match last_test_mod(&code) {
                Some(i) => code[..i].to_string(),
                None => code,
            };
        }
        // 非 usage / 非 commands：**整文件**（不切）——见上「为什么收紧」
        code_only(src)
    }

    /// **最后一个** `#[cfg(test)] mod <名字>` 在**去空白代码串**里的起点（`None` = 该文件没有测试模块）。
    ///
    /// 判据写成「去空白后 `#[cfg(test)]` 与 `mod` 直接相邻」⇒ **同一行写与折行写等价**
    /// （`#[cfg(test)] mod tests {` 与 `#[cfg(test)]` + 换行 + `mod tests {` 都命中）——
    /// 这正是「对 rustfmt 折行免疫」的关键。`pub mod` 去空白后成 `pubmod`，故也要认。
    /// `mod` 之后必须是**模块名首字符**（ASCII 字母 / `_`），免得把 `…mod_foo(…)` 之类噪声算进来。
    fn last_test_mod(code: &str) -> Option<usize> {
        const ATTR: &str = concat!("#[cfg", "(test)]");
        let mut last = None;
        let mut from = 0usize;
        while let Some(i) = code[from..].find(ATTR) {
            let start = from + i;
            let rest = &code[start + ATTR.len()..];
            let rest = rest.strip_prefix("pub").unwrap_or(rest);
            if let Some(name) = rest.strip_prefix("mod") {
                if name.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                    last = Some(start);
                }
            }
            from = start + ATTR.len();
        }
        last
    }

    /// `last_test_mod` 的**表驱动自测**（第四轮新增）：它现在是「非 usage 文件判据面」的**唯一入口**，
    /// 自己必须有用例 —— 尤其「**生产闸 ⇒ `None`**」那一支（`#[cfg(test)] fn …` / `#[cfg(test)] { … }`
    /// 都不是测试模块）与「**多个测试模块 ⇒ 取最后一个**」。
    ///
    /// ⚠️ **同一行形态在当前仓里没有实例**（实测 191 处全是折行形态：属性独占一行、`mod` 在下一行），
    /// 而**去空白后两种写法完全同形**（都成 `#[cfg(test)]mod…`）⇒ 它们**不是两条分支**，
    /// 这里用去空白后的等价输入把「同一行」那一支也钉住（免得读者以为它没被覆盖）。
    #[test]
    fn last_test_mod_recognizes_modules_and_ignores_production_gates() {
        let c = |s: &str| code_only(s);
        // 折行形态（本仓常态）
        assert_eq!(last_test_mod(&c("#[cfg(test)]\nmod tests {\n}\n")), Some(0));
        // 同一行形态（去空白后与上一条同形）
        assert_eq!(last_test_mod(&c("#[cfg(test)] mod tests { }\n")), Some(0));
        // `pub mod`
        assert_eq!(
            last_test_mod(&c("#[cfg(test)]\npub mod tests_of_splitting {\n}\n")),
            Some(0)
        );
        // **生产闸**（`collect.rs:70` 那一类）：函数闸与块闸都不算测试模块
        assert_eq!(
            last_test_mod(&c("fn a() {}\n#[cfg(test)]\nfn gate() -> u8 { 0 }\n")),
            None
        );
        assert_eq!(last_test_mod(&c("#[cfg(test)]\n{ let _ = 1; }\n")), None);
        // 没有属性 / 只有别的属性 ⇒ None
        assert_eq!(last_test_mod("mod tests {}"), None);
        assert_eq!(last_test_mod(&c("#[cfg(not(test))]\nmod tests {}\n")), None);
        // 多个测试模块 ⇒ **切在最后一个**（中间的 `prod_after` 被切掉 = 已声明的残余盲区）
        let two = c("mod a {}\n#[cfg(test)]\nmod tests { }\nfn prod_after() {}\n#[cfg(test)]\nmod tests2 { }\n");
        assert_eq!(two.matches(ATTR_MARKER).count(), 2);
        let cut = last_test_mod(&two).expect("两个测试模块");
        assert!(
            two[cut..].contains("modtests2{"),
            "切点必须是**最后**一个测试模块的起点"
        );
        assert!(
            !two[cut..].contains("fnprod_after("),
            "「最后一个测试模块之前的生产代码」落在面内、之后的落在面外 —— 残余盲区见 `prod_code_only` 文档"
        );
        assert!(two[..cut].contains("fnprod_after("));
    }

    /// `#[cfg(test)]` 的拼接形态（与 `code_only` / `prod_only` 里的同源，避免在断言串里写整串字面量）
    const ATTR_MARKER: &str = concat!("#[cfg", "(test)]");

    /// 递归收集 `src-tauri/src` 下的 `.rs` 源文件（`(相对路径, 全文)`）——`usage-disabled` 预留锁用。
    /// 与 `collect_src_files`（前端那一侧）同法：按**扩展名**发现，不写死文件清单。
    fn collect_rust_files(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut Vec<(String, String)>,
    ) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("无法读取目录 {}: {}", dir.display(), e));
        for entry in entries {
            let path = entry.expect("目录项读取失败").path();
            if path.is_dir() {
                collect_rust_files(&path, root, out);
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
    }

    /// **`usage-disabled` 是预留码**（2026-10-06 第三轮裁决）：它在**三处都登记了**
    /// （Rust `USAGE_CODES` / 前端 `KNOWN_USAGE_CODES` / zh+en `usage.rpc.*`），但**没有任何路径
    /// 会发出它** —— 总开关关闭时后端返回的是**空结果**（`empty_dashboard` / `empty_records`），
    /// 不是错误（见 `query.rs::master_switch_off_yields_empty_state_with_zero_collected_at`）。
    ///
    /// 为什么不去「真发出来」（本轮**刻意**不做）：那会让**冻结契约**把「总开关关闭」的语义从
    /// 「空结果」改成「错误」（契约 §3），并打红上面那条既有用例；而该码当前**没有消费方**。
    /// 将来若要启用，需同步改：① 契约 §3（总开关关闭时的返回语义）；② `master_switch_off_*` 用例；
    /// ③ 前端 `usageErrMsg` 的展示路径（`usageErrors.ts` 的白名单已含该码 ⇒ 不会收敛成
    /// `usage-internal`，locale 键也已就位）；④ 本用例（它会把新增的发射点判红）。
    ///
    /// 判据（**源码级**）：扫 `src-tauri/src/**` 的**生产半边**（口径见 `prod_code_only`：
    /// `services/usage/**` 走 BEGIN/END 哨兵面，其余切在**最后一个** `#[cfg(test)] mod` 之前）：
    /// ① 唯一出现该码字面量的文件必须是 `services/usage/error.rs`（权威表的登记处）；
    /// ② 任何文件里都**不得**以它构造错误 —— 逐个形态点名，**包含 `Self::new(` 这一支**
    ///    （`error.rs` 内部最省事的写法就是 `Self::new("usage-disabled", …)`；漏了它，往
    ///    `error.rs` 生产半边加一个 `pub fn disabled() -> Self` 就全绿）。
    /// **能变红**：在任一生产文件里插一行构造（实测：`collect.rs:365` 与 `error.rs` 的 `Self::new`
    /// 两处变异都真红；旧面下前者**是绿的**，见报告「旧绿 / 新红」）。
    #[test]
    fn usage_disabled_stays_reserved_with_no_production_emitter() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<(String, String)> = Vec::new();
        collect_rust_files(&root, &root, &mut files);
        assert!(
            files.len() > 50,
            "前提：递归扫描必须真的读到后端源码（读到 {} 个文件）",
            files.len()
        );
        // 前提：码表的那个登记处必须在扫描面里（否则下面「唯一登记处」那条会因扫不到而假绿）
        assert!(
            files.iter().any(|(p, _)| p == "services/usage/error.rs"),
            "前提：扫描面必须包含 services/usage/error.rs（实际 {} 个文件）",
            files.len()
        );
        let text_of = |rel: &str| -> &str {
            &files
                .iter()
                .find(|(p, _)| p == rel)
                .unwrap_or_else(|| panic!("前提：扫描面必须包含 {rel}"))
                .1
        };

        // **面锚（第四轮新增；无注入也能抓住「面缩回第一个 `#[cfg(test)]`」这类回归）**：
        // ① 覆盖面：`collect.rs` 的**生产段**（BEGIN 之前 ∪ END 之后；本轮实测 BEGIN 在 `:532`）
        //    必须在面内 —— 含 `#[cfg(test)]` 闸（`:70` 一带）之后的**全部**生产代码；
        // ② 不覆盖面：它的**测试模块**（BEGIN..END 之间）里的名字一个都不许进面。
        let face_collect = prod_code_only(
            "services/usage/collect.rs",
            text_of("services/usage/collect.rs"),
        );
        for want in [
            "fncollect_sources(",
            "pubfnrun_collection(",
            "pubfncollect(",
            "settings::load()",
        ] {
            assert!(
                face_collect.contains(want),
                "前提：判据面必须覆盖 `collect.rs` 的**生产段**（缺 `{want}`）——面是不是又缩回\
                 第一个 `#[cfg(test)]` 字面量了？（`collect.rs:70` 那个是生产闸，不是测试模块）"
            );
        }
        for banned in ["scan_face_size_is_pinned", "modtests{"] {
            assert!(
                !face_collect.contains(banned),
                "前提：判据面不得含 `collect.rs` 的**测试模块**（不该出现 `{banned}`）——\
                 否则断言里写的字面量会被自己数进去"
            );
        }
        // ③ 第二个 GATE 文件（`mod.rs` 的首个 `#[cfg(test)]` 在 :38 的文档注释里）：
        //    面必须一直覆盖到文件尾的 `self_lock` 生产模块。
        let face_mod = prod_code_only("services/usage/mod.rs", text_of("services/usage/mod.rs"));
        assert!(
            face_mod.contains("pub(crate)fnexclusion_is_empty("),
            "前提：判据面必须覆盖 `mod.rs` 的 `self_lock::exclusion_is_empty`（生产段，文件尾）"
        );
        // ④ `commands/**` 那一支（唯一保留切法的）：切在**最后一个**测试模块 ⇒ 也必须保住生产段
        //    （`commands/usage.rs` 的生产命令签名在测试模块之前）。
        let face_cmd = prod_code_only("commands/usage.rs", text_of("commands/usage.rs"));
        assert!(
            face_cmd.contains("pubasyncfnusage_get_settings("),
            "前提：`commands/**` 的判据面必须覆盖到最后一个测试模块之前的生产段"
        );
        // ⑤ **整文件支的面锚（第五轮新增）**：非 usage / 非 commands 的文件一律**整文件**进面
        //    （`face.len() == code_only(全文).len()` ⇒ **没有被切**）。这是「有人把切法改回去」的
        //    机械哨兵 —— 例如又把 `remote/server.rs` 末尾那 450 KB 切掉。
        let mut whole_file_checked = 0usize;
        for (rel, text) in &files {
            if rel.starts_with("commands/") || rel.starts_with("services/usage/") {
                continue;
            }
            let face = prod_code_only(rel, text);
            let full = code_only(text);
            assert_eq!(
                face.len(),
                full.len(),
                "前提：`{rel}` 的判据面必须等于**整文件**（非 usage / 非 commands 支一律不切）——\
                 有人把切法改回去了？（面 {} 字符 / 全文 {} 字符）",
                face.len(),
                full.len()
            );
            whole_file_checked += 1;
        }
        assert!(
            whole_file_checked > 40,
            "前提：整文件支必须真的覆盖到大批文件（实际 {whole_file_checked} 个）"
        );
        // ⑥ 全仓最大文件钉两条**尾部**锚（`remote/server.rs` 末尾 450 KB 曾在面外）：
        //    一条是文件尾的测试辅助函数签名，一条是它最后一条断言的文案片段。它们**刻意**选在
        //    文件末尾 —— 若有人把「整文件」改成任何形式的切法，这两条先红。
        let face_server = prod_code_only("remote/server.rs", text_of("remote/server.rs"));
        for want in ["fnscreen_lines(", "无法自动确认"] {
            assert!(
                face_server.contains(want),
                "前提：`remote/server.rs` 的判据面必须含**文件尾部**的 `{want}`（整文件扫 ⇒ 尾部必在面内）"
            );
        }

        let mut holders: Vec<String> = Vec::new();
        for (rel, text) in &files {
            let code = prod_code_only(rel, text);
            if code.contains("usage-disabled") {
                holders.push(rel.clone());
            }
            for pat in [
                "UsageError::new(\"usage-disabled\"",
                "UsageError{code:\"usage-disabled\"",
                "code:\"usage-disabled\"",
                // **`Self::new(` 形态**（2026-10-06 第四轮补）：`error.rs` 内部最省事的写法，
                // 漏了它就等于给「唯一允许出现该字面量的那个文件」开了后门。
                "Self::new(\"usage-disabled\"",
                // 去空白后所有调用形态都含这一串（`UsageError::new(` / `Self::new(` / `super::…::new(`），
                // 作为兜底（比上面几条更宽 —— 宁可误报，不可漏报）。
                "new(\"usage-disabled\"",
            ] {
                assert!(
                    !code.contains(pat),
                    "{rel} 以 `usage-disabled` 构造了错误（形态 `{pat}`）——该码是**预留码**、当前不得有\
                     发射点（见本用例文档「将来若要启用」的四条同步）"
                );
            }
        }
        assert_eq!(
            holders,
            vec!["services/usage/error.rs".to_string()],
            "`usage-disabled` 只允许作为**登记**出现在 services/usage/error.rs 的 `USAGE_CODES` 表里"
        );
    }

    /// 契约 §2 的前端镜像：**可空性**是这里最容易漂的一处
    /// （`| null` 写成 `?` → 前端把「不可得」当缺键，空态分支与 GC 7 一起失效）。
    #[test]
    fn frontend_type_mirror_keeps_nullability() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/types/usage.ts");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
        let flat = code_only(&text);
        for want in [
            "compare:CompareBlock|null;",
            "recentSession:RecentSessionUsage|null;",
            "userEst:number|null;",
            "title:string|null;",
            "sessions:number|null;",
            "errorCode?:string;",
            "from?:string;",
            "to?:string;",
            "subagentMode?:\"include\"|\"parentsOnly\";",
            "exporttypeUsageAvailabilityMetric=UsageMetric;",
        ] {
            assert!(
                flat.contains(&code_only(want)),
                "src/types/usage.ts 缺少契约字段/可空性：`{want}`"
            );
        }
    }

    /// 递归收集 `src/` 下的 `.ts` / `.tsx` 源文件（`(相对路径, 全文)`）。
    /// 第一版这把锁**瞄错了文件**（只扫 `hooks/useSessions.ts` + `stores/sessionStore.ts`，
    /// 而它们只同步/存状态、**不 invoke**）→ 评审 Important #2：往**真** poll 点插一行
    /// `invoke("usage_collect")` 时三条锁全绿，而那正是 GC 3 唯一会成真的「每 3 秒全量扫描」路径。
    /// 现在改成**按内容发现**：凡含 `refetchInterval` / `POLL_INTERVAL` 的文件一律入清单。
    fn collect_src_files(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut Vec<(String, String)>,
    ) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("无法读取目录 {}: {}", dir.display(), e));
        for entry in entries {
            let path = entry.expect("目录项读取失败").path();
            if path.is_dir() {
                collect_src_files(&path, root, out);
                continue;
            }
            if !matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("ts") | Some("tsx")
            ) {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
    }

    /// W-22 的**静态半边**：3 秒轮询热路径不得引用用量命令。行为式用例在
    /// `tests/usage_polling_budget_test.rs`（真跑扫描后 `collect_calls()` 仍为 0）。
    ///
    /// 清单口径（评审 Important #2 的修法）：**所有**含 `refetchInterval` / `POLL_INTERVAL`
    /// 的前端文件（真 poll 点是 `lib/query/queries/sessions.ts` → `lib/api/session.ts` 的
    /// `invoke("get_all_sessions")`）+ 三个状态侧文件。含**前提断言**：若清单没命中真 poll 点
    /// 或递归扫描读不到源码，本用例自己先红（防「瞄错文件仍然全绿」）。
    #[test]
    fn polling_hot_path_never_references_usage_collect() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
        let mut files: Vec<(String, String)> = Vec::new();
        collect_src_files(&root, &root, &mut files);
        assert!(
            files.len() > 50,
            "前提：递归扫描必须真的读到前端源码（读到 {} 个文件）",
            files.len()
        );

        // 前提 1：内容发现必须命中真 poll 点
        let poll_files: Vec<&(String, String)> = files
            .iter()
            .filter(|(_, t)| t.contains("refetchInterval") || t.contains("POLL_INTERVAL"))
            .collect();
        assert!(
            poll_files
                .iter()
                .any(|(p, _)| p == "lib/query/queries/sessions.ts"),
            "前提：轮询点清单必须包含 src/lib/query/queries/sessions.ts（含 refetchInterval），实际 {:?}",
            poll_files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()
        );
        // 前提 2：必须命中真正 invoke("get_all_sessions") 的文件
        assert!(
            files
                .iter()
                .any(|(p, t)| p == "lib/api/session.ts" && t.contains("get_all_sessions")),
            "前提：必须命中 invoke(\"get_all_sessions\") 的真实调用点 src/lib/api/session.ts"
        );

        // 反向：热路径清单（真 poll 点 + 状态侧三文件）一律不得引用用量命令
        let extra = [
            "lib/api/session.ts",
            "hooks/useSessions.ts",
            "stores/sessionStore.ts",
        ];
        let mut checked: Vec<&str> = Vec::new();
        for (p, text) in files.iter().filter(|(p, t)| {
            t.contains("refetchInterval")
                || t.contains("POLL_INTERVAL")
                || extra.contains(&p.as_str())
        }) {
            for forbidden in [
                "usage_collect",
                "usage_dashboard",
                "usage_records",
                "usage_export_csv",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "{p} 属于 3 秒轮询热路径，不得引用用量命令 `{forbidden}`（GC 3 / AGENTS.md / W-22）"
                );
            }
            checked.push(p);
        }
        assert!(
            checked.contains(&"lib/query/queries/sessions.ts"),
            "前提：真 poll 点必须被反向检查覆盖（实际 {:?}）",
            checked
        );
    }

    /// **裁决 D**：嵌套值对象的 wire 形状在 Rust 侧必须**有锁**（此前只有顶层键被钉住，
    /// 而空账本让 `rows` / `trend` / `cards` 为空、`compare` / `recentSession` 为 null →
    /// `UsageRow` / `UsageMetrics` / `UsageBuckets` / `UsageAvailability` / `TrendPoint` /
    /// `UsageCard` / `CompareBlock` / `RecentSessionUsage` 的键名**从未在真实序列化输出上被断言**）。
    ///
    /// 为什么要紧：给 `UsageRow` 去掉 `rename_all` 或改任一嵌套字段名 → **Rust 全绿 + TS 全绿**，
    /// 而真机 wire 与 mock/TS **静默分叉**（`isSubagent` 假值、`userEst` 由 `null` 变**缺键**
    /// → GC 7 在 UI 上失效）。这里用**构造值**（零 DB、零落库）直接断 serde 输出。
    #[test]
    fn nested_value_objects_keep_camel_case_wire_keys() {
        fn keys(v: &serde_json::Value) -> Vec<String> {
            let mut k: Vec<String> = v
                .as_object()
                .expect("必须是 JSON 对象")
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        }
        fn obj<T: serde::Serialize>(v: T) -> serde_json::Value {
            serde_json::to_value(v).expect("值对象必须可序列化")
        }

        let buckets = UsageBuckets {
            input_fresh: 1,
            cache_read: 2,
            cache_write: 3,
            output: 4,
        };
        assert_eq!(
            keys(&obj(buckets)),
            vec!["cacheRead", "cacheWrite", "inputFresh", "output"]
        );

        let metrics = UsageMetrics {
            request_total: 10,
            cache_hit_rate: 0.5,
            user_est: None, // **不可得 = null 且必须出现**（GC 7）
            requests: 2,
        };
        let mv = obj(metrics);
        assert_eq!(
            keys(&mv),
            vec!["cacheHitRate", "requestTotal", "requests", "userEst"]
        );
        assert!(
            mv.get("userEst").is_some(),
            "userEst 必须出现（缺键 = 前端 undefined）"
        );
        assert!(
            mv["userEst"].is_null(),
            "userEst 不可得必须是 null（GC 7，不得填 0/缺键）"
        );

        let row = UsageRow {
            key: "codex".into(),
            label: "codex".into(),
            buckets,
            metrics,
            source_kind: SourceKind::Inferred,
            // **算不出**该位（日档 / 记录页卡内行）→ `None`：wire 上必须是显式 `null`
            is_subagent: None,
        };
        let rv = obj(row);
        assert_eq!(
            keys(&rv),
            vec![
                "buckets",
                "isSubagent",
                "key",
                "label",
                "metrics",
                "sourceKind"
            ],
            "UsageRow 的键名/大小写逐字冻结（去掉 rename_all 会全变 snake_case）"
        );
        assert_eq!(rv["sourceKind"], "inferred", "三态序列化形态");
        // —— 新口径锁（契约 §2 2026-10-03 用户裁决：`T | null` 必须序列化出显式 null）——
        // 变异敏感性：构造处改回 `Some(false)` 或加上 `skip_serializing_if` → 下面的断言真红。
        assert!(
            rv.get("isSubagent").is_some(),
            "isSubagent 必须出现（缺键 = 前端 undefined）"
        );
        assert!(
            rv["isSubagent"].is_null(),
            "不可得必须序列化成 null（GC 7 同族；不得 skip、不得填 false）"
        );
        // 反向对照（同一用例内）：`Some(true)` 在 wire 上是**真布尔**，不得被可空化牵连成 null
        let row_true = UsageRow {
            key: "codex".into(),
            label: "codex".into(),
            buckets,
            metrics,
            source_kind: SourceKind::Inferred,
            is_subagent: Some(true),
        };
        assert_eq!(
            obj(row_true)["isSubagent"],
            serde_json::json!(true),
            "Some(true) 必须是真布尔 true（`Option` 只在 None 时出 null）"
        );

        let avail = UsageAvailability {
            metric: UsageMetric::UserEst,
            available: false,
            reason: None,
            per_source: None,
        };
        let av = obj(avail);
        assert_eq!(keys(&av), vec!["available", "metric"], "`?:` 缺省不出现");
        assert_eq!(av["metric"], "userEst");
        let avail_full = UsageAvailability {
            metric: UsageMetric::ErrorModel,
            available: true,
            reason: Some("无判据".into()),
            per_source: Some(std::collections::BTreeMap::from([(
                "kimi".to_string(),
                false,
            )])),
        };
        assert_eq!(
            keys(&obj(avail_full)),
            vec!["available", "metric", "perSource", "reason"]
        );

        let trend = TrendPoint {
            key: "2026-10-03T09".into(),
            label: "09:00".into(),
            buckets,
            metrics,
        };
        assert_eq!(
            keys(&obj(trend)),
            vec!["buckets", "key", "label", "metrics"]
        );

        let compare = CompareBlock {
            prev_buckets: buckets,
            prev_metrics: metrics,
        };
        let cv = obj(compare);
        assert_eq!(
            keys(&cv),
            vec!["prevBuckets", "prevMetrics"],
            "totalsBuckets/prevBuckets 同族"
        );

        let recent = RecentSessionUsage {
            source_id: "claude".into(),
            session_id: "s1".into(),
            title: None, // 契约 `title: string | null` → 必须出现
            buckets,
            metrics,
        };
        let rcv = obj(recent);
        assert_eq!(
            keys(&rcv),
            vec!["buckets", "metrics", "sessionId", "sourceId", "title"]
        );
        assert!(rcv["title"].is_null(), "title 可空但必须出现（GC 7）");

        let card = UsageCard {
            tool_id: "claude".into(),
            tool_label: "claude".into(),
            buckets,
            metrics,
            rows: vec![],
        };
        assert_eq!(
            keys(&obj(card)),
            vec!["buckets", "metrics", "rows", "toolId", "toolLabel"]
        );

        // WorkSummary：契约「全字段必须出现；不可得 = null」
        let ws = obj(WorkSummary::default());
        assert_eq!(
            keys(&ws),
            vec![
                "errorModel",
                "errorTool",
                "errorTurn",
                "interrupted",
                "longestTurnPerTool",
                "sessions",
                "toolAvgMs",
                "toolCalls",
                "topTool",
                "topToolMs",
                "turnsPerTool",
            ]
        );
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
            assert!(ws[k].is_null(), "WorkSummary.{k} 不可得必须是 null（GC 7）");
        }
        let ws_full = obj(WorkSummary {
            sessions: Some(3),
            turns_per_tool: std::collections::BTreeMap::from([("claude".to_string(), Some(2))]),
            error_model: Some(1),
            error_turn: Some(1),
            error_tool: Some(1),
            interrupted: Some(1),
            tool_calls: Some(1),
            tool_avg_ms: Some(1),
            top_tool: Some(TopTool {
                name: "Bash".into(),
                count: 1,
            }),
            top_tool_ms: Some(TopToolMs {
                name: "Bash".into(),
                ms: 1,
            }),
            longest_turn_per_tool: std::collections::BTreeMap::from([(
                "claude".to_string(),
                Some(LongestTurn { p50: 1, max: 2 }),
            )]),
        });
        assert_eq!(keys(&ws_full["topTool"]), vec!["count", "name"]);
        assert_eq!(keys(&ws_full["topToolMs"]), vec!["ms", "name"]);
        assert_eq!(
            keys(&ws_full["longestTurnPerTool"]["claude"]),
            vec!["max", "p50"]
        );

        // 顶层容器里的 camelCase 容器键（dashboard 的 totalsBuckets / records 的 cards）
        let d = UsageDashboard {
            range: UsageRange {
                preset: UsageRangePreset::Today,
                from: None,
                to: None,
            },
            group_by: UsageGroupBy::Tool,
            rows: vec![],
            totals: metrics,
            totals_buckets: buckets,
            hero: 0,
            trend: vec![],
            compare: None,
            recent_session: None,
            work_summary: WorkSummary::default(),
            availability: vec![],
            collected_at: 0,
        };
        let dv = obj(d);
        assert_eq!(
            keys(&dv["totalsBuckets"]),
            vec!["cacheRead", "cacheWrite", "inputFresh", "output"]
        );
        assert!(
            dv["compare"].is_null() && dv["recentSession"].is_null(),
            "契约 `| null` 必须出现"
        );
        let rec = UsageRecords {
            range: UsageRange {
                preset: UsageRangePreset::Today,
                from: None,
                to: None,
            },
            group_by: UsageGroupBy::Tool,
            cards: vec![],
            availability: vec![],
            collected_at: 0,
        };
        assert!(keys(&obj(rec)).contains(&"cards".to_string()));
    }
}
