// Task 8 · 手动覆盖价 UI。
// 2026-10-20 第三轮：Manager 从定价状态面板移入配置卡「高级覆盖」（本文件直接渲染 Manager
// 或通过 prefill prop 驱动）；状态面板只保留迁移提示。
// 覆盖：列表四价/来源/日期/覆盖标记；新增（空串→null、至少一项）；编辑（模型键只读）；
// 删除二次确认；非法数字拒绝；无目录来源仍可保存；保存后统一失效 USAGE_KEY；
// 条目快捷入口 prefill（新建预填原始模型名 / 已有覆盖进入编辑态）。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import i18n from "@/i18n";
import { PriceOverrideManager } from "@/components/usage/PriceOverrideManager";
import { PricingStatusPanel } from "@/components/usage/PricingStatusPanel";
import { USAGE_KEY } from "@/lib/query/queries/usageV2";
import { resetUsageV2MockState } from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { PricingStatus } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);

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

function callsOf(cmd: string): Array<[string, Record<string, unknown> | undefined]> {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as Array<
    [string, Record<string, unknown> | undefined]
  >;
}

const FRESH: PricingStatus = {
  enabled: true,
  snapshotId: "snap-1",
  fetchedAt: 1_700_000_000_000,
  stale: false,
  refreshing: false,
  lastError: null,
};

function openAddForm() {
  fireEvent.click(screen.getByTestId("price-override-add"));
}

function fillForm(fields: {
  model?: string;
  input?: string;
  cacheRead?: string;
  cacheWrite?: string;
  output?: string;
}) {
  if (fields.model !== undefined) {
    fireEvent.change(screen.getByTestId("price-override-model"), {
      target: { value: fields.model },
    });
  }
  if (fields.input !== undefined) {
    fireEvent.change(screen.getByTestId("price-override-input"), {
      target: { value: fields.input },
    });
  }
  if (fields.cacheRead !== undefined) {
    fireEvent.change(screen.getByTestId("price-override-cache-read"), {
      target: { value: fields.cacheRead },
    });
  }
  if (fields.cacheWrite !== undefined) {
    fireEvent.change(screen.getByTestId("price-override-cache-write"), {
      target: { value: fields.cacheWrite },
    });
  }
  if (fields.output !== undefined) {
    fireEvent.change(screen.getByTestId("price-override-output"), {
      target: { value: fields.output },
    });
  }
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  resetUsageV2MockState();
  tauriInvokeMock.mockClear();
});

describe("Task 8 · 手动覆盖价管理", () => {
  it("1. 列表展示四价、目录来源、发布日期、快照/更新日期与用户覆盖标记", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));

    const row = await screen.findByTestId("price-override-row-gpt-5");
    expect(row).toHaveTextContent("gpt-5");
    expect(screen.getByTestId("price-override-input-gpt-5")).toHaveTextContent("1.25");
    expect(screen.getByTestId("price-override-cache-read-gpt-5")).toHaveTextContent("0.125");
    expect(screen.getByTestId("price-override-cache-write-gpt-5")).toHaveTextContent("—");
    expect(screen.getByTestId("price-override-output-gpt-5")).toHaveTextContent("10");

    expect(screen.getByTestId("price-override-source-gpt-5")).toHaveTextContent("openai / gpt-5");
    expect(screen.getByTestId("price-override-release-gpt-5")).toHaveTextContent("2025-08-07");
    expect(screen.getByTestId("price-override-snapshot-gpt-5")).toHaveTextContent(
      /\d{4}-\d{2}-\d{2}/
    );
    expect(screen.getByTestId("price-override-updated-gpt-5")).toHaveTextContent(
      /\d{4}-\d{2}-\d{2}/
    );
    expect(screen.getByTestId("price-override-badge-gpt-5")).toHaveTextContent("用户覆盖价");
  });

  it("2. 新增：空串提交 null；保存走 override_set 并统一失效 USAGE_KEY；无目录来源仍可保存", async () => {
    const client = makeClient();
    const invalidate = vi.spyOn(client, "invalidateQueries");
    render(wrap(<PriceOverrideManager t={tf} />, client));

    await screen.findByTestId("price-override-row-gpt-5");
    openAddForm();
    fillForm({
      model: "OpenAI/GPT-5.1-Codex",
      input: "3",
      cacheWrite: "0",
    });
    fireEvent.click(screen.getByTestId("price-override-save"));

    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_override_set")[0][1]).toEqual({
      request: {
        normalizedModelId: "OpenAI/GPT-5.1-Codex",
        currency: "USD",
        input: "3",
        cacheRead: null,
        cacheWrite: "0",
        output: null,
      },
    });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: USAGE_KEY }));

    // mock 归一化后 = gpt-5-1-codex；该模型不在初始目录夹具 → 无目录来源；币种徽标 = USD
    const row = await screen.findByTestId("price-override-row-gpt-5-1-codex");
    expect(screen.getByTestId("price-override-currency-gpt-5-1-codex")).toHaveTextContent("USD");
    expect(screen.getByTestId("price-override-source-gpt-5-1-codex")).toHaveTextContent(
      "无有效目录来源"
    );
    expect(row).toHaveTextContent("3");
    expect(screen.queryByTestId("price-override-form")).toBeNull();
    expect(screen.getByTestId("price-override-saved")).toBeTruthy();
  });

  it("3. 编辑：模型键只读、四价预填；清空某项提交 null", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));

    const row = await screen.findByTestId("price-override-row-gpt-5");
    fireEvent.click(within(row).getByTestId("price-override-edit-gpt-5"));

    const model = screen.getByTestId("price-override-model");
    expect(model).toBeDisabled();
    expect(model).toHaveValue("gpt-5");
    expect(screen.getByTestId("price-override-input")).toHaveValue("1.25");
    expect(screen.getByTestId("price-override-cache-read")).toHaveValue("0.125");
    expect(screen.getByTestId("price-override-cache-write")).toHaveValue("");
    expect(screen.getByTestId("price-override-output")).toHaveValue("10");

    fillForm({ input: "2.5", output: "" });
    fireEvent.click(screen.getByTestId("price-override-save"));

    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_override_set")[0][1]).toEqual({
      request: {
        normalizedModelId: "gpt-5",
        currency: "USD",
        input: "2.5",
        cacheRead: "0.125",
        cacheWrite: null,
        output: null,
      },
    });
    await waitFor(() =>
      expect(screen.getByTestId("price-override-input-gpt-5")).toHaveTextContent("2.5")
    );
    expect(screen.getByTestId("price-override-output-gpt-5")).toHaveTextContent("—");
  });

  it("3b. 币种切换：默认 USD；切 CNY 后请求带 currency=CNY、列表徽标换 CNY、编辑回填已存币种", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));

    // 初始夹具的 gpt-5 覆盖价是 USD；徽标在场
    await screen.findByTestId("price-override-row-gpt-5");
    expect(screen.getByTestId("price-override-currency-gpt-5")).toHaveTextContent("USD");

    openAddForm();
    // 默认 USD 高亮
    expect(screen.getByTestId("price-override-currency-usd")).toHaveAttribute(
      "aria-pressed",
      "true"
    );
    expect(screen.getByTestId("price-override-currency-cny")).toHaveAttribute(
      "aria-pressed",
      "false"
    );
    // 价目输入的括号单位跟随币种
    expect(screen.getByText("新增输入价 (USD)")).toBeTruthy();

    fireEvent.click(screen.getByTestId("price-override-currency-cny"));
    expect(screen.getByTestId("price-override-currency-cny")).toHaveAttribute(
      "aria-pressed",
      "true"
    );
    expect(screen.getByText("新增输入价 (CNY)")).toBeTruthy();

    fillForm({ model: "glm-5.3", input: "8" });
    fireEvent.click(screen.getByTestId("price-override-save"));
    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_override_set")[0][1]).toEqual({
      request: {
        normalizedModelId: "glm-5.3",
        currency: "CNY",
        input: "8",
        cacheRead: null,
        cacheWrite: null,
        output: null,
      },
    });
    await waitFor(() =>
      expect(screen.getByTestId("price-override-currency-glm-5-3")).toHaveTextContent("CNY")
    );

    // 编辑已存覆盖价：币种按钮回填已存值（CNY 高亮），不回默认 USD
    fireEvent.click(
      within(await screen.findByTestId("price-override-row-glm-5-3")).getByTestId(
        "price-override-edit-glm-5-3"
      )
    );
    expect(screen.getByTestId("price-override-currency-cny")).toHaveAttribute(
      "aria-pressed",
      "true"
    );
    expect(screen.getByTestId("price-override-currency-usd")).toHaveAttribute(
      "aria-pressed",
      "false"
    );
  });

  it("4. 删除需二次确认：确认前零调用，确认后按归一化模型 ID 删除", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));

    const row = await screen.findByTestId("price-override-row-gpt-5");
    fireEvent.click(within(row).getByTestId("price-override-delete-gpt-5"));

    expect(screen.getByTestId("price-override-confirm-gpt-5")).toHaveTextContent(
      tf("usage.pricing.overrides.deleteConfirm")
    );
    // 可访问性：确认区为 live region，焦点进入确认按钮，触发按钮反映展开态
    expect(screen.getByTestId("price-override-confirm-gpt-5")).toHaveAttribute("role", "alert");
    expect(screen.getByTestId("price-override-confirm-gpt-5")).toHaveAttribute(
      "aria-live",
      "polite"
    );
    expect(screen.getByTestId("price-override-delete-gpt-5")).toHaveAttribute(
      "aria-expanded",
      "true"
    );
    expect(screen.getByTestId("price-override-delete-confirm-gpt-5")).toHaveFocus();
    expect(callsOf("usage_pricing_override_delete")).toHaveLength(0);

    fireEvent.click(screen.getByTestId("price-override-delete-confirm-gpt-5"));
    await waitFor(() => expect(callsOf("usage_pricing_override_delete")).toHaveLength(1));
    expect(callsOf("usage_pricing_override_delete")[0][1]).toEqual({
      normalizedModelId: "gpt-5",
    });

    await waitFor(() => expect(screen.queryByTestId("price-override-row-gpt-5")).toBeNull());
    expect(screen.getByText(tf("usage.pricing.overrides.empty"))).toBeTruthy();
  });

  it("5. 校验：至少一项非空；非数字/超过 9 位小数拒绝且不发请求、草稿保留", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));
    await screen.findByTestId("price-override-row-gpt-5");
    openAddForm();
    fillForm({ model: "my-model" });

    fireEvent.click(screen.getByTestId("price-override-save"));
    expect(await screen.findByTestId("price-override-form-error")).toHaveTextContent(
      "至少填写一项价格"
    );
    expect(callsOf("usage_pricing_override_set")).toHaveLength(0);

    fillForm({ input: "abc" });
    fireEvent.click(screen.getByTestId("price-override-save"));
    expect(screen.getByTestId("price-override-form-error")).toHaveTextContent(
      "价格必须是最多 9 位小数的非负数"
    );
    expect(callsOf("usage_pricing_override_set")).toHaveLength(0);

    fillForm({ input: "1.1234567890" });
    fireEvent.click(screen.getByTestId("price-override-save"));
    expect(screen.getByTestId("price-override-form-error")).toHaveTextContent(
      "价格必须是最多 9 位小数的非负数"
    );
    expect(callsOf("usage_pricing_override_set")).toHaveLength(0);

    // 草稿保留；0 是合法价格（与“缺价”严格区分）
    expect(screen.getByTestId("price-override-model")).toHaveValue("my-model");
    fillForm({ input: "0" });
    fireEvent.click(screen.getByTestId("price-override-save"));
    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    expect((callsOf("usage_pricing_override_set")[0][1]?.request as { input?: string }).input).toBe(
      "0"
    );
  });

  it("6. 保存失败显示错误并保留表单与草稿；取消关闭表单", async () => {
    const restore = override({
      usage_pricing_override_set: () =>
        Promise.reject({ code: "usage-settings-invalid", detail: "override boom" }),
    });

    render(wrap(<PriceOverrideManager t={tf} />));
    await screen.findByTestId("price-override-row-gpt-5");
    openAddForm();
    fillForm({ model: "my-model", input: "1" });
    fireEvent.click(screen.getByTestId("price-override-save"));

    expect(await screen.findByTestId("price-override-error")).toHaveTextContent("override boom");
    expect(screen.getByTestId("price-override-form")).toBeTruthy();
    expect(screen.getByTestId("price-override-model")).toHaveValue("my-model");
    expect(screen.getByTestId("price-override-input")).toHaveValue("1");

    fireEvent.click(screen.getByTestId("price-override-cancel"));
    expect(screen.queryByTestId("price-override-form")).toBeNull();

    restore();
  });

  it("7. 状态面板不再挂覆盖价管理器（2026-10-20 第三轮移入配置卡高级区）；只显示迁移提示且不触发启用/刷新", async () => {
    const restore = override({
      usage_pricing_get: () => Promise.resolve(FRESH),
    });

    render(wrap(<PricingStatusPanel t={tf} />));
    await screen.findByTestId("usage-pricing-toggle");

    // Manager 不再在状态面板里（避免同一页两个覆盖价管理器）
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    // 迁移提示在场，指引到「显示与计价配置 → 高级覆盖」
    expect(screen.getByText(tf("usage.pricing.overridesMovedHint"))).toBeTruthy();
    // 状态面板自身行为不变：不自动启用/刷新
    expect(callsOf("usage_pricing_set_enabled")).toHaveLength(0);
    expect(callsOf("usage_pricing_refresh")).toHaveLength(0);

    restore();
  });

  it("8. 定价状态加载中/失败时状态面板自洁：无 Manager、提示与错误互不遮挡", async () => {
    // Manager 已移入配置卡高级区；状态面板的三态（加载中/失败）只影响状态区自身。
    const restorePending = override({
      usage_pricing_get: () => new Promise(() => {}),
    });
    const pending = render(wrap(<PricingStatusPanel t={tf} />));
    expect(await screen.findByText(tf("common.loading"))).toBeTruthy();
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    expect(screen.getByText(tf("usage.pricing.overridesMovedHint"))).toBeTruthy();
    pending.unmount();
    restorePending();

    const restoreError = override({
      usage_pricing_get: () =>
        Promise.reject({ code: "usage-db-failed", detail: "pricing status boom" }),
    });
    render(wrap(<PricingStatusPanel t={tf} />));
    expect(await screen.findByText(/pricing status boom/)).toBeTruthy();
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    expect(screen.getByText(tf("usage.pricing.overridesMovedHint"))).toBeTruthy();

    restoreError();
  });

  it("9. 条目快捷入口 prefill（新建）：模型输入预填**原始模型名**且可编辑、四价为空", async () => {
    render(
      wrap(
        <PriceOverrideManager
          t={tf}
          prefill={{ modelId: "glm-5.3", normalizedModelId: "glm-5-3", nonce: 1 }}
        />
      )
    );

    // 不需要点「新增」：prefill 到达即打开表单；模型输入 = 原始模型名（不是归一化键）
    const model = await screen.findByTestId("price-override-model");
    expect(model).toHaveValue("glm-5.3");
    expect(model).not.toBeDisabled();
    expect(screen.getByTestId("price-override-input")).toHaveValue("");
    expect(screen.getByTestId("price-override-output")).toHaveValue("");

    // 保存走归一化：normalizedModelId 由后端归一化（glm-5-3）
    fillForm({ input: "8" });
    fireEvent.click(screen.getByTestId("price-override-save"));
    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_override_set")[0][1]).toEqual({
      request: {
        normalizedModelId: "glm-5.3",
        currency: "USD",
        input: "8",
        cacheRead: null,
        cacheWrite: null,
        output: null,
      },
    });
    expect(await screen.findByTestId("price-override-row-glm-5-3")).toBeTruthy();
  });

  it("10. 条目快捷入口 prefill（已有覆盖）：进入编辑态——归一化键只读 + 四价回填", async () => {
    render(
      wrap(
        <PriceOverrideManager
          t={tf}
          prefill={{ modelId: "gpt-5", normalizedModelId: "gpt-5", nonce: 1 }}
        />
      )
    );

    // 已有同键覆盖 → beginEdit：模型输入显示归一化键且 disabled，四价回填已存值
    const model = await screen.findByTestId("price-override-model");
    expect(model).toHaveValue("gpt-5");
    expect(model).toBeDisabled();
    expect(screen.getByTestId("price-override-input")).toHaveValue("1.25");
    expect(screen.getByTestId("price-override-cache-read")).toHaveValue("0.125");
    expect(screen.getByTestId("price-override-output")).toHaveValue("10");
  });

  it("11. 覆盖表说明与作用域文案：Manager 头部/表单带 help Info 与「作用范围：所有渠道」提示", async () => {
    render(wrap(<PriceOverrideManager t={tf} />));

    expect(await screen.findByTestId("price-override-manager")).toHaveTextContent(
      tf("usage.pricing.overrides.title")
    );
    // 头部 Info 图标（Radix Tooltip 触发器）与全局作用域说明在场
    expect(screen.getByTestId("price-override-info")).toBeTruthy();
    expect(screen.getByText(tf("usage.pricing.overrides.scopeAllChannels"))).toBeTruthy();

    // 表单 Info（模型键说明）随表单挂载
    openAddForm();
    expect(await screen.findByTestId("price-override-model-info")).toBeTruthy();

    // 可访问名（P2-3）：label 用 htmlFor 关联输入框，可访问名 = 「模型名」；
    // InfoTip 是 label 的兄弟节点，其说明文字不得混进可访问名（旧结构把按钮塞进 label 会串味）
    const modelInput = screen.getByLabelText(tf("usage.pricing.overrides.model"));
    expect(modelInput).toHaveAttribute("id", "price-override-model");
    expect(modelInput).toHaveAttribute("data-testid", "price-override-model");
  });
});
