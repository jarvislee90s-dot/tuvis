// tests/hooks/useRemoteEvents.test.tsx — M5 A6 更新：/pair/pin 认证制落地后
// remote-pair-request 配对请求事件已随审批制下线（A6 删除监听与 i18n 文案），本测试
// 改为锁定两件事：① 监听注册表里不再出现 remote-pair-request（防回潮）；
// ② 保留事件（隧道地址就绪）仍解析到真实文案，不得出现原始键路径（键错位回归哨兵，
// check:i18n 只验中英成对、不验代码↔键一致性）。
import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const { handlers, listenMock, sendNotificationMock, toastSuccessMock, toastErrorMock } = vi.hoisted(() => {
  const handlers = new Map<string, (e: { payload: unknown }) => void | Promise<void>>();
  return {
    handlers,
    listenMock: vi.fn(
      async (event: string, handler: (e: { payload: unknown }) => void | Promise<void>) => {
        handlers.set(event, handler);
        return () => {
          handlers.delete(event);
        };
      }
    ),
    sendNotificationMock: vi.fn(),
    toastSuccessMock: vi.fn(),
    toastErrorMock: vi.fn(),
  };
});

vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock, emit: vi.fn(async () => {}) }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  sendNotification: sendNotificationMock,
  isPermissionGranted: vi.fn(async () => true),
  requestPermission: vi.fn(async () => "granted"),
}));
vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), { info: vi.fn(), success: toastSuccessMock, error: toastErrorMock }),
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言（jsdom navigator.language=en）
import i18n from "@/i18n";
import { useRemoteEvents } from "@/hooks/useRemoteEvents";

void i18n;

describe("useRemoteEvents 全局远程事件（M5 A6）", () => {
  it("remote-pair-request 监听已随审批制下线，不得再注册", async () => {
    renderHook(() => useRemoteEvents());
    // 等 effect 内 async IIFE 把保留事件全部注册完
    await waitFor(() => expect(handlers.has("remote-tunnel-address")).toBe(true));
    expect(handlers.has("remote-pair-request")).toBe(false);
  });

  it("remote-tunnel-address：toast 解析 tunnelReadyToast 真实文案（键错位回归哨兵）", async () => {
    renderHook(() => useRemoteEvents());
    await waitFor(() => expect(handlers.has("remote-tunnel-address")).toBe(true));
    const handler = handlers.get("remote-tunnel-address");
    if (!handler) throw new Error("remote-tunnel-address handler not registered");
    await handler({ payload: { url: "https://mam.example.com/m" } });
    await waitFor(() => expect(toastSuccessMock).toHaveBeenCalled());
    const msg = String(toastSuccessMock.mock.calls[0][0]);
    expect(msg).not.toContain("settings.remote"); // 缺键即泄漏键路径
    expect(msg).toContain("https://mam.example.com/m");
  });

  it("remote-toggle-failed：错误载荷经 formatInvokeError 解析——码态载荷出英文语义句，不再裸码+中文（复审 N4）", async () => {
    renderHook(() => useRemoteEvents());
    await waitFor(() => expect(handlers.has("remote-toggle-failed")).toBe(true));
    const handler = handlers.get("remote-toggle-failed");
    if (!handler) throw new Error("remote-toggle-failed handler not registered");
    await handler({
      payload: { error: "REMOTE_PUBLIC_ACK_REQUIRED: 对外绑定需先确认已配置 TLS 反向代理（remote_confirm_public）" },
    });
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    const msg = String(toastErrorMock.mock.calls[0][0]);
    // 已知码 → en 语义句（不含码前缀、不含中文）
    expect(msg).not.toMatch(/^[A-Z][A-Z0-9_]+:/);
    expect(msg).not.toMatch(/[一-鿿]/);
    expect(msg.toLowerCase()).toContain("tls");
  });

  it("remote-tunnel-error：守护放弃类码态载荷同样解析（双通道之 toast 侧）", async () => {
    renderHook(() => useRemoteEvents());
    await waitFor(() => expect(handlers.has("remote-tunnel-error")).toBe(true));
    const handler = handlers.get("remote-tunnel-error");
    if (!handler) throw new Error("remote-tunnel-error handler not registered");
    await handler({
      payload: { error: "REMOTE_TUNNEL_GAVE_UP: cloudflared 连续失败 3 次，已停止守护" },
    });
    await waitFor(() => expect(toastErrorMock).toHaveBeenCalled());
    const msg = String(toastErrorMock.mock.calls[0][0]);
    expect(msg).not.toMatch(/^[A-Z][A-Z0-9_]+:/);
    expect(msg).not.toMatch(/[一-鿿]/);
  });
});
