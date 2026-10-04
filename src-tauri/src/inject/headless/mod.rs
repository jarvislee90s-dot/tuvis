// 无头注入底座（H4 生命周期 / H6 回执·审计·版本门控）——Task 6。
//
// **范围**：本模块只做**共享底座**，不含任何工具通道（zcode/codex/WB/CLI 四家
// 分属 Task 8/9/11/13）。三层结构：
// - [`receipt`]：回执归一（纯函数；前缀跳过 JSON 解析、未知帧不猜、200 字截断）；
// - [`runner`]：进程生命周期（全局并发上限、watchdog 超时、取消、kill 进程树）；
// - [`gate`]：版本门控探针（`--prompt` 干跑 / `queue --help` 子命令在场 + 结果缓存）。
//
// **审计归属（Task 6 裁决 A）**：无头动作落**既有的** `write_audit` 表（W5 单一账本，
// 9 列 NOT NULL；该表 `action` 列无 CHECK 约束——见 `database/schema.rs`，故**无需
// migration**）。设备身份（device_id/device_name）来自移动端 gate 上下文，
// **runner 不自造**：runner 只归一回执 + 记录终止方，由端点（持有 gate 上下文与连接）
// 经 [`audit_headless`] 落 `headless` / `headless_cancel` 两行。
pub mod gate;
pub mod receipt;
pub mod runner;
/// H7 zcode 无头通道（Task 8；codex/workbuddy/CLI 三家分属 Task 9/11/13）
pub mod zcode;

/// 无头动作审计词（H6）：turn 落账（终态 = 回执终态）
pub const ACTION_HEADLESS: &str = "headless";
/// 无头取消审计词（H4/H6）：移动端主动取消（**先到者生效**——watchdog 先到则记
/// `headless` 且 stage=timeout，不落本词）
pub const ACTION_HEADLESS_CANCEL: &str = "headless_cancel";

/// watchdog 默认超时（H4 / 裁决 15：600s；两端实测 turn 仅 8–23s，留足余量）
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// 超时下界（1s；0 会让 watchdog 即刻到点）
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// 超时上界（1h；防手滑写成天数级看门狗形同虚设）
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// 全局并发上限默认值（H4：默认 2）
pub const DEFAULT_CONCURRENCY: usize = 2;
/// 并发下界（1；0 会让所有请求永久排队）
pub const MIN_CONCURRENCY: usize = 1;
/// 并发上界（8；防手机端连点多会话打爆机器）
pub const MAX_CONCURRENCY: usize = 8;

/// 超时 clamp（越界一律收进区间——与 `remote.max_devices` 的 clamp 口径一致）
pub fn clamp_timeout_ms(v: u64) -> u64 {
    v.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// 并发上限 clamp
pub fn clamp_concurrency(v: usize) -> usize {
    v.clamp(MIN_CONCURRENCY, MAX_CONCURRENCY)
}

/// H4 配置落点取值（watchdog 超时 + 全局并发上限）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadlessLimits {
    pub timeout_ms: u64,
    pub concurrency: usize,
}

impl Default for HeadlessLimits {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

impl HeadlessLimits {
    /// 运行期读取（runner 启动时用；spec H4「runner 启动时读取」）。
    /// 与 `remote_status` 下发**同一条读取路径**（`remote::*_conn`）——杜绝
    /// 「设置页显示 5 分钟、runner 却按 10 分钟跑」的双轨漂移。
    pub fn from_conn(conn: &rusqlite::Connection) -> Self {
        Self {
            timeout_ms: crate::remote::headless_timeout_ms_conn(conn),
            concurrency: crate::remote::headless_concurrency_conn(conn),
        }
    }
}

/// 无头审计上下文（**由调用方注入**）：设备身份来自移动端 gate 上下文，
/// runner 与本模块都不自造设备名/会话号——机器自发动作不得冒充某台手机。
pub struct HeadlessAuditCtx {
    pub device_id: String,
    pub device_name: String,
    pub agent_type: String,
    pub session_id: String,
    /// 无头通道名（Task 7/8 的 HeadlessKind 展示名，如 `headless_zcode`）——
    /// **不是**终端注入器名（那对无头动作是假值）
    pub channel: String,
    /// 命令形态/消息原文：摘要按 W5 口径在 [`crate::inject::audit_write_channel`] 内现算
    pub content: String,
}

/// **耗时口径（H6「耗时」；评审 Important 2）**：`write_audit` 表**没有 duration 列**
/// （9 列全 NOT NULL，见 `database/schema.rs`），故把耗时**显式编码进 `result` 列**：
/// `"<终态> · <duration_ms>ms"`（如 `ok · 1234ms`）。选 result 而非 summary 的理由：
/// summary 是**内容摘要**（W5 口径，80 字截断，`dao/write_audit.rs` 词表注释已声明），
/// 混入耗时会让摘要口径漂移；result 本就承载「回执终态」，追加可读的秒表读数最自然，
/// 审计页原样展示即可读，且 `· <n>ms` 可 grep。口径由本函数**单点产生**并被测试钉死
/// （`audit_result_pins_duration_encoding`）。若将来要独立成列 → 需 migration + 审计
/// 视图同步（此处登记，不擅自改表）。
pub fn audit_result(result: &str, duration_ms: u64) -> String {
    format!("{result} · {duration_ms}ms")
}

/// 无头审计落账（**单一账本**：既有 `write_audit` 表 + 既有 W5 摘要口径）。
/// 端点侧在回合终结（`ACTION_HEADLESS`）与取消生效（`ACTION_HEADLESS_CANCEL`）
/// 两处调用；runner 不落账（它没有设备身份，也没有连接）。
/// `duration_ms` 由 `Receipt::duration_ms` 原样传入（耗时口径见 [`audit_result`]）。
///
/// **登记（Task 6 现状）**：本函数**目前没有生产调用者**——Task 6 只交付底座，接线在
/// Task 8/9/11/13 的端点（`pub fn` 不触发 dead_code，未接线既不会编译报错也不会测试红，
/// 故在此显式登记，别让它被漏掉）。
pub fn audit_headless(
    conn: &rusqlite::Connection,
    ctx: &HeadlessAuditCtx,
    action: &str,
    result: &str,
    duration_ms: u64,
) {
    crate::inject::audit_write_channel(
        conn,
        &ctx.device_id,
        &ctx.device_name,
        &ctx.agent_type,
        &ctx.session_id,
        &ctx.channel,
        &ctx.content,
        action,
        &audit_result(result, duration_ms),
    );
}

/// **H4 配置生效内核（可测核）**：从连接读两键 → 把**并发上限落到给定的全局名额** →
/// 返回限值。生产出口两处：① app 启动（`remote::init_headless_limits` ← `lib.rs` setup）
/// ② 设置页写入后（`remote::set_headless_limits_core`）。**并发是全局属性**（名额在
/// [`runner::global_sem`] 上），所以「改设置即生效」= 在这里 `set_cap`，不是每回合重建。
pub fn apply_limits_to(conn: &rusqlite::Connection, sem: &runner::GlobalSem) -> HeadlessLimits {
    let limits = HeadlessLimits::from_conn(conn);
    sem.set_cap(limits.concurrency);
    limits
}

/// **runner 启动装配（生产入口，计划 Step 4「runner 启动时读取」）**：一次把 H4 两键
/// 落到回合配置（**超时**随回合）与全局名额（**并发**全局瞬时）。端点每回合 spawn 前
/// 调用本函数而不是 [`runner::RunnerCfg::new`]——否则超时永远是默认 600s（设置形同虚设）。
///
/// **Task 8/9/11/13 的义务（登记：目前只由本文档约束，无类型强制）**：① 每回合经本函数
/// 建 runner（拿设置里的 watchdog 超时）；② 并发经 [`runner::global_sem`] 取名额（别自建
/// `GlobalSem`，否则「全局上限 2」名存实亡）；③ 回合终结/取消落账走 [`audit_headless`]
/// （含 `duration_ms`）。三件事都没有编译期强制——Task 7 的路由与 Task 8 的端点接线时
/// 请按此清单对齐。
pub fn runner_from_conn(program: &str, conn: &rusqlite::Connection) -> runner::RunnerCfg {
    let mut cfg = runner::RunnerCfg::new(program);
    let limits = apply_limits_to(conn, &runner::global_sem());
    cfg.apply_limits(limits);
    cfg
}

/// 测试专用串行锁（仅 cfg(test)；与 `inject::queue::LOOP_HANDLE_TEST_LOCK` 同型同用法，
/// 故置于模块顶层而非 tests 内——`remote::mod` 的测试要跨模块取它）。理由：H4 的并发
/// 上限住在**进程级单例** `runner::global_sem()` 上，两个用例会改它并断言其值（本模块的
/// runner 启动装配、`remote::tests` 的写入内核）——并行跑时断言窗口可能交错（一个改 3、
/// 另一个读回 1）。**两测全程持本锁强制串行**（确定性方案，不靠重试启发式）。
/// 不持锁的那一个（`configured_concurrency_cap_actually_gates_second_turn`）用**自建**
/// `GlobalSem`，不涉全局单例。
#[cfg(test)]
pub(crate) static HEADLESS_CAP_TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        conn
    }

    /// H4 配置落点的数字单点：默认值/上下界在此钉死（设置层只做 KV 解析与转发）
    #[test]
    fn limits_defaults_and_clamp_are_pinned() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 600_000, "裁决 15：watchdog 默认 600s");
        assert_eq!(DEFAULT_CONCURRENCY, 2, "H4：全局并发默认 2");
        assert_eq!(HeadlessLimits::default().timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(HeadlessLimits::default().concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(
            clamp_timeout_ms(0),
            MIN_TIMEOUT_MS,
            "0 会即刻超时——抬到下界"
        );
        assert_eq!(clamp_timeout_ms(u64::MAX), MAX_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(120_000), 120_000);
        assert_eq!(
            clamp_concurrency(0),
            MIN_CONCURRENCY,
            "0 会让所有请求永久排队"
        );
        assert_eq!(clamp_concurrency(99), MAX_CONCURRENCY);
        assert_eq!(clamp_concurrency(3), 3);
    }

    /// 审计落账单点（裁决 A）：与终端注入**同表同摘要口径**（W5 80 字摘要）；
    /// 设备/会话/通道/动作/终态/**耗时**逐列可查，不另造平行账本。
    #[test]
    fn audit_headless_writes_the_single_w5_ledger_row() {
        let conn = mem_conn();
        let ctx = HeadlessAuditCtx {
            device_id: "d1".into(),
            device_name: "iPhone".into(),
            agent_type: "zcode".into(),
            session_id: "sess_1".into(),
            channel: "headless_zcode".into(),
            content: format!("{}——超长消息不得整段落库", "改".repeat(200)),
        };
        audit_headless(&conn, &ctx, ACTION_HEADLESS, "ok", 1_234);
        audit_headless(&conn, &ctx, ACTION_HEADLESS_CANCEL, "cancelled", 800);
        let rows = crate::database::dao::write_audit::recent_conn(&conn, 10);
        assert_eq!(rows.len(), 2, "无头动作必须落既有 write_audit 表");
        // 最新在前：取消行
        assert_eq!(rows[0].action, ACTION_HEADLESS_CANCEL);
        assert_eq!(rows[0].result, "cancelled · 800ms", "耗时口径必须钉死");
        assert_eq!(rows[1].action, ACTION_HEADLESS);
        assert_eq!(rows[1].device_name, "iPhone");
        assert_eq!(rows[1].agent_type, "zcode");
        assert_eq!(rows[1].session_id, "sess_1");
        assert_eq!(rows[1].channel, "headless_zcode", "通道列 = 无头通道名");
        assert_eq!(rows[1].result, "ok · 1234ms", "H6「耗时」必须落账");
        assert!(
            rows[1].summary.chars().count() <= crate::inject::normalize::AUDIT_SUMMARY_CHARS + 1,
            "摘要必须走 W5 截断口径（耗时不得塞进 summary 污染摘要口径）: {}",
            rows[1].summary
        );
    }

    /// H6「耗时」编码口径（评审 Important 2）：`"<终态> · <n>ms"`——单点产生、纯函数可钉
    #[test]
    fn audit_result_pins_duration_encoding() {
        assert_eq!(audit_result("ok", 1_234), "ok · 1234ms");
        assert_eq!(audit_result("cancelled", 0), "cancelled · 0ms");
        assert_eq!(
            audit_result("failed(timeout)", 600_000),
            "failed(timeout) · 600000ms"
        );
    }

    /// 动作词表钉死（扩展词表时同步改 dao/write_audit.rs 注释与设置页口径）
    #[test]
    fn audit_actions_are_pinned() {
        assert_eq!(ACTION_HEADLESS, "headless");
        assert_eq!(ACTION_HEADLESS_CANCEL, "headless_cancel");
    }

    /// 评审 Important 3：H4「可配」必须**真生效**——设置值落到全局名额，
    /// 上限 1 时第二个并发回合**即时排队**（位置 2），不静默丢也不无限等
    #[test]
    fn configured_concurrency_cap_actually_gates_second_turn() {
        let conn = mem_conn();
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_CONCURRENCY,
            "1",
        );
        let sem = runner::GlobalSem::new(DEFAULT_CONCURRENCY);
        let limits = apply_limits_to(&conn, &sem);
        assert_eq!(limits.concurrency, 1);
        assert_eq!(sem.cap(), 1, "设置值必须落到全局名额上限");
        let hold = sem.acquire(); // 占掉唯一名额 = 在飞的第一个回合
        let mut r = runner::RunnerCfg::for_test().sem(sem.clone());
        let out = r.run_once(|_| {});
        assert_eq!(
            out.status,
            receipt::ReceiptStatus::Queued,
            "上限 1 时第二个回合必须排队: {out:?}"
        );
        assert_eq!(out.queue_position(), Some(2));
        drop(hold);
        assert_eq!(sem.in_flight(), 0);
    }

    /// 计划 Step 4「runner 启动时读取」的生产入口：一次装配**超时 + 全局名额**（同一条
    /// 读取路径，杜绝「设置页 90s、runner 仍跑 600s」双轨）。
    /// 持 [`HEADLESS_CAP_TEST_LOCK`]：本测改**进程级单例**名额并与 remote 侧同型用例互斥。
    #[test]
    fn runner_from_conn_reads_both_keys_at_start() {
        let _serial = HEADLESS_CAP_TEST_LOCK.lock().unwrap();
        let conn = mem_conn();
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_TIMEOUT_MS,
            "90000",
        );
        crate::database::dao::settings::set_setting_conn(
            &conn,
            crate::remote::KEY_HEADLESS_CONCURRENCY,
            "3",
        );
        let cfg = runner_from_conn("mam-test-stub", &conn);
        assert_eq!(
            cfg.watchdog_timeout_ms(),
            90_000,
            "超时必须来自设置（不是默认 600s）"
        );
        assert_eq!(
            runner::global_sem().cap(),
            3,
            "并发上限必须同时落到全局名额"
        );
        // 缺键 → 回默认（不把半配置状态当 0）
        let empty = mem_conn();
        let d = runner_from_conn("mam-test-stub", &empty);
        assert_eq!(d.watchdog_timeout_ms(), DEFAULT_TIMEOUT_MS);
        assert_eq!(runner::global_sem().cap(), DEFAULT_CONCURRENCY);
        // 复位全局名额（进程级单例，别把 3 留给后续用例）
        runner::global_sem().set_cap(DEFAULT_CONCURRENCY);
    }
}
