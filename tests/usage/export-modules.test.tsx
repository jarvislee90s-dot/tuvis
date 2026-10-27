// 出图模块选择（2026-10-27 实机反馈轮 #3a）三件套：
//  A. `src/lib/usage/exportModules.ts`：默认值 / 坏值回落 / 往返（localStorage key `tuvis.usage.exportModules`）；
//  B. `layoutExport` 动态段序：**改版前锁定**（默认模块 → 与改造前逐字相同的段序/步骤数/总高，
//     基线值取自改版前的实现实跑：height=1560 / contentBottom=1466 / steps=54）+ 多分组段 /
//     费用段（含「等 N」披露：只画 Top6 而总额是全部行合计，X2 同类缺陷必须说出来）/
//     无工作小结段 / 空分组段；
//  C. 「导出设置」弹层的出图模块块：总览勾死禁用、四维度子项、费用/工作小结开关即点即存。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { UsageExportSettings } from "@/components/usage/UsageExportSettings";
import {
  DEFAULT_EXPORT_MODULES,
  loadExportModules,
  saveExportModules,
} from "@/lib/usage/exportModules";
import {
  BUBBLE_BG,
  layoutExport,
  type ExportCostSection,
  type ExportGroupSection,
  type ExportInput,
  type ExportStep,
  type MeasureText,
} from "@/lib/usage/exportLayout";
// 评审 Important：`buildCostSection` 导出仅用于「>6 行 ⇒ more 非 null」回归；
// 生产纪律不变——`useExportShare` 这个 hook 仍只允许 `UsageExportActions` import。
import { buildCostSection } from "@/lib/usage/useExportShare";
import type { CostRow, UsageCosts } from "@/types/usageV2";

// ── 共用夹具 ─────────────────────────────────────────────────────────────────

const STORAGE_KEY = "tuvis.usage.exportModules";

/** 假测量：与既有布局用例同款（1 字符 = 10px） */
const measure: MeasureText = (text) => text.length * 10;

function groupSection(over: Partial<ExportGroupSection> = {}): ExportGroupSection {
  return {
    groupBy: "tool",
    title: "分组分布",
    rows: [
      { name: "Claude", value: "1,246,567", share: 1 },
      { name: "Codex", value: "460,789", share: 0.37 },
    ],
    groupsMore: null,
    ...over,
  };
}

function baseInput(over: Partial<ExportInput> = {}): ExportInput {
  return {
    brand: "兔维斯 · 用量看板",
    rangeLabel: "近 7 天 · 09/27 – 10/03",
    heroLabel: "近 7 天 Token 合计",
    hero: "2,036,981",
    heroSub: "数据截止时间: 2026-10-03 12:00:00",
    metrics: [
      ["请求输入", "1,954,268"],
      ["缓存命中", "1,200,000"],
      ["未缓存命中输入", "754,268"],
      ["产出", "82,713"],
      ["命中率", "91.2%"],
      ["请求次数", "320"],
    ],
    trendTitle: "用量趋势",
    emptyLabel: "暂无数据",
    points: [
      { label: "09/27", value: 100 },
      { label: "09/28", value: 250 },
      { label: "10/03", value: 400 },
    ],
    groups: [groupSection()],
    cost: null,
    work: true,
    toolsTitle: "工具调用",
    tools2x2: [
      ["调用总数", "1,234"],
      ["平均耗时", "2.4 秒"],
      ["次数第一", "Bash 120"],
      ["耗时第一", "Edit 31.2 秒"],
    ],
    quote: "缓存吃得干干净净（命中率 91.2%），省钱小能手！",
    footer: "纯 token · 含子代理 · 本地聚合",
    pose: "waving",
    poseColIndex: 0,
    sheet: { rows: 11, geometry: { frameW: 192, frameH: 208, matchesConstants: true } },
    ...over,
  };
}

const textsOf = (input: ExportInput): string[] =>
  layoutExport(input, measure).steps.flatMap((s) => (s.kind === "text" ? [s.text] : []));

// ── A. 偏好纯层 ──────────────────────────────────────────────────────────────

describe("exportModules（#3a：出图模块偏好，localStorage 记忆）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("1. 缺值 → 默认（与今日出图完全一致：一段按工具 + 工作小结 + 无费用）", () => {
    expect(loadExportModules()).toEqual(DEFAULT_EXPORT_MODULES);
    expect(DEFAULT_EXPORT_MODULES).toEqual({ distribution: ["tool"], cost: false, work: true });
  });

  it("2. 坏值回落：坏 JSON / 非对象 / 字段类型不对 → 整体或单字段回落默认", () => {
    localStorage.setItem(STORAGE_KEY, "{not json");
    expect(loadExportModules()).toEqual(DEFAULT_EXPORT_MODULES);
    localStorage.setItem(STORAGE_KEY, JSON.stringify("string"));
    expect(loadExportModules()).toEqual(DEFAULT_EXPORT_MODULES);
    // distribution 不是数组 → 回落默认；cost/work 类型不对 → 各自回落
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ distribution: "tool", cost: "yes" }));
    expect(loadExportModules()).toEqual(DEFAULT_EXPORT_MODULES);
  });

  it("3. distribution 只接受四个合法 groupBy：非法值丢弃、去重、保序；空数组是合法选择", () => {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ distribution: ["model", "bogus", "tool", "model"], cost: true, work: false })
    );
    expect(loadExportModules()).toEqual({
      distribution: ["model", "tool"],
      cost: true,
      work: false,
    });
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ distribution: [], cost: false, work: true })
    );
    expect(loadExportModules().distribution).toEqual([]);
  });

  it("4. 往返：save → load 原样回来；默认值拷贝不被共享引用改脏", () => {
    saveExportModules({ distribution: ["project", "provider"], cost: true, work: false });
    expect(loadExportModules()).toEqual({
      distribution: ["project", "provider"],
      cost: true,
      work: false,
    });
    const d = loadExportModules();
    d.distribution.push("model");
    expect(loadExportModules().distribution).toEqual(["project", "provider"]);
  });
});

// ── B. 布局动态段 ────────────────────────────────────────────────────────────

describe("layoutExport 动态段序（#3a）", () => {
  it("1. 改版前锁定：默认形状（单段 tool + 无费用 + work）→ 段序/步骤数/总高与改造前逐字一致", () => {
    // 分组行与改造前基线夹具逐字同形（3 行，含 0 值「空档」行——它验证 0 值也照画）
    const l = layoutExport(
      baseInput({
        groups: [
          groupSection({
            rows: [
              { name: "Claude", value: "1,246,567", share: 1 },
              { name: "Codex", value: "460,789", share: 0.37 },
              { name: "空档", value: "0", share: 0 },
            ],
          }),
        ],
      }),
      measure
    );
    // 段序 = 改造前的 9 锚点（groups 只出现一次、cost 缺席、tools/quote 在场）
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "tools",
      "quote",
      "footer",
    ]);
    // 基线值取自改造前实现实跑（见文件头），任何一个变了都是版式破坏，先回改 brief 再动这里
    expect(l.steps).toHaveLength(54);
    expect(l.contentBottom).toBe(1466);
    expect(l.height).toBe(1560);
  });

  it("2. distribution 两个维度 → 两段分组、标题按选择顺序、锚点 y 严格递增", () => {
    const l = layoutExport(
      baseInput({
        groups: [
          groupSection({ groupBy: "tool", title: "分组分布 · 按工具" }),
          groupSection({ groupBy: "model", title: "分组分布 · 按模型" }),
        ],
      }),
      measure
    );
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "groups",
      "tools",
      "quote",
      "footer",
    ]);
    expect(l.anchors.filter((a) => a.id === "groups").map((a) => a.key)).toEqual(["tool", "model"]);
    const texts = textsOf(
      baseInput({
        groups: [
          groupSection({ title: "分组分布 · 按工具" }),
          groupSection({ title: "分组分布 · 按模型" }),
        ],
      })
    );
    expect(texts.indexOf("分组分布 · 按工具")).toBeLessThan(texts.indexOf("分组分布 · 按模型"));
    const ys = l.anchors.map((a) => a.y);
    for (let i = 1; i < ys.length; i++) expect(ys[i]).toBeGreaterThan(ys[i - 1]);
  });

  it("3. 费用段：插在分组段之后、工具格之前；标题/总额/行都画，带 share 的行画描边样式条", () => {
    const costOf = (more: string | null): ExportCostSection => ({
      title: "API 等效费用",
      totalLabel: "合计（估价）",
      total: "$12.34",
      rows: [
        { label: "Anthropic / claude-x", value: "$10.00", share: 1 },
        { label: "OpenAI / gpt-y", value: "$2.34" },
      ],
      more,
    });
    const l = layoutExport(baseInput({ cost: costOf(null) }), measure);
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "cost",
      "tools",
      "quote",
      "footer",
    ]);
    const all = textsOf(baseInput({ cost: costOf(null) }));
    expect(all).toContain("API 等效费用");
    expect(all).toContain("合计（估价）");
    expect(all).toContain("$12.34");
    const bars = l.steps.filter((s) => s.kind === "bar") as Extract<
      (typeof l.steps)[number],
      { kind: "bar" }
    >[];
    // 分组段 2 条（tool 维度实心 #3b82f6）+ 费用段 1 条（描边样式：淡紫填充 + 紫描边）
    expect(bars).toHaveLength(3);
    expect(bars[0]!.fill).toBe("#3b82f6");
    const costBar = bars[2]!;
    expect(costBar.fill).toBe("rgba(139,92,246,0.18)");
    expect(costBar.stroke).toBe("#8b5cf6");

    // 「等 N」披露（评审 Important，X2 同类）：more 非空 ⇒ 逐字画出、内容底 +22、
    // 在费用段内 / 工具格之前（总额是全部行的合计，可见行少于总行数必须说出来）
    expect(textsOf(baseInput({ cost: costOf(null) }))).not.toContain("等 2");
    const disclosed = textsOf(baseInput({ cost: costOf("等 2") }));
    expect(disclosed).toContain("等 2");
    expect(disclosed.indexOf("等 2")).toBeGreaterThan(disclosed.indexOf("$12.34"));
    expect(disclosed.indexOf("等 2")).toBeLessThan(disclosed.indexOf("工具调用"));
    const plain = layoutExport(baseInput({ cost: costOf(null) }), measure);
    const withMore = layoutExport(baseInput({ cost: costOf("等 2") }), measure);
    expect(withMore.contentBottom).toBe(plain.contentBottom + 22);
    expect(withMore.height).toBe(Math.max(1560, Math.ceil(withMore.contentBottom + 34)));
  });

  it("3b. 分组条色随维度（GROUP_ACCENTS 四色各一、同段同色），费用条描边样式与实心条区分", () => {
    const l = layoutExport(
      baseInput({
        groups: [
          groupSection({ groupBy: "tool" }),
          groupSection({ groupBy: "project" }),
          groupSection({ groupBy: "provider" }),
          groupSection({ groupBy: "model" }),
        ],
        cost: {
          title: "API 等效费用",
          totalLabel: "合计（估价）",
          total: "$12.34",
          rows: [{ label: "Anthropic / claude-x", value: "$10.00", share: 1 }],
          more: null,
        },
      }),
      measure
    );
    const bars = l.steps.filter((s): s is Extract<ExportStep, { kind: "bar" }> => s.kind === "bar");
    // 4 段 × 2 行 = 8 条分组条 + 1 条费用条
    expect(bars).toHaveLength(9);
    // 每个维度一个强调色（tool→project→provider→model 段序）
    expect(bars[0]!.fill).toBe("#3b82f6");
    expect(bars[2]!.fill).toBe("#10b981");
    expect(bars[4]!.fill).toBe("#f59e0b");
    expect(bars[6]!.fill).toBe("#8b5cf6");
    // 同段内同样式实心条（同族同色）
    expect(bars[1]!.fill).toBe("#3b82f6");
    expect(bars[3]!.fill).toBe("#10b981");
    // 费用条：淡紫填充 + 紫描边（描边样式，非实心；不随维度变化）
    expect(bars[8]!.fill).toBe("rgba(139,92,246,0.18)");
    expect(bars[8]!.stroke).toBe("#8b5cf6");
  });

  it("4. work=false：工具格与立绘/气泡整段缺席（无 sprite、无气泡底），内容底/总高收缩", () => {
    const longQuote = "这是一条很长的评语。".repeat(40);
    const full = layoutExport(baseInput({ quote: longQuote }), measure);
    const off = layoutExport(baseInput({ quote: longQuote, work: false }), measure);
    expect(off.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "footer",
    ]);
    expect(off.steps.some((s) => s.kind === "sprite")).toBe(false);
    expect(off.steps.some((s) => s.kind === "rect" && s.color === BUBBLE_BG)).toBe(false);
    // 图在最后一个数据段后正常收尾：页脚公式不变、总高确实缩了
    expect(off.height).toBe(Math.max(1560, Math.ceil(off.contentBottom + 34)));
    expect(off.contentBottom).toBeLessThan(full.contentBottom);
    expect(off.height).toBeLessThan(full.height);
    // 短内容同样成立（默认图 1560 下限内：内容底缩、总高贴公式）
    const offShort = layoutExport(baseInput({ work: false }), measure);
    expect(offShort.contentBottom).toBeLessThan(layoutExport(baseInput(), measure).contentBottom);
    expect(offShort.height).toBe(Math.max(1560, Math.ceil(offShort.contentBottom + 34)));
  });

  it("5. distribution=[]：无分组段（无标题、无锚点、无 bar），其余段照常", () => {
    const l = layoutExport(baseInput({ groups: [] }), measure);
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "tools",
      "quote",
      "footer",
    ]);
    expect(textsOf(baseInput({ groups: [] }))).not.toContain("分组分布");
    expect(l.steps.some((s) => s.kind === "bar")).toBe(false);
  });
});

// ── C. 设置弹层 ──────────────────────────────────────────────────────────────

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

const SETTINGS = {
  enabled: false,
  minibarRange: "today",
  minibarToolRows: 3,
  detailRetentionDays: 90,
  collectIntervalMin: 10,
  providerMapRules: "",
  exportQuote: "",
  exportPose: "random",
};

describe("UsageExportSettings 出图模块块（#3a）", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    localStorage.clear();
    invokeMock.mockReset();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "usage_get_settings") return SETTINGS;
      if (cmd === "usage_set_settings") return SETTINGS;
      return null;
    });
  });

  const openDialog = async () => {
    render(<UsageExportSettings />);
    fireEvent.click(screen.getByTestId("usage-export-settings-open"));
    await screen.findByTestId("usage-export-settings-dialog");
  };

  it("1. 总览勾死禁用；四个分组子项 / 费用 / 工作小结都在场且默认值正确", async () => {
    await openDialog();
    const overview = screen.getByTestId("usage-export-module-overview");
    expect(overview).toBeDisabled();
    expect(overview).toHaveAttribute("data-state", "checked");
    for (const gb of ["tool", "project", "provider", "model"]) {
      const box = screen.getByTestId(`usage-export-module-dist-${gb}`);
      expect(box).toHaveAttribute("data-state", gb === "tool" ? "checked" : "unchecked");
    }
    expect(screen.getByTestId("usage-export-module-cost")).toHaveAttribute(
      "data-state",
      "unchecked"
    );
    expect(screen.getByTestId("usage-export-module-work")).toHaveAttribute("data-state", "checked");
    // 评语/姿态通路未动：弹层里仍有评语输入与姿态选择
    expect(screen.getByTestId("usage-export-quote-input")).toBeTruthy();
    expect(screen.getByTestId("usage-export-pose")).toBeTruthy();
  });

  it("2. 勾选「API 等效费用」→ **立即**写 localStorage（无 Rust 往返）+ 行内出已保存；重开弹层回读一致", async () => {
    await openDialog();
    fireEvent.click(screen.getByTestId("usage-export-module-cost"));
    await waitFor(() =>
      expect(JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "")).toMatchObject({ cost: true })
    );
    expect(loadExportModules().cost).toBe(true);
    // 无 Rust 往返：模块勾选**不**发 usage_set_settings
    expect(invokeMock.mock.calls.filter((c) => c[0] === "usage_set_settings")).toHaveLength(0);
    // 行内「已保存」提示（复用既有 note 位）
    await waitFor(() => expect(screen.getByTestId("usage-export-settings-note")).toBeTruthy());

    // 取消勾选同样即存
    fireEvent.click(screen.getByTestId("usage-export-module-cost"));
    await waitFor(() => expect(loadExportModules().cost).toBe(false));

    // 重开弹层：勾选态从 localStorage 回读
    fireEvent.click(screen.getByTestId("usage-export-settings-cancel"));
    fireEvent.click(screen.getByTestId("usage-export-settings-open"));
    await screen.findByTestId("usage-export-settings-dialog");
    fireEvent.click(screen.getByTestId("usage-export-module-cost"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-module-cost")).toHaveAttribute(
        "data-state",
        "checked"
      )
    );
  });

  it("3. 分组子项切换：勾上 按模型 → distribution 含 model；取消 按工具 → 不含 tool", async () => {
    await openDialog();
    fireEvent.click(screen.getByTestId("usage-export-module-dist-model"));
    await waitFor(() => expect(loadExportModules().distribution).toEqual(["tool", "model"]));
    fireEvent.click(screen.getByTestId("usage-export-module-dist-tool"));
    await waitFor(() => expect(loadExportModules().distribution).toEqual(["model"]));
  });
});

// ── D. 费用段装配的「等 N」披露（评审 Important，X2 同类缺陷）──────────────────

/** 最小费用行夹具：只填 `buildCostSection` 实际读取的字段（其余与装配无关） */
function costRowFixture(i: number): CostRow {
  return {
    key: `k${i}`,
    tokenUsage: 1000 - i,
    estimatedUsd: "0.01",
    estimatedCny: "0.07",
    channelKind: "provider",
    displayProvider: `Channel ${i}`,
    displayModelName: null,
    identity: { modelId: `model-${i}` },
  } as unknown as CostRow;
}

const costsOf = (n: number): UsageCosts =>
  ({ rows: Array.from({ length: n }, (_, i) => costRowFixture(i)) }) as unknown as UsageCosts;

describe("buildCostSection 费用段披露（评审 Important）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("1. >6 行 ⇒ more 非 null（超出行数，i18n 同 usage.moreN）；≤6 ⇒ null", () => {
    const t = i18n.t.bind(i18n);
    const eight = buildCostSection(costsOf(8), t);
    expect(eight.rows).toHaveLength(6); // 只画 6 行
    expect(eight.more).toBe(t("usage.moreN", { n: 2 })); // 但必须说出还有 2 行
    expect(eight.more).toContain("2");
    expect(buildCostSection(costsOf(6), t).more).toBeNull();
    expect(buildCostSection(costsOf(7), t).more).toBe(t("usage.moreN", { n: 1 }));
  });

  it("2. total 恒覆盖**全部**行（披露的存在理由）：8 行 × $0.01 = $0.08，不随 Top6 截断", () => {
    const eight = buildCostSection(costsOf(8), i18n.t.bind(i18n));
    expect(eight.total).toBe("$0.08");
  });
});
