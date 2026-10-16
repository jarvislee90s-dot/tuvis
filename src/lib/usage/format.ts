// 用量域显示规范纯层（计划② Task 2）——「数字怎么显示」的唯一出处。
// 消费方：首屏汇总 / 趋势卡 / 工作小结 / 分布卡 / 浮窗 / 三种导出（Task 3/4/6/7/8/9/11/12/13）。
// 纪律：零 DOM API、零新增依赖；数字口径逐字取自 spec P8「显示规范与状态」的格式化表；
// 纯函数测试不进 jsdom 专属 API（计划 §3 第 33/34 条）。
import { AGENT_BADGE } from "@/lib/agentBadge";
import type { Lang } from "@/lib/usage/lang";

/**
 * 「不可得」占位（值本身为 `null` / 算不出）。**② 新增文件内共用**，各组件不得自造。
 * ① 的 i18n 值 `settings.usageStatus.notAvailable` 是同一占位（`—`），但不属本计划，故此处另立常量。
 */
export const EM_DASH = "—";

/**
 * 「对象名未知」占位（拿到的是个不认识的标识，如 `toolId` 不在 `AGENT_BADGE` 里）。
 * **② 新增文件内共用**，各组件不得自造。
 * ⚠️ 与 `EM_DASH` **不是一回事、不得互替**：`EM_DASH` = 这个值拿不到（null / 不可得）；
 * `UNKNOWN_TOKEN` = 值拿到了，但我不认识这个对象。
 */
export const UNKNOWN_TOKEN = "?";

/**
 * 常规 token 数值（spec P8，**2026-10-06 用户裁决修订**）——精度只按「有没有单位」分两档：
 *
 * | 形态 | 精度 | 例 |
 * |---|---|---|
 * | **不带单位**（< 1 万） | **小数点后 0 位**（完全整数） | `8462` / `999` |
 * | 带单位「**万**」（< 1 亿） | **恒 2 位小数** | `1.00万` / `85.00万` / `123.46万` |
 * | 带单位「**亿**」（≥ 1 亿） | **恒 2 位小数** | `1.00亿` / `20.00亿` / `33.79亿` |
 *
 * 用户裁决原文：「① 如果不带单位（例如 8462 个 token），则保持为完全的整数（即小数点后 0 位）；
 * ② 如果记录的单位是『万』或『亿』，则统一保持小数点后两位。**该规则对所有的工具都生效**。」
 * ⇒ 相对 spec P8 旧表的两处变化：**万档 1 位 → 2 位**、**两档一律不再剥尾零**（旧：万剥 `.0`、亿去尾零）。
 * 负数与非有限值归 `"0"`（绝不显示 `NaN`）。
 * ⚠️ 与 `fmtInt` 刻意区分：hero 大数字与所有 hover「精确值」不走本函数（那是完整千分位）。
 *
 * en 档（issue #160 决策 1）：`<1K` 整数 / `<1M` X.XXK / `<1B` X.XXM / `≥1B` X.XXB；
 * en 时长单位词见 `fmtDur` / `fmtLongest`（`X.X min` / `X.X s` / `N ms`）。
 * 「档内进位不跨档」两个钉死用例：999_999 → `1000.00K`（不翻 M）、999_999_999 → `1000.00M`（不翻 B）。
 */
export function fmtTokens(n: number, locale: Lang = "zh"): string {
  if (!Number.isFinite(n) || n <= 0) return "0";
  if (locale === "en") {
    if (n < 1_000) return String(Math.round(n));
    if (n < 1_000_000) return (n / 1_000).toFixed(2) + "K";
    if (n < 1_000_000_000) return (n / 1_000_000).toFixed(2) + "M";
    return (n / 1_000_000_000).toFixed(2) + "B";
  }
  if (n < 10_000) return String(Math.round(n));
  if (n < 100_000_000) return (n / 10_000).toFixed(2) + "万";
  return (n / 100_000_000).toFixed(2) + "亿";
}

/** hero 大数字（spec P8）：**完整千分位**、四舍五入，不用万/亿缩写。非有限值归 `"0"`。 */
export function fmtInt(n: number): string {
  if (!Number.isFinite(n)) return "0";
  const rounded = Math.round(n);
  // `Math.round(-0.4)` 得 `-0`（`toLocaleString` 会印成 "-0"）：先归零，界面不出现负零
  return (rounded === 0 ? 0 : rounded).toLocaleString("en-US");
}

/**
 * 百分比（spec P8）：入参是 **0–1 分数**（契约要点 7 的口径），出参 1 位小数 + `%`。
 * 全仓只有这一套口径，**不得**在此引入 0–100 的第二套；`compare === null` 时由消费方不渲染，
 * 不要靠本函数把缺失值化成 `0.0%`。
 */
export function fmtPct(x: number): string {
  return (x * 100).toFixed(1) + "%";
}

/** 时长三档（spec P8）：≥60 秒 → 分钟（1 位小数）；≥1 秒 → 秒（1 位小数）；否则毫秒（取整）。
 *  en（issue #160）：数值口径不变，仅换单位词 —— `X.X min` / `X.X s` / `N ms`。 */
export function fmtDur(ms: number, locale: Lang = "zh"): string {
  if (ms >= 60_000) return (ms / 60_000).toFixed(1) + (locale === "en" ? " min" : " 分钟");
  if (ms >= 1_000) return (ms / 1_000).toFixed(1) + (locale === "en" ? " s" : " 秒");
  return Math.round(ms) + (locale === "en" ? " ms" : " 毫秒");
}

/** 最长单 turn 两档（spec P8 / D19）：≥60 秒 → 分钟（1 位小数）；否则整秒（0 ms 落「0 秒」）。
 *  en（issue #160）：数值口径不变，仅换单位词 —— `X.X min` / `N s`。 */
export function fmtLongest(ms: number, locale: Lang = "zh"): string {
  if (ms >= 60_000) return (ms / 60_000).toFixed(1) + (locale === "en" ? " min" : " 分钟");
  return Math.round(ms / 1_000) + (locale === "en" ? " s" : " 秒");
}

/**
 * 采集时刻（Task 6 `usage.grid.asOf` / Task 11 文本摘要）：**本地时区** `YYYY-MM-DD HH:mm:ss`。
 * 非有限值与 `≤0` 归 `EM_DASH` —— `collectedAt === 0` 是「尚未采集」哨兵（计划 §3 第 8 条），
 * 消费方先判并渲染 `usage.notCollected`；纯层绝不把哨兵印成 `1970-01-01`。
 */
export function fmtDateTime(ts: number): string {
  if (!Number.isFinite(ts) || ts <= 0) return EM_DASH;
  const d = new Date(ts);
  const p = (v: number) => String(v).padStart(2, "0");
  return (
    `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}` +
    ` ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
  );
}

/**
 * 用量面工具显示名的**唯一口径**（计划 §3.13）= `AGENT_BADGE[toolId].label`。
 * **不得**改用 `getAgentLabel`（§3.14）：它按 APP/CLI 分形，对 codex 返回 `"Codex CLI"`，
 * 而用量域 codex 是单一源，必须显示 `"Codex"`。
 * `AGENT_BADGE` 里没有的 `toolId` → `UNKNOWN_TOKEN`（对象名未知，见该常量注释）。
 */
export function usageAgentLabel(toolId: string): string {
  const badge = AGENT_BADGE[toolId];
  return badge ? badge.label : UNKNOWN_TOKEN;
}
