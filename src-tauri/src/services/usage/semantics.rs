//! 口径层（计划①）：缓存语义 / 四桶归一 / 命中率派生 … 的纯函数与枚举。
//! **本模块是用量域的「唯一口径层」（Global Constraints 6）**：7 个采集器不得自己算命中率，
//! 只能上报 `RawUsage` 原始字段；语义判定、四桶归一、请求输入 / 命中率 / hero / 用户输入(估)
//! 全部在本文件的纯函数里完成。
//! `CacheSemantics` 枚举与 `as_db/from_db` 两个映射由 Task 3 落地（DAO 的编译前置），
//! 其余口径函数由 Task 5 补全。
use super::model::{UsageBuckets, UsageSourceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSemantics {
    Exclusive,
    Subset,
    TotalOnly,
    /// **`backfill`：不是「缓存语义」，是「来源标记」**（第 14 轮 dsh 时间归属改造）。
    ///
    /// 语义：这一行的量**不是**按事件时间实测到的，而是「日志已被清理、只能从投影缓存累计差额
    /// 补录」的部分 —— 只进**全天账**（`hour_key` = 日键），**不进任何短窗口**。
    /// 为什么复用 `cache_semantics` 列而不是新增列：任务书 §3.2 明确允许（「可用 `cache_semantics`
    /// 或新增列；新增列必须走 migration 并申报接口变更」）；本列是 TEXT、取值集合由本枚举定义，
    /// 扩充取值不需要 migration。**取值的扩充已在契约里做日期化申报**（见
    /// `docs/engineering/plans/2026-10-03-usage-interface-contract.md` 的 2026-10-07 变更申报）。
    ///
    /// **不得**拿它当语义用：`normalize()` 对它返回 `None`（补录行由采集器按 `Exclusive`
    /// 构造好四桶，不经过归一化）。
    Backfill,
}

impl CacheSemantics {
    pub fn as_db(&self) -> &'static str {
        match self {
            CacheSemantics::Exclusive => "exclusive",
            CacheSemantics::Subset => "subset",
            CacheSemantics::TotalOnly => "total-only",
            CacheSemantics::Backfill => "backfill",
        }
    }
    pub fn from_db(s: &str) -> Self {
        match s {
            "subset" => CacheSemantics::Subset,
            "total-only" => CacheSemantics::TotalOnly,
            "backfill" => CacheSemantics::Backfill,
            _ => CacheSemantics::Exclusive,
        }
    }

    /// 是不是「补录」（来源标记，非实测）。UI/导出将来据此区分「实测 / 补录」。
    pub fn is_backfill(&self) -> bool {
        matches!(self, CacheSemantics::Backfill)
    }
}

/// 原始用量（采集器上报的唯一形状，字段名贴近源但语义未判）
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RawUsage {
    /// 源里的 input 字段原文（语义待判：可能是「未缓存输入」也可能是「含缓存读的输入」）
    pub input_raw: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// 源里的 output 字段（不含 reasoning）
    pub output: i64,
    /// 单列的推理 token（opencode/zcode 有；并入产出桶，**不参与语义判据**）
    pub reasoning: i64,
    /// total_tokens 类总量（判据在场时逐条判定；缺省 None = 无判据）
    pub total_raw: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticsPolicy {
    pub declared: CacheSemantics,
    /// 是否允许用 total 算术改判（说明书 §4.2 规则 1：逐条判定优先于声明式默认）
    pub allow_arithmetic: bool,
}

/// 逐源声明默认（说明书 §4.2 规则 5 / 矩阵 §3.2）：七源声明值都是 Exclusive，
/// codex 靠判据逐条改判、拿不准落回该声明。
pub fn declared_default(_source: UsageSourceId) -> CacheSemantics {
    CacheSemantics::Exclusive
}

/// 逐源策略：**codex / zcode / workbuddy / opencode 允许算术判据**（与 `declared_defaults_match_matrix`
/// 的断言逐字对齐）——判据在场即按规则 1 改判，缺判据落声明默认（规则 2/5）。
/// 实测判据来源：codex / zcode `computed_total_tokens` / workbuddy `total_tokens`（三者本机均成立）；
/// **opencode 本机未取得可用判据**（矩阵 §3.2），留在允许清单里只是为了「将来拿到判据即可生效」，
/// 实际总是落声明默认 `Exclusive`。
/// claude / kimi / dsh 的字段名即互斥（无 total 判据），不参与算术判定。
pub fn default_policy(source: UsageSourceId) -> SemanticsPolicy {
    let allow = matches!(
        source,
        UsageSourceId::Codex
            | UsageSourceId::ZCode
            | UsageSourceId::WorkBuddy
            | UsageSourceId::OpenCode
    );
    SemanticsPolicy {
        declared: declared_default(source),
        allow_arithmetic: allow,
    }
}

/// 算术判据（说明书 §4.2 规则 1）。**先判互斥**：`cached == 0` 时两式同真，
/// 按规则 2（拿不准一律互斥）落互斥；opencode V1 的「全项相加」形态也判互斥。
/// 返回 None = 本记录无可用判据 → 调用方落声明默认。
///
/// **耦合注记（W-07）**：下面 `exclusive_form_with_reasoning` 这个分支与「严格四桶式」
/// **在本仓当前配置下不可区分，只依赖一条常量**——`declared_default` 对**每个** arithmetic-enabled
/// 源恒为 `Exclusive`（七源声明默认一律互斥，见 `declared_default`）。
/// 因此即使某条记录只被含 reasoning 的形态命中、而严格四桶式判 `None`，
/// `resolve_semantics` 也会因声明默认是 `Exclusive` 而给出**同一个值**。
/// **将来若有源声明 `Subset` 且允许算术判据**（本机七源都不是），该分支立刻变成可观测的差异点，
/// 届时必须用真机数据裁决「该源的 total 是否把 reasoning 也加进去了」，不得沿用本注记。
pub fn decide_by_arithmetic(raw: &RawUsage) -> Option<CacheSemantics> {
    let t = raw.total_raw?;
    let exclusive_form = raw.input_raw + raw.cache_read + raw.cache_write + raw.output;
    let exclusive_form_with_reasoning = exclusive_form + raw.reasoning;
    if t == exclusive_form || t == exclusive_form_with_reasoning {
        return Some(CacheSemantics::Exclusive);
    }
    if t == raw.input_raw + raw.output {
        return Some(CacheSemantics::Subset);
    }
    None
}

/// 判据优先、声明兜底（规则 1 + 规则 2）
pub fn resolve_semantics(policy: SemanticsPolicy, raw: &RawUsage) -> CacheSemantics {
    if policy.allow_arithmetic {
        if let Some(sem) = decide_by_arithmetic(raw) {
            return sem;
        }
    }
    policy.declared
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedUsage {
    pub buckets: UsageBuckets,
    pub request_total: i64,
}

/// 四桶归一 + 请求输入派生（说明书 §4.2 表）。
/// 规范桶口径：`input_fresh` 一律是**未缓存输入**，`output` 含 reasoning。
/// 返回 None = stub / TotalOnly / 无效记录（整条剔除，不计入任何指标）。
pub fn normalize(raw: &RawUsage, sem: CacheSemantics) -> Option<NormalizedUsage> {
    if sem == CacheSemantics::TotalOnly {
        return None;
    }
    if raw.input_raw == 0 && raw.cache_read == 0 && raw.cache_write == 0 && raw.output == 0 {
        // 四桶全 0：total 非 0 是 stub（剔除）；total 也是 0/无 → 真空回合（保留，计一次请求）
        if raw.total_raw.unwrap_or(0) != 0 {
            return None;
        }
    }
    let (input_fresh, request_total) = match sem {
        CacheSemantics::Exclusive => (
            raw.input_raw,
            raw.input_raw + raw.cache_read + raw.cache_write,
        ),
        CacheSemantics::Subset => {
            let fresh = (raw.input_raw - raw.cache_read).max(0);
            (fresh, raw.input_raw) // Subset 请求输入 = input 原文（= fresh + cache_read）
        }
        CacheSemantics::TotalOnly => return None,
        // 补录是**来源标记**、不是语义：真走到这里说明有人拿它当语义用 —— 拒绝归一
        //（不猜一个语义出来；补录行由采集器按 Exclusive 构造好四桶）。
        CacheSemantics::Backfill => return None,
    };
    Some(NormalizedUsage {
        buckets: UsageBuckets {
            input_fresh,
            cache_read: raw.cache_read,
            cache_write: raw.cache_write,
            output: raw.output + raw.reasoning,
        },
        request_total,
    })
}

/// 命中率 = cache_read ÷ 请求输入；分母 0 记 0（不得 NaN / 空）
pub fn cache_hit_rate(cache_read: i64, request_total: i64) -> f64 {
    if request_total <= 0 {
        return 0.0;
    }
    cache_read as f64 / request_total as f64
}

/// hero 大数字 = 请求输入 + 产出（不含缓存写二次相加、不含用户输入估算）
pub fn hero_of(request_total: i64, output: i64) -> i64 {
    request_total + output
}

/// 用户输入(估)：CJK 每字 1 + ASCII 每词 1（沿用原插件；只对能读到用户文本的源计算）
pub fn user_est_of(text: &str) -> i64 {
    let mut cjk = 0i64;
    let mut words = 0i64;
    let mut in_word = false;
    for ch in text.chars() {
        let c = ch as u32;
        let is_cjk = matches!(
            c,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xAC00..=0xD7AF
        );
        if is_cjk {
            cjk += 1;
            in_word = false;
        } else if ch.is_alphanumeric() {
            if !in_word {
                words += 1;
                in_word = true;
            }
        } else {
            in_word = false;
        }
    }
    cjk + words
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::model::UsageSourceId;

    fn raw(input: i64, read: i64, write: i64, out: i64, total: Option<i64>) -> RawUsage {
        RawUsage {
            input_raw: input,
            cache_read: read,
            cache_write: write,
            output: out,
            reasoning: 0,
            total_raw: total,
        }
    }

    /// 判据（说明书 §4.2 规则 1）：codex 的 `total_tokens` 算术，两式**先判互斥**（cached==0
    /// 时两式同真 → 按规则 2 落互斥）。禁止用 `cached > input` 单条判据（本机查准 100% 但漏判 4.5%）。
    #[test]
    fn arithmetic_criterion_prefers_exclusive_on_tie() {
        // t == input + cached + output → 互斥
        assert_eq!(
            decide_by_arithmetic(&raw(200, 800, 0, 200, Some(1200))),
            Some(CacheSemantics::Exclusive)
        );
        // t == input + output 且 cached != 0 → 包含
        assert_eq!(
            decide_by_arithmetic(&raw(1000, 800, 0, 200, Some(1200))),
            Some(CacheSemantics::Subset)
        );
        // cached == 0：两式同真 → 并列时按互斥（规则 2：拿不准一律互斥）
        assert_eq!(
            decide_by_arithmetic(&raw(1000, 0, 0, 200, Some(1200))),
            Some(CacheSemantics::Exclusive)
        );
        // 无 total → 无判据
        assert_eq!(decide_by_arithmetic(&raw(1000, 0, 0, 200, None)), None);
        // 判据不成立（total 与任何一式都不等）→ 无判据，落声明默认
        assert_eq!(decide_by_arithmetic(&raw(1000, 0, 0, 200, Some(7))), None);
        // opencode V1 形态：total = input + output + reasoning + read + write → 互斥
        let oc = RawUsage {
            input_raw: 18621,
            cache_read: 62976,
            cache_write: 0,
            output: 538,
            reasoning: 781,
            total_raw: Some(82916),
        };
        assert_eq!(decide_by_arithmetic(&oc), Some(CacheSemantics::Exclusive));
    }

    /// 逐源声明默认与判据开关（说明书 §4.2 规则 5 / 矩阵 §3.2，**已定案**）：
    /// * **七源声明默认一律 `Exclusive`**（`declared_default` 恒回互斥）；
    /// * **允许算术判据**（`allow_arithmetic == true`）的是 **codex / zcode / workbuddy / opencode**
    ///   四个——与本文件 `default_policy` 的 `matches!` 列表**逐字对齐**（注释只描述代码，不再另立结论）；
    /// * 判据在场则逐条改判（规则 1），判据缺失才落声明默认（规则 2/5）。
    ///   **opencode 本机未取得可用判据**（矩阵 §3.2）→ 它在允许清单里但实际总落声明默认，不是矛盾。
    #[test]
    fn declared_defaults_match_matrix() {
        for s in UsageSourceId::ALL {
            assert_eq!(
                declared_default(s),
                CacheSemantics::Exclusive,
                "{:?} 的声明默认必须互斥",
                s
            );
        }
        // 允许算术判据的四个源（与 default_policy 的 matches! 列表逐字一致）
        for s in [
            UsageSourceId::Codex,
            UsageSourceId::ZCode,
            UsageSourceId::WorkBuddy,
            UsageSourceId::OpenCode,
        ] {
            assert!(default_policy(s).allow_arithmetic, "{:?} 允许判据", s);
        }
        // 字段名即互斥的三个源：不参与算术判定
        for s in [
            UsageSourceId::Claude,
            UsageSourceId::Kimi,
            UsageSourceId::Dsh,
        ] {
            assert!(!default_policy(s).allow_arithmetic, "{:?} 无 total 判据", s);
        }
        // 判据缺失（total_raw = None）→ 一律落声明默认互斥（含 opencode）
        assert_eq!(
            resolve_semantics(
                default_policy(UsageSourceId::OpenCode),
                &raw(1000, 0, 0, 200, None)
            ),
            CacheSemantics::Exclusive
        );

        // ---- `allow_arithmetic` 是**真的开关**（说明书 §4.2 规则 5：claude/kimi/dsh 不参与算术判定；
        //      GC 14：「显式开关，一处可切回」）。上面全部断言只证明了「开关为 true 时判据生效」，
        //      反向（开关为 false 时**判据必须被忽略**）此前**零覆盖**——把 `resolve_semantics` 里的
        //      `if policy.allow_arithmetic` 守卫整个删掉（退化成 `decide_by_arithmetic(..).unwrap_or(declared)`），
        //      本用例原先照样全绿（评审 Important #2 的假绿）。下面两条把它锁死。
        // ① 判别器：claude 声明互斥且**不开**算术 → 尽管该记录算术上判 `Subset`（1000+200 == 1200），
        //    结果仍必须是声明的 `Exclusive`。若守卫被删 → 这里会得到 `Subset` → 红。
        let claude_but_arithmetic_says_subset = raw(1_000, 800, 0, 200, Some(1_200));
        assert_eq!(
            decide_by_arithmetic(&claude_but_arithmetic_says_subset),
            Some(CacheSemantics::Subset),
            "前提断言：该记录**确实**带可用判据，且判据指向 Subset"
        );
        assert_eq!(
            resolve_semantics(
                default_policy(UsageSourceId::Claude),
                &claude_but_arithmetic_says_subset
            ),
            CacheSemantics::Exclusive,
            "claude 不开算术判据 → 必须原样落声明默认，不得被判据改判"
        );
        // ② 反向控制：声明 `Subset` 且**不开**算术 + 一个算术上判 `Exclusive` 的记录
        //    （200+800+0+200 == 1200）→ 结果必须是声明的 `Subset`。两个方向都锁住后，
        //    任何「忽略开关」的实现都无法同时满足 ① 与 ②。
        assert_eq!(
            decide_by_arithmetic(&raw(200, 800, 0, 200, Some(1_200))),
            Some(CacheSemantics::Exclusive),
            "前提断言：该记录**确实**带可用判据，且判据指向 Exclusive"
        );
        assert_eq!(
            resolve_semantics(
                SemanticsPolicy {
                    declared: CacheSemantics::Subset,
                    allow_arithmetic: false,
                },
                &raw(200, 800, 0, 200, Some(1_200))
            ),
            CacheSemantics::Subset,
            "开关关闭时声明值必须原样胜出（含非 Exclusive 的声明）"
        );
    }

    /// 黄金用例（说明书 §4.2 规则 3 + §10）：本机 Codex 全量 35,935 条事件中
    /// 「包含」16,900 条、「互斥」16,298 条（31 个文件内两种语义混存）。
    /// 语料按**该计数**构造，逐条常量按本机量级取值。
    /// **本用例锁的是「三种口径的相对次序」与「一律包含 >100% 物理不可能」这条反例**，
    /// **不是**真机比率的数值复刻：合成语料按「条数」而非「逐条 token 量级分布」构造，
    /// 故 `all_subset` 实测 182.63% 与真机 201.69% 差约 19pt（评审 Minor #5 指出的错误说法已更正）。
    /// 真机精确比率（92.69% / 201.69% / 66.85%）的回归另见 Task 12 的 `#[ignore]` 真机全量测试（92.69% ± 0.5pt）。
    #[test]
    fn codex_golden_ratios() {
        const SUBSET_N: usize = 16_900; // 本机实测「包含」条数
        const EXCLUSIVE_N: usize = 16_298; // 本机实测「互斥」条数
                                           // 包含型：input 已含 cache_read（100000 含 95000），total = input + output
        let subset_raw = raw(100_000, 95_000, 0, 500, Some(100_500));
        // 互斥型：input 不含 cache_read，total = input + cached + output
        let exclusive_raw = raw(5_000, 100_000, 0, 500, Some(105_500));

        // (cache_read 合计, 正确判据请求输入, 一律包含请求输入, 一律互斥请求输入)
        let mut acc = (0i64, 0i64, 0i64, 0i64);
        // `let mut feed` 的 `mut` 是任务书原稿的多余绑定：闭包本身不捕获任何可变状态，
        // 累加器是**按引用参数**传入的。`-D warnings` 下 `unused_mut` 是 error（同 D-05 先例），故删。
        let feed = |raw: &RawUsage, expected: CacheSemantics, acc: &mut (i64, i64, i64, i64)| {
            let sem = resolve_semantics(default_policy(UsageSourceId::Codex), raw);
            assert_eq!(sem, expected, "逐条判定必须识别出该条语义");
            let n = normalize(raw, sem).unwrap();
            let as_subset = normalize(raw, CacheSemantics::Subset).unwrap();
            let as_exclusive = normalize(raw, CacheSemantics::Exclusive).unwrap();
            acc.0 += n.buckets.cache_read;
            acc.1 += n.request_total;
            acc.2 += as_subset.request_total;
            acc.3 += as_exclusive.request_total;
        };
        for _ in 0..SUBSET_N {
            feed(&subset_raw, CacheSemantics::Subset, &mut acc);
        }
        for _ in 0..EXCLUSIVE_N {
            feed(&exclusive_raw, CacheSemantics::Exclusive, &mut acc);
        }
        let (c, r_correct, r_all_subset, r_all_exclusive) = acc;

        let correct = c as f64 / r_correct as f64;
        let all_subset = c as f64 / r_all_subset as f64;
        let all_exclusive = c as f64 / r_all_exclusive as f64;
        // 本实现下的实测值（实现者用瞬时探针跑出、已删探针）：
        //   c = 3_235_300_000, r_correct = 3_401_290_000, r_all_subset = 1_771_490_000,
        //   r_all_exclusive = 5_006_790_000
        //   → correct = 0.9512 / all_subset = 1.8263 / all_exclusive = 0.6462
        // 三者**次序与量级**与真机一致（>100% 的荒谬值 / 偏低 / 居中），
        // 与真机 92.69% / 201.69% / 66.85% 的**绝对差**来自合成语料按「条数」而非按
        // 「逐条 token 量级分布」构造（真机的包含型 input 只有 2 千 ~2 万量级，本用例取 10 万），
        // 精确回归由 Task 12 的 `#[ignore]` 真机全量测试负责（92.69% ± 0.5pt）。
        // 正确判据必须落在断言带 0.92..=0.96 内（实测 0.9512，与真机 92.69% 差 2.4pt）
        assert!(
            (0.92..=0.96).contains(&correct),
            "正确判据命中率 {correct:.4}，应接近 0.9269"
        );
        // 一律按包含 → >100%（物理不可能）——反例断言（说明书 §10 明文要求）
        assert!(all_subset > 1.0, "一律包含必须 >100%，实测 {all_subset:.4}");
        // 一律按互斥 → 显著偏低（实测 66.85%）
        assert!(
            all_exclusive < 0.70,
            "一律互斥必须 <70%，实测 {all_exclusive:.4}"
        );
        assert!(
            all_exclusive < correct && correct < all_subset,
            "三种口径的次序必须成立"
        );
    }

    /// 归一化：Subset 的 input_fresh = input - cache_read；Exclusive 的 input_fresh = input；
    /// 请求输入按语义分母不同（Exclusive 含缓存写，Subset 等于 input 原文）
    #[test]
    fn normalize_shapes_by_semantics() {
        let r = raw(1_000, 800, 100, 200, None);
        let ex = normalize(&r, CacheSemantics::Exclusive).unwrap();
        assert_eq!(
            ex.buckets,
            UsageBuckets {
                input_fresh: 1_000,
                cache_read: 800,
                cache_write: 100,
                output: 200
            }
        );
        assert_eq!(ex.request_total, 1_900);
        let sub = normalize(&r, CacheSemantics::Subset).unwrap();
        assert_eq!(
            sub.buckets.input_fresh, 200,
            "Subset 下未缓存输入 = input - cache_read"
        );
        assert_eq!(sub.buckets.cache_read, 800);
        assert_eq!(sub.request_total, 1_000, "Subset 请求输入 = input 原文");

        // ---- §4.1「reasoning 一律并入 output」：任务书里 `raw()` 助手把 reasoning 硬写 0，
        //      全仓唯一 `reasoning != 0` 的记录只喂给 `decide_by_arithmetic`、**从不进 `normalize`**
        //      → 把 `normalize` 里的 `output: raw.output + raw.reasoning` 退化成 `output: raw.output`，
        //      本文件原先 8 条用例**全绿**（评审 Important #3 的假绿）。这条直接构造 `RawUsage` 把它锁死。
        //      取真机量级（opencode V1 抽样 reasoning=781，见 research/OpenCode-V2-schema-查证.md:362）。
        let with_reasoning = RawUsage {
            input_raw: 100,
            cache_read: 0,
            cache_write: 0,
            output: 20,
            reasoning: 781,
            total_raw: None,
        };
        let nz = normalize(&with_reasoning, CacheSemantics::Exclusive).unwrap();
        assert_eq!(
            nz.buckets.output, 801,
            "产出桶必须含 reasoning（20 + 781），否则 hero 与各源自身 total 对不上（§4.1）"
        );
        // reasoning **只**并入产出桶，**不得**进入请求输入（= 命中率分母）与其它桶。
        assert_eq!(
            nz.request_total, 100,
            "reasoning 不得进入请求输入（命中率分母）"
        );
        assert_eq!(
            nz.buckets.input_fresh, 100,
            "reasoning 不得进入未缓存输入桶"
        );
        assert_eq!(nz.buckets.cache_read, 0);
        assert_eq!(nz.buckets.cache_write, 0);
        // 产出桶总数（含 reasoning）必须等于四桶之和里属于产出的部分 → hero = 请求输入 + 产出
        assert_eq!(hero_of(nz.request_total, nz.buckets.output), 901);
    }

    /// stub 剔除（说明书 §5.3.5）：四桶全 0 而 total 非 0 → None（本机 840 条 / 86 文件）
    #[test]
    fn stub_records_are_dropped() {
        assert!(normalize(&raw(0, 0, 0, 0, Some(1234)), CacheSemantics::Exclusive).is_none());
        // 四桶全 0 且 total 也 0 → 真空回合，**不能丢**（claude 14.1% 样本如此）
        assert!(normalize(&raw(0, 0, 0, 0, Some(0)), CacheSemantics::Exclusive).is_some());
        assert!(normalize(&raw(0, 0, 0, 0, None), CacheSemantics::Exclusive).is_some());
    }

    /// TotalOnly（运行时兜底，说明书 §4.2 表下注）：只有总量无四桶 → 不参与四桶与命中率
    #[test]
    fn total_only_yields_no_buckets() {
        // ① 原断言（任务书 Step 1 原文）：四桶全 0 + total 非 0 —— 它**同时**满足 stub 规则，
        //    因此**只锁得住 stub 规则，锁不住 TotalOnly 分支**（评审 Important #1 的假绿：
        //    删掉 `normalize` 开头的 TotalOnly 早退、并把 match 里的 TotalOnly 分支改成产出桶值，
        //    这一条照样通过）。保留它作为 **stub 规则**的锁。
        assert!(normalize(&raw(0, 0, 0, 0, Some(9)), CacheSemantics::TotalOnly).is_none());
        // ② 真正的 TotalOnly 锁：**四桶非零**，故必然穿过 stub 早退，只能由 TotalOnly 分支返回 None。
        //    `raw(1_000, 500, 0, 200, Some(1_700))` 四桶 = 1000/500/0/200，无一件为 0 → 不走 stub 规则。
        assert!(
            normalize(
                &raw(1_000, 500, 0, 200, Some(1_700)),
                CacheSemantics::TotalOnly
            )
            .is_none(),
            "四桶非零的 TotalOnly 记录必须被整条剔除（只有总量、无四桶拆分 → 不参与四桶与命中率）"
        );
        // 前提断言（防假绿）：同一输入在 Exclusive 下**确实产出桶值**——
        // 证明上面那条 None 不是「因为无论如何都是 None」而侥幸通过。
        let same = normalize(
            &raw(1_000, 500, 0, 200, Some(1_700)),
            CacheSemantics::Exclusive,
        );
        assert!(
            same.is_some(),
            "前提断言：同一四桶非零输入在 Exclusive 下必须产出桶值"
        );
        assert_eq!(same.unwrap().request_total, 1_500);
    }

    /// 命中率分母为 0 记 0（不得 NaN / 空）；hero = 请求输入 + 产出
    #[test]
    fn hit_rate_and_hero_edges() {
        assert_eq!(cache_hit_rate(0, 0), 0.0);
        assert_eq!(cache_hit_rate(50, 200), 0.25);
        assert_eq!(hero_of(1_000, 250), 1_250);
    }

    /// 用户输入(估)：CJK 每字 1 + ASCII 每词 1（沿用原插件口径）
    #[test]
    fn user_est_counts_cjk_chars_and_ascii_words() {
        // 4 个 CJK 字（你/好/世/界）+ 1 个 ASCII 词（world）= 5。
        // 任务书原文此处写 6 并注「2 个 ASCII 词」，但该字符串里只有 1 个 ASCII 词——
        // 是原稿的算术笔误（把另一条用例的 `hello world` 抄了过来），故按同一公式修正期望值。
        assert_eq!(user_est_of("你好 world 世界"), 5);
        assert_eq!(user_est_of(""), 0);
        assert_eq!(user_est_of("hello-world foo_bar"), 4); // 标点断开：hello/world/foo/bar
        assert_eq!(user_est_of("修复 bug 并跑 test"), 6); // 修复(2) + bug + 并(1) + 跑(1) + test
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
