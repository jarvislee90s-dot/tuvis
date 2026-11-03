// Task 12 · 页面编排：A/B 固定展开、C 默认展开、E 默认收起；切范围/页签折叠状态保留；
// B 局部筛选不影响全局 hero；C 分组切换不影响 B。
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

window.matchMedia = ((query: string) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: vi.fn(),
  removeListener: vi.fn(),
  addEventListener: vi.fn(),
  removeEventListener: vi.fn(),
  dispatchEvent: vi.fn(),
})) as unknown as typeof window.matchMedia;

vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    isMaximized: vi.fn(async () => false),
    onResized: vi.fn(async () => () => {}),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    close: vi.fn(),
  }),
}));

import i18n from "@/i18n";
import { UsageDashboardBody } from "@/components/usage/UsageDashboardBody";
import { tauriInvokeMock } from "../msw/tauriMocks";

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

type Impl = (cmd: string, args?: unknown) => Promise<unknown>;

/** 在既有 mock 路由上覆写指定命令；返回恢复函数 */
function override(overrides: Record<string, Impl>) {
  const original = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
  tauriInvokeMock.mockImplementation((cmd: string, args?: unknown) =>
    overrides[cmd] ? overrides[cmd](cmd, args) : (original?.(cmd, args) ?? Promise.resolve())
  );
  return () => {
    if (original) tauriInvokeMock.mockImplementation(original);
  };
}

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return render(<UsageDashboardBody active />, { wrapper });
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
});

describe("usage-dashboard V2 编排", () => {
  it("1. A/B 无折叠钮；C 初始展开；E 收起且卡头摘要常在", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    // A Token 总览与 B 用量趋势固定展开：卡壳里没有折叠按钮
    expect(screen.getByTestId("usage-summary-card").querySelector("[aria-expanded]")).toBeNull();
    expect(screen.getByTestId("usage-trend-card").querySelector("[aria-expanded]")).toBeNull();

    // C 默认 billingProfile 且展开
    expect(screen.getByTestId("usage-fold-toggle-distribution")).toHaveAttribute(
      "aria-expanded",
      "true"
    );
    // 折叠指示符是 Chevron 图标（展开向下 / 收起向右），不再是 +/− 文本
    expect(
      screen.getByTestId("usage-fold-toggle-distribution").querySelector("svg.lucide-chevron-down")
    ).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("usage-fold-content-distribution")).toBeTruthy());
    expect(
      callsOf("usage_distribution").some(
        (c) =>
          JSON.stringify(c[1]) ===
          JSON.stringify({
            range: { preset: "last7d" },
            groupBy: "billingProfile",
          })
      )
    ).toBe(true);

    // E 默认收起；卡头摘要在空态/收起时仍可见
    expect(screen.getByTestId("usage-fold-toggle-work")).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.getByTestId("usage-fold-toggle-work").querySelector("svg.lucide-chevron-right")
    ).toBeTruthy();
    expect(screen.queryByTestId("usage-fold-content-work")).toBeNull();
    expect(screen.getByTestId("usage-fold-summary-work")).toBeTruthy();

    // 展开 E 显示原内容（无卡中卡 body）
    fireEvent.click(screen.getByTestId("usage-fold-toggle-work"));
    expect(screen.getByTestId("usage-fold-toggle-work")).toHaveAttribute("aria-expanded", "true");
    expect(
      screen.getByTestId("usage-fold-toggle-work").querySelector("svg.lucide-chevron-down")
    ).toBeTruthy();
    expect(screen.getByTestId("usage-work-body")).toBeTruthy();
  });

  it("2. 切范围 / 切页签后折叠状态保留（不在 board 条件子树里重置）", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    // 收起 C、展开 E 作为初始基线（D 默认展开，见用例 10）
    fireEvent.click(screen.getByTestId("usage-fold-toggle-distribution"));
    fireEvent.click(screen.getByTestId("usage-fold-toggle-work"));
    expect(screen.getByTestId("usage-fold-toggle-distribution")).toHaveAttribute(
      "aria-expanded",
      "false"
    );
    expect(screen.getByTestId("usage-fold-toggle-work")).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "true");

    // 切范围
    fireEvent.click(screen.getByText("近 30 天"));
    await waitFor(() =>
      expect(screen.getByText("近 30 天")).toHaveAttribute("aria-pressed", "true")
    );
    expect(screen.getByTestId("usage-fold-toggle-distribution")).toHaveAttribute(
      "aria-expanded",
      "false"
    );
    expect(screen.getByTestId("usage-fold-toggle-work")).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "true");

    // 切到记录页再回看板
    fireEvent.click(screen.getByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-tab-board"));
    await screen.findByTestId("usage-hero");
    expect(screen.getByTestId("usage-fold-toggle-distribution")).toHaveAttribute(
      "aria-expanded",
      "false"
    );
    expect(screen.getByTestId("usage-fold-toggle-work")).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "true");
  });

  it("3. 趋势选择 codex 不改变全局 hero（A 用旧 dashboard 查询）", async () => {
    renderPage();
    const hero = await screen.findByTestId("usage-hero");
    expect(hero).toHaveTextContent("203.70万");

    fireEvent.change(screen.getByTestId("usage-trend-mode"), { target: { value: "tool" } });
    await waitFor(() => expect(screen.getByTestId("usage-trend-target")).toBeTruthy());
    fireEvent.change(screen.getByTestId("usage-trend-target"), {
      target: { value: "tool:codex" },
    });

    await waitFor(() =>
      expect(
        callsOf("usage_trend").some(
          (c) =>
            JSON.stringify(c[1]?.selection) ===
            JSON.stringify({ scope: "tool", selectedId: "codex" })
        )
      ).toBe(true)
    );
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("203.70万");
  });

  it("4. 分布切换只改 C 查询，不改变 B 的趋势选择", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");
    await waitFor(() => expect(screen.getByTestId("usage-dist-group-model")).toBeTruthy());
    const trendCallsBefore = callsOf("usage_trend").length;

    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(
        callsOf("usage_distribution").some(
          (c) =>
            JSON.stringify(c[1]) ===
            JSON.stringify({
              range: { preset: "last7d" },
              groupBy: "model",
            })
        )
      ).toBe(true)
    );

    expect(callsOf("usage_trend").length).toBe(trendCallsBefore);
    for (const call of callsOf("usage_trend")) {
      expect(call[1]?.selection).toEqual({ scope: "all" });
    }
  });

  it("5. distributionGroup / trendSelection 切范围与页签后保留", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    // 趋势选 codex，分布切 model
    fireEvent.change(screen.getByTestId("usage-trend-mode"), { target: { value: "tool" } });
    fireEvent.change(screen.getByTestId("usage-trend-target"), {
      target: { value: "tool:codex" },
    });
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-group-model")).toHaveAttribute("aria-pressed", "true")
    );

    // 切范围：两个状态都保留，并用同一 selection/groupBy 重查
    fireEvent.click(screen.getByText("近 30 天"));
    await waitFor(() =>
      expect(
        callsOf("usage_trend").some(
          (c) =>
            JSON.stringify(c[1]) ===
            JSON.stringify({
              range: { preset: "last30d" },
              selection: { scope: "tool", selectedId: "codex" },
            })
        )
      ).toBe(true)
    );
    await waitFor(() => expect(screen.getByTestId("usage-dist-group-model")).toBeTruthy());
    expect(screen.getByTestId("usage-dist-group-model")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-trend-target")).toHaveValue("tool:codex");

    // 切页签再回来
    fireEvent.click(screen.getByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-tab-board"));
    await screen.findByTestId("usage-hero");
    await waitFor(() => expect(screen.getByTestId("usage-dist-group-model")).toBeTruthy());
    expect(screen.getByTestId("usage-dist-group-model")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-trend-target")).toHaveValue("tool:codex");
  });

  it("6. E 卡头摘要：真实 sessions/toolCalls；dashboard 空/不可得时出 —", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");
    const summary = screen.getByTestId("usage-fold-summary-work");
    expect(summary).toHaveTextContent("42");
    expect(summary).toHaveTextContent("41,412");

    // 空态：工作活动字段全部不可得 ⇒ 卡头摘要 `—`（不是 0）
    cleanup();
    localStorage.setItem("mam-mock-usage", "empty");
    renderPage();
    await screen.findByTestId("usage-empty");
    const emptySummary = screen.getByTestId("usage-fold-summary-work");
    expect(emptySummary).toHaveTextContent("—");
    expect(emptySummary.textContent).not.toMatch(/\d/);
  });

  it("7. B 不可用原因本地化：不直接印错误码，只保留一个 usage-trend-unavailable", async () => {
    const restore = override({
      usage_trend: () =>
        Promise.resolve({
          selection: { scope: "all" },
          trend: [],
          hero: 0,
          compare: null,
          available: false,
          reason: "usage-filter-unavailable",
        }),
    });

    renderPage();
    const hint = await screen.findByTestId("usage-trend-unavailable");
    expect(hint).toHaveTextContent("该筛选在当前时间档位算不出");
    expect(hint.textContent).not.toContain("usage-filter-unavailable");
    expect(screen.getAllByTestId("usage-trend-unavailable")).toHaveLength(1);
    // 筛选区用自己的 testid 与自己的文案，允许一键回到全部
    expect(screen.getByTestId("usage-trend-filter-unavailable")).toBeTruthy();
    expect(screen.getByTestId("usage-trend-reset")).toBeTruthy();

    restore();
  });

  it("8. 分区错误隔离：趋势失败只替换 B 卡体，A/C/E 照常", async () => {
    const restore = override({
      usage_trend: () => Promise.reject({ code: "usage-db-failed", detail: "trend boom" }),
    });

    renderPage();
    expect(await screen.findByTestId("usage-trend-error")).toHaveTextContent("trend boom");
    // A 与 C/E 不受影响
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("203.70万");
    await waitFor(() => expect(screen.getByTestId("usage-dist-group-billingProfile")).toBeTruthy());
    expect(screen.getByTestId("usage-fold-toggle-work")).toBeTruthy();
    // 全局错误卡没有出现（B 的错误留在 B 卡内）
    expect(screen.queryByTestId("usage-error")).toBeNull();

    restore();
  });

  it("9. 分区错误隔离：旧 dashboard 失败只拖垮 A/E，B/C 照常", async () => {
    const restore = override({
      usage_dashboard: () => Promise.reject({ code: "usage-db-failed", detail: "dashboard boom" }),
    });

    renderPage();
    expect(await screen.findByTestId("usage-error")).toHaveTextContent("dashboard boom");
    // B 图与 C 行仍渲染
    await waitFor(() => expect(screen.getByTestId("usage-trend-svg")).toBeTruthy());
    expect(screen.getByTestId("usage-dist-expand")).toBeTruthy();
    // E 展开后显示自己的错误体
    fireEvent.click(screen.getByTestId("usage-fold-toggle-work"));
    expect(screen.getByTestId("usage-fold-content-work")).toHaveTextContent("dashboard boom");

    restore();
  });

  it("10. D 位于 C 后 E 前：默认展开显示费用行且不自动启用定价；可手动收起", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    // 顺序：C → D → E
    const folds = screen
      .getAllByTestId(/^usage-fold-/)
      .map((el) => el.dataset.testid)
      .filter((id) => /^usage-fold-(distribution|cost|work)$/.test(id ?? ""));
    expect(folds).toEqual(["usage-fold-distribution", "usage-fold-cost", "usage-fold-work"]);

    // 默认展开（2026-10-20 用户裁决）：正文挂载、费用行直接可见、不自动 setEnabled
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "true");
    await waitFor(() => expect(screen.getByTestId("usage-cost-body")).toBeTruthy());
    expect(screen.getByTestId("usage-cost-view-channel")).toHaveAttribute("aria-pressed", "true");
    await waitFor(() =>
      expect(screen.getAllByTestId(/^usage-cost-row-/).length).toBeGreaterThan(0)
    );
    expect(callsOf("usage_pricing_set_enabled")).toHaveLength(0);

    // 手动收起后正文卸载（折叠 DOM 契约保留）
    fireEvent.click(screen.getByTestId("usage-fold-toggle-cost"));
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("usage-cost-body")).toBeNull();
  });

  it("11. D 查询失败只替换 D 卡体，A/B/C/E 不受影响", async () => {
    const restore = override({
      usage_costs: () => Promise.reject({ code: "usage-db-failed", detail: "cost boom" }),
    });

    renderPage();
    await screen.findByTestId("usage-hero");

    // D 默认展开：错误直接可见
    expect(await screen.findByTestId("usage-cost-error")).toHaveTextContent("cost boom");
    // A/B/C/E 照常
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("203.70万");
    expect(screen.getByTestId("usage-trend-card")).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("usage-dist-group-billingProfile")).toBeTruthy());
    expect(screen.getByTestId("usage-fold-toggle-work")).toBeTruthy();

    restore();
  });

  it("12. 首屏结构：A → B → C 卡头 → D → E，且 C 卡头在 board 中常驻", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    const summary = screen.getByTestId("usage-summary-card");
    const trend = screen.getByTestId("usage-trend-card");
    const distribution = screen.getByTestId("usage-fold-distribution");
    const cost = screen.getByTestId("usage-fold-cost");
    const work = screen.getByTestId("usage-fold-work");
    const ordered = [summary, trend, distribution, cost, work];
    for (let i = 1; i < ordered.length; i += 1) {
      // FOLLOWING 位表示 ordered[i] 在 ordered[i-1] 之后
      expect(ordered[i - 1].compareDocumentPosition(ordered[i])).toBe(
        Node.DOCUMENT_POSITION_FOLLOWING
      );
    }
    // C 卡头可见（默认展开），不依赖正文是否在视口内（jsdom 无法测视口）
    expect(within(distribution).getByText("分组分布")).toBeTruthy();
    // A 的 `lg:grid-cols-3` 与趋势图压缩由源码锁（结构断言）
    expect(summary.querySelector(".lg\\:grid-cols-3")).toBeTruthy();
  });

  it("13. 渠道筛选选项来自 ChannelOption：无未指定渠道，选中键进 TrendSelection", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    // 渠道选项查询随范围发起
    await waitFor(() =>
      expect(
        callsOf("usage_channel_options").some(
          (c) => JSON.stringify(c[1]) === JSON.stringify({ range: { preset: "last7d" } })
        )
      ).toBe(true)
    );

    fireEvent.change(screen.getByTestId("usage-trend-mode"), {
      target: { value: "billingProfile" },
    });
    const target = await screen.findByTestId("usage-trend-target");
    await waitFor(() => expect(target).toHaveValue("bp:profile-a"));

    // 只列 ChannelOption，不含每工具的「未指定渠道」
    const optionTexts = Array.from(target.querySelectorAll("option")).map(
      (option) => option.textContent ?? ""
    );
    expect(optionTexts).toEqual(["Profile A", "OpenRouter", "anthropic"]);
    expect(optionTexts.some((text) => text.includes("未指定渠道"))).toBe(false);

    fireEvent.change(target, { target: { value: "provider:anthropic" } });
    await waitFor(() =>
      expect(
        callsOf("usage_trend").some(
          (c) =>
            JSON.stringify(c[1]?.selection) ===
            JSON.stringify({ scope: "billingProfile", selectedId: "provider:anthropic" })
        )
      ).toBe(true)
    );
    // A 不联动
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("203.70万");
  });

  it("14. 选中渠道在选项中失效：显示原因与回到全部；不自动改选其它渠道", async () => {
    const option = (
      key: string,
      label: string,
      kind: "billingProfile" | "provider",
      billingProfileId: string | null
    ) => ({ key, label, kind, billingProfileId, tokenUsage: 1 });
    const restore = override({
      usage_channel_options: (_cmd, args) =>
        Promise.resolve(
          (args as { range?: { preset?: string } } | undefined)?.range?.preset === "last30d"
            ? [option("bp:profile-a", "Profile A", "billingProfile", "profile-a")]
            : [
                option("bp:profile-a", "Profile A", "billingProfile", "profile-a"),
                option("provider:anthropic", "anthropic", "provider", null),
              ]
        ),
    });

    renderPage();
    await screen.findByTestId("usage-hero");
    fireEvent.change(screen.getByTestId("usage-trend-mode"), {
      target: { value: "billingProfile" },
    });
    const target = await screen.findByTestId("usage-trend-target");
    await waitFor(() => expect(target).toHaveValue("bp:profile-a"));
    fireEvent.change(target, { target: { value: "provider:anthropic" } });
    await waitFor(() =>
      expect(
        callsOf("usage_trend").some(
          (c) =>
            JSON.stringify(c[1]?.selection) ===
            JSON.stringify({ scope: "billingProfile", selectedId: "provider:anthropic" })
        )
      ).toBe(true)
    );

    // 切范围后 provider:anthropic 不在新选项中 → 显示原因 + 回到全部
    fireEvent.click(screen.getByText("近 30 天"));
    await waitFor(() => expect(screen.getByTestId("usage-trend-filter-unavailable")).toBeTruthy());
    expect(screen.getByTestId("usage-trend-target")).toHaveValue("provider:anthropic");
    fireEvent.click(screen.getByTestId("usage-trend-reset"));
    await waitFor(() =>
      expect(
        callsOf("usage_trend").some(
          (c) =>
            JSON.stringify(c[1]?.selection) === JSON.stringify({ scope: "all" }) &&
            JSON.stringify(c[1]?.range) === JSON.stringify({ preset: "last30d" })
        )
      ).toBe(true)
    );

    restore();
  });

  it("15. 渠道选项加载中：显示加载态、不显示不可得、不发空键 billingProfile 请求", async () => {
    const restore = override({
      usage_channel_options: () => new Promise(() => {}),
    });

    renderPage();
    await screen.findByTestId("usage-hero");
    fireEvent.change(screen.getByTestId("usage-trend-mode"), {
      target: { value: "billingProfile" },
    });

    expect(await screen.findByTestId("usage-trend-channels-loading")).toHaveTextContent(
      i18n.t("common.loading")
    );
    expect(screen.queryByTestId("usage-trend-filter-unavailable")).toBeNull();
    expect(screen.queryByTestId("usage-trend-channels-error")).toBeNull();
    // enabled 门控：空键的渠道选择不得落到 IPC
    expect(
      callsOf("usage_trend").filter(
        (c) => (c[1]?.selection as { scope?: string } | undefined)?.scope === "billingProfile"
      )
    ).toHaveLength(0);

    restore();
  });

  it("16. 渠道选项失败：显示原因与重试；失效 bp 键显示已失效渠道而非裸键", async () => {
    const option = {
      key: "provider:anthropic",
      label: "anthropic",
      kind: "provider" as const,
      billingProfileId: null,
      tokenUsage: 1,
    };
    const restore = override({
      usage_channel_options: (_cmd, args) => {
        const preset = (args as { range?: { preset?: string } } | undefined)?.range?.preset;
        if (preset === "last30d")
          return Promise.reject({ code: "usage-db-failed", detail: "channels boom" });
        return Promise.resolve([
          {
            key: "bp:profile-a",
            label: "Profile A",
            kind: "billingProfile" as const,
            billingProfileId: "profile-a",
            tokenUsage: 1,
          },
          option,
        ]);
      },
    });

    renderPage();
    await screen.findByTestId("usage-hero");
    fireEvent.change(screen.getByTestId("usage-trend-mode"), {
      target: { value: "billingProfile" },
    });
    const target = await screen.findByTestId("usage-trend-target");
    await waitFor(() => expect(target).toHaveValue("bp:profile-a"));

    // 切范围 → 新范围渠道选项失败：原因 + 重试；不落成裸键
    fireEvent.click(screen.getByText("近 30 天"));
    expect(await screen.findByTestId("usage-trend-channels-error")).toHaveTextContent(
      "channels boom"
    );
    expect(screen.getByTestId("usage-trend-channels-retry")).toBeTruthy();
    expect(screen.queryByTestId("usage-trend-filter-unavailable")).toBeNull();
    expect(screen.getByTestId("usage-trend-target-missing")).toHaveTextContent(
      i18n.t("usage.trend.invalidChannel")
    );
    expect(screen.getByTestId("usage-trend-target-missing")).not.toHaveTextContent("bp:");

    restore();
  });

  it("17. 实机反馈二轮 ②：工具栏并行单行（看板/记录页签在左、范围条在右），页头区不再吸顶", async () => {
    renderPage();
    await screen.findByTestId("usage-tab-board");

    // 页签对与范围条同处一个 justify-between 行（两行合并为一行）
    const board = screen.getByTestId("usage-tab-board");
    const records = screen.getByTestId("usage-tab-records");
    const row = board.closest(".justify-between") as HTMLElement | null;
    expect(row).not.toBeNull();
    const range = row!.querySelector('[data-testid^="usage-range-"]') as HTMLElement | null;
    expect(range).not.toBeNull();
    expect(row!.contains(board)).toBe(true);
    expect(row!.contains(records)).toBe(true);
    // 单行顺序：看板/记录页签对在前、范围条在后
    expect(board.compareDocumentPosition(range!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(records.compareDocumentPosition(range!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();

    // 去吸顶：页头块保留通栏负边距 + 下边框，但不再 sticky 压屏
    const header = board.closest(".border-b") as HTMLElement | null;
    expect(header).not.toBeNull();
    expect(header!.className).toContain("-mx-4");
    expect(header!.className).not.toContain("sticky");
    expect(header!.className).not.toContain("z-10");
  });
});
