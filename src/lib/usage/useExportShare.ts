// 分享图装配与落盘（计划② Task 12 步骤 6）——把「当前看板数据」装配成布局入参、出图、走 Task 10 的
// 落盘接缝。**唯一消费方** = `UsageExportActions.tsx`（硬约束：只有它能 import 本 hook / 页面与其它
// 组件一律不得 import）。
//
// 五条口径（逐条不得发挥）：
//  * **图集只 fetch 一次**：`activePetSheet()` 只回答「当前宠物是谁 + 行数」，`loadSheet()` 一次
//    `fetch → blob → createImageBitmap` 同时产出**几何与位图**（同源，不可能一处新一处旧）；
//    位图**不进布局层**（`ExportInput.sheet` 只吃 `{ rows, geometry }`）——这是刻意分层。
//  * 6 行指标取 `usage.grid.*`（`requestInput → cacheRead → cacheMiss → output → hitRate → requests`，
//    与 A 总览同序），值用**精确千分位**（与 hero 同口径：摘要有列宽压力才缩写，图片没有）；
//    `asOf` **不进**指标行——它归 `heroSub`（`collectedAt === 0` → `usage.notCollected`，§3 第 8 条）。
//    V2 已删除「用户输入(估)」指标（不是 API Token）。
//  * `heroLabel` 取 `usage.hero.label`（**不是** `usage.card.trend`：用了它全图会连着出现两次
//    「用量趋势」）；品牌 = `` `兔维斯 · ${t("usage.title")}` ``（**不新造** `summaryTitle`）。
//  * 不可得一律 `EM_DASH`，**绝不回退填 0**（`workSummary` 四条：`toolCalls` / `toolAvgMs` /
//    `topTool` / `topToolMs`；§3 第 5 条）。
//  * 命中率取 `format.ts` 的 `fmtPct`（**全仓唯一一套** 0–1 分数口径：1 位小数 + `%`）。
//    这里早先自产过一个 `fmtPctText`，理由是「免得把 `format.ts → agentBadge.tsx` 那条 React 链
//    拖进导出面」——该理由在本文件 import `format.ts` 的那一刻即已作废（下方一行就取用了它的多个
//    符号），留着副本只会多一处口径漂移点，故删副本、改用唯一出处。
import { trendPoints } from "@/components/usage/TrendChart";
import { usageGetSettings } from "@/lib/api/usage";
import { distributionRows } from "@/lib/usage/distribution";
import { defaultExportDeps, renderShareImage } from "@/lib/usage/exportImage";
import { GROUPS_MAX, type ExportInput } from "@/lib/usage/exportLayout";
import { labelOf, pngFilename } from "@/lib/usage/exportText";
import { EM_DASH, fmtDur, fmtInt, fmtPct } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { pickQuote, type QuoteVars } from "@/lib/usage/quotes";
import type { QuoteAgg } from "@/lib/usage/quotePool";
import { asOfText, rangeLabelOf, spanLabelOf, tokenUnmeasured, type TFn } from "@/lib/usage/range";
import { copyImage as copyImageClipboard, saveBytes, type SaveOutcome } from "@/lib/usage/saveFile";
import { activePetSheet, loadSheet, type SheetBitmap } from "@/lib/usage/sheet";
import type { UsageDashboard, UsageSettings } from "@/types/usage";

/**
 * 趋势方向（**末两点**比高低；本计划无其它生产者，故定义在本文件——控制器裁决 D-2）：
 * `null` = **算不出方向**（点不足 2 个，或末两点相等，§3 第 5 条）——评语池的两条趋势档都**不**命中，
 * 落兜底；绝不把「说不上来」印成「在收敛」。
 */
function trendUpOf(points: readonly { value: number }[]): boolean | null {
  if (points.length < 2) return null;
  const prev = points[points.length - 2].value;
  const last = points[points.length - 1].value;
  if (last === prev) return null;
  return last > prev;
}

/** 指标 6 行：顺序与 A 总览一致（请求输入 / 缓存命中 / 未缓存命中 / 产出 / 命中率 / 请求次数） */
function metricRows(dash: UsageDashboard, t: TFn): [string, string][] {
  const label = (key: string) => t(`usage.grid.${key}`);
  // **没量到 token** 的窗口：由 token 桶派生的行一律 `—`、**绝不印 0**；请求次数出真值。
  const unmeasured = tokenUnmeasured(dash);
  const token = (v: number) => (unmeasured ? EM_DASH : fmtInt(v));
  return [
    [label("requestInput"), token(dash.totals.requestTotal)],
    [label("cacheRead"), token(dash.totalsBuckets.cacheRead)],
    // 与 A 总览同公式
    [label("cacheMiss"), token(dash.totals.requestTotal - dash.totalsBuckets.cacheRead)],
    [label("output"), token(dash.totalsBuckets.output)],
    [label("hitRate"), unmeasured ? EM_DASH : fmtPct(dash.totals.cacheHitRate)],
    [label("requests"), fmtInt(dash.totals.requests)],
  ];
}

/**
 * 工具 2×2 = `workSummary` 的四个现成字段（`toolCalls` / `toolAvgMs` / `topTool` / `topToolMs`），
 * 各自 `null` → `EM_DASH`。`topTool.name` / `topToolMs.name` 是**工具调用名**（后端 `tool_stats` 的键，
 * 如 `Bash`），**原样**渲染——套 `usageAgentLabel` 会把它印成 `?`（那是采集源 id 专用的）。
 */
function toolCells(dash: UsageDashboard, t: TFn): [string, string][] {
  const work = dash.workSummary;
  return [
    [t("usage.work.toolCallsTotal"), work.toolCalls === null ? EM_DASH : fmtInt(work.toolCalls)],
    [
      t("usage.work.toolAvg"),
      work.toolAvgMs === null ? EM_DASH : fmtDur(work.toolAvgMs, getLang()),
    ],
    [
      t("usage.work.topToolCount"),
      work.topTool === null ? EM_DASH : `${work.topTool.name} · ${fmtInt(work.topTool.count)}`,
    ],
    [
      t("usage.work.topToolMs"),
      work.topToolMs === null
        ? EM_DASH
        : `${work.topToolMs.name} · ${fmtDur(work.topToolMs.ms, getLang())}`,
    ],
  ];
}

/**
 * 设置读取：**点击时一次性读**（不缓存、不挂载即读）。
 *
 * 为什么不用 `useUsageSettingsQuery`（Task 5 的「唯一设置查询入口」）：
 *  * 那个查询**挂载即取**（`useQuery` 没有 `enabled` 开关），而本 hook 的唯一消费方
 *    `UsageExportActions` 有一条硬绊线——「复制文本这条链一次 IPC 都不发」
 *    （`usage-export-csv.test.tsx` 用例 1 直接断言 `tauriInvokeMock` 全程未被调用）；
 *    挂载即读会当场把它弄红。
 *  * 该组件的既有测试**不套** `QueryClientProvider`（`useQuery` 无 provider 直接抛），要挂载即读
 *    就得改动 Task 11 的 4 条既有用例——那是**回归**，不是本任务该付的代价。
 * 这里的读是「即取即用、不留缓存」：读不到（IPC 失败）→ `null` → 按缺省出图（`random` + 无自定义
 * 评语），既不阻断导出，也不构成第二份设置缓存，故不存在与 `["usage"]` 失效家族脱节的问题。
 */
async function readSettings(): Promise<UsageSettings | null> {
  try {
    return await usageGetSettings();
  } catch {
    return null;
  }
}

/**
 * 看板数据 → 布局入参 + 位图（图集只取一次；几何与位图同源）。
 * `bitmap` 与 `input.sheet` 分开返回，就是为了让布局层拿不到位图。
 */
async function buildShareInput(
  dash: UsageDashboard,
  t: TFn,
  settings: UsageSettings | null
): Promise<{ input: ExportInput; bitmap: SheetBitmap | null }> {
  const chartPoints = trendPoints(dash.trend);
  const points = chartPoints.map((p) => ({ label: p.label, value: p.value }));
  const span = spanLabelOf(chartPoints);
  const rangeName = rangeLabelOf(dash.range, t);
  const dist = distributionRows(dash.rows);
  /**
   * 分享图**没画出来**的分组行数（X2 缺陷修复）：本层最多画 `GROUPS_MAX`(6) 行，而屏上分布卡是
   * `DIST_MAX_ROWS`(10) 行 + 「等 N」。两项都要算：
   *  * `dist.rows.length - min(dist.rows.length, GROUPS_MAX)` = 图比列表少画的那几行；
   *  * `dist.moreCount` = 列表自己被 10 行上限折掉的余量（`distributionRows` 已经算好但旧实现丢掉了）。
   * 两者相加才是「真实组数 − 图上画出来的行数」。为 0 ⇒ 不出这行（不印「等 0」）。
   */
  const groupsMoreCount =
    dist.rows.length - Math.min(dist.rows.length, GROUPS_MAX) + dist.moreCount;
  // **没量到 token** 的窗口（2026-10-06 用户裁决）：token 位一律 `—`（头部 hero、指标行、评语的
  // `{tokens}`），**计数类照常**；趋势也**不画**（全零点会被画成一条贴底零线 = 假图，见
  // `UsageTrendCard` 的同款纪律）——交给布局层的空态分支（`points.length < 2` → `emptyLabel`）。
  // 跨度/范围名仍用**真实点**算（「近 7 天 · 09/27 – 10/03」是真实信息，不因没量到 token 而消失）。
  const unmeasured = tokenUnmeasured(dash);

  // 图集：只取一次（几何与位图同源）；取不到 → sheet = null → **不画立绘也要出图**
  const pet = await activePetSheet().catch(() => null);
  const loaded = pet ? await loadSheet(pet.url, pet.rows) : null;
  const sheet = pet && loaded ? { rows: pet.rows, geometry: loaded.geometry } : null;

  const agg: QuoteAgg = {
    total: dash.hero,
    hitPct: dash.totals.cacheHitRate,
    trendUp: trendUpOf(points),
    // 多「线」= 当前分组维度下不止一组（维度是 model 时即多模型；其余维度下「多线作战」同样成立）。
    // 导出**不另发查询**（§3 第 17 条），故只能看当前看板已有的分组行。
    multiModel: dist.rows.length > 1,
    topTool: dash.workSummary.topTool?.name ?? null,
  };
  const vars: QuoteVars = {
    range: rangeName,
    // 评语气泡里的 `{tokens}` 同样是 token 量 ⇒ 没量到时出 `—`（不印 0）
    tokens: unmeasured ? EM_DASH : fmtInt(dash.hero),
    hitPct: unmeasured ? EM_DASH : fmtPct(dash.totals.cacheHitRate),
    models: String(dist.rows.length),
  };

  return {
    input: {
      brand: `兔维斯 · ${t("usage.title")}`,
      // 跨度算不出（点 < 2）时**不挂**那个分隔符，避免出现「近 7 天 · 」这种半截头
      rangeLabel: span ? `${rangeName} · ${span}` : rangeName,
      heroLabel: t("usage.hero.label", { range: rangeName }),
      hero: unmeasured ? EM_DASH : fmtInt(dash.hero),
      heroSub: `${t("usage.grid.asOf")}: ${asOfText(dash.collectedAt, t)}`,
      metrics: metricRows(dash, t),
      trendTitle: t("usage.card.trend"),
      emptyLabel: t("usage.empty"),
      // 没量到 token ⇒ 不把全零点交给画布（贴底零线 = 假图）；布局层按「点 < 2」走空态
      points: unmeasured ? [] : points,
      groupTitle: t("usage.card.distribution"),
      // 完整列表交给布局层，由它按 `GROUPS_MAX` 截断（**截断只此一处**；本层只负责把「少了多少」
      // 算准，见上面的 `groupsMoreCount`）
      groups: dist.rows.map((row) => ({
        // 行名分派与文本摘要同一实现（`exportText.ts::labelOf`），此处不另立副本
        name: labelOf(row, dash.groupBy, t),
        value: fmtInt(row.value),
        share: row.share,
      })),
      groupsMore: groupsMoreCount > 0 ? t("usage.moreN", { n: groupsMoreCount }) : null,
      toolsTitle: t("usage.work.toolCalls"),
      tools2x2: toolCells(dash, t),
      quote: pickQuote(agg, getLang(), settings?.exportQuote ?? "", vars),
      footer: t("usage.footer.caliber"),
      pose: settings?.exportPose ?? "random",
      poseColIndex: 0,
      sheet,
    },
    bitmap: loaded?.bitmap ?? null,
  };
}

/** 分享图两条出口（都先出图；差别只在最后一步去向） */
export interface ShareExport {
  /**
   * 出图并**落盘**（Task 10 的 `saveBytes` → 契约的二进制落盘命令）。文件名走
   * `pngFilename(range)` = `mam-usage-<窗口标识>-<导出时刻>.png` —— **带导出时刻是刻意的**
   * （2026-10-06 修复 X1）：落盘层是裸 `fs::write`，同名即静默覆盖，而 `mam-usage-<preset>.png`
   * 对固定档位跨天同名、对自定义档每次同名。
   */
  exportImage(dash: UsageDashboard): Promise<SaveOutcome>;
  /** 出图并**复制到剪贴板**（best-effort 副本，不落盘；成功也没有路径可定位） */
  copyImage(dash: UsageDashboard): Promise<SaveOutcome>;
}

/**
 * 装配 + 落盘。调用点恒为组件顶层（`UsageExportActions`），返回的两个动作都是**点击时**才干活：
 * 挂载期零 IPC、零 canvas、零 fetch（这正是 `useExportShare(t)` 只收 `t` 的原因——看板数据由调用方
 * 逐次传进来，设置在里面按需读）。
 */
export function useExportShare(t: TFn): ShareExport {
  const render = async (dash: UsageDashboard): Promise<Blob> => {
    const { input, bitmap } = await buildShareInput(dash, t, await readSettings());
    return renderShareImage(input, defaultExportDeps(bitmap));
  };

  return {
    async exportImage(dash: UsageDashboard): Promise<SaveOutcome> {
      const blob = await render(dash);
      return saveBytes(new Uint8Array(await blob.arrayBuffer()), pngFilename(dash.range));
    },
    async copyImage(dash: UsageDashboard): Promise<SaveOutcome> {
      const blob = await render(dash);
      const r = await copyImageClipboard(blob);
      // 接缝的 `CopyOutcome` → 本 hook 的 `SaveOutcome`（brief 钉死的返回形状）：副本没有落盘路径，
      // 成功侧的 `path` 恒为空串 —— 调用方据此**不**显示「打开所在目录」（没有文件可定位）。
      return r.ok ? { ok: true, path: "" } : { ok: false, reason: "failed", error: r.error };
    },
  };
}
