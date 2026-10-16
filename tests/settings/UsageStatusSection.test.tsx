// tests/settings/UsageStatusSection.test.tsx — 计划① Task 24：设置页「用量采集状态」验收面。
// 覆盖：采集按钮触发 usage_collect({force:true}) 且期间禁用 / 逐源状态渲染（含失败源错误码文案）/
// 命中率一位小数 / 空态与错误态（可见可重试）/ 采集后自动刷新今日口径 /
// 总开关关闭的空态（dashboard 早退）与「不可得 ≠ 0」（userEst 可空、collectedAt=0）/
// 采集失败的错误码收敛（未知码 → usage.rpc.usage-internal）/
// i18n zh-en 无缺键（settings.usageStatus 子树，对齐 SignalHealthSection.test.tsx 扫描）。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

import i18n from "@/i18n";
import { UsageStatusSection } from "@/components/settings/UsageStatusSection";

void i18n; // tests/setup.ts 未初始化 i18n：显式引入并按默认英文断言（jsdom navigator.language=en）

const collectResult = () => ({
  collectedAt: 1_700_000_000_000,
  durationMs: 4321,
  totalNewRecords: 128,
  sources: [
    { sourceId: "claude", ok: true, parsedFiles: 17, newRecords: 42 },
    { sourceId: "codex", ok: true, parsedFiles: 425, newRecords: 61 },
    { sourceId: "kimi", ok: true, parsedFiles: 82, newRecords: 12 },
    { sourceId: "opencode", ok: true, parsedFiles: 21, newRecords: 8 },
    { sourceId: "workbuddy", ok: true, parsedFiles: 3, newRecords: 2 },
    {
      sourceId: "zcode",
      ok: false,
      parsedFiles: 0,
      newRecords: 0,
      errorCode: "usage-source-db-open",
    },
    { sourceId: "dsh", ok: true, parsedFiles: 99, newRecords: 3 },
  ],
});

const dashboard = (over?: Record<string, unknown>) => ({
  range: { preset: "today" },
  groupBy: "tool",
  rows: [],
  totals: { requestTotal: 1_000_000, cacheHitRate: 0.9269, userEst: 12345, requests: 321 },
  totalsBuckets: { inputFresh: 120_000, cacheRead: 880_000, cacheWrite: 0, output: 45_000 },
  hero: 1_045_000,
  trend: [],
  compare: null,
  recentSession: null,
  workSummary: {
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
  },
  availability: [
    // 真机 `empty_dashboard` 也带 `availability_table()`（恒 8 条；query.rs:335 / :1406）→
    // fixture 与 wire 同形（组件当前不消费该字段，但「桩要像真数据」是执行提示词 §4 的纪律）
    { metric: "turn", available: true, perSource: { dsh: false } },
    { metric: "errorModel", available: true, perSource: { workbuddy: false } },
    { metric: "errorTurn", available: true, perSource: { claude: false } },
    { metric: "errorTool", available: true, perSource: { opencode: false } },
    { metric: "interrupted", available: true, perSource: { kimi: false } },
    { metric: "longestTurn", available: true, perSource: {} },
    { metric: "toolCalls", available: true, perSource: {} },
    {
      metric: "userEst",
      available: true,
      perSource: { zcode: false, opencode: false, dsh: false },
    },
  ],
  collectedAt: 1_700_000_000_000,
  ...over,
});

const emptyDashboard = () =>
  dashboard({
    totals: { requestTotal: 0, cacheHitRate: 0, userEst: null, requests: 0 },
    totalsBuckets: { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 },
    collectedAt: 0,
  });

// 真机「总开关关闭」的线形状（`query.rs: empty_dashboard`，Task 24 前提必须与真实现一致）：
// 零行 / 零桶 / 零请求 / user_est=null / trend 空 / 无环比 / 无本会话 / collectedAt=0。
// 与「开着但今日无数据」的区别只在 collectedAt 与 trend，此处刻意取更严的关闭态。
const disabledDashboard = () =>
  dashboard({
    rows: [],
    totals: { requestTotal: 0, cacheHitRate: 0, userEst: null, requests: 0 },
    totalsBuckets: { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 },
    hero: 0,
    trend: [],
    compare: null,
    recentSession: null,
    workSummary: {
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
    },
    collectedAt: 0,
  });

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "usage_collect") return collectResult();
    if (cmd === "usage_dashboard") return dashboard();
    return null;
  });
});
afterEach(() => vi.clearAllMocks());

describe("UsageStatusSection 验收面（Task 24）", () => {
  it("挂载即拉今日口径；点「立即采集」触发 usage_collect({force:true})，期间按钮禁用", async () => {
    render(<UsageStatusSection />);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("usage_dashboard", {
        range: { preset: "today" },
        groupBy: "tool",
      })
    );
    // 采集在途：用未决 promise 卡住，断言按钮 disabled（loading 态可见）
    let release: (v: unknown) => void = () => {};
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_collect") return new Promise((r) => (release = r));
      if (cmd === "usage_dashboard") return dashboard();
      return null;
    });
    fireEvent.click(screen.getByTestId("usage-status-collect"));
    await waitFor(() => expect(screen.getByTestId("usage-status-collect")).toBeDisabled());
    // 采集中钩子必须可见（数据侧唯一抓手），且文案是「采集中…」
    expect(screen.getByTestId("usage-status-collecting").textContent).toContain("Collecting");
    release(collectResult());
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("usage_collect", { force: true }));
    // 采集成功后自动刷新今日口径（dashboard 至少第二次）
    await waitFor(() =>
      expect(
        invokeMock.mock.calls.filter((c) => c[0] === "usage_dashboard").length
      ).toBeGreaterThanOrEqual(2)
    );
    // 采集结束后按钮恢复可点、采集中钩子消失
    await waitFor(() => expect(screen.getByTestId("usage-status-collect")).not.toBeDisabled());
    expect(screen.queryByTestId("usage-status-collecting")).toBeNull();
  });

  it("逐源渲染：7 行，失败源标红并显示错误码文案（走 usage.rpc.<code>）", async () => {
    render(<UsageStatusSection />);
    fireEvent.click(screen.getByTestId("usage-status-collect"));
    await waitFor(() =>
      expect(document.querySelectorAll("[data-testid='usage-source-row']").length).toBe(7)
    );
    // 成功态徽标 6 个（`getByText` 在多命中时会抛错 → 用 getAllByText 精确计数）
    expect(screen.getAllByText("OK")).toHaveLength(6);
    expect(screen.getAllByText("Failed")).toHaveLength(1); // 失败态徽标
    // 失败源的错误码经 usage.rpc.usage-source-db-open 翻译（en 文案）
    expect(screen.getByText(/Cannot open usage database read-only/)).toBeTruthy();
    // 汇总：新增记录 128（千分位无关，此处 3 位数）+ 耗时
    expect(screen.getByText(/128/)).toBeTruthy();
  });

  it("今日口径：四桶等宽对齐、命中率一位小数、请求次数可见", async () => {
    render(<UsageStatusSection />);
    const buckets = await screen.findByTestId("usage-today-buckets");
    expect(buckets.textContent).toContain("120,000");
    expect(buckets.textContent).toContain("880,000");
    expect(buckets.textContent).toContain("45,000");
    // 0.9269 × 100 = 92.7%（一位小数，四舍五入）
    expect(screen.getByText("92.7%")).toBeTruthy();
    expect(screen.getByText(/321/)).toBeTruthy();
  });

  it("空态：今日无数据（四桶全 0 且 requests=0）→ 空态文案，不显示 0 值表格", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "usage_dashboard" ? emptyDashboard() : null
    );
    render(<UsageStatusSection />);
    expect(await screen.findByTestId("usage-status-empty")).toBeTruthy();
    expect(screen.queryByTestId("usage-today-buckets")).toBeNull();
  });

  it("错误态：dashboard 失败 → 可见错误 + 重试按钮（不得静默回落成空态）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") throw { code: "usage-db-failed", detail: "locked" };
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      expect(await screen.findByTestId("usage-status-error")).toBeTruthy();
      expect(screen.queryByTestId("usage-status-empty")).toBeNull();
      // 重试：改回成功实现后点重试，错误态消失
      invokeMock.mockImplementation(async (cmd: string) =>
        cmd === "usage_dashboard" ? dashboard() : null
      );
      fireEvent.click(screen.getByRole("button", { name: /retry/i }));
      await waitFor(() => expect(screen.queryByTestId("usage-status-error")).toBeNull());
    } finally {
      errSpy.mockRestore();
    }
  });
});

// 总开关关闭（dashboard 早退空态）与 GC 7「不可得 ≠ 0」——本验收面唯一的两种假数字来源。
describe("UsageStatusSection 总开关关闭 / 不可得（Task 24）", () => {
  it("总开关关闭 → 空态、不显示 0 值表格，且打开设置页本身不触发采集", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "usage_dashboard" ? disabledDashboard() : null
    );
    render(<UsageStatusSection />);
    expect(await screen.findByTestId("usage-status-empty")).toBeTruthy();
    expect(screen.queryByTestId("usage-today-buckets")).toBeNull();
    // 文案纪律：CSV 导出**不受**总开关门控（已登记待裁项 B-2）→ 不得写成「关闭后一切都停」
    const text = screen.getByTestId("usage-status-empty").textContent ?? "";
    expect(text).toMatch(/No usage data for today/);
    expect(text).not.toMatch(/everything stops|all stopped|halted/i);
    // 打开设置页不扫描：只有点「立即采集」才调 usage_collect
    expect(invokeMock.mock.calls.filter((c) => c[0] === "usage_collect")).toHaveLength(0);
  });

  it("不可得显示空态而非 0：userEst=null → 「—」且该行不出现任何数字；collectedAt=0 → 尚未采集", async () => {
    // 前提：今日**有**真实字节（四桶/请求非 0）但 userEst 不可得（zcode/opencode/dsh 的口径）
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "usage_dashboard"
        ? dashboard({
            totals: { requestTotal: 1_000, cacheHitRate: 0.5, userEst: null, requests: 3 },
            collectedAt: 0,
          })
        : null
    );
    render(<UsageStatusSection />);
    const row = await screen.findByTestId("usage-today-user-est");
    expect(row.textContent).toContain("—");
    // 绝不回退成 0：该行不得出现任何数字
    expect(row.textContent ?? "").not.toMatch(/\d/);
    // collectedAt=0（本次运行还没采集过 / 总开关关闭）→ 明说「本次运行尚未采集」并指路，
    // 不渲染 1970 的假时间；也不得谎称「本档没有数据」
    expect(screen.getByText("Not collected in this run (click Collect now)")).toBeTruthy();
    // 四桶仍在（有数据就不该整体退化成空态）
    expect(screen.getByTestId("usage-today-buckets")).toBeTruthy();
  });

  // W-08 的残路径：分母为 0 时 0.0% 与「真的 0% 命中」不可区分 → 命中率位写「—」
  it("requests=0 但四桶非 0 → 命中率显示「—」而不是 0.0%（W-08）", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "usage_dashboard"
        ? dashboard({
            totals: { requestTotal: 0, cacheHitRate: 0, userEst: null, requests: 0 },
            // 四桶非 0 → 不落空态分支（前提断言，防「整体空态」假绿）
            totalsBuckets: { inputFresh: 5, cacheRead: 0, cacheWrite: 0, output: 7 },
          })
        : null
    );
    render(<UsageStatusSection />);
    // 前提：四桶真的渲染出来了（否则下面的「命中率 —」可能只是因为整体空态 → 假绿）
    expect(await screen.findByTestId("usage-today-buckets")).toBeTruthy();
    const hit = screen.getByTestId("usage-today-hit-rate");
    expect(hit.textContent).toBe("—");
    expect(document.body.textContent ?? "").not.toContain("0.0%");
  });

  // M4：上游漏序列化 `userEst` 键（`undefined`）时不得抛（否则整段分区白屏）
  it("userEst 键缺失（undefined）→ 仍显示「—」且分区不崩", async () => {
    invokeMock.mockImplementation(async (cmd: string) =>
      cmd === "usage_dashboard"
        ? dashboard({
            totals: { requestTotal: 1, cacheHitRate: 0.5, userEst: undefined, requests: 1 },
          })
        : null
    );
    render(<UsageStatusSection />);
    const row = await screen.findByTestId("usage-today-user-est");
    expect(row.textContent).toContain("—");
    expect(screen.getByTestId("usage-today-buckets")).toBeTruthy();
  });

  it("userEst 有值时照常显示（防「恒显示 —」的退化解）", async () => {
    render(<UsageStatusSection />);
    const row = await screen.findByTestId("usage-today-user-est");
    expect(row.textContent).toContain("12,345");
  });

  it("采集返回零源（总开关关闭时不扫描任何源）→ 逐源区给显式空态，且截止时间写「—」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_collect")
        return {
          collectedAt: 1_700_000_000_000,
          durationMs: 0,
          sources: [],
          totalNewRecords: 0,
        };
      if (cmd === "usage_dashboard") return disabledDashboard();
      return null;
    });
    render(<UsageStatusSection />);
    fireEvent.click(screen.getByTestId("usage-status-collect"));
    expect(await screen.findByTestId("usage-status-no-sources")).toBeTruthy();
    expect(document.querySelectorAll("[data-testid='usage-source-row']").length).toBe(0);
    // M1：零源轮的 `collectedAt` 是「now」而不是读到的数据代际 → 不得显示成真时间
    const summary = screen.getByTestId("usage-status-summary");
    expect(summary.textContent).toContain("Data as of：—");
    expect(summary.textContent ?? "").not.toContain("2023");
  });

  // I1：关闭态与「今天没跑」在数字上同形 → 必须读 `usage_get_settings` 才能区分
  it("总开关关闭（usage_get_settings.enabled=false）→ 显式关闭提示 + 关闭态空态文案，不显示 0 值表格", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return disabledDashboard();
      if (cmd === "usage_get_settings") return { enabled: false };
      return null;
    });
    render(<UsageStatusSection />);
    const notice = await screen.findByTestId("usage-status-disabled");
    expect(notice.textContent).toContain("usage statistics switch is off");
    // 空态文案**不得**再说「可能今天没跑」（那是把「被关掉」伪装成「没数据」）
    const empty = screen.getByTestId("usage-status-empty");
    expect(empty.textContent).toContain("switch is off");
    expect(empty.textContent ?? "").not.toMatch(/no tool ran/);
    expect(screen.queryByTestId("usage-today-buckets")).toBeNull();
  });

  // **C-3（用户裁决：保持现状 + 补一条前端提示）**：CSV 不受总开关门控**不改代码**，
  // 但关闭态下必须说清「导出的是**已采集的历史账本**」——否则用户会以为导出的是「关闭后的
  // 空账本」，或以为要先打开开关才能导出。导出按钮**保留**：关掉采集后仍要能备份/导出自己的
  // 历史账本（那才是真正的数据可用性损失），而导出的隐私风险不因开关变化（都要用户主动点）。
  it("关闭态下导出按钮**保留**且注明「导出的是已采集的历史账本」（C-3）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return disabledDashboard();
      if (cmd === "usage_get_settings") return { enabled: false };
      return null;
    });
    render(<UsageStatusSection />);
    await screen.findByTestId("usage-status-disabled");
    // ① 导出按钮仍在、仍可点（**不得**因总开关关闭而隐藏或禁用）
    const btn = screen.getByTestId("usage-status-export") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    // ② 必须注明导出的是**已采集的历史账本**（两句都要在：历史账本 + 关闭期间不再采集新数据）
    const hint = screen.getByTestId("usage-status-export-disabled-hint");
    expect(hint.textContent ?? "").toMatch(/already-collected historical ledger/i);
    expect(hint.textContent ?? "").toMatch(/nothing new is collected/i);
  });

  it("打开态**不**显示关闭态导出提示（不得误报「已关闭」）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return dashboard();
      if (cmd === "usage_get_settings") return { enabled: true };
      return null;
    });
    render(<UsageStatusSection />);
    await screen.findByTestId("usage-status-export");
    expect(screen.queryByTestId("usage-status-export-disabled-hint")).toBeNull();
  });

  it("总开关打开（enabled=true）+ 真的没数据 → 不显示关闭提示，空态说「今天没跑」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return emptyDashboard();
      if (cmd === "usage_get_settings") return { enabled: true };
      return null;
    });
    render(<UsageStatusSection />);
    expect(await screen.findByTestId("usage-status-empty")).toBeTruthy();
    expect(screen.queryByTestId("usage-status-disabled")).toBeNull();
    expect(screen.getByTestId("usage-status-empty").textContent).toMatch(/no tool ran/);
  });

  it("读总开关失败（reject）→ 不误报「已关闭」（宁可不提示，也不谎称用户关掉了）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return emptyDashboard();
      if (cmd === "usage_get_settings") throw { code: "usage-db-failed", detail: "locked" };
      return null;
    });
    render(<UsageStatusSection />);
    expect(await screen.findByTestId("usage-status-empty")).toBeTruthy();
    expect(screen.queryByTestId("usage-status-disabled")).toBeNull();
  });
});

// 采集失败（命令级 reject）与错误码收敛：未知码不得把原始错误串渲染成用户文案。
describe("UsageStatusSection 采集失败与错误码收敛（Task 24）", () => {
  it("采集失败 → usage-status-collect-error 可见、走 usage.rpc 文案并透出 detail；按钮恢复可点、可重试", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_collect")
        throw { code: "usage-source-io", detail: "permission denied: /x/y.jsonl" };
      if (cmd === "usage_dashboard") return dashboard();
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      fireEvent.click(screen.getByTestId("usage-status-collect"));
      const box = await screen.findByTestId("usage-status-collect-error");
      expect(box.textContent).toContain("Failed to read usage source file");
      // detail 通道（W-28：命令级失败带 detail；逐源失败**没有** detail 字段，只能给泛码）
      expect(box.textContent).toContain("permission denied: /x/y.jsonl");
      await waitFor(() => expect(screen.getByTestId("usage-status-collect")).not.toBeDisabled());
      expect(screen.queryByTestId("usage-status-collecting")).toBeNull();
      // 可重试：下一次采集成功后错误态必须消失、逐源结果出现
      invokeMock.mockImplementation(async (cmd: string) => {
        if (cmd === "usage_collect") return collectResult();
        if (cmd === "usage_dashboard") return dashboard();
        return null;
      });
      fireEvent.click(screen.getByTestId("usage-status-collect"));
      await waitFor(() =>
        expect(document.querySelectorAll("[data-testid='usage-source-row']").length).toBe(7)
      );
      expect(screen.queryByTestId("usage-status-collect-error")).toBeNull();
    } finally {
      errSpy.mockRestore();
    }
  });

  it("未知错误码收敛成 usage.rpc.usage-internal（KNOWN_USAGE_CODES 白名单，不渲染原始码）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_collect") throw { code: "totally-unknown-code", detail: "boom-xyz" };
      if (cmd === "usage_dashboard") return dashboard();
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      fireEvent.click(screen.getByTestId("usage-status-collect"));
      const box = await screen.findByTestId("usage-status-collect-error");
      expect(box.textContent).toContain("Usage operation failed");
      expect(box.textContent).toContain("boom-xyz");
      expect(box.textContent).not.toContain("totally-unknown-code");
    } finally {
      errSpy.mockRestore();
    }
  });

  it("dashboard 失败的错误文案同样过错误码表（未知码收敛 internal）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") throw { code: "nope-not-a-code", detail: "db gone" };
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      const box = await screen.findByTestId("usage-status-error");
      expect(box.textContent).toContain("Usage operation failed");
      expect(box.textContent).not.toContain("nope-not-a-code");
    } finally {
      errSpy.mockRestore();
    }
  });
});

// 导出与定位（`ensure_reveal_allowed` 白名单的唯一手工验收点）：前端三步必须同序、
// 路径必须用后端返回值；失败时不得去开一个不存在的路径。
describe("UsageStatusSection 导出与打开所在目录（Task 24）", () => {
  const CSV_TEXT =
    "groupKey,label,inputFresh,cacheRead,cacheWrite,output,requestTotal,cacheHitRate,requests,userEst,sourceKind\n";

  it("点「导出并打开所在目录」→ usage_export_csv → export_save_text → reveal_dir（路径取自落盘返回值）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return dashboard();
      if (cmd === "usage_export_csv") return CSV_TEXT;
      if (cmd === "export_save_text") return "/tmp/exports/mam-usage-today-123.csv";
      return null;
    });
    render(<UsageStatusSection />);
    fireEvent.click(screen.getByTestId("usage-status-export"));
    // 契约 §3 的入参逐项（range/groupBy/filters）
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("usage_export_csv", {
        range: { preset: "today" },
        groupBy: "tool",
        filters: {},
      })
    );
    // 落盘文件名过 Rust 白名单 `[A-Za-z0-9._-]` 且以 .csv 结尾（只有 .csv 前置 BOM）
    const save = invokeMock.mock.calls.find((c) => c[0] === "export_save_text");
    expect((save?.[1] as { content: string }).content).toBe(CSV_TEXT);
    expect((save?.[1] as { name: string }).name).toMatch(/^[A-Za-z0-9._-]+\.csv$/);
    // 第三步用的必须是**落盘返回值**里的路径
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("reveal_dir", {
        path: "/tmp/exports/mam-usage-today-123.csv",
      })
    );
    expect(screen.getByTestId("usage-status-export-path").textContent).toBe(
      "/tmp/exports/mam-usage-today-123.csv"
    );
  });

  it("落盘失败 → 可见错误 + 按钮恢复可点，且**不**调 reveal_dir（没有可信路径可开）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return dashboard();
      if (cmd === "usage_export_csv") return CSV_TEXT;
      // Task 21 已登记的真实形状：落盘失败是 `Err(String)` 静态文案，不带错误码
      if (cmd === "export_save_text") throw "导出目录不可写";
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      fireEvent.click(screen.getByTestId("usage-status-export"));
      expect(await screen.findByTestId("usage-status-export-error")).toBeTruthy();
      expect(invokeMock.mock.calls.filter((c) => c[0] === "reveal_dir")).toHaveLength(0);
      expect(screen.queryByTestId("usage-status-export-path")).toBeNull();
      await waitFor(() => expect(screen.getByTestId("usage-status-export")).not.toBeDisabled());
    } finally {
      errSpy.mockRestore();
    }
  });

  // I2：`export.rs` 明文「定位失败只 warn——文件已落盘，定位失败不该让导出算失败」。
  // 开发机设了 `TUVIS_HOME` 时白名单必然拒绝（`exports_dir()` 认 `TUVIS_HOME`，白名单只认 home/.tuvis）
  // → 这一支在验收环境里**会真的发生**，误报「导出失败」会直接把用户带偏。
  it("落盘成功 + reveal 失败 → **不**报导出失败，路径仍显示，只提示未能自动打开目录", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_dashboard") return dashboard();
      if (cmd === "usage_export_csv") return CSV_TEXT;
      if (cmd === "export_save_text") return "/tmp/exports/mam-usage-today-123.csv";
      if (cmd === "reveal_dir") throw "路径不存在: /tmp/exports/mam-usage-today-123.csv";
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(<UsageStatusSection />);
      fireEvent.click(screen.getByTestId("usage-status-export"));
      const reveal = await screen.findByTestId("usage-status-reveal-error");
      expect(reveal.textContent).toContain("/tmp/exports/mam-usage-today-123.csv");
      expect(reveal.textContent).toContain("could not open its folder");
      // 关键：导出**不算失败**（路径照常显示、无「导出失败」文案）
      expect(screen.queryByTestId("usage-status-export-error")).toBeNull();
      expect(screen.getByTestId("usage-status-export-path").textContent).toBe(
        "/tmp/exports/mam-usage-today-123.csv"
      );
      await waitFor(() => expect(screen.getByTestId("usage-status-export")).not.toBeDisabled());
    } finally {
      errSpy.mockRestore();
    }
  });
});

// 接线锁（Task 24 的 4 处改动：联合类型 / import / menuItems / 渲染分支）：
// 渲染整个 SettingsPage 需要一堆窗口与主题 mock，而这里的失败形态是纯机械的
// 「有人删了一处接线」→ 用静态源码扫描锁住，比渲染断言更稳（Task 20 的静态锁先例）。
//
// ⚠️ **只锁正向**（I3 裁决）：这里**不得**再写 `not.toContain('id: "usage"')` /
// `not.toContain("BarChart3")` / `not.toContain("UsageSection")` 之类**负向**断言——
// 契约 §5 与计划② 都要求 ② 在同一 `settings.tsx` 里加这三样（② 计划 L8707/L8713），
// 负向锁会让 ② 落地时**必然假红**，逼它在「不改别人的测试」与「套件必须绿」之间二选一。
// ⇒ **①→② delta 清单已登记**：② 落地时可同步改本用例（或直接删掉负向残留）。
describe("UsageStatusSection 设置页接线（Task 24）", () => {
  const src = readFileSync(path.join(process.cwd(), "src/pages/settings.tsx"), "utf8");

  it("四处接线齐备（联合类型 / import / 块表 / 渲染守卫）", () => {
    expect(src).toContain('| "usageStatus";');
    expect(src).toContain(
      'import { UsageStatusSection } from "@/components/settings/UsageStatusSection";'
    );
    // ⚠️ **2026-10-06 两级导航重构**：侧栏不再有「每个分区一个菜单项」这一层，`menuItems` 被
    // `SETTINGS_BLOCKS`（6 大块，每块带 `sections` 列表）取代 ⇒ 原先锁的两条**已随结构消失**，
    // 不是接线被删：`id: "usageStatus" as SettingSection`（分区不再有菜单项）与
    // `/^\s+Activity,$/m`（分区不再各自有图标，导航只有 6 个块图标）。
    // 取而代之锁**同一件事的两个新形态**：它必须挂在某个块的 `sections` 里（漏挂 ⇒ 该分区在
    // 界面上**永远看不见**，这是重构最真实的回归面），且渲染守卫必须在位。
    expect(src).toMatch(/sections: \["usage", "usageStatus"\]/);
    expect(src).toMatch(/\{visibleIds\.has\("usageStatus"\) && <UsageStatusSection \/>\}/);
  });
});

// i18n 契约：组件引用的 settings.usageStatus.* 键必须在 zh 与 en 同齐备
describe("UsageStatusSection i18n zh/en 无缺键（Task 24）", () => {
  const root = process.cwd();
  const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
    Object.entries(obj).flatMap(([k, v]) =>
      typeof v === "object" && v !== null
        ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
        : [`${prefix}${k}`]
    );

  it("源码引用键 zh/en 双语齐备，且两 locale 的 settings.usageStatus 键集相等", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const zhKeys = new Set(flat(zh.settings.usageStatus).map((k) => `settings.usageStatus.${k}`));
    const enKeys = new Set(flat(en.settings.usageStatus).map((k) => `settings.usageStatus.${k}`));
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);
    const text = readFileSync(
      path.join(root, "src/components/settings/UsageStatusSection.tsx"),
      "utf8"
    );
    const used = new Set<string>();
    for (const m of text.matchAll(/settings\.usageStatus\.([A-Za-z0-9_]+)/g)) used.add(m[0]);
    expect(used.size).toBeGreaterThanOrEqual(12);
    expect([...used].filter((k) => !zhKeys.has(k))).toEqual([]);
    expect([...used].filter((k) => !enKeys.has(k))).toEqual([]);
  });
});
