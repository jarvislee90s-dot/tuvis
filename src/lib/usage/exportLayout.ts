// 分享图布局纯层（计划② Task 12 步骤 4）——**只产步骤列表**：不碰 canvas、不取时间、不发 IPC。
// 消费方 = `exportImage.ts`（逐步执行）；两遍绘制（度量遍算高、绘制遍照步骤走）共用一个 `layoutExport`，
// 高度由本层**一次算定**并返回 `height` ⇒ 两遍步进结构上不可能分歧。
//
// 版式（brief 步骤 4 的表格 + 2026-10-27 实机反馈轮 #3a 的模块化改造，逐条不得发挥）：
//  * 段序：品牌 → 范围 → hero（小标题 + 大数字 + 说明）→ 指标行 → 趋势 mini →
//    **分组 Top × N（每个选中的 groupBy 一段，≤6 行；`distribution: []` 则整段缺席）** →
//    **API 等效费用（可选段）** → 工具 2×2 → 评语（立绘 + 气泡）→ 页脚；每段一个 `anchors` 锚点，
//    **y 严格递增**。后三段（费用 / 工具格 / 立绘气泡）由装配层给的模块开关决定画不画：
//    **没有段就不出标题、不出空盒**；
//  * `bar.w` = **轨道宽恒为 `CW`**（608），`share` 是填充比（夹取 `max(0.04, min(1, share))`）；
//    执行层画 `w * share`——把 `w` 定义成「填充宽」会让前景条被平方（缩成 4% 的 4%）；
//  * 立绘固定高 360、`petW = round(360 × frameW / frameH)`、`petX = EXPORT_W - PAD - petW`，
//    cut 框 `(col × frameW, row × frameH, frameW, frameH)`（列已按该动作帧数夹过）；
//  * 气泡三式与「让位」照抄 brief：气泡高过立绘时页脚跟着下移；
//  * `sheet === null` → **无 sprite 步骤**，其余版式与总高照常；趋势点 < 2 → 出空态占位而不是空图。
//  * `work === false`（2026-10-27 #3a）→ **工具格与立绘/气泡整段缺席**（一个开关管两段），
//    图在最后一个数据段后以正常底距收尾（页脚照常，不残留空的立绘区）。
//
// 色值纪律（§3 第 21 条）：本文件是**全仓唯一允许硬编码色值**的用量模块（分享图固定浅色版式），
// **不读任何 CSS 变量**（`usage-theme-tokens.test.ts` 用例 4 对本文件做反向断言）；气泡底色 `#fffdf9`
// 是 brief 钉死的版式常量，不是「忘了走 token」。
//
// 分层纪律：位图**不进本层**（`SheetGeometry` 只是几何），本层拿不到、也不该拿到 `ImageBitmap`。
import { CHART_PAD } from "@/components/usage/TrendChart";
import { poseCol, resolvePoseRow, type PetRows, type SheetGeometry } from "@/lib/usage/sheet";
import type { UsageGroupBy } from "@/types/usage";

// —— 钉死的版式常量 ——
/** 竖版卡宽（固定，不随窗口） */
export const EXPORT_W = 720;
/** 立绘固定高（**不随内容压缩**） */
export const EXPORT_PET_H = 360;
/** 总高下限 */
export const EXPORT_MIN_H = 1560;
/** 占比条填充下限（0–1 分数；与 Task 2 的 `DIST_MIN_BAR_PCT / 100` 同值） */
export const EXPORT_MIN_BAR_PCT = 0.04;
/** 卡外页边距 */
export const M = 24;
/** 卡内左右留白 */
export const PAD = 32;
/** 卡内容宽：`EXPORT_W - M × 2 - PAD × 2` = 608 */
export const CW = EXPORT_W - M * 2 - PAD * 2;

// —— 固定浅色（不读 CSS 变量；见文件头）——
export const PAGE = "#f5f6f8";
export const CARD = "#ffffff";
export const BUBBLE_BG = "#fffdf9";
export const INK = "#1f2937";
export const SUB = "#6b7280";
export const FAINT = "#9ca3af";
export const ACCENT_A = "#3b82f6";
export const ACCENT_B = "#8b5cf6";

/**
 * 分组维度强调色（二轮实机反馈 ②，2026-10-27 用户指示）：分组占比条**随维度取色**
 * （同族同样式实心圆角条，维度之间以色相区分，多段同图不再一片蓝）。
 * 固定浅色调色板纪律不变（不读 CSS 变量，`usage-theme-tokens.test.ts` 反向断言）。
 */
export const GROUP_ACCENTS: Record<UsageGroupBy, string> = {
  tool: "#3b82f6",
  project: "#10b981",
  provider: "#f59e0b",
  model: "#8b5cf6",
};
/** 费用段占比条（描边样式，二轮实机反馈 ②）：淡紫填充 + 紫描边，与分组段实心条一眼区分 */
export const COST_BAR_FILL = "rgba(139,92,246,0.18)";
export const COST_BAR_STROKE = "#8b5cf6";

/** 分隔线 / 占比条轨道（同一个浅灰，不另立第二档） */
const LINE = "#eceef2";
/** 趋势与工具格子的浅底 */
const SOFT_BG = "#f7f8fa";

// —— 字体（唯一出处；执行层只照抄步骤里的字符串）——
const FONT_BRAND = "600 22px system-ui";
const FONT_RANGE = "400 16px system-ui";
const FONT_HERO_LABEL = "400 16px system-ui";
const FONT_HERO = "700 56px system-ui";
const FONT_HERO_SUB = "400 14px system-ui";
const FONT_SECTION = "600 16px system-ui";
const FONT_ROW = "400 15px system-ui";
const FONT_ROW_VALUE = "600 15px system-ui";
const FONT_QUOTE = "500 19px system-ui";
const FONT_TICK = "400 11px system-ui";
const FONT_FOOTER = "400 15px system-ui";

// —— 版式节奏（本层私有；改了不会牵动别处）——
const CARD_R = 24;
const SECTION_GAP = 22;
const ROW_PITCH = 30;
const GROUP_ROW_H = 44;
const BAR_H = 8;
const CELL_W = (CW - 16) / 2;
const CELL_H = 64;
const CELL_GAP = 16;
const TREND_H = 120;
const TREND_BOX_PAD = 12;
const QUOTE_PAD = 20;
const QUOTE_ASC = 16;
const QUOTE_LINE_H = 27;
const BUBBLE_R = 16;
/** 气泡与立绘之间的横向间隙（气泡右缘 ≤ `petX - 18`） */
const BUBBLE_TAIL_GAP = 18;
const FOOTER_BLOCK_H = 34;
/** 分组 Top 最多几行（brief 钉死 ≤6） */
/** 分组区最多画几行（**导出给装配层用**：`useExportShare` 要按它算「等 N」的 N，见 `groupsMore`） */
export const GROUPS_MAX = 6;
/** 趋势 x 轴标签最多几个（竖版静态图：30 个日标签会糊成一团；首末必显） */
const TREND_LABEL_CAP = 8;

/** 文本测量：`(文本, 字体) → 宽`。度量遍只调它，绘制遍不调。 */
export type MeasureText = (text: string, font: string) => number;

/** 趋势点（`trendPoints` 的产物再瘦身：本层只认标签与值） */
export interface ExportPoint {
  label: string;
  value: number;
}

/** 分组 Top 一行：`value` 是**已格式化**的精确值，`share` 是 0–1 分数 */
export interface ExportGroupRow {
  name: string;
  value: string;
  share: number;
}

/**
 * 一段分组分布（2026-10-27 #3a）：每个选中的 `UsageGroupBy` 一段。
 * `groupsMore` 的语义与旧单段字段逐字相同（见 `ExportInput` 历史注释：本层最多画 `GROUPS_MAX`(6)
 * 行，屏上分布卡是 `DIST_MAX_ROWS`(10) 行 + 「等 N」，少的行数**必须**说出来）——按本接口纪律，
 * 这里收的是**成品字符串**（本层不做 i18n、不拼「等 N」的措辞）。
 */
export interface ExportGroupSection {
  groupBy: UsageGroupBy;
  title: string;
  rows: readonly ExportGroupRow[];
  groupsMore: string | null;
}

/** API 等效费用一行：`label` / `value` 是成品字符串；`share` 缺省 = 该行不画占比条 */
export interface ExportCostRow {
  label: string;
  value: string;
  share?: number;
}

/**
 * API 等效费用段（2026-10-27 #3a，新增段）：总额 + ≤6 行（行语法与分组段同一套：
 * 名称左、金额右、占比条在下）。数据与费用卡同源（`usage_costs`），币种跟随显示币种；
 * 「无全局总额」是**费用卡**的契约，导出段按用户裁决要出总额（`totalLabel` 写明「估价」口径）。
 *
 * `more`（评审 Important，X2 同类缺陷）：本层只画 `GROUPS_MAX`(6) 行，而 `total` 是**全部行**
 * 的合计——可见行之和与总额对不上时**必须**说出来（「等 N」已由装配层译好；`null` = 一行没少、
 * 不出这行）。与分组段的 `groupsMore` 同构同款绘制。
 */
export interface ExportCostSection {
  title: string;
  totalLabel: string;
  total: string;
  rows: readonly ExportCostRow[];
  more: string | null;
}

/**
 * 绘制步骤：执行层逐步 `execute`，顺序即绘制顺序（先页背景、再白卡、再内容）。
 * `trend` 的 `pts` 是**绝对画布坐标**（投影已在纯层算完），执行层不做任何投影。
 */
export type ExportStep =
  | {
      kind: "rect";
      x: number;
      y: number;
      w: number;
      h: number;
      color: string;
      r?: number;
      stroke?: string;
    }
  | { kind: "line"; x1: number; y1: number; x2: number; y2: number; color: string; w: number }
  | {
      kind: "text";
      text: string;
      x: number;
      y: number;
      font: string;
      color: string;
      align?: "left" | "center" | "right";
    }
  | { kind: "trend"; x: number; y: number; w: number; h: number; pts: { x: number; y: number }[] }
  | {
      kind: "bar";
      x: number;
      y: number;
      w: number;
      h: number;
      /** 已夹取的填充比（0.04–1）；填充宽 = `w × share`，**执行层**才算 */
      share: number;
      track: string;
      fill: string;
      /** 可选描边色（二轮实机反馈 ②：费用段描边样式——执行层沿填充条同一圆角路径描一圈） */
      stroke?: string;
      r?: number;
    }
  | {
      kind: "sprite";
      x: number;
      y: number;
      w: number;
      h: number;
      sx: number;
      sy: number;
      sw: number;
      sh: number;
    };

/** 段锚点 id（`groups` 一段一个锚点、可重复出现；缺席的段**没有**锚点；y 严格递增） */
export type ExportSectionId =
  | "brand"
  | "range"
  | "hero"
  | "metrics"
  | "trend"
  | "groups"
  | "cost"
  | "tools"
  | "quote"
  | "footer";

export interface ExportAnchor {
  id: ExportSectionId;
  /** 分组段的维度（`id === "groups"` 时用于区分多段；其余段缺省） */
  key?: UsageGroupBy;
  y: number;
}

/** 布局入参：**全部是已格式化的字符串/数字**（格式化口径归装配层，本层不做 i18n、不做千分位） */
export interface ExportInput {
  /** `兔维斯 · ${t("usage.title")}` */
  brand: string;
  /** `${rangeLabelOf(range)} · ${spanLabelOf(trend)}`，如「近 7 天 · 09/27 – 10/03」 */
  rangeLabel: string;
  /** `t("usage.hero.label", { range })`（**不是** `card.trend`——用了它全图会连着出现两次「用量趋势」） */
  heroLabel: string;
  /** token 量（二轮实机反馈 ②：`fmtTokens` 万/亿，与看板同口径；非 Token 计数不走这） */
  hero: string;
  /** 采集时刻；`collectedAt === 0` 时调用方给 `usage.notCollected` */
  heroSub: string;
  /** 指标行（6 行），`[标签, 值]` */
  metrics: [string, string][];
  /** `t("usage.card.trend")` */
  trendTitle: string;
  /** 趋势点 < 2 时画的占位文案（`t("usage.empty")`） */
  emptyLabel: string;
  points: readonly ExportPoint[];
  /**
   * 分组分布段（2026-10-27 #3a：**数组**，每个选中的 `UsageGroupBy` 一段；空数组 = 整段缺席）。
   * 旧单段字段（`groupTitle` / `groups` / `groupsMore`）已并入 `ExportGroupSection`，
   * 字段语义逐条不变（含「屏上分布卡 N 行 / 本图最多 6 行」的 `groupsMore` 注记）。
   */
  groups: readonly ExportGroupSection[];
  /** API 等效费用段；`null` = 未选 / 取数失败 / 无行 → **整段缺席**（无标题、无空盒） */
  cost: ExportCostSection | null;
  /**
   * 工作小结开关（2026-10-27 #3a）：`true` = 画工具格 + 立绘/气泡（与旧版式一致）；
   * `false` = 两段**都**缺席，图在最后一个数据段后以正常底距收尾。
   */
  work: boolean;
  /** `t("usage.work.toolCalls")` */
  toolsTitle: string;
  /** 工具 2×2 的 4 行，`[标签, 值]`（`null` 已由装配层换成 `EM_DASH`） */
  tools2x2: [string, string][];
  quote: string;
  /** `t("usage.footer.caliber")` */
  footer: string;
  /** 姿态键（`settings.exportPose`；未知/`random` 回落待机行，见 `resolvePoseRow`） */
  pose: string;
  /** 代表帧列（恒 0；见 `poseCol`） */
  poseColIndex: number;
  /** 图集几何 + 行数；`null` = 加载失败/几何非法 → 无立绘也要出图 */
  sheet: { rows: PetRows; geometry: SheetGeometry } | null;
}

export interface ExportLayout {
  /** 总高（绘制遍用它建画布）：`max(EXPORT_MIN_H, ceil(contentBottom + 34))` */
  height: number;
  steps: ExportStep[];
  anchors: ExportAnchor[];
  /** 内容底（页脚文字块下沿）：总高公式的入参，导出给测试逐字核对这条钉死的公式 */
  contentBottom: number;
}

/** 占比条填充比：夹取到 `[EXPORT_MIN_BAR_PCT, 1]`（非有限值归下限，绝不画 0 宽或超宽） */
function clampShare(share: number): number {
  if (!Number.isFinite(share)) return EXPORT_MIN_BAR_PCT;
  return Math.max(EXPORT_MIN_BAR_PCT, Math.min(1, share));
}

/**
 * 峰值归一投影（**与 `TrendChart` 的 `project` 同一式**：x 按序号等距铺满绘图区、y 峰值贴顶 / 0 贴底，
 * 全 0 数据不除零）。那一支没有导出，故按同一公式在这里重写；`CHART_PAD` 直接复用，避免两处版式漂移。
 * 出 x 轴标签 → 底部留白取 `bLabel`。
 */
function projectTrend(
  points: readonly ExportPoint[],
  x: number,
  y: number,
  w: number,
  h: number
): { x: number; y: number }[] {
  const n = points.length;
  const innerW = Math.max(0, w - CHART_PAD.l - CHART_PAD.r);
  const innerH = Math.max(0, h - CHART_PAD.t - CHART_PAD.bLabel);
  const max = points.reduce((m, p) => Math.max(m, p.value), 0);
  return points.map((p, i) => ({
    x: x + (n <= 1 ? CHART_PAD.l + innerW / 2 : CHART_PAD.l + (innerW * i) / (n - 1)),
    y: y + CHART_PAD.t + innerH * (1 - (max > 0 ? p.value / max : 0)),
  }));
}

/** x 轴标签抽稀：`≤ cap` 全出；否则步长 `ceil(n / cap)` 且**末点必显**（与 `TrendChart` 同规则） */
function trendLabelIndexes(n: number): number[] {
  if (n <= TREND_LABEL_CAP) return Array.from({ length: n }, (_, i) => i);
  const step = Math.ceil(n / TREND_LABEL_CAP);
  const out: number[] = [];
  for (let i = 0; i < n - 1; i += step) out.push(i);
  out.push(n - 1);
  return out;
}

/**
 * 按测量宽度贪心换行：先按空白切词，词本身超宽时**按字符硬断**（中文没有空格，不断就会溢出气泡）。
 * 返回至少一行（空串 → `[""]`），保证气泡高度公式恒有解。
 */
function wrapText(text: string, maxW: number, font: string, measure: MeasureText): string[] {
  const words = text.split(/\s+/).filter((w) => w.length > 0);
  const lines: string[] = [];
  let cur = "";
  const flush = () => {
    if (cur) lines.push(cur);
    cur = "";
  };
  for (const word of words) {
    const candidate = cur ? `${cur} ${word}` : word;
    if (measure(candidate, font) <= maxW) {
      cur = candidate;
      continue;
    }
    flush();
    if (measure(word, font) <= maxW) {
      cur = word;
      continue;
    }
    let chunk = "";
    for (const ch of word) {
      if (chunk && measure(chunk + ch, font) > maxW) {
        lines.push(chunk);
        chunk = "";
      }
      chunk += ch;
    }
    cur = chunk;
  }
  flush();
  return lines.length > 0 ? lines : [""];
}

/**
 * 看板数据（已格式化）→ 竖版分享图的绘制步骤 + 总高。纯函数：同样的输入与测量函数永远给同样的步骤序。
 * 调用方须保证 `input.metrics` 是 6 行、`input.tools2x2` 是 4 行、`cost.rows` ≤ 6 行
 * （截断口径属装配层）。
 */
export function layoutExport(input: ExportInput, measure: MeasureText): ExportLayout {
  const content: ExportStep[] = [];
  const anchors: ExportAnchor[] = [];
  const contentX = M + PAD;
  const contentRight = M + (EXPORT_W - M * 2) - PAD;
  const text = (
    t: string,
    x: number,
    yy: number,
    font: string,
    color: string,
    align?: "left" | "center" | "right"
  ): ExportStep => ({ kind: "text", text: t, x, y: yy, font, color, align });
  const rule = (yy: number): ExportStep => ({
    kind: "line",
    x1: contentX,
    y1: yy,
    x2: contentRight,
    y2: yy,
    color: LINE,
    w: 1,
  });
  const sectionTitle = (title: string, yy: number): void => {
    content.push(text(title, contentX, yy + 16, FONT_SECTION, INK));
  };

  let y = M + PAD; // 56

  // ① 品牌
  anchors.push({ id: "brand", y });
  content.push(text(input.brand, contentX, y + 22, FONT_BRAND, INK));
  y += 22 + 10;

  // ② 范围（含跨度）
  anchors.push({ id: "range", y });
  content.push(text(input.rangeLabel, contentX, y + 16, FONT_RANGE, SUB));
  y += 16 + 12;
  content.push(rule(y));
  y += SECTION_GAP;

  // ③ hero：小标题 + 大数字 + 说明（说明位固定归 `heroSub`；`collectedAt === 0` 由装配层换成 notCollected）
  anchors.push({ id: "hero", y });
  content.push(text(input.heroLabel, contentX, y + 16, FONT_HERO_LABEL, SUB));
  y += 16 + 16;
  content.push(text(input.hero, contentX, y + 50, FONT_HERO, INK));
  y += 50 + 12;
  content.push(text(input.heroSub, contentX, y + 14, FONT_HERO_SUB, SUB));
  y += 14 + 22;
  content.push(rule(y));
  y += SECTION_GAP;

  // ④ 指标行（6 行；标签左、值右，值已是精确千分位/百分号文本）
  anchors.push({ id: "metrics", y });
  for (const [label, value] of input.metrics) {
    content.push(text(label, contentX, y + 15, FONT_ROW, SUB));
    content.push(text(value, contentRight, y + 15, FONT_ROW_VALUE, INK, "right"));
    y += ROW_PITCH;
  }
  y += 10;

  // ⑤ 趋势 mini：点 < 2 → 空态占位（不画空图）
  anchors.push({ id: "trend", y });
  sectionTitle(input.trendTitle, y);
  y += 16 + 20;
  const boxX = contentX - TREND_BOX_PAD;
  const boxY = y - TREND_BOX_PAD;
  const boxW = CW + TREND_BOX_PAD * 2;
  const boxH = TREND_H + TREND_BOX_PAD * 2;
  content.push({ kind: "rect", x: boxX, y: boxY, w: boxW, h: boxH, color: SOFT_BG, r: 14 });
  if (input.points.length < 2) {
    content.push(
      text(input.emptyLabel, contentX + CW / 2, y + TREND_H / 2, FONT_ROW, SUB, "center")
    );
  } else {
    const pts = projectTrend(input.points, contentX, y, CW, TREND_H);
    content.push({ kind: "trend", x: contentX, y, w: CW, h: TREND_H, pts });
    for (const i of trendLabelIndexes(input.points.length)) {
      content.push(
        text(input.points[i].label, pts[i].x, y + TREND_H + 2, FONT_TICK, SUB, "center")
      );
    }
  }
  y = boxY + boxH + SECTION_GAP;

  // ⑥ 分组 Top × N（2026-10-27 #3a：每个选中的 groupBy 一段，段序 = 装配层给的数组序；
  //    ≤6 行；轨道恒 CW，填充比交给执行层乘）
  // 「等 N」：图上看不到的行数**必须**说出来（旧实现静默丢行，X2）。分组段与费用段同一款：
  // 字号比数据行小一档、左对齐 ⇒ 读起来是「这一段的补充说明」，而不是第 7 行。
  const paintMoreNote = (more: string, yy: number): number => {
    content.push(text(more, contentX, yy + 12, FONT_TICK, SUB));
    return yy + 22;
  };
  const paintGroupSection = (section: ExportGroupSection, yStart: number): number => {
    let yy = yStart;
    sectionTitle(section.title, yy);
    yy += 16 + 20;
    // 占比条随维度取色（二轮实机反馈 ②）：同段同色实心圆角条，多段之间以色相区分
    const accent = GROUP_ACCENTS[section.groupBy];
    for (const group of section.rows.slice(0, GROUPS_MAX)) {
      content.push(text(group.name, contentX, yy + 15, FONT_ROW, INK));
      content.push(text(group.value, contentRight, yy + 15, FONT_ROW_VALUE, SUB, "right"));
      content.push({
        kind: "bar",
        x: contentX,
        y: yy + 22,
        w: CW,
        h: BAR_H,
        share: clampShare(group.share),
        track: LINE,
        fill: accent,
        r: 4,
      });
      yy += GROUP_ROW_H;
    }
    if (section.groupsMore !== null) yy = paintMoreNote(section.groupsMore, yy);
    return yy + 8;
  };
  for (const section of input.groups) {
    anchors.push({ id: "groups", key: section.groupBy, y });
    y = paintGroupSection(section, y);
  }

  // ⑦ API 等效费用（可选段）：标题 + 总额行 + ≤6 行（名称左 / 金额右 / 占比条在下，
  //    与分组段同一套行语法；占比条用**描边样式**（淡紫填充 + 紫描边，二轮实机反馈 ②）
  //    与分组段的实心条区分）。
  //    装配层保证 `rows` ≤ 6（截断口径归装配层，同 `metrics` 6 行 / `tools2x2` 4 行）；
  //    `more` 非空 = 有行没画出来（`total` 是全部行的合计，缺披露会让人以为对不上账是算错）。
  if (input.cost) {
    anchors.push({ id: "cost", y });
    sectionTitle(input.cost.title, y);
    y += 16 + 20;
    content.push(text(input.cost.totalLabel, contentX, y + 15, FONT_ROW, SUB));
    content.push(text(input.cost.total, contentRight, y + 15, FONT_ROW_VALUE, INK, "right"));
    y += ROW_PITCH;
    for (const row of input.cost.rows) {
      content.push(text(row.label, contentX, y + 15, FONT_ROW, INK));
      content.push(text(row.value, contentRight, y + 15, FONT_ROW_VALUE, SUB, "right"));
      if (typeof row.share === "number") {
        content.push({
          kind: "bar",
          x: contentX,
          y: y + 22,
          w: CW,
          h: BAR_H,
          share: clampShare(row.share),
          track: LINE,
          fill: COST_BAR_FILL,
          stroke: COST_BAR_STROKE,
          r: 4,
        });
        y += GROUP_ROW_H;
      } else {
        y += ROW_PITCH;
      }
    }
    if (input.cost.more !== null) y = paintMoreNote(input.cost.more, y);
    y += 8;
  }

  // ⑧ 工作小结 + ⑨ 评语（`work` 一个开关管两段；false 时两段整体缺席，不残留空的立绘区）
  if (input.work) {
    // 工具 2×2（4 格；格宽 = (CW - 16) / 2）
    anchors.push({ id: "tools", y });
    sectionTitle(input.toolsTitle, y);
    y += 16 + 20;
    input.tools2x2.forEach(([label, value], i) => {
      const cx = contentX + (i % 2) * (CELL_W + CELL_GAP);
      const cy = y + Math.floor(i / 2) * (CELL_H + CELL_GAP);
      content.push({ kind: "rect", x: cx, y: cy, w: CELL_W, h: CELL_H, color: SOFT_BG, r: 12 });
      content.push(text(label, cx + 14, cy + 26, FONT_ROW, SUB));
      content.push(text(value, cx + 14, cy + 52, FONT_ROW_VALUE, INK));
    });
    const cellRows = Math.ceil(input.tools2x2.length / 2);
    y += cellRows * CELL_H + Math.max(0, cellRows - 1) * CELL_GAP + SECTION_GAP;

    // 评语：立绘（固定高）与气泡左右分离；气泡高过立绘时页脚让位
    anchors.push({ id: "quote", y });
    const petY = y;
    const sheet = input.sheet;
    const petW = sheet
      ? Math.round((EXPORT_PET_H * sheet.geometry.frameW) / sheet.geometry.frameH)
      : 0;
    const petX = EXPORT_W - PAD - petW;
    const bubbleY = petY + EXPORT_PET_H * 0.1;
    const quoteTextW = measure(input.quote, FONT_QUOTE);
    const bubbleW = Math.min(
      340,
      Math.max(150, quoteTextW + 44),
      Math.max(150, petX - BUBBLE_TAIL_GAP - PAD)
    );
    const bubbleX = Math.max(PAD, petX - BUBBLE_TAIL_GAP - bubbleW);
    const quoteLines = wrapText(input.quote, bubbleW - QUOTE_PAD * 2, FONT_QUOTE, measure);
    const bubbleH = QUOTE_PAD * 2 + QUOTE_ASC + (quoteLines.length - 1) * QUOTE_LINE_H;
    content.push({
      kind: "rect",
      x: bubbleX,
      y: bubbleY,
      w: bubbleW,
      h: bubbleH,
      color: BUBBLE_BG,
      r: BUBBLE_R,
      stroke: LINE,
    });
    quoteLines.forEach((line, i) => {
      content.push(
        text(
          line,
          bubbleX + QUOTE_PAD,
          bubbleY + QUOTE_PAD + QUOTE_ASC + i * QUOTE_LINE_H,
          FONT_QUOTE,
          INK
        )
      );
    });
    if (sheet) {
      const row = resolvePoseRow(input.pose, sheet.rows);
      const col = poseCol(input.pose, input.poseColIndex);
      content.push({
        kind: "sprite",
        x: petX,
        y: petY,
        w: petW,
        h: EXPORT_PET_H,
        sx: col * sheet.geometry.frameW,
        sy: row * sheet.geometry.frameH,
        sw: sheet.geometry.frameW,
        sh: sheet.geometry.frameH,
      });
    }
    const clusterBottom = Math.max(petY + (sheet ? EXPORT_PET_H : 0), bubbleY + bubbleH);
    // `clusterBottom` **恒** ≥ `petY`（`EXPORT_PET_H` 是正数常量；无立绘时那一项就是 `petY` 本身），
    // 而 `petY === y` 且中间没有再动过 `y` ⇒ 这里直接取 `clusterBottom`。原先写成
    // `Math.max(clusterBottom, y)` 是恒取前者的空运算，只会让人误以为 `y` 可能更大。
    y = clusterBottom + 24;
  }

  // ⑩ 页脚
  anchors.push({ id: "footer", y });
  content.push(text(input.footer, contentX, y + 15, FONT_FOOTER, FAINT));
  const contentBottom = y + FOOTER_BLOCK_H;
  const height = Math.max(EXPORT_MIN_H, Math.ceil(contentBottom + 34));

  return {
    height,
    steps: [
      { kind: "rect", x: 0, y: 0, w: EXPORT_W, h: height, color: PAGE },
      { kind: "rect", x: M, y: M, w: EXPORT_W - M * 2, h: height - M * 2, color: CARD, r: CARD_R },
      ...content,
    ],
    anchors,
    contentBottom,
  };
}
