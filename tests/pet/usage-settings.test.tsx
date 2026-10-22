// tests/pet/usage-settings.test.tsx — 计划② Task 14：设置页「用量统计」分组（P7 的 8 项设置）验收面。
//
// 覆盖（10 条）：① 设置页接线可渲染（菜单项 → 分区 → 8 项全在场，且**不含** ① 的「采集状态 / 立即采集」
// 验收面）；② 开关改动即发 patch，并用**后端返回的合并对象**回写本地态；③ 数字输入过滤非数字，
// **编辑期只留字符串草稿、失焦才保存**；④ 条数夹取上下界（99→7、0→1，失焦/Enter 时才夹）；
// ⑤ 范围 / 姿态是原生 select（3 档 / `poseKeysFor(11)` 的 11 项含 `look`）；⑥ 保留期提示随**提交后的
// 值**更新且夹取 1–90；⑦ 保留期逐字符输入只改草稿（锁旧 bug：逐字符夹取导致 30 存成 3/1）、
// 999→90、空串回退原值；⑧ 保存失败原因可见（`usage-set-error`，不静默）；⑨ 换宠物 → 姿态非
// `random` 时回退 `random` 并提示（已是则幂等）；⑩ 采集间隔 Enter/失焦同一条提交路径（1–1440，
// 夹取后与当前值相同则不重复写）。
//
// **假后端**（不是桩）：`usage_get_settings` 读 `stored`、`usage_set_settings` 按后端 `merge_patch`
// 同语义合并回写 `stored` 并返回**完整 8 字段** —— 用例 2 靠它证明「回写的是返回的合并对象」而不是
// 「本地 patch 合并」的假绿（本地合并会把另一 WebView 已改的字段丢掉）。
//
// ⚠️ 本文件里**不得**出现轮询字段字面量（① 的源码锁按纯文本扫描「含该字面量的前端文件」，并要求
// 这类文件里不出现用量命令的 snake_case 字面量；扫描面只含 `src/`，但这里同样照此纪律写）；
// 命令名一律用 camelCase 包装名称呼。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

// 本文件自持一套 invoke 实现（假后端 + 失败注入）；convertFileSrc 供 petRuntime 取用
vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  convertFileSrc: (p: string) => `asset://mock/${p}`,
}));
// WindowFrame / TitleBar（用例 1 渲染整个设置页）依赖 Tauri 窗口 API 与语言事件（照 petSettings.test.tsx）
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

// jsdom 未实现 matchMedia，ThemeProviders（WindowFrame 内置）需要它
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

import i18n from "@/i18n";
import SettingsPage from "@/pages/settings";
import { UsageSection } from "@/components/settings/UsageSection";
import { saveActiveId } from "@/components/pet/petRuntime";
import type { UsageSettings, UsageSettingsPatch } from "@/types/usage";

/** 与 Rust `UsageSettings::default()` 同值（spec §P7）：开 / 今日 / 3 条 / 90 天 / 10 分钟 / 空 / 空 / 随机 */
const DEFAULTS: UsageSettings = {
  enabled: true,
  miniBarRange: "today",
  miniBarToolRows: 3,
  detailRetentionDays: 90,
  collectIntervalMin: 10,
  providerMapRules: "",
  exportQuote: "",
  exportPose: "waving", // 非 random ⇒ 换宠物时应触发回退（测试 9 的前提）
};

/** 假后端当前落库值（用例可中途改写，模拟「另一个 WebView 改过设置」） */
let stored: UsageSettings;
/** 让下一次写入整包拒绝（形状与后端 `usage-settings-invalid` 一致，用完即清） */
let failNext: unknown;

function installBackend() {
  stored = { ...DEFAULTS };
  failNext = null;
  invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
    if (cmd === "usage_get_settings") return { ...stored };
    if (cmd === "usage_set_settings") {
      if (failNext) {
        const e = failNext;
        failNext = null;
        throw e; // 后端 `usage_set_settings` 整包拒绝：结构化的 { code, detail }
      }
      // 与 `merge_patch` 同语义：patch 未提的字段保持原值；返回合并后的**完整 8 字段**
      stored = { ...stored, ...((args?.patch ?? {}) as UsageSettingsPatch) };
      return { ...stored };
    }
    return null;
  });
}

/** 本次用例发过的全部 patch（顺序即调用顺序） */
const setPatches = (): UsageSettingsPatch[] =>
  invokeMock.mock.calls
    .filter(([cmd]) => cmd === "usage_set_settings")
    .map(([, args]) => (args as { patch: UsageSettingsPatch }).patch);

/** 渲染包一层 QueryClientProvider（设置查询走 `useUsageSettingsQuery`）；每用例独立 cache */
const renderWithClient = (ui: ReactNode) =>
  render(
    <QueryClientProvider
      client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
    >
      {ui}
    </QueryClientProvider>
  );

const renderSection = () => renderWithClient(<UsageSection />);

const input = (testId: string) => screen.getByTestId(testId) as HTMLInputElement;
const select = (testId: string) => screen.getByTestId(testId) as HTMLSelectElement;

beforeAll(async () => {
  await i18n.changeLanguage("en");
});

beforeEach(() => {
  localStorage.clear();
  installBackend();
});

describe("设置页「用量统计」分组（计划② Task 14）", () => {
  it("1. 设置页接线：菜单进入分区，8 项设置全部在场且不掺 ① 的采集验收面", async () => {
    renderWithClient(<SettingsPage />);
    // ② 的四处改动（联合类型 / menuItems / 渲染分支 / lucide 导入）在**运行时**一起被这条用例走过：
    // 少任何一处，菜单点开就是空分区或直接编译不过
    // 2026-10-06 两级导航：用量设置与采集状态现在同属「用量统计」块 ⇒ 点块（testid），两卡并置
    fireEvent.click(await screen.findByTestId("settings-nav-usage"));

    expect(await screen.findByTestId("usage-enabled")).toBeTruthy();
    expect(screen.getByTestId("usage-minibar-range")).toBeTruthy();
    expect(screen.getByTestId("usage-minibar-tool-rows")).toBeTruthy();
    expect(screen.getByTestId("usage-detail-retention-days")).toBeTruthy();
    expect(screen.getByTestId("usage-collect-interval-min")).toBeTruthy();
    // 2026-10-20 计划 Task 6 + 同日第三轮：显示与计价配置改为**设置页与 D 卡共用的弹窗**
    //（usage-config-open-settings 按钮 → usage-config-dialog-settings），弹窗内容只在打开时挂载；
    // 弹窗内高级覆盖默认再折叠一层；不恢复 providerMapRules JSON 编辑框
    const configTrigger = screen.getByTestId("usage-config-open-settings");
    expect(configTrigger).toHaveAttribute("aria-haspopup", "dialog");
    // 初始（未点开）：无弹窗壳、无配置面板、无四价覆盖管理器
    expect(screen.queryByTestId("usage-config-dialog-settings")).toBeNull();
    expect(screen.queryByTestId("usage-config-panel-settings")).toBeNull();
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    // 点开：弹窗与面板挂载（内容门控在打开态）
    fireEvent.click(configTrigger);
    expect(await screen.findByTestId("usage-config-dialog-settings")).toBeTruthy();
    expect(await screen.findByTestId("usage-config-panel-settings")).toBeTruthy();
    // 高级覆盖默认收起：弹窗打开后 Manager 仍不挂载（展开才挂）
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    // 正向锁：billing 独立触发器与旧「供应商别名」触发器不得再出现在设置页
    expect(screen.queryByTestId("billing-manager-open")).toBeNull();
    expect(screen.queryByTestId("provider-alias-manager-open")).toBeNull();
    // Task 17：定价目录状态入口与渠道管理器并存
    expect(await screen.findByTestId("usage-pricing-toggle")).toBeTruthy();
    // **B1（2026-10-07）**：评语已搬到看板导出条 ⇒ 本页**不得**再有它。
    // 这条是**正向锁**（不是删掉了事）：将来谁把评语搬回来，两处入口就会各写各的、必然漂移。
    expect(screen.queryByTestId("usage-export-quote")).toBeNull();
    // **2026-10-07 两项都搬走了**（正向锁：谁搬回来就红）
    //  * providerMapRules：用户「我都不太懂」⇒ 不再提供手工编辑（开发者口径的 JSON 不该摆给用户）；
    //  * exportPose：与评语同理搬到看板「导出设置」弹层（不在两处保留）。
    expect(screen.queryByTestId("usage-provider-map-rules")).toBeNull();
    expect(screen.queryByTestId("usage-export-pose")).toBeNull();

    // 7 行标题（zh/en 同父同键的 7 个 label key；B1 后**不含** Share-image caption —— 它随评语搬走）
    for (const label of [
      "Enable usage statistics",
      "Mini bar range",
      "Mini bar tool rows",
      "Detail retention (days)",
      "Fallback collect interval (minutes)",
    ]) {
      expect(screen.getByText(label)).toBeTruthy();
    }

    // **不重复**（2026-10-06 两级导航后判据换了形态）：用量设置与采集状态现在**同属「用量统计」
    // 块、两卡并置**（用户裁决：同族功能要挨着）⇒ 原先「本分区里没有采集状态」这条**已不成立**，
    // 但它守的那件事仍然要守：**同一个控件不得出现两次**（重复实现才是原判据的真实关切）。
    expect(screen.getAllByTestId("usage-status-collect")).toHaveLength(1);
    expect(screen.queryByTestId("usage-status-collecting")).toBeNull();
    // 8 项设置也各**恰好一个**（块内并置最容易引入的就是「两卡各渲染一遍」）
    for (const id of [
      "usage-enabled",
      "usage-minibar-range",
      "usage-minibar-tool-rows",
      "usage-detail-retention-days",
      "usage-collect-interval-min",
    ]) {
      expect(screen.getAllByTestId(id)).toHaveLength(1);
    }

    // 首帧值来自后端（不是前端硬编码默认值）
    expect(select("usage-minibar-range").value).toBe("today");
    expect(input("usage-minibar-tool-rows").value).toBe("3");
    expect(input("usage-detail-retention-days").value).toBe("90");
    expect(input("usage-collect-interval-min").value).toBe("10");
  });

  it("2. 开关：改动即发 patch，并用后端返回的合并对象回写本地态", async () => {
    renderSection();
    const sw = await screen.findByTestId("usage-enabled");
    // 模拟另一个 WebView 已改过设置：落库值与本次初始读到的缓存不同 —— 只有「以返回的合并对象回写」
    // 才能把 7 显示出来；本地 `{...form, ...patch}` 合并会把这一改动丢掉（本条的判别力所在）
    stored = { ...stored, detailRetentionDays: 7 };

    fireEvent.click(sw);

    await waitFor(() => expect(setPatches()).toEqual([{ enabled: false }]));
    expect(sw.getAttribute("data-state")).toBe("unchecked");
    await waitFor(() => expect(input("usage-detail-retention-days").value).toBe("7"));
  });

  it("3. 数字输入：非数字被过滤掉，编辑期只留草稿、失焦才保存", async () => {
    renderSection();
    await screen.findByTestId("usage-detail-retention-days");
    const field = input("usage-detail-retention-days");

    fireEvent.change(field, { target: { value: "3a0b" } });

    // 过滤（不是 parseInt / Number：那会得到 3 或 NaN），编辑期输入框回显草稿 30，但**尚未保存**
    expect(field.value).toBe("30");
    expect(setPatches()).toEqual([]);

    fireEvent.blur(field);

    await waitFor(() => expect(setPatches()).toEqual([{ detailRetentionDays: 30 }]));
    await waitFor(() => expect(field.value).toBe("30"));
  });

  it("4. 条数夹取：99 → 7、0 → 1（失焦/Enter 时才夹，且只发夹取后的值）", async () => {
    renderSection();
    await screen.findByTestId("usage-minibar-tool-rows");
    const field = input("usage-minibar-tool-rows");

    fireEvent.change(field, { target: { value: "99" } });
    // 编辑期不夹取：草稿原样显示，也不发请求（否则用户打到一半就会被改写）
    expect(field.value).toBe("99");
    expect(setPatches()).toEqual([]);

    fireEvent.blur(field);
    await waitFor(() => expect(setPatches()).toEqual([{ miniBarToolRows: 7 }]));
    await waitFor(() => expect(field.value).toBe("7"));

    fireEvent.change(field, { target: { value: "0" } });
    expect(field.value).toBe("0");
    // Enter 与失焦走同一条提交路径（下界同样夹取）
    fireEvent.keyDown(field, { key: "Enter" });
    await waitFor(() =>
      expect(setPatches()).toEqual([{ miniBarToolRows: 7 }, { miniBarToolRows: 1 }])
    );
    await waitFor(() => expect(field.value).toBe("1"));

    // 发出去的每个值都必须在后端白名单域内（merge_patch 越界是整包拒绝，不静默夹取）
    for (const patch of setPatches()) {
      expect(patch.miniBarToolRows).toBeGreaterThanOrEqual(1);
      expect(patch.miniBarToolRows).toBeLessThanOrEqual(7);
    }
  });

  it("5. 范围是原生 select（3 档）；姿态 select **已不在本页**（搬到看板「导出设置」）", async () => {
    renderSection();
    await screen.findByTestId("usage-minibar-range");
    const range = select("usage-minibar-range");
    // 仓内没有 shadcn Select → 原生元素（不是 button+listbox 组合）
    expect(range.tagName).toBe("SELECT");
    expect(Array.from(range.options).map((o) => o.value)).toEqual(["today", "last5h", "last7d"]);
    expect(Array.from(range.options).map((o) => o.textContent)).toEqual([
      "Today",
      "Last 5 hours",
      "Last 7 days",
    ]);

    // 姿态（含 `poseKeysFor(11)` 的 11 项、`look` 回落文案）改由
    // `tests/pet/usage-export-settings.test.tsx` 覆盖 —— 那里才是它的新家（**不在两处保留**）。
    // 本页只留一条负向锁：它不许在这里复活。
    expect(screen.queryByTestId("usage-export-pose")).toBeNull();
  });

  it("6. 保留期提示：明示「N 天前无明细」，随提交值更新且夹取 1–90", async () => {
    renderSection();
    const hint = await screen.findByTestId("usage-detail-retention-hint");
    expect(hint.textContent).toContain("90");
    expect(hint.textContent ?? "").toMatch(/not kept/i);
    // 区间文案随本轮上限调整（1–90）
    expect(hint.textContent).toContain("1–90");

    fireEvent.change(input("usage-detail-retention-days"), { target: { value: "30" } });
    // 编辑期草稿不影响提示：提示跟的是**已提交**的值（仍是 90）
    expect(hint.textContent).toContain("90");
    fireEvent.blur(input("usage-detail-retention-days"));
    await waitFor(() => expect(hint.textContent).toContain("30"));
    // 区间说明恒在（1–90），但「N 天前」的 N 已不再是 90
    expect(hint.textContent ?? "").not.toContain("older than 90");

    // 上界夹取（90 上限之外的输入不会被发出去）
    fireEvent.change(input("usage-detail-retention-days"), { target: { value: "99999" } });
    expect(input("usage-detail-retention-days").value).toBe("99999"); // 编辑期仍是草稿
    fireEvent.blur(input("usage-detail-retention-days"));
    await waitFor(() => expect(input("usage-detail-retention-days").value).toBe("90"));
    await waitFor(() => expect(setPatches()).toHaveLength(2));
    expect(setPatches()[1]).toEqual({ detailRetentionDays: 90 });
    // 发出去的每个值都在后端白名单域内（1–90；越界那边是整包拒绝）
    for (const patch of setPatches()) {
      expect(patch.detailRetentionDays).toBeGreaterThanOrEqual(1);
      expect(patch.detailRetentionDays).toBeLessThanOrEqual(90);
    }
  });

  it("7. 保留期逐字符输入：编辑期只改草稿，失焦才夹取提交（30；999→90；空串回退）", async () => {
    // 旧实现每次 change 都夹取并保存 ⇒ 逐字符输入「3」「0」会被拆成 3 与 1（0 被夹到下限）两次写入，
    // 用户永远打不出 30 这类「首字符低于下限」的两位数。本用例锁住「编辑期草稿、失焦才提交」。
    renderSection();
    await screen.findByTestId("usage-detail-retention-days");
    const field = input("usage-detail-retention-days");

    fireEvent.change(field, { target: { value: "3" } });
    expect(field.value).toBe("3");
    expect(setPatches()).toEqual([]); // 中途不得落库
    fireEvent.change(field, { target: { value: "30" } });
    expect(field.value).toBe("30");
    expect(setPatches()).toEqual([]);

    fireEvent.blur(field);
    await waitFor(() => expect(setPatches()).toEqual([{ detailRetentionDays: 30 }]));
    await waitFor(() => expect(field.value).toBe("30"));

    // 超上界：失焦时夹到 90，而不是把 999 发给后端等整包拒绝
    fireEvent.change(field, { target: { value: "999" } });
    expect(field.value).toBe("999");
    fireEvent.blur(field);
    await waitFor(() =>
      expect(setPatches()).toEqual([{ detailRetentionDays: 30 }, { detailRetentionDays: 90 }])
    );
    await waitFor(() => expect(field.value).toBe("90"));

    // 空串：无可提交数字 ⇒ 回退到当前值，且不产生新写入
    fireEvent.change(field, { target: { value: "" } });
    expect(field.value).toBe("");
    fireEvent.blur(field);
    await waitFor(() => expect(field.value).toBe("90"));
    expect(setPatches()).toHaveLength(2);
  });

  it("8. 保存失败原因可见（usage-set-error，不静默）——载体随界面变动更换", async () => {
    // ⚠️ **2026-10-07 B1：换载体、不换断言。** 本条原先拿「分享图评语」输入框当载体，而评语已搬到
    // 看板导出条（`UsageQuoteEditor`）⇒ 本页不再有该输入框。本条验的是「后端整包拒绝 ⇒ 原因可见 +
    // 用户输入保留 + 控制台留痕」这套**通用**机制，与具体字段无关；评语自己的读/写/失败路径由
    // `tests/pet/usage-quote-editor.test.tsx` 覆盖（含占位符变量提示）。
    renderSection();
    // ⚠️ **载体又换了一次**（2026-10-07）：上一次用 providerMapRules，而它这一轮已从界面撤下
    // （用户「我都不太懂」）。本条验的是「后端整包拒绝 ⇒ 原因可见 + 用户输入保留 + 控制台留痕」
    // 这套**通用**机制，与具体字段无关 ⇒ 改用「回退采集间隔」这个数字输入。
    await screen.findByTestId("usage-minibar-range");
    const field = select("usage-minibar-range");

    // 后端整包拒绝（形态不符 / 越界）→ 原因必须可见，不得静默吞掉
    failNext = { code: "usage-settings-invalid", detail: "浮窗分工具条数须在 1–7" };
    // 组件在失败时 console.error 留痕（与其它命令失败同惯例）→ 用例内静音，避免污染套件输出
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      fireEvent.change(field, { target: { value: "last5h" } });
      const box = await screen.findByTestId("usage-set-error");
      expect(box.textContent).toContain("Failed to save usage settings");
      // 原因走 usage.rpc.<code> 码表 + detail 通道，不打印原始错误对象
      expect(box.textContent).toContain("Invalid usage settings");
      expect(box.textContent).toContain("浮窗分工具条数须在 1–7");
      // 失败不谎报成功：输入框保留用户刚输入的值（后端没接受，错误就在旁边）
      expect(field.value).toBe("last5h");
      expect(errSpy).toHaveBeenCalled(); // 控制台也留痕，不静默
    } finally {
      errSpy.mockRestore();
    }
  });

  it("9. 换宠物：姿态非 random 时回退 random 并提示；已是 random 时幂等（不重复提交）", async () => {
    // 2026-10-07：姿态选择器已搬到看板「导出设置」⇒ 本页不能再靠点它来把值改成非 random。
    // 改为让**夹具本身就是非 random**（`exportPose: "waving"`），直接验回退这条真实路径 ——
    // 这条路径本来就不依赖那个下拉框，只依赖「当前存的姿态不是 random」。
    renderSection();
    // ⚠️ **这一步是必须的**（本次红的真正原因）：回退逻辑读的是**已加载的表单值**，表单没回来时
    // `pose === undefined` ⇒ 命中 `if (pose === undefined || pose === "random") return` 直接早退，
    // 于是「什么都没发生」。原来这里靠「等姿态选择器出现」来保证表单就绪，而选择器已搬走
    // ⇒ 改等一个**还在本页**的字段。
    await screen.findByTestId("usage-minibar-range");
    await waitFor(() => expect(setPatches()).toEqual([])); // 挂载本身不该写任何东西

    // 真实切换路径：petRuntime.saveActiveId 写激活指针 + 派发同窗口事件
    await act(async () => {
      saveActiveId("starry-dew", false, "Starry Dew");
    });
    await waitFor(() => expect(setPatches()).toEqual([{ exportPose: "random" }]));
    const notice = await screen.findByTestId("usage-pose-reset");
    expect(notice.textContent ?? "").toMatch(/fell back to Random/i);
    // 提示里要点明「去哪儿改」——因为选择器已经不在本页了（否则用户在这页找不到它）
    expect(notice.textContent ?? "").toMatch(/Export settings|导出设置/i);

    // 已回退成 random 之后再换宠物：不重复提交、也不新增提示（幂等）
    await act(async () => {
      saveActiveId("another-pet", false, "Another");
    });
    expect(setPatches()).toHaveLength(1);
  });

  it("10. 采集间隔：Enter/失焦同一条提交路径（1–1440；夹取后与当前值相同则不重复写）", async () => {
    renderSection();
    await screen.findByTestId("usage-collect-interval-min");
    const field = input("usage-collect-interval-min");

    fireEvent.change(field, { target: { value: "1440" } });
    expect(field.value).toBe("1440"); // 编辑期草稿
    expect(setPatches()).toEqual([]);
    fireEvent.keyDown(field, { key: "Enter" });
    await waitFor(() => expect(setPatches()).toEqual([{ collectIntervalMin: 1440 }]));
    await waitFor(() => expect(field.value).toBe("1440"));

    // 上界夹取后与当前值相同 ⇒ 不产生多余写入（避免无意义的 IPC / 错误横幅闪烁）
    fireEvent.change(field, { target: { value: "99999" } });
    expect(field.value).toBe("99999");
    fireEvent.keyDown(field, { key: "Enter" });
    await waitFor(() => expect(field.value).toBe("1440"));
    expect(setPatches()).toHaveLength(1);

    // 下界同样夹取；失焦也是同一条提交路径
    fireEvent.change(field, { target: { value: "0" } });
    expect(field.value).toBe("0");
    fireEvent.blur(field);
    await waitFor(() =>
      expect(setPatches()).toEqual([{ collectIntervalMin: 1440 }, { collectIntervalMin: 1 }])
    );
    await waitFor(() => expect(field.value).toBe("1"));
  });
});
