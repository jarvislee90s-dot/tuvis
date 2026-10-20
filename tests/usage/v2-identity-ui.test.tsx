// Task 13 · 记录页渠道绑定 Dialog / 计费渠道管理器。
// 覆盖：project 卡按项目键查询 identities；同名多候选不合并且不自动选中；每行展示当前归属与 Token；
// 只读身份、默认 exact/今后；预览前不能保存；候选/历史日期变化清空预览；冲突后禁用保存；
// 空名不能保存；archived 不可选；Manager 新建/改名/归档/规则编辑、失败回滚与撤销。
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
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
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

import i18n from "@/i18n";
import UsageDashboardPage from "@/pages/usage-dashboard";
import { BillingAssignmentDialog } from "@/components/usage/BillingAssignmentDialog";
import { BillingProfilesManager, ruleIssues } from "@/components/usage/BillingProfilesManager";
import { usageBillingCommit } from "@/lib/api/usageV2";
import { mockUsageRecords, resetUsageV2MockState } from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { UsageRange } from "@/types/usage";
import type { BillingConfig, BillingRule, IdentityUsage } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);
const RANGE: UsageRange = { preset: "last7d" };

type Kind = IdentityUsage["attribution"]["attributionKind"];

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(children: ReactNode, client = makeClient()) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

function identity(
  sourceId: "codex" | "claude",
  modelId = "claude-sonnet-4-5",
  billingProfileId: string | null = null,
  kind: Kind = billingProfileId ? "userAssigned" : "unknown"
): IdentityUsage {
  return {
    identity: { sourceId, observedProvider: null, modelId },
    sourceKind: "measured",
    attribution: {
      billingProfileId,
      attributionKind: kind,
      ruleIds: [],
      reason: billingProfileId ? null : "unassigned",
    },
    buckets: { inputFresh: 80, cacheRead: 20, cacheWrite: 0, output: 40 },
    metrics: { requestTotal: 100, cacheHitRate: 0.2, userEst: 12, requests: 3 },
  };
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

function callsOf(cmd: string): Array<[string, Record<string, unknown> | undefined]> {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as Array<
    [string, Record<string, unknown> | undefined]
  >;
}

function configOf(
  revision: number,
  profiles: BillingConfig["profiles"],
  rules: BillingRule[] = []
): BillingConfig {
  return { revision, profiles, rules, pricingRules: [] };
}

function rule(
  id: string,
  billingProfileId: string,
  match: "exact" | "prefix" = "exact",
  effectiveFrom = 0,
  effectiveTo: number | null = null,
  mixed = false
): BillingRule {
  return {
    id,
    identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
    match,
    billingProfileId,
    effectiveFrom,
    effectiveTo,
    mixed,
  };
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

let defaultImpl: ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;

beforeEach(() => {
  localStorage.clear();
  resetUsageV2MockState();
  tauriInvokeMock.mockClear();
  defaultImpl = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
});

afterEach(() => {
  if (defaultImpl) tauriInvokeMock.mockImplementation(defaultImpl);
});

describe("Task 13 · 记录页渠道绑定", () => {
  it("1. project 卡查询 identities；三种行状态/标记、同名多候选不合并且不自动选中", async () => {
    const restore = override({
      usage_records: () => {
        const base = mockUsageRecords(RANGE, "project", {});
        const card = base.cards[0];
        const third = { ...card.rows[0], key: "third", label: "claude-haiku-4-5" };
        return Promise.resolve({
          ...base,
          cards: [
            {
              ...card,
              toolId: "multiagents-manager",
              toolLabel: "MultiAgents-Manager",
              rows: [card.rows[0], card.rows[1], third],
            },
          ],
        });
      },
      usage_identities: () =>
        Promise.resolve([
          // 同名两候选：一个已归属、一个含糊
          identity("codex", "claude-sonnet-4-5", "profile-a", "userAssigned"),
          identity("claude", "claude-sonnet-4-5", "profile-mixed", "ambiguous"),
          // 另一个已归属行：同名两候选（同渠道、都 userAssigned）
          identity("codex", "claude-opus-4-1", "profile-b", "userAssigned"),
          identity("claude", "claude-opus-4-1", "profile-b", "userAssigned"),
          // 不匹配任何行的候选（用于验证 label 匹配分支）
          identity("codex", "other-model"),
        ]),
    });

    render(wrap(<UsageDashboardPage />, makeClient()));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    const card = await screen.findByTestId("usage-record-card-multiagents-manager");

    // row0 = claude-sonnet-4-5（同名两候选，其中一条 ambiguous）⇒ 含糊 + 「绑定计费归属」
    const ambiguousRow = within(card).getByTestId("usage-record-row-multiagents-manager:0");
    expect(within(ambiguousRow).getByTestId("usage-record-attr-ambiguous")).toHaveTextContent(
      "含糊"
    );
    const assignButton = within(ambiguousRow).getByTestId(
      "usage-record-assign-multiagents-manager:0"
    );
    expect(assignButton).toHaveTextContent("绑定计费归属");
    // 旧逐行入口（指定渠道 / 修改关联 / 查看规则）已收敛，不再逐行露出
    expect(ambiguousRow).not.toHaveTextContent("指定渠道");
    expect(ambiguousRow).not.toHaveTextContent("修改关联");
    expect(ambiguousRow).not.toHaveTextContent("查看规则");

    // row1 = claude-opus-4-1（已归属）⇒ 用户指定标记；普通行不再有绑定入口
    const assignedRow = within(card).getByTestId("usage-record-row-multiagents-manager:1");
    expect(within(assignedRow).getByTestId("usage-record-attr-assigned")).toHaveTextContent(
      "用户指定"
    );
    expect(within(assignedRow).queryByTestId(/^usage-record-assign-/)).toBeNull();
    expect(assignedRow).not.toHaveTextContent("修改关联");
    expect(assignedRow).not.toHaveTextContent("查看规则");
    // 所有行都保留「供应商显示名」入口
    expect(
      within(assignedRow).getByTestId("usage-record-alias-multiagents-manager:1")
    ).toHaveTextContent("供应商显示名");

    // row2 = claude-haiku-4-5（无身份映射）⇒ 无标记、无绑定入口
    const unassignedRow = within(card).getByTestId("usage-record-row-multiagents-manager:2");
    expect(within(unassignedRow).queryByTestId(/^usage-record-attr-/)).toBeNull();
    expect(within(unassignedRow).queryByTestId(/^usage-record-assign-/)).toBeNull();
    expect(unassignedRow).not.toHaveTextContent("指定渠道");
    expect(
      within(unassignedRow).getByTestId("usage-record-alias-multiagents-manager:2")
    ).toHaveTextContent("供应商显示名");

    // 点击含糊行的「绑定计费归属」：只保留 label 匹配的 2 条候选（不匹配的 other-model 被过滤），
    // 且多候选时没有任何候选项被自动选中。
    fireEvent.click(assignButton);
    await waitFor(() => expect(screen.getByTestId("billing-candidate-0")).toBeTruthy());
    expect(screen.getByTestId("billing-candidate-1")).toBeTruthy();
    expect(screen.queryByTestId("billing-candidate-2")).toBeNull();
    expect(screen.getByTestId("billing-candidate-0")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("billing-candidate-1")).toHaveAttribute("aria-pressed", "false");
    // 每条候选都展示当前归属 + Token（不只选中项）
    expect(screen.getByTestId("billing-candidate-0")).toHaveTextContent("profile-a");
    expect(screen.getByTestId("billing-candidate-1")).toHaveTextContent("profile-mixed");
    expect(screen.getByTestId("billing-candidate-0")).toHaveTextContent("140");
    expect(screen.getByTestId("billing-candidate-1")).toHaveTextContent("140");

    const call = callsOf("usage_identities").at(-1)!;
    expect(call[1]?.filters).toMatchObject({ projects: ["multiagents-manager"] });
    expect((call[1]?.filters as Record<string, unknown>).toolIds).toBeUndefined();

    restore();
  });

  it("1b. tool 卡：跨工具同 label 不互相覆盖（claude 已归属、codex 未归属）；observed 显示日志实测", async () => {
    const restore = override({
      usage_identities: () =>
        Promise.resolve([
          identity("claude", "claude-sonnet-4-5", "profile-a", "userAssigned"),
          identity("codex", "claude-sonnet-4-5", null, "unknown"),
          identity("zcode", "claude-opus-4-1", "profile-z", "observed"),
        ]),
    });

    render(wrap(<UsageDashboardPage />, makeClient()));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    await screen.findByTestId("usage-record-card-claude");

    const claudeRow = screen.getByTestId("usage-record-row-claude:0");
    const codexRow = screen.getByTestId("usage-record-row-codex:0");
    // 同一 label 在 claude 卡已归属，但 codex 卡仍必须是未归属；普通行不再有绑定入口
    expect(within(claudeRow).getByTestId("usage-record-attr-assigned")).toHaveTextContent(
      "用户指定"
    );
    expect(within(claudeRow).queryByTestId(/^usage-record-assign-/)).toBeNull();
    expect(claudeRow).not.toHaveTextContent("修改关联");
    expect(within(codexRow).queryByTestId(/^usage-record-attr-/)).toBeNull();
    expect(within(codexRow).queryByTestId(/^usage-record-assign-/)).toBeNull();
    expect(codexRow).not.toHaveTextContent("指定渠道");
    // 归属三态不改变「供应商显示名」入口：所有行都可编辑
    expect(within(claudeRow).getByTestId("usage-record-alias-claude:0")).toHaveTextContent(
      "供应商显示名"
    );
    expect(within(codexRow).getByTestId("usage-record-alias-codex:0")).toHaveTextContent(
      "供应商显示名"
    );

    // observed → 「日志实测」；标记配色使用主题语义类，不写死 emerald/amber
    const observedRow = screen.getByTestId("usage-record-row-zcode:1");
    const observed = within(observedRow).getByTestId("usage-record-attr-observed");
    expect(observed).toHaveTextContent("日志实测");
    expect(observed.className).not.toMatch(/emerald|amber/);
    expect(within(claudeRow).getByTestId("usage-record-attr-assigned").className).not.toMatch(
      /emerald|amber/
    );
    expect(within(claudeRow).getByTestId("usage-record-attr-assigned").className).toMatch(
      /text-primary|text-foreground/
    );

    restore();
  });

  it("1c. project 卡：同 label 跨 profile → 含糊；assigned + unknown 混合 → 含糊", async () => {
    let restore = override({
      usage_identities: () =>
        Promise.resolve([
          identity("codex", "claude-sonnet-4-5", "profile-a", "userAssigned"),
          identity("claude", "claude-sonnet-4-5", "profile-b", "userAssigned"),
        ]),
    });

    render(wrap(<UsageDashboardPage />, makeClient()));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    await screen.findByTestId("usage-record-card-multiagents-manager");
    let row = screen.getByTestId("usage-record-row-multiagents-manager:0");
    expect(within(row).getByTestId("usage-record-attr-ambiguous")).toHaveTextContent("含糊");
    // 含糊行是唯一保留绑定入口的状态
    expect(within(row).getByTestId("usage-record-assign-multiagents-manager:0")).toHaveTextContent(
      "绑定计费归属"
    );
    expect(row).not.toHaveTextContent("修改关联");
    restore();

    // 同一 label 一条 assigned + 一条 unknown ⇒ 含糊（不能因 assigned 把未归属也标成已归属）
    cleanup();
    restore = override({
      usage_identities: () =>
        Promise.resolve([
          identity("codex", "claude-sonnet-4-5", "profile-a", "userAssigned"),
          identity("claude", "claude-sonnet-4-5", null, "unknown"),
        ]),
    });
    render(wrap(<UsageDashboardPage />, makeClient()));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    await screen.findByTestId("usage-record-card-multiagents-manager");
    row = screen.getByTestId("usage-record-row-multiagents-manager:0");
    expect(within(row).getByTestId("usage-record-attr-ambiguous")).toHaveTextContent("含糊");
    expect(within(row).queryByTestId("usage-record-attr-assigned")).toBeNull();
    restore();
  });

  it("1b. 绑定浮窗候选 Token 使用万/亿缩写（不出现原始长数字）", async () => {
    const big = identity("codex");
    big.metrics = { ...big.metrics, requestTotal: 1_954_268 };
    big.buckets = { ...big.buckets, output: 82_713 };
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[big]}
          range={RANGE}
          t={tf}
        />
      )
    );
    const candidate = await screen.findByTestId("billing-candidate-0");
    expect(candidate).toHaveTextContent("203.70万");
    expect(candidate).not.toHaveTextContent("2,036,981");
  });

  it("2. 只读身份 + 默认 exact/今后；预览前不能保存；候选变化清空预览；保存用 preview revision", async () => {
    const onOpenChange = vi.fn();
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={onOpenChange}
          identities={[identity("codex")]}
          range={RANGE}
          t={tf}
        />
      )
    );

    expect(await screen.findByTestId("billing-profile-select")).toBeTruthy();
    const readonly = screen.getByTestId("billing-readonly-identity");
    expect(readonly).toHaveTextContent("Codex");
    expect(readonly).toHaveTextContent("—");
    expect(readonly).toHaveTextContent("claude-sonnet-4-5");
    expect(readonly).toHaveTextContent("未指定渠道");
    expect(screen.getByTestId("billing-match-mode")).toHaveValue("exact");
    expect(screen.getByText(/生效时间（只读）：/)).toBeTruthy();

    expect(screen.getByTestId("billing-save-button")).toBeDisabled();
    expect(screen.getByTestId("billing-preview-button")).toBeDisabled();
    expect(screen.getByText("渠道名称不能为空")).toBeTruthy();

    fireEvent.change(screen.getByTestId("billing-new-profile-name"), {
      target: { value: "Team A" },
    });
    expect(screen.getByTestId("billing-preview-button")).toBeEnabled();

    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");
    expect(screen.getByTestId("billing-preview")).toHaveTextContent("命中 Token");
    expect(screen.getByTestId("billing-save-button")).toBeEnabled();

    fireEvent.change(screen.getByTestId("billing-new-profile-name"), {
      target: { value: "Team A renamed" },
    });
    expect(screen.queryByTestId("billing-preview")).toBeNull();
    expect(screen.getByTestId("billing-save-button")).toBeDisabled();

    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");
    fireEvent.click(screen.getByTestId("billing-save-button"));
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));

    const commits = callsOf("usage_billing_commit");
    expect(commits).toHaveLength(1);
    const request = commits[0][1]?.request as {
      expectedRevision: number;
      candidate: BillingConfig;
    };
    expect(request.expectedRevision).toBe(0);
    expect(request.candidate.profiles.some((p) => p.name === "Team A renamed")).toBe(true);
    expect(request.candidate.rules[0].identity).toEqual({
      sourceId: "codex",
      observedProvider: null,
      modelId: "claude-sonnet-4-5",
    });
  });

  it("3. 保存冲突：保留草稿、清空 preview、保存禁用；重新预览后可保存", async () => {
    const restore = override({
      usage_billing_commit: () =>
        Promise.reject({ code: "usage-settings-invalid", detail: "billing revision conflict" }),
    });
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[identity("codex")]}
          range={RANGE}
          t={tf}
        />
      )
    );

    await screen.findByTestId("billing-profile-select");
    const getsBefore = callsOf("usage_billing_get").length;
    fireEvent.change(screen.getByTestId("billing-new-profile-name"), {
      target: { value: "Draft channel" },
    });
    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");
    fireEvent.click(screen.getByTestId("billing-save-button"));

    const error = await screen.findByTestId("billing-error");
    expect(error).toHaveTextContent("billing revision conflict");
    expect(screen.getByTestId("billing-new-profile-name")).toHaveValue("Draft channel");
    // 冲突后旧 preview 失效：保存禁用，必须重新预览
    expect(screen.queryByTestId("billing-preview")).toBeNull();
    expect(screen.getByTestId("billing-save-button")).toBeDisabled();
    expect(screen.getByText(/修改任何候选或规则后需重新预览/)).toBeTruthy();
    await waitFor(() => expect(callsOf("usage_billing_get").length).toBeGreaterThan(getsBefore));

    // 重新预览后保存按钮恢复可用
    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");
    expect(screen.getByTestId("billing-save-button")).toBeEnabled();

    restore();
  });

  it("3b. 历史日期：勾选/改日期都会清空旧预览，需二次预览", async () => {
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[identity("codex")]}
          range={RANGE}
          t={tf}
        />
      )
    );
    await screen.findByTestId("billing-profile-select");
    fireEvent.change(screen.getByTestId("billing-new-profile-name"), {
      target: { value: "History channel" },
    });

    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");

    fireEvent.click(screen.getByTestId("billing-historical"));
    expect(screen.queryByTestId("billing-preview")).toBeNull();
    expect(screen.getByTestId("billing-save-button")).toBeDisabled();

    fireEvent.change(screen.getByTestId("billing-historical-date"), {
      target: { value: "2026-09-01" },
    });
    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");
    expect(screen.getByTestId("billing-save-button")).toBeEnabled();

    // 再改日期 → 旧 preview 再次失效
    fireEvent.change(screen.getByTestId("billing-historical-date"), {
      target: { value: "2026-09-02" },
    });
    expect(screen.queryByTestId("billing-preview")).toBeNull();
    expect(screen.getByTestId("billing-save-button")).toBeDisabled();
  });

  it("3c. 打开期间后台 refetch（identities 引用变化）不清空已选候选与预览", async () => {
    const client = makeClient();
    const candidates = [identity("codex"), identity("claude")];
    const view = render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={candidates}
          range={RANGE}
          t={tf}
        />,
        client
      )
    );

    await screen.findByTestId("billing-candidate-1");
    fireEvent.click(screen.getByTestId("billing-candidate-1"));
    await screen.findByTestId("billing-profile-select");
    fireEvent.change(screen.getByTestId("billing-new-profile-name"), {
      target: { value: "Refetch channel" },
    });
    fireEvent.click(screen.getByTestId("billing-preview-button"));
    await screen.findByTestId("billing-preview");

    // 后台 refetch：内容相同、数组引用不同（重挂不能发生，故复用同一 QueryClient）
    view.rerender(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[...candidates]}
          range={RANGE}
          t={tf}
        />,
        client
      )
    );

    expect(screen.getByTestId("billing-candidate-1")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("billing-preview")).toBeTruthy();
    expect(screen.getByTestId("billing-save-button")).toBeEnabled();
  });

  it("4. archived 渠道显示但禁用选择", async () => {
    const config: BillingConfig = configOf(1, [
      { id: "p-arch", name: "Old channel", kind: "api", note: "", archived: true },
    ]);
    const restore = override({ usage_billing_get: () => Promise.resolve(config) });
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[identity("codex")]}
          range={RANGE}
          t={tf}
        />
      )
    );

    const select = await screen.findByTestId("billing-profile-select");
    const option = select.querySelector("option[value='p-arch']") as HTMLOptionElement;
    expect(option.disabled).toBe(true);
    expect(select).toHaveValue("__new__");

    restore();
  });

  it("4b. billing 未就绪时打开：数据到达后默认渠道自动纠正为第一个非归档；手选不被覆盖", async () => {
    const config: BillingConfig = configOf(1, [
      { id: "p1", name: "P1", kind: "api", note: "", archived: false },
      { id: "p2", name: "P2", kind: "api", note: "", archived: false },
    ]);
    let resolveBilling: (value: BillingConfig) => void = () => {};
    // 首个调用挂起（模拟 pending）；release 后**后续 refetch 调用**立即返回 config，
    // 否则 refetch 的新 promise 无人 resolve，invalidateQueries 会永久挂起。
    let released = false;
    const restore = override({
      usage_billing_get: () =>
        new Promise<BillingConfig>((resolve) => {
          if (released) resolve(config);
          else resolveBilling = resolve;
        }),
    });
    const client = makeClient();
    render(
      wrap(
        <BillingAssignmentDialog
          open
          onOpenChange={vi.fn()}
          identities={[identity("codex")]}
          range={RANGE}
          t={tf}
        />,
        client
      )
    );

    // billing pending：渠道选择尚未渲染，绝不先落「新建渠道」
    expect(screen.getByText("加载中…")).toBeTruthy();
    expect(screen.queryByTestId("billing-profile-select")).toBeNull();

    // 数据到达：默认渠道自动纠正为第一个非归档渠道（而非卡死在「新建渠道」）
    released = true;
    resolveBilling(config);
    const select = await screen.findByTestId("billing-profile-select");
    await waitFor(() => expect(select).toHaveValue("p1"));

    // 用户手选 p2 后，后台 refetch（["usage"] 失效链 → data 引用变化）不得覆盖手选
    fireEvent.change(select, { target: { value: "p2" } });
    expect(select).toHaveValue("p2");
    const getsBefore = callsOf("usage_billing_get").length;
    await client.invalidateQueries({ queryKey: ["usage"] });
    await waitFor(() => expect(callsOf("usage_billing_get").length).toBeGreaterThan(getsBefore));
    expect(screen.getByTestId("billing-profile-select")).toHaveValue("p2");

    restore();
  });

  it("5. Manager 撤销产生新 revision（0 = 空配置）", async () => {
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: configOf(0, [{ id: "p1", name: "P1", kind: "api", note: "", archived: false }]),
    });
    await usageBillingCommit({
      expectedRevision: 1,
      candidate: configOf(1, [{ id: "p1", name: "P1", kind: "api", note: "", archived: false }]),
    });

    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    expect(await screen.findByTestId("billing-revision")).toHaveTextContent("当前版本：2");

    fireEvent.change(screen.getByTestId("billing-undo-target"), { target: { value: "0" } });
    fireEvent.click(screen.getByTestId("billing-undo"));

    await waitFor(() => expect(callsOf("usage_billing_undo")).toHaveLength(1));
    expect(callsOf("usage_billing_undo")[0][1]).toEqual({
      expectedRevision: 2,
      targetRevision: 0,
    });
    await waitFor(() =>
      expect(screen.getByTestId("billing-revision")).toHaveTextContent("当前版本：3")
    );
  });

  it("5b. Manager 关闭期间不发起 last7d identities 查询，打开后才发起", async () => {
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: configOf(0, [{ id: "p1", name: "P1", kind: "api", note: "", archived: false }]),
    });

    render(wrap(<BillingProfilesManager t={tf} />));
    await screen.findByTestId("billing-manager-open");
    // billing 配置查询照常发起，但 7 天账本 identities 查询被「打开」门控拦住
    await waitFor(() => expect(callsOf("usage_billing_get").length).toBeGreaterThan(0));
    expect(callsOf("usage_identities")).toHaveLength(0);

    fireEvent.click(screen.getByTestId("billing-manager-open"));
    await waitFor(() => expect(callsOf("usage_identities").length).toBeGreaterThan(0));
    expect(callsOf("usage_identities")[0][1]?.range).toMatchObject({ preset: "last7d" });
  });

  it("6. Manager 渠道名受控：提交失败回滚旧值，撤销/重拉后刷新显示", async () => {
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: configOf(0, [
        { id: "p1", name: "OldName", kind: "api", note: "", archived: false },
      ]),
    });
    await usageBillingCommit({
      expectedRevision: 1,
      candidate: configOf(1, [
        { id: "p1", name: "Renamed", kind: "api", note: "", archived: false },
      ]),
    });

    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    const nameInput = await screen.findByTestId("billing-profile-name-p1");
    expect(nameInput).toHaveValue("Renamed");

    // 提交失败 → 立即回滚到服务端旧值
    const restore = override({
      usage_billing_commit: () =>
        Promise.reject({ code: "usage-settings-invalid", detail: "boom" }),
    });
    fireEvent.change(nameInput, { target: { value: "ShouldNotStick" } });
    fireEvent.blur(nameInput);
    await screen.findByTestId("billing-manager-error");
    expect(screen.getByTestId("billing-profile-name-p1")).toHaveValue("Renamed");
    restore();

    // 撤销到 revision 1 后，受控输入随重拉刷新为 OldName
    fireEvent.change(screen.getByTestId("billing-undo-target"), { target: { value: "1" } });
    fireEvent.click(screen.getByTestId("billing-undo"));
    await waitFor(() =>
      expect(screen.getByTestId("billing-profile-name-p1")).toHaveValue("OldName")
    );
  });

  it("6b. 后台 refetch（revision 未变）不清空输入中的改名草稿", async () => {
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: configOf(0, [{ id: "p1", name: "P1", kind: "api", note: "", archived: false }]),
    });
    const client = makeClient();
    render(wrap(<BillingProfilesManager t={tf} />, client));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    const nameInput = await screen.findByTestId("billing-profile-name-p1");
    expect(nameInput).toHaveValue("P1");

    // 模拟采集 ticker 失效链：invalidate → refetch → data 引用变化但 revision 仍为 1
    fireEvent.change(nameInput, { target: { value: "正在输入的新名" } });
    await client.invalidateQueries({ queryKey: ["usage"] });
    // revision 门控生效：草稿不被重建回服务端值
    expect(screen.getByTestId("billing-profile-name-p1")).toHaveValue("正在输入的新名");
  });

  it("7. Manager 新建 / 改名 / 归档 / 规则编辑", async () => {
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: configOf(
        0,
        [
          { id: "p1", name: "P1", kind: "api", note: "", archived: false },
          { id: "p2", name: "P2", kind: "api", note: "", archived: false },
        ],
        [rule("r1", "p1")]
      ),
    });

    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    // 新建
    fireEvent.change(screen.getByTestId("billing-new-profile"), {
      target: { value: "Team X" },
    });
    fireEvent.click(screen.getByTestId("billing-create-profile"));
    await waitFor(() => expect(screen.getByDisplayValue("Team X")).toBeTruthy());

    // 改名
    const p1Name = screen.getByTestId("billing-profile-name-p1");
    fireEvent.change(p1Name, { target: { value: "P1 renamed" } });
    fireEvent.blur(p1Name);
    await waitFor(() =>
      expect(screen.getByTestId("billing-profile-name-p1")).toHaveValue("P1 renamed")
    );

    // 归档
    fireEvent.click(screen.getByTestId("billing-profile-archive-p1"));
    await waitFor(() =>
      expect(screen.getByTestId("billing-profile-p1")).toHaveTextContent("已归档")
    );

    // 规则编辑：目标 / 匹配方式 / 日期 / 停用
    fireEvent.change(screen.getByTestId("billing-rule-target-r1"), {
      target: { value: "p2" },
    });
    await waitFor(() => expect(screen.getByTestId("billing-rule-target-r1")).toHaveValue("p2"));
    fireEvent.change(screen.getByTestId("billing-rule-match-r1"), {
      target: { value: "prefix" },
    });
    await waitFor(() => expect(screen.getByTestId("billing-rule-match-r1")).toHaveValue("prefix"));
    fireEvent.change(screen.getByTestId("billing-rule-from-r1"), {
      target: { value: "2026-09-01" },
    });
    await waitFor(() =>
      expect(screen.getByTestId("billing-rule-from-r1")).toHaveValue("2026-09-01")
    );

    fireEvent.click(screen.getByTestId("billing-rule-remove-r1"));
    await waitFor(() => expect(screen.queryByTestId("billing-rule-r1")).toBeNull());
  });
});

describe("Task 13 · ruleIssues 与后端归因语义对齐", () => {
  it("8. 同一 identity 两条不重叠生效区间不报冲突", () => {
    const issues = ruleIssues([
      rule("a", "p1", "exact", 0, 1_000),
      rule("b", "p2", "exact", 1_000, 2_000),
    ]);
    expect(issues.conflicts).toHaveLength(0);
  });

  it("9. exact + prefix 不同渠道不报冲突（不同 rank）", () => {
    const issues = ruleIssues([
      rule("a", "p1", "exact", 0, null),
      rule("b", "p2", "prefix", 0, null),
    ]);
    expect(issues.conflicts).toHaveLength(0);
  });

  it("10. 同 rank 重叠且不同渠道报冲突；严格更窄区间支配不报冲突", () => {
    const overlap = ruleIssues([
      rule("a", "p1", "exact", 0, 2_000),
      rule("b", "p2", "exact", 1_000, 3_000),
    ]);
    expect(overlap.conflicts).toHaveLength(1);

    const contained = ruleIssues([
      rule("a", "p1", "exact", 0, 3_000),
      rule("b", "p2", "exact", 1_000, 2_000),
    ]);
    expect(contained.conflicts).toHaveLength(0);
  });

  it("11. mixed 规则仍标记含糊", () => {
    const issues = ruleIssues([rule("a", "p1", "exact", 0, null, true)]);
    expect(issues.ambiguous).toHaveLength(1);
  });
});
