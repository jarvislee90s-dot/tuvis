// 记录页「供应商别名浮窗」（`src/components/usage/ProviderAliasDialog.tsx` + 宿主
// `UsageRecordsPanel.tsx`）——从被删除的 `v2-provider-alias-ui.test.tsx`（基线 19e789c0
// 用例 7–13）恢复的覆盖；2026-10-20 计划 §4.2「records 页浮窗不动」。
//
// 覆盖：
//  * 每行按钮打开浮窗、只读原始身份；
//  * 空 provider 构造 `kind=toolModel`、非空 provider 构造 `kind=provider`；
//  * 已有同键规则预填显示名并复用 id；
//  * 同名多候选必须显式选择（不静默选第一项），保存用被选身份的完整键；
//  * 保存前命中预览（近 30 天同源查询）：provider 规则跨工具/模型、toolModel 规则不掺
//    其它身份、Token 万/亿缩写、范围口径明示；
//  * 显式绑定行保留可点开的别名入口 + 浮窗内可见提示；ambiguous 行不视为显式绑定；
//  * 保存成功关闭浮窗并失效 `USAGE_KEY`（别名查询重取），同一窗口内记录页行名立即改为
//    「别名 / 原始模型」，`title` 仍保留原始标签；
//  * project 卡同一 label 跨工具映射出多个显示名时，行名保守回退原始 label（不猜）。
//
// 纪律：不恢复已退役的设置页 ProviderAliasManager 用例（其管理面已由
// `v2-config-dialog.test.tsx` 的 A/B 段接管）；只消费既有 i18n 键与 mock 命令，
// 不改 locale / fixtures / types；断言行为级（testid + 可见文案 + IPC 入参）。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（主题判定 / WindowFrame 需要；与其余 usage 测试同模式）
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
import UsageDashboardPage from "@/pages/usage-dashboard";
import {
  mockProviderAliasesGet,
  mockProviderAliasUpsert,
  mockUsageRecords,
  resetUsageV2MockState,
} from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { UsageRange, UsageSourceId } from "@/types/usage";
import type { IdentityUsage, ProviderAliasConfig, ProviderAliasRule } from "@/types/usageV2";

const RANGE: UsageRange = { preset: "last7d" };

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(children: ReactNode, client = makeClient()) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
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

function identity(
  sourceId: UsageSourceId,
  observedProvider: string | null,
  modelId: string,
  requestTotal = 100,
  output = 40
): IdentityUsage {
  return {
    identity: { sourceId, observedProvider, modelId },
    sourceKind: "measured",
    attribution: {
      billingProfileId: null,
      attributionKind: "unknown",
      ruleIds: [],
      reason: "unassigned",
    },
    buckets: { inputFresh: 80, cacheRead: 20, cacheWrite: 0, output },
    metrics: { requestTotal, cacheHitRate: 0.2, userEst: 12, requests: 3 },
  };
}

function aliasRule(
  id: string,
  kind: ProviderAliasRule["kind"],
  provider: string | null,
  sourceId: UsageSourceId | null,
  modelId: string | null,
  displayName: string
): ProviderAliasRule {
  return { id, kind, provider, sourceId, modelId, displayName };
}

function aliasConfig(aliases: ProviderAliasRule[]): ProviderAliasConfig {
  return { aliases };
}

/** 记录页：渲染整页并切到「记录」页签（单卡夹具下即 codex 卡） */
async function openRecords() {
  render(wrap(<UsageDashboardPage />));
  fireEvent.click(await screen.findByTestId("usage-tab-records"));
  return await screen.findByTestId("usage-record-card-codex");
}

/** 把 codex 卡替换为单行夹具（行标签可控，便于按完整身份对齐） */
function recordsWithCodexLabel(label: string) {
  const base = mockUsageRecords(RANGE, "tool", {});
  const card = base.cards.find((c) => c.toolId === "codex")!;
  return {
    ...base,
    cards: [{ ...card, rows: [{ ...card.rows[0], label }] }],
  };
}

/** 命中预览查询口径（与设置页同源）：近 30 天 + 空筛选（不受记录页筛选/范围影响） */
function hasLast30dIdentitiesCall(): boolean {
  return callsOf("usage_identities").some(
    (c) =>
      JSON.stringify(c[1]?.range) === JSON.stringify({ preset: "last30d" }) &&
      JSON.stringify(c[1]?.filters) === "{}"
  );
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

describe("记录页 · 供应商别名浮窗", () => {
  it("7. 行按钮打开浮窗；只读身份 + 空 provider 保存 toolModel；行名改为「别名 / 模型」，title 保留原始标签", async () => {
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("claude-sonnet-4-5")),
      usage_identities: () => Promise.resolve([identity("codex", null, "claude-sonnet-4-5")]),
    });

    await openRecords();
    const before = callsOf("usage_provider_aliases_get").length;
    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    await screen.findByTestId("alias-readonly-identity");

    const readonly = screen.getByTestId("alias-readonly-identity");
    expect(readonly).toHaveTextContent(i18n.t("usage.alias.sourceTool"));
    expect(readonly).toHaveTextContent("Codex");
    expect(readonly).toHaveTextContent(i18n.t("usage.alias.provider"));
    expect(readonly).toHaveTextContent("—"); // 原始 provider 为空
    expect(readonly).toHaveTextContent("claude-sonnet-4-5");
    expect(readonly).toHaveTextContent(i18n.t("usage.alias.current"));
    expect(screen.getByTestId("alias-scope-hint")).toHaveTextContent(
      i18n.t("usage.alias.toolModelScopeHint")
    );

    fireEvent.change(screen.getByTestId("alias-display-name"), {
      target: { value: "我的 Claude" },
    });
    fireEvent.click(screen.getByTestId("alias-save-button"));

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_upsert")[0][1].rule).toMatchObject({
      kind: "toolModel",
      provider: null,
      sourceId: "codex",
      modelId: "claude-sonnet-4-5",
      displayName: "我的 Claude",
    });

    // 保存成功：浮窗关闭，USAGE_KEY 失效 → 别名查询重取
    await waitFor(() => expect(screen.queryByTestId("alias-save-button")).toBeNull());
    await waitFor(() =>
      expect(callsOf("usage_provider_aliases_get").length).toBeGreaterThan(before)
    );

    // 同一窗口内（无重新挂载）行名立即改为「别名 / 原始模型」；原始账本标签留在 title 里。
    // 这是同一条 ["usage"] 失效链：别名查询与记录页行名共用一次重取。
    const row = screen.getByTestId("usage-record-row-codex:0");
    await waitFor(() => expect(row).toHaveTextContent("我的 Claude / claude-sonnet-4-5"));
    expect(within(row).getByTitle("claude-sonnet-4-5")).toBeTruthy();
    // 已有别名后按钮切换为「编辑」
    await waitFor(() =>
      expect(screen.getByTestId("usage-record-alias-codex:0")).toHaveTextContent(
        i18n.t("usage.alias.edit")
      )
    );

    restore();
  });

  it("7b. project 卡同一 label 跨工具映射出多个显示名 → 行名保守回退原始 label（不猜）", async () => {
    // 同一 project 卡内，两个工具各有一条空 provider、同模型的账本行：
    // codex 挂 toolModel 别名（`Codex Team / shared-model`），claude 未挂（裸 `shared-model`）
    // ⇒ 同一 row.label 映射出两个显示名 → displayLabelOf 必须回退原始 label，而不是任选一个。
    const MODEL = "shared-model";
    await mockProviderAliasUpsert(
      aliasRule("alias-codex-shared", "toolModel", null, "codex", MODEL, "Codex Team")
    );
    const base = mockUsageRecords(RANGE, "project", {});
    const projectCard = base.cards[0]; // multiagents-manager
    const projectRecords = {
      ...base,
      cards: [
        {
          ...projectCard,
          rows: [{ ...projectCard.rows[0], key: "shared:0", label: MODEL }],
        },
      ],
    };
    const restore = override({
      usage_records: () => Promise.resolve(projectRecords),
      usage_identities: () =>
        Promise.resolve([
          identity("codex", null, MODEL, 100, 40),
          identity("claude", null, MODEL, 50, 10),
        ]),
    });

    render(wrap(<UsageDashboardPage />));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    fireEvent.click(screen.getByTestId("usage-records-dim-project"));
    const card = await screen.findByTestId("usage-record-card-multiagents-manager");

    // 等 identities 与别名查询都落地：project 卡按 label 命中别名 → 按钮变「编辑」。
    // 这一步同时排除「数据未到、拿空列表回退原始 label」的假绿。
    await waitFor(() =>
      expect(
        within(card).getByTestId("usage-record-alias-multiagents-manager:0")
      ).toHaveTextContent(i18n.t("usage.alias.edit"))
    );

    const row = within(card).getByTestId("usage-record-row-multiagents-manager:0");
    // 行名 span（title = 原始 label）必须逐字等于原始裸模型名，不采用任何别名
    const label = within(row).getByTitle(MODEL);
    expect(label.textContent).toBe(MODEL);
    expect(label).toHaveAttribute("title", MODEL);
    expect(row).not.toHaveTextContent("Codex Team");

    restore();
  });

  it("8. 非空 provider 构造 provider 规则；已有同键规则预填并复用 id；保存后行名立即更新", async () => {
    // 用可变 mock 状态而非静态 get 覆盖：保存后的 ["usage"] 失效重取要能读到新显示名。
    await mockProviderAliasUpsert(
      aliasRule("alias-openrouter", "provider", "openrouter", null, null, "旧显示名")
    );
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("openrouter / gpt-5")),
      usage_identities: () => Promise.resolve([identity("codex", "openrouter", "gpt-5")]),
      usage_provider_aliases_get: () => Promise.resolve(mockProviderAliasesGet()),
    });

    await openRecords();
    // 已有同 provider 别名：按钮即「编辑」，行名 = `旧显示名 / gpt-5`，title 保留原始标签
    expect(screen.getByTestId("usage-record-alias-codex:0")).toHaveTextContent(
      i18n.t("usage.alias.edit")
    );
    const row = screen.getByTestId("usage-record-row-codex:0");
    expect(row).toHaveTextContent("旧显示名 / gpt-5");
    expect(within(row).getByTitle("openrouter / gpt-5")).toBeTruthy();

    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    const readonly = await screen.findByTestId("alias-readonly-identity");
    expect(readonly).toHaveTextContent("openrouter");
    expect(readonly).toHaveTextContent("旧显示名");
    expect(screen.getByTestId("alias-scope-hint")).toHaveTextContent(
      i18n.t("usage.alias.providerScopeHint")
    );
    expect((screen.getByTestId("alias-display-name") as HTMLInputElement).value).toBe("旧显示名");

    fireEvent.change(screen.getByTestId("alias-display-name"), {
      target: { value: "OpenRouter 新名" },
    });
    fireEvent.click(screen.getByTestId("alias-save-button"));

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_upsert")[0][1].rule).toMatchObject({
      id: "alias-openrouter",
      kind: "provider",
      provider: "openrouter",
      sourceId: null,
      modelId: null,
      displayName: "OpenRouter 新名",
    });

    // 同一窗口内保存后行名立即换成新别名；原始「openrouter / gpt-5」仍留在 title
    await waitFor(() =>
      expect(screen.getByTestId("usage-record-row-codex:0")).toHaveTextContent(
        "OpenRouter 新名 / gpt-5"
      )
    );
    expect(
      within(screen.getByTestId("usage-record-row-codex:0")).getByTitle("openrouter / gpt-5")
    ).toBeTruthy();

    restore();
  });

  it("9. 同名多候选必须显式选择，保存使用被选中身份的完整键", async () => {
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("claude-sonnet-4-5")),
      usage_identities: () =>
        Promise.resolve([
          identity("codex", null, "claude-sonnet-4-5"),
          identity("claude", null, "claude-sonnet-4-5"),
        ]),
    });

    await openRecords();
    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));

    // 两条同名候选：不自动选中，保存禁用
    const c0 = await screen.findByTestId("alias-candidate-0");
    const c1 = screen.getByTestId("alias-candidate-1");
    expect(c0).toHaveAttribute("aria-pressed", "false");
    expect(c1).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("alias-save-button")).toBeDisabled();
    expect(screen.getByText(i18n.t("usage.billing.selectCandidate"))).toBeTruthy();

    fireEvent.click(c1);
    expect(c1).toHaveAttribute("aria-pressed", "true");
    const readonly = await screen.findByTestId("alias-readonly-identity");
    expect(readonly).toHaveTextContent("Claude");

    fireEvent.change(screen.getByTestId("alias-display-name"), {
      target: { value: "Claude Code 订阅" },
    });
    fireEvent.click(screen.getByTestId("alias-save-button"));

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_upsert")[0][1].rule).toMatchObject({
      kind: "toolModel",
      provider: null,
      sourceId: "claude",
      modelId: "claude-sonnet-4-5",
      displayName: "Claude Code 订阅",
    });

    restore();
  });

  it("10. 保存前命中预览：provider 规则跨工具/模型、Token 合计与当前记录范围", async () => {
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("openrouter / gpt-5")),
      usage_provider_aliases_get: () => Promise.resolve(aliasConfig([])),
      usage_identities: () =>
        Promise.resolve([
          identity("codex", "openrouter", "gpt-5", 100, 40),
          identity("claude", "openrouter", "claude-sonnet-4-5", 50, 10),
          // 其它 provider 不应计入预览
          identity("codex", "other", "gpt-5", 999, 1),
        ]),
    });

    await openRecords();
    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    await screen.findByTestId("alias-readonly-identity");

    // 保存前即可见，不需要先保存
    expect(screen.getByTestId("alias-hit-preview")).toBeTruthy();
    // 命中预览来自异步 identities 查询（近 30 天）：必须等数据落地，
    // 否则 getByTestId 会读在 loading 窗（2026-10-19 最终回归实测确定性红）
    const tools = await screen.findByTestId("alias-hit-tools");
    expect(tools).toHaveTextContent("Codex");
    expect(tools).toHaveTextContent("Claude");
    const models = screen.getByTestId("alias-hit-models");
    expect(models).toHaveTextContent("gpt-5");
    expect(models).toHaveTextContent("claude-sonnet-4-5");
    expect(screen.getByTestId("alias-hit-tokens")).toHaveTextContent("200"); // 140 + 60
    expect(screen.getByTestId("alias-hit-range")).toHaveTextContent(
      i18n.t("usage.alias.hitRangeValue", { range: i18n.t("usage.alias.usageWindow") })
    );
    expect(screen.queryByTestId("alias-hit-empty")).toBeNull();
    // 预览与设置页同源：近 30 天 + 空筛选（不受记录页筛选/范围影响）
    expect(hasLast30dIdentitiesCall()).toBe(true);

    restore();
  });

  it("10b. 别名浮窗命中预览 Token 使用万/亿缩写", async () => {
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("openrouter / gpt-5")),
      usage_provider_aliases_get: () => Promise.resolve(aliasConfig([])),
      usage_identities: () =>
        Promise.resolve([identity("codex", "openrouter", "gpt-5", 1_954_268, 82_713)]),
    });

    await openRecords();
    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    await screen.findByTestId("alias-readonly-identity");
    const tokens = await screen.findByTestId("alias-hit-tokens");
    expect(tokens).toHaveTextContent("203.70万");
    expect(tokens).not.toHaveTextContent("2,036,981");

    restore();
  });

  it("11. toolModel 预览只命中同工具+模型，Token 合计不掺其它身份", async () => {
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("claude-sonnet-4-5")),
      usage_provider_aliases_get: () => Promise.resolve(aliasConfig([])),
      usage_identities: () =>
        Promise.resolve([
          identity("codex", null, "claude-sonnet-4-5", 100, 40),
          // 同工具的未知模型：toolModel 规则不得把它计入
          identity("codex", null, "", 7, 3),
        ]),
    });

    await openRecords();
    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    await screen.findByTestId("alias-readonly-identity");

    // 异步命中预览：等数据落地再断言（同上）
    expect(await screen.findByTestId("alias-hit-tools")).toHaveTextContent("Codex");
    expect(screen.getByTestId("alias-hit-tokens")).toHaveTextContent("140");
    expect(screen.getByTestId("alias-hit-models")).toHaveTextContent("claude-sonnet-4-5");
    expect(screen.getByTestId("alias-hit-models")).not.toHaveTextContent("未知模型");
    expect(hasLast30dIdentitiesCall()).toBe(true);

    restore();
  });

  it("12. 显式绑定计费渠道的行保留可点开的别名入口，并在浮窗内给出可见提示", async () => {
    const bound = identity("codex", "deepseek", "gpt-5", 100, 40);
    bound.attribution = {
      billingProfileId: "p1",
      attributionKind: "userAssigned",
      ruleIds: ["rule-1"],
      reason: null,
    };
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("deepseek / gpt-5")),
      usage_identities: () => Promise.resolve([bound]),
    });

    await openRecords();
    const button = screen.getByTestId("usage-record-alias-codex:0");
    // 不再用 disabled + title 藏提示：按钮可点开，提示在浮窗内可见。
    expect(button).not.toBeDisabled();
    expect(button).not.toHaveAttribute("title");

    fireEvent.click(button);
    await screen.findByTestId("alias-readonly-identity");
    expect(screen.getByTestId("alias-explicit-bound-hint")).toHaveTextContent(
      i18n.t("usage.alias.explicitBoundHint")
    );

    restore();
  });

  it("13. ambiguous 行（即使带 profileId）保留可用的别名入口，不视为显式绑定", async () => {
    const ambiguous = identity("codex", "mix", "gpt-5", 100, 40);
    ambiguous.attribution = {
      billingProfileId: "p1",
      attributionKind: "ambiguous",
      ruleIds: ["rule-1"],
      reason: "conflict",
    };
    const restore = override({
      usage_records: () => Promise.resolve(recordsWithCodexLabel("mix / gpt-5")),
      usage_identities: () => Promise.resolve([ambiguous]),
    });

    await openRecords();
    const button = screen.getByTestId("usage-record-alias-codex:0");
    expect(button).not.toBeDisabled();
    fireEvent.click(button);
    await screen.findByTestId("alias-readonly-identity");
    expect(screen.queryByTestId("alias-explicit-bound-hint")).toBeNull();

    restore();
  });
});
