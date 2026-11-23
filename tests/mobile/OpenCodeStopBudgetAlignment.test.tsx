// H5（task HE）：停止统一端到端预算的手机侧对齐断言。
//
// 服务端契约（task-HE 后端）：统一预算覆盖服务端动作链接收 → 正式回执全链
// （`ACTION_E2E_BUDGET_MS`，stop 派发前超时 ⇒ 确定未发送 `OC_DISPATCH_BUDGET_EXHAUSTED`）。
// 手机侧对齐（用户裁决）：
// 1. 停止 POST 的客户端等待 = 服务端预算 + 余量（保证「预算内必收到正式回执」，
//    客户端超时只在服务端预算已尽后出现，语义仍是「结果未知，不自动重发」）；
// 2. 预算拒绝码按「确定零写出」分流（清锚、可安全重试），文案不暗示失败终态；
// 3. 404/核对失败只表示「当前没读到」（既有语义回归，OpenCodeNativePanel
//    verify_404_keeps_send_unknown_and_locked 在案）。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  isDefiniteRefusal,
  nativeErrorKey,
  OPENCODE_ACTION_POST_TIMEOUT_MS,
  OPENCODE_STOP_POST_TIMEOUT_MS,
  OPENCODE_STOP_SERVER_BUDGET_MS,
} from "@/mobile/opencodeNativeApi";
import type { PublicNativeView } from "@/mobile/opencodeNativeApi";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import i18n from "@/i18n";
import { pinZhNavigator } from "./pinZhNavigator";
import { MockEventSource } from "./eventSourceMock";
import { resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";

let unpin: () => void;
beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());

describe("停止预算常量对齐（手机等待 ⇄ 服务端预算）", () => {
  it("停止等待 = 服务端预算 + 余量，且余量不吞掉通用超时", () => {
    // 镜像常量：与服务端 budget.rs `ACTION_E2E_BUDGET_MS` 同值（45s）。
    expect(OPENCODE_STOP_SERVER_BUDGET_MS).toBe(45_000);
    // 停止等待 = 预算 + 余量：服务端预算内必回正式回执，客户端超时语义仍是未知态。
    expect(OPENCODE_STOP_POST_TIMEOUT_MS).toBe(OPENCODE_STOP_SERVER_BUDGET_MS + 5_000);
    // 停止等待必须比通用写 POST 超时宽（慢链路径——单飞等待/DB 锁窗——不再被
    // 通用 10s 提前掐成未知态；对齐裁决：预算内可见明确状态）。
    expect(OPENCODE_STOP_POST_TIMEOUT_MS).toBeGreaterThan(OPENCODE_ACTION_POST_TIMEOUT_MS);
  });

  it("预算拒绝码 = 确定零写出（清锚可重试），且有专属本地化文案（非机器串）", () => {
    expect(isDefiniteRefusal("OC_DISPATCH_BUDGET_EXHAUSTED")).toBe(true);
    const key = nativeErrorKey("OC_DISPATCH_BUDGET_EXHAUSTED");
    expect(key).not.toBeNull();
    const copy = i18n.t(key as string);
    expect(copy).not.toBe(key);
    // 文案不暗示「停止失败」终态——说的是确定未发出、可安全重试。
    expect(copy).not.toMatch(/停止失败|Stop failed/);
  });

  it("停止 POST 挂起：通用 10s 超时不再触发；停止等待处才落可辨未知态（零重发）", async () => {
    const { actionPosts } = renderStopPanel();
    const stopButton = await findEnabledStop();
    vi.useFakeTimers();
    fireEvent.click(stopButton);
    expect(actionPosts).toHaveLength(1);
    // 通用写 POST 超时（10s）已过：停止等待更宽，不得在此被掐（预算内等正式回执）。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(OPENCODE_ACTION_POST_TIMEOUT_MS);
    });
    expect(actionPosts).toHaveLength(1);
    expect(screen.queryByTestId("native-stop-result")).toBeNull();
    // 停止等待处：客户端超时落可辨未知态（不自动重发、不二次 POST）。
    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        OPENCODE_STOP_POST_TIMEOUT_MS - OPENCODE_ACTION_POST_TIMEOUT_MS
      );
    });
    const result = screen.getByTestId("native-stop-result");
    expect(result.textContent).toContain(i18n.t("mobile.openCodeNative.stop.unknown"));
    expect(actionPosts).toHaveLength(1);
  });
});

describe("停止入口仅对当前 running 视图开放", () => {
  it.each(["ready", "idle:succeeded"])("%s 视图不显示停止入口", async (runtimeState) => {
    renderStopPanel(runtimeState);
    await screen.findByTestId("native-messages-toggle");

    expect(screen.queryByTestId("native-stop")).not.toBeInTheDocument();
  });

  it("刷新失败后保留旧 running 视图，但收走停止入口", async () => {
    const { failViewRefresh } = renderStopPanel();
    await findEnabledStop();

    failViewRefresh();
    await screen.findByTestId("native-load-error");

    expect(screen.getByTestId("native-messages-toggle")).toBeInTheDocument();
    expect(screen.queryByTestId("native-stop")).not.toBeInTheDocument();
  });

  it("running 时回执在途的快速双击只发送一次停止 POST", async () => {
    const { actionPosts } = renderStopPanel();
    const stopButton = await findEnabledStop();

    // 在同一 React 提交前触发两次点击，不依赖按钮先变为 disabled 才去重。
    act(() => {
      fireEvent.click(stopButton);
      fireEvent.click(stopButton);
    });

    expect(actionPosts).toEqual([expect.objectContaining({ operation: { kind: "stop_session" } })]);
  });
});

function renderStopPanel(runtimeState = "running") {
  const actionPosts: unknown[] = [];
  let viewStatus = 200;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/opencode/actions") && init?.method === "POST") {
        actionPosts.push(JSON.parse(String(init.body)));
        return new Promise<Response>((_resolve, reject) => {
          init.signal?.addEventListener("abort", () =>
            reject(new DOMException("aborted", "AbortError"))
          );
        });
      }
      if (url.includes("/opencode/view")) {
        return new Response(viewStatus === 200 ? JSON.stringify(nativeView(runtimeState)) : "{}", {
          status: viewStatus,
        });
      }
      if (url.includes("/m/api/v1/channel")) {
        return new Response(
          JSON.stringify({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 }),
          { status: 200 }
        );
      }
      return new Response("{}", { status: 200 });
    })
  );
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  render(<OpenCodeNativePanel sessionId="sess-1" />);
  return {
    actionPosts,
    failViewRefresh() {
      viewStatus = 503;
      act(() => MockEventSource.latest().emit("revision", "2"));
    },
  };
}

function findEnabledStop() {
  return waitFor(() => {
    const button = screen.getByTestId("native-stop");
    expect(button).toBeEnabled();
    return button;
  });
}

function nativeView(runtimeState: string): PublicNativeView {
  return {
    sessionId: "sess-1",
    bindingToken: "bind_1",
    routeVersion: 1,
    revision: 1,
    complete: true,
    capabilities: {
      send: { allowed: true, evidence: "contract: prompt", reasonCode: null },
      retract: { allowed: true, evidence: "contract: inbox delete", reasonCode: null },
      stop_session: { allowed: true, evidence: "contract: interrupt", reasonCode: null },
      set_agent: { allowed: true, evidence: "contract: agent", reasonCode: null },
    },
    messages: [],
    pending: [],
    inbox: [],
    runtimeState,
    files: {},
    route: "opencode_native",
    routeReasonCode: null,
  };
}

beforeEach(() => {
  resetActionAnchorMemoryForTests();
  sessionStorage.clear();
  MockEventSource.reset();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
