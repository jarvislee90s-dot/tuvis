// Task 12 · C 用量分布：Top5 独立纯层 + 四维按钮 + 命中率 + 无美元列 + label 分派。
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { UsageDistributionBody } from "@/components/usage/UsageDistributionCard";
import { DIST_TOP_ROWS, distributionV2Rows, distributionV2View } from "@/lib/usage/distributionV2";
import type { DistributionGroup, DistributionRow } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function distRow(
  key: string,
  label: string,
  requestTotal: number,
  output: number,
  opts: Partial<DistributionRow> = {}
): DistributionRow {
  return {
    key,
    label,
    sourceIds: ["codex"],
    billingProfileId: null,
    attributionKind: "unknown",
    buckets: { inputFresh: requestTotal, cacheRead: 0, cacheWrite: 0, output },
    metrics: { requestTotal, cacheHitRate: 0.5, userEst: null, requests: 1 },
    channelKey: null,
    channelKind: null,
    displayProvider: null,
    observedProvider: null,
    observedProviders: [],
    hasEmptyProvider: false,
    modelId: null,
    ...opts,
  };
}

/** 7 行（故意乱序），用于默认 5 行 / 展开 7 行；结构化渠道字段与 label 同源 */
const ROWS: DistributionRow[] = [
  distRow("r3", "Profile C", 300, 0),
  distRow("r7", "Profile G", 700, 0),
  distRow("r1", "Profile A", 100, 0),
  distRow("r5", "Profile E", 500, 0),
  distRow("r2", "Profile B", 200, 0),
  distRow("r6", "Profile F", 600, 0),
  distRow("r4", "Profile D", 400, 0),
].map((r) => ({
  ...r,
  billingProfileId: `profile-${r.key.slice(1)}`,
  attributionKind: "userAssigned" as const,
  channelKey: `bp:profile-${r.key.slice(1)}`,
  channelKind: "billingProfile" as const,
  displayProvider: r.label,
}));

const BODY_PROPS = {
  groupBy: "billingProfile" as DistributionGroup,
  onGroupByChange: vi.fn(),
  onExpandedChange: vi.fn(),
  t: tf,
};

describe("distributionV2（纯函数）", () => {
  it("1. 过滤正行、按 hero 降序、share 与旧口径一致、Top5 展开独立", () => {
    const zero = distRow("zero", "Zero", 0, 0);
    const sorted = distributionV2Rows([...ROWS, zero]);
    expect(sorted).toHaveLength(7); // 零值行被丢
    expect(sorted.map((r) => r.key)).toEqual(["r7", "r6", "r5", "r4", "r3", "r2", "r1"]);
    expect(sorted[0].value).toBe(700);
    expect(sorted[0].share).toBe(1);
    expect(sorted[1].share).toBeCloseTo(600 / 700, 6);
    for (const row of sorted) expect(row.share).toBeGreaterThanOrEqual(0.04);

    expect(DIST_TOP_ROWS).toBe(5);
    expect(distributionV2View(sorted, false)).toEqual({
      rows: sorted.slice(0, 5),
      moreCount: 2,
    });
    expect(distributionV2View(sorted, true)).toEqual({ rows: sorted, moreCount: 0 });
  });
});

describe("UsageDistributionBody（C 卡体）", () => {
  it("2. 默认 5 行、显示更多后 7 行、收起；四维按钮与命中率、无美元列", () => {
    const onGroupByChange = vi.fn();
    const onExpandedChange = vi.fn();
    const view = render(
      <UsageDistributionBody
        {...BODY_PROPS}
        rows={ROWS}
        expanded={false}
        onGroupByChange={onGroupByChange}
        onExpandedChange={onExpandedChange}
      />
    );

    // 四维按钮，默认 billingProfile
    for (const dim of ["billingProfile", "tool", "project", "model"]) {
      expect(screen.getByTestId(`usage-dist-group-${dim}`)).toHaveAttribute(
        "aria-pressed",
        String(dim === "billingProfile")
      );
    }
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    expect(onGroupByChange).toHaveBeenCalledWith("model");

    // 默认 Top5（降序），more 按钮
    expect(screen.getAllByTestId(/^usage-dist-r/)).toHaveLength(5);
    expect(screen.getByTestId("usage-dist-expand")).toHaveTextContent("2");
    fireEvent.click(screen.getByTestId("usage-dist-expand"));
    expect(onExpandedChange).toHaveBeenCalledWith(true);

    // 命中率显示：每行都有
    expect(screen.getByTestId("usage-hit-r7")).toHaveTextContent("50.0%");

    // 无美元列（本卡仅 Token/命中率/相对条）
    expect(view.container.textContent).not.toContain("$");
    expect(view.container.textContent).not.toMatch(/USD/i);

    view.unmount();

    // 展开态：7 行 + 收起按钮
    render(
      <UsageDistributionBody
        {...BODY_PROPS}
        rows={ROWS}
        expanded
        onExpandedChange={onExpandedChange}
      />
    );
    expect(screen.getAllByTestId(/^usage-dist-r/)).toHaveLength(7);
    expect(screen.getByTestId("usage-dist-expand")).toHaveTextContent("收起");
    expect(screen.queryByTestId("usage-dist-r9")).toBeNull();
  });

  it("3. 渠道维度：显示名 / i18n 键；未指定渠道不追加工具后缀；原始 provider 进 title", () => {
    const rows: DistributionRow[] = [
      distRow("bp", "Team API", 500, 0, {
        billingProfileId: "profile-a",
        attributionKind: "userAssigned",
        channelKey: "bp:profile-a",
        channelKind: "billingProfile",
        displayProvider: "Team API",
      }),
      distRow("alias", "OpenRouter", 400, 0, {
        channelKey: "provider:OpenRouter",
        channelKind: "alias",
        displayProvider: "OpenRouter",
        observedProvider: "openrouter",
      }),
      distRow("u", "usage.label.unassignedChannel", 300, 0, {
        sourceIds: ["codex", "claude"],
        channelKey: "unassigned",
        channelKind: "unassigned",
      }),
      distRow("amb", "usage.label.ambiguousChannel", 200, 0, {
        sourceIds: ["zcode"],
        channelKey: "ambiguous",
        channelKind: "ambiguous",
      }),
    ];

    const view = render(<UsageDistributionBody {...BODY_PROPS} rows={rows} expanded={false} />);
    expect(screen.getByTestId("usage-dist-bp")).toHaveTextContent("Team API");
    expect(screen.getByTestId("usage-dist-alias")).toHaveTextContent("OpenRouter");
    // 原始 provider 保留在 title（展示名与原始串可不同）
    expect(screen.getByTestId("usage-dist-alias")).toHaveAttribute(
      "title",
      expect.stringContaining("openrouter")
    );
    // 未指定渠道：渠道维度全局一行，**不再追加工具后缀**
    expect(screen.getByTestId("usage-dist-u").textContent).toBe("未指定渠道");
    expect(screen.getByTestId("usage-dist-u").textContent).not.toContain("Codex");
    expect(screen.getByTestId("usage-dist-u").textContent).not.toContain(
      "usage.label.unassignedChannel"
    );
    expect(screen.getByTestId("usage-dist-amb").textContent).toBe("渠道可能混合");
    expect(screen.getByTestId("usage-dist-amb").textContent).not.toContain(
      "usage.label.ambiguousChannel"
    );
    view.unmount();
  });

  it("3b. 模型维度：显示名 / 完整模型；未指定按工具拆行；空模型显示未知模型；跨工具合并一行", () => {
    const rows: DistributionRow[] = [
      distRow("m-bp", "gpt-5", 500, 0, {
        billingProfileId: "profile-a",
        attributionKind: "userAssigned",
        channelKey: "bp:profile-a",
        channelKind: "billingProfile",
        displayProvider: "Team API",
        observedProvider: "openai",
        modelId: "gpt-5",
      }),
      distRow("m-unassigned", "", 300, 0, {
        sourceIds: ["codex"],
        channelKey: "unassigned",
        channelKind: "unassigned",
        modelId: "",
      }),
      distRow("m-amb", "gpt-x", 200, 0, {
        sourceIds: ["zcode"],
        channelKey: "ambiguous",
        channelKind: "ambiguous",
        observedProvider: "mix-provider",
        modelId: "gpt-x",
      }),
      distRow("m-amb-tool", "gpt-y", 150, 0, {
        sourceIds: ["zcode"],
        channelKey: "ambiguous",
        channelKind: "ambiguous",
        observedProvider: null,
        modelId: "gpt-y",
      }),
      // 同一显示名跨工具合并：后端只出一行，sourceIds 保留两个工具
      distRow("m-merged", "claude-4", 100, 0, {
        sourceIds: ["claude", "codex"],
        channelKey: "provider:Acme",
        channelKind: "alias",
        displayProvider: "Acme",
        observedProvider: "acme",
        modelId: "claude-4",
      }),
    ];

    const view = render(
      <UsageDistributionBody {...BODY_PROPS} rows={rows} groupBy="model" expanded={false} />
    );
    expect(screen.getByTestId("usage-dist-m-bp")).toHaveTextContent("Team API / gpt-5");
    // title 保留原始 provider 与完整模型
    expect(screen.getByTestId("usage-dist-m-bp")).toHaveAttribute("title", "openai / gpt-5");
    // 未指定渠道 + 空模型：工具上下文 + 未知模型，不显示空串
    expect(screen.getByTestId("usage-dist-m-unassigned")).toHaveTextContent(
      "未指定渠道 · Codex / 未知模型"
    );
    expect(screen.getByTestId("usage-dist-m-unassigned").textContent).not.toContain(
      "usage.label.unknownModel"
    );
    // 可能混合：优先原始 provider，缺失时退工具
    expect(screen.getByTestId("usage-dist-m-amb")).toHaveTextContent(
      "渠道可能混合 · mix-provider / gpt-x"
    );
    expect(screen.getByTestId("usage-dist-m-amb-tool")).toHaveTextContent(
      "渠道可能混合 · ZCode / gpt-y"
    );
    // 同显示名跨工具合并后只渲染一行
    expect(screen.getByTestId("usage-dist-m-merged")).toHaveTextContent("Acme / claude-4");
    expect(screen.getByTestId("usage-dist-m-merged")).toHaveAttribute("title", "acme / claude-4");
    expect(screen.getAllByTestId(/^usage-dist-m-/)).toHaveLength(5);
    view.unmount();
  });

  it("3c. hasEmptyProvider：有非空证据时 tooltip 追加含未标注来源；无证据的未指派行不重复标注", () => {
    const rows: DistributionRow[] = [
      distRow("mixed", "OpenAI", 500, 0, {
        channelKey: "provider:OpenAI",
        channelKind: "alias",
        displayProvider: "OpenAI",
        observedProvider: "openai",
        observedProviders: ["openai"],
        hasEmptyProvider: true,
      }),
      distRow("mixed-model", "gpt-5", 400, 0, {
        channelKey: "provider:OpenAI",
        channelKind: "alias",
        displayProvider: "OpenAI",
        observedProvider: "openai",
        observedProviders: ["openai"],
        hasEmptyProvider: true,
        modelId: "gpt-5",
      }),
      distRow("clean", "Clean", 300, 0, {
        channelKey: "provider:Clean",
        channelKind: "provider",
        displayProvider: "Clean",
        observedProvider: "clean",
        observedProviders: ["clean"],
        hasEmptyProvider: false,
      }),
      distRow("unlabeled", "usage.label.unassignedChannel", 200, 0, {
        channelKey: "unassigned",
        channelKind: "unassigned",
        observedProviders: [],
        hasEmptyProvider: true,
      }),
    ];

    const view = render(<UsageDistributionBody {...BODY_PROPS} rows={rows} expanded={false} />);
    expect(screen.getByTestId("usage-dist-mixed")).toHaveAttribute(
      "title",
      "OpenAI · openai · 含未标注来源"
    );
    expect(screen.getByTestId("usage-dist-clean")).toHaveAttribute("title", "Clean · clean");
    expect(screen.getByTestId("usage-dist-unlabeled")).not.toHaveAttribute("title");
    view.unmount();

    render(<UsageDistributionBody {...BODY_PROPS} rows={rows} groupBy="model" expanded={false} />);
    expect(screen.getByTestId("usage-dist-mixed-model")).toHaveAttribute(
      "title",
      "openai / gpt-5 · 含未标注来源"
    );
    expect(screen.getByTestId("usage-dist-clean")).toHaveAttribute("title", "clean / Clean");
  });

  it("4. 空态：内容区出暂无数据，不出任何行（卡头由 UsageFold 保留）", () => {
    const view = render(<UsageDistributionBody {...BODY_PROPS} rows={[]} expanded={false} />);
    expect(screen.getByTestId("usage-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryByTestId(/^usage-dist-r/)).toBeNull();
    expect(view.container.textContent).not.toMatch(/\$/);
  });
});
