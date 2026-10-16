import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import CodexQueuePanel from "@/mobile/CodexQueuePanel";
import type { CodexActionView, CodexQueueView } from "@/mobile/codexIpcApi";

// Task 9：CodexQueuePanel（本设备待发队列）行为锁：
// - 显示本设备待发与实际 FIFO 位置；撤回按钮；
// - 撤回成功条目消失（操作后重拉）；409 保留并刷新；cancelled 无重试；
// - compat_outstanding 说明条；**不提供插队**。

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function action(over: Partial<CodexActionView> = {}): CodexActionView {
  return {
    actionId: "a1",
    state: "queued",
    position: 1,
    receipt: null,
    ...over,
  };
}

function queueView(
  actions: CodexActionView[],
  blockedReasonCode: string | null = null
): CodexQueueView {
  return { actions, blockedReasonCode };
}

interface Routes {
  queue?: CodexQueueView[];
  /** GET /codex/queue 的逐次响应（操作后重拉断言用） */
  retractStatus?: number;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  routes = {};
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/codex/queue/retract")) {
      if (routes.retractStatus) {
        return new Response(JSON.stringify({ error: "action_conflict" }), {
          status: routes.retractStatus,
        });
      }
      const body = JSON.parse(String(init?.body)) as { actionId: string };
      return new Response(
        JSON.stringify({
          actionId: body.actionId,
          state: "cancelled",
          position: null,
          receipt: null,
        }),
        { status: 200 }
      );
    }
    if (url.includes("/codex/queue")) {
      const next = routes.queue?.shift();
      return new Response(JSON.stringify(next ?? queueView([])), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

function queueGetCalls(): number {
  return fetchMock.mock.calls.filter(
    ([u]) => String(u).includes("/codex/queue") && !String(u).includes("/retract")
  ).length;
}

describe("CodexQueuePanel（Task 9）", () => {
  it("renders_fifo_positions：挂载拉取显示待发与实际 FIFO 位置；无插队按钮", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 }), action({ actionId: "a2", position: 2 })]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    const row1 = await screen.findByTestId("codex-queue-row-a1");
    expect(row1.textContent).toContain("第1位");
    expect(screen.getByTestId("codex-queue-row-a2").textContent).toContain("第2位");
    // 撤回按钮在 queued 行上
    expect(screen.getByTestId("codex-queue-retract-a1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-retract-a2")).toBeTruthy();
    // **不提供插队**：立即发送/插队语义的按钮一律不存在
    expect(screen.queryByText("立即发送")).toBeNull();
    expect(screen.queryByText("插队")).toBeNull();
    expect(screen.queryByTestId("codex-queue-jump")).toBeNull();
  });

  it("retract_success_removes_entry：撤回成功（cancelled）→ 操作后重拉，条目随权威列表消失", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })]),
      queueView([]), // 操作后重拉：已撤条目不在队
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-row-a1");
    fireEvent.click(screen.getByTestId("codex-queue-retract-a1"));
    await waitFor(() => expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull());
    // 撤回体只有 actionId（无 sessionId/正文——服务端账本按 actionId 定位）
    const call = fetchMock.mock.calls.find(([u]) => String(u).includes("/codex/queue/retract"));
    expect(JSON.parse(String((call![1] as RequestInit).body))).toEqual({ actionId: "a1" });
    // 操作后重拉（挂载 1 次 + 操作后 1 次）
    expect(queueGetCalls()).toBe(2);
    // 全空且无 blocked → 面板自隐
    await waitFor(() => expect(screen.queryByTestId("codex-queue-panel")).toBeNull());
  });

  it("retract_409_keeps_and_refreshes：已派发 409 → 条目保留 + 权威列表刷新 + 说明", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })]),
      queueView([action({ actionId: "a1", state: "reserved", position: null })]), // 重拉：已派发
    ];
    routes.retractStatus = 409;
    render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-row-a1");
    fireEvent.click(screen.getByTestId("codex-queue-retract-a1"));
    // 409 → 说明出现（不谎报撤回成功）
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("已派发");
    // 操作后重拉：条目以权威状态保留（reserved 无撤回钮）
    expect(queueGetCalls()).toBe(2);
    await waitFor(() => expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy());
    expect(screen.queryByTestId("codex-queue-retract-a1")).toBeNull();
  });

  it("cancelled_no_retry：cancelled 条目只呈现终态，无撤回/重试按钮", async () => {
    installFetch();
    routes.queue = [
      queueView([
        action({
          actionId: "a3",
          state: "cancelled",
          position: null,
          receipt: {
            actionId: "a3",
            delivery: "not_sent",
            route: "none",
            turnId: null,
            envelopeId: null,
            reasonCode: null,
          },
        }),
      ]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-row-a3");
    expect(screen.getByTestId("codex-queue-row-a3").textContent).toContain("已取消");
    expect(screen.queryByTestId("codex-queue-retract-a3")).toBeNull();
    expect(screen.queryByRole("button", { name: "重试" })).toBeNull();
  });

  it("compat_outstanding_banner：blockedReasonCode 非空 → 暂停派发说明条（compat_outstanding）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "compat_outstanding")];
    render(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.textContent).toContain("兼容");
    expect(banner.textContent).toContain("暂停");
  });

  it("empty_hidden：无条目且无 blocked → 面板不渲染（自隐惯例）", async () => {
    installFetch();
    routes.queue = [queueView([])];
    render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
  });
});
