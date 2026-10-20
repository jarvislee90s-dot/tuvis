// 复制文本摘要纯层（计划② Task 11 步骤 3；spec P4「纯文本 = 头部一行 + 各指标行『标签: 值』+ 分组明细行」）。
// 纯字符串、零副作用、通道无关：本文件**不碰** IPC / 剪贴板 / DOM，`t` 与 `dash` 都由调用方给
// （消费方 = `UsageExportActions` 的「复制文本」按钮）。
//
// 六条口径（逐条不得发挥）：
//  ① 首行 = `usage.title` · `rangeLabelOf(dash.range)`；趋势首末键算得出跨度时追加**全角括号**
//     `（09/27 – 10/03）`（半角是另一套口径）。标题键就用 `usage.title` —— **不新增** `summaryTitle`
//     （同值，en 侧历史上还大小写分叉过）。
//  ② 指标行顺序固定 `requestInput → cacheRead → cacheMiss → output → hitRate → requests → asOf`，
//     标签一律取 `usage.grid.*`（与 A 总览同一批键，不另造一套文案）；V2 已删除「用户输入(估)」。
//  ③ 值一律**精确千分位**（`fmtInt`，含请求次数）：摘要没有列宽压力，**不用**万/亿缩写（那是看板网格的取舍）。
//     命中率取 `fmtPct`（**0–1 分数**口径，全仓唯一一套，不得在此引入 0–100 的第二套）。
//  ④ 不可得一律 `EM_DASH`、绝不填 0（§3 第 5 条）；
//     `collectedAt === 0` 是「尚未采集」哨兵（§3 第 8 条）→ 渲染 `usage.notCollected`，不是 `—`、更不是 1970 年。
//  ⑤ 分组明细行全交 Task 2 的 `distributionRows`：行序（值降序）、零值行丢弃、≤10 行截断与 `moreCount`
//     都是那一层的裁决，本文件**不重算**；值取 `DistRow.value`（= `requestTotal + output`，与 hero /
//     分布卡同口径），**不另立**「摘要专用值」——brief 逐值核对里那两个样例数（`1,246,567` / `460,789`）
//     取自更早的 `requestTotal` 口径，Task 2 已把该字段改成 `requestTotal + output`（1,292,245 / 484,245），
//     本文件按**冻结层**口径走（依据与代价见 `tests/pet/usage-export-text.test.ts` 文件头与 task-11 报告）；
//     空结果出 `EM_DASH`，`moreCount > 0` 时追加 `usage.moreN`。
//     行名分派逐维不同：provider → `t(label)`（真机不可得时是 i18n 键，直渲会印键名）、
//     tool → `usageAgentLabel(label)`（该维度 `label` 是工具 id，展示名只有 `AGENT_BADGE` 一个出处，§3 第 13 条）、
//     project / model → **原文**（D21 不做大小写规范化；模型名不短化，摘要没有列宽约束）。
//  ⑥ 隐私白名单（§3 第 23 条）：本文件只把结构化数字与静态文案拼进字符串，**读不到**任何会话正文
//     （入参 `UsageDashboard` 里根本没有正文字段），有 `tests/pet/usage-export-text.test.ts` 的纯函数断言
//     与 `tests/pet/usage-privacy.test.tsx` 的渲染级断言双重门禁。
import { distributionRows, type DistRow } from "@/lib/usage/distribution";
import { EM_DASH, fmtInt, fmtPct, usageAgentLabel } from "@/lib/usage/format";
import { asOfText, rangeLabelOf, spanLabelOf, tokenUnmeasured, type TFn } from "@/lib/usage/range";
import type { UsageDashboard, UsageGroupBy, UsageRange } from "@/types/usage";

/**
 * 行名分派（逐维不同，见文件头 ⑤）。本函数**不做短化**：摘要是纯文本，没有列宽约束。
 * **导出面共用的唯一实现**：分享图装配（`useExportShare.ts`）也 import 本函数，不另写一份
 * 同规则副本（两处口径必须永远一致：provider → `t(label)`、tool → `usageAgentLabel`、其余原文）。
 */
export function labelOf(row: DistRow, groupBy: UsageGroupBy, t: TFn): string {
  if (groupBy === "provider") return t(row.label);
  if (groupBy === "tool") return usageAgentLabel(row.label);
  return row.label;
}

/**
 * 看板数据 → 可复制的纯文本摘要（spec P4）。逐行版式见文件头；返回**多行字符串**（`\n` 分隔，无尾换行）。
 * `dash.trend` 少于 2 点时 `spanLabelOf` 出空串 ⇒ 首行不带括号段（不造假跨度）。
 */
export function buildTextSummary(dash: UsageDashboard, t: TFn): string {
  const head = `${t("usage.title")} · ${rangeLabelOf(dash.range, t)}`;
  const span = spanLabelOf(dash.trend);
  const lines: string[] = [span ? `${head}（${span}）` : head];

  const { totals, totalsBuckets } = dash;
  // **没量到 token** 的窗口：由 token 桶派生的行一律 `—`、**绝不印 0**；**计数类（请求次数）照常出真值**。
  // 判据只一处 = `range.ts::tokenUnmeasured`（**窗口级**）。V2 已删除「用户输入(估)」行。
  // ⚠️ **CSV 是例外**：那是数据文件（11 列原始整数、给 Excel / 脚本用），保留原始 0 与 userEst 列。
  // 本函数只管**给人看**的文本摘要。
  const unmeasured = tokenUnmeasured(dash);
  const token = (v: number) => (unmeasured ? EM_DASH : fmtInt(v));
  // 顺序与 A 总览一致：请求输入 → 缓存命中 → 未缓存命中 → 产出 → 命中率 → 请求次数。
  lines.push(`${t("usage.grid.requestInput")}: ${token(totals.requestTotal)}`);
  lines.push(`${t("usage.grid.cacheRead")}: ${token(totalsBuckets.cacheRead)}`);
  // cacheMiss 与 A 同公式（requestTotal - cacheRead），上期无需另算
  lines.push(
    `${t("usage.grid.cacheMiss")}: ${token(totals.requestTotal - totalsBuckets.cacheRead)}`
  );
  lines.push(`${t("usage.grid.output")}: ${token(totalsBuckets.output)}`);
  lines.push(`${t("usage.grid.hitRate")}: ${unmeasured ? EM_DASH : fmtPct(totals.cacheHitRate)}`);
  // **计数类**（请求次数）与采集时刻**不受影响**：它们是真值，不是 token 量
  lines.push(`${t("usage.grid.requests")}: ${fmtInt(totals.requests)}`);
  // 采集时刻的唯一口径出处：`asOfText`（`collectedAt === 0` → `usage.notCollected`，见文件头 ④）
  lines.push(`${t("usage.grid.asOf")}: ${asOfText(dash.collectedAt, t)}`);

  lines.push(`${t(`usage.group.${dash.groupBy}`)}:`);
  const { rows, moreCount } = distributionRows(dash.rows);
  if (rows.length === 0) {
    // 空分布出占位而不是空段：粘出去的一行「工具:」后面什么都没有会让人以为复制失败
    lines.push(EM_DASH);
  } else {
    for (const row of rows) lines.push(`${labelOf(row, dash.groupBy, t)}  ${fmtInt(row.value)}`);
    if (moreCount > 0) lines.push(t("usage.moreN", { n: moreCount }));
  }

  return lines.join("\n");
}

/**
 * CSV 文件名：`custom` → `mam-usage-<from>_<to>.csv`，其余档 → `mam-usage-<preset>.csv`。
 * **必以 `.csv` 结尾**——Rust 落盘层的 `with_bom_if_csv` 只认该后缀（无 BOM 时中文 Windows 版 Excel
 * 会按本地代码页解码，中文列名乱码）；也**恒以 `mam-usage-` 开头**（§3 第 27 条，天然避开 Windows 保留设备名）。
 * 自定义档缺边界时退化成档位名（仍是 `.csv`），不拼出 `mam-usage-_2026-10-03.csv` 这种半截名字。
 */
export function csvFilename(range: UsageRange): string {
  if (range.preset === "custom" && range.from && range.to) {
    return `mam-usage-${range.from}_${range.to}.csv`;
  }
  return `mam-usage-${range.preset}.csv`;
}

/**
 * 分享图文件名：`mam-usage-<窗口标识>-<导出时刻>.png`。
 *
 * **为什么必须带导出时刻**（2026-10-06 缺陷修复 X1）：落盘层是**裸 `fs::write`、没有重名处理**
 * （`src-tauri/src/commands/export.rs::save_bytes_file_in`）⇒ 同名即**静默覆盖**，而界面照样提示
 * 「已保存：<路径>」。原文件名 `mam-usage-<preset>.png` 对 `today` / `last5h` / `last7d` / `last30d`
 * **跨天同名**，对 `custom` 更是**每次同名**（连导两次自定义区间，第二次就把第一次覆盖掉）——
 * 被覆盖的那张图不会有任何提示。
 *
 * 窗口标识只用来**看得出这是哪个区间**（`custom` 带 `from_to`，与 `csvFilename` 同规则），
 * 唯一性由时刻保证（秒级；同一秒内连导两次仍是同名，但那是人手点不出来的速度）。
 *
 * ⚠️ **CSV 刻意不加时刻**（`csvFilename` 保持稳定名）：它是脚本按名取用的数据文件，
 * 稳定的 `mam-usage-last7d.csv` 有用。代价是 **CSV 跨天同名仍会被覆盖** —— 已登记为 PR 的
 * 「已知边界」，要改口径就同时改这两个函数（它们必须永远是同一套命名规则，见 `usage-export-*` 用例）。
 *
 * `now` 由调用方给（默认取当前时刻）：纯函数要能钉住时刻才可测。
 */
export function pngFilename(range: UsageRange, now: Date = new Date()): string {
  const p = (v: number) => String(v).padStart(2, "0");
  const stamp =
    `${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}` +
    `-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}`;
  const base =
    range.preset === "custom" && range.from && range.to
      ? `${range.from}_${range.to}`
      : range.preset;
  return `mam-usage-${base}-${stamp}.png`;
}
