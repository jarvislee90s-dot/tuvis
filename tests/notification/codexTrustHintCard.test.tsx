// tests/notification/codexTrustHintCard.test.tsx — 2A：codex 信任门常驻操作卡。
// 经 NotificationPage 渲染、驱动 notification:new 事件进入 action 分支（组件未导出，
// 测真实挂载路径而非直拆内部组件）。覆盖：复制命令 → 剪贴板写入 + ack("copy") +
// 关窗；不再提醒 → ack("never") + 关窗；X → 仅藏窗且【不】触发 ack（「弹卡 ≠ 销账」
// 语义在前端唯一的守护断言）；复制失败 / ack 失败 → 卡面就地提示、不关窗不销账、
// busy 复位可重试；actionCommand 缺省回退 "/hooks"；操作卡不走会话卡生命周期
// （不 setSize 改高，建房高度由 Rust 侧按操作卡高度管理）。
// mock 模式沿用 SignalHealthSection.test.tsx（vi.hoisted + vi.mock 三件套：
// invoke / listen / getCurrentWindow），i18n 显式引入按默认英文断言。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Rust 侧 NotificationPayload（camelCase 直达）前端同构样例类型
type ActionCardPayload = {
  agentType: string;
  agentLabel: string;
  projectName: string;
  statusColor: string;
  status: string;
  lastMessage: string;
  title: string;
  pid: number;
  sessionId: string;
  action?: string;
  actionCommand?: string;
};

const { invokeMock, listenMock, windowMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
  // getCurrentWindow 每次调用须返回同实例（label 是 emit_to 定向监听目标）
  windowMock: {
    label: "notification-0",
    show: vi.fn(async () => {}),
    hide: vi.fn(async () => {}),
    scaleFactor: vi.fn(async () => 1),
    outerPosition: vi.fn(async () => ({ x: 0, y: 0 })),
    setSize: vi.fn(async () => {}),
    setPosition: vi.fn(async () => {}),
  },
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => windowMock }));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import NotificationPage from "@/pages/notification";

void i18n;

// jsdom 无剪贴板内核：stub writeText（复制按钮断言入口）
const clipboardWrite = vi.fn(async () => {});

// listen 捕获的 notification:new 处理器（驱动操作卡渲染的测试入口）
const handlerRef: { current: ((e: { payload: ActionCardPayload }) => void) | null } = {
  current: null,
};

// ack 命令调用次数（销账唯一入口，X 路径负向断言的关键计数）
const ackCalls = () => invokeMock.mock.calls.filter((c) => c[0] === "codex_trust_notice_ack");

const actionPayload = (overrides: Partial<ActionCardPayload> = {}): ActionCardPayload => ({
  agentType: "codex",
  agentLabel: "Codex",
  projectName: "demo",
  statusColor: "#f59e0b",
  status: "running",
  lastMessage: "",
  title: "",
  pid: 0,
  sessionId: "",
  action: "codexTrustHint",
  actionCommand: "/hooks",
  ...overrides,
});

// 渲染页面并注入一张操作卡（卡面标题出现 = action 分支已进入）。事件分发包 act：
// handler 直调 setState，不包会产生 not wrapped in act 噪音（fireEvent 自带 act）
const showActionCard = async (overrides: Partial<ActionCardPayload> = {}) => {
  render(<NotificationPage />);
  await waitFor(() => expect(handlerRef.current).toBeTruthy());
  await act(async () => {
    handlerRef.current?.({ payload: actionPayload(overrides) });
  });
  expect(await screen.findByText("Codex hooks need trust")).toBeTruthy();
};

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(null);
  listenMock.mockReset();
  listenMock.mockImplementation(async (_event: string, handler: (e: unknown) => void) => {
    handlerRef.current = handler as typeof handlerRef.current;
    return () => {};
  });
  windowMock.show.mockClear();
  windowMock.hide.mockClear();
  windowMock.setSize.mockClear();
  windowMock.setPosition.mockClear();
  clipboardWrite.mockClear();
  Object.defineProperty(window.navigator, "clipboard", {
    value: { writeText: clipboardWrite },
    configurable: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("codex 信任门操作卡（2A）", () => {
  it("复制命令：写剪贴板 → ack(copy) → 关窗（销账只在按钮路径上）", async () => {
    await showActionCard();
    fireEvent.click(screen.getByRole("button", { name: "Copy command" }));
    await waitFor(() => expect(clipboardWrite).toHaveBeenCalledWith("/hooks"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("codex_trust_notice_ack", { action: "copy" })
    );
    await waitFor(() => expect(windowMock.hide).toHaveBeenCalled());
  });

  it("不再提醒：直接 ack(never) + 关窗，不碰剪贴板", async () => {
    await showActionCard();
    fireEvent.click(screen.getByRole("button", { name: /don't remind again/i }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("codex_trust_notice_ack", { action: "never" })
    );
    await waitFor(() => expect(windowMock.hide).toHaveBeenCalled());
    expect(clipboardWrite).not.toHaveBeenCalled();
  });

  it("X 仅藏窗：hide 且不 ack、不碰剪贴板、卡面保留（弹卡 ≠ 销账）", async () => {
    await showActionCard();
    fireEvent.click(screen.getByRole("button", { name: /reminded again on next launch/i }));
    await waitFor(() => expect(windowMock.hide).toHaveBeenCalled());
    expect(ackCalls()).toHaveLength(0);
    expect(clipboardWrite).not.toHaveBeenCalled();
    // 卡面仍在 DOM：X 不卸载组件，pending 保留语义在后端 KV（下次启动再弹）
    expect(screen.getByText("Codex hooks need trust")).toBeTruthy();
  });

  it("复制失败：卡面就地提示，不 ack、不关窗（命令文本醒目在手可手抄兜底）", async () => {
    clipboardWrite.mockRejectedValueOnce(new Error("no clipboard in focusless window"));
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await showActionCard();
      fireEvent.click(screen.getByRole("button", { name: "Copy command" }));
      expect(
        await screen.findByText("Copy failed — please type the command above manually")
      ).toBeTruthy();
      expect(ackCalls()).toHaveLength(0);
      expect(windowMock.hide).not.toHaveBeenCalled();
    } finally {
      errSpy.mockRestore();
    }
  });

  it("ack 失败：卡面就地提示且不关窗，busy 复位后可重试（销账不因 IPC 抖动静默丢失）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "codex_trust_notice_ack") throw new Error("ack boom");
      return null;
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await showActionCard();
      const neverBtn = screen.getByRole("button", { name: /don't remind again/i });
      fireEvent.click(neverBtn);
      expect(await screen.findByText("Action failed, please retry")).toBeTruthy();
      expect(windowMock.hide).not.toHaveBeenCalled();
      await waitFor(() => expect(neverBtn).not.toBeDisabled());
    } finally {
      errSpy.mockRestore();
    }
  });

  it("actionCommand 缺省回退 /hooks，卡面等宽展示命令文本", async () => {
    await showActionCard({ actionCommand: undefined });
    expect(screen.getByText("/hooks")).toBeTruthy();
  });

  it("操作卡不走会话卡生命周期：不改高、仅 show 一次（常驻直到用户处置）", async () => {
    await showActionCard();
    expect(windowMock.setSize).not.toHaveBeenCalled();
    await waitFor(() => expect(windowMock.show).toHaveBeenCalledTimes(1));
  });
});
