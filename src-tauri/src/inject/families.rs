//! 注入族规格层（M9R Task 1，纯核）：**脆弱常量集中落点（宪法横切 6）**——分块/间隔/
//! 预算/阈值/版本指纹全部在此，大版本升级复验走项目技能 win-console-inject-probe 快路径。
//!
//! 族表为 M6R 探测定案（2026-09-18/19，证据 `research/refs/phase2-消息注入/`）：
//! 四家 CLI 按 TUI 家族分 A/B 两族——
//! - **A 族 [`TuiFamily::RawVt`]**（ReadConsoleInput 原生 VT 流）：claude / kimi /
//!   opencode，方向键走 vk=0 字符流（单批原子写）；
//! - **B 族 [`TuiFamily::Crossterm`]**（crossterm event 体系）：codex，键须带
//!   VK+scan，vk=0 控制字符会被丢弃。
//!
//! `verified_with` 是版本指纹：仅对该版本验证过注入规格；CLI 大版本升级后必须
//! 先跑 win-console-inject-probe 复验，再改此表（不得凭直觉改数值）。
//!
//! 本模块零平台 cfg：族表/背压判定/预算/分块计划全部是纯函数，Windows 上全绿可测。

/// TUI 家族（A/B 两族，M6R 定案）：决定事件构造形态（VT 字符流 vs VK+scan）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiFamily {
    /// A 族：原生 VT 流（ReadConsoleInput 消费 vk=0 的 Unicode 字符事件）。
    RawVt,
    /// B 族：crossterm 事件体系（须 VK+scan 键形态，vk=0 会被丢）。
    Crossterm,
}

/// 单工具的注入族规格（M6R 探测定案表的一行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilySpec {
    /// 所属 TUI 家族（A/B 族按家分支的依据）。
    pub family: TuiFamily,
    /// 版本指纹：注入规格实测验证时的 CLI 版本（大版本升级须复验）。
    pub verified_with: &'static str,
    /// 慢消费者（opencode ~70 事件/秒）：任意长度都走背压节流（R2-1）。
    pub slow_consumer: bool,
    /// A1 直发确认超时（毫秒，Task 5 消费）。
    pub confirm_timeout_ms: u64,
}

// 分块/提交延迟三常量（D20 起**定义在** [`super::timing`]，本模块只**转出**）：
// 它们是「注入路径真实开销」的组成部分，与屏读轮询窗同属一个时序族——故按
// 「单一事实源」收在 timing.rs（宪法 D20 / 计划 §2.9 的落地要求）。
// 此处 `pub use` 只为保持既有路径（`families::CHUNK_CHARS` 等调用点与文档锚）不断，
// **禁止**在本文件重新定义这三者（那就是「同一判据两处实现」的老路；
// `inject::timing::tests::chunk_constants_have_one_definition` 会先红）。
pub use super::timing::{CHUNK_CHARS, CHUNK_GAP_MS, SUBMIT_DELAY_MS};

/// 长文阈值（字符数）：快消费者超过才切背压（恰好 2000 不背压）。
pub const LONG_MSG_CHARS: usize = 2000;
/// 背压模式占用回落阈值（事件数）：写停到占用 ≤ 此值再继续。
pub const DRAIN_TO: u32 = 40;
/// 判冻窗口阈值：占用连续 ≥5s 且相邻采样无下降（逐样本下降口径，M9R 评审
/// C1 修正后语义）→ 判冻。
pub const OCC_ABNORMAL_MS: u64 = 5_000;
/// 基础注入总预算（毫秒）：非背压路径的硬上限。
pub const BASE_BUDGET_MS: u64 = 10_000;
/// 背压斜率：每字符放宽毫秒数（opencode 实测 10k 字符 ≈110–183s，45ms/字符
/// 放宽到 460s 留足余量）。斜率按 chars 计；emoji 等代理对字符事件数翻倍，
/// 极端 emoji 负载预算可能偏紧——Task 12 实机校准项。
pub const BACKPRESSURE_MS_PER_CHAR: u64 = 45;

/// 无族回退规格（queue 驱动的 flush 对本表未收录工具的兜底）：
/// 快消费者默认口径（A 族形态 + 5s 确认超时）。脆弱常量集中落点（宪法横切 6）
/// 故放本模块；windows_console 旧薄壳已改引本常量，单一来源勿复制。
pub(crate) const FALLBACK_SPEC: FamilySpec = FamilySpec {
    family: TuiFamily::RawVt,
    verified_with: "default-fast",
    slow_consumer: false,
    confirm_timeout_ms: 5_000,
};

/// 工具 → 族规格（小写精确匹配，对齐 `AgentType` serde lowercase 形态）。
/// M6R 探测定案表；其余工具返回 None（flush 循环以 [`FALLBACK_SPEC`] 兜底）。
/// **Task 7 起路由表已按实际分派定性——旧口径的「黑盒/无头家」不再对应同一批工具**：
/// workbuddy → `Headless(WbAcp)`（H9）、zcode → `Headless(Zcode)`（H7/H10）、codex APP 形态
/// → `Headless(CodexQueue)`（H8）、claude/kimi/opencode 无进程形态 → `Headless(ClaudeP|
/// KimiP|OpencodeRun)`（H11/C4）、dsh 需要无头的场合 → `dsh_headless_pending`（H13 不在本批）、
/// openclaw → `blackbox`（gateway 另评）——这些家在端点**入队之前**即被拒（H3 门/路由/
/// 无头分派点），正常路径不消费本表。
pub fn family_for(tool: &str) -> Option<FamilySpec> {
    let (family, verified_with, slow_consumer) = match tool {
        "claude" => (TuiFamily::RawVt, "2.1.251", false),
        "kimi" => (TuiFamily::RawVt, "2.0.0", false),
        "opencode" => (TuiFamily::RawVt, "2.0.22", true),
        "codex" => (TuiFamily::Crossterm, "0.154.0", false),
        _ => return None,
    };
    Some(FamilySpec {
        family,
        verified_with,
        slow_consumer,
        // 自裁值（测试未钉）：直发确认秒级命中口径下留双倍余量；慢消费者含
        // SQLite 副本查询开销，再加倍。
        confirm_timeout_ms: if slow_consumer { 10_000 } else { 5_000 },
    })
}

/// 背压判定（R2-1）：慢消费者或长文切背压——慢消费者任意长度都节流；快消费者
/// 仅超过 [`LONG_MSG_CHARS`] 才节流（恰好 2000 不背压）。
pub fn use_backpressure(spec: &FamilySpec, chars: usize) -> bool {
    spec.slow_consumer || chars > LONG_MSG_CHARS
}

/// macOS 平台回车投影表（F2，mac-reverify-b9a501c §四-B）：族表（[`family_for`]）
/// 平台无关——kimi 在 **Windows** 上实为 A 族（M6R 探测定案），而「macOS 注入后
/// 尾随回车被 TUI 吞没、文本滞留 composer」是 **平台 × 工具** 特异行为（Mac 实测
/// codex 与 kimi 同现、claude 对照正常）。数据源 = `research/refs/phase2-消息注入/`
/// known-families.md 平台差异表（mac-verification.md「回车投影」概念入库）。
///
/// true = macOS 上直发确认失败须给「补按一次回车」指引文案（confirm.rs 消费）；
/// Windows 及其他平台不消费本表（Windows crossterm 有真回车事件形态，语义不变）。
/// 默认 false（未实测工具宁可少提示，不误报）。
pub fn macos_enter_swallowed(tool: &str) -> bool {
    matches!(tool, "codex" | "kimi")
}

/// 真总预算（毫秒）：背压路径按 [`BACKPRESSURE_MS_PER_CHAR`] 斜率放宽
/// （[`BASE_BUDGET_MS`] + 每字符 45ms），非背压路径固定 [`BASE_BUDGET_MS`]。
pub fn inject_budget_ms(spec: &FamilySpec, chars: usize) -> u64 {
    if use_backpressure(spec, chars) {
        BASE_BUDGET_MS + chars as u64 * BACKPRESSURE_MS_PER_CHAR
    } else {
        BASE_BUDGET_MS
    }
}

/// 分块/背压计划（纯构造，Task 3 执行层消费）：正文按 [`CHUNK_CHARS`] 向上取整
/// 的分块数 + 是否走背压。回车块不在此计划内（执行层固定 [`SUBMIT_DELAY_MS`]
/// 后单批发回车）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlan {
    /// 正文分块数（按 chars 计数；空文本 → 0 块）。
    pub text_chunks: usize,
    /// 是否切背压节流。
    pub backpressure: bool,
}

/// 构造 [`ChunkPlan`]：`text_chunks` = 正文按 [`CHUNK_CHARS`] 向上取整（按 chars
/// 计数）；`backpressure` = [`use_backpressure`]。
pub fn chunk_plan(text: &str, spec: &FamilySpec) -> ChunkPlan {
    let chars = text.chars().count();
    ChunkPlan {
        text_chunks: chars.div_ceil(CHUNK_CHARS),
        backpressure: use_backpressure(spec, chars),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部 8 常量钉值（廉价防漂移保险：脆弱常量集中落点，改任一数值必须过此关）
    #[test]
    fn constants_match_probe() {
        assert_eq!(CHUNK_CHARS, 80);
        assert_eq!(CHUNK_GAP_MS, 50);
        assert_eq!(SUBMIT_DELAY_MS, 150);
        assert_eq!(LONG_MSG_CHARS, 2000);
        assert_eq!(DRAIN_TO, 40);
        assert_eq!(OCC_ABNORMAL_MS, 5_000);
        assert_eq!(BASE_BUDGET_MS, 10_000);
        assert_eq!(BACKPRESSURE_MS_PER_CHAR, 45);
    }

    #[test]
    fn fallback_spec_matches_default() {
        // 无族回退钉值（const 常量 → const 块编译期断言，clippy assertions_on_constants）：
        // 快消费者口径（A 族 + 5s 确认超时）——queue/确认层的黑盒家兜底，与 M6R
        // 「无族按快消费者默认」定案对齐
        const _: () = {
            assert!(matches!(FALLBACK_SPEC.family, TuiFamily::RawVt));
            assert!(!FALLBACK_SPEC.slow_consumer);
            assert!(FALLBACK_SPEC.confirm_timeout_ms == 5_000);
        };
    }

    #[test]
    fn family_table_matches_probe() {
        let c = family_for("claude").unwrap();
        assert_eq!(c.family, TuiFamily::RawVt);
        assert!(!c.slow_consumer);
        assert_eq!(c.verified_with, "2.1.251");
        let o = family_for("opencode").unwrap();
        assert!(o.slow_consumer);
        assert_eq!(o.verified_with, "2.0.22");
        let x = family_for("codex").unwrap();
        assert_eq!(x.family, TuiFamily::Crossterm);
        assert_eq!(x.verified_with, "0.154.0");
        let k = family_for("kimi").unwrap(); // 全行断言：族/版本指纹/消费速率
        assert_eq!(k.family, TuiFamily::RawVt);
        assert_eq!(k.verified_with, "2.0.0");
        assert!(!k.slow_consumer);
        assert!(family_for("workbuddy").is_none()); // 路由层已拦，此处纵深防御
    }

    #[test]
    fn backpressure_rules() {
        // R2-1：慢消费者或长文切背压
        let fast = family_for("claude").unwrap();
        let slow = family_for("opencode").unwrap();
        assert!(!use_backpressure(&fast, 80));
        assert!(use_backpressure(&fast, 2001));
        assert!(use_backpressure(&slow, 39));
        assert!(!use_backpressure(&fast, 2000));
    }

    #[test]
    fn budget_scales_for_backpressure() {
        // 真总预算：背压按斜率放宽（opencode 实测 10k≈110-183s）
        let slow = family_for("opencode").unwrap();
        assert_eq!(inject_budget_ms(&slow, 80), 10_000 + 80 * 45);
        assert!(inject_budget_ms(&slow, 10_000) >= 183_000);
        assert_eq!(
            inject_budget_ms(&family_for("claude").unwrap(), 500),
            10_000
        );
    }

    #[test]
    fn chunk_plan_counts_and_flags() {
        // ChunkPlan 刻意不含回车块——回车块归执行层（SUBMIT_DELAY_MS=150ms 后单批）；
        // 本测试只核正文分块计数与背压旗标
        let spec = family_for("claude").unwrap();
        // 丁T3 裁2 起签名在尾部——夹具用真机形态（本测试只看分块计数，形态对齐即可）
        let plan = chunk_plan("hello [mobile test]", &spec);
        assert_eq!(plan.text_chunks, 1);
        assert!(!plan.backpressure);
        let spec_op = family_for("opencode").unwrap();
        assert!(chunk_plan(&"x".repeat(3000), &spec_op).backpressure);
    }
}
