// Task 17 前端 · 定价状态面板 + 参考 API 模型编辑。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import i18n from "@/i18n";
import { PricingStatusPanel } from "@/components/usage/PricingStatusPanel";
import { BillingProfilesManager } from "@/components/usage/BillingProfilesManager";
import { usageBillingCommit } from "@/lib/api/usageV2";
import { resetUsageV2MockState } from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type {
  BillingConfig,
  BillingRule,
  IdentityUsage,
  PricingCatalogEntry,
  PricingRule,
  PricingStatus,
} from "@/types/usageV2";

const tf = i18n.t.bind(i18n);

function wrap(
  children: ReactNode,
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

type Impl = (cmd: string, args?: unknown) => Promise<unknown>;

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

const EMPTY: PricingStatus = {
  enabled: false,
  snapshotId: null,
  fetchedAt: null,
  stale: true,
  refreshing: false,
  lastError: null,
};

const FRESH: PricingStatus = {
  enabled: true,
  snapshotId: "snap-1",
  fetchedAt: 1_700_000_000_000,
  stale: false,
  refreshing: false,
  lastError: null,
};

const FAILED: PricingStatus = {
  ...FRESH,
  stale: true,
  lastError: "download failed",
};

const RULE: BillingRule = {
  id: "r1",
  identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
  match: "exact",
  billingProfileId: "p1",
  effectiveFrom: 0,
  effectiveTo: null,
  mixed: false,
};

const PRICING_RULE: PricingRule = {
  id: "pr1",
  identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
  match: "exact",
  reference: { providerId: "openai", modelId: "gpt-5" },
};

function observedIdentity(): IdentityUsage {
  return {
    identity: { sourceId: "claude", observedProvider: "anthropic", modelId: "claude-sonnet" },
    sourceKind: "measured",
    attribution: {
      billingProfileId: null,
      attributionKind: "unknown",
      ruleIds: [],
      reason: "unassigned",
    },
    buckets: { inputFresh: 10, cacheRead: 0, cacheWrite: 0, output: 5 },
    metrics: { requestTotal: 10, cacheHitRate: 0, userEst: null, requests: 1 },
  };
}

function identityOptionValue(): string {
  const select = screen.getByTestId("pricing-identity-select");
  const option = Array.from(select.querySelectorAll("option")).find(
    (item) => item.value !== ""
  ) as HTMLOptionElement;
  return option.value;
}

function chooseFirstIdentity() {
  const select = screen.getByTestId("pricing-identity-select");
  fireEvent.change(select, { target: { value: identityOptionValue() } });
}

function fillReference(providerId = "openai", modelId = "gpt-5") {
  fireEvent.change(screen.getByTestId("pricing-provider-id"), {
    target: { value: providerId },
  });
  fireEvent.change(screen.getByTestId("pricing-model-id"), {
    target: { value: modelId },
  });
}

function config(): BillingConfig {
  return {
    revision: 1,
    profiles: [{ id: "p1", name: "P1", kind: "api", note: "", archived: false }],
    rules: [RULE],
    pricingRules: [],
  };
}

async function seedBilling() {
  await usageBillingCommit({
    expectedRevision: 0,
    candidate: { ...config(), revision: 0 },
  });
}

async function seedBillingWith(pricingRules: PricingRule[]) {
  await usageBillingCommit({
    expectedRevision: 0,
    candidate: { ...config(), revision: 0, pricingRules },
  });
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  resetUsageV2MockState();
  tauriInvokeMock.mockClear();
});

describe("PricingStatusPanel", () => {
  it("1. 首次未开启：说明 + 隐私边界；点击开启后显示后台刷新", async () => {
    let state = EMPTY;
    const restore = override({
      usage_pricing_get: () => Promise.resolve(state),
      usage_pricing_set_enabled: (cmd, args) => {
        const enabled = (args as { enabled?: boolean } | undefined)?.enabled ?? false;
        state = { ...FRESH, enabled, snapshotId: enabled ? null : null, refreshing: enabled };
        return Promise.resolve(state);
      },
    });

    render(wrap(<PricingStatusPanel t={tf} />));
    expect(await screen.findByTestId("usage-pricing-enable")).toBeTruthy();
    expect(screen.getByText(/只下载公开价目表，不上传本地用量/)).toBeTruthy();
    // 长 snapshot ID 不再展示；只保留快照获取时间与数据源
    expect(screen.queryByTestId("usage-pricing-snapshot")).toBeNull();
    expect(screen.getByTestId("usage-pricing-fetched-at")).toHaveTextContent("快照获取时间");
    expect(screen.getAllByText(/models\.dev/).length).toBeGreaterThan(0);

    fireEvent.click(screen.getByTestId("usage-pricing-enable"));
    await waitFor(() =>
      expect(callsOf("usage_pricing_set_enabled")[0][1]).toEqual({ enabled: true })
    );
    await waitFor(() =>
      expect(screen.getByTestId("usage-pricing-refreshing")).toHaveTextContent("正在刷新")
    );
    // 开启不等于手动刷新按钮
    expect(callsOf("usage_pricing_refresh")).toHaveLength(0);

    restore();
  });

  it("2. 失败显示上次有效时间与重试；重试调用手动刷新", async () => {
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FAILED),
      usage_pricing_refresh: () => Promise.resolve(FAILED),
    });

    render(wrap(<PricingStatusPanel t={tf} />));
    const error = await screen.findByTestId("usage-pricing-error");
    expect(error).toHaveTextContent("download failed");
    expect(screen.getByTestId("usage-pricing-fetched-at")).toHaveTextContent(/\d{4}/);
    expect(screen.getByTestId("usage-pricing-stale")).toBeTruthy();
    // 长快照 ID 不再出现在界面
    expect(screen.queryByText("snap-1")).toBeNull();

    fireEvent.click(screen.getByTestId("usage-pricing-refresh"));
    await waitFor(() => expect(callsOf("usage_pricing_refresh")).toHaveLength(1));

    restore();
  });

  it("3. 关闭开关只调用 setEnabled(false)，不触发刷新", async () => {
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FRESH),
      usage_pricing_set_enabled: () => Promise.resolve({ ...FRESH, enabled: false }),
    });

    render(wrap(<PricingStatusPanel t={tf} />));
    await screen.findByTestId("usage-pricing-toggle");
    fireEvent.click(screen.getByTestId("usage-pricing-toggle"));

    await waitFor(() =>
      expect(callsOf("usage_pricing_set_enabled")[0][1]).toEqual({ enabled: false })
    );
    expect(callsOf("usage_pricing_refresh")).toHaveLength(0);

    restore();
  });
});

describe("BillingProfilesManager · 参考 API 模型", () => {
  it("4. 必须显式选择身份/输入精确键/勾选授权后才保存为独立 PricingRule", async () => {
    await seedBilling();
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    // 无自动化保存：未点击前零 commit（除 seed 外）
    tauriInvokeMock.mockClear();
    expect(screen.getByTestId("pricing-save")).toBeDisabled();
    // Task 17 复审：过期占位文案已删除
    expect(screen.queryByText(/后续任务/)).toBeNull();
    expect(screen.queryByText(/later task/i)).toBeNull();

    chooseFirstIdentity();
    fireEvent.change(screen.getByTestId("pricing-provider-id"), {
      target: { value: "openai" },
    });
    fireEvent.change(screen.getByTestId("pricing-model-id"), {
      target: { value: "gpt-5" },
    });
    expect(screen.getByTestId("pricing-save")).toBeDisabled();
    expect(callsOf("usage_billing_commit")).toHaveLength(0);

    // 授权确认是显式的一步
    fireEvent.click(screen.getByTestId("pricing-authorize"));
    expect(screen.getByTestId("pricing-save")).toBeEnabled();
    expect(screen.getByTestId("pricing-target-preview")).toHaveTextContent("openai / gpt-5");
    fireEvent.click(screen.getByTestId("pricing-save"));

    await waitFor(() => expect(callsOf("usage_billing_commit")).toHaveLength(1));
    const request = callsOf("usage_billing_commit")[0][1]?.request as {
      expectedRevision: number;
      candidate: BillingConfig;
    };
    expect(request.expectedRevision).toBe(1);
    expect(request.candidate.pricingRules).toHaveLength(1);
    expect(request.candidate.pricingRules[0]).toMatchObject({
      identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
      match: "exact",
      reference: { providerId: "openai", modelId: "gpt-5" },
    });
  });

  it("5. 无快照/键不存在：显示错误与先启用/刷新提示，保留草稿", async () => {
    await seedBilling();
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    chooseFirstIdentity();
    fireEvent.change(screen.getByTestId("pricing-provider-id"), {
      target: { value: "openai" },
    });
    fireEvent.change(screen.getByTestId("pricing-model-id"), {
      target: { value: "gpt-5" },
    });
    fireEvent.click(screen.getByTestId("pricing-authorize"));

    const restore = override({
      usage_billing_commit: () =>
        Promise.reject({
          code: "usage-settings-invalid",
          detail: "pricing catalog unavailable",
        }),
    });
    fireEvent.click(screen.getByTestId("pricing-save"));

    const error = await screen.findByTestId("pricing-error");
    expect(error).toHaveTextContent("pricing catalog unavailable");
    expect(screen.getByTestId("pricing-error")).toHaveTextContent(/先启用|刷新/);
    // 草稿保留
    expect(screen.getByTestId("pricing-provider-id")).toHaveValue("openai");
    expect(screen.getByTestId("pricing-model-id")).toHaveValue("gpt-5");

    restore();
  });

  it("6. 渠道改名不改 ID / 参考规则", async () => {
    await seedBilling();
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    const name = await screen.findByTestId("billing-profile-name-p1");
    tauriInvokeMock.mockClear();
    fireEvent.change(name, { target: { value: "Renamed" } });
    fireEvent.blur(name);

    await waitFor(() => expect(callsOf("usage_billing_commit")).toHaveLength(1));
    const request = callsOf("usage_billing_commit")[0][1]?.request as {
      candidate: BillingConfig;
    };
    expect(request.candidate.profiles[0]).toMatchObject({ id: "p1", name: "Renamed" });
    expect(request.candidate.pricingRules).toHaveLength(0);
    expect(request.candidate.rules[0].billingProfileId).toBe("p1");
  });

  it("7. 身份 select 用稳定 IdentityKey；授权后改目标会取消授权并禁用保存", async () => {
    await seedBilling();
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    const select = screen.getByTestId("pricing-identity-select");
    const option = Array.from(select.querySelectorAll("option")).find(
      (item) => item.value !== ""
    ) as HTMLOptionElement;
    // 稳定键 = source|provider|model，而不是数组索引
    expect(option.value).not.toBe("0");
    expect(option.value).toContain("codex");

    chooseFirstIdentity();
    fillReference();
    fireEvent.click(screen.getByTestId("pricing-authorize"));
    expect(screen.getByTestId("pricing-save")).toBeEnabled();

    // 目标变化 → 授权自动取消、保存再次禁用
    fireEvent.change(screen.getByTestId("pricing-model-id"), {
      target: { value: "gpt-5-mini" },
    });
    expect(screen.getByTestId("pricing-authorize")).not.toBeChecked();
    expect(screen.getByTestId("pricing-save")).toBeDisabled();
  });

  it("8. 空配置也能选择未归属观测身份配置参考模型", async () => {
    const restore = override({
      usage_identities: () => Promise.resolve([observedIdentity()]),
    });
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));

    const select = await screen.findByTestId("pricing-identity-select");
    await waitFor(() => {
      const options = Array.from(select.querySelectorAll("option"));
      expect(options.some((option) => option.textContent?.includes("anthropic"))).toBe(true);
    });
    const option = Array.from(select.querySelectorAll("option")).find((item) =>
      item.textContent?.includes("anthropic")
    ) as HTMLOptionElement;
    fireEvent.change(select, { target: { value: option.value } });
    fillReference("anthropic", "claude-sonnet");
    fireEvent.click(screen.getByTestId("pricing-authorize"));
    fireEvent.click(screen.getByTestId("pricing-save"));

    await waitFor(() => expect(callsOf("usage_billing_commit")).toHaveLength(1));
    const request = callsOf("usage_billing_commit")[0][1]?.request as {
      candidate: BillingConfig;
    };
    expect(request.candidate.pricingRules[0].identity).toEqual({
      sourceId: "claude",
      observedProvider: "anthropic",
      modelId: "claude-sonnet",
    });

    restore();
  });

  it("9. 移除参考规则走离线 commit，不调用定价网络命令", async () => {
    await seedBillingWith([PRICING_RULE]);
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("pricing-rule-pr1");

    tauriInvokeMock.mockClear();
    fireEvent.click(screen.getByTestId("pricing-rule-remove-pr1"));
    await waitFor(() => expect(callsOf("usage_billing_commit")).toHaveLength(1));
    const request = callsOf("usage_billing_commit")[0][1]?.request as {
      candidate: BillingConfig;
    };
    expect(request.candidate.pricingRules).toHaveLength(0);
    // 只读本地定价状态可以发生；刷新/搜索网络命令一律不得触发。
    expect(callsOf("usage_pricing_refresh")).toHaveLength(0);
    expect(callsOf("usage_pricing_catalog")).toHaveLength(0);
  });

  it("10. PricingStatusPanel 订阅事件并去重 refreshing 文案", async () => {
    const restore = override({
      usage_pricing_get: () => Promise.resolve({ ...FRESH, refreshing: true }),
      usage_pricing_refresh: () => new Promise<PricingStatus>(() => {}),
    });
    const mockedListen = vi.mocked(listen);
    const original = mockedListen.getMockImplementation();
    let fire: (() => void) | null = null;
    mockedListen.mockImplementation(async (_event, handler) => {
      fire = () => (handler as () => void)();
      return () => {};
    });

    render(wrap(<PricingStatusPanel t={tf} />));
    await screen.findByTestId("usage-pricing-refresh");
    await waitFor(() => expect(fire).not.toBeNull());

    // 事件失效 → pricing 重取
    const before = callsOf("usage_pricing_get").length;
    fire!();
    await waitFor(() => expect(callsOf("usage_pricing_get").length).toBeGreaterThan(before));

    // 手动刷新进行中：状态 refreshing + busy 只出一条「正在刷新…」
    fireEvent.click(screen.getByTestId("usage-pricing-refresh"));
    await waitFor(() => expect(screen.getAllByText(/正在刷新/).length).toBe(1));

    if (original) mockedListen.mockImplementation(original);
    restore();
  });

  it("11. actionError 在成功刷新后清空", async () => {
    let attempt = 0;
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FRESH),
      usage_pricing_refresh: () => {
        attempt += 1;
        return attempt === 1 ? Promise.reject(new Error("refresh boom")) : Promise.resolve(FRESH);
      },
    });
    render(wrap(<PricingStatusPanel t={tf} />));
    await screen.findByTestId("usage-pricing-refresh");

    fireEvent.click(screen.getByTestId("usage-pricing-refresh"));
    await screen.findByTestId("usage-pricing-error");
    fireEvent.click(screen.getByTestId("usage-pricing-refresh"));
    await waitFor(() => expect(screen.queryByTestId("usage-pricing-error")).toBeNull());

    restore();
  });

  it("12. 错误 detail 区分「无快照」与「键缺失」", async () => {
    await seedBilling();
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    chooseFirstIdentity();
    fillReference();
    fireEvent.click(screen.getByTestId("pricing-authorize"));

    let detail = "pricing catalog unavailable";
    const restore = override({
      usage_billing_commit: () => Promise.reject({ code: "usage-settings-invalid", detail }),
    });
    fireEvent.click(screen.getByTestId("pricing-save"));
    const noSnapshot = await screen.findByTestId("pricing-error");
    expect(noSnapshot).toHaveTextContent(/无有效价目快照/);

    // 改目标同时清错误/取消授权
    fireEvent.change(screen.getByTestId("pricing-model-id"), {
      target: { value: "gpt-5-mini" },
    });
    expect(screen.queryByTestId("pricing-error")).toBeNull();
    fireEvent.click(screen.getByTestId("pricing-authorize"));
    // 逐字对齐后端 billing.rs `validate_pricing_references` 的真实 detail
    //（pricingHintText 按前缀登记依赖，后端改文案须同步）
    detail = "pricing reference not found: openai/gpt-5";
    fireEvent.click(screen.getByTestId("pricing-save"));
    const keyMissing = await screen.findByTestId("pricing-error");
    expect(keyMissing).toHaveTextContent(/参考模型键不存在/);

    restore();
  });

  // ── 手工测试回归：目录可搜索下拉 ────────────────────────────────────────

  const CATALOG: PricingCatalogEntry[] = [
    {
      providerId: "ollama-cloud",
      modelId: "deepseek-v4.1-flash",
      releaseDate: "2025-09-01",
      hasRates: true,
      conditional: false,
    },
    {
      providerId: "zhipuai-coding-plan",
      modelId: "glm-5.3",
      releaseDate: "2025-10-01",
      hasRates: true,
      conditional: true,
    },
    {
      providerId: "ollama-cloud",
      modelId: "kimi-k2",
      releaseDate: null,
      hasRates: false,
      conditional: false,
    },
  ];

  async function openManagerWithCatalog(
    catalog: PricingCatalogEntry[] = CATALOG,
    pricing: PricingStatus = FRESH
  ) {
    await seedBilling();
    const restore = override({
      usage_pricing_get: () => Promise.resolve(pricing),
      usage_pricing_catalog: (_cmd, args) => {
        const { limit } = (args ?? {}) as { query?: string; limit?: number };
        // 与真实后端同序：按 provider/model 字典序。
        const sorted = [...catalog].sort((a, b) =>
          `${a.providerId}\u0000${a.modelId}`.localeCompare(`${b.providerId}\u0000${b.modelId}`)
        );
        return Promise.resolve(sorted.slice(0, limit ?? 50));
      },
    });
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");
    return restore;
  }

  function searchCatalog(value: string) {
    fireEvent.change(screen.getByTestId("pricing-catalog-search"), { target: { value } });
  }

  it("13. 搜索目录 → 选中 → 自动填充两个输入框 → 仍可手改 → 保存参数正确", async () => {
    const restore = await openManagerWithCatalog();
    tauriInvokeMock.mockClear();
    chooseFirstIdentity();

    searchCatalog("DEEP");
    const option = await screen.findByRole("option", {
      name: /ollama-cloud \/ deepseek-v4\.1-flash/,
    });
    // 行内标记：发布日 / 条件价 / 无费率
    expect(option).toHaveTextContent("2025-09-01");
    expect(
      screen.getByRole("option", { name: /zhipuai-coding-plan \/ glm-5\.3/ })
    ).toHaveTextContent("条件价");
    expect(screen.getByRole("option", { name: /ollama-cloud \/ kimi-k2/ })).toHaveTextContent(
      "无费率"
    );

    fireEvent.click(option);
    expect(screen.getByTestId("pricing-provider-id")).toHaveValue("ollama-cloud");
    expect(screen.getByTestId("pricing-model-id")).toHaveValue("deepseek-v4.1-flash");
    // 不自动勾选授权；改动目标会取消授权（显式授权纪律）
    expect(screen.getByTestId("pricing-authorize")).not.toBeChecked();
    expect(screen.getByTestId("pricing-save")).toBeDisabled();

    // 选中后仍可继续手改
    fireEvent.change(screen.getByTestId("pricing-model-id"), {
      target: { value: "deepseek-v4.1-flash-custom" },
    });
    fireEvent.click(screen.getByTestId("pricing-authorize"));
    fireEvent.click(screen.getByTestId("pricing-save"));

    await waitFor(() => expect(callsOf("usage_billing_commit")).toHaveLength(1));
    const request = callsOf("usage_billing_commit")[0][1]?.request as {
      candidate: BillingConfig;
    };
    expect(request.candidate.pricingRules[0].reference).toEqual({
      providerId: "ollama-cloud",
      modelId: "deepseek-v4.1-flash-custom",
    });
    // IPC 参数逐字：query + limit（hook 固定 20 条）
    const catalogCall = callsOf("usage_pricing_catalog")[0];
    expect(catalogCall[1]).toEqual({ query: "DEEP", limit: 20 });

    restore();
  });

  it("13b. 搜索结果每行可复制 provider/model；复制不触发选中", async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    const restore = await openManagerWithCatalog();

    searchCatalog("DEEP");
    await screen.findByRole("listbox");
    fireEvent.click(screen.getByTestId("pricing-catalog-copy-ollama-cloud/deepseek-v4.1-flash"));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith("ollama-cloud/deepseek-v4.1-flash"));
    // 复制按钮不代替「选中」：两个输入框仍为空
    expect(screen.getByTestId("pricing-provider-id")).toHaveValue("");
    expect(screen.getByTestId("pricing-model-id")).toHaveValue("");

    restore();
  });

  it("14. 无快照：显示先启用/刷新提示，且不发起目录搜索", async () => {
    const restore = await openManagerWithCatalog(CATALOG, EMPTY);
    tauriInvokeMock.mockClear();

    searchCatalog("deep");
    expect(await screen.findByTestId("pricing-catalog-no-snapshot")).toHaveTextContent(/启用|刷新/);
    expect(callsOf("usage_pricing_catalog")).toHaveLength(0);
    expect(screen.queryByRole("listbox")).toBeNull();

    restore();
  });

  it("14b. 无快照缓存 → usage-pricing-updated 后搜索框可用并返回结果", async () => {
    let pricing = EMPTY;
    await seedBilling();
    const restore = override({
      usage_pricing_get: () => Promise.resolve(pricing),
      usage_pricing_catalog: () => Promise.resolve(CATALOG),
    });

    const mockedListen = vi.mocked(listen);
    const original = mockedListen.getMockImplementation();
    let fire: (() => void) | undefined;
    mockedListen.mockImplementation(async (event, handler) => {
      if (event === "usage-pricing-updated") fire = handler as () => void;
      return () => {};
    });

    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");
    expect(await screen.findByTestId("pricing-catalog-no-snapshot")).toBeTruthy();

    await waitFor(() => expect(fire).toBeDefined());
    pricing = FRESH;
    await act(async () => {
      fire!();
    });
    await waitFor(() => expect(screen.queryByTestId("pricing-catalog-no-snapshot")).toBeNull());

    searchCatalog("deep");
    expect(
      await screen.findByRole("option", { name: /ollama-cloud \/ deepseek-v4\.1-flash/ })
    ).toBeTruthy();

    if (original) mockedListen.mockImplementation(original);
    restore();
  });

  it("14c. 打开对话框时强制刷新 pricing（缓存里的无快照不再挡搜索）", async () => {
    await seedBilling();
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    // 模拟「真实库已有快照，但前端缓存仍是旧的空状态」
    client.setQueryData(["usage", "v2", "pricing"], EMPTY);
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FRESH),
      usage_pricing_catalog: () => Promise.resolve(CATALOG),
    });

    render(wrap(<BillingProfilesManager t={tf} open onOpenChange={() => {}} />, client));
    await screen.findByTestId("billing-rule-r1");
    await waitFor(() => expect(callsOf("usage_pricing_get").length).toBeGreaterThan(0));
    expect(screen.queryByTestId("pricing-catalog-no-snapshot")).toBeNull();

    searchCatalog("deep");
    expect(
      await screen.findByRole("option", { name: /ollama-cloud \/ deepseek-v4\.1-flash/ })
    ).toBeTruthy();

    restore();
  });

  it("15. 查询失败显示错误与重试；空结果显示空态", async () => {
    let attempt = 0;
    await seedBilling();
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FRESH),
      usage_pricing_catalog: () => {
        attempt += 1;
        return attempt === 1
          ? Promise.reject({ code: "usage-db-failed", detail: "catalog boom" })
          : Promise.resolve([]);
      },
    });
    render(wrap(<BillingProfilesManager t={tf} />));
    fireEvent.click(await screen.findByTestId("billing-manager-open"));
    await screen.findByTestId("billing-rule-r1");

    searchCatalog("zzz");
    const error = await screen.findByTestId("pricing-catalog-error");
    expect(error).toHaveTextContent("catalog boom");

    fireEvent.click(screen.getByTestId("pricing-catalog-retry"));
    expect(await screen.findByTestId("pricing-catalog-empty")).toBeTruthy();
    expect(screen.queryByTestId("pricing-catalog-error")).toBeNull();

    restore();
  });

  it("16. 键盘：ArrowDown 高亮 + Enter 选中 + Escape 收起", async () => {
    const restore = await openManagerWithCatalog();
    const input = screen.getByTestId("pricing-catalog-search");

    searchCatalog("ollama");
    await screen.findByRole("listbox");
    input.focus();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(screen.getByTestId("pricing-provider-id")).toHaveValue("ollama-cloud");
    expect(screen.getByTestId("pricing-model-id")).toHaveValue("kimi-k2");
    expect(screen.queryByRole("listbox")).toBeNull();

    // Escape：重新打开结果后收起，不改变已填值
    fireEvent.focus(input);
    expect(await screen.findByRole("listbox")).toBeTruthy();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(screen.getByTestId("pricing-model-id")).toHaveValue("kimi-k2");

    restore();
  });
});
