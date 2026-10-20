// Task 8（计划②）· 工作小结区（spec D15–D19）· 纯层判据 4 条 + 组件行为 7 条（共 11 用例）。
// 判据来源：spec D15–D19 与 §9.5（会话数分层 / turn 逐工具不合计 / 报错三层 + 打断单列 /
// 最长单 turn 逐工具 p50+最长且标注含挂机 / 工具调用 2×2）、计划 §3 第 5/9/13/18 条、
// Task 8 步骤 3/4/5。
//
// 纪律：
//  * 语言固定 zh（同 `usage-page.test.tsx`）：`src/i18n/index.ts` 的 detector 在 jsdom 下读
//    navigator(en-US) ⇒ 默认英文；本文件断言的中文文案（「含挂机」「无回合概念」）只有 zh 有。
//  * 数据源用**同源夹具** `mockUsageDashboard`（Task 1）：页面就是这样把 `workSummary` /
//    `availability` 交给本组件的 —— 夹具改口径时本文件跟着改，不另存一份期望值。
//  * **缺 `availability` 条目 ≠ 不可得**（§3 第 9 条）：`sessions` / `toolAvgMs` / `topTool` /
//    `topToolMs` 四条运行时本就没有条目，空态只由**值本身为 `null`** 驱动（用例 3 与 10）。
//  * 聚合报错行的**逐源缺口**（用例 12/13）：真机 `availability_table()` 对**每个**指标都挂
//    7 源全量映射，`caps.rs` 里 `workbuddy.error_model` / `claude.error_turn` + `workbuddy.error_turn`
//    / `opencode.error_tool` / `kimi.interrupted` 均为 `false` ⇒ 四层报错行**都有**缺口（夹具已按
//    真值表补齐，只列 `false` 的源）。提示由 `perSource` 数据驱动，源用原始 id（`AGENT_BADGE`
//    无 `opencode`，走 `usageAgentLabel` 会印成 `?`）。
//  * 计数类一律 `fmtInt`（完整千分位）；p50 走 `fmtDur`、最长走 `fmtLongest`（spec P8）。
//  * `topTool` / `topToolMs` 的 `name` 是**工具调用名**（后端 `tool_stats` 的键，如 `Bash`；
//    `query.rs::work_summary_with`），不是采集源 id ⇒ 原样渲染，**不得**当工具 id 走
//    `usageAgentLabel`（那样会印成 `?`）。逐工具行的键（`turnsPerTool` / `longestTurnPerTool`）
//    才是工具 id，其显示名一律 `usageAgentLabel`。
import { render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
// ⚠️ import 顺序即 TDD 的两个红灯顺序（Task 8 步骤 3/4）：先撞未创建的纯层
// `@/lib/usage/availability`，纯层落盘后再撞组件 `@/components/usage/UsageWorkSummary`。
import {
  availabilityOf,
  isMetricAvailable,
  isToolAvailable,
  metricReason,
  toolReason,
} from "@/lib/usage/availability";
import { UsageWorkSummary } from "@/components/usage/UsageWorkSummary";
import { EM_DASH, fmtDur, fmtLongest } from "@/lib/usage/format";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";
import type { UsageAvailability, WorkSummary } from "@/types/usage";

/** i18next 的 `t` 直接作 `TFn` 传入（与 `usage-page.test.tsx` 同法） */
const tf = i18n.t.bind(i18n);

/** 页面传给组件的就是这两块（同源夹具；默认档 last7d + 按工具分组） */
const DASH = mockUsageDashboard({ preset: "last7d" }, "tool");

/** 「全字段 null」的工作小结（用例 10：空态绝不出现 0） */
function emptyWork(): WorkSummary {
  return {
    sessions: null,
    turnsPerTool: {},
    errorModel: null,
    errorTurn: null,
    errorTool: null,
    interrupted: null,
    toolCalls: null,
    toolAvgMs: null,
    topTool: null,
    topToolMs: null,
    longestTurnPerTool: {},
  };
}

/** 渲染工作小结区（页面口径：`work` = `dash.workSummary`、`availability` = `dash.availability`） */
function renderWork(work: WorkSummary, availability: UsageAvailability[]) {
  return render(<UsageWorkSummary work={work} availability={availability} t={tf} />);
}

// 语言固定 zh（**顶层** beforeAll：纯层用例里也有中文断言，见文件头纪律）
beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("usage-worksummary 纯层（availability 判据）", () => {
  it("1. 逐源不可得（perSource）≠ 指标级不可得；同指标其余源照常可用", () => {
    const turnAv: UsageAvailability = {
      metric: "turn",
      available: true,
      perSource: { workbuddy: false },
    };

    // 指标级仍「可用」：perSource 只否掉那一个源
    expect(isMetricAvailable(turnAv)).toBe(true);
    expect(isToolAvailable(turnAv, "workbuddy")).toBe(false);
    expect(isToolAvailable(turnAv, "claude")).toBe(true);

    // 夹具口径（Task 8 步骤 2 + 修复轮 2）：claude 无回合级失败字段属**逐源**不可得，不是整指标
    // 不可得；缺口照 `caps.rs` 真值表（claude.error_turn 与 workbuddy.error_turn 均为 false）
    expect(availabilityOf(DASH.availability, "errorTurn")).toEqual({
      metric: "errorTurn",
      available: true,
      perSource: { claude: false, workbuddy: false },
    });
    expect(isMetricAvailable(availabilityOf(DASH.availability, "errorTurn"))).toBe(true);
    expect(isToolAvailable(availabilityOf(DASH.availability, "errorTurn"), "claude")).toBe(false);
    expect(isToolAvailable(availabilityOf(DASH.availability, "errorTurn"), "workbuddy")).toBe(
      false
    );
    expect(isToolAvailable(availabilityOf(DASH.availability, "errorTurn"), "zcode")).toBe(true);
  });

  it("2. 指标级 available=false → 所有源都不可得，原因取「所有来源均不可得」", () => {
    const av: UsageAvailability = { metric: "errorModel", available: false, reason: "mock-empty" };

    expect(isMetricAvailable(av)).toBe(false);
    // 指标级不可得覆盖全部源，**不得**因为没有 perSource 条目就放行
    expect(isToolAvailable(av, "claude")).toBe(false);
    expect(isToolAvailable(av, "workbuddy")).toBe(false);

    expect(metricReason(tf, av)).toBe(tf("usage.work.unavailableAll"));
    expect(toolReason(tf, "claude", av)).toBe(tf("usage.work.unavailableAll"));
    expect(metricReason(tf, av)).not.toBe(tf("usage.work.unavailable"));
  });

  it("3. 缺 availability 条目按「可用」处理（sessions / toolAvgMs / topTool / topToolMs）", () => {
    // `availability_table()` 运行时只出 8 条，这四个字段没有条目 ⇒ 按「可用」处理（§3 第 9 条）
    for (const metric of ["sessions", "toolAvgMs", "topTool", "topToolMs"] as const) {
      expect(availabilityOf(DASH.availability, metric)).toBeNull();
      expect(isMetricAvailable(availabilityOf(DASH.availability, metric))).toBe(true);
    }

    // 判据本身对 `null` 也成立（组件在 `availability` 为空数组 / 缺参时走同一支）
    expect(isMetricAvailable(null)).toBe(true);
    expect(isToolAvailable(null, "claude")).toBe(true);
    expect(metricReason(tf, null)).toBe(tf("usage.work.unavailable"));
  });

  it("4. 逐源原因取该源回合口径（turnBasis.<toolId>）；与指标级原因分工不同", () => {
    const turnAv = availabilityOf(DASH.availability, "turn");
    // 两个**不同源**的逐源不可得（workbuddy 无回合概念 / claude 无回合级失败字段）各取**自己**的口径
    const errTurnAv = availabilityOf(DASH.availability, "errorTurn");

    expect(toolReason(tf, "workbuddy", turnAv)).toBe(tf("usage.turnBasis.workbuddy"));
    expect(toolReason(tf, "workbuddy", turnAv)).toContain("无回合概念");
    expect(toolReason(tf, "claude", errTurnAv)).toBe(tf("usage.turnBasis.claude"));
    expect(toolReason(tf, "claude", errTurnAv)).not.toBe(toolReason(tf, "workbuddy", turnAv));

    // 指标级仍可用 ⇒ 指标级原因绝不是「所有来源均不可得」
    expect(metricReason(tf, turnAv)).not.toBe(tf("usage.work.unavailableAll"));

    // 该源可用时不该调用本函数（空态由「值本身为 null」驱动）；真调到时给中性原因，
    // 既不是别的源的口径、也不是「所有来源均不可得」
    expect(toolReason(tf, "claude", turnAv)).toBe(tf("usage.work.unavailable"));
    expect(toolReason(tf, "claude", null)).toBe(tf("usage.work.unavailable"));
  });
});

describe("usage-worksummary 组件（工作小结区）", () => {
  it("5. 会话数：D17 分层后的单值 + hover 分层说明；段头常驻「计数已分层」", () => {
    const view = renderWork(DASH.workSummary, DASH.availability);

    expect(screen.getByTestId("usage-work-summary")).toBeTruthy();
    expect(screen.getByText(tf("usage.work.title"))).toBeTruthy();

    const sessions = screen.getByTestId("work-sessions");
    expect(sessions).toHaveTextContent("42"); // 夹具 = 分层后（只计父会话）的单值
    expect(sessions).toHaveAttribute("title", tf("usage.work.sessionsHint"));
    // 「分层后的**单值**」（D17 默认裁定）：行内只有一个数字，不做父子两行/可展开层
    expect(sessions.textContent?.match(/\d+/g)).toEqual(["42"]);

    expect(screen.getByTestId("work-subagent-layer")).toHaveTextContent(
      tf("usage.work.subagentLayer")
    );

    view.unmount();
  });

  it("6. turn 逐工具分列（无合计行）：workbuddy 无回合概念 → `—` + 该源口径 title", () => {
    const view = renderWork(DASH.workSummary, DASH.availability);

    // 每工具一行，行名走 usageAgentLabel（键是工具 id，不是展示名）
    expect(screen.getByTestId("work-turns-claude")).toHaveTextContent("Claude");
    expect(screen.getByTestId("work-turns-claude")).toHaveTextContent("64");
    expect(screen.getByTestId("work-turns-codex")).toHaveTextContent("Codex");
    expect(screen.getByTestId("work-turns-codex")).toHaveTextContent("3,189");
    expect(screen.getByTestId("work-turns-zcode")).toHaveTextContent("ZCode");
    expect(screen.getByTestId("work-turns-zcode")).toHaveTextContent("1,207");

    // 每工具 title = 该源回合口径（各源判据逐源可查，D16）
    expect(screen.getByTestId("work-turns-claude")).toHaveAttribute(
      "title",
      tf("usage.turnBasis.claude")
    );

    // perSource 不可得 → 空态（`—` + 原因），**不是 0**
    const wb = screen.getByTestId("work-turns-workbuddy");
    expect(wb).toHaveTextContent("WorkBuddy");
    expect(wb).toHaveTextContent(EM_DASH);
    expect(wb).toHaveAttribute("title", tf("usage.turnBasis.workbuddy"));
    expect(wb.textContent).not.toContain("0");

    // 恰 4 行、无合计行：64 + 3,189 + 1,207 = 4,460 在哪都不能出现
    expect(screen.queryByTestId("work-turns-total")).toBeNull();
    expect(view.container.textContent).not.toContain("4,460");
    expect(view.container.textContent).not.toContain("4460");

    view.unmount();
  });

  it("7. 报错三层 + 用户打断：四行分列、各有各的值，绝不合并成一个总数", () => {
    const view = renderWork(DASH.workSummary, DASH.availability);

    // 四行都在，且各自带行名（D18：三层「错误」互不包含，相加不表示任何含义）
    const model = screen.getByTestId("work-errorModel");
    const turn = screen.getByTestId("work-errorTurn");
    const tool = screen.getByTestId("work-errorTool");
    const interrupted = screen.getByTestId("work-interrupted");

    expect(model).toHaveTextContent(tf("usage.work.errorModel"));
    expect(model).toHaveTextContent("21");
    expect(turn).toHaveTextContent(tf("usage.work.errorTurn"));
    expect(turn).toHaveTextContent("8");
    expect(tool).toHaveTextContent(tf("usage.work.errorTool"));
    expect(tool).toHaveTextContent("343");
    expect(interrupted).toHaveTextContent(tf("usage.work.interrupted"));
    expect(interrupted).toHaveTextContent("344");

    // 四行行名两两不同（不是同一行复制四遍）
    expect(new Set([model, turn, tool, interrupted].map((el) => el.textContent)).size).toBe(4);

    // 绝不出现合计：21+8+343=372；21+8+343+344=716（打断不计入错误）
    expect(screen.queryByTestId("work-errors-total")).toBeNull();
    expect(view.container.textContent).not.toContain("372");
    expect(view.container.textContent).not.toContain("716");

    view.unmount();
  });

  it("8. 最长单 turn 逐工具 p50 + 最长；段头常驻「含挂机、未剔 idle」", () => {
    // 逐值核对（Task 8 步骤 7）：格式化口径取自 Task 2 的 format.ts，**不得**改断言
    expect(fmtDur(310_607)).toBe("5.2 分钟");
    expect(fmtLongest(50_311_958)).toBe("838.5 分钟");
    expect(fmtDur(1_864_000)).toBe("31.1 分钟");

    const view = renderWork(DASH.workSummary, DASH.availability);

    // 段头提示常驻（D19：七源里只有 opencode 有 idle 字段，无法统一剔除）
    expect(screen.getByTestId("work-longest-hint")).toHaveTextContent("含挂机");

    // zcode：p50 走 fmtDur、最长走 fmtLongest（两个数分列，不是一个数）
    const zcode = screen.getByTestId("work-longest-zcode");
    expect(zcode).toHaveTextContent("ZCode");
    expect(zcode).toHaveTextContent("5.2 分钟");
    expect(zcode).toHaveTextContent("838.5 分钟");
    expect(screen.getByTestId("work-longest-claude")).toHaveTextContent("24.4 秒");

    // workbuddy 无任何 duration 字段 → 空态 + 该源口径原因（不是 0 分钟）
    const wb = screen.getByTestId("work-longest-workbuddy");
    expect(wb).toHaveTextContent(EM_DASH);
    expect(wb).toHaveAttribute("title", tf("usage.turnBasis.workbuddy"));

    // 逐工具分列、不跨工具合计：不出现「总最长」行
    expect(screen.queryByTestId("work-longest-total")).toBeNull();

    view.unmount();
  });

  it("9. 工具调用 2×2（总数 / 平均耗时 / 次数第一 / 耗时第一）+ Top 两个第一名的列表", () => {
    const view = renderWork(DASH.workSummary, DASH.availability);

    expect(screen.getByTestId("work-tool-calls")).toHaveTextContent("41,412");
    expect(screen.getByTestId("work-tool-avg")).toHaveTextContent("79 毫秒"); // fmtDur(79)

    // Top 列表：V2 把「耗时第一」移入高级统计，主区保留「次数第一」
    const list = screen.getByTestId("work-top-list");
    expect(list.tagName).toBe("UL");
    expect(list.querySelectorAll("li")).toHaveLength(1);

    const byCount = screen.getByTestId("work-top-count");
    expect(byCount).toHaveTextContent(tf("usage.work.topToolCount"));
    expect(byCount).toHaveTextContent("Bash"); // 工具调用名原样渲染（不是采集源 id）
    expect(byCount).toHaveTextContent("12,004");

    // 高级统计 details 内：平均耗时 / 耗时第一 / p50+最长
    expect(screen.getByTestId("work-advanced")).toHaveTextContent(tf("usage.work.advanced"));
    const byMs = screen.getByTestId("work-top-ms");
    expect(byMs).toHaveTextContent(tf("usage.work.topToolMs"));
    expect(byMs).toHaveTextContent("31.1 分钟"); // fmtDur(1_864_000)

    view.unmount();
  });

  it("10. 全字段 null：逐指标空态一律 `—`，全程绝不出现 0（也不出合计）", () => {
    const view = renderWork(emptyWork(), []);

    // 缺 availability 条目不算空 ⇒ 这里的空态全部由「值本身为 null」驱动
    const sessions = screen.getByTestId("work-sessions");
    expect(sessions).toHaveTextContent(EM_DASH);
    expect(sessions).toHaveAttribute("title", tf("usage.work.unavailable"));

    // 整块无一行可显示 → 段内空态（不是一行 0、也不是整卡消失）
    expect(screen.getByTestId("work-turns-empty")).toBeTruthy();
    expect(screen.getByTestId("work-longest-empty")).toBeTruthy();

    for (const testid of [
      "work-errorModel",
      "work-errorTurn",
      "work-errorTool",
      "work-interrupted",
      "work-tool-calls",
      "work-tool-avg",
      "work-top-count",
      "work-top-ms",
    ]) {
      const cell = screen.getByTestId(testid);
      expect(cell).toHaveTextContent(EM_DASH);
      expect(cell).toHaveAttribute("title", tf("usage.work.unavailable"));
      expect(cell.textContent).not.toContain("0");
    }

    // 全卡零数字：不出现 0 / NaN / Infinity / undefined（填 0 在结构上不可能）。
    // 「p50」是高级统计的段名（字母 p + 数字 50），不是指标值，比较前先剔除。
    const text = (view.container.textContent ?? "").replace(/p50/g, "");
    expect(text).not.toMatch(/\d/);
    expect(text).not.toMatch(/NaN|Infinity|undefined|null/);

    view.unmount();
  });

  it("11. 指标级不可得：该行整行空态（`—` + 原因），其余指标照常出数", () => {
    const availability: UsageAvailability[] = DASH.availability.map((a) =>
      a.metric === "errorModel"
        ? { metric: "errorModel", available: false, reason: "mock-empty" }
        : a
    );
    const view = renderWork(DASH.workSummary, availability);

    // 值本身有（21）但指标级不可得 ⇒ 整行空态，原因取「所有来源均不可得」
    const row = screen.getByTestId("work-errorModel");
    expect(row).toHaveTextContent(EM_DASH);
    expect(row).not.toHaveTextContent("21");
    expect(row).toHaveAttribute("title", tf("usage.work.unavailableAll"));

    // 逐指标空态：同一份 availability 里其余条目不受影响（不是整卡空态）
    expect(screen.getByTestId("work-errorTurn")).toHaveTextContent("8");
    expect(screen.getByTestId("work-sessions")).toHaveTextContent("42");
    expect(screen.getByTestId("work-tool-calls")).toHaveTextContent("41,412");

    view.unmount();
  });

  it("12. 聚合报错四行逐行按 perSource 标注缺口：写明各自不含哪些源（源 id 原文）", () => {
    const view = renderWork(DASH.workSummary, DASH.availability);

    // Task 15 验收条目 B-5 + 真机 `caps.rs` 真值表：**四层报错行都有缺口**（`availability_table()`
    // 对每个指标都挂 7 源全量映射），逐行给各自被排除的源——否则「21 / 8 / 343 / 344」会被读成
    // 完整合计。数字与结构**零改动**。
    const rows: ReadonlyArray<readonly [string, string, string]> = [
      ["work-errorModel", "21", "workbuddy"],
      ["work-errorTurn", "8", "claude, workbuddy"],
      ["work-errorTool", "343", "opencode"],
      ["work-interrupted", "344", "kimi"],
    ];
    for (const [testid, value, sources] of rows) {
      const row = screen.getByTestId(testid);
      expect(row).toHaveTextContent(value);
      expect(row).toHaveAttribute("title", tf("usage.work.partialSources", { sources }));
      for (const sourceId of sources.split(", ")) {
        expect(row.getAttribute("title")).toContain(sourceId);
      }
      // 源用**原始 id**呈现（① 的 `UsageStatusSection.tsx:239` 先例）：`opencode` 不在
      // `AGENT_BADGE` 里，走 `usageAgentLabel` 会印成 `?`
      expect(row.getAttribute("title")).not.toContain("?");
    }

    view.unmount();
  });

  it("13. 机制：提示由 perSource 数据驱动——`available: true` 且无 perSource 的行不出提示", () => {
    // 真机的 `availability_table()` 给**每个**指标都挂 perSource，所以「无缺口」不能拿某一行当
    // 代表（夹具此前恰好缺 errorModel / interrupted 的 perSource 条目，那是夹具与真机发散，
    // 修复轮 2 已补齐）⇒ 用合成 av 直接锁机制：把两行改成「可用但没有任何 perSource」。
    const availability: UsageAvailability[] = DASH.availability.map((a) =>
      a.metric === "errorModel" || a.metric === "errorTool"
        ? { metric: a.metric, available: true }
        : a
    );
    const view = renderWork(DASH.workSummary, availability);

    // 无 perSource ⇒ 无缺口 ⇒ 不挂 title（防「一律加」），值照常出
    expect(screen.getByTestId("work-errorModel")).toHaveTextContent("21");
    expect(screen.getByTestId("work-errorModel")).not.toHaveAttribute("title");
    expect(screen.getByTestId("work-errorTool")).toHaveTextContent("343");
    expect(screen.getByTestId("work-errorTool")).not.toHaveAttribute("title");

    // 同一份 av 里仍有缺口的两行照常出提示 ⇒ 证明是「按数据出提示」，不是「一律加 / 一律不加」
    expect(screen.getByTestId("work-errorTurn").getAttribute("title")).toContain("claude");
    expect(screen.getByTestId("work-interrupted").getAttribute("title")).toContain("kimi");

    view.unmount();
  });
});
