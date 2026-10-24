// tests/settings/zcodeRemoteSettings.test.tsx — A5（设计 §4.1/§4.2/§4.3）：
// 「④ 工具特殊配置」首版只放 **ZCode 一张折叠卡**。
// 覆盖三条契约：
//  1. 默认收起 + 键盘可操作 + 草稿/已存值分开 + 折叠不删配置（zcode_card_starts_collapsed）；
//  2. 保存失败**就地**报错且不破坏任何东西：已存值与草稿都保留、复制取已存值、
//     删除与空白保存同源（config_error_is_inline_and_non_destructive）；
//  3. 教程是应用内取链四步，**不承诺自动登录／网页引导／固定有效期**；「打开官方页面」
//     是用户显式动作；**仅填链接不显示已联机**（语义红线：A4 的 saved 只表示格式已落库）。
// 夹具只用合成 sid，不含任何真实链接（设计 §4.3「完整链接不写测试夹具」）。
// mock 模式沿用 tests/settings/remoteSection.test.tsx（vi.hoisted + invoke/event/sonner/opener）。
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock, toastSuccessMock, toastErrorMock, openUrlMock, writeTextMock } = vi.hoisted(
  () => ({
    invokeMock: vi.fn(),
    toastSuccessMock: vi.fn(),
    toastErrorMock: vi.fn(),
    openUrlMock: vi.fn(async () => {}),
    writeTextMock: vi.fn(async () => {}),
  })
);
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// useAppTranslation 内部 listen("@tauri-apps/api/event") 在 jsdom 无 Tauri 内核，须 mock
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), {
    info: vi.fn(),
    success: toastSuccessMock,
    error: toastErrorMock,
  }),
}));
// 「打开官方页面」走系统浏览器（@tauri-apps/plugin-opener），jsdom 无内核
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: openUrlMock }));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import { ToolRemoteSettingsSection } from "@/components/settings/ToolRemoteSettingsSection";

void i18n;

// ---- 合成夹具（A4 wire DTO：{configState, url}，坏值恒 null 不下发）----
const SAVED_URL = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a5-0001";
const DRAFT_URL = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a5-0002";
const BAD_DRAFT = "https://evil.example/remote/v4?sid=synthetic-sid-a5-0003";

const missing = { configState: "missing", url: null };
const saved = { configState: "saved", url: SAVED_URL };
const invalid = { configState: "invalid", url: null };

/** 默认后端行为：读 saved；写回显（空白 → missing，其余 → saved） */
function mockBackend(getPayload: unknown = saved) {
  invokeMock.mockImplementation(async (cmd: string, args?: { rawUrl?: string }) => {
    if (cmd === "zcode_remote_config_get") return getPayload;
    if (cmd === "zcode_remote_config_set") {
      const raw = args?.rawUrl ?? "";
      if (raw.trim() === "") return missing;
      if (!raw.startsWith("https://zcode.z.ai/")) {
        throw "ZCODE_URL_HOST: 仅支持 zcode.z.ai 官方主机";
      }
      return { configState: "saved", url: raw };
    }
    return null;
  });
}

const setCalls = () => invokeMock.mock.calls.filter((c) => c[0] === "zcode_remote_config_set");
const getCalls = () => invokeMock.mock.calls.filter((c) => c[0] === "zcode_remote_config_get");

const header = () => screen.getByTestId("zcode-card-header");
const panel = () => document.querySelector('[data-testid="zcode-card-panel"]');
const input = () => screen.getByLabelText(/Official remote-control link/i) as HTMLInputElement;
const stateText = () => screen.getByTestId("zcode-config-state").textContent;

beforeEach(() => {
  invokeMock.mockReset();
  openUrlMock.mockReset();
  writeTextMock.mockReset();
  toastSuccessMock.mockReset();
  toastErrorMock.mockReset();
  // jsdom 无剪贴板内核
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: writeTextMock },
    configurable: true,
  });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("ZcodeRemoteSettingsCard 默认收起（A5 zcode_card_starts_collapsed）", () => {
  it("zcode_card_starts_collapsed：只有 ZCode 头部；点击／键盘展开；草稿与已存值分开；折叠不删配置", async () => {
    mockBackend(saved);
    render(<ToolRemoteSettingsSection />);

    // ① 首版只渲染 ZCode 一张卡——不交付空工具卡，也不预建多工具框架
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    expect(document.querySelectorAll("[data-tool-card]")).toHaveLength(1);
    expect(document.querySelector('[data-tool-card="zcode"]')).toBeTruthy();
    expect(screen.queryByText("Claude")).toBeNull();
    expect(screen.queryByText("Codex")).toBeNull();

    // ② 收起态：卡头有工具图标 + ZCode + 简短配置状态 + 展开指示；卡内配置不可见
    const head = header();
    expect(head).toHaveAttribute("aria-expanded", "false");
    // aria-controls 指向收起态**不存在**的面板（展开后才挂出——不靠 visibility 藏着）
    expect(head.getAttribute("aria-controls")).toBe("zcode-remote-card-panel");
    expect(document.getElementById("zcode-remote-card-panel")).toBeNull();
    expect(head.textContent).toContain("ZCode");
    expect(within(head).getByText("ZCode")).toBeTruthy(); // 工具名（品牌字面量）
    expect(stateText()).toContain("Saved");
    expect(head.querySelector("svg")).toBeTruthy(); // 工具图标 + 展开指示
    expect(panel()).toBeNull();
    expect(screen.queryByLabelText(/Official remote-control link/i)).toBeNull();
    expect(screen.queryByRole("button", { name: /^save$/i })).toBeNull();

    // ③ 点击展开：aria-expanded=true，aria-controls 指向真实存在的面板
    fireEvent.click(head);
    expect(header()).toHaveAttribute("aria-expanded", "true");
    const panelId = header().getAttribute("aria-controls")!;
    expect(document.getElementById(panelId)).toBeTruthy();
    // 展开即回显已存值（读的是配置，不是连接——不发起任何连接动作）
    expect(input().value).toBe(SAVED_URL);
    expect(setCalls()).toHaveLength(0);

    // ④ 草稿与已存值分开：改输入框**不**落库、不改配置状态
    fireEvent.change(input(), { target: { value: DRAFT_URL } });
    expect(setCalls()).toHaveLength(0);
    expect(stateText()).toContain("Saved");
    expect(document.body.textContent).not.toMatch(/connected/i);

    // ⑤ 折叠不删配置、不触发连接：收起 → 再展开，草稿与已存值都还在，零写调用
    fireEvent.click(header());
    expect(header()).toHaveAttribute("aria-expanded", "false");
    expect(panel()).toBeNull();
    expect(setCalls()).toHaveLength(0);
    fireEvent.click(header());
    expect(input().value).toBe(DRAFT_URL);
    expect(stateText()).toContain("Saved");

    // ⑥ 键盘 Enter／Space 可操作（原生 button 语义）
    cleanup();
    invokeMock.mockClear(); // 重新挂载另起一笔读数，计数从零再算
    mockBackend(saved);
    render(<ToolRemoteSettingsSection />);
    const user = userEvent.setup();
    const head2 = header();
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    head2.focus();
    await user.keyboard("{Enter}");
    expect(header()).toHaveAttribute("aria-expanded", "true");
    await user.keyboard("{Enter}");
    expect(header()).toHaveAttribute("aria-expanded", "false");
    await user.keyboard("{ }");
    expect(header()).toHaveAttribute("aria-expanded", "true");
  });

  it("未配置／历史坏值各给一句简短状态；读取只用 zcode_remote_config_get（不探测连接）", async () => {
    mockBackend(missing);
    const view = render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(stateText()).toContain("Not configured"));
    expect(setCalls()).toHaveLength(0);

    view.unmount();
    cleanup();
    mockBackend(invalid);
    render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(stateText()).toContain("needs updating"));
    // invalid = 历史值复验不通过：url 恒 null（坏值不下发），展开后输入框是空的
    fireEvent.click(header());
    expect(input().value).toBe("");
    expect(screen.getByTestId("zcode-invalid-note")).toBeTruthy();
    expect(setCalls()).toHaveLength(0);
  });
});

describe("ZcodeRemoteSettingsCard 就地报错与非破坏保存（A5 config_error_is_inline_and_non_destructive）", () => {
  it("config_error_is_inline_and_non_destructive：坏链接错误紧邻输入框；失败保留草稿与已存值；复制用已存值；删除与空白保存同源", async () => {
    mockBackend(saved);
    render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    fireEvent.click(header());

    // ① 保存失败：错误**紧邻输入框**并关联可访问性说明（aria-invalid + aria-describedby），
    //    不是只弹 toast
    fireEvent.change(input(), { target: { value: BAD_DRAFT } });
    fireEvent.click(screen.getByTestId("zcode-save"));
    await waitFor(() => expect(setCalls()).toHaveLength(1));
    expect(setCalls()[0][1]).toEqual({ rawUrl: BAD_DRAFT });

    const err = await screen.findByTestId("zcode-url-error");
    // 后端码（ZCODE_URL_HOST）经 formatInvokeError 本地化（jsdom 默认英文）
    expect(err.textContent).toContain("Only the official zcode.z.ai host is supported");
    expect(err).toHaveAttribute("role", "alert");
    expect(input()).toHaveAttribute("aria-invalid", "true");
    expect(input().getAttribute("aria-describedby")).toContain(err.id);
    // 只落字段旁的错误，不额外用 toast 重复播报
    expect(toastErrorMock).not.toHaveBeenCalled();

    // ② 失败保留草稿，也保留已存值（卡头状态不变、不假装删掉了什么）
    expect(input().value).toBe(BAD_DRAFT);
    expect(stateText()).toContain("Saved");
    fireEvent.click(header());
    fireEvent.click(header());
    expect(input().value).toBe(BAD_DRAFT);

    // ③ 复制走**已保存值**，不是草稿
    fireEvent.click(screen.getByTestId("zcode-copy"));
    await waitFor(() => expect(writeTextMock).toHaveBeenCalledWith(SAVED_URL));

    // ④ 删除 = 空白保存同源（同一条 set 出口，rawUrl 空串）
    fireEvent.click(screen.getByTestId("zcode-delete"));
    await waitFor(() => expect(setCalls()).toHaveLength(2));
    expect(setCalls()[1][1]).toEqual({ rawUrl: "" });
    await waitFor(() => expect(stateText()).toContain("Not configured"));
    expect(input().value).toBe("");
    expect(toastErrorMock).not.toHaveBeenCalled();

    // ⑤ 空白保存（清空后点保存）与删除同一条路径
    fireEvent.change(input(), { target: { value: DRAFT_URL } });
    fireEvent.click(screen.getByTestId("zcode-save"));
    await waitFor(() => expect(setCalls()).toHaveLength(3));
    expect(setCalls()[2][1]).toEqual({ rawUrl: DRAFT_URL });
    await waitFor(() => expect(stateText()).toContain("Saved"));
    // 成功后错误条消失（不残留上一次失败）
    expect(screen.queryByTestId("zcode-url-error")).toBeNull();

    fireEvent.change(input(), { target: { value: "" } });
    fireEvent.click(screen.getByTestId("zcode-save"));
    await waitFor(() => expect(setCalls()).toHaveLength(4));
    expect(setCalls()[3][1]).toEqual({ rawUrl: "" });
    await waitFor(() => expect(stateText()).toContain("Not configured"));
    expect(screen.getByTestId("zcode-copy")).toBeDisabled();
    expect(screen.getByTestId("zcode-open-official")).toBeDisabled();
  });

  it("已保存只说「待联机」：任何时刻都不出现「已联机／connected」这类宣称", async () => {
    mockBackend(missing);
    render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    fireEvent.click(header());
    fireEvent.change(input(), { target: { value: DRAFT_URL } });
    fireEvent.click(screen.getByTestId("zcode-save"));
    await waitFor(() => expect(stateText()).toContain("Saved"));
    // 填了链接 ≠ 已联机（设计 §4.2 语义红线）
    expect(document.body.textContent).not.toMatch(/connected/i);
    expect(document.body.textContent).not.toContain("已联机");
    expect(screen.getByTestId("zcode-pending-note")).toBeTruthy();
  });
});

describe("ZcodeRemoteSettingsCard 教程与外跳（A5 §4.2）", () => {
  it("教程是显式动作 + 应用内取链四步；不承诺自动登录／网页引导／固定有效期；不自动外跳", async () => {
    mockBackend(saved);
    render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    fireEvent.click(header());

    // 收起态不预先展示教程；点击才展开（显式动作）
    expect(screen.queryByTestId("zcode-tutorial")).toBeNull();
    expect(openUrlMock).not.toHaveBeenCalled(); // 挂载/展开从不自动打开用户 ZCode 或浏览器
    fireEvent.click(screen.getByTestId("zcode-tutorial-toggle"));
    const tutorial = screen.getByTestId("zcode-tutorial");
    expect(within(tutorial).getAllByRole("listitem")).toHaveLength(4);
    // 四步：APP 内开移动远控 → 开启 → 复制链接／扫码 → 粘贴
    expect(tutorial.textContent).toMatch(/ZCode app/);
    expect(tutorial.textContent).toMatch(/remote control/i);
    expect(tutorial.textContent).toMatch(/QR/);
    expect(tutorial.textContent).toMatch(/[Pp]aste/);
    // 不承诺：自动登录 / 网页引导 / 固定有效月数
    expect(tutorial.querySelector("a[href]")).toBeNull();
    // 明示「不代劳」的申报在场（不承诺自动登录／代开代关）
    expect(tutorial.textContent).toMatch(/never/i);
    expect(tutorial.textContent).not.toMatch(/auto(matic(ally)?)? (log|sign|open)/i);
    // 不承诺固定有效期
    expect(tutorial.textContent).not.toMatch(/\b\d+ (days?|months?)\b/i);

    // 「打开官方页面」只在用户点击时用**已保存值**外跳
    fireEvent.change(input(), { target: { value: DRAFT_URL } });
    fireEvent.click(screen.getByTestId("zcode-open-official"));
    await waitFor(() => expect(openUrlMock).toHaveBeenCalledWith(SAVED_URL));
    expect(openUrlMock.mock.calls[0][0]).not.toBe(DRAFT_URL);
  });

  it("装配段只挂 ZCode 一张卡（不预建多工具框架；分组锚与顺序由 remoteSection 的顺序锁守）", async () => {
    mockBackend(saved);
    render(<ToolRemoteSettingsSection />);
    await waitFor(() => expect(getCalls()).toHaveLength(1));
    expect(document.querySelectorAll("[data-tool-card]")).toHaveLength(1);
    // 挂载/展开从不自动外跳、自动扫描登录态或注册设备
    expect(openUrlMock).not.toHaveBeenCalled();
    expect(screen.queryByText("Claude")).toBeNull();
    expect(screen.queryByText("Codex")).toBeNull();
  });
});

// i18n 契约（同 tests/settings/remoteSection.test.tsx 口径）：新组件引用的每个
// settings.remote.* 键必须在 zh 与 en 同齐备——改文案不许只改一边。
describe("ZcodeRemoteSettingsCard i18n zh/en 无缺键（A5）", () => {
  const root = process.cwd();
  const flat = (obj: Record<string, unknown>, prefix = ""): string[] =>
    Object.entries(obj).flatMap(([k, v]) =>
      typeof v === "object" && v !== null
        ? flat(v as Record<string, unknown>, `${prefix}${k}.`)
        : [`${prefix}${k}`]
    );

  it("新组件引用的 settings.remote.* 键 zh/en 双语齐备，且 settings.remote 子树键集相等", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const zhKeys = new Set(flat(zh.settings.remote).map((k) => `settings.remote.${k}`));
    const enKeys = new Set(flat(en.settings.remote).map((k) => `settings.remote.${k}`));
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([]);
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([]);

    const sources = [
      path.join(root, "src/components/settings/ToolRemoteSettingsSection.tsx"),
      path.join(root, "src/components/settings/ZcodeRemoteSettingsCard.tsx"),
    ];
    const used = new Set<string>();
    for (const f of sources) {
      const text = readFileSync(f, "utf8");
      for (const m of text.matchAll(/settings\.remote\.([A-Za-z0-9_.]+)/g)) {
        used.add(m[0]);
      }
    }
    expect(used.size).toBeGreaterThan(8);
    expect([...used].filter((k) => !zhKeys.has(k))).toEqual([]);
    expect([...used].filter((k) => !enKeys.has(k))).toEqual([]);
  });

  it("手机侧发送区原因键（mobile.composer.zcode*）zh/en 同齐备", () => {
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    const api = readFileSync(path.join(root, "src/mobile/api.ts"), "utf8");
    const used = [...api.matchAll(/mobile\.composer\.(zcode[A-Za-z0-9_]*)/g)].map((m) => m[1]);
    expect(used.length).toBeGreaterThan(3);
    for (const key of new Set(used)) {
      expect(zh.mobile.composer[key], `zh 缺 mobile.composer.${key}`).toBeTypeOf("string");
      expect(en.mobile.composer[key], `en 缺 mobile.composer.${key}`).toBeTypeOf("string");
    }
  });
});
