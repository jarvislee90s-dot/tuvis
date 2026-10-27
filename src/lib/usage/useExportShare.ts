// 分享图装配与落盘（计划② Task 12 步骤 6）——把「当前看板数据」装配成布局入参、出图、走 Task 10 的
// 落盘接缝。**唯一消费方** = `UsageExportActions.tsx`（硬约束：只有它能 import 本 hook / 页面与其它
// 组件一律不得 import）。
//
// 五条口径（逐条不得发挥）：
//  * **图集只 fetch 一次**：`activePetSheet()` 只回答「当前宠物是谁 + 行数」，`loadSheet()` 一次
//    `fetch → blob → createImageBitmap` 同时产出**几何与位图**（同源，不可能一处新一处旧）；
//    位图**不进布局层**（`ExportInput.sheet` 只吃 `{ rows, geometry }`）——这是刻意分层。
//  * 6 行指标取 `usage.grid.*`（`requestInput → cacheRead → cacheMiss → output → hitRate → requests`，
//    与 A 总览同序），token 位用 `fmtTokens(…, getLang())` 与看板同口径的万/亿缩写
//    （二轮实机反馈 ②，2026-10-27 用户指示「导出图与看板同口径」；请求次数仍 `fmtInt`）；
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
//  * 模块选择（2026-10-27 实机反馈轮 #3a）：出图前读 `loadExportModules()`（localStorage，不走
//    Rust 设置）。每个选中的 groupBy 取一份看板数据：**当前看板的 groupBy 命中时直接复用 `dash`
//    （零 IPC）**，否则走既有只读命令（`usageDashboard`，与 `useUsageDashboardQuery` 同一 API 函数
//    ——**不新增命令**）；费用段与费用卡同源（`usageCosts`，币种/汇率走显示币种的 localStorage 读侧）。
//    取数失败 → 该段**缺席**（不阻断导出、不出空盒）；`work = false` → 不取图集、评语/姿态不进图。
import { trendPoints } from "@/components/usage/TrendChart";
import { usageDashboard, usageGetSettings } from "@/lib/api/usage";
import { usageCosts } from "@/lib/api/usageV2";
import { formatNativeMoney, readDisplayCurrency } from "@/lib/usage/displayCurrency";
import { distributionRows } from "@/lib/usage/distribution";
import { defaultExportDeps, renderShareImage } from "@/lib/usage/exportImage";
import {
  GROUPS_MAX,
  type ExportCostSection,
  type ExportGroupSection,
  type ExportInput,
} from "@/lib/usage/exportLayout";
import { loadExportModules, type ExportModules } from "@/lib/usage/exportModules";
import { labelOf, pngFilename } from "@/lib/usage/exportText";
import { EM_DASH, fmtDur, fmtInt, fmtPct, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { pickQuote, type QuoteVars } from "@/lib/usage/quotes";
import type { QuoteAgg } from "@/lib/usage/quotePool";
import { asOfText, rangeLabelOf, spanLabelOf, tokenUnmeasured, type TFn } from "@/lib/usage/range";
import { copyImage as copyImageClipboard, saveBytes, type SaveOutcome } from "@/lib/usage/saveFile";
import { activePetSheet, loadSheet, type SheetBitmap } from "@/lib/usage/sheet";
import type { CostRow, UsageCosts } from "@/types/usageV2";
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
  // token 位走 `fmtTokens`（二轮实机反馈 ②，2026-10-27 用户指示：导出图与看板同口径万/亿）
  const unmeasured = tokenUnmeasured(dash);
  const token = (v: number) => (unmeasured ? EM_DASH : fmtTokens(v, getLang()));
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
 * 十进制字符串求和（费用总额用）：只收非负十进制串（与显示币种层同一正则口径），
 * BigInt 缩放对齐小数位，无浮点误差。无可加项 → `null`（调用方显示 `—`，绝不假装 0）。
 */
function sumDecimalStrings(values: readonly (string | null)[]): string | null {
  const parts = values.filter((v): v is string => v !== null && /^\d+(?:\.\d+)?$/.test(v));
  if (parts.length === 0) return null;
  const scale = Math.max(...parts.map((p) => (p.split(".")[1] ?? "").length));
  let sum = 0n;
  for (const p of parts) {
    const [whole, frac = ""] = p.split(".");
    sum += BigInt(whole + frac.padEnd(scale, "0"));
  }
  const digits = sum.toString().padStart(scale + 1, "0");
  return scale > 0 ? `${digits.slice(0, -scale)}.${digits.slice(-scale)}` : digits;
}

/**
 * 费用段装配（与费用卡同一条数据通路）：行 = Token 全局降序 Top 6（与费用卡 Top5 同一排序式），
 * 行名 = 「显示渠道 / 模型」（平铺视图语法）；金额按显示币种取后端算好的原生值（不在此乘汇率）；
 * `share` 按 Token 占比（费用卡的排序键，不是金额占比——未估价行也有 Token 可比）。
 * 总额 = **全部行**估价求和（用户裁决「总额 + Top 行」；卡片「无全局总额」契约只约束卡片本身，
 * 导出段的 `totalLabel` 写明「估价」口径）。全未估价 → 总额 `—`。
 * `more`（评审 Important，X2 同类缺陷）：只画 Top 6 而总额是全部行的合计，可见行之和与总额
 * 对不上**必须**说出来——超出 `GROUPS_MAX` 的行数译成「等 N」（与分组段 `groupsMore` 同一 i18n
 * 键），为 0 ⇒ `null` 不出这行。**导出给测试**做「>6 ⇒ 非 null / ≤6 ⇒ null」回归
 * （生产消费方纪律不变：`useExportShare` 这个 hook 仍只允许 `UsageExportActions` import）。
 */
export function buildCostSection(costs: UsageCosts, t: TFn): ExportCostSection {
  const { currency } = readDisplayCurrency();
  const sorted = [...costs.rows].sort(
    (a, b) => b.tokenUsage - a.tokenUsage || a.key.localeCompare(b.key)
  );
  const maxTokens = sorted[0]?.tokenUsage ?? 0;
  const nativeAmountOf = (row: CostRow): string | null =>
    formatNativeMoney(currency === "cny" ? row.estimatedCny : row.estimatedUsd, currency);
  const channelOf = (row: CostRow): string => {
    // unassigned / ambiguous 的 label 是 i18n 键（与费用卡 `i18nLabel` 同规则）；已知渠道取显示名
    if (row.channelKind === "unassigned") return t("usage.label.unassignedChannel");
    if (row.channelKind === "ambiguous") return t("usage.label.ambiguousChannel");
    return row.displayProvider ?? "";
  };
  const rows = sorted.slice(0, GROUPS_MAX).map((row) => {
    const model =
      row.displayModelName ??
      (row.identity.modelId === "" ? t("usage.label.unknownModel") : row.identity.modelId);
    const channel = channelOf(row);
    const amount = nativeAmountOf(row);
    return {
      label: channel ? `${channel} / ${model}` : model,
      value: amount ?? t("usage.cost.unpriced"),
      share: maxTokens > 0 ? row.tokenUsage / maxTokens : undefined,
    };
  });
  // 总额对**原始**十进制串求和（`nativeAmountOf` 的产物带 `$`/`¥` 符号与千分位，进不了求和正则）
  const total =
    formatNativeMoney(
      sumDecimalStrings(
        costs.rows.map((row) => (currency === "cny" ? row.estimatedCny : row.estimatedUsd))
      ),
      currency
    ) ?? EM_DASH;
  // 只画 6 行 ≠ 只有 6 行：超出部分必须披露（`total` 覆盖全部行，见上）
  const more =
    sorted.length > GROUPS_MAX ? t("usage.moreN", { n: sorted.length - GROUPS_MAX }) : null;
  return {
    title: t("usage.cost.title"),
    totalLabel: t("usage.export.costTotal"),
    total,
    rows,
    more,
  };
}

/**
 * 看板数据 → 布局入参 + 位图（图集只取一次；几何与位图同源）。
 * `bitmap` 与 `input.sheet` 分开返回，就是为了让布局层拿不到位图。
 * `modules` 由调用方在导出时刻读好（2026-10-27 #3a）：决定分组段数量、费用段与工作小结段。
 */
async function buildShareInput(
  dash: UsageDashboard,
  t: TFn,
  settings: UsageSettings | null,
  modules: ExportModules
): Promise<{ input: ExportInput; bitmap: SheetBitmap | null }> {
  const chartPoints = trendPoints(dash.trend);
  const points = chartPoints.map((p) => ({ label: p.label, value: p.value }));
  const span = spanLabelOf(chartPoints);
  const rangeName = rangeLabelOf(dash.range, t);
  const dist = distributionRows(dash.rows);
  /**
   * 分享图**没画出来**的分组行数（X2 缺陷修复）：本层最多画 `GROUPS_MAX`(6) 行，而屏上分布卡是
   * `DIST_MAX_ROWS`(10) 行 + 「等 N」。两项都要算：
   *  * `rowsLength - min(rowsLength, GROUPS_MAX)` = 图比列表少画的那几行；
   *  * `moreCount` = 列表自己被 10 行上限折掉的余量（`distributionRows` 已经算好但旧实现丢掉了）。
   * 两者相加才是「真实组数 − 图上画出来的行数」。为 0 ⇒ 不出这行（不印「等 0」）。
   * （#3a 起每个分组段各算各的，公式仍是这一条。）
   */
  const groupsMoreOf = (rowsLength: number, moreCount: number): string | null => {
    const n = rowsLength - Math.min(rowsLength, GROUPS_MAX) + moreCount;
    return n > 0 ? t("usage.moreN", { n }) : null;
  };
  /**
   * 分组段 × N：每个选中的 groupBy 一段。**当前看板的 groupBy 命中时直接复用 `dash`**（零 IPC，
   * 与屏幕上那份数据严格同源）；其余维度走既有只读命令（`usageDashboard`，与看板查询同一 API
   * 函数，**不新增命令**）；失败 → 该段缺席（不阻断导出）。
   * 段标题：单段保持旧版式逐字（`分组分布`，默认图与改造前逐字节一致）；多段才加维度后缀消歧义。
   */
  const groupSections: ExportGroupSection[] = [];
  for (const groupBy of modules.distribution) {
    const d: UsageDashboard | null =
      groupBy === dash.groupBy ? dash : await usageDashboard(dash.range, groupBy).catch(() => null);
    if (!d) continue;
    const dd = distributionRows(d.rows);
    groupSections.push({
      groupBy,
      title:
        modules.distribution.length > 1
          ? `${t("usage.card.distribution")} · ${t(`usage.group.${groupBy}`)}`
          : t("usage.card.distribution"),
      rows: dd.rows.map((row) => ({
        // 行名分派与文本摘要同一实现（`exportText.ts::labelOf`），此处不另立副本
        name: labelOf(row, groupBy, t),
        // token 值与看板同口径（二轮实机反馈 ②：`fmtTokens` 万/亿，非完整千分位）
        value: fmtTokens(row.value, getLang()),
        share: row.share,
      })),
      groupsMore: groupsMoreOf(dd.rows.length, dd.moreCount),
    });
  }

  // 费用段：与费用卡同一查询（`usageCosts`，只读账本不刷价）；失败 / 无行 → 整段缺席
  const costs = modules.cost
    ? await usageCosts(dash.range, readDisplayCurrency().rate).catch(() => null)
    : null;
  const cost = costs && costs.rows.length > 0 ? buildCostSection(costs, t) : null;

  // **没量到 token** 的窗口（2026-10-06 用户裁决）：token 位一律 `—`（头部 hero、指标行、评语的
  // `{tokens}`），**计数类照常**；趋势也**不画**（全零点会被画成一条贴底零线 = 假图，见
  // `UsageTrendCard` 的同款纪律）——交给布局层的空态分支（`points.length < 2` → `emptyLabel`）。
  // 跨度/范围名仍用**真实点**算（「近 7 天 · 09/27 – 10/03」是真实信息，不因没量到 token 而消失）。
  const unmeasured = tokenUnmeasured(dash);

  // 图集：`work = false` 时**连 fetch 都不发**（立绘/气泡整段缺席，图集自然用不上）；
  // 只取一次（几何与位图同源）；取不到 → sheet = null → **不画立绘也要出图**
  const pet = modules.work ? await activePetSheet().catch(() => null) : null;
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
    // 评语气泡里的 `{tokens}` 同样是 token 量 ⇒ 没量到时出 `—`（不印 0）；
    // 与 hero 同函数同 lang（Batch 5 补充，用户裁决「整张导出图 token 单位与看板一致」）：
    // `fmtTokens` 万/亿，不再保留完整千分位
    tokens: unmeasured ? EM_DASH : fmtTokens(dash.hero, getLang()),
    hitPct: unmeasured ? EM_DASH : fmtPct(dash.totals.cacheHitRate),
    models: String(dist.rows.length),
  };

  return {
    input: {
      brand: `兔维斯 · ${t("usage.title")}`,
      // 跨度算不出（点 < 2）时**不挂**那个分隔符，避免出现「近 7 天 · 」这种半截头
      rangeLabel: span ? `${rangeName} · ${span}` : rangeName,
      heroLabel: t("usage.hero.label", { range: rangeName }),
      // hero 与看板同口径（二轮实机反馈 ②：`fmtTokens` 万/亿，非完整千分位）
      hero: unmeasured ? EM_DASH : fmtTokens(dash.hero, getLang()),
      heroSub: `${t("usage.grid.asOf")}: ${asOfText(dash.collectedAt, t)}`,
      metrics: metricRows(dash, t),
      trendTitle: t("usage.card.trend"),
      emptyLabel: t("usage.empty"),
      // 没量到 token ⇒ 不把全零点交给画布（贴底零线 = 假图）；布局层按「点 < 2」走空态
      points: unmeasured ? [] : points,
      groups: groupSections,
      cost,
      work: modules.work,
      toolsTitle: t("usage.work.toolCalls"),
      tools2x2: toolCells(dash, t),
      // `work = false` ⇒ 立绘/气泡整段缺席：不取图集（`sheet: null`）、评语/姿态不进图
      quote: modules.work ? pickQuote(agg, getLang(), settings?.exportQuote ?? "", vars) : "",
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
    // 模块选择**导出时刻现读**（localStorage，同设置的「点击时一次性读」纪律，不留第二份缓存）
    const modules = loadExportModules();
    const { input, bitmap } = await buildShareInput(dash, t, await readSettings(), modules);
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
