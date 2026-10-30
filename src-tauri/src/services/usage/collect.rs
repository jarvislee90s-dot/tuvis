//! 采集调度（说明书 §7.2 / 契约 §3）：
//! * **按需 + 稀疏兜底**：只有 `usage_collect` 会扫描，它有单飞 + 最小间隔保护；`force=false`
//!   且距上次小于 `collectIntervalMin` → 直接返回上次结果，**不扫描**（GC 3）；
//! * **单飞**：并发调用串行化在 `COLLECT_FLIGHT` 上（`Lazy<(Mutex<…>, Condvar)>`，与
//!   `adapter::SCAN_FLIGHT` 同款的 `once_cell::sync::Lazy` + `std::sync` 原语，不另造并发原语），
//!   后来者拿到的是同一次结果；**扫描期间飞行锁必须已释放**（见下面「锁序铁律」）；
//! * **一次扫描产出全部指标**（GC 4）：文件读取只经 `CollectContext::read_incremental`，
//!   同一文件读第二遍会被记成 `repeat_reads`（采集器测试断言它为 0）；
//! * **单源故障隔离**（W-06）：某源 `collect` / `apply_delta` 返回 `Err` 只算「本源的
//!   `ok=false` + `errorCode`」，**不得** `unwrap` / `?` 上抛 / 卡死循环——否则确定性毒行
//!   会把**其余六个源**连带采不到数；
//! * 采集在**后台线程**执行（调用方 `spawn_blocking` / `spawn_initial_collection`），
//!   绝不进 3 秒轮询（GC 3/5）。
//!
//! ## ⚠️ 锁序铁律（评审 Critical A：ABBA 死锁，**不得回退**）
//! **`COLLECT_FLIGHT` 的任何临界区内不得获取 `DB` 锁**（也不得执行整轮扫描——扫描会走
//! `dao::usage::load_cursors` / `ledger::apply_delta` / `ledger::purge_expired`，全都取 `DB`）。
//! 违反它就是 `COLLECT_FLIGHT → DB` 这条序；而查询层（Task 19 的 `usage_records`）是
//! `DB → cached_result()` 的反序 → ABBA：`std::sync::Mutex` 无超时、`DB` 又是**全库唯一连接锁**，
//! 两个方向一相遇就是**整个 app 的 DB 访问一起挂死**（启动采集扫 codex 是分钟级窗口，
//! 前端此时打开用量页即可触发）。
//! 所以本模块把三件事**分到三处、各有独立锁**：
//! ① 飞行判定与单飞（`COLLECT_FLIGHT`）；② 结果发布（`LAST_RESULT`，读者只短暂持锁 clone）；
//! ③ 轮次计数（`COLLECT_CALLS` 原子量，取它不需要任何锁）。
//! `cached_result()` / `collect_calls()` **永不触碰飞行锁** → 可以在持 `DB` 锁的查询路径里
//! 安全调用。另外 `settings::load()`（生产路径会经 `get_setting` 取 `DB` 锁）必须在
//! **取飞行锁之前**调用。
//! 锁：`cached_result_never_blocks_behind_an_in_flight_scan`（真跑复现整条环路）。
//!
//! ## ✅ 时区：落库键与窗口键**一律宿主本地**（W-14 已由 Q-4 裁决消除）
//! 本域**落库**的 `hour_key` / `day_key` 由各采集器一律走 `hour_key_of_host`（宿主本地）产出，
//! 与**查询侧** `range::resolve_range` 的窗口键**同源**（spec §P6，2026-10-05 用户裁决）。
//! **实现纪律**：各源自带的时区（七源里只有 codex 读 `turn_context.timezone`）**可读入备查、
//! 不参与任何键计算**——「读得到却不用」是**有意**的，后来者**不得**把它当 bug「改回去」
//! （改回命名时区即重造两套时钟）。封棺钉：`collect::tests::no_source_timezone_is_fed_into_any_key_function`
//! （源码级自省：生产半边不得出现命名时区机器）+ `collectors::codex::tests::detail_hour_key_is_host_local_even_when_source_timezone_is_present`。
//! **W-14 的旧失败模式作废**：原先「落库用源自带时区、窗口用宿主本地」= 两套时钟，δ ≠ 0 时
//! 边界行落到窗外 → **静默少算**（跨整点 ≤1h、跨日 ≤1d）。统一到宿主本地后该分叉**结构上
//! 不可能发生**，也**不再需要**「把 tz 持久化（改契约）」那类修法（详见 KNOWN-PLAN-DEFECTS「W-14」）。
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use once_cell::sync::Lazy;

use super::caps::{caps_of, SourceCaps};
use super::delta::{CursorDelta, SourceDelta};
use super::error::UsageError;
use super::ledger;
use super::model::{UsageCollectResult, UsageSourceId, UsageSourceStatus};
use super::provider::ProviderRule;

/// 单源采集的读取/字节统计（GC 4「一遍扫描产出全部指标」的可执行断言载体）。
/// `bytes_read` 口径：只累计 payload 读，**不含**验证尾指纹时读的那 ≤64 字节
/// （W-18：拿它做预算断言时须知这笔账不完整）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectStats {
    pub reads: usize,
    pub repeat_reads: usize,
    pub bytes_read: u64,
}

/// lib 单测构建的默认规则注入 = **空规则**（内存注入，零 DB、零全局 override）。
/// 理由（§3.2.3 FIX-6）：采集器的 `collect()` 需要供应商规则；若在 lib 单测里经
/// `settings::load()` 取，① 会读并在首次创建开发机真实 `~/.tuvis/tuvis.db`；
/// ② 与 Task 10 那几个改全局 override 的用例在同一个测试二进制里并行跑时互相踩
/// （谁都不知道读到谁的值）。需要规则的单测用 `CollectContext::with_provider_rules(...)` 显式给。
#[cfg(test)]
fn default_rules_override() -> Option<Vec<ProviderRule>> {
    Some(Vec::new())
}

/// 生产/集成测试构建的默认 = `None`（走 `settings::load()`）：`collect_source` 的正常路径；
/// 集成测试先 `support::setup()` 重定向 `HOME`/`TUVIS_HOME`，读到的也是临时库（GC 19）。
#[cfg(not(test))]
fn default_rules_override() -> Option<Vec<ProviderRule>> {
    None
}

/// 文件型源建 `SessionFileScan` 必须用的 **namespace**（GC 5 / AGENTS.md 扫描预算契约）：
/// `usage-<UsageSourceId::db_id()>`。
/// **不得复用 `monitor/` 既有的 ns**（claude-cwd / claude-digest / dsh-log / workbuddy-tail /
/// kimi-wire / codex-digest）：同 ns 混存不同产物类型时 `FileParseCache::get_or_parse` 的
/// `Arc::downcast` 会失配 → 该条目**每轮重新解析**（缓存静默失效，正是 L2 要省掉的那笔开销）。
/// `const fn` 形态是为了能写
/// `const SCAN: SessionFileScan = SessionFileScan::new(scan_namespace(sid));`。
pub const fn scan_namespace(source: UsageSourceId) -> &'static str {
    match source {
        UsageSourceId::Claude => "usage-claude",
        UsageSourceId::Codex => "usage-codex",
        UsageSourceId::Kimi => "usage-kimi",
        UsageSourceId::OpenCode => "usage-opencode",
        UsageSourceId::WorkBuddy => "usage-workbuddy",
        UsageSourceId::ZCode => "usage-zcode",
        UsageSourceId::Dsh => "usage-dsh",
    }
}

/// 采集上下文：文件读取的唯一入口（同时统计"一遍扫描"纪律）
pub struct CollectContext<'a> {
    pub home: &'a Path,
    pub now_ms: i64,
    /// 本源的既有游标（键 = **文件标识**：文件型源为 `file_key_of(root, path)` 产出的
    /// 路径派生唯一键（B5），SQLite 型源为会话 id 或表名水位键）
    pub cursors: &'a HashMap<String, CursorDelta>,
    /// **供应商规则的显式注入**（`None` = 走 `settings::load()` 的生产路径）。
    /// 单测构建的默认值见 `default_rules_override()`——**lib 单测永远不读全局设置库**。
    rules_override: Option<Vec<ProviderRule>>,
    reads: Cell<usize>,
    repeat: Cell<usize>,
    bytes: Cell<u64>,
    seen: RefCell<HashSet<PathBuf>>,
}

impl<'a> CollectContext<'a> {
    pub fn new(home: &'a Path, now_ms: i64, cursors: &'a HashMap<String, CursorDelta>) -> Self {
        Self {
            home,
            now_ms,
            cursors,
            rules_override: default_rules_override(),
            reads: Cell::new(0),
            repeat: Cell::new(0),
            bytes: Cell::new(0),
            seen: RefCell::new(HashSet::new()),
        }
    }

    pub fn cursor_of(&self, file_key: &str) -> Option<&CursorDelta> {
        self.cursors.get(file_key)
    }

    /// **供应商规则的唯一读取口**：显式注入优先，否则读全局设置（生产路径）。
    /// 采集器**不得**自己调 `settings::load()`——那会让 lib 单测去读/建开发机真实
    /// `~/.tuvis/tuvis.db`，并与同二进制的其它用例抢全局 override（§3.2.3 FIX-6）。
    pub fn provider_rules(&self) -> Vec<ProviderRule> {
        match &self.rules_override {
            Some(r) => r.clone(),
            None => {
                super::provider::parse_provider_rules(&super::settings::load().provider_map_rules)
            }
        }
    }

    /// 显式注入供应商规则（单测用；生产路径由 `settings::load()` 提供，见 `provider_rules()`）
    pub fn with_provider_rules(mut self, rules: Vec<ProviderRule>) -> Self {
        self.rules_override = Some(rules);
        self
    }

    /// 读文件增量（唯一入口）。续读状态在 `cursor_of(file_key).state_json` 里，
    /// 采集器处理完本轮后把**新状态**写回它产出的 `CursorDelta.state_json`。
    pub fn read_incremental(
        &self,
        path: &Path,
        file_key: &str,
    ) -> std::io::Result<super::stream::IncrementalRead> {
        self.reads.set(self.reads.get() + 1);
        if !self.seen.borrow_mut().insert(path.to_path_buf()) {
            self.repeat.set(self.repeat.get() + 1);
            log::warn!(
                "usage: 同一文件被重复读取（违反「一遍扫描」纪律）: {}",
                path.display()
            );
        }
        let prev = self.cursors.get(file_key).cloned();
        let r = super::stream::read_incremental(path, file_key, prev.as_ref())?;
        self.bytes.set(self.bytes.get() + r.bytes_read);
        // **W-17**：游标表的键（`file_key`）与行内 `session_id` 必须恒等。四条第 4 参路径里
        // 只有 L2 快速路返回 `next: p.clone()`（行键跟着 `prev.session_id` 走，不看入参），
        // 于是库行一旦键/值分叉就会被**原样**带下去（行再也写不回自己那把键下）。
        // 这条断言把「隐式保证」变成**显式**的——它是全域唯一依赖该保证的位置。
        // 生产上不可能触发：`load_cursors_conn` 就是以 `session_id` 列作 map 键读回来的。
        assert_eq!(
            r.next.session_id, file_key,
            "游标行的 session_id 必须等于游标表的键（W-17：快速路原样透传 prev.session_id）"
        );
        Ok(r)
    }

    pub fn stats(&self) -> CollectStats {
        CollectStats {
            reads: self.reads.get(),
            repeat_reads: self.repeat.get(),
            bytes_read: self.bytes.get(),
        }
    }
}

/// 采集器契约。实现者**只上报原始字段与计数**，一切口径计算走 semantics/project/provider/range
/// （GC 6）：**不得**自己算命中率、**不得**直连 `provider::resolve_provider`（供应商一律经
/// `DeltaBuilder::provider_of(explicit, model)`，否则 `provider_kind` 落不了库、UI 三态全退化）；
/// 供应商规则只能读 `CollectContext::provider_rules()`（GC 21），**不得**自己调 `settings::load()`。
///
/// **项目归属调用纪律（W-13 / D21）**：逐记录一律用 `project::project_key_of(cwd)`（**零 FS**）；
/// `project::record_project(cwd)`（内含一次 `canonicalize`，是本域唯一的非纯函数）**每个会话只调一次**
/// ——把它挂到每条记录上，就是在 codex 2.21 GB / 226,035 行的采集热路径里叠数十万次 syscall。
///
/// **一遍扫描**：文件读取只经 `CollectContext::read_incremental`，每个候选文件一轮只读一遍
/// （`stats.repeat_reads` 必须为 0）；SQLite 型源豁免 L2/L3 但同样要一次性读全。
pub trait UsageCollector: Send + Sync {
    fn source_id(&self) -> UsageSourceId;
    /// 一次扫描：每个候选文件只读一遍，在同一遍里产出四桶 / turn / 工具 / 报错 / 时长
    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError>;
    /// 本源的可得性表（默认查静态表）。
    /// **只许测试替身覆写**（M-4）：生产采集器一律用默认实现——`caps.rs` 头注释写的是
    /// 「采集器与查询层共用同一张表，杜绝两边漂移」，而**可覆写点正是漂移的唯一入口**
    /// （某个源在采集器里覆写出一张「自己版本」的可得性表，查询层却还在读静态表）。
    fn caps(&self) -> SourceCaps {
        caps_of(self.source_id())
    }
}

pub fn all_collectors() -> Vec<Box<dyn UsageCollector>> {
    super::collectors::all()
}

/// 单源采集：把采集器结果与统计一起交出（**不落库**——落库由 run_collection 统一做，
/// 便于测试只跑解析、也便于失败隔离）
pub fn collect_source(
    c: &dyn UsageCollector,
    home: &Path,
    now_ms: i64,
    cursors: &HashMap<String, CursorDelta>,
) -> (UsageSourceStatus, SourceDelta, CollectStats) {
    let ctx = CollectContext::new(home, now_ms, cursors);
    let sid = c.source_id();
    match c.collect(&ctx) {
        Ok(delta) => {
            let stats = ctx.stats();
            let status = UsageSourceStatus {
                source_id: sid,
                ok: true,
                parsed_files: delta.parsed_files,
                new_records: delta.new_records,
                error_code: None,
            };
            (status, delta, stats)
        }
        Err(e) => {
            log::warn!(
                "usage: 源 {} 采集失败 [{}]: {}",
                sid.db_id(),
                e.code,
                e.detail
            );
            let status = UsageSourceStatus {
                source_id: sid,
                ok: false,
                parsed_files: 0,
                new_records: 0,
                error_code: Some(e.code),
            };
            (status, SourceDelta::default(), ctx.stats())
        }
    }
}

/// 一轮采集的**可注入内核**（生产入口 `run_collection` 传真实值；单测传替身 → 零 DB、
/// 零真实采集器）。它存在的唯一理由是让 W-06 的两条判据与「总开关关闭 → 一个源都不跑、
/// 连游标都不读」可真跑地断言：Task 10 的登记表是**空表**，只经 `run_collection` 观察的话
/// 开启/关闭都返回 `sources: []`（假绿），抓不住任何一条。
///
/// 遍历纪律（W-06）：任何一源的 `collect` / `apply` 返回 `Err`，**只记进该源的
/// `UsageSourceStatus`（`ok=false` + `errorCode`）然后继续下一源**——不得 `unwrap`、
/// 不得 `?` 上抛终止整轮、不得让循环卡死；否则确定性毒行会把其余六个源连带采不到数。
fn collect_sources(
    enabled: bool,
    now_ms: i64,
    home: &Path,
    collectors: Vec<Box<dyn UsageCollector>>,
    load_cursors: &dyn Fn(&str) -> HashMap<String, CursorDelta>,
    apply: &dyn Fn(UsageSourceId, &SourceDelta) -> Result<(), UsageError>,
) -> (Vec<UsageSourceStatus>, i64) {
    // 总开关关闭（说明书 §P7）：**在读游标（全局 DB 入口）之前**返回——不采集、不落库。
    if !enabled {
        log::debug!("usage: 总开关关闭，跳过本轮采集（零扫描、零落库）");
        return (Vec::new(), 0);
    }
    let mut statuses = Vec::new();
    let mut total = 0i64;
    for c in collectors {
        let sid = c.source_id();
        let cursors = load_cursors(sid.db_id());
        let (mut status, delta, stats) = collect_source(c.as_ref(), home, now_ms, &cursors);
        if stats.repeat_reads > 0 {
            log::warn!(
                "usage: 源 {} 本轮存在重复文件读取 {} 次（违反「一遍扫描」纪律）",
                sid.db_id(),
                stats.repeat_reads
            );
        }
        if status.ok {
            if let Err(e) = apply(sid, &delta) {
                // W-06：单源落库失败 = 本源的 ok=false + errorCode，整轮继续（不中断、不上抛）
                status.ok = false;
                status.error_code = Some(e.code.clone());
                // **批级回滚**（评审 M-1 更正）：ledger 是「一批 = 一个短事务」的**批级**提交
                // （`ledger.rs` 的 `plan_batches` + `write_one_batch`），游标批排在最后 →
                // 失败时**前几批很可能已经落盘**。本字段按「本轮**确认完整落库**」口径报 0
                // （不能报 delta 里的「计划写入行数」：那会让调用方把没落盘的行也算进去）。
                // 残留的「已提交但游标未推进 → 下轮重扫该文件」是 D-16 已接受的代价
                // （宁可重扫，不可漏）；其对增量累加账的重复计入风险已登记上报（见 fix report 疑虑）。
                status.new_records = 0;
                log::warn!(
                    "usage: 源 {} 落库失败 [{}]: {}",
                    sid.db_id(),
                    e.code,
                    e.detail
                );
            }
        }
        total += status.new_records;
        statuses.push(status);
    }
    (statuses, total)
}

/// 一轮完整采集（7 源串行；单源失败不影响其他源）
pub fn run_collection(now_ms: i64) -> UsageCollectResult {
    // ── Task 16 Step 3：lib 单测构建下的**采集拒绝闸**（写在生产函数体内）──────────────
    // 事故事实（唯一需要记住的因果）：lib 测试是**独立 crate**、不经 `tests/support.rs::setup()`
    // ⇒ 本函数既用 `dirs::home_dir()` 解析**真实**扫描根，又会经 `load_cursors` / `apply_delta`
    // 取**进程级 `DB`**（那时也绑在真实 `~/.tuvis/tuvis.db`）。`TUVIS_HOME=$(mktemp -d)` **只保账本、
    // 不保读源** ⇒ 2026-10-03 00:11 一轮真实采集落进用户账本（四表被写入、游标含真实路径）。
    //
    // 判据：`cfg(test)` 下**未显式设置 fixture 根（源目录覆盖）**时**响亮拒绝**——
    // **不是**静默 0、**不是**扫真实目录。位置必须在 `settings::load()` **之前**：
    // 那是本函数第一个全局 DB 入口（会取 `DB` 锁）。
    // 生产构建、以及本仓集成测试（链接的是**非 test** 构建）里本闸不存在。
    #[cfg(test)]
    {
        if tests::fixture_home().is_none() {
            panic!(
                "usage 采集在 lib 单测构建里被拒绝：未显式设置 fixture 根（源目录覆盖）。\
                 lib 测试不经 tests/support.rs::setup()，本函数会用 dirs::home_dir() 扫**真实**\
                 源目录、并把结果写进进程级 DB（2026-10-03 00:11 事故）。\
                 需要真跑的用例请先调 tests::set_fixture_home(Some(<tempdir>))。"
            );
        }
    }

    let started = std::time::Instant::now();
    let settings = super::settings::load();
    // 扫描根：生产一半逐字不变（`dirs::home_dir()`）；lib 单测一半只认 fixture 根
    // ——`TUVIS_HOME` 保账本，fixture 根保**读源**，两半都要在。
    // 集成测试（链接非 test 构建，没有上面的 lib 缝）走 `TUVIS_USAGE_FIXTURE_ROOT`
    // 环境变量覆盖：**只在 debug/test 构建生效，release 根本不读**（与 `TUVIS_HOME`
    // 同一门，`connection.rs::app_data_home` 先例）——2026-10-07 事故族收口：Windows
    // 下 `dirs::home_dir()` 不认 HOME，只重定向账本不重定向读源时，集成测试会扫
    // 开发机的**真实** profile（实测 zcode 71681 行），fixture 断言全被真机数据冲掉。
    #[cfg(test)]
    let home = tests::fixture_home().expect("上面那道拒绝闸已断言 fixture 根存在");
    #[cfg(all(not(test), debug_assertions))]
    let home = match std::env::var_os("TUVIS_USAGE_FIXTURE_ROOT") {
        Some(root) if !root.is_empty() => std::path::PathBuf::from(root),
        _ => dirs::home_dir().unwrap_or_default(),
    };
    #[cfg(all(not(test), not(debug_assertions)))]
    let home = dirs::home_dir().unwrap_or_default();
    let load_cursors = |sid: &str| crate::database::dao::usage::load_cursors(sid);
    // 落库走 `ledger::apply_delta`（分批短事务 + 批间让出，GC 2）；返回值只用于成败判定
    let apply =
        |sid: UsageSourceId, delta: &SourceDelta| ledger::apply_delta(sid, delta).map(|_| ());
    let (sources, total) = collect_sources(
        settings.enabled,
        now_ms,
        &home,
        all_collectors(),
        &load_cursors,
        &apply,
    );
    if settings.enabled {
        // 保留期清理：聚合后删明细（日聚合永久）
        if let Err(e) = ledger::purge_expired(settings.detail_retention_days, now_ms) {
            log::warn!("usage: 保留期清理失败 [{}]: {}", e.code, e.detail);
        }
    }
    UsageCollectResult {
        collected_at: now_ms,
        duration_ms: if settings.enabled {
            started.elapsed().as_millis() as i64
        } else {
            0
        },
        sources,
        total_new_records: total,
    }
}

/// 单飞与最小间隔的**飞行状态**。**只守判定，不守扫描**：
/// 扫描期间这把锁必须处于**已释放**状态（`collect_with` 只借它做「是否要扫」的判定与占位）。
#[derive(Default)]
struct Flight {
    /// 正在扫描（单飞互斥点；等待者用 `Condvar` 睡在这一位上）
    in_flight: bool,
    /// 上次**真扫**的结果与时刻（最小间隔的锚点 = 上次真扫时刻，**不是**上次调用时刻）
    last: Option<UsageCollectResult>,
    last_at_ms: i64,
}

/// 单飞互斥 + 最小间隔判定。**锁序铁律（评审 Critical A）**：本锁的临界区内**不得**获取
/// `DB` 锁、也不得执行整轮扫描（扫描必然取 `DB`）。见模块文档「锁序铁律」。
static COLLECT_FLIGHT: Lazy<(Mutex<Flight>, Condvar)> =
    Lazy::new(|| (Mutex::new(Flight::default()), Condvar::new()));

/// 采集轮次计数（**不参与飞行锁**：`collect_calls()` 可能在持 `DB` 锁的路径上被调用——
/// 例如启动采集与查询并发时，为了读一个计数去取飞行锁就等于把那条致命锁序又引进来一次）
static COLLECT_CALLS: AtomicUsize = AtomicUsize::new(0);

/// **结果发布区**（与飞行互斥**分开**的第二把锁）：只在**扫描结束、且所有 DB 操作都做完
/// 之后**写入；读者（`cached_result()`）只短暂持锁 `clone`，**永不**触碰飞行锁
/// → 「持 `DB` 锁的查询路径调 `cached_result()`」与「飞行中的采集」之间不再有任何等待关系
/// （这正是把 ABBA 环路拆掉的那一刀）。锁：`cached_result_never_blocks_behind_an_in_flight_scan`。
static LAST_RESULT: Lazy<Mutex<Option<UsageCollectResult>>> = Lazy::new(|| Mutex::new(None));

/// 飞行位释放护栏：扫描 **panic** 时也必须清位并唤醒等待者，否则单飞永久卡死
/// （与 `adapter/mod.rs` 的 `ScanFlightGuard` 同款：一次 panic 把「单轮失败」恶化成
/// 「看板冻结到重启」）。
struct FlightGuard;

impl Drop for FlightGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*COLLECT_FLIGHT;
        {
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.in_flight = false;
        } // 先改谓词（持锁），再唤醒——否则等待者可能丢唤醒（谓词变更必须在锁内）
        cvar.notify_all();
    }
}

/// 单飞 + 最小间隔（契约 §3）。语义与任务书一致：并发调用**复用同一次结果**、窗口内不重扫、
/// `force=true` 无视窗口；区别只在**持锁范围**——扫描在锁外跑（评审 Critical A 的修法）。
pub fn collect_with(
    force: bool,
    now_ms: i64,
    runner: &dyn Fn(i64) -> UsageCollectResult,
) -> UsageCollectResult {
    // ① 设置读取放在**取飞行锁之前**：生产构建里 `settings::load()` 会经
    //    `dao::settings::get_setting` 取 `DB` 锁 → 放进临界区就是
    //    `COLLECT_FLIGHT → DB` 这条致命序的第二个实例（模块文档「锁序铁律」）。
    let interval_ms = super::settings::load().collect_interval_min.max(1) * 60_000;
    let (lock, cvar) = &*COLLECT_FLIGHT;
    let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
    // ② 单飞：飞行中就让出锁去等（`Condvar::wait` 原子释放锁；等待期间不持任何锁、不取 DB）。
    //    醒来后**重新判定**最小间隔 → 后来者拿到的是刚落地的同一次结果，而不是各扫一遍。
    while st.in_flight {
        st = cvar.wait(st).unwrap_or_else(|e| e.into_inner());
    }
    if !force {
        if let Some(last) = st.last.clone() {
            if now_ms.saturating_sub(st.last_at_ms) < interval_ms {
                return last; // 不扫描（也不碰 DB：连 settings 都在锁外读过了）
            }
        }
    }
    // ③ 占位后**立即释放飞行锁**：整轮扫描在锁外跑（runner 走 `load_cursors`/`apply_delta`/`purge_expired`）。
    //    护栏从占位那一刻就挂上：扫描 panic 也必须清位 + 唤醒（见 `FlightGuard`）。
    st.in_flight = true;
    let _flight_guard = FlightGuard;
    drop(st);
    COLLECT_CALLS.fetch_add(1, Ordering::SeqCst);
    let result = runner(now_ms);
    // ④ 发布：扫描结束、**所有 DB 操作都做完之后**才写回状态（此后等待者醒来即复用）
    {
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        st.last = Some(result.clone());
        st.last_at_ms = now_ms; // 锚点 = 本次**真扫**时刻（M-6 锁：滑动窗口抑制会被用例抓住）
    }
    *LAST_RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.clone());
    result
}

pub fn collect(force: bool) -> UsageCollectResult {
    // ── Task 16 Step 3：同一道**采集拒绝闸**（`collect()` 正是 2026-10-03 被注入生产文件的
    // 那一句）。它比 `run_collection()` 更早一步：`collect_with` 会先经 `settings::load()`
    // 取全局 `DB` 锁，所以拒绝必须发生在进入 `collect_with` **之前**。
    // 判据与文案与 `run_collection()` 的同名守卫**同源**（`tests::fixture_home()`）。
    #[cfg(test)]
    {
        if tests::fixture_home().is_none() {
            panic!(
                "usage 采集在 lib 单测构建里被拒绝：未显式设置 fixture 根（源目录覆盖）。\
                 lib 测试不经 tests/support.rs::setup()，采集会用 dirs::home_dir() 扫**真实**\
                 源目录、并把结果写进进程级 DB（2026-10-03 00:11 事故）。\
                 需要真跑的用例请先调 tests::set_fixture_home(Some(<tempdir>))。"
            );
        }
    }
    collect_with(force, super::now_ms(), &run_collection)
}

/// 最近一次采集结果。**只读 `LAST_RESULT`**（短暂持锁 clone）——**不触碰飞行锁**，
/// 所以查询层可以「先持 `DB` 锁、再调本函数」（Task 19 的 `usage_records` 就是这么写的），
/// 不会与飞行中的采集构成 ABBA（评审 Critical A）。
pub fn cached_result() -> Option<UsageCollectResult> {
    LAST_RESULT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 采集轮次计数（**「采集不进入 3 秒轮询」的可执行断言**：会话扫描后它必须仍为 0）。
/// 原子量：**不取任何锁**（否则又是 `COLLECT_FLIGHT → DB` 那条序的入口）。
pub fn collect_calls() -> usize {
    COLLECT_CALLS.load(Ordering::SeqCst)
}

/// 单测专用：复位单飞状态（评审 C5）。`COLLECT_FLIGHT` / `LAST_RESULT` / `COLLECT_CALLS`
/// 都是**进程级**全局量，三个用例（最小间隔 / 并发单飞 / 总开关）共享它们 → 不隔离就会
/// 间歇性互相串味（典型交错：`concurrent…` 把 `last_at_ms` 置为 2e12，`min_interval…` 的
/// `saturating_sub` 饱和成 0 → 拿到**另一个用例的结果**）。
/// 每个用例持有 `tests::TEST_LOCK` 后调用本函数，即可保证各自从零态开始。
#[cfg(test)]
pub fn reset_state_for_test() {
    {
        let (lock, _cvar) = &*COLLECT_FLIGHT;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        *st = Flight::default();
    }
    *LAST_RESULT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    COLLECT_CALLS.store(0, Ordering::SeqCst);
}

/// 应用启动一次（Task 20 在 lib.rs setup 调用；延迟 3s 避开启动高峰，失败只 warn）
pub fn spawn_initial_collection() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let r = collect(false);
        log::info!(
            "usage: 启动采集完成（{} 源 / {} 条新记录 / {}ms）",
            r.sources.len(),
            r.total_new_records,
            r.duration_ms
        );
    });
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    // **必须显式导入设置层**（本轮自查额外发现①，§3.2.3）：`use super::*` 只带进
    // `collect.rs` 自己 `use` 进来的名字，兄弟模块 `settings` 与其类型/函数不在其中——
    // 漏了就是 E0433（`settings` 未解析）/ E0412（`UsageSettings`）/ E0425（`merge_patch`），
    // Task 10 整个 `mod tests` 编译不过、"7 个测试全绿"不可达。
    use crate::services::usage::model::{UsageSettings, UsageSettingsPatch};
    use crate::services::usage::settings::{self, merge_patch, parse_settings};
    // 同款显式导入（D-01）：`use super::*` 只带进 `collect.rs` 自己 `use` 进来的那几个名字
    // （`caps_of` / `SourceCaps`），`turn_semantics` 不在其中 → 裸用就是 E0425。
    // 刻意写在测试模块里：放模块级会让生产构建多一条 `unused_imports`（门禁 `-D warnings` 下是 error）。
    use crate::services::usage::caps::turn_semantics;

    /// **用例独立性（评审 C5）**：本模块有三个用例共享全局 `COLLECT_STATE`（单飞 / 最小间隔 /
    /// 总开关），必须 ① 串行化、② 每个用例开头复位状态、③ **完全不碰数据库**
    /// （旧版 `crate::database::init()` + `save(...)` 写的是开发机真实 `~/.tuvis/tuvis.db`，
    /// 与本文件 `settings::load_from_conn` 那条纪律直接矛盾）。
    /// 现在设置走 `settings::set_override_for_test`（内存注入，不落库、不读库）——
    /// 于是这些用例既不会互相串味，也不会污染真实库。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fake_result(at: i64) -> UsageCollectResult {
        UsageCollectResult {
            collected_at: at,
            duration_ms: 1,
            sources: Vec::new(),
            total_new_records: 0,
        }
    }

    /// **Task 16 Step 3 的 fixture 根（源目录覆盖）**：`cfg(test)` 下 `collect()` /
    /// `run_collection()` 只认它——没设置就**响亮拒绝**（见两处生产函数体里的拒绝闸）。
    ///
    /// **为什么放在排除区里**（自省锁的落点纪律）：它是**测试辅助顶层项的存储**，
    /// 不是生产代码；放 BEGIN 之前会落进「生产半边」判据面。守卫本身（`if …panic!`）
    /// 按纪律写在 `collect()` / `run_collection()` 的**生产函数体内**，只经这对
    /// `pub(super)` 存取口读它——两边都在 `cfg(test)` 之下，生产构建里一个字都不存在。
    static FIXTURE_HOME: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// 当前 fixture 根（`None` = 没设置过 ⇒ 采集入口一律拒绝）。
    pub(super) fn fixture_home() -> Option<PathBuf> {
        FIXTURE_HOME
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 设置 / 复位 fixture 根。要真跑采集的用例必须先调它（传一个**临时目录**）。
    pub(super) fn set_fixture_home(home: Option<PathBuf>) {
        *FIXTURE_HOME.lock().unwrap_or_else(|e| e.into_inner()) = home;
    }

    /// **fixture 根的 Drop 守卫**（Task 16 修复轮 1 / Minor 3）：把它从「手工复位的纪律」变成
    /// **无条件不变量**。
    ///
    /// 为什么必须这样：`FIXTURE_HOME` 是**进程级全局**，靠用例体末尾 `set_fixture_home(None)`
    /// 复位属于「只要中间 panic 就失效」的纪律 —— 一旦某个用例在设位与复位之间 panic
    /// （断言失败、或被测代码把 panic 传染出来），**后续每个用例**都会一直拿到这个
    /// 「指向（可能已不存在的）临时路径的根」：采集不会被拒绝，而是安静地去扫一个空目录
    /// ⇒ **响亮锁被悄悄降级成静默绿**。RAII 后 panic 展开也会走 `Drop`，复位无条件成立。
    /// 声明顺序也有讲究：`_g`（串行锁）必须先声明 ⇒ 后析构 ⇒ 复位发生在**仍持锁时**。
    struct FixtureHomeGuard;

    impl FixtureHomeGuard {
        fn pin(home: Option<PathBuf>) -> Self {
            set_fixture_home(home);
            Self
        }
    }

    impl Drop for FixtureHomeGuard {
        fn drop(&mut self) {
            set_fixture_home(None);
        }
    }

    /// **锁表第 2 条「采集拒绝」**：`cfg(test)` 下**没有** fixture 根时调采集入口 ⇒
    /// **响亮失败**（panic），不是静默 0、更不是扫真实目录。
    ///
    /// 为什么用 `catch_unwind` 而不是「让它直接炸」：本用例要**断言那个 panic 存在**——
    /// 不 catch 的话 panic 会让本用例自己变红，红灯不能当锁。panic 文案照常经**默认 panic
    /// hook** 打到测试输出（本用例不替换 hook）⇒「响亮」这一半是真可观测的；
    /// 而任何**真实**触发（例如把 `collect(false)` 注入生产文件，见 Step 5 重演）都会让
    /// **被注入的那个用例**失败并把这段文案打进 failures 段。
    #[test]
    fn collection_is_refused_without_an_explicit_fixture_root() {
        let _g = exclusive();
        reset_state_for_test();
        let _fixture = FixtureHomeGuard::pin(None); // 「没设置过」（Drop 会无条件复位）

        fn message_of(e: &(dyn std::any::Any + Send)) -> String {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "（非字符串 panic 载荷）".to_string())
        }

        let calls_before = collect_calls();

        // ① `collect(false)` —— 事故里被注入生产文件的正是这一句
        let payload = std::panic::catch_unwind(|| collect(false)).expect_err(
            "cfg(test) 下未设置 fixture 根时 `collect(false)` 必须**响亮失败**：\
             静默返回一个 0 结果、或照旧扫真实源目录，都等于事故复现",
        );
        let msg = message_of(payload.as_ref());
        assert!(
            msg.contains("fixture 根") && msg.contains("被拒绝"),
            "拒绝文案必须点明原因（否则下次真红又要花一轮定位），实际：{msg}"
        );

        // ② `collect(true)`（强制轮，绕过最小间隔窗口）同样拒绝——不得有「force 就放行」的后门
        assert!(
            std::panic::catch_unwind(|| collect(true)).is_err(),
            "`collect(true)` 也必须被拒绝（force 不得成为绕过拒绝闸的后门）"
        );

        // ③ `run_collection()` 直调同样拒绝——它才是真正碰 `dirs::home_dir()` 与进程级 `DB` 的那个
        assert!(
            std::panic::catch_unwind(|| run_collection(1_700_000_000_000)).is_err(),
            "`run_collection()` 也必须被拒绝（`collect()` 之外还有它这一个入口）"
        );

        // ④ 拒绝必须是「**没开始**」：连采集轮次都不许计入（否则「响亮」就只是事后响）
        assert_eq!(
            collect_calls(),
            calls_before,
            "被拒绝的采集调用不得计入 `COLLECT_CALLS`（一轮都没开始跑）"
        );

        reset_state_for_test();
    }

    /// **Minor 3 的锁**：fixture 根的复位必须是**无条件不变量**（panic 展开也走 `Drop`），
    /// 而不是「用例体末尾手工调一次」的纪律。
    ///
    /// 为什么值得单列成用例：靠手工复位时，只要**任何一个**用例在「设位 → 复位」之间 panic
    /// （断言失败、或被测代码把 panic 传染出来），进程级 `FIXTURE_HOME` 就会一直留着那个根
    /// ⇒ 后续用例的 `collect()` / `run_collection()` **不再被拒绝**，而是安静地去扫一个
    /// 空目录 ⇒ **响亮锁被静默降级**（正是本任务要防的那一类「假绿」）。
    /// 判据三条：① 守卫存活期间根已设；② 正常离开作用域后回 `None`；
    /// ③ **panic 展开后同样回 `None`**，且此后 `collect(false)` 重新回到「响亮拒绝」。
    #[test]
    fn fixture_home_guard_resets_even_on_panic() {
        let _g = exclusive();
        reset_state_for_test();
        let probe = PathBuf::from("/tmp/mam-fixture-guard-probe");

        // ①② 正常作用域
        {
            let _f = FixtureHomeGuard::pin(Some(probe.clone()));
            assert_eq!(
                fixture_home(),
                Some(probe.clone()),
                "守卫存活期间 fixture 根必须已设"
            );
        }
        assert_eq!(fixture_home(), None, "守卫离开作用域后必须复位");

        // ③ panic 展开路径
        let caught = std::panic::catch_unwind(|| {
            let _f = FixtureHomeGuard::pin(Some(PathBuf::from("/tmp/mam-fixture-guard-probe")));
            panic!("模拟用例中途 panic（断言失败 / 被测代码把 panic 传染出来）");
        });
        assert!(
            caught.is_err(),
            "本腿必须真的 panic（否则测的不是展开路径）"
        );
        assert_eq!(
            fixture_home(),
            None,
            "**panic 展开后也必须复位**——否则后续用例会一直拿到这个根，\
             「无 fixture 根 ⇒ 响亮拒绝」的锁就被静默降级成「安静扫空目录」"
        );
        assert!(
            std::panic::catch_unwind(|| collect(false)).is_err(),
            "复位后 `collect(false)` 必须重新回到「响亮拒绝」（锁没有被降级）"
        );

        reset_state_for_test();
    }

    /// 契约 §3：`force=false` 且距上次采集小于 `collectIntervalMin` → **直接返回上次结果（不扫描）**
    #[test]
    fn min_interval_reuses_last_result_without_scanning() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            collect_interval_min: 10,
            ..Default::default()
        }));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner = move |now: i64| {
            c.fetch_add(1, Ordering::SeqCst);
            fake_result(now)
        };
        let t0 = 1_000_000_000_000i64;
        let a = collect_with(true, t0, &runner); // force 必扫（锚点 = t0）
        let b = collect_with(false, t0 + 60_000, &runner); // 1 分钟后：仍在 10 分钟窗口内
                                                           // **M-6（评审）**：窗口锚点必须是**上次真扫时刻**，不是「上次调用时刻」。
                                                           // 只喂「1 分钟 → 11 分钟」这种跳变锁不住它：把 `st.last_at_ms = now_ms` 挪进复用分支
                                                           // （= 滑动窗口抑制：每次复用都把窗口往前推）在旧断言下**仍绿**。
                                                           // 所以这里在窗口内**连续探两次**（1min 已探过，再探 9min），再探**恰好 t0+10min**：
                                                           // 锚点若被滑动到 t0+9min，最后一次就该「复用」而不是重扫 → 下面 calls==2 立刻红。
        let b9 = collect_with(false, t0 + 9 * 60_000, &runner);
        assert_eq!(
            b9, a,
            "窗口内两次调用都必须复用同一次结果（滑动窗口抑制会在这里露馅）"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "窗口内连续调用（1min / 9min）都不得重扫：锚点是上次真扫时刻 t0"
        );
        let c = collect_with(false, t0 + 10 * 60_000, &runner); // 恰好 t0+10min：超窗口，重扫
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "距上次**真扫**满 10 分钟必须重扫（锚点被滑动会假绿）"
        );
        assert_eq!(a, b, "窗口内必须复用上次结果（同一对象语义）");
        assert_ne!(b, c);
        let d = collect_with(true, t0 + 11 * 60_000 + 1000, &runner);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "force=true 无视最小间隔");
        assert_eq!(d.collected_at, t0 + 11 * 60_000 + 1000);
        settings::set_override_for_test(None);
        reset_state_for_test();
    }

    /// **评审 Critical A 的锁（ABBA 死锁）**：`cached_result()` **不得**被飞行中的扫描挡住。
    ///
    /// 环路的两半边（评审 + 控制窗口已独立复核）：① 旧版 `collect_with` 把飞行锁一直持到
    /// 整轮扫描结束（codex 是分钟级窗口），期间还取 `DB`（`load_cursors` / `apply_delta`）；
    /// ② 查询层（Task 19 的 `usage_records`）先取 `DB`、再调 `cached_result()`。
    /// 两序相对即 ABBA——`std::sync::Mutex` 无超时，而 `DB` 是**全库唯一连接锁**。
    ///
    /// **为什么用「DB 替身」而不是真的 `DB`**：在 lib 单测里锁 `DB` 会初始化并读写开发机真实
    /// `~/.tuvis/tuvis.db`（D-07 已登记的泄漏），而本模块的用例必须零 DB（GC 19 / C5）。
    /// ABBA 的成立条件**只取决于「另一把锁的持锁者会不会去等 `cached_result()`」**，
    /// 与那把锁是不是 `DB` 无关——所以替身足以**确定性地**复现整条环路，且零 DB。
    /// 旧实现下本用例会在 `recv_timeout` 超时处红；修复后立即返回绿。
    #[test]
    fn cached_result_never_blocks_behind_an_in_flight_scan() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            collect_interval_min: 10,
            ..Default::default()
        }));
        let db = Arc::new(Mutex::new(())); // 「DB」替身（见用例文档）
        let (tx_scan, rx_scan) = std::sync::mpsc::channel::<()>(); // 采集：扫描已开跑
        let (tx_holds, rx_holds) = std::sync::mpsc::channel::<()>(); // 查询：已持有「DB」
        let (tx_val, rx_val) = std::sync::mpsc::channel::<Option<UsageCollectResult>>();
        let db_scan = db.clone();
        let scanner = std::thread::spawn(move || {
            collect_with(true, 5_000_000_000_000, &move |now: i64| {
                let _ = tx_scan.send(());
                // 等查询侧先拿到「DB」：把竞态变成确定序（旧实现下这里就是环路的另一半）
                let _ = rx_holds.recv_timeout(std::time::Duration::from_secs(5));
                // 扫描末尾的落库（= run_collection 调 apply_delta 的位置）也要「DB」
                let _db = db_scan.lock().unwrap_or_else(|e| e.into_inner());
                fake_result(now)
            })
        });
        rx_scan
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("扫描必须真的开跑（且开跑时飞行锁应已释放）");
        let db_query = db.clone();
        std::thread::spawn(move || {
            let _db = db_query.lock().unwrap_or_else(|e| e.into_inner()); // usage_records 形态：先持 DB
            let _ = tx_holds.send(());
            let _ = tx_val.send(cached_result()); // 再调 cached_result()
        });
        let got = rx_val
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("持 DB 锁时调 cached_result() 不得阻塞（旧实现 = ABBA 死锁）");
        assert!(got.is_none(), "首轮尚未发布 → None（而不是阻塞）");
        // 采集线程此时能拿到「DB」并跑完（若上面是死锁，这里 join 会永远等不到）
        let published = scanner.join().expect("采集线程不得 panic");
        assert_eq!(published.collected_at, 5_000_000_000_000);
        assert_eq!(
            cached_result().map(|r| r.collected_at),
            Some(5_000_000_000_000),
            "扫描结束后发布区必须有结果（发布在 DB 操作之后）"
        );
        assert_eq!(collect_calls(), 1, "轮次计数走原子量，不取飞行锁");
        settings::set_override_for_test(None);
        reset_state_for_test();
    }

    /// 飞行锁**不贯穿扫描**（Critical A 的结构面）：扫描进行中 `collect_calls()` /
    /// `cached_result()` 都必须立刻返回——它们一个走原子量、一个走发布区，都不碰飞行锁。
    /// 旧实现里这三个读取口全部挤在 `COLLECT_STATE` 上，扫描期间一律阻塞。
    #[test]
    fn call_counter_and_cached_result_do_not_take_the_flight_lock() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            collect_interval_min: 10,
            ..Default::default()
        }));
        let (tx_scan, rx_scan) = std::sync::mpsc::channel::<()>();
        let (tx_release, rx_release) = std::sync::mpsc::channel::<()>();
        let scanner = std::thread::spawn(move || {
            collect_with(true, 1_234_567_890_000, &move |now: i64| {
                let _ = tx_scan.send(());
                // 扫描停在半途：这一整段时间里飞行锁都不该被持有
                let _ = rx_release.recv_timeout(std::time::Duration::from_secs(5));
                fake_result(now)
            })
        });
        rx_scan
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("扫描必须真的开跑");
        // 扫描进行中：两个读取口都必须立刻返回（旧实现会在这里卡到扫描结束）
        let (tx_probe, rx_probe) = std::sync::mpsc::channel::<usize>();
        std::thread::spawn(move || {
            let calls = collect_calls();
            let _ = cached_result();
            let _ = tx_probe.send(calls);
        });
        let calls = rx_probe
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("扫描期间 collect_calls()/cached_result() 不得阻塞（=ABBA 的前置条件）");
        assert_eq!(calls, 1, "本轮已开始扫描 → 计数为 1（且是立刻读到的）");
        let _ = tx_release.send(());
        scanner.join().unwrap();
        settings::set_override_for_test(None);
        reset_state_for_test();
    }

    /// **Critical A 的另一半（结构面）**：扫描进行中，**飞行锁必须处于空闲状态**。
    /// 这是「`COLLECT_FLIGHT` 临界区内不得取 `DB`」这条铁律的前半——只要扫描期间它压根
    /// 没被持有，就不可能存在「持飞行锁的线程去等 `DB`」这个方向。
    /// 手法：扫描停在中途时对本模块的 `COLLECT_FLIGHT` 做 `try_lock()`（测试模块在
    /// `collect.rs` 内部，能看见私有静态量）。旧实现（飞行锁贯穿整轮扫描）这里必然 `Err`。
    #[test]
    fn flight_lock_is_released_while_the_scan_runs() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            collect_interval_min: 10,
            ..Default::default()
        }));
        let (tx_scan, rx_scan) = std::sync::mpsc::channel::<()>();
        let (tx_release, rx_release) = std::sync::mpsc::channel::<()>();
        let scanner = std::thread::spawn(move || {
            collect_with(true, 42, &move |now: i64| {
                let _ = tx_scan.send(());
                let _ = rx_release.recv_timeout(std::time::Duration::from_secs(5));
                fake_result(now)
            })
        });
        rx_scan
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("扫描必须真的开跑");
        let (lock, _cvar) = &*COLLECT_FLIGHT;
        assert!(
            lock.try_lock().is_ok(),
            "扫描期间飞行锁必须已释放：否则任何想拿它的路径都要阻塞整轮扫描（Critical A）"
        );
        // 前提坐实：扫描此刻确实还在跑（否则上面的断言可能只是「扫描早就结束了」）
        assert_eq!(collect_calls(), 1);
        let _ = tx_release.send(());
        scanner.join().unwrap();
        assert!(cached_result().is_some(), "扫描结束后必须发布结果");
        settings::set_override_for_test(None);
        reset_state_for_test();
    }

    /// 契约 §3：**单飞——并发调用复用同一次结果**（不得各扫一遍）
    #[test]
    fn concurrent_calls_share_one_run() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            collect_interval_min: 10,
            ..Default::default()
        }));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let runner = move |now: i64| {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            fake_result(now)
        };
        let runner = Arc::new(runner);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let r = runner.clone();
                std::thread::spawn(move || collect_with(false, 2_000_000_000_000, r.as_ref()))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "并发 4 次只能扫 1 次");
        assert!(
            results.iter().all(|r| *r == results[0]),
            "四次并发必须拿到同一次结果"
        );
        settings::set_override_for_test(None);
        reset_state_for_test();
    }

    /// 「一次扫描产出全部指标」的可执行断言：同一文件被读第二遍 → repeat_reads > 0
    /// （只用 tempfile + CollectContext：**零全局状态、零 DB**，可与其他用例并行）
    #[test]
    fn repeat_reads_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("wire.jsonl");
        std::fs::write(&f, "a\n").unwrap();
        let cursors = std::collections::HashMap::new();
        let ctx = CollectContext::new(dir.path(), 0, &cursors);
        let _ = ctx.read_incremental(&f, "f1").unwrap();
        let _ = ctx.read_incremental(&f, "f1").unwrap(); // 违规：同一文件读两遍
        let s = ctx.stats();
        assert_eq!(s.reads, 2);
        assert_eq!(
            s.repeat_reads, 1,
            "重复读必须被记为违规（采集器测试断言它为 0）"
        );
        // W-15：**exact**。空游标表 → 两遍都是全量路 → 2 + 2 = 4 字节。
        // 只写 `>= 4` 的话，「第二遍没真读」与「真读了两遍」在断言上等价（路径不可辨）。
        assert_eq!(
            s.bytes_read, 4,
            "两遍全量读 = 2×2 字节（W-15：把路径也纳入断言）"
        );
    }

    /// 总开关关闭 → 不采集、不落库（说明书 §P7）
    /// **零 DB**：设置走内存注入；关闭后 `run_collection` 在读设置之后立即返回，
    /// 连 `load_cursors` 都不会走到（旧版会写开发机真实库）。
    ///
    /// **Task 16 追加**：`run_collection` 在 `cfg(test)` 下要求**显式** fixture 根（源目录覆盖）。
    /// 本用例断言的是「总开关关闭 → 一个源都不跑」，故给一个**固定不存在的空根**：
    /// 即便总开关判断被删掉，采集也只会在这个空目录里一无所获——**不会**碰真实源目录。
    /// 断言一字未松。
    #[test]
    fn disabled_switch_skips_all_sources() {
        let _g = exclusive();
        reset_state_for_test();
        settings::set_override_for_test(Some(UsageSettings {
            enabled: false,
            ..Default::default()
        }));
        // fixture 根走 **Drop 守卫**（Minor 3）：即便本用例中途 panic（断言失败 / 被测代码
        // 把 panic 传染出来），复位也无条件发生 —— 否则后续用例会一直拿到这个空根，
        // 把「无 fixture 根 ⇒ 响亮拒绝」的锁**悄悄降级**成「有根 ⇒ 安静扫空目录」。
        let _fixture = FixtureHomeGuard::pin(Some(
            std::env::temp_dir().join("mam-usage-fixture-must-not-exist"),
        ));
        let r = run_collection(1_700_000_000_000);
        assert!(r.sources.is_empty(), "关闭后不得跑任何源");
        assert_eq!(r.total_new_records, 0);
        settings::set_override_for_test(None); // 复位：不得影响其他用例（`_fixture` 由 Drop 复位）
        reset_state_for_test();
    }

    /// **总开关关闭的「真锁」**（补 §P7 那条判据）：Task 10 的登记表是**空**的，
    /// 于是 `run_collection` 无论开关都返回 `sources: []` ——单看上面那条用例抓不住
    /// 「关闭到底有没有跳过源」。这里经**可注入内核**塞进两个替身采集器：关闭时它们必须
    /// **一次都没被调用**，且 `load_cursors`（全局 DB 入口）也必须一次都没被调用。
    /// 删掉总开关判断 → 替身立刻被调用 → 红。
    #[test]
    fn disabled_switch_skips_every_collector_and_never_reads_cursors() {
        struct Counted {
            calls: Arc<AtomicUsize>,
        }
        impl UsageCollector for Counted {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Claude
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(SourceDelta::default())
            }
        }
        let collector_calls = Arc::new(AtomicUsize::new(0));
        let cursor_loads = Arc::new(AtomicUsize::new(0));
        let collectors: Vec<Box<dyn UsageCollector>> = vec![
            Box::new(Counted {
                calls: collector_calls.clone(),
            }),
            Box::new(Counted {
                calls: collector_calls.clone(),
            }),
        ];
        let loads = cursor_loads.clone();
        let load_cursors = move |_: &str| {
            loads.fetch_add(1, Ordering::SeqCst);
            HashMap::new()
        };
        let apply = |_: UsageSourceId, _: &SourceDelta| Ok(());
        let (sources, total) = collect_sources(
            false,
            1_700_000_000_000,
            std::path::Path::new("/nonexistent"),
            collectors,
            &load_cursors,
            &apply,
        );
        assert!(sources.is_empty(), "关闭后不得产出任何源状态");
        assert_eq!(total, 0);
        assert_eq!(
            collector_calls.load(Ordering::SeqCst),
            0,
            "关闭后采集器一次都不许被调用"
        );
        assert_eq!(
            cursor_loads.load(Ordering::SeqCst),
            0,
            "关闭后连游标都不许读（零 DB 是 C5 的硬口径）"
        );
    }

    /// 可得性静态表（说明书 §9.5 落码）：抽查关键格子，防有人"顺手"把不可得的填成可用
    #[test]
    fn caps_table_matches_availability_matrix() {
        assert!(
            !caps_of(UsageSourceId::WorkBuddy).turn,
            "workbuddy 无回合概念"
        );
        assert!(
            !caps_of(UsageSourceId::WorkBuddy).longest_turn,
            "workbuddy 无 duration 字段"
        );
        assert!(
            !caps_of(UsageSourceId::OpenCode).error_tool,
            "opencode 工具级不可得"
        );
        assert!(
            !caps_of(UsageSourceId::Claude).error_turn,
            "claude 无回合级失败字段"
        );
        assert!(
            caps_of(UsageSourceId::Kimi).user_est,
            "kimi 能读用户文本（§4.3 只对 claude/codex/kimi 计算）"
        );
        assert!(
            !caps_of(UsageSourceId::ZCode).user_est,
            "zcode 读不到用户文本"
        );
        for s in UsageSourceId::ALL {
            // §9.5（2026-10-06 更新）：dsh 的工具调用/逐工具为**部分覆盖**（原始日志并非每个会话
            // 都还在——**比例不写死**，覆盖率随机器与清理策略变化；探测期的「19/99」已作废，
            // 2026-10-06 实测账本口径已接近全量）
            // → 仍记 available=true，缺失会话显示空态，reason 里写明覆盖范围（不得当作全量、不得填 0）
            assert!(
                caps_of(s).tool_calls,
                "{:?} 的工具调用可得（dsh 为部分覆盖，见 §9.5/§8.6）",
                s
            );
        }
        // 任务书勘误（与 D-09/D-17 同族，**已登记在报告「偏差申报」**）：Step 1 写的是
        // `contains("sessionStats.turns")`，而 Step 3 给的 caps.rs 文案是
        // `"dsh: sessionStats.val.turns（…）"` —— `"sessionStats.val.turns".contains("sessionStats.turns")`
        // 为 **false**（任务书自相矛盾）。判据本身（dsh 的 turn 口径必须点名 sessionStats 里的 turns）
        // 不变，只把期望子串收紧到**真实 JSON 路径**：Task 17 的 dsh 载荷是
        // `{"sessionStats":{"ver":1,"seq":1,"val":{"turns":…}}}`（task-17-brief.md:30/288），
        // 产品文案（进 availability.reason 的那句）一字未改。
        assert!(turn_semantics(UsageSourceId::Dsh).contains("sessionStats.val.turns"));
        assert!(turn_semantics(UsageSourceId::Codex).contains("task_started"));
    }

    /// 设置补丁：默认值 = **说明书 §P7**；越界补丁必须报 usage-settings-invalid
    #[test]
    fn settings_patch_validation() {
        let d = UsageSettings::default();
        assert_eq!(d.mini_bar_tool_rows, 3);
        assert_eq!(d.detail_retention_days, 90);
        assert_eq!(d.collect_interval_min, 10);
        assert_eq!(
            d.mini_bar_range,
            crate::services::usage::model::MiniBarRange::Today
        );
        let merged = merge_patch(
            &d,
            &UsageSettingsPatch {
                detail_retention_days: Some(30),
                export_quote: Some("今天也很努力".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(merged.detail_retention_days, 30);
        assert_eq!(merged.export_quote, "今天也很努力");
        assert_eq!(merged.mini_bar_tool_rows, 3, "未给的字段保持不变");
        for bad in [
            UsageSettingsPatch {
                detail_retention_days: Some(0),
                ..Default::default()
            },
            UsageSettingsPatch {
                collect_interval_min: Some(0),
                ..Default::default()
            },
            UsageSettingsPatch {
                mini_bar_tool_rows: Some(99),
                ..Default::default()
            },
            UsageSettingsPatch {
                provider_map_rules: Some("{not json".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                merge_patch(&d, &bad).unwrap_err().code,
                "usage-settings-invalid"
            );
        }
        // 坏 JSON 的设置整体回退默认（不 panic、不清库）
        assert_eq!(parse_settings(Some("{broken")), UsageSettings::default());
        assert_eq!(parse_settings(None), UsageSettings::default());
    }

    /// **FIX-6 锁（§3.2.3）**：lib 单测构建里 `CollectContext::new` 必须**内存注入空规则**。
    /// 若采集器侧回落到 `settings::load()`，lib 单测就会 ① 读/首次创建开发机真实
    /// `~/.tuvis/tuvis.db`，② 与本模块改全局 override 的用例在同二进制并行时互相踩。
    /// 手法：先塞一份**带规则**的 override（真读库的路径必然命中它），再断言新建 ctx 拿到的
    /// 仍是空表 —— 实现一旦退化成"总是读设置"，这里立刻红（这是本条的编译外运行期锁）。
    #[test]
    fn collect_context_never_reads_global_settings_for_provider_rules() {
        let _g = exclusive();
        settings::set_override_for_test(Some(UsageSettings {
            provider_map_rules: r#"{"rules":[{"prefix":"claude-","provider":"anthropic"}]}"#.into(),
            ..Default::default()
        }));
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(std::path::Path::new("/nonexistent"), 0, &no_cursors);
        assert!(
            ctx.provider_rules().is_empty(),
            "lib 单测构建必须由 CollectContext 注入空规则（不得经 settings::load() 读全局设置库）"
        );
        // 显式注入通道仍然可用：需要规则的单测走 `with_provider_rules(...)`
        let rules = crate::services::usage::provider::parse_provider_rules(
            r#"{"rules":[{"prefix":"m-","provider":"p"}]}"#,
        );
        // 复用同一个空游标表（`no_cursors` 是共享借用，可再借一次）
        let ctx2 = CollectContext::new(std::path::Path::new("/nonexistent"), 0, &no_cursors)
            .with_provider_rules(rules);
        assert_eq!(ctx2.provider_rules().len(), 1, "显式注入必须生效");
        settings::set_override_for_test(None);
    }

    /// **编译期契约（独立验证复核 §3.2.2 阻塞 1 的锁）**：DAO 读回的游标表类型必须与
    /// `collect_source` 的第 4 参类型**逐字相同**。旧计划的 DAO 侧另有 `CursorRow`
    /// （与 `CursorDelta` 两个 struct、无 `From`）→ 装配行 E0308，而当时**没有任何测试**
    /// 能提前抓住它（要等 `run_collection` 装配时才炸）。
    /// 下面这个闭包**永远不会被调用**：它存在的唯一意义是让编译器在这里对齐两个类型。
    #[test]
    fn dao_cursor_map_type_matches_collect_source_signature() {
        struct Noop;
        impl UsageCollector for Noop {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Claude
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                Ok(SourceDelta::default())
            }
        }
        // ① DAO 的全局包装必须返回 `HashMap<String, CursorDelta>`
        let _loader: fn(&str) -> HashMap<String, CursorDelta> =
            crate::database::dao::usage::load_cursors;
        // ② 该类型必须能**零转换**交给 `collect_source`（不同型即 E0308）
        let _never_called = |cursors: &HashMap<String, CursorDelta>| {
            let _ = collect_source(&Noop, std::path::Path::new("."), 0, cursors);
        };
    }

    /// **GC 5 锁（AGENTS.md 扫描预算契约）**：文件型源的 `SessionFileScan` 必须用
    /// **新 namespace `usage-<source>`**。同 ns 混存不同产物类型会让
    /// `FileParseCache::get_or_parse` 的 `Arc::downcast` 失配 → 该条目**每轮重新解析**
    /// （缓存静默失效，正是扫描预算契约要防的那件事），而且失配是静默的（只按未命中覆盖）。
    /// 锁三条：① 形态 = `usage-` + 落库 id；② 七个源互不重复；③ 与 `monitor/` 既有六个
    /// 生产 ns 不撞车。
    #[test]
    fn session_scan_namespace_is_usage_scoped_unique_and_collision_free() {
        // ① 形态：`usage-<UsageSourceId::db_id()>`（与 `adapter::TOOL_IDS` 同一套字符串）
        for s in UsageSourceId::ALL {
            assert_eq!(scan_namespace(s), format!("usage-{}", s.db_id()));
        }
        // ② 七个源两两不同（否则两个源的缓存条目会互相覆盖/失配）
        let mut ns: Vec<&str> = UsageSourceId::ALL
            .iter()
            .map(|s| scan_namespace(*s))
            .collect();
        let total = ns.len();
        ns.sort_unstable();
        ns.dedup();
        assert_eq!(ns.len(), total, "每个源的 namespace 必须唯一");
        // ③ 与既有生产 ns 不撞车（2026-10-03 实测清单；新增 ns 必须另起一族）
        for existing in [
            "claude-cwd",
            "claude-digest",
            "dsh-log",
            "workbuddy-tail",
            "kimi-wire",
            "codex-digest",
        ] {
            assert!(
                !UsageSourceId::ALL
                    .iter()
                    .any(|s| scan_namespace(*s) == existing),
                "不得复用既有 namespace（{existing}）——同 ns 混存会让 Arc::downcast 失配"
            );
        }
        // 采集器的用法就是 `const SCAN: SessionFileScan = SessionFileScan::new(scan_namespace(..))`：
        // 这里就地验一次它能在 `const` 上下文里建起来（`&'static str` 形态）
        const CLAUDE_SCAN: crate::monitor::session_scan::SessionFileScan =
            crate::monitor::session_scan::SessionFileScan::new(scan_namespace(
                UsageSourceId::Claude,
            ));
        // 取一次缓存不 panic（真读取路径由各采集器自己的用例覆盖）
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("wire.jsonl");
        std::fs::write(&f, "a\n").unwrap();
        let _v: Arc<Vec<String>> =
            CLAUDE_SCAN.parse(&f, |p| vec![std::fs::read_to_string(p).unwrap()]);
    }

    /// **W-15 锁**：L2 快速路（`(mtime,size)` 未变 → 零读取、游标行原样透传）必须在
    /// `CollectContext` 这一层可断言——只断言「值对了」看不出走的是哪条路。
    /// 手法：先真读一遍拿到正确代际，再把游标行里的**尾指纹改成假的**（(mtime,size) 仍与磁盘
    /// 相符 → 仍然命中快速路），然后读第二遍：
    ///   * 走快速路 → 零字节、游标行**逐字段原样**（那条假指纹被原样带回）；
    ///   * 快速路被删掉 → 落到「追加路」，`needs_rescan` 因指纹不符判「必须重扫」→ 全量重读
    ///     → `bytes_read` 涨、`rescan=true`、`next != prev`（三条断言同时红）。
    #[test]
    fn fast_path_reads_zero_bytes_and_passes_the_cursor_row_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("wire.jsonl");
        std::fs::write(&f, "a\nb\n").unwrap(); // 4 字节
        let empty = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), 0, &empty);
        let first = ctx1.read_incremental(&f, "k1").unwrap();
        assert!(first.rescan, "前提：首读必须走全量路");
        assert_eq!(ctx1.stats().bytes_read, 4, "前提：首读只付一次文件大小");
        // 造一条「指纹陈旧」的游标行（(mtime_ms, file_size) 与磁盘相符 → 命中快速路）。
        // 快速路的契约是**原样透传**（`stream.rs` 的 `next: p.clone()`）：它不校验指纹，
        // 于是这条陈旧指纹会被带回来——这正是可以拿来做判别器的观察点。
        let mut stale = first.next.clone();
        stale.fingerprint = "0".repeat(64);
        let mut cursors = HashMap::new();
        cursors.insert("k1".to_string(), stale.clone());
        let ctx2 = CollectContext::new(dir.path(), 0, &cursors);
        let second = ctx2.read_incremental(&f, "k1").unwrap();
        let s2 = ctx2.stats();
        assert_eq!(s2.reads, 1, "第二遍仍然算一次「读取请求」（入口被走到）");
        assert_eq!(s2.repeat_reads, 0, "不同 ctx 各读一遍不算重复读");
        assert_eq!(
            s2.bytes_read, 0,
            "未变文件必须零字节（W-15：这条才抓得住快速路）"
        );
        assert!(!second.rescan, "未变文件不得被判成重扫");
        assert!(second.lines.is_empty(), "未变文件不得再吐行");
        assert_eq!(second.next, stale, "快速路 = 游标行逐字段原样透传");
    }

    /// **W-17 锁**：`stream.rs` 的快速路返回 `next: p.clone()`，即**行键跟着 `prev.session_id`
    /// 走**，而不是入参 `session_id` —— 四条路径里唯一不看去参的一条。`CollectContext` 是
    /// 全域唯一的读入口，正是把这条「隐式保证」变成**显式**的地方：
    /// 游标表的键（`file_key`）与行内 `session_id` 必须恒等，否则损坏的键/值分叉会被原样带
    /// 下去（行永远写不回它自己那把键下）。
    #[test]
    #[should_panic(expected = "游标行的 session_id 必须等于游标表的键")]
    fn read_incremental_rejects_a_cursor_row_whose_key_diverged_from_its_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("wire.jsonl");
        std::fs::write(&f, "a\n").unwrap();
        let empty = HashMap::new();
        let first = CollectContext::new(dir.path(), 0, &empty)
            .read_incremental(&f, "k1")
            .unwrap();
        // 先把前提坐实：正常路径下两者本来就相等（否则本用例是在测一个不存在的分叉）
        assert_eq!(first.next.session_id, "k1");
        // 造一条「map 键 = k1、行内 session_id = 别的」的游标（库行损坏 / 未来某条路径忘了传 file_key）
        let mut diverged = first.next.clone();
        diverged.session_id = "other-key".to_string();
        let mut cursors = HashMap::new();
        cursors.insert("k1".to_string(), diverged);
        let ctx = CollectContext::new(dir.path(), 0, &cursors);
        // (mtime,size) 相符 → 走快速路 → 原样带回 `prev.session_id` = "other-key" → 断言触发
        let _ = ctx.read_incremental(&f, "k1");
    }

    /// **W-06 锁（前半）**：采集器返回 `Err` 必须落成「本源的 `ok=false` + `errorCode`」，
    /// **不得** panic、不得上抛、不得让它带走整轮；失败的源**不产出任何行**
    /// （宁可空，不可半写——D-16 的整源 Err 语义）。
    #[test]
    fn collector_error_becomes_source_status_with_code_and_default_delta() {
        struct Failing;
        impl UsageCollector for Failing {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Codex
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                Err(UsageError::new(
                    "usage-source-io",
                    "wire.jsonl: mtime unavailable",
                ))
            }
        }
        let no_cursors = HashMap::new();
        let (status, delta, stats) = collect_source(
            &Failing,
            std::path::Path::new("/nonexistent"),
            0,
            &no_cursors,
        );
        assert!(
            !status.ok,
            "采集 Err 必须落成 ok=false（否则 UI 上看到的是「这个源没有用量」）"
        );
        assert_eq!(
            status.error_code.as_deref(),
            Some("usage-source-io"),
            "错误码必须进 status（W-06：不能只 log::warn!）"
        );
        assert_eq!(status.source_id, UsageSourceId::Codex);
        assert_eq!(status.parsed_files, 0);
        assert_eq!(status.new_records, 0);
        // `SourceDelta` 没有 `PartialEq`（Task 9 的形状，不去动它）→ 逐字段断言「空产物」
        assert!(delta.details.is_empty(), "失败的源不产出任何明细行");
        assert!(delta.sessions.is_empty());
        assert!(
            delta.cursors.is_empty(),
            "失败的源不得推进任何游标（下轮重扫该源）"
        );
        assert_eq!(delta.parsed_files, 0);
        assert_eq!(delta.new_records, 0);
        assert_eq!(stats.reads, 0);
        assert_eq!(stats.repeat_reads, 0);
        assert_eq!(stats.bytes_read, 0);
    }

    /// **W-06 锁（后半）**：一源的 `collect` 失败、另一源的 `apply_delta` 失败，都必须被当作
    /// 「本源的 ok=false + errorCode」记进 `UsageSourceStatus`，**然后继续下一源**
    /// （不得 `unwrap` / `?` 上抛 / 卡死循环）——否则确定性毒行会把其余六个源连带采不到数。
    /// 经可注入内核跑三个替身：**零 DB、零真实采集器**。
    #[test]
    fn source_failures_are_isolated_and_the_loop_still_reaches_later_sources() {
        struct Healthy {
            records: i64,
        }
        impl UsageCollector for Healthy {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Claude
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                Ok(SourceDelta {
                    parsed_files: 1,
                    new_records: self.records,
                    ..Default::default()
                })
            }
        }
        struct CollectFails;
        impl UsageCollector for CollectFails {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Codex
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                Err(UsageError::new("usage-source-io", "poisoned row"))
            }
        }
        struct ApplyFails;
        impl UsageCollector for ApplyFails {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Kimi
            }
            fn collect(&self, _ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                // 采集成功（真的解析出了 5 条），但落库会失败（磁盘满 / 库损坏）
                Ok(SourceDelta {
                    parsed_files: 2,
                    new_records: 5,
                    ..Default::default()
                })
            }
        }
        let collectors: Vec<Box<dyn UsageCollector>> = vec![
            Box::new(Healthy { records: 3 }),
            Box::new(CollectFails),
            Box::new(ApplyFails),
        ];
        let load_cursors = |_: &str| HashMap::new();
        let apply = |sid: UsageSourceId, _: &SourceDelta| {
            if sid == UsageSourceId::Kimi {
                Err(UsageError::new(
                    "usage-db-failed",
                    "database or disk is full",
                ))
            } else {
                Ok(())
            }
        };
        let (sources, total) = collect_sources(
            true,
            0,
            std::path::Path::new("/nonexistent"),
            collectors,
            &load_cursors,
            &apply,
        );
        assert_eq!(
            sources.len(),
            3,
            "循环必须走完三个源（单源失败不得中断/短路）"
        );
        assert_eq!(sources[0].source_id, UsageSourceId::Claude);
        assert!(sources[0].ok);
        assert_eq!(sources[0].error_code, None);
        assert_eq!(sources[0].new_records, 3);
        assert_eq!(sources[1].source_id, UsageSourceId::Codex);
        assert!(!sources[1].ok);
        assert_eq!(sources[1].error_code.as_deref(), Some("usage-source-io"));
        assert_eq!(
            sources[2].source_id,
            UsageSourceId::Kimi,
            "源 id 不得被换掉"
        );
        assert!(!sources[2].ok);
        assert_eq!(
            sources[2].error_code.as_deref(),
            Some("usage-db-failed"),
            "落库失败的码来自 apply_delta，而不是被吞掉"
        );
        assert_eq!(
            sources[2].parsed_files, 2,
            "解析确实做过，parsed_files 如实上报（失败的是落库那一段）"
        );
        assert_eq!(
            sources[2].new_records, 0,
            "落库失败 = 一行都没进账本（D-16 整批回滚），不得报「预计写入」的行数"
        );
        assert_eq!(total, 3, "总数只累计真正落库成功的源");
    }

    /// **W-15 经装配路径**：`collect_source` 必须把该源真实的 `reads / repeat_reads /
    /// bytes_read` 交出来（采集器用例断言 `repeat_reads == 0` 靠的就是它），
    /// 且正常源 `ok=true`、无错误码。
    #[test]
    fn collect_source_reports_the_stats_of_one_pass_over_files() {
        struct Reader {
            path: PathBuf,
        }
        impl UsageCollector for Reader {
            fn source_id(&self) -> UsageSourceId {
                UsageSourceId::Claude
            }
            fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
                let r = ctx
                    .read_incremental(&self.path, "k1")
                    .map_err(|e| UsageError::new("usage-source-io", e.to_string()))?;
                Ok(SourceDelta {
                    parsed_files: 1,
                    new_records: r.lines.len() as i64,
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("wire.jsonl");
        std::fs::write(&f, "a\nb\n").unwrap();
        let no_cursors = HashMap::new();
        let (status, delta, stats) =
            collect_source(&Reader { path: f.clone() }, dir.path(), 0, &no_cursors);
        assert!(status.ok);
        assert_eq!(status.error_code, None);
        assert_eq!(status.parsed_files, 1);
        assert_eq!(status.new_records, 2);
        assert_eq!(delta.new_records, 2);
        assert_eq!(stats.reads, 1, "一遍扫描：每个候选文件只读一遍");
        assert_eq!(stats.repeat_reads, 0, "采集器用例断言为 0 的那条纪律");
        assert_eq!(
            stats.bytes_read, 4,
            "4 字节文件、一遍扫描 = 4 字节（不是 0、不是 8）"
        );
    }

    /// **判据面由共享实现给出**（第 5 轮：用**显式哨兵标记**，删掉全部「结构推断」）。
    ///
    /// 历史与完整理由见 `crate::services::usage::self_lock` 的模块文档；这里只留**本锁**要用的：
    /// * 判据面 = `self_lock::scan_face(文件名, 源码)` = 文件头 → **排除区 BEGIN** 之前，
    ///   **加上** END 之后 → 文件尾（去行内空白、剥**整行** `//` 注释）；
    /// * 34 个文件**每个都必须恰好一对标记**：**28 个有测试模块**的文件把标记**包住测试模块**
    ///   （BEGIN 在 `#[cfg(test)]` 前一行、END 在测试模块收口 `}` 之后）；**6 个无测试模块**的文件
    ///   （`caps.rs` / `delta.rs` / `model.rs` / `v2/model.rs` / `v2/mod.rs`）
    ///   把两行**相邻**放在文件末尾 ⇒ 排除区为空、**整文件都在面内**（第 6 轮：`mod.rs` 因新增
    ///   `self_lock_tests` 由空区转为测试模块区；第 42 轮：`v2/model_config.rs` 以**测试模块区**
    ///   形态入面）；**分类由
    ///   `usage_source_scan_covers_every_file_on_disk` 机械钉住（28/6）**；
    /// * 标记缺失 / 重复 / 顺序反了 / 面为空 ⇒ `scan_face` **panic**（响亮失败，不许静默退化）。
    ///
    /// **为什么不再用「锚点 + 大括号配平到 EOF」**：那是**用文本启发式推断 Rust 结构**，五轮里
    /// 每一轮都被更早的假锚点骗过——前缀命中的 `mod tests_of_line_splitting`、多行字符串里的
    /// `#[cfg(test)]` / `mod tests {`、**块注释里的同款两行**；而且**盲区永远是 `[锚点, EOF)`**：
    /// 「收口校验」管不到锚点与收口点之间装了什么，而**生产代码可以写在测试模块之后**（Rust 不看
    /// 声明顺序）。第 4 轮判据正是在那里漏掉两条构造（回归追加在文件末尾 ⇒ 落在面外 ⇒ 假绿）。
    ///
    /// 注意：`commands/usage.rs` / `commands/export.rs` 里同名的 `prod_only` 仍按**首个
    /// `#[cfg(test)]` 字面量**截断（**两轮均按指令未改**，见台账 C-【Important-1】第 3 行）——
    /// 那两个文件的首个 `#[cfg(test)]` **就是**紧邻 `mod tests` 的真属性 ⇒ 截断区等于生产半边
    /// 全部、当下无盲区；但那套启发式的弱点在那里**依然存在**（同族风险未消除、只是当前无实例）。
    /// **实现纪律的源码级自省锁（spec §P6 改写后 / Q-4 裁决 / A-1）**：`services/usage/` 的
    /// **生产半边**里，**没有任何地方把源时区喂进键函数**，且命名时区机器**不得回到本域**。
    ///
    /// 为什么行为锁不够：`collectors::codex::tests::detail_hour_key_is_host_local_even_when_source_timezone_is_present`
    /// 只钉住 **codex 一路**（它是七源里唯一读过命名时区的源）。后来者「修回去」更可能的形态是
    /// 在 `range.rs` 重造一个**带 tz 参数**的键函数、或让**别的源**也去读时区——那两条都不经过
    /// codex 的用例。本锁把裁决的**结构面**钉死：键函数**只有宿主本地单参形态**。
    ///
    /// **能变红**（三支）：
    /// ① 判据面里写回 `SourceTz` / `chrono_tz` / `from_name(` → 反向断言红；
    /// ② 重造带参键函数（出现 `hour_key_of(` / `day_key_of(` 而非 `…_host(`）→ 反向断言红
    ///    （这是「把源时区喂进键函数」**最省事的可行形态**；换名成 `bucket_of(ts, tz)` 同样可行，
    ///    见反例①——故此处**不写"唯一"**：本判据是名字级，只认名字形态）；
    /// ③ 把宿主本地键入口删光 → 正向断言红（防「本域压根没有键函数」时反向全绿的假绿）。
    ///
    /// 针一律 `concat!` 拆开写：否则本用例自己的字面量就会命中自己（恒真/恒假）。
    ///
    /// **扫描面 / 判据层级 / 反例**（台账 C-【Important-1】的登记原文，此处为权威副本；
    /// **第 5 轮按「显式哨兵」口径重写**）：
    /// * **扫描面** = `USAGE_SOURCES` 全部 **34** 个文件（与磁盘**逐一相等**见
    ///   `usage_source_scan_covers_every_file_on_disk`）的**判据面**（`self_lock::scan_face`：
    ///   文件头 → BEGIN 之前 **∪** END 之后 → 文件尾；去行内空白、剥**整行** `//` 注释）。
    ///   **实测（34 个文件逐一对账）**：28 个有测试模块的文件 = **生产半边**（标记包住测试模块）、
    ///   6 个无测试模块的文件 = **整文件**（两行标记相邻于末尾 ⇒ 排除区为空）；
    ///   判据面合计 = **11506 行 / 255773 字符**（第 40 轮手工测试回归，见
    ///   `scan_face_size_is_pinned`；相对第 39 轮 +20 行 / +446 字符，变化来自
    ///   `v2/cost.rs` 的 `safe_components_output_only` 与 `v2/cost_query.rs` 的 dsh 日档
    ///   output-only 分支；文件数与 27/6 分类未变、排除区标记未动。第 39 轮为
    ///   11486 行 / 255327 字符（+12 行 / +263 字符：`parse_catalog_rate` 目录坏费率逐条
    ///   降级），第 38 轮为 11474 行 / 255064 字符（+65 行 / +1743 字符：
    ///   `PricingCatalogEntry`、下载超时与目录搜索入口），第 37 轮 11409/253321。
    ///   第 4 轮口径曾报 191434 字符：那 8 个「整文件扫描」
    ///   的文件把测试代码也算进去了。本轮的"面"更小但**更准**——26 个文件 = 生产半边、6 个无测试
    ///   模块的文件 = 整文件，且 **END 之后追加的生产代码也在面内**。数字同步写进本批 commit
    ///   message 与 `FINAL-FIX-report.md`，并由 `scan_face_size_is_pinned` **逐字钉住**）。
    /// * **枚举口径**（B-1）：`usage_source_scan_covers_every_file_on_disk` 递归枚举该目录下的
    ///   `.rs`（**大小写不敏感** —— 本机 `rustc` 能从 `foo.RS` 编译 `mod foo;`）。**不在枚举面内**
    ///   的承载形态：`include!` 的 `.inc`、`include_str!` 引入的外部文本，以及 **`#[path = "…"]`
    ///   把生产代码挂到 `services/usage/**` 之外**（B-2：与 `include!` 同族，**只登记不解析**——
    ///   真去解析 `#[path]` 会引出无穷的逃逸面）。
    /// * **判据层级** = **名字级**（针是 API 名 / 函数名形态，**不是行为性质**）。
    /// * **正向断言的作用域 = 33 个文件判据面的拼接串（并集语义，B-4）**：`hour_key_of_host(` /
    ///   `day_key_of_host(` / `tz_name` 只要**任一**文件面内有即过 ⇒「删掉某个文件的全部调用点」
    ///   **不会变红**（单文件级存在性没有被钉住；要钉得分文件断言——本批按现状**只登记不改**）。
    /// * **反例（抓不到的回归）** = ①换名字重造带 tz 的键函数（如 `bucket_of(ts, tz)`）；
    ///   ②**行尾注释**里写针（只剥**整行** `//` 注释；`/* */` 块注释与行尾注释一律**不剥**）
    ///   或用字符串拼接绕过针；③把键计算挪进**排除区内的测试代码**（判据面**故意**含
    ///   `#[cfg(test)]` **非模块项**，但被标记包住的测试模块内部不在面内）；
    ///   ④**「同名加参」**（P1-1：把 `day_key_of_host(ts_ms)` 改成 `day_key_of_host(ts_ms, tz)`）
    ///   ——第 7 轮已修：**元数断言 + 签名逐字钉**（见 `arity_sites` 与签名断言）；
    ///   ⑤**蓄意改写锁面标记**——把 BEGIN 上移 / 把 END 下移（排除区扩大、盖住生产代码）：
    ///   见 `self_lock` 的威胁模型声明，**本锁防的是顺手回归与无意的习惯性改回，不防这个**。
    /// * **第 4 轮的四条致盲构造在哨兵判据下「按构造已失效」**：`R4_FAKE`（块注释假锚点）/
    ///   `R4b_FAKE_STR`（多行字符串假锚点）/ `R8_COLLECT`（**零注入** + EOF 追加回归）/
    ///   `R8_STREAM`（测试函数体内 `let stray_json = "{"` + EOF 追加回归）——假锚点如今只是**普通
    ///   文本**（判据不再认任何锚点），EOF 追加落在「**END 之后**」⇒ **在面内**；「收口」这个概念
    ///   本身已被删除，所以「把收口推迟到 EOF」也无从谈起。实测：四条**旧判据 ok（假绿）→ 新判据
    ///   FAILED**（见 `FINAL-FIX-report.md`「第 5 轮」）。
    /// * **R9（注释切针 = fmt 自毁）**：`pub fn day_key_of/**/(ts_ms: i64, tz_name: &str)` 这种把针
    ///   从中间切开、用空注释隔断的写法，**本判据抓不到**（`day_key_of/**/(` 里没有 `day_key_of(`），
    ///   但 `cargo fmt` 会把注释并回去 ⇒ **活不过一次格式化**（复审实测：探针 ok、`cargo fmt --check`
    ///   报 1）。登记它有两层意思：既不假装判据能抓它，也不依赖 fmt 当兜底。
    /// * **反例（会误报的情形）** = ⑥**排除区外**的测试专用项 / 字符串里的针 ⇒ **假阳性**
    ///   （生产零改动也红；**响亮且安全**，刻意保留）；⑦`opencode` 那把锁的 5 行窗口
    ///   （整行注释不占槽、**空行占槽**）另见该处登记。
    /// * **结论句**：真正的**性质级**保证是**行为锁**
    ///   `collectors::codex::tests::detail_hour_key_is_host_local_even_when_source_timezone_is_present`
    ///   （26 小时差的跨时区行为锁）。本锁只是「**结构面 + 名字级机械纪律**」——防**顺手改回去**、
    ///   防**新增文件逃逸**；**不要把本锁当它的替代品**。
    #[test]
    fn no_source_timezone_is_fed_into_any_key_function() {
        let code: String = USAGE_SOURCES
            .iter()
            .map(|(name, src)| {
                crate::services::usage::self_lock::scan_face(name, src)
                    .into_iter()
                    .map(|(_, line)| line)
                    .collect::<String>()
            })
            .collect();
        // 反向 ①：命名时区机器不得回来（A-1 已把它与 `chrono-tz` 依赖一起移除）
        for forbidden in [
            concat!("Source", "Tz"),
            concat!("chrono", "_tz"),
            concat!("chrono", "-tz"),
            concat!("from_", "name("),
        ] {
            assert!(
                !code.contains(forbidden),
                "生产半边不得出现 `{forbidden}`：各源时区**可读入备查、不得参与键计算**\
                 （spec §P6 / 2026-10-05 用户裁决）。「读得到却不用」是**有意**的，\
                 后来者不得把它当 bug 修回去"
            );
        }
        // 反向 ②：键函数**只有宿主本地单参形态**——出现带参形态即有人在给键函数喂时区
        for forbidden in [concat!("hour_key", "_of"), concat!("day_key", "_of")] {
            // **针不带括号**（第 6 轮 B-1）：带括号只能抓**调用**形态，别名 / 函数指针
            // （`let f: fn(i64, &str) -> String = day_key_of; f(0, ..)`）会整个漏掉。
            // 去掉括号 + 词边界后：`hour_key_of_host(` 自动豁免（其后一字符是 `_`）——那正是
            // **允许**的宿主本地形态；而 `day_key_of(` / `day_key_of;` / `day_key_of/**/(` 全部命中。
            let hit = code.match_indices(forbidden).find(|(at, _)| {
                let after = code[(at + forbidden.len())..].chars().next();
                !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            assert!(
                hit.is_none(),
                "生产半边出现了 `{forbidden}`（判据面第 {} 字节起）：键函数只允许**宿主本地单参**\
                 形态（`hour_key_of_host` / `day_key_of_host`）——带 tz 参数的键函数就是\
                 「源时区参与键计算」，会重造窗口键与落库键的两套时钟（R-32/W-14 已随裁决消除）。\
                 针**不带括号**：别名 / 函数指针（`let f = day_key_of;`）同样命中",
                hit.map(|(at, _)| at).unwrap_or(0)
            );
        }
        // 正向：宿主本地入口必须在场（否则上面两条会因为「本域压根没有键函数」而假绿）
        for required in [
            concat!("hour_key_of", "_host("),
            concat!("day_key_of", "_host("),
        ] {
            assert!(
                code.contains(required),
                "生产半边必须仍有 `{required}`：这是本域**唯一**的键入口，\
                 缺了它上面两条反向断言就退化成恒真"
            );
        }
        // ── P1-1（第 7 轮）：`*_of_host` 家族**只允许宿主本地单参形态** ──
        // 反向 ② 的「去括号 + 词边界」只看后一字符 ⇒ `day_key_of_host(ts, tz)` 的后一字符是 `_`，
        // 被当标识符字符**豁免** ⇒「**同名加参**」整个免疫（复审实测：改一个已有函数的签名、加一个
        // tz 参数，锁全绿）；而「改一个已有函数的签名」恰恰是本锁立项时要防的「后来者修回去」
        // **最省事**的形态。下面两条把它钉死：**参数元数** + **签名逐字**。
        let mut arity_sites = 0usize;
        for name in [
            concat!("hour_key_of", "_host("),
            concat!("day_key_of", "_host("),
        ] {
            for (at, _) in code.match_indices(name) {
                let args_at = at + name.len();
                let mut depth = 1i32;
                let mut j = args_at;
                while j < code.len() && depth > 0 {
                    match code.as_bytes()[j] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                assert!(
                    depth == 0,
                    "`{name}` 在判据面里找不到配对括号（判据面第 {} 字节起）——签名/调用被写坏了，\
                     直接红，不许猜",
                    at
                );
                let args = &code[args_at..j - 1];
                arity_sites += 1;
                assert!(
                    !args.contains(','),
                    "`{name}` **只允许宿主本地单参形态**；出现第二个参数 = 有人把源时区喂进了\
                     键函数（P1-1「同名加参」）。命中：`{name}{args})`。\
                     修法：键函数只吃时间戳（宿主本地时钟），源时区**可读入备查、不得参与键计算**"
                );
            }
        }
        assert!(
            arity_sites >= 2,
            "判据面只找到 {arity_sites} 处 `*_of_host(`（应 ≥ 2：`hour_key_of_host` 与 \
             `day_key_of_host` 的定义各一处）——元数断言在「零命中」时会退化成恒真"
        );
        // 签名**逐字**钉住（P1-1 ③）：只钉「名字在场」时，改签名（加参数）照样能过；把签名本身
        // 写进判据面，才有「宿主本地单参」这句话的机械版本。
        for sig in [
            concat!("pubfn", "hour_key_of_host(ts_ms:i64)->String{"),
            concat!("pubfn", "day_key_of_host(ts_ms:i64)->String{"),
        ] {
            assert!(
                code.contains(sig),
                "判据面必须逐字含 `{sig}`：键函数的定义只允许这**一个**签名（宿主本地单参）——\
                 签名变了（加 tz 参数 / 改返回类型）就是「源时区参与键计算」（P1-1），\
                 或有人在绕过本锁的名字级判据"
            );
        }
        // 正向（spec §P6「可读入备查」的另一半，不得被顺手删掉）：codex 仍必须把源时区
        // 读进续读状态——删掉它就把「可读入备查」也一起做没了（与反向 ① 互为约束）。
        assert!(
            code.contains(concat!("tz_", "name")),
            "codex 生产半边必须仍把 `turn_context.timezone` 读进续读状态备查\
             （spec §P6：可读入备查**但**不得参与键计算；两半都要在）"
        );
    }

    /// 自省锁的**扫描面**：`services/usage/**/*.rs` **全部 37 个文件**。
    /// 与磁盘**逐一对账**见 `usage_source_scan_covers_every_file_on_disk`（防第 38 个文件静默逃逸）。
    const USAGE_SOURCES: [(&str, &str); 37] = [
        ("caps.rs", include_str!("caps.rs")),
        ("collect.rs", include_str!("collect.rs")),
        ("collectors/claude.rs", include_str!("collectors/claude.rs")),
        ("collectors/codex.rs", include_str!("collectors/codex.rs")),
        ("collectors/dsh.rs", include_str!("collectors/dsh.rs")),
        ("collectors/kimi.rs", include_str!("collectors/kimi.rs")),
        ("collectors/mod.rs", include_str!("collectors/mod.rs")),
        (
            "collectors/opencode.rs",
            include_str!("collectors/opencode.rs"),
        ),
        (
            "collectors/workbuddy.rs",
            include_str!("collectors/workbuddy.rs"),
        ),
        ("collectors/zcode.rs", include_str!("collectors/zcode.rs")),
        ("cursor.rs", include_str!("cursor.rs")),
        ("dedup.rs", include_str!("dedup.rs")),
        ("delta.rs", include_str!("delta.rs")),
        ("error.rs", include_str!("error.rs")),
        ("ledger.rs", include_str!("ledger.rs")),
        ("mod.rs", include_str!("mod.rs")),
        ("model.rs", include_str!("model.rs")),
        ("project.rs", include_str!("project.rs")),
        ("provider.rs", include_str!("provider.rs")),
        ("query.rs", include_str!("query.rs")),
        ("range.rs", include_str!("range.rs")),
        ("semantics.rs", include_str!("semantics.rs")),
        ("settings.rs", include_str!("settings.rs")),
        ("stream.rs", include_str!("stream.rs")),
        ("v2/billing.rs", include_str!("v2/billing.rs")),
        ("v2/channel.rs", include_str!("v2/channel.rs")),
        ("v2/cost.rs", include_str!("v2/cost.rs")),
        ("v2/cost_query.rs", include_str!("v2/cost_query.rs")),
        ("v2/currency.rs", include_str!("v2/currency.rs")),
        ("v2/identity.rs", include_str!("v2/identity.rs")),
        ("v2/model.rs", include_str!("v2/model.rs")),
        ("v2/model_config.rs", include_str!("v2/model_config.rs")),
        ("v2/mod.rs", include_str!("v2/mod.rs")),
        ("v2/native_price.rs", include_str!("v2/native_price.rs")),
        ("v2/pricing.rs", include_str!("v2/pricing.rs")),
        ("v2/query.rs", include_str!("v2/query.rs")),
        ("v2/time_price.rs", include_str!("v2/time_price.rs")),
    ];

    /// **面尺寸钉子（第 7 轮 C-1）**：把「判据面 = N 行 / M 字符」这个数字**逐字钉住**。
    ///
    /// 只在注释里写数字 ⇒ 每轮都会腐烂（第 5 轮写的 6804 行 / 146908 字符，到第 6 轮加完
    /// `self_lock` 就陈旧了，且**没有任何用例会发现**）。数字变了 = **判据面变了**，
    /// 必须同步本常量、锁注释、台账与报告 —— 这是「**声明 = 实际**」的机械版。
    /// 口径：`chars().count()`（**Unicode 标量 / 字符**，不是字节；与历次报告口径一致）。
    ///
    /// **第 46 轮同步（2026-10-20 原生人民币价/当日汇率）**：12422/277406 →
    /// **13447/301286**（+1025 行 / +23880 字符）。变化来自新增 `v2/native_price.rs`、
    /// `v2/currency.rs` 与 `v2/{model,cost_query,pricing,mod}.rs` 的生产半边；两个新文件
    /// 都有测试模块 ⇒ 分类 28/6 → **30/6**，扫描面 34 → **36** 文件。后续 clippy 修正
    /// （`#[allow(too_many_arguments)]`、`needless_range_loop`、`needless_question_mark`）
    /// 与「单分量 FX 溢出降级」修复已并入该轮钉子。
    ///
    /// **第 47 轮同步（2026-10-20 第三轮配置内嵌化）**：13447/301286 →
    /// **13451/301445**（+4 行 / +159 字符）。变化全部来自
    /// `v2/model.rs` 的 `ObservedModelEntry.normalized_model_id` 字段与
    /// `v2/model_config.rs` 的 `normalize_model_id` 填充；新增测试在排除区内 ⇒ 不进面。
    ///
    /// **第 8 轮同步（Task 16 Step 3，2026-10-03 真机数据隔离守卫）**：6870/148415 →
    /// **6896/149006**（+26 行 / +591 字符）。变化**全部来自 `collect()` 与 `run_collection()`
    /// 生产函数体内的两处 `#[cfg(test)]` **采集拒绝闸**（无 fixture 根 ⇒ 响亮拒绝）；
    /// 新增的 fixture 根存取口与用例落在 BEGIN..END 排除区内 ⇒ **不进面**。
    /// 钉子只同步、不放宽：本用例的断言仍是逐字相等。
    ///
    /// **第 9 轮同步（2026-10-06 用户裁决变更波）**：6896/149006 → **6904/149235**
    /// （+8 行 / +229 字符）。变化**全部来自 `query.rs` 的生产半边**：`recent_session_with` 的
    /// 文档注释与函数体内的说明改写了（「候选必须限定在**本期量到过请求输入**的会话」——即
    /// `request_total > 0`，并写明真空回合的成因）；新增的两条用例落在 BEGIN..END 排除区内 ⇒
    /// **不进面**。面**没有新增文件**、排除区标记未动、其余 23 个文件逐字未改。
    ///
    /// **第 10 轮同步（2026-10-06 清理过期「19/99」）**：6904/149235 → **6904/149263**
    /// （**+0 行 / +28 字符**）。行数**没变**是预期：本轮的 `caps.rs` / `dsh.rs` 改动**全是整行
    /// `//` 注释**（判据面**剥整行注释** ⇒ 既不计行也不计字符），而 `collect.rs` 自己那处改在
    /// BEGIN..END 排除区内；**字符 +28 全部来自 `query.rs` 生产半边里那条 `LongestTurn` 的
    /// `reason` 字符串**（把「覆盖率写死成一个比例」改成「覆盖率随机器与清理策略变化，
    /// 2026-10-06 实测账本口径已接近全量」）。面**没有新增文件**、
    /// 排除区标记未动、其余 22 个文件逐字未改。
    ///
    /// **第 11 轮同步（2026-10-06 CSV 公式注入中和）**：6904/149263 → **6913/149429**
    /// （**+9 行 / +166 字符**）。变化**全部来自 `query.rs` 生产半边里的 `csv_escape` 一个函数体**
    /// （**7 行 → 16 行**，与实测 **+9 行**逐字对上）：
    ///   * 新增 `let neutralize = matches!(…四种前缀…)` —— **4 行**（`matches!` 的多行参数表
    ///     由 rustfmt 固定成 4 行）；
    ///   * 新增 `let body = if neutralize { … } else { … };` —— **5 行**（其中 **4 行是
    ///     `cargo fmt` 的规范化产物**：手写时它是一行、`cargo fmt --check` 要求把它展开成块；
    ///     这也是「先 6909、后 6913」的原因，**字符数两次都是 149429** —— 面按 `!is_whitespace`
    ///     过滤，换行/缩进不进字符数）；
    ///   * 原来的两处 `s.contains(…)` / `s.replace(…)` 改成 `body.…`（**0 行**，只换字符）。
    ///
    /// **归属已实证**：把 `csv_escape` 的函数体单独还原成旧版（7 行原样）后，本钉子**回到
    /// 6904/149263 且通过**——即这 +9/+166 **没有第二处来源**（§8 的 `error.rs` 改动只有**整行
    /// `//` 注释**与**行尾注释的对齐空白**变化——判据面剥空白 ⇒ 面尺寸不变；本轮新增的用例全在
    /// BEGIN..END 排除区内）。面**没有新增文件**、
    /// 排除区标记未动、其余 23 个文件逐字未改。
    ///
    /// **第 12 轮同步（2026-10-06 `csv_escape` 中和集补 `\n`）**：6913/149429 → **6913/149440**
    /// （**+0 行 / +11 字符**）。变化**全部来自 `query.rs` 生产半边里 `csv_escape` 的
    /// `matches!(…)` 多一个分支 `| Some('\n')`**（**11 字符**逐字对上，行数不变是因为该参数表
    /// 本来就是一行的多行调用、加一个分支仍在同一行内 ⇒ rustfmt 不重排）。**归属**：第 12 轮
    /// 改的另外三处（`commands/usage.rs` 的锁与辅助函数、两处文字计数）**都不在面内**
    /// （前者在 `#[cfg(test)] mod tests` 里；`collect.rs` 的机制句在 BEGIN..END 排除区内；
    /// 前端测试文件根本不在面内）。面**没有新增文件**、排除区标记未动、其余 23 个文件逐字未改。
    ///
    /// **第 13 轮同步（2026-10-06 分布维度合并为「供应商 / 模型」）**：6913/149440 → **6917/149472**
    /// （**+4 行 / +32 字符**）。变化**全部来自 `query.rs` 生产半边**的三处，且只有这三处：
    ///   * **新增私有 `route_label`**（生产行 **7 行 + 1 空行**）——「供应商 / 模型」的**唯一实现**；
    ///   * `group_of` 的 `UsageGroupBy::Model` 分支：原来的**一行**改成**四行块**（`=> {` /
    ///     `let label = …` / `(label.clone(), label, …)` / `}`）；
    ///   * `records_with_conn` 卡内行：原来**内联的 9 行 if/else** 删掉，换成**一行**
    ///     `let (k, label) = (route_label(r), route_label(r));`。
    ///
    /// 净行数 = 8 + 4 − 1 − 1 − 9 = **+1 代码行 + 1 空行 = +4**（与实测逐字对上）；字符数 +32 是
    /// 三者相抵后的净值（新增的 `route_label` 与 `format!` 被删掉的那份重复 `format!` 抵掉了大半）。
    ///
    /// **归属**：本轮的另外几处改动都不在面内——`query.rs` 的模块头说明与函数内注释是
    /// **整行 `//`**（判据面剥掉，**不进面**）、新增用例在 BEGIN..END 排除区内；
    /// 前端（`UsageDistributionCard.tsx` / `i18n` / 测试）根本不在面内。
    /// 面**没有新增文件**、排除区标记未动、其余 23 个文件逐字未改。
    ///
    /// **第 14 轮同步（2026-10-07 dsh 改「按事件发生时间」入账）**：6917/149472 → **7234/156289**
    /// （**+317 行 / +6817 字符**）。变化**只来自两个文件的生产半边** —— 无新增文件、
    /// 排除区标记未动、其余 22 个文件逐字未改：
    ///   * `collectors/dsh.rs`：**+309 行 / +6627 字符** —— 逐事件归属这条链的全部生产代码：
    ///     新增 `EventUsage` / `usage_events_of` / `EventTailPlan` / `event_watermark_of` /
    ///     `event_tail_plan` / `EventHourAgg` / `PendingBackfill` / `backfill_buckets` /
    ///     `emit_backfill`；`LogDigest` 多两个字段（`text_len` / `usage_events`）并在
    ///     `LogDigest::of` 里填上；A 段改写（**不再**写四桶与请求数，改为把差额 `D` 挂账）；
    ///     B 段新增逐事件归属块与 `D − E` 对账（补录目标按**已记录水位 R** 算）；日志侧游标由
    ///     「`byte_offset` 一律 0」改为**两维真实水位**（解压正文字节 + 已消费事件条数），
    ///     并加「老游标（`byte_offset`/`ordinal` 一律 0 且没有 R）不得当成有效水位」这道闸；
    ///   * `semantics.rs`：**+8 行 / +190 字符** —— `CacheSemantics` 新增 `Backfill` 变体
    ///     （补录来源标记）+ `as_db`/`from_db` 各多一个分支 + `is_backfill()`，`normalize`
    ///     多一条「拒绝把来源标记当语义用」的显式分支。
    ///
    /// 两处增量都是**可计代码行**：判据面剥掉整行 `//` 注释（**含 `///` 文档注释**），
    /// 本轮新写的数百行文档注释因此**不进面**。`collect.rs` 生产半边**零改动** ——
    /// 日聚合由 `ledger::daily_rows_of` 从明细行折叠，落库路径不需要跟着改。
    ///
    /// **第 15 轮同步（2026-10-13 集成测试采集读源缝 `TUVIS_USAGE_FIXTURE_ROOT`）**：
    /// 7234/156289 → **7239/156516** → 7239/156596 → 7238/156598（PR #158 N5 detail 机器化 +80 字符；cargo fmt 合并多行 format! −1 行/+2 字符）。变化**只来自 `collect.rs`
    /// 生产半边**的 `run_collection` 扫描根解析块——`#[cfg(not(test))]` 单臂换成三分支
    /// cfg（test 保持 lib 拒绝闸半边原样 / ¬test∧debug_assertions 读环境变量覆盖 /
    /// 其余构建原样 `dirs::home_dir()`）；函数其余逐字未改，无新增文件、排除区标记未动、
    /// 其余 23 个文件逐字未改。集成测试半边的行为锁在
    /// `tests/real_db_isolation_test.rs::usage_fixture_root_env_is_inside_the_debug_gate`
    /// （源码针）与 `tests/usage_ipc_test.rs` / `usage_two_round_incremental_test.rs`（实跑）。
    ///
    /// **PR #158 四轮同步**：7238/156598 → **7241/156998**。用量固定 detail 英文化
    /// （含 ledger::internal）；dsh 的 format! 经 rustfmt 展开增加 3 行，排除区未移动。
    ///
    /// **第 26 轮同步（2026-10-18 V2 定价引用校验）**：9972/218543 →
    /// **10014/219561**。`billing.rs` 新增 candidate pricingRules 的 active catalog 校验
    /// （文件数与 26/6 分类不变）。
    ///
    /// **第 27 轮同步（2026-10-19 PR #170 评审修复）**：10014/219561 →
    /// **10078/220847**。`v2/pricing.rs` 新增进程级目录缓存与 `RawValue` 费率解析
    /// （去掉 crate 级 `arbitrary_precision`），`v2/cost_query.rs` 目录类型改为 `Arc`；
    /// 文件数与 26/6 分类不变，排除区标记未动。
    ///
    /// **第 28 轮同步（2026-10-19 PR #170 评审修复二批）**：10078/220847 →
    /// **10120/221775**。`v2/pricing.rs` 刷新链路重构为共享内核 `run_refresh` +
    /// `RefreshHost` trait（测试内核与生产入口共享，fetch 与 emit 注入）；
    /// 文件数与 26/6 分类不变，排除区标记未动。
    ///
    /// **第 29 轮同步（2026-10-19 Task 1 wire 契约扩展）**：10120/221775 →
    /// **10213/224043**（+93 行 / +2268 字符）。变化**只来自 `v2/model.rs`**（空排除区 ⇒
    /// 整文件在判据面内）：新增 `ChannelKind` / `ProviderAliasKind` / `ProviderAliasRule` /
    /// `ProviderAliasConfig` / `ChannelOption` / `PriceOverrideRequest` / `PriceOverrideView`，
    /// 扩展 `DistributionRow` 的渠道/模型字段与 `CostRow` 的渠道/覆盖价字段，并追加
    /// `PricingBasis::UserOverride`。文件数仍为 32、26/6 分类不变，排除区标记未动。
    ///
    /// **第 30 轮同步（2026-10-19 Task 2 provider 别名与渠道解析）**：10213/224043 →
    /// **10506/230806**（+293 行 / +6763 字符）。变化来自：
    ///   * 新增 `v2/channel.rs` 的生产半边（`ChannelResolver` / `resolve_channel` /
    ///     `join_key` / 校验与配置服务）——测试模块落在排除区内，不进面；
    ///   * `v2/mod.rs` 新增 `pub mod channel;` 与模块文档；
    ///   * `services/usage/mod.rs` 自省锁文档的 32→33、26/6→27/6 同步（该文件的面在
    ///     BEGIN 之前，文档改动会进面）。
    ///     文件数 32→33（`USAGE_SOURCES`）、分类 26/6→**27/6**；排除区标记未动。
    ///
    /// **第 31 轮同步（2026-10-19 Task 3 定价归一化、目录去重与手动覆盖）**：
    /// 10506/230806 → **11024/241840**（+518 行 / +11034 字符）。变化**只来自 `v2/pricing.rs`
    /// 生产半边**：`normalize_model_id` 与尾部剥离辅助、`NormalizedQuote` / `normalized` 索引、
    /// release_date 去重比较、`match_model` 前缀回退、`apply_override`、覆盖价 DAO 服务入口与
    /// `rate_string`；新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面。文件数仍为 33、
    /// 27/6 分类与排除区标记均未动。
    ///
    /// **第 32 轮同步（2026-10-19 Task 4 Token 查询层接入显示渠道）**：
    /// 11024/241840 → **11115/244751**（+91 行 / +2911 字符）。变化来自：
    ///   * `v2/query.rs` 生产半边：分布改用解析后的显示渠道（`ChannelResolver` /
    ///     `resolve_channel`）、模型维度按渠道键/工具拆行、趋势筛选改渠道键、
    ///     新增 `channel_options_with_conn` 与生产入口；
    ///   * `v2/channel.rs` 删除 Task 2 遗留的 `#[allow(dead_code)]` 豁免（解析器已接入）。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 33 轮同步（2026-10-19 Task 5 费用查询接入自动定价与渠道行）**：
    /// 11115/244751 → **11310/250553**（+195 行 / +5802 字符）。变化来自：
    ///   * `v2/cost_query.rs` 生产半边：按显示渠道/原始模型拆分费用行、渠道解析接入、
    ///     手动覆盖 > 用户规则 > 归一化自动匹配（含前缀歧义 reason）、行键扩展、
    ///     CSV 追加渠道/来源/覆盖列；
    ///   * `v2/pricing.rs` 新增 `load_effective_overrides_with_conn`（覆盖价一次性加载）。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 34 轮同步（2026-10-19 Task 6 IPC/前端数据层）**：
    /// 11310/250553 → **11333/251182**（+23 行 / +629 字符）。变化仅来自
    /// `v2/pricing.rs` 新增覆盖价生产入口（`overrides` / `set_override` / `delete_override`
    /// 及 `db_lock_failed` 辅助）——命令层仍不取锁；文件数仍为 33、
    /// 27/6 分类与排除区标记均未动。
    ///
    /// **第 35 轮同步（2026-10-19 PR #170 增量复审修复）**：11333/251182 →
    /// **11390/252622**（+57 行 / +1440 字符）。变化来自本批生产代码：
    ///   * `v2/model.rs`：`DistributionRow` / `CostRow` 各新增 `observed_providers` 字段，
    ///     `observed_provider` 语义收紧（空区文件 ⇒ 整文件在面内）；
    ///   * `v2/query.rs`：分布分组累计空 provider 标记并按「唯一且无空行」收紧兼容字段；
    ///   * `v2/cost_query.rs`：费用分组累计去重原始 provider 并写入行；
    ///   * `v2/pricing.rs`：`is_noise_token` 移除裸数字、`is_noise_suffix` 保留日期/版本形态，
    ///     前缀同价判定纳入 `conditional`；
    ///   * `v2/channel.rs`：upsert 服务边界 trim `provider` / `displayName`。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 36 轮同步（2026-10-19 PR #170 残留 P2-R1 修复）**：11390/252622 →
    /// **11407/253250**（+17 行 / +628 字符）。变化来自：
    ///   * `v2/model.rs`：`CostRow` 新增 `has_empty_provider`；
    ///   * `v2/cost_query.rs`：费用分组累计空 provider 标记、按「唯一且无空行」收紧
    ///     `identity.observed_provider`、CSV 单值列留空规则；
    ///   * `v2/pricing.rs`：`is_date_noise` 的月/日范围与 `parse_release_date` 对齐。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 37 轮同步（2026-10-19 最终复核 P2：C 卡未标注来源提示）**：11407/253250 →
    /// **11409/253321**（+2 行 / +71 字符）。变化**只来自生产半边**：
    ///   * `v2/model.rs`：`DistributionRow` 新增 `has_empty_provider`（空排除区 ⇒ 整文件在面内）；
    ///   * `v2/query.rs`：分布分组标记写入 wire 行。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 38 轮同步（2026-10-19 手工测试回归：定价下载超时 + 目录搜索）**：
    /// 11409/253321 → **11474/255064**（+65 行 / +1743 字符）。变化**只来自生产半边**：
    ///   * `v2/model.rs`：新增 `PricingCatalogEntry`（参考 API 模型下拉搜索的 wire 类型）；
    ///   * `v2/pricing.rs`：下载超时拆成 `connect_timeout(10s)` + 总 `timeout(120s)`、
    ///     上限判据抽成 `ensure_payload_within_limit`，并新增
    ///     `catalog_entries_with_conn` / `catalog_entries` 搜索入口。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 39 轮同步（2026-10-19 手工测试回归：目录坏费率逐条降级）**：
    /// 11474/255064 → **11486/255327**（+12 行 / +263 字符）。变化**只来自生产半边**：
    ///   * `v2/pricing.rs`：新增 `parse_catalog_rate`（>9 位小数的上游 f64 噪声截断到
    ///     9 位，其余非法形态返回 None），`parse_catalog` 遇单条坏费率只将该分量标为
    ///     未估价并记录 warning，不再拒绝整个目录。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 40 轮同步（2026-10-19 手工测试回归：dsh 日档 output-only 计价）**：
    /// 11486/255327 → **11506/255773**（+20 行 / +446 字符）。变化**只来自生产半边**：
    ///   * `v2/cost.rs`：新增 `safe_components_output_only`（dsh 日档只给 output，
    ///     输入/缓存桶保持未估价），并更新模块/`basis_trusted` 的安全依据注释；
    ///   * `v2/cost_query.rs`：`calculate_row` 增加 dsh 日档 output-only 的 safe 分量选择，
    ///     并对该形态追加 `untrustedInputBuckets` reason。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 41 轮同步（2026-10-20 配置与费用表重构 Task 1 wire 契约）**：
    /// 11506/255773 → **11570/257450**（+64 行 / +1677 字符）。变化**只来自生产半边**：
    ///   * `v2/model.rs`（空排除区 ⇒ 整文件在面内）：新增 `ObservedModelEntry` /
    ///     `ModelDisplayNameRequest` / `PricingIdentityRequest` / `PricingCatalogSearch` /
    ///     `PricingQuotePreview` / `PricingAutoMatch`，`PricingCatalogEntry` 追加
    ///     `curated_kind`、`CostRow` 追加 `display_model_name`；
    ///   * `v2/pricing.rs`：`catalog_entries_with_conn` 的条目构造补 `curated_kind: None`
    ///     （T3 作用域搜索接管分类）；
    ///   * `v2/cost_query.rs`：`CostRow` 构造补 `display_model_name: None`
    ///     （T4 分组阶段接管解析）。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 33、
    ///     27/6 分类与排除区标记均未动。
    ///
    /// **第 42 轮同步（2026-10-20 配置与费用表重构 Task 2 模型配置服务）**：
    /// 11570/257450 → **11917/265531**（+347 行 / +8081 字符）。变化来自：
    ///   * **新文件 `v2/model_config.rs`**（生产半边进面，测试模块在排除区内）：
    ///     观测身份、观察条目聚合、别名解析接入、显示名/计价身份的校验与读改写；
    ///   * `v2/channel.rs`：新增 `display_provider_of`（观察条目的别名解析，
    ///     不触 billing attribution）。
    ///     文件数 33→**34**（`USAGE_SOURCES` 新增 `("v2/model_config.rs", …)`）、
    ///     分类 27/6→**28/6**（新文件带测试模块区）；排除区标记未动。
    ///
    /// **第 45 轮同步（2026-10-20 配置与费用表重构 Task 5 IPC 服务入口）**：
    /// 12393/276678 → **12422/277406**（+29 行 / +728 字符）。变化**只来自
    /// `v2/pricing.rs` 生产半边**：精选目录/价格预览/自动匹配的生产入口
    /// （`catalog_search_scoped` / `quote_preview` / `auto_match`，短锁形态与
    /// 覆盖价入口同款）。新增测试落在集成文件 ⇒ 不进面；文件数仍为 34、
    /// 28/6 分类与排除区标记均未动。
    ///
    /// **第 44 轮同步（2026-10-20 配置与费用表重构 Task 4 明细聚合计费）**：
    /// 12126/270560 → **12393/276678**（+270 行 / +6119 字符）。变化**只来自生产半边**：
    ///   * `v2/cost_query.rs`：`CostBucketProvenance` / `cost_snapshot_with_conn`
    ///     （Detail/Daily 覆盖检测与整段回退）、`load_detail_daily_rows`（明细按日聚合）、
    ///     `calculate_row` 的 provenance 可信判据、`resolve_row_pricing` 第 3 级计价身份
    ///     覆盖、`GroupAcc.display_model_names` 与 `CostRow.display_model_name` 解析；
    ///   * `v2/model_config.rs`：新增 `RowConfigIndex`（行级配置一次性加载）。
    ///     新增测试全部落在 BEGIN..END 排除区内/集成文件 ⇒ 不进面；文件数仍为 34、
    ///     28/6 分类与排除区标记均未动。
    ///
    /// **第 43 轮同步（2026-10-20 配置与费用表重构 Task 3 精选来源）**：
    /// 11917/265531 → **12126/270560**（+208 行 / +5029 字符）。变化**只来自生产半边**：
    ///   * `v2/pricing.rs`：精选常量与 `curated_kind_of` / `is_curated`、`MatchScope`、
    ///     `PriceCatalog.normalized_curated` / `provider_count`（解析期精选索引）、
    ///     `match_model_scoped`（原 `match_model` 重构为索引参数化）、
    ///     `catalog_search_scoped_with_conn` / `quote_preview_with_conn` / `auto_match_with_conn`；
    ///   * `v2/cost_query.rs`：自动匹配固定 `MatchScope::Curated`。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 34、
    ///     28/6 分类与排除区标记均未动。
    ///     **第 48 轮同步（2026-10-21 自定义价格规则与分时计费）**：
    ///     13451/301445 → **13936/312799**（+485 行 / +11354 字符），测试模块区 30→32（time_price + settings 测试入排除区）。
    ///     新增 `v2/time_price.rs`（窗口校验/时区换算/峰谷判定，含测试排除区）；
    ///     其余变化来自 `v2/pricing.rs` 的自定义规则/精选精简、`v2/cost_query.rs` 的
    ///     小时取数与分时费用字段、`v2/model.rs` 的新 wire 类型。
    ///
    /// **第 49 轮同步（2026-10-30 WorkBuddy 5.9.3 子代理入账 + 看板在场判据）**：
    /// 13936/312799 → **14176/316773**（+240 行 / +3974 字符）。变化**只来自生产半边**：
    ///   * `services/usage/collectors/workbuddy.rs`：`collect_file` 抽取、子代理目录枚举、
    ///     身份解析（`resolve_subagent_session_id`）与 `WbState.subagent_session_id`；
    ///   * `monitor/workbuddy_parser.rs`：`WbDbSession` / `recent_db_sessions` /
    ///     `get_workbuddy_sessions_with` / `SESSION_ACTIVE_WINDOW_MS` / 补偿 `is_live`；
    ///   * `monitor/host.rs`：`tool_host_pid` + 纯核 `process_is_tool_host`。
    ///     新增测试全部落在 BEGIN..END 排除区内 ⇒ 不进面；文件数仍为 34、分类与排除区标记均未动。
    #[test]
    fn scan_face_size_is_pinned() {
        let (mut lines, mut chars) = (0usize, 0usize);
        for (name, src) in USAGE_SOURCES {
            let face = crate::services::usage::self_lock::scan_face(name, src);
            lines += face.len();
            chars += face.iter().map(|(_, l)| l.chars().count()).sum::<usize>();
        }
        assert_eq!(
            (lines, chars),
            (14176, 316773),
            "判据面尺寸变了（实测 {lines} 行 / {chars} 字符）：本用例是**声明 = 实际**的钉子——\
             先弄清面为什么变（新增文件？改了排除区标记？动了生产代码？），再同步本常量、\
             `no_source_timezone_is_fed_into_any_key_function` 的锁注释、`KNOWN-PLAN-DEFECTS.md` \
             与 `FINAL-FIX-report.md`"
        );
    }

    /// **A 段第 2 条（P1-1 收尾）**：硬编码的扫描面数组必须与磁盘**逐一对账**——
    /// 防第 34 个 `.rs` 文件（新增模块 / 新增子目录）静默逃逸自省锁。
    ///
    /// 判据：`CARGO_MANIFEST_DIR/src/services/usage` 下递归枚举全部 `.rs`，与 `USAGE_SOURCES`
    /// 的**名字集合必须相等**（不是包含关系）。**能变红**：往该目录放一个新 `.rs`（不改任何
    /// 生产代码）→ 本用例红；新增的源文件必须同时进 `USAGE_SOURCES` 并确认其生产半边无禁项。
    /// **反例（枚举面之外）**：①`include!` 的 `.inc`、`include_str!` 引入的外部文本、
    /// **`#[path = "…"] pub mod x;`**（代码可落在本目录之外）
    /// ②**大小写变体以外的**非 `.rs` 名字（B-1 修后 `.RS` / `.Rs` **已进面**：本机 `rustc`
    /// 能从 `foo.RS` 编译 `mod foo;`，故扩展名比较改为 `eq_ignore_ascii_case("rs")`）。
    #[test]
    fn usage_source_scan_covers_every_file_on_disk() {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("扫描面根目录必须可读") {
                let path = entry.expect("目录项必须可读").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
                {
                    out.push(
                        path.strip_prefix(root)
                            .expect("枚举根必须是 CARGO_MANIFEST_DIR 下的 usage 目录")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/usage");
        let mut on_disk: Vec<String> = Vec::new();
        walk(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut listed: Vec<String> = USAGE_SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        listed.sort();
        // **B-3（第 6 轮）**：把「28 个测试模块区 / 6 个空区」这个分类**机械钉住**——
        // 只在注释/文档里写分类会静默失真（给空区文件加个测试模块 ⇒ 文档过期而用例全绿）。
        let empty = USAGE_SOURCES
            .iter()
            .filter(|(name, src)| crate::services::usage::self_lock::exclusion_is_empty(name, src))
            .count();
        assert_eq!(
            (USAGE_SOURCES.len() - empty, empty),
            (32, 5),
            "排除区分类应为 **32 个测试模块区 / 5 个空区**（空区 = `caps.rs` / `delta.rs` / \
             `model.rs` / `v2/model.rs` / `v2/mod.rs`）。改动某文件的\
             排除区形态（加测试模块 / 改成空区）时，必须**同步**本断言、`self_lock` 的文档与\
             两处锁的声明"
        );
        assert_eq!(
            listed, on_disk,
            "自省锁的扫描面必须与磁盘**逐一相等**：`services/usage/**/*.rs` 新增文件后必须同时\
             加进 `USAGE_SOURCES`（并确认新文件的生产半边不含禁项），否则第 35 个文件会静默\
             逃逸反向锁——这正是 P1-1 抓到的「覆盖声明 > 实际判据」同族"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
