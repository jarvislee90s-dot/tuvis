// Task 2（计划②）· 显示规范纯层 `src/lib/usage/format.ts` 的行为锁。
// 期望值逐字取自 spec P8「显示规范与状态」（数字格式化表）：
//   <1 万显原值；<1 亿「万」1 位小数＋剥末尾 `.0`；≥1 亿「亿」2 位小数＋去全部尾零；
//   hero 用完整千分位（不用万/亿）；命中率 1 位小数＋`%`（入参是 **0–1 分数**）；
//   时长 ≥60s → 分钟 1 位小数、≥1s → 秒 1 位小数、否则毫秒；最长单 turn 两档（分钟 / 整秒）。
// 纯函数：**不使用任何 jsdom 专属 API**（计划 §3 第 33/34 条），node 上下文也能直测。
import { describe, expect, it } from "vitest";
import {
  EM_DASH,
  UNKNOWN_TOKEN,
  fmtDateTime,
  fmtDur,
  fmtInt,
  fmtLongest,
  fmtPct,
  fmtTokens,
  usageAgentLabel,
} from "@/lib/usage/format";

describe("format.ts（spec P8 显示规范）", () => {
  it("1. fmtTokens 万档：**恒 2 位小数**（2026-10-06 用户裁决：带单位一律 2 位，不再剥尾零）", () => {
    expect(fmtTokens(999)).toBe("999");
    expect(fmtTokens(9_999)).toBe("9999");
    // 万档分界：10000 / 1e4 = 1 → 恒 2 位 ⇒ "1.00万"（2026-10-06 裁决：不再剥尾零）
    expect(fmtTokens(10_000)).toBe("1.00万");
    expect(fmtTokens(12_500)).toBe("1.25万");
    expect(fmtTokens(1_234_567)).toBe("123.46万");
  });

  it("2. fmtTokens 亿档：**恒 2 位小数**（2026-10-06 用户裁决：带单位一律 2 位，不再去尾零）", () => {
    expect(fmtTokens(100_000_000)).toBe("1.00亿");
    expect(fmtTokens(150_000_000)).toBe("1.50亿");
    expect(fmtTokens(123_456_789)).toBe("1.23亿");
    expect(fmtTokens(1_012_000_000)).toBe("10.12亿");
    expect(fmtTokens(2_000_000_000)).toBe("20.00亿");
  });

  it("3. fmtTokens 负数与非有限值归 0（绝不显示 NaN）", () => {
    expect(fmtTokens(0)).toBe("0");
    expect(fmtTokens(-1)).toBe("0");
    expect(fmtTokens(-1_500_000_000)).toBe("0");
    expect(fmtTokens(Number.NaN)).toBe("0");
    expect(fmtTokens(Number.POSITIVE_INFINITY)).toBe("0");
    expect(fmtTokens(Number.NEGATIVE_INFINITY)).toBe("0");
  });

  it("4. fmtInt 完整千分位（hero 口径，不用万/亿缩写）", () => {
    expect(fmtInt(2_036_981)).toBe("2,036,981");
    expect(fmtInt(1_246_567)).toBe("1,246,567");
    expect(fmtInt(999)).toBe("999");
    expect(fmtInt(0)).toBe("0");
  });

  it("5. fmtInt 四舍五入后再分组", () => {
    expect(fmtInt(1_234_567.6)).toBe("1,234,568");
    expect(fmtInt(1_234.4)).toBe("1,234");
    expect(fmtInt(-0.4)).toBe("0");
  });

  it("6. fmtPct 入参是 0–1 分数，出参 1 位小数 + %", () => {
    expect(fmtPct(0)).toBe("0.0%");
    expect(fmtPct(0.5)).toBe("50.0%");
    expect(fmtPct(0.674)).toBe("67.4%");
    expect(fmtPct(1)).toBe("100.0%");
  });

  it("7. fmtDur 分钟档：≥60 秒 → 1 位小数分钟", () => {
    expect(fmtDur(60_000)).toBe("1.0 分钟");
    expect(fmtDur(310_607)).toBe("5.2 分钟");
    expect(fmtDur(1_864_000)).toBe("31.1 分钟");
  });

  it("8. fmtDur 秒档：≥1 秒 → 1 位小数秒", () => {
    expect(fmtDur(1_000)).toBe("1.0 秒");
    expect(fmtDur(4_500)).toBe("4.5 秒");
    expect(fmtDur(59_999)).toBe("60.0 秒");
  });

  it("9. fmtDur 毫秒档：否则毫秒取整", () => {
    expect(fmtDur(0)).toBe("0 毫秒");
    expect(fmtDur(250.6)).toBe("251 毫秒");
    expect(fmtDur(999)).toBe("999 毫秒");
  });

  it("10. fmtLongest 两档：≥60 秒 → 分钟 1 位小数；否则整秒", () => {
    expect(fmtLongest(50_311_958)).toBe("838.5 分钟");
    expect(fmtLongest(60_000)).toBe("1.0 分钟");
    expect(fmtLongest(59_000)).toBe("59 秒");
    expect(fmtLongest(1_000)).toBe("1 秒");
    expect(fmtLongest(0)).toBe("0 秒");
  });

  // 第 11 条（`shortModel`）已于 2026-10-06 删除：模型维度合并为「供应商 / 模型」后，
  // 分布卡改用**整列取宽**（`.grid-cols-[minmax(0,auto)_…]`）而不是 JS 短化，
  // `shortModel` 失去唯一消费方 ⇒ 函数与它的用例一并删除（不留死导出）。
  // 名称的「显示不全」由布局解决、由 `UsageDistributionCard` 的用例钉住。

  it("12. fmtDateTime：本地时刻 YYYY-MM-DD HH:mm:ss；≤0 / 非有限 → EM_DASH", () => {
    // 用本地日历构造入参 → 任意时区下期望值都是同一串（与夹具的时区无关写法同源）
    const local = new Date(2026, 9, 3, 14, 37, 5).getTime();
    expect(fmtDateTime(local)).toBe("2026-10-03 14:37:05");
    expect(fmtDateTime(new Date(2026, 0, 9, 8, 5, 0).getTime())).toBe("2026-01-09 08:05:00");
    // 形如「采集时刻」的形态（Task 6 的 asOf / Task 11 的文本摘要只断这个形态）
    expect(fmtDateTime(local)).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
    // collectedAt === 0 是「尚未采集」哨兵（消费方先判）；纯层不印 1970、不印 NaN
    expect(fmtDateTime(0)).toBe(EM_DASH);
    expect(fmtDateTime(-1)).toBe(EM_DASH);
    expect(fmtDateTime(Number.NaN)).toBe(EM_DASH);
  });

  // 计划 §3.13 / §3.14：用量面工具名的唯一口径（本任务步骤 3 的必备导出）
  it("13. usageAgentLabel = AGENT_BADGE[toolId].label；codex 不得分形；未知 id → UNKNOWN_TOKEN", () => {
    expect(usageAgentLabel("claude")).toBe("Claude");
    // 不得走 getAgentLabel —— 它对 codex 按 APP/CLI 分形返回 "Codex CLI"
    expect(usageAgentLabel("codex")).toBe("Codex");
    expect(usageAgentLabel("kimi")).toBe("Kimi Code");
    expect(usageAgentLabel("zcode")).toBe("ZCode");
    expect(usageAgentLabel("dsh")).toBe("DSH");
    // AGENT_BADGE 里没有的对象名 → 「对象名未知」，不是「不可得」
    expect(usageAgentLabel("no-such-tool")).toBe(UNKNOWN_TOKEN);
    // 两个占位常量语义不同，取值也不同，不得互替
    expect(EM_DASH).toBe("—");
    expect(UNKNOWN_TOKEN).toBe("?");
  });
});

describe("format.ts en 档（issue #160 决策 1：K/M/B + min/s/ms）", () => {
  it("14. fmtTokens en：<1K 纯整数（含进位同构 zh 9999.6→10000）", () => {
    expect(fmtTokens(999, "en")).toBe("999");
    expect(fmtTokens(999.6, "en")).toBe("1000");
    expect(fmtTokens(0, "en")).toBe("0");
    expect(fmtTokens(-1, "en")).toBe("0");
    expect(fmtTokens(Number.NaN, "en")).toBe("0");
    expect(fmtTokens(Number.POSITIVE_INFINITY, "en")).toBe("0");
  });
  it("15. fmtTokens en：K 档恒 2 位小数不剥尾零；档内进位钉死", () => {
    expect(fmtTokens(1_000, "en")).toBe("1.00K");
    expect(fmtTokens(12_500, "en")).toBe("12.50K");
    expect(fmtTokens(999_999, "en")).toBe("1000.00K"); // 999.999→toFixed(2)→1000.00，不跨档翻 M
  });
  it("16. fmtTokens en：M / B 档", () => {
    expect(fmtTokens(1_000_000, "en")).toBe("1.00M");
    expect(fmtTokens(1_234_567, "en")).toBe("1.23M");
    expect(fmtTokens(17_950_000, "en")).toBe("17.95M");
    expect(fmtTokens(123_456_789, "en")).toBe("123.46M");
    expect(fmtTokens(999_999_999, "en")).toBe("1000.00M"); // M 档内进位，不跨档翻 B
    expect(fmtTokens(1_000_000_000, "en")).toBe("1.00B");
    expect(fmtTokens(4_370_000_000, "en")).toBe("4.37B");
  });
  it("17. fmtDur en：数值口径不变，仅换单位词", () => {
    expect(fmtDur(60_000, "en")).toBe("1.0 min");
    expect(fmtDur(310_607, "en")).toBe("5.2 min");
    expect(fmtDur(1_864_000, "en")).toBe("31.1 min");
    expect(fmtDur(1_000, "en")).toBe("1.0 s");
    expect(fmtDur(4_500, "en")).toBe("4.5 s");
    expect(fmtDur(59_999, "en")).toBe("60.0 s");
    expect(fmtDur(0, "en")).toBe("0 ms");
    expect(fmtDur(250.6, "en")).toBe("251 ms");
    expect(fmtDur(999, "en")).toBe("999 ms");
  });
  it("18. fmtLongest en：两档", () => {
    expect(fmtLongest(50_311_958, "en")).toBe("838.5 min");
    expect(fmtLongest(60_000, "en")).toBe("1.0 min");
    expect(fmtLongest(59_000, "en")).toBe("59 s");
    expect(fmtLongest(1_000, "en")).toBe("1 s");
    expect(fmtLongest(0, "en")).toBe("0 s");
  });
  it("19. 缺省 locale = zh（漏传兜底：迁移遗漏不破坏中文界面）", () => {
    expect(fmtTokens(12_500)).toBe("1.25万");
    expect(fmtDur(60_000)).toBe("1.0 分钟");
    expect(fmtLongest(59_000)).toBe("59 秒");
  });
});
