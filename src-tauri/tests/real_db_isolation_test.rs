//! **Task 16 的源码级隔离锁**（真机数据隔离守卫的「结构半边」）。
//!
//! ## 本文件承担什么、不承担什么（诚实分工）
//!
//! 锁表四条里，**只有第 4 条「事故重演」是运行级证据**（`scripts/check-real-db-untouched.sh`
//! 的 before → 命令 → after 取样，见 `task-16-report.md` 的 Step 5 记录）。另外三条的分工是：
//!
//! | 锁 | 落点 | 为什么不能在别处 |
//! |---|---|---|
//! | ① 路径隔离 | `src/database/connection.rs` 的 `#[cfg(test)] mod tests`（**lib 单测**） | `#[cfg(test)]` 只在 lib 单测构建里为真；集成测试链接的是**非 test** 构建 ⇒ 在那边断言 `cfg(test)` 行为是**恒真假绿** |
//! | ② 采集拒绝 | `src/services/usage/collect.rs` 的 `mod tests`（**lib 单测**） | 同上；且被注入的 `collect(false)` 只有在 lib 单测构建里才会被守卫拦住 |
//! | ③ 带外自检 | `scripts/check-real-db-untouched.sh --self-test`（4 腿：恒绿/改内容/改行数/覆盖开关） | 一个 `#[test]` 只能观察**自己那段窗口**，发现不了「**别的**测试碰了真机库」 |
//! | ④ 事故重演 | `task-16-report.md` 的 Step 5 原始输出（脚本 + 注入 `kimi.rs`） | 需要**真跑** lib 测试，不可能固化成一条常驻用例 |
//!
//! 本文件（集成 target）补的是**源码级结构断言**：守卫**存在**、**在 `cfg(test)` 之下**、
//! **留在 `debug_assertions` 门控之内**、并且**先于**第一个全局 DB 入口。这三条恰恰是
//! 「行为锁」看不见的：守卫可以被删掉而 lib 用例因为并行/时序而照样全绿。
//!
//! **本文件不碰任何数据库**：它只读 `src/**/*.rs` 的文本。因此在 `cargo test` 全量里
//! 天然安全，也不需要 `support::setup()`。

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_src(rel: &str) -> String {
    let p = manifest_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 {}：{e}", p.display()))
}

/// 与自省锁同款的「去行内空白 + 剥整行 `//` 注释」归一化：让断言对 rustfmt 的**折行**
/// 免疫，但不放过任何**语义**改动（例如把 `#[cfg(test)]` 挪走、把 `panic!` 换成静默返回）。
fn squeeze(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(|l| l.chars().filter(|c| !c.is_whitespace()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn at(hay: &str, needle: &str, what: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("{what}：源码里找不到 `{needle}`"))
}

// =============================================================================
// 锁 ①（源码半边）：`app_data_home()` 的 `cfg(test)` 兜底分支
// 行为断言在 `src/database/connection.rs::tests`（lib 单测）里。
// =============================================================================

/// `src/database/connection.rs`：
/// ① `cfg(test)` 兜底分支**存在**且落到 `std::env::temp_dir()/tuvis-test-<pid>`；
/// ② 它在 `cfg!(debug_assertions)` 门控**之内**（否则 release 会被重定向 ⇒ 真机 app 的
///    数据目录被劫持）；
/// ③ 真实 home 兜底 `dirs::home_dir()` 仍**在门控之外**（release 的出路）。
#[test]
fn connection_cfg_test_home_fallback_is_inside_the_debug_gate() {
    let src = squeeze(&read_src("src/database/connection.rs"));
    let fn_at = at(&src, "fnapp_data_home_with(", "app_data_home_with 的定义");
    let rest = &src[fn_at..];

    let gate = at(rest, "ifcfg!(debug_assertions){", "debug_assertions 门控");
    let real_home = at(
        rest,
        "dirs::home_dir().unwrap_or_default()",
        "真实 home 兜底",
    );
    assert!(
        gate < real_home,
        "`dirs::home_dir()` 兜底必须在 `cfg!(debug_assertions)` 门控**之后**（release 的出路）"
    );

    // 归一化后的逐字形态：`#[cfg(test)] return std::env::temp_dir().join(format!("tuvis-test-{}", std::process::id()));`
    // （2026-10-10 品牌更名 cf69cf35：mam-test-{} → tuvis-test-{}，本针同步）
    let wanted = concat!(
        "#[cfg(test)]\n",
        "returnstd::env::temp_dir().join(format!(\"tuvis-test-{}\",std::process::id()));"
    );
    let branch = at(rest, wanted, "cfg(test) temp 目录兜底分支");
    assert!(
        gate < branch && branch < real_home,
        "**路径隔离锁（源码半边）**：`cfg(test)` 兜底分支必须夹在 `cfg!(debug_assertions){{` \
         与真实 home 兜底**之间**——放门控外 = release 也被重定向；放兜底之后 = 死代码。\
         实际位置 gate={gate} / branch={branch} / real_home={real_home}"
    );
}

// =============================================================================
// 锁 ②（源码半边）：`collect()` / `run_collection()` 生产函数体内的采集拒绝闸
// 行为断言在 `src/services/usage/collect.rs::tests`
// （`collection_is_refused_without_an_explicit_fixture_root`）。
// =============================================================================

/// 取一个**生产函数**里「从函数签名到第一个全局 DB 入口」的那一段（函数头）。
/// 为什么用「到第一个 DB 入口」而不是配对花括号：本文件刻意避开朴素花括号匹配
/// （它会字符串/注释里的括号骗到）；而「拒绝闸必须**先于**第一个 DB 入口」正是要断的性质。
fn function_head<'a>(src: &'a str, signature: &str, first_db_entry: &str) -> &'a str {
    let start = at(src, signature, "生产函数签名");
    let entry = at(
        &src[start..],
        first_db_entry,
        &format!("`{first_db_entry}`（`{signature}` 的第一个全局 DB 入口）"),
    ) + start;
    &src[start..entry]
}

#[test]
fn collect_refuses_before_touching_the_flight_lock_or_db() {
    let src = squeeze(&read_src("src/services/usage/collect.rs"));
    let head = function_head(
        &src,
        "pubfncollect(force:bool)->UsageCollectResult{",
        "collect_with(force,",
    );

    for needle in ["#[cfg(test)]", "tests::fixture_home()", "panic!("] {
        assert!(
            head.contains(needle),
            "**采集拒绝锁（源码半边，`collect()`）**：函数头里必须有 `{needle}` —— \
             `collect()` 正是 2026-10-03 被注入生产文件的那一句，它比 `run_collection()` \
             更早经 `collect_with` → `settings::load()` 取全局 `DB` 锁，拒绝必须发生在那之前。\
             实际函数头：\n{head}"
        );
    }
}

#[test]
fn run_collection_refuses_before_the_first_global_db_entry() {
    let src = squeeze(&read_src("src/services/usage/collect.rs"));
    let head = function_head(
        &src,
        "pubfnrun_collection(now_ms:i64)->UsageCollectResult{",
        "super::settings::load()",
    );

    for needle in ["#[cfg(test)]", "tests::fixture_home()", "panic!("] {
        assert!(
            head.contains(needle),
            "**采集拒绝锁（源码半边，`run_collection()`）**：函数头里必须有 `{needle}` —— \
             它是真正用 `dirs::home_dir()` 解析**真实**扫描根、并经 \
             `load_cursors` / `apply_delta` 取进程级 `DB` 的那个函数。实际函数头：\n{head}"
        );
    }
    // 顺序：拒绝闸必须**先于**第一个全局 DB 入口（`settings::load()`）——否则「响亮失败」
    // 之前已经先碰过真库了。函数头的切片**止于**该入口，故「闸在头内」等价于「闸在入口之前」；
    // 这里再钉一条：头内不得出现真实扫描根的**解析绑定**（拒绝文案里可以**提到**它，那正是
    // 「为什么拒绝」的说明）。生产半边的那一行必须在 `cfg(not(test))` 之下、且在此头之外。
    assert!(
        !head.contains("lethome=dirs::home_dir()"),
        "**采集拒绝锁**：真实扫描根解析绑定（`let home = dirs::home_dir()`）不得出现在拒绝闸之前"
    );

    // 拒绝必须是**响亮**的：函数头里不得出现「静默兜底」形态
    for silent in [
        "unwrap_or_default()",
        "unwrap_or_else(",
        "Ok(UsageCollectResult",
    ] {
        assert!(
            !head.contains(silent),
            "**采集拒绝锁**：拒绝闸不得退化成静默兜底 `{silent}`（判据：不是静默 0）"
        );
    }
}

/// **判据的「条件」也要钉**（变异实验实测的缺口）：上面的三针只钉「`#[cfg(test)]` 块里
/// 有 `fixture_home()` 和 `panic!`」。实测把条件**反相**（`is_none()` → `is_some()`）后，
/// 三针**全绿** —— 块还在、针还在，只有**判据**掉了。故这里把两处拒绝闸的**逐字形态**
/// 钉住（与自省锁同款「声明 = 实际」纪律）：归一化后必须**恰好两处**是这个形状。
#[test]
fn both_refusal_gates_are_pinned_verbatim() {
    let src = squeeze(&read_src("src/services/usage/collect.rs"));
    let pinned = concat!(
        "#[cfg(test)]\n",
        "{\n",
        "iftests::fixture_home().is_none(){\n",
        "panic!("
    );
    assert_eq!(
        src.matches(pinned).count(),
        2,
        "**采集拒绝锁（判据钉子）**：`collect()` 与 `run_collection()` 两处拒绝闸必须**逐字**\
         是 `#[cfg(test)] {{ if tests::fixture_home().is_none() {{ panic!( … ) }} }}` 这个形状，\
         实测 {} 处。条件反相（`is_some()`）、或把 `panic!` 换成静默返回，都在这里打红——\
         只钉「块里有针」是钉不住的（变异实验实测：反相后三条针全绿）",
        src.matches(pinned).count()
    );
    // 反向钉：真实扫描根的解析绑定必须仍在 `cfg(not(test))` 之下（生产一半不得被 test 换掉）
    assert!(
        src.contains("lethome=dirs::home_dir().unwrap_or_default();")
            && src.contains("#[cfg(not(test))]"),
        "**采集拒绝锁**：`run_collection()` 的 `cfg(not(test))` 生产扫描根必须逐字在场"
    );
}

// =============================================================================
// 锁 ⑤（源码半边）：集成测试的采集读源缝 `TUVIS_USAGE_FIXTURE_ROOT`
// 行为半边在 `tests/usage_ipc_test.rs` 与 `tests/usage_two_round_incremental_test.rs`
// （集成测试实跑）。2026-10-13 随缝落成，评审 P2-3 要求与 `TUVIS_HOME` 的门控针同款。
// =============================================================================

/// `run_collection()` 的读源根覆盖必须**逐字**保持三分支形态：
/// ② 集成测试臂读 `TUVIS_USAGE_FIXTURE_ROOT`，且**必须**钉在
///    `cfg(all(not(test), debug_assertions))]` 门内——release 读它 = 生产扫描根可被
///    环境变量劫持（与 `connection.rs::app_data_home` 的 `TUVIS_HOME` 同一道门的道理）；
/// ③ release 出路必须是 `cfg(all(not(test), not(debug_assertions)))]` 下的
///    `dirs::home_dir().unwrap_or_default()`（门控外的真实 home 兜底不得被删）。
/// 变异靶：把 env 读取挪出门外、删掉 release 出路臂，都在这里打红。
/// （① lib 单测臂由上面的拒绝闸判据钉与 `both_refusal_gates_are_pinned_verbatim` 覆盖。）
#[test]
fn usage_fixture_root_env_is_inside_the_debug_gate() {
    let src = squeeze(&read_src("src/services/usage/collect.rs"));
    let env_arm = concat!(
        "#[cfg(all(not(test),debug_assertions))]\n",
        "lethome=matchstd::env::var_os(\"TUVIS_USAGE_FIXTURE_ROOT\"){"
    );
    let release_arm = concat!(
        "#[cfg(all(not(test),not(debug_assertions)))]\n",
        "lethome=dirs::home_dir().unwrap_or_default();"
    );
    let env_at = at(&src, env_arm, "集成测试读源根缝（debug 门内）");
    let rel_at = at(&src, release_arm, "release 真实 home 出路臂");
    assert!(
        env_at < rel_at,
        "读源根三分支必须保持 test → debug 门 → release 出路的顺序（实测 env@{env_at} / release@{rel_at}）"
    );
}

// =============================================================================
// 哨兵纪律（Step 3 的落点纪律）的源码级复验
// 权威判据是 `services/usage/mod.rs::self_lock` 的 `scan_face` / `assert_exclude_shape`
// （由 `collect::tests` 的用例驱动）。这里只多钉一条与**本任务**直接相关的事实：
// 新增的守卫在**生产半边**，新增的测试辅助项在**排除区**。
// =============================================================================

#[test]
fn sentinel_pair_is_intact_and_the_guard_lives_outside_the_exclusion_zone() {
    let raw = read_src("src/services/usage/collect.rs");
    let begin = concat!(
        "// ==== usage 自省锁：",
        "以下为排除区（测试代码），勿删勿复制 ===="
    );
    let end = concat!("// ==== usage 自省锁：", "排除区结束，勿删勿复制 ====");

    assert_eq!(
        raw.lines().filter(|l| l.trim() == begin).count(),
        1,
        "BEGIN 标记必须**恰好 1 行**（0 = 被删、≥2 = 被复制）"
    );
    assert_eq!(
        raw.lines().filter(|l| l.trim() == end).count(),
        1,
        "END 标记必须**恰好 1 行**（0 = 被删、≥2 = 被复制）"
    );
    assert_eq!(
        raw.lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|l| l.trim()),
        Some(end),
        "END 必须是文件的最后一个非空行（本文件带测试模块 ⇒ END 是最后一行）"
    );

    let b = at(&raw, begin, "BEGIN");
    let e = at(&raw, end, "END");
    assert!(b < e, "BEGIN 必须在 END 之前");

    // 拒绝闸（生产代码）必须在 BEGIN **之前**；fixture 根存取口（测试辅助项）在 BEGIN/END **之间**
    let prod = &raw[..b];
    let excl = &raw[b..e];
    assert!(
        prod.contains("tests::fixture_home().is_none()"),
        "**哨兵纪律**：采集拒绝闸必须在 BEGIN **之前**（那是生产半边）"
    );
    assert!(
        excl.contains("fn fixture_home()"),
        "**哨兵纪律**：fixture 根存取口必须落在 BEGIN 与 END **之间**（测试辅助项不得落进生产半边判据面）"
    );
    assert!(
        !excl.contains("pub fn collect(") && !excl.contains("pub fn run_collection("),
        "**哨兵纪律**：生产函数体不得出现在 BEGIN 与 END **之间**（`self_lock` 的第三条致盲路）"
    );
}
