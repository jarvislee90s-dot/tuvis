// Task 9（计划②）· 页签壳 + 用量记录页 `src/components/usage/UsageRecordsPanel.tsx`（10 用例；
// 第 10 条是 2026-10-06 补的「导出口径 = 记录页那一份筛选与卡片维度」）。
// 判据来源：spec P3（每工具一卡 / 卡内行**固定**「供应商 / 模型」/ 筛选 = 工具 + 子代理模式）、
// D6（卡内行维度不随分组切换）、D7（卡片分组只接受 tool | project）、D21（项目 key 小写、label 原文）、
// 契约 §3 要点 4（日档 + parentsOnly 会被后端判第 9 码 → 预防优于报错）与计划 §3 第 5/6/13/19/22 条。
//
// 纪律：
//  * 语言固定 zh（同 `usage-page.test.tsx`）：detector 在 jsdom 下读 navigator(en-US) ⇒ 默认英文；
//    本文件断言的中文文案（「看板」/「含子代理」/「展开其余 3 行」）只有 zh 有。
//  * 渲染**页面**再看记录页签（不是孤立渲染面板）：页签壳、共用 range、`invoke` 入参都是本任务的
//    交付面；`invoke` 走 tests/setup.ts 接进 `@tauri-apps/api/core` 的那个单例（① 的 mock case →
//    ② 同源夹具），不另起 `vi.mock`，也不覆盖 mock 实现（那样绕开夹具、断言会变成自说自话）。
//  * 值口径与 hero 一致（`metrics.requestTotal + buckets.output`）：claude 卡合计 129.22万 =
//    `fmtTokens(1_246_567 + 45_678)`，行 0 = `fmtTokens(155_821 + 5_710)`（夹具份额 = 15/120）。
//  * 日档守卫三层（禁用 + 复位 + filters 双保险）**逐层断言**，判据是「日档绝不发出带 parentsOnly
//    的调用」这条 API 级扫描，不是「界面看起来对」。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, onTestFinished, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（theme-provider 的首帧主题判定会用到；与 usage-page.test.tsx 同模式）
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

// WindowFrame/TitleBar 依赖 Tauri 窗口 API，jsdom 下全 mock（与 petSettings/toolManagement 同模式）
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    isMaximized: vi.fn(async () => false),
    onResized: vi.fn(async () => () => {}),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    close: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

import i18n from "@/i18n";
import { UsageDashboardBody } from "@/components/usage/UsageDashboardBody";
import { RECORD_ROW_CAP } from "@/components/usage/UsageRecordsPanel";
import { EM_DASH } from "@/lib/usage/format";
import { tauriInvokeMock } from "../msw/tauriMocks";

type InvokeCall = [string, Record<string, unknown> | undefined];

/** setup.ts 把 invoke 接到了这个 vi.fn 上（每用例后清空调用记录） */
function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

/** 页面渲染（独立 cache + `retry: false`：只验页面行为，重试策略不是本任务的产物） */
function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return render(<UsageDashboardBody active />, { wrapper });
}

/** 打开「记录」页签（默认页签是「看板」——两页签共用同一 range 与同一次打开窗口） */
function openRecords() {
  const page = renderPage();
  fireEvent.click(screen.getByTestId("usage-tab-records"));
  return page;
}

/** 某个卡片内的查询域（逐行断言用） */
function card(key: string): HTMLElement {
  return screen.getByTestId(`usage-record-card-${key}`);
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("usage-records（计划② Task 9：页签壳 + 用量记录页）", () => {
  beforeEach(() => localStorage.clear());

  it("第四轮实机反馈：记录卡三列表头（供应商显示名 / token 数 / 请求次数）+ 每行请求次数 + 合计行", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    fireEvent.click(screen.getByTestId("usage-tab-records"));
    await screen.findByTestId("usage-record-card-claude");

    // 表头三列在场
    const cols = screen.getByTestId("usage-record-cols-claude");
    expect(cols.textContent).toContain("供应商显示名");
    expect(cols.textContent).toContain("token 数");
    expect(cols.textContent).toContain("请求次数");

    // 行内请求次数列：夹具 claude 行 0/1 的 requests 数（mock 提供，非空数字）
    expect(screen.getByTestId("usage-record-requests-claude:0").textContent).toMatch(/^\d/);
    expect(screen.getByTestId("usage-record-requests-claude:1").textContent).toMatch(/^\d/);

    // 合计行：含「合计」；请求合计 ≥ 任一单行值（全量行的 Σ）
    const total = screen.getByTestId("usage-record-total-claude");
    expect(total.textContent).toContain("合计");
    const reqCells = screen
      .getAllByTestId(/^usage-record-requests-claude:/)
      .map((el) => Number((el.textContent ?? "").replace(/,/g, "")))
      .filter((n) => Number.isFinite(n));
    const totalReqs = Number(
      (total.textContent ?? "").split("\n").pop()?.replace(/[^\d]/g, "") ?? ""
    );
    if (reqCells.length && Number.isFinite(totalReqs) && totalReqs > 0) {
      expect(totalReqs).toBeGreaterThanOrEqual(Math.max(...reqCells));
    }
  });

  it("1. 页签壳（默认看板，两页签共用同一 range / 同一次打开窗口）+ 每工具一卡、标题走展示名与合计", async () => {
    // 可见即采的 60s 去抖记在模块作用域（跨实例共享）：前一个用例已记下时间戳，把 Date 拨过
    // 窗口，模拟「新开窗口」的首次打开仍会强制采集；只伪造 Date，waitFor 仍走真实时钟。
    const at = vi.spyOn(Date, "now").mockReturnValue(Date.now() + 61_000);
    onTestFinished(() => at.mockRestore());

    renderPage();

    // 页签条在范围条下方，`aria-pressed` 表达当前页；默认「看板」
    expect(screen.getByTestId("usage-tab-board")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-tab-records")).toHaveAttribute("aria-pressed", "false");
    await screen.findByTestId("usage-hero");

    fireEvent.click(screen.getByTestId("usage-tab-records"));
    expect(screen.getByTestId("usage-tab-records")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-tab-board")).toHaveAttribute("aria-pressed", "false");
    // 记录页走面板（自带三态），**不复用**看板 body
    expect(screen.queryByTestId("usage-hero")).toBeNull();

    // 每（夹具）工具一卡：默认卡片维度 = 按工具
    const claude = await screen.findByTestId("usage-record-card-claude");
    expect(screen.getAllByTestId(/^usage-record-card-/)).toHaveLength(4);
    expect(screen.getByTestId("usage-records-dim-tool")).toHaveAttribute("aria-pressed", "true");

    // 标题 = `usageAgentLabel(card.toolId)`（真机 toolLabel 是**小写工具 id**，直渲会印 claude）
    expect(claude).toHaveTextContent("Claude");
    expect(screen.getByTestId("usage-record-card-workbuddy")).toHaveTextContent("WorkBuddy");

    // 卡片合计 = requestTotal + output（与 hero 同口径），只显示万/亿缩写
    expect(claude).toHaveTextContent("129.22万");
    expect(claude.querySelector("[title='1,292,245']")).toBeNull();

    // 两页签共用同一 range：记录页首查 = 打开的当前档；看板查询仍在、采集恰一次（同一次打开窗口）
    expect(callsOf("usage_records")[0][1]).toEqual({
      range: { preset: "last7d" },
      groupBy: "tool",
      filters: {},
    });
    expect(callsOf("usage_dashboard").length).toBeGreaterThan(0);
    expect(callsOf("usage_collect_visible")).toHaveLength(1);
  });

  it("2. 卡内行恒为「供应商 / 模型」原文 + 万/亿缩写（前端不拼、不随卡片维度变）", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    const row0 = screen.getByTestId("usage-record-row-claude:0");
    // 真机 `row.label` 已是 `format!("{} / {}", provider, model)`；夹具这里是「供应商不可得」的形态
    // （只有模型名）⇒ 直渲原文：既不拼第二层，也不改写成卡片维度的名字
    expect(row0).toHaveTextContent("claude-sonnet-4-5");
    expect(within(row0).getByTitle("claude-sonnet-4-5")).toBeTruthy();
    expect(row0.textContent).not.toContain("/");
    // 行序 = 后端 rows 顺序（testid 的 index 与 row.key 一一对应）
    expect(screen.getByTestId("usage-record-row-claude:1")).toHaveTextContent("claude-opus-4-1");

    // 数值口径与 hero 一致（`requestTotal + output`），只显示缩写，不挂原始值 tooltip
    expect(row0).toHaveTextContent("16.15万");
    expect(row0.querySelector("[title='161,531']")).toBeNull();
  });

  it("3. 供应商归因三态徽标逐行可见，且三态配色互不相同（深色档另有取值）", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 逐行都带徽标（不是只给卡片挂一个）：claude 卡折叠态 12 行全为「推断」
    const inferred = within(card("claude")).getAllByTestId("usage-record-source-inferred");
    expect(inferred).toHaveLength(RECORD_ROW_CAP);
    expect(inferred[0]).toHaveTextContent("推断");

    const measured = within(card("zcode")).getAllByTestId("usage-record-source-measured")[0];
    const unknown = within(card("workbuddy")).getByTestId("usage-record-source-unknown");
    expect(measured).toHaveTextContent("实测");
    expect(unknown).toHaveTextContent("未知");

    // 三态可区分：文案不同 + 配色不同 + 各有深色档取值（浅色档取色一律走主题 token）
    expect(measured.className).not.toBe(inferred[0].className);
    expect(unknown.className).not.toBe(inferred[0].className);
    expect(inferred[0].className).toMatch(/dark:/);
    expect(measured.className).toMatch(/dark:/);
    expect(unknown.className).toMatch(/dark:/);
    expect(within(card("codex")).queryAllByTestId("usage-record-source-unknown")).toHaveLength(0);
  });

  it("4. 超 12 行折叠为「展开其余 N 行」，展开后再收起（逐卡独立）", async () => {
    expect(RECORD_ROW_CAP).toBe(12);
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // claude 卡夹具 15 行 → 12 行 + 折叠按钮；未超限的卡没有按钮
    expect(screen.getAllByTestId(/^usage-record-row-claude:/)).toHaveLength(RECORD_ROW_CAP);
    const more = screen.getByTestId("usage-record-more-claude");
    expect(more).toHaveTextContent("展开其余 3 行");
    expect(screen.queryByTestId("usage-record-more-codex")).toBeNull();

    fireEvent.click(more);
    expect(screen.getAllByTestId(/^usage-record-row-claude:/)).toHaveLength(15);
    expect(screen.getByTestId("usage-record-more-claude")).toHaveTextContent("收起");

    fireEvent.click(screen.getByTestId("usage-record-more-claude"));
    expect(screen.getAllByTestId(/^usage-record-row-claude:/)).toHaveLength(RECORD_ROW_CAP);
    expect(screen.getByTestId("usage-record-more-claude")).toHaveTextContent("展开其余 3 行");
  });

  it("5. 切「按项目」→ groupBy=project 重查，卡片键小写 / 标题原文，卡内行维度不变", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    await waitFor(() =>
      expect(callsOf("usage_records").some((c) => c[1]?.groupBy === "project")).toBe(true)
    );
    expect(screen.getByTestId("usage-records-dim-project")).toHaveAttribute("aria-pressed", "true");

    // D21：卡片 testid = 契约 key（小写规范化），标题 = label 原文（大小写原样）
    const proj = await screen.findByTestId("usage-record-card-multiagents-manager");
    expect(proj).toHaveTextContent("MultiAgents-Manager");

    // D6：卡内行**恒为**「供应商 / 模型」，不跟着卡片维度变（切「按项目」只换卡片键）
    const projRow0 = screen.getByTestId("usage-record-row-multiagents-manager:0");
    expect(projRow0).toHaveTextContent("claude-sonnet-4-5");
    expect(projRow0).not.toHaveTextContent("MultiAgents-Manager");
    expect(within(projRow0).getByTitle("claude-sonnet-4-5")).toBeTruthy();
  });

  it("6. 工具 chips = D1 的 7 源（展示名走 usageAgentLabel）；小时档下 chips 与子代理模式写进 filters", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 不是夹具那 4 个工具：恰为 7 个采集源，顺序 = D1。SUPPORTED_TOOLS 是看板
    // 筛选清单（7 工具），与用量 7 源仅是巧合同成员（位次不同），语义不同源
    expect(
      screen.getAllByTestId(/^usage-records-tool-/).map((el) => el.getAttribute("data-testid"))
    ).toEqual([
      "usage-records-tool-claude",
      "usage-records-tool-codex",
      "usage-records-tool-kimi",
      "usage-records-tool-opencode",
      "usage-records-tool-workbuddy",
      "usage-records-tool-zcode",
      "usage-records-tool-dsh",
    ]);
    expect(screen.getByTestId("usage-records-tool-claude")).toHaveTextContent("Claude");
    expect(screen.getByTestId("usage-records-tool-kimi")).toHaveTextContent("Kimi Code");

    // 小时档：chips 与子代理模式都能算得出，逐个写进 `filters`
    fireEvent.click(screen.getByTestId("usage-range-today"));
    await waitFor(() =>
      expect(
        callsOf("usage_records").some(
          (c) => JSON.stringify(c[1]?.range) === JSON.stringify({ preset: "today" })
        )
      ).toBe(true)
    );

    fireEvent.click(screen.getByTestId("usage-records-tool-claude"));
    await waitFor(() =>
      expect(
        callsOf("usage_records").some(
          (c) => JSON.stringify(c[1]?.filters) === JSON.stringify({ toolIds: ["claude"] })
        )
      ).toBe(true)
    );
    expect(screen.getByTestId("usage-records-tool-claude")).toHaveAttribute("aria-pressed", "true");
    // 筛选真的落到卡片集合上（夹具按 toolIds 过滤卡片池）
    await waitFor(() => expect(screen.queryByTestId("usage-record-card-codex")).toBeNull());
    expect(screen.getByTestId("usage-record-card-claude")).toBeTruthy();

    fireEvent.click(screen.getByTestId("usage-records-subagent"));
    await waitFor(() =>
      expect(
        callsOf("usage_records").some(
          (c) =>
            JSON.stringify(c[1]?.filters) ===
            JSON.stringify({ toolIds: ["claude"], subagentMode: "parentsOnly" })
        )
      ).toBe(true)
    );
    // 小时档 + parentsOnly 正常出数（不会被第 9 码打回）
    expect(await screen.findByTestId("usage-record-card-claude")).toBeTruthy();
  });

  it("7. 切「仅父会话」→ 开关态文案与页脚口径同步切「不含子代理」", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 默认口径「含子代理」；开关文案 = 当前状态
    expect(screen.getByTestId("usage-records-caliber")).toHaveTextContent("含子代理");
    expect(screen.getByTestId("usage-records-caliber")).not.toHaveTextContent("不含子代理");

    fireEvent.click(screen.getByTestId("usage-range-today"));
    const sw = await screen.findByTestId("usage-records-subagent");
    await waitFor(() => expect(sw).not.toBeDisabled());
    expect(sw).toHaveAttribute("aria-checked", "false");
    expect(sw).toHaveAttribute("aria-label", "含子代理");

    fireEvent.click(sw);
    await waitFor(() =>
      expect(screen.getByTestId("usage-records-caliber")).toHaveTextContent("不含子代理")
    );
    expect(screen.getByTestId("usage-records-subagent")).toHaveAttribute("aria-checked", "true");
    expect(screen.getByTestId("usage-records-subagent")).toHaveAttribute("aria-label", "仅父会话");
  });

  it("8. 日档：开关禁用 + 常驻原因 + 进档复位；日档绝不发 parentsOnly，小时档 + parentsOnly 正常", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 默认档 last7d = 日档：开关禁用 + 常驻原因（可见文案 + 开关 title 双通道）
    const daySwitch = screen.getByTestId("usage-records-subagent");
    expect(daySwitch).toBeDisabled();
    const reason = screen.getByTestId("usage-records-subagent-unavailable");
    expect(reason).toHaveTextContent("日聚合行不带会话标识");
    expect(reason).toHaveTextContent("近 5 小时");
    // 开关的 title 与可见原因同源（`toHaveAttribute` 不吃 RegExp，直接断原文）
    expect(daySwitch.getAttribute("title")).toContain("日聚合行不带会话标识");

    // 小时档：开关可用、无禁用原因，打开后 parentsOnly 正常出数
    fireEvent.click(screen.getByTestId("usage-range-today"));
    await waitFor(() => expect(screen.getByTestId("usage-records-subagent")).not.toBeDisabled());
    expect(screen.queryByTestId("usage-records-subagent-unavailable")).toBeNull();
    fireEvent.click(screen.getByTestId("usage-records-subagent"));
    await waitFor(() =>
      expect(
        callsOf("usage_records").some((c) => c[1]?.filters?.subagentMode === "parentsOnly")
      ).toBe(true)
    );
    expect(await screen.findByTestId("usage-record-card-claude")).toBeTruthy();

    // 回日档：第三层（filters 双保险）先保证不发该组合，第二层（useEffect 复位）把开关拨回 default
    fireEvent.click(screen.getByTestId("usage-range-last7d"));
    await waitFor(() => expect(screen.getByTestId("usage-records-subagent")).toBeDisabled());
    await waitFor(() =>
      expect(screen.getByTestId("usage-records-subagent")).toHaveAttribute("aria-checked", "false")
    );
    expect(screen.getByTestId("usage-records-subagent").getAttribute("title")).toContain(
      "日聚合行不带会话标识"
    );
    expect(screen.getByTestId("usage-records-caliber")).toHaveTextContent("含子代理");
    // API 级判据：**日档**（last7d / last30d / custom）的调用一律不得带 parentsOnly
    for (const call of callsOf("usage_records")) {
      const preset = (call[1]?.range as { preset?: string } | undefined)?.preset;
      if (preset !== "last5h" && preset !== "today") {
        expect(call[1]?.filters?.subagentMode).toBeUndefined();
      }
    }
  });

  it("9. 明细保留期由设置项驱动并常驻明示；空态出「暂无数据」而不是 0", async () => {
    const page = openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 设置夹具默认 `detailRetentionDays = 90`（设置查询与记录页并行，谁先落地不定 ⇒ 这句要等）
    await waitFor(() =>
      expect(screen.getByTestId("usage-records-retention")).toHaveTextContent("90")
    );
    expect(screen.getByTestId("usage-records-retention")).toHaveTextContent("明细");
    page.unmount();

    // 空态：无卡片、无 0 值文本；保留期照常明示（口径与保留期不是「有数据才说」的东西）
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    localStorage.setItem("mam-mock-usage", "empty");
    openRecords();
    expect(await screen.findByTestId("usage-records-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryAllByTestId(/^usage-record-card-/)).toHaveLength(0);
    expect(screen.queryByText("0")).toBeNull();
    await waitFor(() =>
      expect(screen.getByTestId("usage-records-retention")).toHaveTextContent("90")
    );
    warn.mockRestore();
  });

  it("10. 导出口径 = 记录页那一份筛选与卡片维度（2026-10-06 用户裁决：CSV 要接收记录页的筛选条件）", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // ① 记录页选一个工具 chip + 把卡片维度切到「按项目」
    fireEvent.click(screen.getByTestId("usage-records-tool-opencode"));
    await waitFor(() =>
      expect(
        callsOf("usage_records").some(
          (c) => (c[1]?.filters as { toolIds?: string[] } | undefined)?.toolIds?.[0] === "opencode"
        )
      ).toBe(true)
    );
    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    await waitFor(() =>
      expect(callsOf("usage_records").some((c) => c[1]?.groupBy === "project")).toBe(true)
    );

    // ② 导出 CSV：必须与屏幕**同一个口径**（同一份 filters，且 groupBy = 记录页的卡片维度）。
    //    旧实现在这里恒发 `filters: {}` + **看板的**分布维度 ⇒ 屏幕按项目筛出来、导出却是全量按工具。
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() => expect(callsOf("usage_export_csv")).toHaveLength(1));
    const csv = callsOf("usage_export_csv")[0][1] as {
      groupBy?: string;
      filters?: { toolIds?: string[]; subagentMode?: string };
    };
    expect(csv.groupBy).toBe("project"); // ← 记录页的 cardDim，不是看板的分布维度
    expect(csv.filters).toEqual({ toolIds: ["opencode"] });
    expect(csv.filters?.subagentMode).toBeUndefined();

    // ③ 小时档 + 「仅父会话」：导出同样要带上（与 `usage_records` 一致）
    fireEvent.click(screen.getByTestId("usage-range-today"));
    await waitFor(() => expect(screen.getByTestId("usage-records-subagent")).not.toBeDisabled());
    fireEvent.click(screen.getByTestId("usage-records-subagent"));
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() => expect(callsOf("usage_export_csv")).toHaveLength(2));
    const csv2 = callsOf("usage_export_csv")[1][1] as { filters?: { subagentMode?: string } };
    expect(csv2.filters?.subagentMode).toBe("parentsOnly");

    // ④ 日档：三层守卫**同样守住导出这条路**——CSV 绝不带 `subagentMode`
    //    （否则后端以第 9 码 `usage-filter-unavailable` 拒收，导出一个「数字含子代理、页脚说不含」的文件）
    fireEvent.click(screen.getByTestId("usage-range-last7d"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-records-subagent")).toHaveAttribute("aria-checked", "false")
    );
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() => expect(callsOf("usage_export_csv")).toHaveLength(3));
    const csv3 = callsOf("usage_export_csv")[2][1] as {
      range?: { preset?: string };
      filters?: { subagentMode?: string };
    };
    expect(csv3.range?.preset).toBe("last7d");
    expect(csv3.filters?.subagentMode).toBeUndefined();
    // 反向判据：日档下**每一次**导出都不得带该组合（不只上面那一次）
    for (const call of callsOf("usage_export_csv")) {
      const preset = (call[1]?.range as { preset?: string } | undefined)?.preset;
      if (preset !== "last5h" && preset !== "today") {
        expect((call[1]?.filters as { subagentMode?: string })?.subagentMode).toBeUndefined();
      }
    }

    // ⑤ **看板页签**的导出保持旧口径：`filters: {}` + 看板的分布维度（看板没有筛选控件）。
    //    这一支原先无人覆盖 ⇒ 将来若把 `rec.filters` 改成无条件传，不会有人红。
    fireEvent.click(screen.getByTestId("usage-tab-board"));
    await screen.findByTestId("usage-hero");
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() => expect(callsOf("usage_export_csv")).toHaveLength(4));
    const csv4 = callsOf("usage_export_csv")[3][1] as { groupBy?: string; filters?: unknown };
    expect(csv4.filters).toEqual({}); // 看板没有筛选控件 ⇒ 不筛
    expect(csv4.groupBy).toBe("tool"); // 看板的分布维度（默认「按工具」），不是记录页的卡片维度
  });

  it("11. **没量到 token 的卡**（只有真空回合行）→ 该卡合计与行值出 `—`、卡仍在；有 token 的卡逐字不变（P8）", async () => {
    // 真机可达（用户 2026-10-06 给出的账本实测）：某个工具在该窗口**只落了真空回合行**——
    // 四桶全零、但 `requests ≥ 1`。用户裁决「没产生数值就不显示数值」⇒ 该卡的 token 位出 `—`；
    // **卡本身仍在**（它有 requests 真值，不是「整页空态」）。判据是**逐卡**的（窗口级判据在记录页
    // 不适用：一行都没有的卡与「有行但没量到 token」的卡是两回事）。
    localStorage.setItem("mam-mock-usage", "unmeasured");
    const page = openRecords();
    const claude = await screen.findByTestId("usage-record-card-claude");
    // 卡片合计（= requestTotal + output）⇒ `合计 —`，不是 `合计 0`；hover 的精确值也不许是 0
    expect(claude).toHaveTextContent(`合计 ${EM_DASH}`);
    expect(claude).not.toHaveTextContent("合计 0");
    expect(claude.querySelector("[title='0']")).toBeNull();
    // 卡内行的 token 位同理（行尾那个值）
    expect(screen.getByTestId("usage-record-tokens-claude:0").textContent).toBe(EM_DASH);
    // 四张卡都这样，且每个**值**位都是 `—`（模型/工具名里有数字，故只逐值断言，不做整卡"无数字"）
    const cards = screen.getAllByTestId(/^usage-record-card-/);
    expect(cards).toHaveLength(4);
    for (const card of cards) {
      expect(card).toHaveTextContent(`合计 ${EM_DASH}`);
      const values = [...card.querySelectorAll("span.font-medium")];
      expect(values.length).toBeGreaterThan(0);
      for (const v of values) expect(v.textContent).toBe(EM_DASH);
    }
    // 不是「整页空态」：卡在、页脚口径在
    expect(screen.queryByTestId("usage-records-empty")).toBeNull();
    expect(screen.getByTestId("usage-records-caliber")).toBeTruthy();
    page.unmount();

    // **反断言**：有 token 的窗口逐字不变（与用例 1 同一批期望值）
    localStorage.removeItem("mam-mock-usage");
    openRecords();
    const ok = await screen.findByTestId("usage-record-card-claude");
    expect(ok).toHaveTextContent("合计 129.22万");
    expect(ok.querySelector("[title='1,292,245']")).toBeNull();
    expect(screen.getByTestId("usage-record-tokens-claude:0").textContent).toBe("16.15万");
  });

  it("12. 页脚「高级」入口打开计费归属与参考价规则管理器（旧逐行规则入口已收敛）", async () => {
    openRecords();
    await screen.findByTestId("usage-record-card-claude");

    // 普通行不再有「指定渠道 / 修改关联 / 查看规则」入口
    expect(screen.queryAllByTestId(/^usage-record-assign-/)).toHaveLength(0);
    expect(screen.queryAllByTestId(/^usage-record-rules-/)).toHaveLength(0);
    expect(screen.queryByText("查看规则")).toBeNull();

    const advanced = screen.getByTestId("usage-records-billing-advanced");
    expect(advanced).toHaveTextContent("高级：计费归属与参考价规则");
    // 管理器未打开时其内容不渲染
    expect(screen.queryByTestId("billing-revision")).toBeNull();

    fireEvent.click(advanced);
    expect(await screen.findByTestId("billing-revision")).toHaveTextContent("当前版本：0");
    // 页脚入口与管理器标题同口径（「计费归属与参考价规则」）
    expect(screen.getByText("计费归属与参考价规则")).toBeTruthy();
  });
});
