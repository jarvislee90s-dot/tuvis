// Task 11（计划②）· 导出动作条 `src/components/usage/UsageExportActions.tsx`（4 用例）。
// 判据来源：spec P4（复制文本摘要 / CSV 导出）、计划 §3 第 24 条（导出**必须走 Rust 落盘**，不经
// `<a download>`；统一经 `saveFile.ts` 接缝）、第 25 条（定位复用既有 `reveal_dir` 且**传文件路径**）、
// 第 26 条（剪贴板只作副本入口）、第 27 条（`.csv` 结尾 / `mam-usage-` 开头）、第 28 条（导出失败是
// `String` 自由文本 ⇒ **透传**，用量命令的结构化码才走 `usageErrMsg`）、第 15 条（导出链不得触发扫描）。
//
// 纪律：
//  * 语言固定 zh（detector 在 jsdom 下读 navigator(en-US) ⇒ 默认英文；本文件断言的中文文案只有 zh 有）。
//  * **渲染测试走共享 invoke mock**（`tests/setup.ts` 接进 `@tauri-apps/api/core` 的那个单例）：
//    不另起 `vi.mock("@tauri-apps/api/core")`，也不 mock `saveFile` 接缝 —— 落盘命令名、参数序、
//    `reveal_dir` 收到的是文件路径还是父目录，都要由**真实接缝 + 真实参数**证出来。
//  * 渲染的是**组件本体**（不渲染页面）：这样「导出链不出现 `usage_collect`」才是真断言——页面挂载
//    本来就会按需采集一次，渲染页面会把这条判据搅成噪音。
//  * 剪贴板在 jsdom 里不存在；用既有先例（`SignalHealthSection.test.tsx`）`Object.defineProperty`
//    给 `navigator.clipboard.writeText` 装一个捕获桩。
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { UsageExportActions } from "@/components/usage/UsageExportActions";
import { EM_DASH } from "@/lib/usage/format";
import { mockUsageCsv, mockUsageDashboard, mockUsageSettings } from "@/lib/usage/mockFixtures";
import type { TFn } from "@/lib/usage/range";
import type { UsageDashboard } from "@/types/usage";
import { tauriInvokeMock } from "../msw/tauriMocks";

// —— Task 12 追加：图片路径（导出图片 / 复制图片）的三个用例 ——
// jsdom 既没有 canvas 内核、也没有 `createImageBitmap`，故**只**替身出图链的两层：
//  · `sheet`：本用例关心「装配 + 落盘 + 剪贴板」，图集几何 / 加载由 `usage-sheet.test.ts` 直测；
//  · `exportImage`：出图由 `usage-export-image.test.ts` 用假画布直测；这里换成固定字节的 PNG，
//    于是 `export_save_bytes` 收到的 base64 能逐字断言（`AQID`），也免得导出链在 jsdom 里真去 fetch。
// **落盘接缝（`saveFile.ts`）不做替身**：命令名、参数序、剪贴板载荷都要由真实接缝证出来。
const { renderShareImageMock } = vi.hoisted(() => ({ renderShareImageMock: vi.fn() }));
vi.mock("@/lib/usage/exportImage", () => ({
  renderShareImage: renderShareImageMock,
  defaultExportDeps: (bitmap: unknown) => ({
    createCanvas: () => document.createElement("canvas"),
    bitmap,
  }),
}));
vi.mock("@/lib/usage/sheet", () => ({
  activePetSheet: () => Promise.resolve(null),
  loadSheet: () => Promise.resolve(null),
  // 「导出设置」弹层（2026-10-07）用 `poseKeysFor` 生成姿态选项：本文件只渲染导出条、
  // **不会打开弹层**，给一个形状对的替身即可（键名与本用例无关）
  poseKeysFor: (n: number) => Array.from({ length: n }, (_, i) => `pose-${i}`),
}));

/** 出图那一层的固定产物（字节 1,2,3 → base64 `AQID`） */
const pngBlob = new Blob([new Uint8Array([1, 2, 3])], { type: "image/png" });

/** jsdom 没有 `ClipboardItem`：捕获桩（`getType` 与真品同形，够断言「复制的就是那张 PNG」） */
class ClipboardItemStub {
  readonly types: string[];
  private readonly items: Record<string, Blob>;
  constructor(items: Record<string, Blob>) {
    this.items = items;
    this.types = Object.keys(items);
  }
  getType(type: string): Promise<Blob> {
    return Promise.resolve(this.items[type]);
  }
}

/** 剪贴板写图（`navigator.clipboard.write([ClipboardItem])` 的捕获桩） */
function installClipboardImage(write: (items: unknown[]) => Promise<void>): void {
  Object.defineProperty(window, "ClipboardItem", { value: ClipboardItemStub, configurable: true });
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: vi.fn(async () => {}), write },
    configurable: true,
  });
}

type InvokeCall = [string, Record<string, unknown> | undefined];

/** setup.ts 把 invoke 接到了这个 vi.fn 上（每用例后清空调用记录） */
function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

/** i18next 的 `t` 直接作 `TFn` 传入（与 `usage-page.test.tsx` 同法） */
const tf = i18n.t.bind(i18n) as unknown as TFn;

/** 剪贴板捕获桩（copyText 的副本出口） */
const clipboardWrite = vi.fn(async () => {});

/** 组件默认 props：看板页签 + 就近 7 天 + 夹具看板数据（导出只用到 range / groupBy / rows） */
function boardProps(over: Partial<Parameters<typeof UsageExportActions>[0]> = {}) {
  return {
    dash: mockUsageDashboard({ preset: "last7d" }, "tool") as UsageDashboard | null,
    range: { preset: "last7d" } as const,
    tab: "board" as const,
    filters: {},
    groupBy: "tool" as const,
    t: tf,
    ...over,
  };
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  clipboardWrite.mockClear();
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: clipboardWrite },
    configurable: true,
  });
});

describe("UsageExportActions（计划② Task 11：复制文本 + CSV 导出）", () => {
  it("1. 复制文本：摘要（含首行）交给剪贴板，反馈「已复制」，且这条链一次 IPC 都不发", async () => {
    render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-copy-text"));

    await waitFor(() => expect(clipboardWrite).toHaveBeenCalledTimes(1));
    // `copyText` 收到的就是这份摘要（它的唯一动作是把它交给剪贴板）
    const copied = clipboardWrite.mock.calls[0][0] as unknown as string;
    expect(copied).toContain("用量看板 · 近 7 天（09/27 – 10/03）");
    expect(copied).toContain("新增输入: 1,954,268");

    expect(screen.getByTestId("usage-export-toast")).toHaveTextContent("已复制");
    // §3 第 26 条：剪贴板只作**副本**入口，这条链不碰落盘、不碰查询（零 IPC）
    expect(tauriInvokeMock).not.toHaveBeenCalled();
  });

  it("2. 导出 CSV：内容取后端字符串 → saveText 落盘（export_save_text）→ 路径可见 + 可定位文件", async () => {
    render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() => expect(callsOf("export_save_text")).toHaveLength(1));

    // 取数：入参 = 当前档位 / 当前维度 / filters 恒 {}（不含 subagentMode ⇒ 不触发日档第 9 码）
    expect(callsOf("usage_export_csv")).toHaveLength(1);
    expect(callsOf("usage_export_csv")[0][1]).toEqual({
      range: { preset: "last7d" },
      groupBy: "tool",
      filters: {},
    });

    // 落盘：前端只做文件名与落盘 —— 内容**逐字**等于后端给的那串（不拼列、不改列序、不加表头）
    const csv = mockUsageCsv({ preset: "last7d" }, "tool", {});
    const [saveName, saveArgs] = callsOf("export_save_text")[0];
    expect(saveName).toBe("export_save_text");
    expect(saveArgs).toEqual({ name: "mam-usage-last7d.csv", content: csv });
    // 表头由后端给（列序 = 契约 CSV_HEADER），前端不得自造。
    // V2 只改展示摘要；Token CSV 的原始列（含 userEst）不变。
    expect(csv.startsWith("groupKey,label,inputFresh,cacheRead,cacheWrite,output,")).toBe(true);
    expect(csv.split("\n")[0]).toContain(",userEst,sourceKind");

    // §3 第 15 条：导出链只读账本，绝不触发扫描
    expect(callsOf("usage_collect")).toHaveLength(0);

    // 落盘**绝对路径**可见
    expect(await screen.findByTestId("usage-export-toast")).toHaveTextContent(
      "已保存：/Users/jarvis/Downloads/mam-usage-last7d.csv"
    );

    // 「查看/定位」复用既有 reveal_dir，且传的是**文件路径本身**（不是父目录）
    fireEvent.click(screen.getByTestId("usage-export-open-dir"));
    await waitFor(() => expect(callsOf("reveal_dir")).toHaveLength(1));
    expect(callsOf("reveal_dir")[0][1]).toEqual({
      path: "/Users/jarvis/Downloads/mam-usage-last7d.csv",
    });
  });

  it("3. 失败路径：落盘失败透传自由文本原因；取数失败走用量错误码渲染；两种情况都不给「打开所在目录」", async () => {
    // (a) 落盘失败：`export_save_text` reject 自由文本（**不是** {code,detail}）⇒ 原样透传，不得走 usageErrMsg
    //     两次 one-shot 按调用序：先给后端 CSV 字符串，再让落盘失败
    tauriInvokeMock
      .mockImplementationOnce(() => Promise.resolve("groupKey,label\nk,l\n"))
      .mockImplementationOnce(() => Promise.reject("disk full"));
    const view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent("导出失败：disk full")
    );
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    view.unmount();

    // 取数失败时不该走到落盘：以 (a) 已发生的调用数为基线，断言 (b) 没有新增
    const savesAfterA = callsOf("export_save_text").length;

    // (b) 取数失败：`usage_export_csv` 是用量命令 ⇒ reject 结构化 {code, detail}，必须走 usageErrMsg 的码表文案
    tauriInvokeMock.mockImplementationOnce(() =>
      Promise.reject({ code: "usage-db-failed", detail: "boom" })
    );
    render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-csv"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent("用量账本读写失败：boom")
    );
    expect(screen.getByTestId("usage-export-toast")).not.toHaveTextContent("[object Object]");
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    expect(callsOf("export_save_text")).toHaveLength(savesAfterA);
  });

  it("4. 记录页签下三个「看板口径」按钮**在位但禁用**（X7：不再凭空消失）；看板数据未就绪时同样禁用", () => {
    render(<UsageExportActions {...boardProps({ tab: "records", dash: null })} />);
    expect(screen.getByTestId("usage-export-csv")).toBeTruthy();
    // X7（2026-10-06）：口径不变（文本摘要与分享图都是看板口径的产物，不在记录页出口），
    // 但**不再整组撤掉**——以前 `queryByTestId(...)` 为 null，用户看到的是「按钮凭空消失」，
    // 以为功能坏了。现在在位 + `disabled` + `title` 说明原因。
    // 评语按钮（B1 从设置页搬来）也在同一条导出条上 ⇒ 同样「在位但禁用」
    for (const id of [
      "usage-export-copy-text",
      "usage-export-image",
      "usage-export-copy-image",
      "usage-export-settings-open",
    ]) {
      expect(screen.getByTestId(id)).toBeDisabled();
      expect(screen.getByTestId(id).getAttribute("title")).toBeTruthy();
    }
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    cleanup();

    // 看板页签但数据还没回来：按钮在位但不可点（点了也拼不出摘要）
    render(<UsageExportActions {...boardProps({ dash: null })} />);
    expect(screen.getByTestId("usage-export-copy-text")).toBeDisabled();
    expect(screen.getByTestId("usage-export-copy-text").getAttribute("title")).toBeNull();
    // 评语按钮**也随导出条一起禁用**（2026-10-07 B1）。它严格说只需要「设置」不需要看板数据，
    // 但整条导出条在数据未就绪时统一不可点更好懂（也就多禁用一瞬），故跟随 `!dash` 同纪律。
    expect(screen.getByTestId("usage-export-settings-open")).toBeDisabled();
    expect(screen.getByTestId("usage-export-csv")).not.toBeDisabled();
  });

  it("5. 导出图片：出图 → saveBytes 落盘（export_save_bytes）→ 路径可见 + 可定位；装配入参逐条对齐分享图版式", async () => {
    renderShareImageMock.mockResolvedValue(pngBlob);
    const view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(callsOf("export_save_bytes")).toHaveLength(1));

    // 落盘：前端只做 base64（1,2,3 → AQID）与文件名；文件名 = `mam-usage-<窗口标识>-<导出时刻>.png`
    // （X1，2026-10-06：**必须带导出时刻**——落盘层是裸 `fs::write`，同名即静默覆盖，而
    // `mam-usage-last7d.png` 跨天同名 ⇒ 昨天的图会被今天的悄悄盖掉且照样提示「已保存」）
    const saveArgs = callsOf("export_save_bytes")[0][1] as { name: string; base64: string };
    expect(saveArgs.name).toMatch(/^mam-usage-last7d-\d{8}-\d{6}\.png$/);
    expect(saveArgs.base64).toBe("AQID");
    // §3 第 15 条：导出链只读账本 / 设置，绝不触发扫描
    expect(callsOf("usage_collect")).toHaveLength(0);

    // 出图那一步收到的装配入参：brief 步骤 4 的钉死文案逐条到位（「用量趋势」只此一处小标题）
    const input = renderShareImageMock.mock.calls[0][0] as Record<string, unknown>;
    expect(input.brand).toBe("兔维斯 · 用量看板");
    expect(input.rangeLabel).toBe("近 7 天 · 09/27 – 10/03");
    expect(input.heroLabel).toBe("近 7 天 Token 合计");
    expect(input.trendTitle).toBe("用量趋势");
    expect(input.groupTitle).toBe("分组分布");
    expect(input.toolsTitle).toBe("工具调用");
    expect(input.footer).toBe("纯 token · 含子代理 · 本地聚合");
    expect(input.hero).toBe("2,036,981");
    expect(input.metrics as unknown[]).toHaveLength(6);
    const metricLabels = (input.metrics as string[][]).map(([label]) => label);
    expect(metricLabels).toContain("输出");
    expect(metricLabels).toContain("未缓存命中输入");
    // V2：用户输入估值不再进入分享图
    expect(metricLabels).not.toContain("用户输入(估) · 含子代理");
    expect(input.groups as unknown[]).not.toHaveLength(0);
    expect(input.tools2x2 as unknown[]).toHaveLength(4);
    expect(input.pose).toBe("random"); // 设置缺省（夹具的 exportPose = random）
    expect(input.quote as string).not.toContain("{"); // 4 个占位符都已填
    expect(input.sheet).toBeNull(); // 图集替身给 null → 不画立绘也要出图
    expect(renderShareImageMock.mock.calls[0][1]).toMatchObject({ bitmap: null });

    // 落盘**绝对路径**可见 + 复用 reveal_dir（传文件路径本身）；路径里的文件名就是上面那次落盘的名字
    expect(await screen.findByTestId("usage-export-toast")).toHaveTextContent(
      `已保存：/Users/jarvis/Downloads/${saveArgs.name}`
    );
    fireEvent.click(screen.getByTestId("usage-export-open-dir"));
    await waitFor(() => expect(callsOf("reveal_dir")).toHaveLength(1));
    expect(callsOf("reveal_dir")[0][1]).toEqual({
      path: `/Users/jarvis/Downloads/${saveArgs.name}`,
    });
    view.unmount();

    // `collectedAt === 0` 是「尚未采集」哨兵（§3 第 8 条）→ heroSub 渲染 usage.notCollected
    const dash = mockUsageDashboard({ preset: "last7d" }, "tool") as UsageDashboard;
    render(<UsageExportActions {...boardProps({ dash: { ...dash, collectedAt: 0 } })} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(renderShareImageMock).toHaveBeenCalledTimes(2));
    expect((renderShareImageMock.mock.calls[1][0] as { heroSub: string }).heroSub).toContain(
      "尚未采集"
    );

    // 图片两个入口的可见性纪律：记录页签下**在位但禁用**（X7：不再整组消失）；看板数据未就绪时不可点
    cleanup();
    render(<UsageExportActions {...boardProps({ tab: "records", dash: null })} />);
    expect(screen.getByTestId("usage-export-image")).toBeDisabled();
    expect(screen.getByTestId("usage-export-copy-image")).toBeDisabled();
    cleanup();
    render(<UsageExportActions {...boardProps({ dash: null })} />);
    expect(screen.getByTestId("usage-export-image")).toBeDisabled();
    expect(screen.getByTestId("usage-export-copy-image")).toBeDisabled();
  });

  it("6. 复制图片：出图 → 剪贴板（ClipboardItem image/png）→「图片已复制」；成功也不给「打开所在目录」", async () => {
    renderShareImageMock.mockResolvedValue(pngBlob);
    const written: unknown[] = [];
    const clipboardWrite = vi.fn(async (items: unknown[]) => {
      written.push(items[0]);
    });
    installClipboardImage(clipboardWrite);

    render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-copy-image"));
    await waitFor(() => expect(clipboardWrite).toHaveBeenCalledTimes(1));

    const item = written[0] as ClipboardItemStub;
    expect(item.types).toEqual(["image/png"]);
    // 复制进去的就是出图那一步产出的那张 PNG（逐字节同一对象，不是重画的另一张）
    expect(await item.getType("image/png")).toBe(pngBlob);
    expect(await screen.findByTestId("usage-export-toast")).toHaveTextContent("图片已复制");

    // best-effort 副本：**不落盘**；成功也没有路径可定位，故不给「打开所在目录」
    expect(callsOf("export_save_bytes")).toHaveLength(0);
    expect(callsOf("reveal_dir")).toHaveLength(0);
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
  });

  it("7. 失败路径：出图抛错 / 落盘失败 / 定位失败 / 剪贴板失败都行内可见，且都不给「打开所在目录」", async () => {
    // (a) 出图这条链 rejection（`toBlob` 失败等）→ 处理器**自己的** catch（`void` 不会吞掉错误）
    renderShareImageMock.mockRejectedValueOnce(new Error("canvas boom"));
    let view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent(
        "导出失败：Error: canvas boom"
      )
    );
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    view.unmount();

    // (b) 落盘失败：`export_save_bytes` reject 自由文本（**不是** {code,detail}）⇒ 原样透传
    //     两次 one-shot 按调用序：先给设置（点击时读一次），再让落盘失败
    renderShareImageMock.mockResolvedValueOnce(pngBlob);
    tauriInvokeMock
      .mockImplementationOnce(() => Promise.resolve(mockUsageSettings()))
      .mockImplementationOnce(() => Promise.reject("disk full"));
    view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent("导出失败：disk full")
    );
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    view.unmount();

    // (c) 定位失败：文件其实已经落盘（不改「导出成功」的结论），但入口撤掉 + 如实报原因
    renderShareImageMock.mockResolvedValueOnce(pngBlob);
    view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(screen.getByTestId("usage-export-open-dir")).toBeTruthy());
    tauriInvokeMock.mockImplementationOnce(() => Promise.reject("reveal denied"));
    fireEvent.click(screen.getByTestId("usage-export-open-dir"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent("导出失败：reveal denied")
    );
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    view.unmount();
    // (b)(c) 各落盘一次（失败的一次 + 成功的一次）；以它为基线断言 (d) **一次都没写盘**
    const savesAfterC = callsOf("export_save_bytes").length;
    expect(savesAfterC).toBe(2);

    // (d) 剪贴板不可用（`ClipboardItem` 全仓零先例、secure-context 前提未验证）⇒ 如实报原因，不谎报「已复制」。
    //     文案走**复制专属**键（X5：以前复用「导出失败：…」，可这条路径根本没有导出任何文件，
    //     文案把人引向磁盘、而真正要查的是剪贴板权限）
    renderShareImageMock.mockResolvedValueOnce(pngBlob);
    installClipboardImage(async () => {
      throw new Error("no clipboard");
    });
    view = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-copy-image"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-export-toast")).toHaveTextContent(
        "复制失败：Error: no clipboard"
      )
    );
    expect(callsOf("export_save_bytes")).toHaveLength(savesAfterC); // 复制链不落盘
    expect(screen.queryByTestId("usage-export-open-dir")).toBeNull();
    view.unmount();
  });

  it("8. 分享图：没量到 token 的窗口 → 四桶派生的指标行出 `—`、请求次数与用户输入(估)出真值、不画零线（P8）", async () => {
    // 真机形状：四桶全零但 requests ≥ 1（真空回合）。分享图是**给外部看的静态图**，口径与看板一致：
    // 四桶派生的位出 `—`、计数类与 `用户输入(估)` 出真值；趋势**不画零线**（全零会被 layout 画成
    // 贴底平线 = 假图）。
    renderShareImageMock.mockResolvedValue(pngBlob);
    const d = mockUsageDashboard({ preset: "last7d" }, "tool") as UsageDashboard;
    // 前提锚（2026-10-06 第三轮补）：下面「请求次数出真值」那句若夹具的 requests 恰是 0，就会
    // **空洞地绿** ⇒ 先钉住它是**非零真值**。
    expect(d.totals.requests).toBeGreaterThan(0);
    expect(d.totals.userEst).toBeGreaterThan(0); // `用户输入(估)` 同款锚（本轮恢复 KEEP 的那一格）
    const zero = { inputFresh: 0, cacheRead: 0, cacheWrite: 0, output: 0 };
    // ⚠️ **本夹具只喂被断言的字段**（四个桶 / `metrics` / `trend` / `groups`），**不是真机形状**：
    // 真机在这一档 `recentSession` 必为 `null`（四桶全零 ⇒ 每行 `request_total` 也是 0 ⇒ 后端候选集
    // 为空，见 `mockFixtures.ts::zeroTokensOf` 的长注释）；本文件没有任何消费方读它（`buildShareInput`
    // 只看 `trend` / `totals` / `totalsBuckets` / `rows` / `hero` / `workSummary` / `range` /
    // `collectedAt`），故**没有**为它补形状。后来者**不要照抄**。
    const noTokens: UsageDashboard = {
      ...d,
      rows: d.rows.map((r) => ({
        ...r,
        buckets: { ...zero },
        metrics: { ...r.metrics, requestTotal: 0, cacheHitRate: 0 }, // requests 是真值，保留
      })),
      totalsBuckets: { ...zero },
      totals: { ...d.totals, requestTotal: 0, cacheHitRate: 0 },
      hero: 0,
      trend: d.trend.map((p) => ({
        ...p,
        buckets: { ...zero },
        metrics: { ...p.metrics, requestTotal: 0, cacheHitRate: 0 },
      })),
    };
    const view = render(<UsageExportActions {...boardProps({ dash: noTokens })} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(renderShareImageMock).toHaveBeenCalledTimes(1));

    const input = renderShareImageMock.mock.calls[0][0] as Record<string, unknown>;
    const metrics = input.metrics as [string, string][];
    const valueOf = (label: string) => metrics.find(([l]) => l === label)?.[1];
    // 头部 hero（`:160`）与评语气泡的 `{tokens}`（`:149`）都出 `—`
    expect(input.hero).toBe(EM_DASH);
    expect(input.quote as string).not.toContain("{"); // 4 个占位符都已填（tokens 位填的是 —）
    // **四个由 token 桶派生的指标行**出 `—`（新增输入 / 缓存命中 / 未缓存命中 / 输出 / 命中率；
    // 2026-10-20 Task 8：计量维度值统一——新增输入 / 输出）
    for (const label of ["新增输入", "缓存命中", "未缓存命中输入", "输出", "命中率"]) {
      expect(valueOf(label)).toBe(EM_DASH);
    }
    // **请求次数出真值**：断**具体数字**（不是模板，也明确不许是 `—`）
    expect(valueOf("请求次数")).toBe("460");
    expect(valueOf("请求次数")).not.toBe(EM_DASH);
    // V2：用户输入估值不在分享图指标里
    expect(valueOf("用户输入(估) · 含子代理")).toBeUndefined();
    // 趋势：**不把全零点交给画布**（否则画成贴底零线）⇒ 空数组，交给布局层的空态分支
    expect(input.points as unknown[]).toHaveLength(0);
    // 分组：分布行值全为 0 ⇒ 丢光 ⇒ 交给布局层的空态分支（不是一行 0）
    expect(input.groups as unknown[]).toHaveLength(0);
    view.unmount();

    // **反断言**：有数据的窗口照旧印数字（不得把正常路径也置空）
    renderShareImageMock.mockClear();
    const okView = render(<UsageExportActions {...boardProps()} />);
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(renderShareImageMock).toHaveBeenCalledTimes(1));
    const okInput = renderShareImageMock.mock.calls[0][0] as Record<string, unknown>;
    expect(okInput.hero).toBe("2,036,981");
    const okMetrics = okInput.metrics as [string, string][];
    expect(okMetrics.find(([l]) => l === "输出")?.[1]).toBe("82,713");
    expect(okMetrics.find(([l]) => l === "未缓存命中输入")?.[1]).toBe("636,145");
    // V2：userEst=999 也不得进入分享图（不是 API Token）
    expect(okMetrics.find(([l]) => l === "用户输入(估) · 含子代理")).toBeUndefined();
    expect(okInput.points as unknown[]).not.toHaveLength(0);
    okView.unmount();
  });

  it("9. userEst=999 夹具：分享图装配入参不含用户输入估值", async () => {
    renderShareImageMock.mockResolvedValue(pngBlob);
    const d = mockUsageDashboard({ preset: "last7d" }, "tool") as UsageDashboard;
    const view = render(
      <UsageExportActions
        {...boardProps({ dash: { ...d, totals: { ...d.totals, userEst: 999 } } })}
      />
    );
    fireEvent.click(screen.getByTestId("usage-export-image"));
    await waitFor(() => expect(renderShareImageMock).toHaveBeenCalledTimes(1));

    const input = renderShareImageMock.mock.calls[0][0] as Record<string, unknown>;
    const metrics = input.metrics as [string, string][];
    expect(metrics.map(([label]) => label)).not.toContain("用户输入(估) · 含子代理");
    expect(JSON.stringify(input)).not.toContain("~999");
    view.unmount();
  });
});
