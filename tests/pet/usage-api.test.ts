// Task 1（计划②）· 用量域 IPC 唯一入口 `src/lib/api/usage.ts` 的行为锁。
// 纪律（契约 §3 / 计划② §3）：
//  * 8 条命令名与入参名**逐字**对齐契约（JS 侧一律传 camelCase 键）；
//  * **只有 `usageCollect` 触发扫描**，其余命令只读本地账本；
//  * 两条导出命令返回**落盘绝对路径**，且路径必须带上请求的 `name`（写死文件名会让
//    文件名断言与分享图路径断言全红，见 Task 11/12）；
//  * 隐私白名单：CSV 只出静态文本与结构化摘要，不含任何会话正文类字段。
// 本文件**不自行 mock invoke**：`tests/setup.ts` 已把 `@tauri-apps/api/core` 的 invoke 接到
// `tests/msw/tauriMocks.ts`（① 的 mock case → ② 的同源夹具），故这里同时锁住整条链。
import { describe, expect, it } from "vitest";
import path from "node:path";
import { tauriInvokeMock } from "../msw/tauriMocks";
import {
  exportSaveBytes,
  exportSaveText,
  revealDir,
  usageCollect,
  usageCollectVisible,
  usageDashboard,
  usageExportCsv,
  usageGetSettings,
  usageRecords,
  usageSetSettings,
} from "@/lib/api/usage";
import type { UsageDashboard, UsageRange } from "@/types/usage";

const TODAY: UsageRange = { preset: "today" };
const DAY: UsageRange = { preset: "last7d" };

/** 记下的每次 IPC 调用 = (命令名, 入参键名升序)。setup.ts 把 invoke 接到了这个 vi.fn 上。 */
function invocations(): Array<[string, string[]]> {
  return tauriInvokeMock.mock.calls.map(([cmd, args]): [string, string[]] => [
    cmd,
    Object.keys((args ?? {}) as Record<string, unknown>).sort(),
  ]);
}

describe("用量域 IPC 入口（src/lib/api/usage.ts）", () => {
  it("1. 命令名与入参键名逐字对齐契约（含 F7 可见性采集；另锁既有的 reveal_dir）", async () => {
    await usageCollect(true);
    await usageCollectVisible();
    await usageDashboard(TODAY, "tool");
    await usageRecords(TODAY, "project", { toolIds: ["claude"] });
    await usageExportCsv(TODAY, "project", {});
    await usageGetSettings();
    await usageSetSettings({ detailRetentionDays: 30 });
    await exportSaveText("mam-usage-today.csv", "groupKey\n");
    await exportSaveBytes("mam-usage-today.png", "iVBORw0KGgo=");
    await revealDir("/Users/jarvis/Downloads");

    expect(invocations()).toEqual([
      ["usage_collect", ["force"]],
      ["usage_collect_visible", []],
      ["usage_dashboard", ["groupBy", "range"]],
      ["usage_records", ["filters", "groupBy", "range"]],
      ["usage_export_csv", ["filters", "groupBy", "range"]],
      ["usage_get_settings", []],
      ["usage_set_settings", ["patch"]],
      ["export_save_text", ["content", "name"]],
      ["export_save_bytes", ["base64", "name"]],
      ["reveal_dir", ["path"]],
    ]);
  });

  it("2. 导出落盘返回绝对路径，且路径以请求的 name 结尾", async () => {
    const csvPath = await exportSaveText("mam-usage-last7d.csv", "groupKey\nclaude\n");
    const pngPath = await exportSaveBytes("mam-usage-today.png", "iVBORw0KGgo=");
    for (const [p, name] of [
      [csvPath, "mam-usage-last7d.csv"],
      [pngPath, "mam-usage-today.png"],
    ] as const) {
      expect(typeof p).toBe("string");
      expect(path.isAbsolute(p)).toBe(true);
      expect(p.endsWith(name)).toBe(true);
    }
  });

  it("3. dashboard：hero = 请求输入 + 产出、命中率为 0-1 分数、compare/recentSession/availability 齐备", async () => {
    const d = await usageDashboard(TODAY, "tool");
    // hero 口径（说明书 P1 第 2 条）：totals.requestTotal + totalsBuckets.output
    expect(d.hero).toBe(d.totals.requestTotal + d.totalsBuckets.output);
    expect(d.hero).toBeGreaterThan(0);
    // cacheHitRate 是 0-1 分数（契约 §3 要点 3），不是百分数
    expect(d.totals.cacheHitRate).toBeGreaterThan(0);
    expect(d.totals.cacheHitRate).toBeLessThanOrEqual(1);
    // compare 非 null（夹具覆盖环比）：差值由前端算（计划② §3 第 17 条）
    expect(d.compare).not.toBeNull();
    const cmp = d.compare as NonNullable<UsageDashboard["compare"]>;
    expect(cmp.prevMetrics.requestTotal).toBeGreaterThan(0);
    expect(cmp.prevBuckets.cacheRead).toBeGreaterThan(0);
    // recentSession：5 字段齐备，title 是 `string | null` 且必须在场
    expect(d.recentSession).not.toBeNull();
    const recent = d.recentSession as NonNullable<UsageDashboard["recentSession"]>;
    expect(Object.keys(recent).sort()).toEqual([
      "buckets",
      "metrics",
      "sessionId",
      "sourceId",
      "title",
    ]);
    expect("title" in recent).toBe(true);
    // availability 含 turn 条目（运行时 8 条里的逐源口径，计划① §C）
    expect(d.availability.some((a) => a.metric === "turn")).toBe(true);
  });

  it("4. isSubagent：日档行一律显式 null，小时档真值反向对照", async () => {
    const day = await usageDashboard(DAY, "tool");
    expect(day.rows.length).toBeGreaterThan(0);
    for (const r of day.rows) {
      // 日档算不出（日聚合行不带 session_id）→ 显式 null，**不得**回退成 false
      expect("isSubagent" in r).toBe(true);
      expect(r.isSubagent).toBeNull();
    }
    const hour = await usageDashboard(TODAY, "tool");
    expect(hour.rows.length).toBe(day.rows.length);
    // 小时档是真值：既有 true（只由子代理会话贡献）也有 false（含父会话）
    expect(hour.rows.some((r) => r.isSubagent === true)).toBe(true);
    expect(hour.rows.some((r) => r.isSubagent === false)).toBe(true);
  });

  it("5. records：卡片随 groupBy 换键而卡内行维度不变，卡内行 isSubagent 两档恒 null", async () => {
    // 夹具工具池 = 4 工具（见 src/lib/usage/mockFixtures.ts）：Task 6 的 hero 期望
    // 2,036,981 = 1,954,268 + 82,713 正是这 4 个工具求和；Task 9 的 chips 注记也写
    // 「夹具里那 4 个工具」——Task 1 步骤 6 措辞里的「7 工具」应为 7 源之误（见报告）。
    const byTool = await usageRecords(TODAY, "tool", {});
    expect(byTool.cards.map((c) => c.toolId)).toEqual(["claude", "codex", "zcode", "workbuddy"]);
    const byProject = await usageRecords(TODAY, "project", {});
    expect(byProject.cards.map((c) => c.toolId)).toEqual([
      "multiagents-manager",
      "dsh-foxbell-pet",
      "deepseek-plugins",
      "prompt-lab",
    ]);
    // 卡片键随 groupBy 变，**卡内行维度（供应商 / 模型）不变**：同序行 label 逐字相同
    expect(byProject.cards[0].rows.map((r) => r.label)).toEqual(
      byTool.cards[0].rows.slice(0, 2).map((r) => r.label)
    );
    // 卡内行不带 session_id → isSubagent **两档都是 null**（与 dashboard 行的分档不同）
    for (const card of [...byTool.cards, ...byProject.cards]) {
      for (const row of card.rows) {
        expect("isSubagent" in row).toBe(true);
        expect(row.isSubagent).toBeNull();
      }
    }
    // filters 键名逐字对齐：toolIds 收窄卡片集合（不给 / 空数组 = 不筛）
    expect(
      (await usageRecords(TODAY, "tool", { toolIds: ["codex"] })).cards.map((c) => c.toolId)
    ).toEqual(["codex"]);
    expect((await usageRecords(TODAY, "tool", {})).cards.length).toBe(byTool.cards.length);
  });

  it("6. usage_export_csv 只出字符串：11 列表头 + 行尾换行 + 不含会话正文类字段", async () => {
    const csv = await usageExportCsv(TODAY, "tool", {});
    expect(typeof csv).toBe("string");
    const lines = csv.split("\n");
    expect(lines[0].split(",")).toEqual([
      "groupKey",
      "label",
      "inputFresh",
      "cacheRead",
      "cacheWrite",
      "output",
      "requestTotal",
      "cacheHitRate",
      "requests",
      "userEst",
      "sourceKind",
    ]);
    expect(csv.endsWith("\n")).toBe(true);
    // 每行一个分组键 + 表头 = 5 行（4 个工具卡片行）
    expect(lines.filter((l) => l !== "")).toHaveLength(5);
    // 隐私白名单（契约 §3 第 23 条）：表头不得有会话正文类列，正文里也不得出现会话标题 / 会话 id
    expect(lines[0]).not.toMatch(/session|title|prompt/i);
    expect(csv).not.toContain("sess-recent-1");
    expect(csv).not.toContain("演示会话");
  });

  it("7. usage_set_settings 返回合并后的完整 8 字段（patch 未提的字段保持原值）", async () => {
    const before = await usageGetSettings();
    const merged = await usageSetSettings({ detailRetentionDays: 30, exportPose: "fixed" });
    expect(Object.keys(merged).sort()).toEqual([
      "collectIntervalMin",
      "detailRetentionDays",
      "enabled",
      "exportPose",
      "exportQuote",
      "miniBarRange",
      "miniBarToolRows",
      "providerMapRules",
    ]);
    expect(merged.detailRetentionDays).toBe(30);
    expect(merged.exportPose).toBe("fixed");
    expect(merged.miniBarToolRows).toBe(before.miniBarToolRows);
    expect(merged.miniBarRange).toBe(before.miniBarRange);
    expect(merged.enabled).toBe(before.enabled);
  });
});
