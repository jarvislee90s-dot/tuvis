// 2026-10-20 计划 Task 6 · 显示与计价配置（「供应商/模型条目」单段 + 高级覆盖折叠 + 入口切换）。
//
// 2026-10-20 第三轮修订（同日用户裁决的最终形态）：设置页与 D 卡共用**同一个弹窗**
// UsageConfigDialog（受控 open / 自带 trigger），弹窗内容 = UsageConfigPanel——上半模型条目、
// 下半可折叠「高级覆盖」（默认收起，展开才挂载 PriceOverrideManager），标题/描述/InfoTip 在弹窗头。
// 弹窗只在打开时挂载 ⇒ observed entries / 覆盖价查询随打开态门控；关闭即卸载停发，重开才再拉。
// 入口：设置页 = usage-config-open-settings 触发按钮；D 卡 = usage-cost-manage（aria-haspopup=dialog）。
// BillingProfilesManager / 参考价规则不再在配置里（旧版入口只在「记录」页，配置里只剩一行脚注）。
// Info 图标（Radix Tooltip，role=tooltip）与条目快捷入口（覆盖价预填）保留。
//
// 覆盖：
//  * 入口与门控：设置页初始只有触发按钮（无 dialog、无 observed entries 查询）；点开出现
//    role=dialog 的 usage-config-dialog-settings 且恰发一次查询；Esc / 关闭按钮卸载弹窗、
//    staleTime 内重开不重发；D 卡 manage 按钮打开 usage-config-dialog-cost；两个实例各自
//    trigger 打开各自弹窗、壳 testid 不冲突；观测条目 / 别名读取失败可见可重试；
//  * 条目内供应商改名：普通 provider 全局生效 / 空 provider 写 toolModel / 清空删除回退 /
//    同 provider 多成员只写一次 / 失败可见可重试 / 重入守卫；
//  * Top10 默认 / 展开全部 / 搜索过滤 / 搜索无结果 / 收起清筛选；
//  * 条目 B 面：三行结构（row1 原始证据 / row2 双输入 / row3 计价身份）、展示键多成员折叠、
//    多成员批量写、复制原始模型名、清除自定义名、自动匹配徽标、仅下拉选择、
//    「显示全部 N 个来源」切换 scope、悬停价格表内容；
//  * 高级区：标题「自定义价格规则」、默认收起、展开才挂载 Manager、无 billing manager、
//    旧版绑定说明（legacyMoved / usage-config-legacy-info-*）已删除；
//  * Info tooltip：hover / focus 显示 role=tooltip 说明；
//  * 条目快捷入口：空模型 disabled / 新覆盖预填原始模型名 / 已有覆盖进入编辑态 / 覆盖徽标 /
//    scrollIntoView 滚动回归（prefill 被子组件清空不得取消 rAF；同毫秒重复点击重滚）。
//
// 纪律：只消费既有 hooks / mock / i18n 键；不新增 locale 键。
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import { UsageSection } from "@/components/settings/UsageSection";
import { UsageCostCard } from "@/components/usage/UsageCostCard";
import {
  mockPricingRefresh,
  mockPricingSetEnabled,
  mockProviderAliasesGet,
  resetUsageV2MockState,
} from "@/lib/usage/mockFixtures";
import { modelGroupsOf } from "@/components/usage/UsageConfigModelSection";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { ObservedModelEntry } from "@/types/usageV2";

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

function observedEntry(patch: Partial<ObservedModelEntry>): ObservedModelEntry {
  return {
    providerRaw: "openai",
    sourceId: null,
    modelId: "gpt-5",
    normalizedModelId: "gpt-5",
    displayProvider: null,
    displayModelName: null,
    pricingProvider: null,
    pricingModel: null,
    totalTokens: 100,
    recentTokens7d: 100,
    ...patch,
  };
}

/** 配置条目的 DOM testid（展示键 = 显示供应商 + 原始模型；空 provider 走 u- 前缀）。 */
function entry(provider: string, model: string): string {
  return `usage-config-model-entry-d-${provider}-${model}`;
}

/** mock 别名存储里的 provider 显示名（未配置 = 原始串）；观察条目夹具用它保持与真机解析一致 */
function providerDisplayOf(provider: string): string {
  const rule = mockProviderAliasesGet().aliases.find(
    (item) => item.kind === "provider" && item.provider === provider
  );
  return rule?.displayName ?? provider;
}

/**
 * 渲染设置页并点开配置弹窗（条目列表就绪）。
 * 竞态说明：条目列表要等 observed entries 与 aliases 两个查询都返回才渲染
 * （panel 只等 entries），所以这里必须等到第一条目/空态出现，后续 getBy 才不会抢跑。
 */
async function openConfigPanel(
  waitTestId: string | null = "usage-config-model-entry-d-OpenAI-gpt-5"
) {
  // 自动匹配 mock 与 quote 同款快照门控（无 active 快照 = kind=none）：
  // 真机流程里条目的自动匹配徽标需要有价目快照，这里先启用 + 刷新对齐。
  mockPricingSetEnabled(true);
  mockPricingRefresh();
  render(wrap(<UsageSection />));
  fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
  await screen.findByTestId("usage-config-dialog-settings");
  await screen.findByTestId("usage-config-panel-settings");
  if (waitTestId !== null) await screen.findByTestId(waitTestId);
}

/**
 * Esc 关闭当前打开的配置弹窗并等待卸载（弹窗内容只在打开时挂载 ⇒ 关闭 = 停发查询）。
 * 生产侧 `UsageConfigDialog` 的 onOpenAutoFocus 已把初始焦点交给弹窗标题（不再落到
 * Info 按钮上），所以这里直接按 Esc 即可，无需任何 blur 绕过/前置动作。
 */
async function closeDialogByEscape(instanceId: string) {
  fireEvent.keyDown(screen.getByTestId(`usage-config-dialog-${instanceId}`), { key: "Escape" });
  await waitFor(() => expect(screen.queryByTestId(`usage-config-dialog-${instanceId}`)).toBeNull());
  expect(screen.queryByRole("dialog")).toBeNull();
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

describe("入口与挂载门控（共用弹窗，打开态门控查询）", () => {
  it("1. 设置页初始只有触发按钮（无弹窗、无查询）；点开出现 role=dialog 且恰发一次 observed entries；Esc/关闭按钮卸载，staleTime 内重开不重发", async () => {
    render(wrap(<UsageSection />));

    // 关闭态：触发按钮在场（DialogTrigger 自带 aria-haspopup=dialog）；弹窗壳彻底不存在
    const trigger = await screen.findByTestId("usage-config-open-settings");
    expect(trigger).toHaveTextContent(i18n.t("usage.config.open"));
    expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
    expect(screen.queryByTestId("usage-config-dialog-settings")).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    // PricingStatusPanel 原位不动
    expect(await screen.findByTestId("usage-pricing-toggle")).toBeTruthy();
    // 弹窗不挂载面板：observed entries 不发、四价覆盖管理器不在场
    expect(callsOf("usage_observed_entries_get")).toHaveLength(0);
    expect(screen.queryByTestId("price-override-manager")).toBeNull();

    // 点开：dialog 挂载（role=dialog + 唯一 testid）+ 恰发一次 observed entries
    fireEvent.click(trigger);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveAttribute("data-testid", "usage-config-dialog-settings");
    // 标题/描述移到弹窗头（DialogTitle/DialogDescription）
    expect(within(dialog).getByText(i18n.t("usage.config.title"))).toBeTruthy();
    expect(within(dialog).getByText(i18n.t("usage.config.desc"))).toBeTruthy();
    await waitFor(() => expect(callsOf("usage_observed_entries_get")).toHaveLength(1));
    const section = await screen.findByTestId("usage-config-model-section");
    expect(section).toHaveTextContent(i18n.t("usage.config.modelSection.title"));
    expect(section).toHaveTextContent(i18n.t("usage.config.modelSection.desc"));
    // 初始条目就绪（默认夹具 4 条）
    expect(await screen.findByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeTruthy();

    // Esc 关闭：弹窗卸载、正文消失；staleTime（30s）内重开不重发查询
    await closeDialogByEscape("settings");
    expect(screen.queryByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeNull();
    expect(callsOf("usage_observed_entries_get")).toHaveLength(1);
    fireEvent.click(screen.getByTestId("usage-config-open-settings"));
    expect(await screen.findByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeTruthy();
    expect(callsOf("usage_observed_entries_get")).toHaveLength(1);

    // 关闭按钮（X）同样可关：卸载弹窗
    fireEvent.click(
      within(screen.getByTestId("usage-config-dialog-settings")).getByRole("button", {
        name: "Close",
      })
    );
    await waitFor(() => expect(screen.queryByTestId("usage-config-dialog-settings")).toBeNull());
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("1f. 回归（生产疑点修复）：初始焦点在弹窗标题而非 Info；≥800ms 后 Info 不自动弹层；不 blur 直接 Esc 一次关闭", async () => {
    // 不启用价格快照 ⇒ 条目无自动匹配徽标，行内不会挂常驻 role=tooltip 的价格预览，
    // 因此 queryByRole("tooltip") 全局判空是有效的（Info 浮层若被自动打开会立即命中）。
    render(wrap(<UsageSection />));
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    const dialog = await screen.findByTestId("usage-config-dialog-settings");

    // 焦点 = 弹窗标题（生产 onOpenAutoFocus 显式指定，tabIndex=-1），不是 Info 按钮
    const title = within(dialog).getByText(i18n.t("usage.config.title"));
    expect(title).toHaveAttribute("data-slot", "dialog-title");
    expect(title).toHaveAttribute("tabindex", "-1");
    const info = within(dialog).getByTestId("usage-config-info-settings");
    await waitFor(() => expect(document.activeElement).toBe(title));
    expect(document.activeElement).not.toBe(info);

    // 等过 Tooltip 打开延迟（Radix 默认 700ms，取 800ms 余量）：Info 不得自行弹出。
    // 包进 act：这 800ms 里条目查询还会落定，避免状态更新落在 act 外刷告警。
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 800));
    });
    expect(screen.queryByRole("tooltip")).toBeNull();

    // 不 blur、不移动焦点，直接 Esc：弹窗必须一次关闭
    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("usage-config-dialog-settings")).toBeNull());
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("1b. 观测条目读取失败：错误可见可重试", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.reject({ code: "usage-db-failed", detail: "boom" }),
    });
    render(wrap(<UsageSection />));
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));

    const error = await screen.findByTestId("usage-config-load-error");
    expect(error).toHaveTextContent("boom");

    restore();
  });

  it("1c. 别名读取失败：usage-config-alias-error 可见且重试后恢复条目列表（P2 复审补例）", async () => {
    // 首次 reject，重试（refetch）后放行 → 错误退场、条目列表回来
    let failed = false;
    const restore = override({
      usage_provider_aliases_get: () =>
        failed
          ? Promise.resolve(mockProviderAliasesGet())
          : Promise.reject({ code: "usage-db-failed", detail: "alias boom" }),
    });
    render(wrap(<UsageSection />));
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));

    // 别名错误态替代条目列表渲染（role=alert + 错误原因）
    const error = await screen.findByTestId("usage-config-alias-error");
    expect(error).toHaveAttribute("role", "alert");
    expect(error).toHaveTextContent("alias boom");
    expect(screen.queryByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeNull();

    // 重试：refetch 成功 → 列表恢复
    failed = true;
    fireEvent.click(within(error).getByRole("button", { name: "重试" }));
    expect(await screen.findByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeTruthy();
    expect(screen.queryByTestId("usage-config-alias-error")).toBeNull();

    restore();
  });
});

describe("D 卡入口（usage-cost-manage → 同一个弹窗）", () => {
  function CostHarness() {
    return (
      <UsageCostCard
        range={{ preset: "last7d" }}
        open
        onOpenChange={() => {}}
        t={i18n.t.bind(i18n)}
      />
    );
  }

  it("1d. 初始无弹窗；manage 按钮 aria-haspopup=dialog；点击打开 usage-config-dialog-cost，关闭后卸载停发", async () => {
    render(wrap(<CostHarness />));
    await screen.findByTestId("usage-cost-body");

    // 关闭态：无弹窗、无条目查询
    expect(screen.queryByTestId("usage-config-dialog-cost")).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(callsOf("usage_observed_entries_get")).toHaveLength(0);

    // 打开：manage 按钮 = aria-haspopup=dialog 的受控开关；弹窗挂载并拉取观察条目
    const manage = screen.getByTestId("usage-cost-manage");
    expect(manage).toHaveTextContent(i18n.t("usage.cost.manage"));
    expect(manage).toHaveAttribute("aria-haspopup", "dialog");
    fireEvent.click(manage);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveAttribute("data-testid", "usage-config-dialog-cost");
    expect(screen.getByTestId("usage-config-panel-cost")).toBeTruthy();
    await waitFor(() => expect(callsOf("usage_observed_entries_get").length).toBeGreaterThan(0));

    // 关闭（Esc）：弹窗卸载；查询不再新发（关闭 = 面板不挂载）
    await closeDialogByEscape("cost");
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    const callsAfterClose = callsOf("usage_observed_entries_get").length;
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(callsOf("usage_observed_entries_get")).toHaveLength(callsAfterClose);
  });

  it("1e. 两个实例各自 trigger 打开各自弹窗：壳 testid 带 instanceId 不冲突（settings/cost）", async () => {
    render(
      wrap(
        <>
          <UsageSection />
          <CostHarness />
        </>
      )
    );

    // 设置页 trigger → settings 弹窗；cost 弹窗不在场
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    expect(await screen.findByTestId("usage-config-dialog-settings")).toBeTruthy();
    expect(screen.queryByTestId("usage-config-dialog-cost")).toBeNull();

    // 关掉再从 D 卡 manage 进 → cost 弹窗；settings 弹窗不在场
    //（正文内部 testid 不带实例前缀 ⇒ 同一棵树同时只开一个弹窗；生产路由/窗口互斥）
    await closeDialogByEscape("settings");
    fireEvent.click(screen.getByTestId("usage-cost-manage"));
    expect(await screen.findByTestId("usage-config-dialog-cost")).toBeTruthy();
    expect(screen.queryByTestId("usage-config-dialog-settings")).toBeNull();
  });
});

describe("条目内供应商改名（A 段删除后的唯一入口）", () => {
  it("2. 普通 provider：改供应商名走 provider 别名 upsert，对同一原始 provider 全局生效", async () => {
    await openConfigPanel();

    const input = await screen.findByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5");
    // 未编辑 = 当前显示名；placeholder = 原始 provider
    expect((input as HTMLInputElement).value).toBe("OpenAI");
    expect(input).toHaveAttribute("placeholder", "openai");

    fireEvent.change(input, { target: { value: "OpenAI 官方" } });
    fireEvent.keyDown(input, { key: "Enter" });

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    const rule = callsOf("usage_provider_alias_upsert")[0][1]?.rule as Record<string, unknown>;
    expect(rule).toMatchObject({
      kind: "provider",
      provider: "openai",
      sourceId: null,
      modelId: null,
      displayName: "OpenAI 官方",
    });
  });

  it("2b. 空 provider 条目：写 toolModel 别名（复用既有规则的 id 原位替换）", async () => {
    await openConfigPanel();

    // 默认夹具里 workbuddy/未知模型 已有 toolModel 别名 "WorkBuddy" → 折叠进 d- 展示键
    const input = await screen.findByTestId(
      "usage-config-model-provider-input-d-WorkBuddy-unknown"
    );
    expect((input as HTMLInputElement).value).toBe("WorkBuddy");
    // placeholder = 原始 provider（空 → 未指定渠道文案）
    expect(input).toHaveAttribute("placeholder", i18n.t("usage.label.unassignedChannel"));

    fireEvent.change(input, { target: { value: "WB 渠道" } });
    fireEvent.blur(input);

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    const rule = callsOf("usage_provider_alias_upsert")[0][1]?.rule as Record<string, unknown>;
    expect(rule).toMatchObject({
      kind: "toolModel",
      provider: null,
      sourceId: "workbuddy",
      modelId: "",
      displayName: "WB 渠道",
    });
    // 夹具里已有同键规则 → 原位替换（同 id），不撞唯一索引
    expect(rule.id).toBe("alias-workbuddy-unknown");
  });

  it("2c. 清空 = 删除别名回原名/未指定渠道；无规则可清时不发命令", async () => {
    await openConfigPanel();

    // workbuddy 条目有既有规则 → 清空走 delete
    const tmInput = await screen.findByTestId(
      "usage-config-model-provider-input-d-WorkBuddy-unknown"
    );
    fireEvent.change(tmInput, { target: { value: "" } });
    fireEvent.blur(tmInput);
    await waitFor(() => expect(callsOf("usage_provider_alias_delete")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_delete")[0][1]?.id).toBe("alias-workbuddy-unknown");

    // openai 组无规则 → 清空不发任何命令（0 次 upsert）
    const pInput = screen.getByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5");
    fireEvent.change(pInput, { target: { value: "" } });
    fireEvent.blur(pInput);
    // 尾拍收进 act：让失效重取链上的 React 状态更新都在 act 内落定（消 act 警告）
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(callsOf("usage_provider_alias_upsert")).toHaveLength(0);
    expect(callsOf("usage_provider_alias_delete")).toHaveLength(1);
  });

  it("2d. 同一原始 provider 多成员折叠在一个条目：改名只 upsert 一次（不撞唯一键）", async () => {
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({ providerRaw: "openai", displayProvider: "OpenAI" }),
          // 同一原始 provider 的重复成员（防御面：后端按身份去重前的宽口径）
          observedEntry({
            providerRaw: "openai",
            displayProvider: "OpenAI",
            totalTokens: 7,
            recentTokens7d: 7,
          }),
        ]),
    });
    await openConfigPanel();

    // 两个成员折叠为一个条目
    expect(screen.getByTestId("usage-config-model-entry-d-OpenAI-gpt-5")).toBeTruthy();

    const input = screen.getByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5");
    fireEvent.change(input, { target: { value: "OpenAI 统一" } });
    fireEvent.blur(input);

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    await new Promise((resolve) => setTimeout(resolve, 20));
    // 去重后仍只有一次（二次 upsert 会撞「别名键已存在」假冲突）
    expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1);
    expect(callsOf("usage_provider_alias_upsert")[0][1]?.rule).toMatchObject({
      provider: "openai",
      displayName: "OpenAI 统一",
    });

    restore();
  });

  it("2e. 保存失败：原因可见（usage-config-model-error）、草稿保留", async () => {
    const restore = override({
      usage_provider_alias_upsert: () =>
        Promise.reject({
          code: "usage-settings-invalid",
          detail: "provider alias key already exists",
        }),
    });
    await openConfigPanel();

    const input = await screen.findByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5");
    fireEvent.change(input, { target: { value: "冲突名" } });
    fireEvent.blur(input);

    const error = await screen.findByTestId("usage-config-model-error");
    expect(error).toHaveTextContent("provider alias key already exists");
    expect(error).toHaveAttribute("role", "alert");
    expect(
      (screen.getByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5") as HTMLInputElement)
        .value
    ).toBe("冲突名");

    restore();
  });

  it("2f. 重入守卫：Enter 保存后补发 blur 不双提交（P2-4 同源契约）", async () => {
    // 观察条目的 displayProvider 随别名状态派生（真机后端语义：保存后重取回显新名，
    // blur 第二次进来 value === current 直接早退；夹具硬编码显示名会伪造双提交）。
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "openai",
            displayProvider: providerDisplayOf("openai"),
          }),
        ]),
    });
    // 无别名时 displayProvider = 原始 provider 串（小写 openai）→ 展示键 testid 随之
    await openConfigPanel("usage-config-model-entry-d-openai-gpt-5");

    const input = await screen.findByTestId("usage-config-model-provider-input-d-openai-gpt-5");
    fireEvent.change(input, { target: { value: "重入名" } });
    // 真机序列：Enter 提交 → busy=true → 输入框 disable → 浏览器触发 blur → 再走一次 save。
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.blur(input);

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    // 尾拍收进 act：让重取/失效链上的 React 状态更新都在 act 内落定（消 act 警告）
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1);
    expect(callsOf("usage_provider_alias_upsert")[0][1]?.rule).toMatchObject({
      provider: "openai",
      displayName: "重入名",
    });

    restore();
  });
});

describe("Top10 默认 / 展开全部 / 搜索", () => {
  /** 12 个独立条目（recentTokens7d 递减）：Top10 之外还压着 2 条 */
  function twelveEntries(): ObservedModelEntry[] {
    return Array.from({ length: 12 }, (_, i) => {
      const n = String(i + 1).padStart(2, "0");
      return observedEntry({
        providerRaw: `prov-${n}`,
        displayProvider: `Chan-${n}`,
        modelId: `m-${n}`,
        normalizedModelId: `m-${n}`,
        totalTokens: 1200 - i * 100,
        recentTokens7d: 1200 - i * 100,
      });
    });
  }

  it("3. 默认只显示最近 7 天 Top10（按 recentTokens7d 降序）；底部「展开全部（N）」", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(twelveEntries()),
    });
    await openConfigPanel("usage-config-model-entry-d-Chan-10-m-10");

    // Top10 在场（降序：Chan-01 最大）
    for (let i = 1; i <= 10; i += 1) {
      const n = String(i).padStart(2, "0");
      expect(screen.getByTestId(`usage-config-model-entry-d-Chan-${n}-m-${n}`)).toBeTruthy();
    }
    // 第 11/12 条默认不在场
    expect(screen.queryByTestId("usage-config-model-entry-d-Chan-11-m-11")).toBeNull();
    expect(screen.queryByTestId("usage-config-model-entry-d-Chan-12-m-12")).toBeNull();
    // 排序：第一行 = 7 天用量最大的条目
    const entries = screen.getAllByTestId(/^usage-config-model-entry-/);
    expect(entries[0].dataset.testid).toBe("usage-config-model-entry-d-Chan-01-m-01");

    // 默认没有搜索框（展开后才出现）
    expect(screen.queryByTestId("usage-config-model-search")).toBeNull();
    // 底部按钮 = 展开全部（2）
    const more = screen.getByTestId("usage-config-model-more");
    expect(more).toHaveTextContent(i18n.t("usage.config.modelSection.expandAll", { count: 2 }));
    expect(more).toHaveAttribute("aria-expanded", "false");

    restore();
  });

  it("3b. 展开后出现搜索框并显示全部；收起回到 Top10 且清掉筛选", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(twelveEntries()),
    });
    await openConfigPanel("usage-config-model-entry-d-Chan-10-m-10");

    fireEvent.click(screen.getByTestId("usage-config-model-more"));

    // 全部 12 条在场；搜索框出现；按钮换「收起」
    expect(await screen.findByTestId("usage-config-model-entry-d-Chan-11-m-11")).toBeTruthy();
    expect(screen.getByTestId("usage-config-model-entry-d-Chan-12-m-12")).toBeTruthy();
    expect(screen.getByTestId("usage-config-model-search")).toBeTruthy();
    const more = screen.getByTestId("usage-config-model-more");
    expect(more).toHaveTextContent(i18n.t("usage.config.modelSection.collapse"));
    expect(more).toHaveAttribute("aria-expanded", "true");

    // 收起：回到 Top10，搜索框退场
    fireEvent.click(more);
    await waitFor(() =>
      expect(screen.queryByTestId("usage-config-model-entry-d-Chan-11-m-11")).toBeNull()
    );
    expect(screen.queryByTestId("usage-config-model-search")).toBeNull();
    expect(screen.getByTestId("usage-config-model-entry-d-Chan-01-m-01")).toBeTruthy();

    restore();
  });

  it("3c. 搜索：搜索态展示全部匹配项（含 Top10 之外的）；无结果显示空态", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(twelveEntries()),
    });
    await openConfigPanel("usage-config-model-entry-d-Chan-10-m-10");

    fireEvent.click(screen.getByTestId("usage-config-model-more"));
    const search = await screen.findByTestId("usage-config-model-search");
    fireEvent.change(search, { target: { value: "Chan-1" } });

    // 搜索态不受 Top10 限制：Chan-10..12（匹配 "Chan-1*"）全部出现，Chan-02..09 隐藏
    for (const n of ["10", "11", "12"]) {
      expect(screen.getByTestId(`usage-config-model-entry-d-Chan-${n}-m-${n}`)).toBeTruthy();
    }
    expect(screen.queryByTestId("usage-config-model-entry-d-Chan-02-m-02")).toBeNull();

    // 无匹配：空态文案
    fireEvent.change(search, { target: { value: "不存在的模型" } });
    expect(await screen.findByTestId("usage-config-model-search-empty")).toHaveTextContent(
      i18n.t("usage.config.modelSection.noMatch")
    );
    // 搜索态下按钮仍是「收起」（needle 非空即展示全部匹配）
    expect(screen.getByTestId("usage-config-model-more")).toHaveAttribute("aria-expanded", "true");

    restore();
  });

  it("3d. 条目数 ≤ 10：不显示展开按钮", async () => {
    await openConfigPanel();
    expect(callsOf("usage_observed_entries_get").length).toBeGreaterThan(0);
    expect(screen.queryByTestId("usage-config-model-more")).toBeNull();
  });
});

describe("排序（2026-10-21：三键 + 同键升降序 + Top10 截断前排序）", () => {
  /** 显示名顺序刻意与原始 provider / 用量顺序都不同，证明排序键取自原始证据。 */
  function sortableEntries(): ObservedModelEntry[] {
    return [
      observedEntry({
        providerRaw: "zeta",
        displayProvider: "Alpha",
        modelId: "m-1",
        normalizedModelId: "m-1",
        totalTokens: 100,
        recentTokens7d: 100,
      }),
      observedEntry({
        providerRaw: "alpha",
        displayProvider: "Zeta",
        modelId: "m-2",
        normalizedModelId: "m-2",
        totalTokens: 300,
        recentTokens7d: 300,
      }),
      observedEntry({
        providerRaw: "mike",
        displayProvider: "Mike",
        modelId: "m-3",
        normalizedModelId: "m-3",
        totalTokens: 200,
        recentTokens7d: 200,
      }),
      observedEntry({
        providerRaw: "bravo",
        displayProvider: "Bravo",
        modelId: "m-4",
        normalizedModelId: "m-4",
        totalTokens: 400,
        recentTokens7d: 400,
      }),
    ];
  }

  const entryOrder = (): string[] =>
    screen
      .getAllByTestId(/^usage-config-model-entry-/)
      .map((el) => el.getAttribute("data-testid") ?? "");

  it("7. 默认用量降序；图例与三键在场；同一键再点切换升/降序", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(sortableEntries()),
    });
    await openConfigPanel(entry("Bravo", "m-4"));

    // 默认：用法降序（400 → 100）
    expect(entryOrder()).toEqual([
      entry("Bravo", "m-4"),
      entry("Zeta", "m-2"),
      entry("Mike", "m-3"),
      entry("Alpha", "m-1"),
    ]);
    const usageSort = screen.getByTestId("usage-config-sort-usage");
    expect(usageSort).toHaveAttribute("aria-pressed", "true");
    expect(usageSort).toHaveAttribute(
      "aria-label",
      `${i18n.t("usage.config.modelSection.sortUsage")} ${i18n.t(
        "usage.config.modelSection.sortDesc"
      )}`
    );
    expect(screen.getByTestId("usage-config-sort-provider")).toHaveAttribute(
      "aria-pressed",
      "false"
    );
    expect(screen.getByTestId("usage-config-sort-model")).toHaveAttribute("aria-pressed", "false");

    // 图例三态
    expect(screen.getByText(i18n.t("usage.config.modelSection.legend"))).toBeTruthy();
    expect(screen.getByText(i18n.t("usage.config.modelSection.legendReadonly"))).toBeTruthy();
    expect(screen.getByText(i18n.t("usage.config.modelSection.legendEditable"))).toBeTruthy();
    const modified = screen.getByText(i18n.t("usage.config.modelSection.legendModified"));
    expect(modified.className).toContain("amber-500/10");

    // 同一键再点：升序（100 → 400）
    fireEvent.click(usageSort);
    expect(entryOrder()).toEqual([
      entry("Alpha", "m-1"),
      entry("Mike", "m-3"),
      entry("Zeta", "m-2"),
      entry("Bravo", "m-4"),
    ]);
    expect(usageSort).toHaveAttribute("aria-pressed", "true");
    expect(usageSort).toHaveAttribute(
      "aria-label",
      `${i18n.t("usage.config.modelSection.sortUsage")} ${i18n.t(
        "usage.config.modelSection.sortAsc"
      )}`
    );

    // 第三次点：回到降序
    fireEvent.click(usageSort);
    expect(entryOrder()[0]).toBe(entry("Bravo", "m-4"));

    restore();
  });

  it("7b. 按原始供应商名 / 原始模型名排序（不按显示名），同键再点反向", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(sortableEntries()),
    });
    await openConfigPanel(entry("Bravo", "m-4"));

    // 原始 provider 升序：alpha(B) < bravo(D) < mike(C) < zeta(A)
    fireEvent.click(screen.getByTestId("usage-config-sort-provider"));
    expect(entryOrder()).toEqual([
      entry("Zeta", "m-2"),
      entry("Bravo", "m-4"),
      entry("Mike", "m-3"),
      entry("Alpha", "m-1"),
    ]);
    expect(screen.getByTestId("usage-config-sort-provider")).toHaveAttribute(
      "aria-pressed",
      "true"
    );

    // 再点：原始 provider 降序
    fireEvent.click(screen.getByTestId("usage-config-sort-provider"));
    expect(entryOrder()).toEqual([
      entry("Alpha", "m-1"),
      entry("Mike", "m-3"),
      entry("Bravo", "m-4"),
      entry("Zeta", "m-2"),
    ]);

    // 原始模型名升序：m-1 < m-2 < m-3 < m-4
    fireEvent.click(screen.getByTestId("usage-config-sort-model"));
    expect(entryOrder()).toEqual([
      entry("Alpha", "m-1"),
      entry("Zeta", "m-2"),
      entry("Mike", "m-3"),
      entry("Bravo", "m-4"),
    ]);
    expect(screen.getByTestId("usage-config-sort-model")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-config-sort-provider")).toHaveAttribute(
      "aria-pressed",
      "false"
    );

    // 再点：原始模型名降序
    fireEvent.click(screen.getByTestId("usage-config-sort-model"));
    expect(entryOrder()).toEqual([
      entry("Bravo", "m-4"),
      entry("Mike", "m-3"),
      entry("Zeta", "m-2"),
      entry("Alpha", "m-1"),
    ]);

    restore();
  });

  it("7c. 排序作用于全部条目后再截 Top10（低用量但原始 provider 靠前的条目能进榜）", async () => {
    // 12 条：用量随序号递减；最后两条（m-11/m-12）用 aa-* 原始 provider。
    // 默认用量 Top10 会隐藏它们；按原始 provider 升序后它们应排在最前并进入 Top10。
    const entries = Array.from({ length: 12 }, (_, index) => {
      const n = String(index + 1).padStart(2, "0");
      const lowUsage = index >= 10;
      return observedEntry({
        providerRaw: lowUsage ? `aa-${n}` : `zz-${n}`,
        displayProvider: lowUsage ? `aa-${n}` : `zz-${n}`,
        modelId: `m-${n}`,
        normalizedModelId: `m-${n}`,
        totalTokens: 1300 - (index + 1) * 100,
        recentTokens7d: 1300 - (index + 1) * 100,
      });
    });
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve(entries),
    });
    await openConfigPanel(entry("zz-10", "m-10"));

    // 默认：前 10 条用量最大的在榜；aa-11/aa-12 被截掉
    expect(entryOrder()[0]).toBe(entry("zz-01", "m-01"));
    expect(screen.queryByTestId(entry("aa-11", "m-11"))).toBeNull();
    expect(screen.queryByTestId(entry("aa-12", "m-12"))).toBeNull();

    // 按原始 provider 升序后：aa-11、aa-12 排最前（可见），zz-09/zz-10 被挤出 Top10
    fireEvent.click(screen.getByTestId("usage-config-sort-provider"));
    expect(entryOrder()[0]).toBe(entry("aa-11", "m-11"));
    expect(entryOrder()[1]).toBe(entry("aa-12", "m-12"));
    expect(screen.queryByTestId(entry("zz-09", "m-09"))).toBeNull();
    expect(screen.queryByTestId(entry("zz-10", "m-10"))).toBeNull();
    // Top10 之外仍有 2 条（展开按钮文案不变）
    expect(screen.getByTestId("usage-config-model-more")).toHaveTextContent(
      i18n.t("usage.config.modelSection.expandAll", { count: 2 })
    );

    restore();
  });
});

describe("可交互性标识（2026-10-21）", () => {
  it("7d. 自定义供应商/模型输入 + 计价身份覆盖 badge/ref 琥珀色；普通行无琥珀；按钮 outline/边框", async () => {
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "openai",
            displayProvider: "OpenAI Custom",
            displayModelName: "GPT Custom",
            pricingProvider: "openai",
            pricingModel: "gpt-5",
            totalTokens: 500,
            recentTokens7d: 500,
          }),
          // 普通行：无自定义名、无计价身份覆盖
          observedEntry({
            providerRaw: "azure-openai",
            sourceId: "codex",
            displayProvider: null,
            displayModelName: null,
            modelId: "plain",
            normalizedModelId: "plain",
            totalTokens: 100,
            recentTokens7d: 100,
          }),
        ]),
    });
    await openConfigPanel(entry("OpenAI-Custom", "gpt-5"));

    // 有自定义值：供应商名/模型名输入框琥珀色
    const providerInput = screen.getByTestId(
      "usage-config-model-provider-input-d-OpenAI-Custom-gpt-5"
    );
    expect(providerInput.className).toContain("border-amber-500/60");
    expect(providerInput.className).toContain("text-amber-700");
    const modelInput = screen.getByTestId(
      "usage-config-model-display-name-input-d-OpenAI-Custom-gpt-5"
    );
    expect(modelInput.className).toContain("border-amber-500/60");
    expect(modelInput.className).toContain("text-amber-700");

    // 计价身份覆盖：徽标 + ref 琥珀色
    const badge = await screen.findByTestId("usage-config-pricing-badge-d-OpenAI-Custom-gpt-5");
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.pricingOverridden"));
    expect(badge.className).toContain("amber-500/50");
    expect(badge.className).toContain("text-amber-700");
    const ref = screen.getByTestId("usage-config-pricing-ref-d-OpenAI-Custom-gpt-5");
    expect(ref).toHaveTextContent("openai / gpt-5");
    expect(ref.className).toContain("text-amber-700");

    // 按钮形态：覆盖快捷入口 / 清除覆盖 / 身份选择器 outline；选项按钮有边框
    expect(screen.getByTestId("usage-config-model-override-d-OpenAI-Custom-gpt-5")).toHaveAttribute(
      "data-variant",
      "outline"
    );
    expect(screen.getByTestId("usage-config-pricing-clear-d-OpenAI-Custom-gpt-5")).toHaveAttribute(
      "data-variant",
      "outline"
    );
    const toggle = screen.getByTestId("usage-config-pricing-toggle-d-OpenAI-Custom-gpt-5");
    expect(toggle).toHaveAttribute("data-variant", "outline");
    fireEvent.click(toggle);
    // 目录候选按查询词拉取（未输入时只给短查询提示）
    fireEvent.change(screen.getByTestId("usage-config-pricing-search-d-OpenAI-Custom-gpt-5"), {
      target: { value: "gp" },
    });
    const option = await screen.findByTestId("usage-config-pricing-option-openai/gpt-5");
    expect(option.className).toContain("border");

    // 普通行：无自定义值 / 无覆盖徽标，输入框不带琥珀色；覆盖入口仍保留（未禁用）
    const plainProvider = screen.getByTestId("usage-config-model-provider-input-u-codex-plain");
    expect(plainProvider.className).not.toContain("amber");
    const plainModel = screen.getByTestId("usage-config-model-display-name-input-u-codex-plain");
    expect(plainModel.className).not.toContain("amber");
    expect(screen.queryByTestId("usage-config-model-overridden-u-codex-plain")).toBeNull();
    expect(screen.queryByTestId("usage-config-pricing-badge-u-codex-plain")).toBeNull();
    expect(screen.queryByTestId("usage-config-pricing-ref-u-codex-plain")).toBeNull();
    // 普通行不出现「指派/绑定」类附加按钮（原有覆盖入口保持唯一）
    expect(document.querySelector('[data-testid*="usage-config-assign"]')).toBeNull();
    expect(screen.getByTestId("usage-config-model-override-u-codex-plain")).toBeTruthy();

    restore();
  });
});

describe("条目 B 面 · 模型名与计价身份", () => {
  it("4. 三行结构：展示键折叠多成员、row1 全部原始证据、供应商输入回填显示名", async () => {
    await openConfigPanel();

    // openai + azure-openai 同显「OpenAI」+ 同模型 → 折叠为一个条目
    const entry = screen.getByTestId("usage-config-model-entry-d-OpenAI-gpt-5");
    const raw = screen.getByTestId("usage-config-model-raw-d-OpenAI-gpt-5");
    expect(raw).toHaveTextContent("openai");
    expect(raw).toHaveTextContent("azure-openai");
    expect(raw).toHaveTextContent("gpt-5");
    // row2 供应商输入回填当前显示名；模型名输入为空（placeholder = 原始模型）
    expect(
      (screen.getByTestId("usage-config-model-provider-input-d-OpenAI-gpt-5") as HTMLInputElement)
        .value
    ).toBe("OpenAI");
    expect(
      (
        screen.getByTestId(
          "usage-config-model-display-name-input-d-OpenAI-gpt-5"
        ) as HTMLInputElement
      ).value
    ).toBe("");
    expect(entry).toBeTruthy();

    // edenai 条目独立
    expect(screen.getByTestId("usage-config-model-raw-d-edenai-glm-5.3")).toHaveTextContent(
      "glm-5.3"
    );

    // 空 provider 条目（已有 toolModel 别名 → 折叠进 d- 展示键）：row1 = 工具 / 未知模型
    const tmRaw = screen.getByTestId("usage-config-model-raw-d-WorkBuddy-unknown");
    expect(tmRaw).toHaveTextContent("workbuddy");
    expect(tmRaw).toHaveTextContent(i18n.t("usage.label.unknownModel"));
    expect(
      (
        screen.getByTestId(
          "usage-config-model-provider-input-d-WorkBuddy-unknown"
        ) as HTMLInputElement
      ).value
    ).toBe("WorkBuddy");

    // row2 改名范围提示：每个条目都带「同一展示键一起改名」说明
    expect(
      screen.getByTestId("usage-config-model-display-name-hint-d-OpenAI-gpt-5")
    ).toHaveTextContent(i18n.t("usage.config.modelSection.displayNameHint"));
  });

  it("5. row2 自定义模型名：多成员批量写（同展示键一起改名）", async () => {
    await openConfigPanel();

    const input = screen.getByTestId("usage-config-model-display-name-input-d-OpenAI-gpt-5");
    fireEvent.change(input, { target: { value: "GPT-5 定制" } });
    fireEvent.blur(input);

    await waitFor(() => expect(callsOf("usage_model_display_name_set")).toHaveLength(2));
    const calls = callsOf("usage_model_display_name_set").map(
      (c) => c[1]?.request as { providerRaw: string | null; modelId: string; displayName: string }
    );
    expect(calls).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          providerRaw: "openai",
          sourceId: null,
          modelId: "gpt-5",
          displayName: "GPT-5 定制",
        }),
        expect.objectContaining({
          providerRaw: "azure-openai",
          sourceId: null,
          modelId: "gpt-5",
          displayName: "GPT-5 定制",
        }),
      ])
    );
  });

  it("5b. 保存重入守卫：Enter 保存后补发 blur 不对同一草稿双写（P2-4）", async () => {
    await openConfigPanel();

    const input = screen.getByTestId("usage-config-model-display-name-input-d-OpenAI-gpt-5");
    fireEvent.change(input, { target: { value: "重入探测名" } });
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.blur(input);

    // 多成员批量写恰为一轮：每成员一次（2 成员 = 2 次），不是 4 次
    await waitFor(() => expect(callsOf("usage_model_display_name_set")).toHaveLength(2));
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(callsOf("usage_model_display_name_set")).toHaveLength(2);
  });

  it("6. row2 复制原始模型名：一键填入并保存", async () => {
    await openConfigPanel();

    fireEvent.click(screen.getByTestId("usage-config-model-copy-d-edenai-glm-5.3"));

    await waitFor(() => expect(callsOf("usage_model_display_name_set")).toHaveLength(1));
    expect(callsOf("usage_model_display_name_set")[0][1]?.request).toMatchObject({
      providerRaw: "edenai",
      sourceId: null,
      modelId: "glm-5.3",
      displayName: "glm-5.3",
    });
    expect(
      (
        screen.getByTestId(
          "usage-config-model-display-name-input-d-edenai-glm-5.3"
        ) as HTMLInputElement
      ).value
    ).toBe("glm-5.3");
  });

  it("6b. row2 清除按钮：displayName=null（回原名），输入框清空", async () => {
    // 可变夹具：写命令真实更新这份状态，失效重取后能看到清除效果
    let displayName: string | null = "GLM";
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "edenai",
            modelId: "glm-5.3",
            normalizedModelId: "glm-5-3",
            displayProvider: "edenai",
            displayModelName: displayName,
          }),
        ]),
      usage_model_display_name_set: (_cmd, args) => {
        displayName =
          ((args as { request?: { displayName?: string | null } }).request?.displayName ?? null) ===
          null
            ? null
            : (args as { request: { displayName: string } }).request.displayName;
        return Promise.resolve([
          observedEntry({
            providerRaw: "edenai",
            modelId: "glm-5.3",
            normalizedModelId: "glm-5-3",
            displayProvider: "edenai",
            displayModelName: displayName,
          }),
        ]);
      },
    });
    await openConfigPanel("usage-config-model-entry-d-edenai-glm-5.3");

    const input = await screen.findByTestId(
      "usage-config-model-display-name-input-d-edenai-glm-5.3"
    );
    expect((input as HTMLInputElement).value).toBe("GLM");

    fireEvent.click(screen.getByTestId("usage-config-model-clear-d-edenai-glm-5.3"));

    await waitFor(() => expect(callsOf("usage_model_display_name_set")).toHaveLength(1));
    expect(callsOf("usage_model_display_name_set")[0][1]?.request).toMatchObject({
      providerRaw: "edenai",
      sourceId: null,
      modelId: "glm-5.3",
      displayName: null,
    });
    await waitFor(() =>
      expect(
        (
          screen.getByTestId(
            "usage-config-model-display-name-input-d-edenai-glm-5.3"
          ) as HTMLInputElement
        ).value
      ).toBe("")
    );

    restore();
  });

  it("7. row3 默认自动匹配徽标：官方命中 / 未命中 / 空模型无徽标", async () => {
    await openConfigPanel();

    // gpt-5 精选内精确命中官方 openai
    const badge = await screen.findByTestId("usage-config-pricing-badge-d-OpenAI-gpt-5");
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.pricingAutoMatched"));
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.sources.official"));

    // glm-5.3 精选未命中
    await waitFor(() =>
      expect(screen.getByTestId("usage-config-pricing-none-d-edenai-glm-5.3")).toHaveTextContent(
        i18n.t("usage.config.modelSection.pricingNone")
      )
    );

    // 空模型：不发 auto match 查询，也不显示命中徽标
    expect(callsOf("usage_pricing_auto_match").every((c) => (c[1]?.modelId as string) !== "")).toBe(
      true
    );
    expect(screen.queryByTestId("usage-config-pricing-badge-d-WorkBuddy-unknown")).toBeNull();
  });

  it("7b. row3 前缀命中徽标", async () => {
    const restore = override({
      usage_pricing_auto_match: () =>
        Promise.resolve({
          kind: "prefix",
          reference: { providerId: "openai", modelId: "gpt-5" },
          quote: null,
        }),
    });
    await openConfigPanel();

    const badge = await screen.findByTestId("usage-config-pricing-badge-d-OpenAI-gpt-5");
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.prefixMatched"));

    restore();
  });

  it("8. row3 仅下拉选择：Enter 不提交自由文本；点选项走 identity set（多成员批量）", async () => {
    await openConfigPanel();

    fireEvent.click(screen.getByTestId("usage-config-pricing-toggle-d-OpenAI-gpt-5"));
    const search = screen.getByTestId("usage-config-pricing-search-d-OpenAI-gpt-5");
    fireEvent.change(search, { target: { value: "gp" } });

    const option = await screen.findByTestId("usage-config-pricing-option-openai/gpt-5");
    // 精选作用域：非精选来源不出现
    expect(screen.queryByTestId("usage-config-pricing-option-aggregator-x/gpt-5")).toBeNull();

    // 自由文本 + Enter：不提交（无自由输入纪律）
    fireEvent.keyDown(search, { key: "Enter" });
    expect(callsOf("usage_pricing_identity_set")).toHaveLength(0);

    fireEvent.click(option);
    await waitFor(() => expect(callsOf("usage_pricing_identity_set")).toHaveLength(2));
    const requests = callsOf("usage_pricing_identity_set").map(
      (c) =>
        c[1]?.request as { providerRaw: string; reference: { providerId: string; modelId: string } }
    );
    expect(requests).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          providerRaw: "openai",
          modelId: "gpt-5",
          reference: { providerId: "openai", modelId: "gpt-5" },
        }),
        expect.objectContaining({
          providerRaw: "azure-openai",
          modelId: "gpt-5",
          reference: { providerId: "openai", modelId: "gpt-5" },
        }),
      ])
    );
  });

  it("9. row3 已覆盖徽标 + 清除覆盖（reference=null）", async () => {
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "edenai",
            modelId: "glm-5.3",
            normalizedModelId: "glm-5-3",
            displayProvider: "edenai",
            pricingProvider: "openai",
            pricingModel: "gpt-5",
          }),
        ]),
    });
    await openConfigPanel("usage-config-model-entry-d-edenai-glm-5.3");

    const badge = await screen.findByTestId("usage-config-pricing-badge-d-edenai-glm-5.3");
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.pricingOverridden"));
    expect(screen.getByTestId("usage-config-pricing-ref-d-edenai-glm-5.3")).toHaveTextContent(
      "openai / gpt-5"
    );

    fireEvent.click(screen.getByTestId("usage-config-pricing-clear-d-edenai-glm-5.3"));
    await waitFor(() => expect(callsOf("usage_pricing_identity_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_identity_set")[0][1]?.request).toMatchObject({
      providerRaw: "edenai",
      sourceId: null,
      modelId: "glm-5.3",
      reference: null,
    });

    restore();
  });

  it("10. 「显示全部 N 个来源」开关：切换 scope，非精选来源可选中", async () => {
    await openConfigPanel();

    fireEvent.click(screen.getByTestId("usage-config-pricing-toggle-d-OpenAI-gpt-5"));

    // 未搜索时：仅精选提示
    expect(screen.getByTestId("usage-config-pricing-scope-d-OpenAI-gpt-5")).toHaveTextContent(
      i18n.t("usage.config.modelSection.curatedOnly")
    );

    const search = screen.getByTestId("usage-config-pricing-search-d-OpenAI-gpt-5");
    fireEvent.change(search, { target: { value: "gp" } });

    // N = providerCount（mock 基准 232）；span → button 换型必须重查（旧引用会脱挂）
    await waitFor(() =>
      expect(screen.getByTestId("usage-config-pricing-scope-d-OpenAI-gpt-5")).toHaveTextContent(
        i18n.t("usage.config.modelSection.showAllSources", { count: 232 })
      )
    );
    const toggle = screen.getByTestId("usage-config-pricing-scope-d-OpenAI-gpt-5");
    expect(toggle).toHaveAttribute("aria-pressed", "false");
    await screen.findByTestId("usage-config-pricing-option-openai/gpt-5");
    const curatedCalls = callsOf("usage_pricing_catalog_search").filter(
      (c) => c[1]?.scope === "curated"
    );
    expect(curatedCalls.length).toBeGreaterThan(0);

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-pressed", "true");
    await waitFor(() =>
      expect(callsOf("usage_pricing_catalog_search").some((c) => c[1]?.scope === "all")).toBe(true)
    );
    // 全量口径：非精选来源出现在候选里，且可显式选中
    const option = await screen.findByTestId("usage-config-pricing-option-aggregator-x/gpt-5");
    fireEvent.click(option);
    await waitFor(() => expect(callsOf("usage_pricing_identity_set")).toHaveLength(2));
    expect(callsOf("usage_pricing_identity_set")[0][1]?.request).toMatchObject({
      reference: { providerId: "aggregator-x", modelId: "gpt-5" },
    });
  });

  it("10b. 短查询不永久 loading：提示「输入至少 2 个字符」，凑足后正常出候选（P2-3）", async () => {
    await openConfigPanel();

    fireEvent.click(screen.getByTestId("usage-config-pricing-toggle-d-OpenAI-gpt-5"));
    const search = screen.getByTestId("usage-config-pricing-search-d-OpenAI-gpt-5");

    // 未输入 / 1 个字符：查询未启用（fetchStatus=idle）→ 输入提示，而不是停在「加载中…」
    const hint = i18n.t("usage.billing.pricingCatalogHint");
    expect(screen.getByTestId("usage-config-pricing-type-hint-d-OpenAI-gpt-5")).toHaveTextContent(
      hint
    );
    fireEvent.change(search, { target: { value: "g" } });
    expect(screen.getByTestId("usage-config-pricing-type-hint-d-OpenAI-gpt-5")).toHaveTextContent(
      hint
    );

    // 凑足 2 个字符：查询启动出候选，提示退场
    fireEvent.change(search, { target: { value: "gp" } });
    await screen.findByTestId("usage-config-pricing-option-openai/gpt-5");
    expect(screen.queryByTestId("usage-config-pricing-type-hint-d-OpenAI-gpt-5")).toBeNull();
  });

  it("11. 悬停价格表：四价 + 发布日 + 快照时间 + 条件价标记；aria-describedby 关联", async () => {
    const restore = override({
      usage_pricing_auto_match: (_cmd, args) => {
        const modelId = (args as { modelId?: string }).modelId ?? "";
        if (modelId === "gpt-5") {
          return Promise.resolve({
            kind: "exact",
            reference: { providerId: "openai", modelId: "gpt-5" },
            quote: {
              reference: { providerId: "openai", modelId: "gpt-5" },
              input: "1.25",
              cacheRead: "0.1",
              cacheWrite: null,
              output: "10",
              releaseDate: "2025-08-07",
              conditional: false,
              snapshotDate: 1_700_000_000_000,
            },
          });
        }
        if (modelId === "glm-5.3") {
          return Promise.resolve({
            kind: "exact",
            reference: { providerId: "edenai", modelId: "glm-5.3" },
            quote: {
              reference: { providerId: "edenai", modelId: "glm-5.3" },
              input: "0.4",
              cacheRead: null,
              cacheWrite: null,
              output: "1.6",
              releaseDate: null,
              conditional: true,
              snapshotDate: 1_700_000_000_000,
            },
          });
        }
        return Promise.resolve({ kind: "none", reference: null, quote: null });
      },
    });
    await openConfigPanel();

    const badge = await screen.findByTestId("usage-config-pricing-badge-d-OpenAI-gpt-5");
    const preview = screen.getByTestId("usage-config-price-preview-d-OpenAI-gpt-5");
    // 键盘可达：触发器 aria-describedby 指向预览
    expect(badge).toHaveAttribute("aria-describedby", preview.id);

    fireEvent.focus(badge);
    expect(preview).toHaveTextContent(i18n.t("usage.config.modelSection.pricePreview.input"));
    expect(preview).toHaveTextContent("$1.25");
    expect(preview).toHaveTextContent(i18n.t("usage.config.modelSection.pricePreview.cacheRead"));
    expect(preview).toHaveTextContent("$0.10");
    expect(preview).toHaveTextContent(i18n.t("usage.config.modelSection.pricePreview.output"));
    expect(preview).toHaveTextContent("$10.00");
    expect(preview).toHaveTextContent(i18n.t("usage.config.modelSection.pricePreview.releaseDate"));
    expect(preview).toHaveTextContent("2025-08-07");
    expect(preview).toHaveTextContent(
      i18n.t("usage.config.modelSection.pricePreview.snapshotDate")
    );
    // 无条件价标记
    expect(preview).not.toHaveTextContent(
      i18n.t("usage.config.modelSection.pricePreview.conditional")
    );

    // 条件价条目：edenai 有报价且 conditional=true → 标记出现
    fireEvent.focus(await screen.findByTestId("usage-config-pricing-badge-d-edenai-glm-5.3"));
    const edenaiPreview = screen.getByTestId("usage-config-price-preview-d-edenai-glm-5.3");
    expect(edenaiPreview).toHaveTextContent(
      i18n.t("usage.config.modelSection.pricePreview.conditional")
    );

    restore();
  });

  it("11b. 已覆盖条目的价格表来自目录精确键（quote get）", async () => {
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "edenai",
            modelId: "glm-5.3",
            normalizedModelId: "glm-5-3",
            displayProvider: "edenai",
            pricingProvider: "openai",
            pricingModel: "gpt-5",
          }),
        ]),
      usage_pricing_quote_get: (_cmd, args) =>
        Promise.resolve({
          reference: {
            providerId: (args as { providerId: string }).providerId,
            modelId: (args as { modelId: string }).modelId,
          },
          input: "2",
          cacheRead: null,
          cacheWrite: null,
          output: "8",
          releaseDate: null,
          conditional: false,
          snapshotDate: 1_700_000_000_000,
        }),
    });
    await openConfigPanel("usage-config-model-entry-d-edenai-glm-5.3");

    fireEvent.focus(await screen.findByTestId("usage-config-pricing-badge-d-edenai-glm-5.3"));
    const preview = screen.getByTestId("usage-config-price-preview-d-edenai-glm-5.3");
    expect(preview).toHaveTextContent("$2.00");
    expect(preview).toHaveTextContent("$8.00");

    restore();
  });
});

describe("折叠键无损性（P2-2 回归：sanitize 仅用于 testid，不作折叠依据）", () => {
  it("条目：卫生化同形/键结构同形的展示键不假合并", () => {
    // 字符级碰撞：模型名 "gpt 5" 与 "gpt-5" 的旧折叠键同为 d-OpenAI-gpt-5
    const charCollide = modelGroupsOf([
      observedEntry({ displayProvider: "OpenAI", modelId: "gpt 5", normalizedModelId: "gpt-5" }),
      observedEntry({ displayProvider: "OpenAI", modelId: "gpt-5" }),
    ]);
    expect(charCollide).toHaveLength(2);
    expect(new Set(charCollide.map((group) => group.key)).size).toBe(2);
    expect(charCollide.map((group) => group.modelId)).toEqual(
      expect.arrayContaining(["gpt 5", "gpt-5"])
    );

    // 结构级碰撞：(OpenAI, 5-codex) 与 (OpenAI-5, codex) 的旧键同为 d-OpenAI-5-codex
    const structCollide = modelGroupsOf([
      observedEntry({
        displayProvider: "OpenAI",
        modelId: "5-codex",
        normalizedModelId: "5-codex",
      }),
      observedEntry({ displayProvider: "OpenAI-5", modelId: "codex", normalizedModelId: "codex" }),
    ]);
    expect(structCollide).toHaveLength(2);

    // 空 provider 分支同理：sourceId 相同、模型名空串 vs 字面 "unknown" 是两个条目
    const unknowns = modelGroupsOf([
      observedEntry({
        providerRaw: null,
        sourceId: "workbuddy",
        modelId: "",
        normalizedModelId: "",
      }),
      observedEntry({
        providerRaw: null,
        sourceId: "workbuddy",
        modelId: "unknown",
        normalizedModelId: "unknown",
      }),
    ]);
    expect(unknowns).toHaveLength(2);
    expect(unknowns.map((group) => group.modelId)).toEqual(expect.arrayContaining(["", "unknown"]));
  });

  it("正常折叠不受影响：同展示键多成员仍合并且成员齐全", () => {
    const folded = modelGroupsOf([
      observedEntry({ providerRaw: "openai", displayProvider: "OpenAI", modelId: "gpt-5" }),
      observedEntry({ providerRaw: "azure-openai", displayProvider: "OpenAI", modelId: "gpt-5" }),
    ]);
    expect(folded).toHaveLength(1);
    expect(folded[0].members).toHaveLength(2);
    // 分组携带 7 天用量（成员和），Top10 排序依赖它
    expect(folded[0].recentTokens).toBe(200);
  });
});

describe("高级覆盖区（UsageDisclosure 受控折叠）", () => {
  it("12. 高级区：标题「自定义价格规则」；默认收起；展开才挂载 Manager；旧版脚注已删除", async () => {
    await openConfigPanel();

    const advToggle = screen.getByTestId("usage-config-advanced-settings-toggle");
    expect(advToggle).toHaveAttribute("aria-expanded", "false");
    // 收起时不输出悬空 aria-controls（Nit-5）
    expect(advToggle).not.toHaveAttribute("aria-controls");
    expect(screen.getByTestId("usage-config-advanced-settings-chevron")).not.toHaveAttribute(
      "aria-controls"
    );
    // 收起时内容不挂载：Manager 与 billing 入口都不在场
    expect(screen.queryByTestId("usage-config-advanced-settings-content")).toBeNull();
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    expect(screen.queryByTestId("usage-config-advanced-billing")).toBeNull();

    fireEvent.click(advToggle);
    expect(advToggle).toHaveAttribute("aria-expanded", "true");
    expect(advToggle).toHaveAttribute("aria-controls", "usage-config-advanced-settings-content");
    // 高级标题 + 摘要在标题按钮上（2026-10-21 标题改为「自定义价格规则」）
    const advanced = screen.getByTestId("usage-config-advanced-settings");
    expect(advanced).toHaveTextContent(i18n.t("usage.config.advanced"));
    expect(advanced).toHaveTextContent("自定义价格规则");
    expect(advanced).toHaveTextContent(i18n.t("usage.config.advancedHint"));
    // 展开后 Manager 挂载；高级区 Info 仍在，旧版说明脚注 / 旧版 Info 已删除
    expect(await screen.findByTestId("price-override-manager")).toBeTruthy();
    const content = screen.getByTestId("usage-config-advanced-settings-content");
    expect(content).not.toHaveTextContent(i18n.t("usage.config.legacyMoved"));
    expect(content).not.toHaveTextContent("旧版计费渠道绑定");
    expect(screen.queryByTestId("usage-config-legacy-info-settings")).toBeNull();
    expect(screen.getByTestId("usage-config-advanced-info-settings")).toBeTruthy();

    // chevron 按钮同样可切换收起：Manager 卸载
    fireEvent.click(screen.getByTestId("usage-config-advanced-settings-chevron"));
    await waitFor(() =>
      expect(screen.queryByTestId("usage-config-advanced-settings-content")).toBeNull()
    );
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
  });
});

describe("Info 图标（Radix Tooltip，role=tooltip）", () => {
  /**
   * Radix Tooltip 内容挂在 body 末尾的 portal 里（role=tooltip）；条目行的悬停价格表
   * （PricePreview）同样声明 role=tooltip 且常驻 DOM（CSS hover 显隐，jsdom 不算隐藏）
   * ⇒ 不能 findByRole 等第一个出现（会被 PricePreview 抢先），必须**按说明正文轮询**。
   */
  async function radixTooltipOf(text: string): Promise<HTMLElement> {
    let hit: HTMLElement | undefined;
    await waitFor(
      () => {
        hit = screen.getAllByRole("tooltip").find((el) => el.textContent === text);
        expect(hit, `等待正文为「${text}」的 Info 浮层`).toBeTruthy();
      },
      { timeout: 2_000 }
    );
    return hit as HTMLElement;
  }

  it("13. 外层卡 Info：hover 显示说明浮层", async () => {
    await openConfigPanel(null);

    const user = userEvent.setup();
    await user.hover(screen.getByTestId("usage-config-info-settings"));
    const tooltip = await radixTooltipOf(i18n.t("usage.config.help.panel"));
    expect(tooltip).toBeInTheDocument();
  });

  it("13b. 高级区 Info：focus 显示四价覆盖说明（键盘可达）", async () => {
    await openConfigPanel(null);

    fireEvent.focus(screen.getByTestId("usage-config-advanced-info-settings"));
    const tooltip = await radixTooltipOf(i18n.t("usage.config.help.advanced"));
    expect(tooltip).toBeInTheDocument();
  });
});

describe("条目快捷入口（覆盖价预填）", () => {
  it("14. 空模型（normalizedModelId 为空串）条目的覆盖按钮 disabled", async () => {
    await openConfigPanel();

    expect(screen.getByTestId("usage-config-model-override-d-WorkBuddy-unknown")).toBeDisabled();
  });

  it("15. 新覆盖：点击 glm-5.3 条目入口 → 高级区展开并预填**原始模型名**（可编辑）", async () => {
    await openConfigPanel();
    expect(screen.queryByTestId("price-override-manager")).toBeNull();

    fireEvent.click(screen.getByTestId("usage-config-model-override-d-edenai-glm-5.3"));

    // 高级区自动展开，Manager 随之挂载
    expect(screen.getByTestId("usage-config-advanced-settings-toggle")).toHaveAttribute(
      "aria-expanded",
      "true"
    );
    // 新增路径：模型输入 = 原始模型名 glm-5.3（不是归一化键 glm-5-3），且可编辑；四价为空
    const model = await screen.findByTestId("price-override-model");
    await waitFor(() => expect(model).toHaveValue("glm-5.3"));
    expect(model).not.toBeDisabled();
    expect(screen.getByTestId("price-override-input")).toHaveValue("");
  });

  it("16. 已有覆盖：入口进入编辑态（归一化键只读 + 四价回填）", async () => {
    await openConfigPanel();

    fireEvent.click(screen.getByTestId("usage-config-model-override-d-OpenAI-gpt-5"));

    // 已有同键覆盖 → beginEdit：模型输入显示归一化键且 disabled，四价回填已存值
    const model = await screen.findByTestId("price-override-model");
    await waitFor(() => expect(model).toHaveValue("gpt-5"));
    expect(model).toBeDisabled();
    expect(screen.getByTestId("price-override-input")).toHaveValue("1.25");
    expect(screen.getByTestId("price-override-output")).toHaveValue("10");
  });

  it("17. 已有覆盖徽标 + 入口文案换「编辑覆盖价」；无覆盖条目显示「覆盖此模型价格」", async () => {
    await openConfigPanel();

    // gpt-5 在覆盖表里 → 徽标 + 「编辑覆盖价」
    const badge = screen.getByTestId("usage-config-model-overridden-d-OpenAI-gpt-5");
    expect(badge).toHaveTextContent(i18n.t("usage.config.modelSection.priceOverriddenBadge"));
    expect(screen.getByTestId("usage-config-model-override-d-OpenAI-gpt-5")).toHaveTextContent(
      i18n.t("usage.config.modelSection.editOverridePrice")
    );
    // glm-5.3 无覆盖 → 无徽标 + 「覆盖此模型价格」
    expect(screen.queryByTestId("usage-config-model-overridden-d-edenai-glm-5.3")).toBeNull();
    expect(screen.getByTestId("usage-config-model-override-d-edenai-glm-5.3")).toHaveTextContent(
      i18n.t("usage.config.modelSection.overridePrice")
    );
  });

  it("18. 快捷入口触发 scrollIntoView（P1 回归：prefill 被子组件清空不得取消 rAF；同毫秒重复点击重滚）", async () => {
    await openConfigPanel();

    // setup.ts 已为 jsdom 补 scrollIntoView 空实现；这里替换成 spy 捕获调用。
    // 旧缺陷：effect 依赖 prefill → 高级区挂载后 Manager 消费 prefill 并回传
    // onPrefillHandled → setPrefill(null) 触发的重渲染会走 effect cleanup，
    // 把尚未执行的 rAF cancel 掉 ⇒ 主路径（overrides 查询已就绪）滚动永不发生。
    const spy = vi.spyOn(Element.prototype, "scrollIntoView");
    try {
      fireEvent.click(screen.getByTestId("usage-config-model-override-d-edenai-glm-5.3"));

      // 高级区展开 + Manager 挂载后，rAF 内按 block:"start" 滚到覆盖表
      await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));
      expect(spy).toHaveBeenNthCalledWith(1, { block: "start" });

      // 同毫秒二次点击必须再次滚动（P2-4：nonce 用 ref 递增，不依赖 Date.now）
      fireEvent.click(screen.getByTestId("usage-config-model-override-d-edenai-glm-5.3"));
      await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));
      expect(spy).toHaveBeenNthCalledWith(2, { block: "start" });
    } finally {
      spy.mockRestore();
    }
  });
});

describe("弹窗壳（设置页与 D 卡共用）", () => {
  it("19. dialog 角色 + 唯一 testid + 标题/描述在弹窗头；长模型名截断带 title", async () => {
    const longModel = "a-very-long-model-identifier-that-must-never-stretch-the-dialog-row";
    const restore = override({
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: "edenai",
            modelId: longModel,
            normalizedModelId: longModel,
            displayProvider: "edenai",
          }),
        ]),
    });
    await openConfigPanel(
      "usage-config-model-entry-d-edenai-a-very-long-model-identifier-that-must-never-stretch-the-dialog-row"
    );

    // 弹窗壳回归：role=dialog + instanceId 后缀 testid；正文面板在弹窗里
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("data-testid", "usage-config-dialog-settings");
    expect(within(dialog).getByTestId("usage-config-panel-settings")).toBeTruthy();
    expect(within(dialog).getByTestId("usage-config-info-settings")).toBeTruthy();
    // 宽度与主看板窗口一致：初始 1040px、最小 840px（受视口约束），旧 1100px 上限退场
    expect(dialog.className).toContain("w-[1040px]");
    expect(dialog.className).toContain("min-w-[min(840px,calc(100vw-2rem))]");
    expect(dialog.className).not.toContain("max-w-[min(1100px");

    // 行内截断纪律保留：原始证据 span truncate + 完整 title
    const raw = screen.getByTestId(
      "usage-config-model-raw-d-edenai-a-very-long-model-identifier-that-must-never-stretch-the-dialog-row"
    );
    expect(raw.className).toContain("truncate");
    expect(raw.getAttribute("title")).toContain(longModel);

    restore();
  });

  it("20. 空观测条目：条目区空态文案", async () => {
    const restore = override({
      usage_observed_entries_get: () => Promise.resolve([]),
    });
    await openConfigPanel(null);

    // 空态文案（aliases 查询就绪前 section 内是 loading，所以要 waitFor 文案本身）
    expect(await screen.findByText(i18n.t("usage.config.modelSection.empty"))).toBeTruthy();

    restore();
  });
});
