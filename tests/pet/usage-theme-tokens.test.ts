// Task 4（计划②）· 主题门禁：图表三个取色变量的存在性 + 用量组件的取色纪律（4 条用例）。
// 判据来源：spec「主题适配（兔维斯 特有）」（颜色一律走 兔维斯 既有主题 token，禁止照搬参考实现的固定
// 浅色配色）与计划 §3 第 19/20/21 条。**四条用例同一个文件**（拆两处会让门禁半绿半红、无人看得懂）。
//
// ⚠️ **后 2 条是计划内的长红灯**（计划 §3 第 35 条）：它们要读 Task 7/8/9/12/13 才存在的 7 个文件，
// 故从 Task 4 落盘起到 Task 13 收尾前必然红；**首个报错是 `ENOENT`**——这是预期形态，
// 不得用 try/catch 或 `existsSync` 跳过（那样门禁会永久假绿，Task 14 收尾时形同不存在）。
// 各中间任务的收尾判定只看本任务新增/修改的测试文件，不得为「全绿」去改门禁或提前造别人的组件。
//
// **两条扫描用例的规则刻意不同，且文件清单逐条硬编码、绝不用 glob**：
//   · Group A（5 个用量组件）＝ 禁止参考实现色值 `#3BA7FF` / `#8D6BFF` / `#7a4a2b` / `#fffdf9` /
//     `255,252,248`。**不能改成 glob**：`src/components/pet/**` 下 `FoxbellPet.tsx`（608/653/674/741）
//     含 `#7a4a2b` 与 `rgba(255,252,248,…)`、`pet-cursor.css`（9/16）含 `#7a4a2b`——那是宠物本体的
//     棕褐色配色，不属本计划的用量面，且 §3 第 42 条明令不得改 `FoxbellPet.tsx`：用 glob 会让门禁永久假红。
//   · Group B（2 个导出模块）＝ 禁止出现 `var(--`（**不是**色值清单）。理由：§3 第 21 条要求分享图
//     固定浅色版式、不读任何 CSS 变量；而 Task 12 的版式表把 `#fffdf9` 定为**气泡底色**——
//     把 Group A 的色值清单套到 Group B 上会让 Task 12 永远做不到，与 §3 第 35 条
//     「四条第 14 个任务收尾时全绿」自相矛盾。
import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = process.cwd();
const CSS = readFileSync(path.join(root, "src/index.css"), "utf8");

/** 取某个顶层选择器块的正文（本仓 CSS 的这些块无嵌套花括号，非贪婪到行首 `}` 即可）。 */
function cssBlock(selector: string): string {
  const m = new RegExp(`(?:^|\\n)${selector}\\s*\\{([\\s\\S]*?)\\n\\}`).exec(CSS);
  if (!m) throw new Error(`src/index.css 里找不到 ${selector} 块`);
  return m[1];
}

/** 取块内某个自定义属性的取值（`null` = 该变量不存在）。 */
function cssVar(block: string, name: string): string | null {
  const m = new RegExp(`(?:^|\\n)\\s*${name}\\s*:\\s*([^;]+);`).exec(block);
  return m ? m[1].trim() : null;
}

/** 浅深两档的**唯一取值表**（计划② Task 4 步骤 1；用例 1/2 逐字比对）。
 * 2026-10-26 桌面工作区计划 Task 8：配色方案经用户定稿（spec §3.1），主图线/辅图
 * 随语义 token 更新为 #4263DC/#8B5CF6（浅）。
 * 2026-10-27 实机反馈：深色由雾银纯灰改冷石墨 + 靛蓝/紫罗兰（spec §3.1 修订），
 * 主图线/辅图 = #8B95FF/#B49CFF、轨道对齐选中背景 token——
 * 门禁机制不变（三变量仍是图表唯一取色来源）。 */
const LIGHT = {
  "--usage-accent-a": "#4263dc",
  "--usage-accent-b": "#8b5cf6",
  "--usage-track": "#e3e6ed",
};
const DARK: Record<keyof typeof LIGHT, string> = {
  "--usage-accent-a": "#8b95ff",
  "--usage-accent-b": "#b49cff",
  "--usage-track": "#2c2f45",
};

/**
 * Group A：五个用量组件（计划② Task 4 本任务产物 + Task 7/8/9/13 的产物）。
 * **逐条硬编码**，顺序即报错顺序：Task 4 落盘后首个 `ENOENT` 必须是 `UsageDistributionCard.tsx`。
 */
const USAGE_COMPONENTS = [
  "src/components/usage/TrendChart.tsx",
  "src/components/usage/UsageDistributionCard.tsx",
  "src/components/usage/UsageWorkSummary.tsx",
  "src/components/usage/UsageRecordsPanel.tsx",
  "src/components/pet/UsageMiniBar.tsx",
];

/** Group A 禁用清单：参考实现的硬编码色值（计划 §3 第 19 条）。 */
const FORBIDDEN_COLORS = ["#3BA7FF", "#8D6BFF", "#7a4a2b", "#fffdf9", "255,252,248"];

/** Group B：两个导出模块（Task 12 产物；唯一允许硬编码色值、但**不得**读 CSS 变量，§3 第 21 条）。 */
const EXPORT_MODULES = ["src/lib/usage/exportLayout.ts", "src/lib/usage/exportImage.ts"];

describe("usage-theme-tokens（计划 §3 第 19/20/21 条主题门禁）", () => {
  it("1. :root 定义图表三变量且取值 = 浅色档（图表唯一取色来源）", () => {
    const block = cssBlock(":root");
    for (const [name, value] of Object.entries(LIGHT)) {
      expect(cssVar(block, name), `:root 缺 ${name}`).toBe(value);
    }
  });

  it("2. .dark 定义同一组三变量且深浅取值两两不同（夜间可读）", () => {
    const light = cssBlock(":root");
    const dark = cssBlock(".dark");
    for (const [name, value] of Object.entries(DARK)) {
      const darkValue = cssVar(dark, name);
      expect(darkValue, `.dark 缺 ${name}`).toBe(value);
      // 「浅深各一套」的实质：三变量在深色下**必须**换值，照抄浅色档等于没做主题适配
      expect(darkValue, `${name} 深浅取值不得相同`).not.toBe(cssVar(light, name));
    }
  });

  it("3. 五个用量组件不得出现参考实现硬编码色值（缺文件 → ENOENT 抛出，不得吞）", () => {
    for (const rel of USAGE_COMPONENTS) {
      const src = readFileSync(path.join(root, rel), "utf8");
      for (const bad of FORBIDDEN_COLORS) {
        expect(src.includes(bad), `${rel} 不得出现硬编码色值 ${bad}（一律走主题 token）`).toBe(
          false
        );
      }
    }
  });

  it("4. 两个导出模块不得消费任何 CSS 变量（分享图固定浅色版式，§3 第 21 条）", () => {
    for (const rel of EXPORT_MODULES) {
      const src = readFileSync(path.join(root, rel), "utf8");
      expect(src.includes("var(--"), `${rel} 是固定浅色版式，不得读 CSS 变量`).toBe(false);
    }
  });
});
