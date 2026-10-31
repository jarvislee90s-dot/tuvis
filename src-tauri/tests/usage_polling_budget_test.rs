//! W-22：把 `services/usage/collect.rs::collect_calls()` 的 doc 自述
//! （「**「采集不进入 3 秒轮询」的可执行断言**：会话扫描后它必须仍为 0」）
//! 变成**真断言**——在此之前 `collect_calls()` 全仓零消费者（Task 10 的用例只用局部原子量）。
//!
//! 为什么单独一个测试二进制：`COLLECT_CALLS` 是**进程级**计数，`reset_state_for_test` 只在
//! `#[cfg(test)]`（lib 单测）构建里存在，集成构建拿不到。所以「字面上的 0」只有在
//! **一个从不触发采集的二进制**里才成立：本文件不得出现任何采集入口（下半部分的自省用例锁住）。
//!
//! 热路径 = 前端轮询 `invoke("get_all_sessions")` → `commands::session::get_all_sessions`
//! → `scan_bus::current_or_scan`（生产读共享快照）。本用例直接调兼容入口
//! `adapter::get_all_sessions`（已委托 scan_bus；冷启动无快照时同步首扫）——
//! `commands::session::get_all_sessions` 需要 `tauri::AppHandle`，测试里造不出来。

mod support;

use multi_agents_manager_lib::adapter;
use multi_agents_manager_lib::services::usage::collect;

/// 去掉注释行后的「代码骨架」（自省锁只看代码、不看注释——注释里提到采集入口
/// 不影响第一条用例的「0 前提」，数进去只会制造假红）。
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn session_scan_keeps_collect_calls_at_zero() {
    support::setup();
    // 与 usage_ipc_test 同款的额外隔离：这两个变量会把采集/扫描的根指到真实家目录
    // （本机 `DSH_HOME=~/.dsh` 实测存在），本用例要的是「临时 home 的一次空扫描」
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("KIMI_CODE_HOME");

    // 前提断言：本进程此前**从未**采集过。若写成「扫描前后不变」，一旦前面有别的调用把
    // 计数推到 1，断言就退化成恒真（假绿）；这里要的是**字面上的 0**。
    assert_eq!(
        collect::collect_calls(),
        0,
        "前提：本测试二进制不得触发过任何采集（否则本用例的「0」判据自毁）"
    );

    let resp = adapter::get_all_sessions();
    // 前提断言 2：会话扫描**真的跑完并产出了合法响应**（否则「计数没涨」可能只是没走到扫描）
    assert_eq!(
        resp.total_count,
        resp.sessions.len(),
        "会话扫描必须产出结构合法的响应（totalCount 与 sessions 一致）"
    );

    assert_eq!(
        collect::collect_calls(),
        0,
        "3 秒轮询热路径（get_all_sessions）**不得**触发用量采集（GC 3 / AGENTS.md 扫描预算 / W-22）"
    );
}

/// 自省锁：本文件不得出现采集入口——否则第一条用例的「0 前提」会被本文件自己毁掉。
/// 字符串用 `concat!` 拼出，避免自省扫描把自己的字面量数进去（否则恒红）。
#[test]
fn this_file_never_calls_collection_entries() {
    let src = code_only(include_str!("usage_polling_budget_test.rs"));
    for forbidden in [
        concat!("usage", "_collect"),
        concat!("collect::", "collect("),
        concat!("collect", "_with("),
        concat!("run", "_collection("),
        concat!("spawn_initial", "_collection"),
    ] {
        assert!(
            !src.contains(forbidden),
            "本文件不得出现 `{forbidden}`：它会破坏「字面上的 0」前提（W-22）"
        );
    }
}

/// W-22 的**静态半边之 2**：3 秒轮询的**命令包装**（`commands::session::get_all_sessions`）
/// 自身也不得引用用量域。行为式用例打不到它——它要 `tauri::AppHandle`，集成测试里造不出来
/// （`tauri::test::mock_app` 需要未启用的 `test` feature），所以行为式用例只能打到它内部
/// 调用的 `adapter::get_all_sessions`。这条静态锁补住包装层（变异实测：往包装里插一行
/// `usage::collect::collect(true)` → 本用例真红）。
/// 字面量用 `concat!` 拼出：本文件另有一条自省锁禁止自己出现采集入口。
#[test]
fn polling_command_wrapper_never_references_usage_domain() {
    let src = code_only(include_str!("../src/commands/session.rs"));
    for forbidden in [
        concat!("usage", "::"),
        concat!("usage", "_collect"),
        concat!("collect::", "collect("),
    ] {
        assert!(
            !src.contains(forbidden),
            "3 秒轮询命令 `commands/session.rs` 不得出现 `{forbidden}`（GC 3 / AGENTS.md / W-22）"
        );
    }
}
