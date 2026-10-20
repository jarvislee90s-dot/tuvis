// Task 11（计划②）· 复制文本摘要纯层 `src/lib/usage/exportText.ts` 的行为锁（5 用例；第 5 条是
// 2026-10-06 用户裁决补的「没量到 token 的窗口不印 0」，第三轮补了「`用户输入(估)` 恢复 KEEP」）。
// 判据来源：spec P4「纯文本 = 头部一行（看板名 + 时间范围）+ 各指标行『标签: 值』+ 分组明细行」；
// 计划 §3 第 5 条（不可得一律 EM_DASH，绝不填 0）、第 8 条（`collectedAt === 0` 哨兵）、
// 第 10 条（provider 维度的 label 是 i18n 键 → 必须 `t(label)`）、第 13 条（工具展示名唯一口径
// `usageAgentLabel`）、第 23 条（隐私白名单：只有静态文本与结构化摘要）、第 26/27 条（导出名形态）。
//
// 纪律（计划 §3 第 33/34 条）：本文件是**纯函数**测试——不 render、不读 `document` / `window`；
// `t` 走真实 i18n（zh），缺键或文案漂移会被本文件咬住（不是自造一份假文案自说自话）。
//
// ⚠️ 分布行的取值口径（本任务唯一的判断题，已登记在 task-11-report.md 的「偏离」一节）：
//   行来自 Task 2 的 `distributionRows`，值取 `DistRow.value`（= `requestTotal + output`）——这是该纯层
//   的控制器裁决（「精确值…供 hover 精确值 / 文本摘要用」），也与 Task 7 分布卡同口径同值。
//   brief 逐值核对里钉的 `Claude  1,246,567` 是**旧版纯层**的样例值（当时 `distValue` =
//   `metrics.requestTotal`，见计划修订史 0264cb3 的 `distValue` 实现），此后 Task 2 已把它改成
//   `requestTotal + output`（夹具同一行 = 1,292,245）。本文件按**冻结层**口径断言，不另立第二套「分布值」。
import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { buildTextSummary, csvFilename } from "@/lib/usage/exportText";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";
import { EM_DASH } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import type { UsageDashboard, UsageRange, UsageRow } from "@/types/usage";

/** i18next 的 `t` 直接作 `TFn` 传入（与 `usage-page.test.tsx` 同法） */
const t = i18n.t.bind(i18n) as unknown as TFn;

beforeAll(async () => {
  // detector 在 jsdom 下读 navigator（en-US）⇒ 默认英文；本文件断言的中文文案只有 zh 有
  await i18n.changeLanguage("zh");
});

/** 夹具行：把量清零（用于「空分布」分支——值 = requestTotal + output） */
function zeroed(row: UsageRow): UsageRow {
  return {
    ...row,
    buckets: { ...row.buckets, output: 0 },
    metrics: { ...row.metrics, requestTotal: 0 },
  };
}

describe("buildTextSummary（计划② Task 11：复制文本摘要）", () => {
  it("1. 首行 = 看板名 · 范围（+ 全角括号跨度）+ 6 行指标 + 分组行 + 分布行（精确千分位）", () => {
    const text = buildTextSummary(mockUsageDashboard({ preset: "last7d" }, "tool"), t);
    const lines = text.split("\n");

    // 首行跨度用**全角括号**（半角是另一套口径，不得「顺手修正」）；跨度 = 趋势首末键（09/27 – 10/03）
    expect(lines[0]).toBe("用量看板 · 近 7 天（09/27 – 10/03）");
    // V2 指标行顺序（与 A 总览一致）：requestInput → cacheRead → cacheMiss → output → hitRate → requests → asOf
    // 2026-10-20 Task 8：计量维度值统一——新增输入 / 输出（i18n 值变更，键与行序不变）
    expect(lines[1]).toBe("新增输入: 1,954,268");
    expect(lines[2]).toBe("缓存命中: 1,318,123");
    // 未缓存命中 = requestTotal - cacheRead（与 A 同公式）
    expect(lines[3]).toBe("未缓存命中输入: 636,145");
    expect(lines[4]).toBe("输出: 82,713");
    // 命中率 = 1,318,123 ÷ 1,954,268 = 67.448…% → 一位小数（0–1 分数口径，全仓唯一）
    expect(lines[5]).toBe("命中率: 67.4%");
    expect(lines[6]).toBe("请求次数: 460");
    // 采集时刻只断**时刻形态**（brief：只断 /\d{2}:\d{2}:\d{2}/）：fmtDateTime 走宿主本地时区，
    // 钉死具体小时数会在别的时区红
    expect(lines[7]).toMatch(/\d{2}:\d{2}:\d{2}$/);
    expect(lines[7]).toContain("数据截止时间: ");
    // 分组行头 = `usage.group.<groupBy>` + `:`（与大看板分布卡的维度按钮同一批键）
    expect(lines[8]).toBe("按工具:");
    // 分布行 = `label + 两个空格 + fmtInt(value)`：**精确千分位，不用万/亿缩写**；行序按值降序（Task 2 纯层）
    expect(lines[9]).toBe("Claude  1,292,245");
    expect(lines[10]).toBe("Codex  484,245");
    expect(lines[11]).toBe("ZCode  246,912");
    expect(lines[12]).toBe("WorkBuddy  13,579");
    expect(lines).toHaveLength(13);

    // 隐私白名单（§3 第 23 条）：只有静态文本与结构化数字，不得混入会话正文 / 密钥类字段
    for (const banned of ["lastMessage", "prompt", "apiKey"]) expect(text).not.toContain(banned);
  });

  it("2. 哨兵与空态：collectedAt 0 → 尚未采集、空分布 → —、超 10 行 → 等 N", () => {
    const d = mockUsageDashboard({ preset: "last7d" }, "tool");

    const dash = buildTextSummary({ ...d, collectedAt: 0 }, t);
    // `collectedAt === 0` 是「尚未采集」哨兵 ⇒ 渲染 usage.notCollected，不是 EM_DASH、更不是 1970 年
    expect(dash).toContain("数据截止时间: 尚未采集");
    expect(dash).not.toContain("1970-01-01");

    // 空分布（全零行在 Task 2 纯层就被丢掉）→ 出 EM_DASH，不留一行 0
    const empty = buildTextSummary({ ...d, rows: d.rows.map(zeroed) }, t);
    const emptyLines = empty.split("\n");
    expect(emptyLines[emptyLines.length - 1]).toBe(EM_DASH);
    expect(emptyLines[8]).toBe("按工具:");

    // 超 DIST_MAX_ROWS 行 → 折「等 N」（moreCount 原样进摘要，不是被丢掉的零值行数）。
    // 这里换 model 维度：行名走原文（不经过工具展示名解析），断言能直接咬住 label + 两个空格 + 值。
    const many: UsageDashboard = {
      ...d,
      groupBy: "model",
      rows: Array.from({ length: 12 }, (_, i) => {
        const base = d.rows[0];
        return {
          ...base,
          key: `m${i}`,
          label: `m${i}`,
          buckets: { ...base.buckets, output: 0 },
          metrics: { ...base.metrics, requestTotal: 1_000 - i * 10 },
        };
      }),
    };
    const folded = buildTextSummary(many, t).split("\n");
    expect(folded[8]).toBe("按供应商/模型:");
    const dist = folded.filter((l) => l.includes("  ")); // 两个空格 = 分布行的分隔符
    expect(dist).toHaveLength(10);
    expect(dist[0]).toBe("m0  1,000");
    expect(folded[folded.length - 1]).toBe("等 2");
  });

  it("3. 维度分派：provider 走 t(label)（不可得出「未知」而非键名）、tool 走展示名、project / model 原文", () => {
    // provider：夹具最后一行 label 是 i18n 键（真机不可得供应商就是它）⇒ 必须 t() 后才不是键名
    const prov = buildTextSummary(mockUsageDashboard({ preset: "last7d" }, "provider"), t);
    expect(prov).toContain("按供应商:");
    expect(prov).toContain("anthropic  1,292,245");
    expect(prov).toContain("未知  13,579");
    expect(prov).not.toContain("usage.label.unknownProvider");

    // project：D21 —— key 小写、label **原文**（不做大小写规范化）
    const proj = buildTextSummary(mockUsageDashboard({ preset: "last7d" }, "project"), t);
    expect(proj).toContain("按项目:");
    expect(proj).toContain("MultiAgents-Manager  1,292,245");

    // model：文本摘要**不短化**（2026-10-06 起分布卡也不再短化——模型维度已合并为
    // 「供应商 / 模型」，见 `UsageDistributionCard` 文件头纪律 ②）
    const model = buildTextSummary(mockUsageDashboard({ preset: "last7d" }, "model"), t);
    expect(model).toContain("按供应商/模型:");
    expect(model).toContain("claude-sonnet-4-5  1,292,245");
  });

  it("4. csvFilename：恒以 mam-usage- 开头、必以 .csv 结尾（BOM 只认该后缀）", () => {
    expect(csvFilename({ preset: "last7d" })).toBe("mam-usage-last7d.csv");
    expect(csvFilename({ preset: "today" })).toBe("mam-usage-today.csv");
    expect(csvFilename({ preset: "custom", from: "2026-09-01", to: "2026-10-03" })).toBe(
      "mam-usage-2026-09-01_2026-10-03.csv"
    );

    const all: UsageRange[] = [
      { preset: "last5h" },
      { preset: "last30d" },
      { preset: "custom", from: "2026-09-01", to: "2026-10-03" },
      // 防御式：自定义档缺边界时也不得产出不以 .csv 结尾的名字
      { preset: "custom" },
    ];
    for (const r of all) {
      expect(csvFilename(r).startsWith("mam-usage-")).toBe(true);
      expect(csvFilename(r).endsWith(".csv")).toBe(true);
    }
  });

  it("5. **没量到 token 的窗口**：四桶派生的行一律 `—`、请求次数出真值、分组段落走空态（P8）", () => {
    // 真机形状：四桶全零但 requests ≥ 1（**真空回合**行）。用户裁决：「只有是真实的数值，你才能
    // 写它的数值。如果没有产生数值，就不显示了」⇒ 摘要里由四桶派生的行出 `—`；**计数类照常**。
    const d = mockUsageDashboard({ preset: "last7d" }, "tool");
    // 前提锚（2026-10-06 第三轮补）：下面「请求次数出真值」那句若夹具的 requests 恰是 0，
    // 就会**空洞地绿**（`请求次数: 0` 也能对上模板）⇒ 先钉住它是**非零真值**。
    expect(d.totals.requests).toBeGreaterThan(0);
    const zeroBuckets = { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 };
    // ⚠️ **本夹具只喂被断言的字段**（`totalsBuckets` / `totals` 与逐行四个桶），**不是真机形状**：
    // 真机在这一档 `recentSession` 必为 `null`（四桶全零 ⇒ 每行 `request_total` 也是 0 ⇒ 后端候选集
    // 为空，见 `mockFixtures.ts::zeroTokensOf` 的长注释），这里的 `trend` 也没清零——**本文件
    // 没有任何消费方读它们**（`buildTextSummary` 只看 `totals` / `totalsBuckets` / `rows` /
    // `collectedAt` / `range` / `trend` 的首末键）。后来者**不要照抄**这个形状。
    const noTokens: UsageDashboard = {
      ...d,
      rows: d.rows.map((r) => ({
        ...r,
        buckets: { ...zeroBuckets },
        metrics: { ...r.metrics, requestTotal: 0, cacheHitRate: 0 }, // requests 是真值，保留
      })),
      totalsBuckets: { ...zeroBuckets },
      totals: { ...d.totals, requestTotal: 0, cacheHitRate: 0 },
      hero: 0,
    };
    const lines = buildTextSummary(noTokens, t).split("\n");
    expect(lines[1]).toBe(`新增输入: ${EM_DASH}`);
    expect(lines[2]).toBe(`缓存命中: ${EM_DASH}`);
    expect(lines[3]).toBe(`未缓存命中输入: ${EM_DASH}`);
    expect(lines[4]).toBe(`输出: ${EM_DASH}`);
    expect(lines[5]).toBe(`命中率: ${EM_DASH}`);
    // **计数类真值照常**（用户裁决逐字点名「请求次数」）：断**具体数字**而不是模板
    expect(lines[6]).toBe("请求次数: 460");
    expect(lines[6]).not.toContain(EM_DASH);
    // 采集时刻不受影响：这一档是「采集过但没有 token」，不是「尚未采集」
    expect(lines[7]).toMatch(/\d{2}:\d{2}:\d{2}$/);
    // 分组段落：分布行（值 = requestTotal + output）全为 0 ⇒ `distributionRows` 丢光 ⇒ 走既有空态分支
    expect(lines[8]).toBe("按工具:");
    expect(lines[9]).toBe(EM_DASH);
    expect(lines).toHaveLength(10);

    // **反断言**：正常窗口照旧印数字（不得把正常路径也置空）
    const ok = buildTextSummary(d, t).split("\n");
    expect(ok[1]).toBe("新增输入: 1,954,268");
    expect(ok[2]).toBe("缓存命中: 1,318,123");
    expect(ok[3]).toBe("未缓存命中输入: 636,145");
    expect(ok[4]).toBe("输出: 82,713");
    expect(ok[5]).toBe("命中率: 67.4%");
    expect(ok[9]).toBe("Claude  1,292,245");
  });

  it("6. V2：userEst=999 不再进入文本摘要（不是 API Token）", () => {
    const d = mockUsageDashboard({ preset: "last7d" }, "tool");
    const text = buildTextSummary({ ...d, totals: { ...d.totals, userEst: 999 } }, t);
    expect(text).not.toContain("用户输入");
    expect(text).not.toContain("~999");
    expect(text.split("\n").filter((l) => l.includes("用户输入"))).toHaveLength(0);
  });
});
