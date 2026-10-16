// 首屏概要：hero 大数字 + 7 行固定顺序指标网格 + 页脚三件套（计划② Task 6 步骤 6）。
//
// 数字口径（spec P8，逐条不得发挥）：
//  * hero = `fmtInt(dash.hero)`：**完整千分位**、不用万/亿；值由后端给（`hero_of` = 请求输入 + 产出），
//    前端**不自算**（四桶在 Subset 语义下可能重叠，反推会重复计数）。
//  * 常规 token 数值（用户输入估 / 产出 / 请求输入 / 缓存命中）= `fmtTokens` 缩写（万/亿），
//    `title` 给精确值——「所有缩写数值都必须在 hover 时给出精确值」。
//  * 命中率 = `fmtPct`（0–1 分数口径，全仓唯一）。
//  * 环比**由本组件现场算**：`prevOf(dash.compare)` + `compareSuffix` / `hitCompareSuffix`
//    （契约只给上一周期聚合，差值全在前端算；§3 第 17 条：**不为此发第二个查询**）。
//    `compare === null` 只显示当前值：不得出现 `0%` / `NaN` / `Infinity`；上一周期为 0 → 「新增」。
//  * `userEst === null` → `EM_DASH`（`~` 前缀只在有值时加）；`collectedAt === 0` 是「尚未采集」哨兵
//    （§3 第 8 条）→ asOf 与页脚 freshness 都渲染 `usage.notCollected`，不是 `—`。
//  * **没量到 token 的窗口**（2026-10-06 用户裁决「只有是真实的数值，你才能写它的数值。如果没有
//    产生数值，就不显示了」）：本窗口四个 token 桶全零时，hero 与**四个由 token 桶派生的行**
//    （产出 / 请求输入(全文累计) / 缓存命中 / 命中率）一律 `EM_DASH` ——
//    真机的**真空回合**行就是这种形状（四桶全零、但 `requests ≥ 1`）。**计数类照常显示真值**
//    （请求次数是量出来的），asOf 也不受影响。判据只一处：`range.ts::tokenUnmeasured`。
//    该类行同样**不出环比段**：那个 0 不是测量值，拿它算出来的差不是真值（`— ↑100.0%` 自相矛盾）。
//  * ⚠️ **`用户输入(估)` 不在这四个之列**（2026-10-06 第三轮用户裁决）：它取自**用户打的字**
//    （`collectors/{claude,codex,kimi}.rs` 三源独立估），**不是**从四个 token 桶推出来的 ⇒
//    「四桶全零」**不蕴含**「用户没打字」（真机账本实测：`claude | requests=1 | 四桶全零 |
//    user_est = 7`，15 条真空回合行里 4 条带真值）。把它一并抹成 `—` 等于把一个**真值**藏起来，
//    与上面那条规则反方向 ⇒ 它**只**保留「不可得（`null`）才空」这一条既有分支（见 `tokenRow`
//    的 `hideWhenUnmeasured`）。
//
// 无 `prev` prop：环比只此一处数据源（`dash.compare`），多一个入参就会有两套口径。
import { Card, CardContent } from "@/components/ui/card";
import { EM_DASH, fmtInt, fmtPct, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import {
  asOfText,
  compareSuffix,
  hitCompareSuffix,
  prevOf,
  tokenUnmeasured,
  type TFn,
} from "@/lib/usage/range";
import type { UsageDashboard } from "@/types/usage";

/** 7 行指标网格的键；键同时是 i18n 后缀与 testid 后缀，**`rows` 数组的顺序即 spec P1 §3 的固定顺序** */
type GridKey =
  "userEst" | "output" | "requestTotal" | "cacheRead" | "hitRate" | "requests" | "asOf";

interface GridRow {
  key: GridKey;
  /** 主值（缩写值或精确值） */
  value: string;
  /** 缩写值的精确值（进 `title`）；空串 = 主值本身已是精确值 */
  exact: string;
  /** 环比段（**屏上只有差值**）；空串 = 整段不渲染（`compare === null` 或该项不可比） */
  compare: string;
  /**
   * 环比段的 `title`（`usage.compareHint` + 上一周期值的**精确值**）；空串 = 不挂。
   *
   * 上一周期的值为什么只在 `title` 里（2026-10-06 用户裁决「同比冗余字太多」）：屏上 8 处
   * （7 行指标 + 趋势区）都印「（上一周期 X · Y）」会把排版挤爆——真机取证：值被压到**折行**
   * （`6.41` 与 `亿` 分两行）、标签被截成 `请求输入(全文…`。现在屏上只留差值，上期值 hover 可得。
   */
  compareTitle: string;
}

export function UsageSummary({
  dash,
  retentionDays,
  t,
}: {
  dash: UsageDashboard;
  /** 明细保留期设置（天）。`null` = 设置还没回来 / 后端没给 → **整段不出**，不填 0、不猜默认 */
  retentionDays: number | null;
  t: TFn;
}) {
  // 格式化语言（issue #160）：数值单位（万/亿 vs K/M/B）随界面语言走，组件体取一次。
  const lang = getLang();
  // 环比：上一周期聚合 → 各指标差值（`compare === null` → `null` → 各项对比段整段不渲染）
  const prev = prevOf(dash.compare);

  // 本窗口是否**真的量到过 token**（见文件头最后两条）：为真 ⇒ **四个 token 桶派生的位**出 `—`、
  // 且那些位不做环比（hero / 产出 / 请求输入 / 缓存命中 / 命中率）。`用户输入(估)` 不在此列。
  const unmeasured = tokenUnmeasured(dash);
  /** 不可得的 token 行（与 `userEst === null` 同一套空态形状：值 `—`、无精确值、无环比） */
  const blankRow = (key: GridKey): GridRow => ({
    key,
    value: EM_DASH,
    exact: "",
    compare: "",
    compareTitle: "",
  });

  /**
   * token 行：缩写 + hover 精确值；`prefix` 承载 userEst 的 `~`（只在有值时加）。
   *
   * `hideWhenUnmeasured`（2026-10-06 第三轮用户裁决）区分两类调用方：
   *  * `true`（默认）= **由四个 token 桶派生的行**（产出 / 请求输入 / 缓存命中）——四桶全零 ⇒ `—`，
   *    也不拿 0 去算环比（`— ↑100.0%` 是自相矛盾的读数）；
   *  * `false` = **`用户输入(估)`**——它从**用户文本**估出来、与四个 token 桶无关：本窗口没量到
   *    token 不代表用户没打字（真机：`requests=1 | 四桶全零 | user_est = 7`）⇒ 真值照常显示，
   *    环比也照常算（当前值与上一周期值都是真值）。它的空态只有「不可得」一条（`=== null`）。
   */
  const tokenRow = (
    key: GridKey,
    cur: number,
    prevValue: number | null | undefined,
    prefix = "",
    hideWhenUnmeasured = true
  ): GridRow =>
    hideWhenUnmeasured && unmeasured
      ? blankRow(key) // 没量到 token ⇒ 不显示数值，也不拿 0 去算环比
      : {
          key,
          value: prefix + fmtTokens(cur, lang),
          exact: prefix + fmtInt(cur),
          // prev 为 0 → compareSuffix 出「新增」；prev 为 null/非有限 → 空串（只显示当前值）
          compare: prevValue == null ? "" : compareSuffix(cur, prevValue, t),
          // 上期值只在 hover 里给，且给的是**精确千分位**（与 `exact` 同口径，不用万/亿缩写）
          compareTitle:
            prevValue == null ? "" : t("usage.compareHint", { prev: prefix + fmtInt(prevValue) }),
        };

  const userEst = dash.totals.userEst;
  // 采集时刻：`collectedAt === 0` =「尚未采集」哨兵（开关关闭 / 本次运行尚未采集）→ usage.notCollected。
  // 网格 asOf 行与页脚 freshness 是**同一个值**，故算一次、两处用（口径唯一出处 `range.ts::asOfText`）。
  const collected = asOfText(dash.collectedAt, t);
  const rows: GridRow[] = [
    userEst === null
      ? // 不可得 ≠ 0（§3 第 5 条）：空态出 `—`，且当前值不可得时不做环比
        blankRow("userEst")
      : // `hideWhenUnmeasured = false`：**估计值不受「没量到 token」影响**（见 tokenRow 的注释）
        tokenRow("userEst", userEst, prev?.metrics.userEst ?? null, "~", false),
    tokenRow("output", dash.totalsBuckets.output, prev?.buckets.output),
    tokenRow("requestTotal", dash.totals.requestTotal, prev?.metrics.requestTotal),
    tokenRow("cacheRead", dash.totalsBuckets.cacheRead, prev?.buckets.cacheRead),
    unmeasured
      ? blankRow("hitRate")
      : {
          key: "hitRate",
          value: fmtPct(dash.totals.cacheHitRate),
          exact: "",
          // 百分点差 = (cur - prev) × 100，由 hitCompareSuffix 算；compare === null → 空串
          compare: hitCompareSuffix(dash.totals.cacheHitRate, dash.compare, t),
          compareTitle: dash.compare
            ? t("usage.compareHint", { prev: fmtPct(dash.compare.prevMetrics.cacheHitRate) })
            : "",
        },
    {
      key: "requests",
      value: fmtInt(dash.totals.requests),
      exact: "",
      compare: compareSuffix(dash.totals.requests, prev?.metrics.requests ?? null, t),
      compareTitle:
        prev?.metrics.requests == null
          ? ""
          : t("usage.compareHint", { prev: fmtInt(prev.metrics.requests) }),
    },
    {
      key: "asOf",
      value: collected,
      exact: "",
      compare: "",
      compareTitle: "",
    },
  ];

  return (
    <Card>
      <CardContent className="space-y-3">
        <div>
          <div className="text-3xl font-semibold tabular-nums" data-testid="usage-hero">
            {unmeasured ? EM_DASH : fmtInt(dash.hero)}
          </div>
          <p className="text-muted-foreground mt-1 text-xs">{t("usage.hero.sub")}</p>
        </div>

        <div className="grid gap-x-8 gap-y-1 sm:grid-cols-2">
          {rows.map((row) => (
            <div
              key={row.key}
              data-testid={`usage-grid-${row.key}`}
              /* 单元格 = **三列 grid**（标签 / 值 / 环比）。为什么不是 `flex justify-between`
                 （2026-10-06 排版修复）：长环比与长标签会互相挤压——真机取证是**值被压到折行**
                 （`6.41` 与 `亿` 分两行）、标签被截成 `请求输入(全文…`，且两列的值右边缘参差。
                 三列定死各自的槽位后：值恒 `whitespace-nowrap` 不折行、环比占**固定宽**（空串也占位
                 ⇒ 所有行的值右边缘对齐）。 */
              className="grid grid-cols-[minmax(0,1fr)_auto_4.75rem] items-baseline gap-x-2 text-sm"
            >
              <span className="text-muted-foreground truncate">{t(`usage.grid.${row.key}`)}</span>
              <span
                className="text-right font-medium whitespace-nowrap tabular-nums"
                title={row.exact || undefined}
              >
                {row.value}
              </span>
              {/* 环比：屏上**只有差值**；上一周期的值在 `title` 里（`usage.compareHint`）。
                  空串 → 渲染空 span **占住槽位**，否则该行的值会贴到右边缘、与其它行错开。 */}
              <span
                className="text-muted-foreground text-right text-xs whitespace-nowrap"
                title={row.compareTitle || undefined}
              >
                {row.compare}
              </span>
            </div>
          ))}
        </div>
      </CardContent>

      {/* 页脚口径三件套：口径常驻 + **环比基准声明一次** + 保留期（设置给了才出）。
          ⚠️ 这里**不再**出「数据新鲜度：<时刻>」——它与网格的 `asOf` 行是**同一个 `collected`**
          （真机截图里同一个时间戳在同一张卡上出现了两遍，2026-10-06 排版修复删掉了页脚那处）；
          腾出的位置放「环比基准」——屏上 8 处环比现在都只印差值，基准必须**在卡上声明一次**，
          否则「+2562.3%」没有可解释的参照物。 */}
      <footer data-testid="usage-footer" className="text-muted-foreground px-6 text-xs">
        <span>{t("usage.footer.caliber")}</span>
        <span> · {t("usage.footer.baseline")}</span>
        {retentionDays != null ? (
          <span> · {t("usage.footer.retention", { days: retentionDays })}</span>
        ) : null}
      </footer>
    </Card>
  );
}
