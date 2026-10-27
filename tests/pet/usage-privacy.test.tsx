// Task 11（计划②）· 隐私白名单的**渲染级**门禁（计划 §3 第 23 条；spec P8 纪律项）——1 用例。
// 「四个面 + 三种导出只允许静态文本与结构化摘要；禁止 prompt 原文、代码内容、密钥或任何会话正文。」
//
// 做法：把**会话数据源**整条换成带金丝雀正文的那条（走共享 invoke mock 的会话命令 ⇒ 任何读会话的
// 组件在这个进程里都会拿到它），再渲染记录页，断言 `document.body.textContent` 里没有金丝雀。
// 用量面**结构上**读不到会话正文（`UsageDashboard` / `UsageRecords` 契约里没有正文字段），所以这条
// 断言是**回归绊线**：将来谁把会话正文（或 `lastMessage` 这类字段）接进用量面，这里立刻红。
//
// 纪律：语言固定 zh（detector 在 jsdom 下读 navigator(en-US) ⇒ 默认英文）；窗口 API 全 mock
// （与 `usage-records.test.tsx` 同法）；用量查询一律走共享 invoke mock，不另起 `vi.mock`。
import { fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（theme-provider 的首帧主题判定会用到；与 usage-page.test.tsx 同模式）
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

// WindowFrame/TitleBar 依赖 Tauri 窗口 API，jsdom 下全 mock（与 petSettings/toolManagement 同模式）
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
import { UsageDashboardBody } from "@/components/usage/UsageDashboardBody";
import { tauriInvokeMock } from "../msw/tauriMocks";

/** 金丝雀：任何会话正文出现在用量面都算隐私白名单失守 */
const CANARY = "CANARY_PROMPT_DO_NOT_RENDER";

/** 真机 `get_all_sessions` 的单条形状（正文 = `lastMessage`） */
function canarySession() {
  return {
    id: "canary-session",
    agentType: "claude",
    projectName: "canary-project",
    projectPath: "/tmp/canary-project",
    title: "演示会话",
    gitBranch: null,
    githubUrl: null,
    status: "processing",
    lastMessage: CANARY,
    lastMessageRole: "assistant",
    lastActivityAt: new Date().toISOString(),
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "cli",
    jumpSupported: true,
  };
}

/** 共享 mock 的原始实现（用例结束后还原：别把金丝雀留在别的用例里） */
type InvokeImpl = (cmd: string, args?: unknown) => unknown;
const invokeMock = tauriInvokeMock as unknown as {
  mock: { calls: [string, unknown][] };
  getMockImplementation(): InvokeImpl | undefined;
  mockImplementation(fn: InvokeImpl): void;
};
const baseImpl = invokeMock.getMockImplementation();

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
  // 会话命令返回带金丝雀的那条；其余命令（用量三条读 + 设置 + 采集）原样走共享 mock
  invokeMock.mockImplementation((cmd, args) =>
    cmd === "get_all_sessions"
      ? Promise.resolve({ sessions: [canarySession()], totalCount: 1, waitingCount: 0 })
      : baseImpl?.(cmd, args)
  );
});

afterEach(() => {
  if (baseImpl) invokeMock.mockImplementation(baseImpl);
});

describe("usage-privacy（计划② Task 11：隐私白名单渲染门禁）", () => {
  it("1. 记录页渲染的是结构化摘要：会话正文金丝雀不出现在 DOM 的任何位置", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    );
    render(<UsageDashboardBody active />, { wrapper });
    fireEvent.click(screen.getByTestId("usage-tab-records"));

    // 先在**有内容**的 DOM 上断言：记录页确实把夹具卡片渲染出来了（不是在空 DOM 上"通过"的）
    expect(await screen.findByTestId("usage-record-card-claude")).toBeTruthy();

    // 用量面结构上不读会话数据：会话命令一次都没发（金丝雀只备在数据源上）
    const sessionsCalls = invokeMock.mock.calls.filter(([c]) => c === "get_all_sessions");
    expect(sessionsCalls).toHaveLength(0);

    // 正文不得出现在任何位置（`document.body` 覆盖整个渲染树与浮层）
    expect(document.body.textContent).not.toContain(CANARY);
    // 顺带咬住形态：正文字段名本身也不该被印成可见文本
    expect(document.body.textContent).not.toContain("lastMessage");
  });
});
