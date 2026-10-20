// Task 18 · 独立费用 CSV：只导当前时间窗全部费用行，不接受旧筛选/旧分组，序列固定。
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import i18n from "@/i18n";
import { UsageExportActions } from "@/components/usage/UsageExportActions";
import { mockUsageCostCsv, mockUsageDashboard } from "@/lib/usage/mockFixtures";
import type { TFn } from "@/lib/usage/range";
import type { UsageDashboard } from "@/types/usage";
import { tauriInvokeMock } from "../msw/tauriMocks";

// 姿态键来自 sheet；本文件不打开导出设置弹层，给形状对的替身即可
vi.mock("@/lib/usage/sheet", () => ({
  activePetSheet: () => Promise.resolve(null),
  loadSheet: () => Promise.resolve(null),
  poseKeysFor: (n: number) => Array.from({ length: n }, (_, i) => `pose-${i}`),
}));

const tf = i18n.t.bind(i18n) as unknown as TFn;

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

let defaultImpl: ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  tauriInvokeMock.mockClear();
  defaultImpl = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
});

afterEach(() => {
  cleanup();
  if (defaultImpl) tauriInvokeMock.mockImplementation(defaultImpl);
});

const SAVED_PATH = "/Users/jarvis/Downloads/tuvis-api-equivalent.csv";

// V2 增量（Task 10）：既有 17 列保持不变，渠道/价格来源/覆盖列追加在后。
const LEGACY_COST_COLUMNS = [
  "billing_profile_id",
  "billing_profile_name",
  "source_ids",
  "observed_provider",
  "model_id",
  "attribution_kind",
  "token_usage",
  "estimated_usd",
  "status",
  "priced_tokens",
  "unpriced_tokens",
  "coverage_rate",
  "pricing_provider",
  "pricing_model",
  "pricing_snapshot_id",
  "pricing_basis",
  "price_updated_at",
];
const APPENDED_COST_COLUMNS = [
  "channel_key",
  "channel_kind",
  "display_provider",
  "normalized_model_id",
  "pricing_release_date",
  "price_override",
];
const COST_COLUMNS = [...LEGACY_COST_COLUMNS, ...APPENDED_COST_COLUMNS];

/** 数据行每个单元格都由后端 `csv_cell` 用双引号包裹；按 CSV 转义规则拆回来。 */
function parseCsvLine(line: string): string[] {
  return Array.from(line.matchAll(/"((?:[^"]|"")*)"/g), (match) => match[1].replace(/""/g, '"'));
}

function installExport(opts: {
  content?: string;
  saveError?: string;
  revealError?: string;
  path?: string;
}) {
  const content = opts.content ?? "billing_profile_id,model_id\np1,gpt-5\n";
  tauriInvokeMock.mockImplementation((cmd: string, args?: unknown) => {
    if (cmd === "usage_cost_export_csv") return Promise.resolve(content);
    if (cmd === "export_save_text") {
      return opts.saveError
        ? Promise.reject(opts.saveError)
        : Promise.resolve(opts.path ?? SAVED_PATH);
    }
    if (cmd === "reveal_dir") {
      return opts.revealError ? Promise.reject(opts.revealError) : Promise.resolve();
    }
    return defaultImpl?.(cmd, args) ?? Promise.resolve();
  });
  return content;
}

function props(over: Partial<Parameters<typeof UsageExportActions>[0]> = {}) {
  return {
    dash: mockUsageDashboard({ preset: "last7d" }, "tool") as UsageDashboard | null,
    range: { preset: "last7d" as const },
    tab: "board" as const,
    filters: { toolIds: ["codex"], subagentMode: "parentsOnly" as const },
    groupBy: "model" as const,
    t: tf,
    ...over,
  };
}

describe("费用 CSV 导出（Task 18）", () => {
  it("1. 只传完整 range，不传旧筛选/旧分组；不调用旧 usageExportCsv；文件名与 BOM 原样", async () => {
    const content = installExport({ content: "billing_profile_id,model_id\np1,gpt-5\n" });
    render(<UsageExportActions {...props()} />);

    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    await waitFor(() => expect(callsOf("usage_cost_export_csv")).toHaveLength(1));

    // 只传 range：B 的局部筛选与 groupBy 都不得进入费用 CSV
    expect(callsOf("usage_cost_export_csv")[0][1]).toEqual({
      range: { preset: "last7d" },
    });
    // 不混用旧 Token CSV
    expect(callsOf("usage_export_csv")).toHaveLength(0);
    // 旧导出按钮仍在、但不是这条链
    expect(screen.getByTestId("usage-export-csv")).toBeTruthy();

    const save = callsOf("export_save_text")[0][1] as { name: string; content: string };
    expect(save.name).toMatch(/^tuvis-api-equivalent-last7d-\d{8}-\d{6}\.csv$/);
    expect(save.name.endsWith(".csv")).toBe(true);
    // 前端不加第二个 BOM：内容原样交给落盘层
    expect(save.content).toBe(content);
    expect(save.content.startsWith("﻿")).toBe(false);

    await waitFor(() => expect(callsOf("reveal_dir")).toHaveLength(1));
    expect(callsOf("reveal_dir")[0][1]).toEqual({ path: SAVED_PATH });
    expect(await screen.findByTestId("usage-export-toast")).toHaveTextContent(
      `已保存：${SAVED_PATH}`
    );
  });

  it("2. 记录页签同样可用；时间窗为自定义时文件名带起止日期", async () => {
    installExport({ path: "/tmp/custom.csv" });
    render(
      <UsageExportActions
        {...props({
          tab: "records",
          range: { preset: "custom", from: "2026-09-01", to: "2026-10-03" },
        })}
      />
    );

    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    await waitFor(() => expect(callsOf("usage_cost_export_csv")).toHaveLength(1));
    expect(callsOf("usage_cost_export_csv")[0][1]).toEqual({
      range: { preset: "custom", from: "2026-09-01", to: "2026-10-03" },
    });
    const save = callsOf("export_save_text")[0][1] as { name: string };
    expect(save.name).toMatch(/^tuvis-api-equivalent-2026-09-01_2026-10-03-\d{8}-\d{6}\.csv$/);
  });

  it("3. 保存失败：如实报错，不伪报成功、不调用 reveal", async () => {
    installExport({ saveError: "disk full" });
    render(<UsageExportActions {...props()} />);

    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    const toast = await screen.findByTestId("usage-export-toast");
    expect(toast).toHaveTextContent("导出失败：disk full");
    expect(toast).not.toHaveTextContent("已保存");
    expect(callsOf("reveal_dir")).toHaveLength(0);
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
  });

  it("4. 保存成功但定位失败：仍显示绝对路径，不提示重新导出", async () => {
    installExport({ revealError: "reveal denied", path: "/tmp/saved.csv" });
    render(<UsageExportActions {...props()} />);

    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    const toast = await screen.findByTestId("usage-export-toast");
    await waitFor(() => expect(callsOf("reveal_dir")).toHaveLength(1));
    expect(toast).toHaveTextContent("/tmp/saved.csv");
    expect(toast).toHaveTextContent("定位失败");
    expect(toast.textContent).not.toContain("导出失败");
    // 文件已经落盘：定位入口仍在
    expect(screen.getByTestId("usage-export-open-dir")).toBeTruthy();
  });

  it("5. 费用 CSV 追加 6 列：无 BOM、无总计行、缺金额为空、公式中和原样", async () => {
    const range = { preset: "last7d" as const };
    const content = installExport({ content: mockUsageCostCsv(range) });
    render(<UsageExportActions {...props({ range })} />);

    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    await waitFor(() => expect(callsOf("usage_cost_export_csv")).toHaveLength(1));
    expect(callsOf("usage_cost_export_csv")[0][1]).toEqual({ range });
    expect(callsOf("usage_export_csv")).toHaveLength(0);

    // 前端不加 BOM，内容逐字交给落盘层
    const save = callsOf("export_save_text")[0][1] as { content: string };
    expect(save.content).toBe(content);
    expect(save.content.startsWith("\uFEFF")).toBe(false);

    const lines = content.replace(/\n$/, "").split("\n");
    const header = lines[0].split(",");
    // 既有列序不变；新列只追加在末尾
    expect(header.slice(0, LEGACY_COST_COLUMNS.length)).toEqual(LEGACY_COST_COLUMNS);
    expect(header.slice(LEGACY_COST_COLUMNS.length)).toEqual(APPENDED_COST_COLUMNS);
    expect(header).toEqual(COST_COLUMNS);

    const rows = lines.slice(1).map(parseCsvLine);
    expect(rows.length).toBeGreaterThan(0);
    // 没有全局合计/小计行：每一行都是完整费用行（状态列合法、列数固定）
    for (const row of rows) {
      expect(row).toHaveLength(COST_COLUMNS.length);
      expect(["priced", "partial", "unpriced"]).toContain(row[8]);
      expect(["billingProfile", "alias", "provider", "unassigned", "ambiguous"]).toContain(row[18]);
      expect(["true", "false"]).toContain(row[22]);
    }
    expect(content).not.toMatch(/"(?:总计|合计|grand\s+total)"/i);
    // 缺金额是空字段，不是 0
    const unpriced = rows.filter((row) => row[8] === "unpriced");
    expect(unpriced.length).toBeGreaterThan(0);
    for (const row of unpriced) expect(row[7]).toBe("");
  });

  it("6. mock 表头与后端 cost_query.rs 的 CSV_HEADER 逐字一致", () => {
    const rust = readFileSync(
      path.join(process.cwd(), "src-tauri/src/services/usage/v2/cost_query.rs"),
      "utf8"
    );
    const match = rust.match(/const CSV_HEADER: &str = "([^"]+)";/);
    expect(match, "未找到 cost_query.rs 的 CSV_HEADER").toBeTruthy();
    const header = mockUsageCostCsv({ preset: "last7d" }).split("\n")[0];
    expect(header).toBe(match?.[1]);
  });
});
