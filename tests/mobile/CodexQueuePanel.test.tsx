import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import CodexQueuePanel from "@/mobile/CodexQueuePanel";
import type { CodexActionView, CodexQueueView } from "@/mobile/codexIpcApi";

// Task 9：CodexQueuePanel（本设备待发队列）行为锁：
// - 显示本设备待发与实际 FIFO 位置；撤回按钮；
// - 撤回成功条目消失（操作后重拉）；409 保留并刷新；cancelled 无重试；
// - compat_outstanding 说明条；**不提供插队**；
// - 人工解除无出口屏障（2026-10-19 复审 Important）：按钮仅无自动恢复路径的阻塞
//   态可见；二次确认前不发请求；确认后调用 + 成功刷新；失败显示不静默。

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
  blockedReasonCode: string | null = null,
  blockedActionIds: string[] = [],
  /** 2026-10-22 复审 P2-2：canInterrupt 能力位（缺省 true = 已活体验证的 build） */
  canInterrupt: boolean = true
): CodexQueueView {
  return { actions, blockedReasonCode, blockedActionIds, canInterrupt };
}

interface Routes {
  queue?: CodexQueueView[];
  /** GET /codex/queue 的逐次响应（操作后重拉断言用） */
  retractStatus?: number;
  /** POST /codex/queue/force-continue 的失败状态码（缺省 200；作用于全部 id） */
  forceContinueStatus?: number;
  /** POST /codex/queue/force-continue 按 actionId 定向的失败体（一败全停/可读化用例） */
  forceContinueFailures?: Record<string, { status: number; body: Record<string, unknown> }>;
  /** R11：GET 挂起不响应（手动放行——乱序防护用例的在途响应） */
  queueHang?: boolean;
  /** POST /codex/turn/interrupt 的失败状态码（缺省 200） */
  interruptStatus?: number;
  /** POST /codex/turn/interrupt 的错误体覆盖（缺省 {error:"capability_unavailable"}） */
  interruptErrorBody?: Record<string, unknown>;
  /** POST /codex/turn/interrupt 的回执体覆盖（null = 桌面无可打断回合） */
  interruptBody?: Record<string, unknown>;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
/** queueHang 挂起请求的放行器（测试中手动 resolve 模拟旧会话迟到响应） */
let releaseQueue: ((v: CodexQueueView) => void) | null = null;

beforeEach(() => {
  routes = {};
  releaseQueue = null;
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/codex/queue/force-continue")) {
      const body = JSON.parse(String(init?.body)) as { actionId: string };
      const failure = routes.forceContinueFailures?.[body.actionId];
      if (failure) {
        return new Response(JSON.stringify(failure.body), { status: failure.status });
      }
      if (routes.forceContinueStatus) {
        return new Response(JSON.stringify({ error: "internal" }), {
          status: routes.forceContinueStatus,
        });
      }
      return new Response(
        JSON.stringify({
          actionId: body.actionId,
          state: "unknown",
          position: null,
          receipt: null,
        }),
        { status: 200 }
      );
    }
    if (url.includes("/codex/turn/interrupt")) {
      if (routes.interruptStatus) {
        return new Response(
          JSON.stringify(routes.interruptErrorBody ?? { error: "capability_unavailable" }),
          { status: routes.interruptStatus }
        );
      }
      return new Response(
        JSON.stringify(
          routes.interruptBody ?? {
            ok: true,
            interruptedTurnId: "t-live",
            expectedTurnId: "t-live",
          }
        ),
        { status: 200 }
      );
    }
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
      if (routes.queueHang) {
        // 只挂起**第一发**（会话切换后新会话的拉取照常返回——迟到的只有旧会话那份）
        routes.queueHang = false;
        return new Promise<Response>((resolve) => {
          releaseQueue = (v: CodexQueueView) =>
            resolve(new Response(JSON.stringify(v), { status: 200 }));
        });
      }
      const next = routes.queue?.shift();
      return new Response(JSON.stringify(next ?? queueView([])), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

function queueGetCalls(): number {
  return fetchMock.mock.calls.filter(
    ([u]) =>
      String(u).includes("/codex/queue") &&
      !String(u).includes("/retract") &&
      !String(u).includes("/force-continue")
  ).length;
}

function forceContinueBodies(): string[] {
  return fetchMock.mock.calls
    .filter(([u]) => String(u).includes("/codex/queue/force-continue"))
    .map(([, init]) => String((init as RequestInit)?.body));
}

function interruptBodies(): string[] {
  return fetchMock.mock.calls
    .filter(([u]) => String(u).includes("/codex/turn/interrupt"))
    .map(([, init]) => String((init as RequestInit)?.body));
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

  it("native_delivery_unknown_banner：原生 Unknown 屏障 → 如实说无自动恢复（不得承诺自动继续）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a3" })], "native_delivery_unknown")];
    render(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.getAttribute("data-reason")).toBe("native_delivery_unknown");
    expect(banner.textContent).toContain("未知");
    expect(banner.textContent).toContain("无自动恢复");
    expect(banner.textContent).not.toContain("自动继续");
  });

  it("native_turn_pending_banner：原生 Turn 屏障 → 分文案（不得说成兼容模式/回 Desktop 手工解除）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a2" })], "native_turn_pending")];
    render(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.getAttribute("data-reason")).toBe("native_turn_pending");
    expect(banner.textContent).toContain("原生回合");
    expect(banner.textContent).toContain("自动继续");
    expect(banner.textContent).not.toContain("兼容");
  });

  it("empty_hidden：无条目且无 blocked → 面板不渲染（自隐惯例）", async () => {
    installFetch();
    routes.queue = [queueView([])];
    render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
  });

  // ==== R11：外部触发联动 + 乱序防护 ====
  it("external_refresh_signal_refetches：外部触发（投影跃迁/发送回执）→ 去抖重拉，新入队条目不离页可见", async () => {
    installFetch();
    routes.queue = [queueView([])];
    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    // 发送入队回执/投影跃迁 → 父级 bump refreshSignal → 面板重拉到新条目
    routes.queue = [queueView([action({ actionId: "a9", position: 1 })])];
    rerender(<CodexQueuePanel sessionId="s1" refreshSignal={1} />);
    const row = await screen.findByTestId("codex-queue-row-a9", undefined, { timeout: 3000 });
    expect(row.textContent).toContain("第1位");
  });

  it("stale_session_response_not_applied：旧会话在途响应迟到 → 不回灌新会话", async () => {
    installFetch();
    routes.queueHang = true; // s1 的拉取挂起（响应迟到）
    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(releaseQueue).toBeTruthy();
    // 切换会话：s2 立即拉取（空队列）
    rerender(<CodexQueuePanel sessionId="s2" />);
    await act(async () => {});
    // s1 的迟到响应到达（含 a1）——旧会话响应不得回灌 s2
    await act(async () => {
      releaseQueue?.(queueView([action({ actionId: "a1", position: 1 })]));
      await Promise.resolve();
    });
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
  });

  it("rapid_refresh_signals_coalesce：同会话密集触发去抖合并为一次重拉", async () => {
    installFetch();
    routes.queue = [queueView([]), queueView([action({ actionId: "a1", position: 1 })])];
    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(queueGetCalls()).toBe(1); // 挂载拉取
    // 密集两次触发（发送回执 + 投影跃迁几乎同时，同步连发）
    await act(async () => {
      rerender(<CodexQueuePanel sessionId="s1" refreshSignal={1} />);
      rerender(<CodexQueuePanel sessionId="s1" refreshSignal={2} />);
    });
    // 去抖窗口内不发请求：两次触发尚未合并落地
    expect(queueGetCalls()).toBe(1);
    // 窗口过后恰好一次合并重拉（去抖尾部定时器）
    const row = await screen.findByTestId("codex-queue-row-a1", undefined, { timeout: 3000 });
    expect(row.textContent).toContain("第1位");
    expect(queueGetCalls()).toBe(2);
  });

  it("streaming_revisions_do_not_starve_enqueue_refresh：流式 revision 每 100ms 推进不断重置去抖——入队刷新仍在 maxWait 上限内落地（C6）", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.queue = [queueView([])];
    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);
    const advance = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });
    };
    await advance(10);
    expect(queueGetCalls()).toBe(1); // 挂载拉取
    // 入队完成 + 此后流式输出每 100ms 推进一次 revision（refreshSignal 持续递增，
    // 每次都重置 250ms 去抖——旧实现在此被无限饿死）
    routes.queue = [queueView([action({ actionId: "a1", position: 1 })])];
    for (let i = 1; i <= 25; i += 1) {
      rerender(<CodexQueuePanel sessionId="s1" refreshSignal={i} />);
      await advance(100);
    }
    // 2.5s > maxWait(2s)：刷新必然已落地（条目可见可撤），不是等流式停歇
    expect(queueGetCalls()).toBeGreaterThanOrEqual(2);
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
    vi.useRealTimers();
  });

  // ==== 人工解除无出口屏障（2026-10-19 复审 Important）====

  it("force_continue_button_only_in_manual_recovery_states：按钮仅两类无自动恢复路径的阻塞态可见（native_turn_pending 不提供；无靶向清单也不显示）", async () => {
    installFetch();
    // native_delivery_unknown：按钮可见
    routes.queue = [queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"])];
    const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-force-continue")).toBeTruthy();
    unmount();
    // compat_outstanding：按钮同样可见
    routes.queue = [queueView([action({ actionId: "a2" })], "compat_outstanding", ["c-1"])];
    const r2 = render(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-force-continue")).toBeTruthy();
    r2.unmount();
    // native_turn_pending（等待 Turn 完成**自动**继续）：不提供人工解除
    routes.queue = [queueView([action({ actionId: "a3" })], "native_turn_pending", ["t-1"])];
    const r3 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    r3.unmount();
    // 无靶向清单（旧后端/无在册屏障）：不显示
    routes.queue = [queueView([action({ actionId: "a4" })], "native_delivery_unknown")];
    const r4 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    r4.unmount();
  });

  it("force_continue_requires_confirmation：点击按钮先出确认条（含风险文案），确认前不发任何请求；取消不打请求", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"])];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    // 确认条出现，风险文案如实（重复内容风险）
    const strip = screen.getByTestId("codex-queue-force-confirm");
    expect(strip.textContent).toContain("Desktop");
    expect(strip.textContent).toContain("重复内容");
    // 确认前零请求
    expect(forceContinueBodies()).toEqual([]);
    // 取消：收起确认条，仍零请求
    fireEvent.click(screen.getByTestId("codex-queue-force-no"));
    expect(screen.queryByTestId("codex-queue-force-confirm")).toBeNull();
    expect(forceContinueBodies()).toEqual([]);
  });

  it("force_continue_success_releases_and_refreshes：确认后调用端点（携带 actionId）→ 队列视图刷新，阻塞解除按钮消失", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"]),
      queueView([]), // 解除后重拉：阻塞清空
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    // 请求体只带 actionId
    await waitFor(() => expect(forceContinueBodies()).toEqual(['{"actionId":"u-1"}']));
    // 操作后重拉：banner 消失、按钮消失
    await waitFor(() => expect(screen.queryByTestId("codex-queue-blocked")).toBeNull());
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    expect(queueGetCalls()).toBe(2);
  });

  it("force_continue_failure_shows_error：失败显示错误不静默（仍重拉权威列表）", async () => {
    installFetch();
    routes.forceContinueStatus = 500;
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"]),
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    // 失败 → note 出现（不静默）
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("继续队列失败");
    // 失败后仍重拉权威列表
    expect(queueGetCalls()).toBe(2);
  });

  it("force_continue_no_barrier_settles_and_continues：第一个 id 409 no_barrier（视图与确认之间屏障被自动路径清除）→ 该 id 收尾继续，第二个 id 仍被请求，零错误 note", async () => {
    installFetch();
    routes.forceContinueFailures = {
      "u-1": { status: 409, body: { error: "no_barrier" } },
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2"]),
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    // 一败（no_barrier）不停：两个 id 都被逐个请求（面板保持可见，note 断言才有效）
    await waitFor(() =>
      expect(forceContinueBodies()).toEqual(['{"actionId":"u-1"}', '{"actionId":"u-2"}'])
    );
    // no_barrier = 确定性「无需解除」，不是错误：不出错误 note
    expect(screen.queryByTestId("codex-queue-note")).toBeNull();
    // 操作后重拉（挂载 1 次 + 循环后 1 次）
    expect(queueGetCalls()).toBe(2);
  });

  it("force_continue_barrier_in_flight_settles_and_continues：第一个 id 409 barrier_in_flight（屏障在册但派发在飞有自动路径）→ 该 id 收尾继续，第二个 id 仍被请求，零错误 note", async () => {
    installFetch();
    routes.forceContinueFailures = {
      "u-1": {
        status: 409,
        body: {
          error: "barrier_in_flight",
          reason: "屏障在册但该消息派发在飞（有自动落账路径），不在人工解除范围",
        },
      },
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2"]),
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    // 一败（barrier_in_flight）不停：两个 id 都被逐个请求
    await waitFor(() =>
      expect(forceContinueBodies()).toEqual(['{"actionId":"u-1"}', '{"actionId":"u-2"}'])
    );
    // barrier_in_flight = 确定性「无需人工解除」（自动路径负责），不是错误：不出错误 note
    expect(screen.queryByTestId("codex-queue-note")).toBeNull();
    // 操作后重拉（挂载 1 次 + 循环后 1 次）
    expect(queueGetCalls()).toBe(2);
  });

  it("force_continue_real_failure_interrupts：第二个 id 真失败（500 internal）→ 中断剩余 + note 带机器码可读化（不裸示「路径 + 状态码」）", async () => {
    installFetch();
    routes.forceContinueFailures = {
      "u-2": { status: 500, body: { error: "internal" } },
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2", "u-3"]),
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "u-2", "u-3"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    // note 走 reason → 机器码 → message 回退链：500 无 reason，退到机器码 internal，
    // 不再把「codex/queue/force-continue 500」裸示人
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("继续队列失败");
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("internal");
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain(
      "codex/queue/force-continue"
    );
    // 真失败中断：u-3 不再请求（重拉后由用户再试）
    expect(forceContinueBodies()).toEqual(['{"actionId":"u-1"}', '{"actionId":"u-2"}']);
    expect(queueGetCalls()).toBe(2);
  });

  it("force_continue_note_prefers_backend_reason：失败体带 reason（queued_use_retract）→ note 显示后端 reason 原文，不裸示 message", async () => {
    installFetch();
    routes.forceContinueFailures = {
      "u-1": {
        status: 409,
        body: { error: "queued_use_retract", reason: "该消息仍在排队，请使用撤回" },
      },
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "compat_outstanding", ["u-1"]),
      queueView([action({ actionId: "a1" })], "compat_outstanding", ["u-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain(
      "该消息仍在排队，请使用撤回"
    );
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain(
      "codex/queue/force-continue"
    );
  });

  it("force_continue_risk_discloses_mixed_turn_entries：清单含等待 Turn 完成的条目时，确认文案如实披露提前解除顺序限制（混合屏障面——行清单不带 state 无从逐行甄别）", async () => {
    installFetch();
    // 混合在册屏障：unknown（无自动恢复）+ accepted+turnId（等 Turn 完成）并存时，
    // 后端 blockedReasonCode 取优先级更高的 native_delivery_unknown，清单两者都在
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1", "t-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-force-continue"));
    const strip = screen.getByTestId("codex-queue-force-confirm");
    expect(strip.textContent).toContain("Turn");
    expect(strip.textContent).toContain("顺序限制");
    // 确认前零请求（两步确认纪律不变）
    expect(forceContinueBodies()).toEqual([]);
  });

  it("force_continue_i18n_keys_exist：新键两侧（zh 取用文案；键集对齐由 check:i18n 保证）", async () => {
    for (const key of [
      "forceContinue",
      "forceContinueRisk",
      "forceContinueYes",
      "forceContinueCancel",
      "forceContinueFailed",
    ]) {
      const v = i18n.t(`mobile.codex.queue.${key}`);
      expect(v, `键 mobile.codex.queue.${key} 必须存在`).not.toBe(`mobile.codex.queue.${key}`);
      expect(v.length).toBeGreaterThan(0);
    }
  });

  // ==== 打断桌面回合（2026-10-21 新增）====

  it("interrupt_button_only_for_native_turn_pending：打断出口仅 native_turn_pending 可见（其余阻塞态走人工解除/无出口）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"])];
    const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-interrupt")).toBeTruthy();
    unmount();
    // native_delivery_unknown / compat_outstanding：不提供打断（那是人工解除的活）
    routes.queue = [queueView([action({ actionId: "a2" })], "native_delivery_unknown", ["u-1"])];
    const r2 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    r2.unmount();
    routes.queue = [queueView([action({ actionId: "a3" })], "compat_outstanding", ["c-1"])];
    const r3 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    r3.unmount();
  });

  it("native_turn_active_interrupt：桌面手打的原生回合（无屏障）→ 横幅分文案 + 打断按钮可见（2026-10-22 原生回合扩展）；空队列也出按钮；无能力 build 照旧隐藏", async () => {
    installFetch();
    // 空队列 + native_turn_active：面板不自隐（阻塞码在场），按钮出——
    // 原生回合打断不要求有排队消息（用户诉求：桌面在跑就能停）
    routes.queue = [queueView([], "native_turn_active")];
    const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.getAttribute("data-reason")).toBe("native_turn_active");
    expect(banner.textContent).toContain("正在运行");
    expect(banner.textContent).toContain("未完成的工作会丢失");
    expect(screen.getByTestId("codex-queue-interrupt")).toBeTruthy();
    unmount();
    // 无能力 build（canInterrupt=false）：按钮照旧源头隐藏（P2-2 纪律跨阻塞态一致）
    routes.queue = [queueView([], "native_turn_active", [], false)];
    const r2 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    r2.unmount();
  });

  it("interrupt_button_hidden_when_capability_false：canInterrupt=false（或缺省的旧载荷）→ native_turn_pending 阻塞态也不渲染打断按钮，零请求（复审 P2-2：无能力 build 不给假出口）", async () => {
    installFetch();
    // canInterrupt=false：本 Desktop build 未活体验证打断能力
    routes.queue = [queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"], false)];
    const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
    // 面板与阻塞说明条在（不是面板没渲染导致的假绿）
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    // 确认条也不出现（按钮都无，确认臂无从进入）
    expect(screen.queryByTestId("codex-queue-interrupt-confirm")).toBeNull();
    // 零打断请求（无假出口 = 不会发出注定 503 的调用）
    expect(interruptBodies()).toEqual([]);
    unmount();
    // 旧后端载荷缺省 canInterrupt 字段 → 同样视为无能力（保守方向）
    routes.queue = [
      {
        actions: [action({ actionId: "a2" })],
        blockedReasonCode: "native_turn_pending",
        blockedActionIds: ["t-1"],
      },
    ];
    const r2 = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    expect(interruptBodies()).toEqual([]);
    r2.unmount();
  });

  it("interrupt_requires_confirmation：点击先出确认条（含工作丢失风险披露），确认前零请求；取消零请求", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"])];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    const strip = screen.getByTestId("codex-queue-interrupt-confirm");
    // 破坏性动作的风险必须如实披露：回合未完成的工作会丢失
    expect(strip.textContent).toContain("未完成的工作会丢失");
    // 确认前零请求（两步确认纪律）
    expect(interruptBodies()).toEqual([]);
    // 取消：收起确认条，仍零请求
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-no"));
    expect(screen.queryByTestId("codex-queue-interrupt-confirm")).toBeNull();
    expect(interruptBodies()).toEqual([]);
  });

  it("interrupt_success_reports_and_refreshes：确认后调用打断端点（带 sessionId）→ 如实回报并重拉队列", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]), // 操作后重拉
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-yes"));
    await waitFor(() => expect(interruptBodies()).toEqual(['{"sessionId":"s1"}']));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("已发出停止指令");
    expect(queueGetCalls()).toBe(2);
  });

  it("interrupt_no_active_turn_is_not_faked_as_success：桌面无可打断回合 → 如实提示，不谎报成功", async () => {
    installFetch();
    routes.interruptBody = { ok: true, interruptedTurnId: null, expectedTurnId: null };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-yes"));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("没有可停止的回合");
  });

  it("interrupt_uncertain_reported_honestly：uncertain=true（指令已发出但结果未知）→ 如实说「不确定」并劝阻连续重按，不报失败也不谎报确定成功（复审 P2-3）", async () => {
    installFetch();
    // RPC 超时形态：200 但 uncertain=true、interruptedTurnId=null、reasonCode=timeout
    routes.interruptBody = {
      ok: true,
      uncertain: true,
      interruptedTurnId: null,
      expectedTurnId: "t-1",
      reasonCode: "timeout",
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]), // 操作后重拉
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-yes"));
    await waitFor(() => expect(interruptBodies()).toEqual(['{"sessionId":"s1"}']));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    // 如实呈现「不确定」+ 明确劝阻连续重按（重按 = 双发破坏性指令）
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("不确定");
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("不要连续重按");
    // 不是失败（不得诱导立刻重按），也不是「没有可停止的回合」
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain("停止失败");
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain("没有可停止的回合");
    // 仍重拉权威列表（回合真实终态以服务端账本为准）
    expect(queueGetCalls()).toBe(2);
  });

  it("interrupt_failure_shows_error：503 能力不可用（body 带中文 reason）→ note 显示后端 reason 原文，不裸示机器码（仍重拉权威列表）", async () => {
    installFetch();
    routes.interruptStatus = 503;
    routes.interruptErrorBody = {
      error: "capability_unavailable",
      reason: "当前桌面 Codex 版本未启用远程停止能力（未完成活体验证），无法远程打断回合",
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-yes"));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("停止失败");
    // reason 回退链第一优先：后端中文 reason 原文可见，机器码不裸示
    expect(screen.getByTestId("codex-queue-note").textContent).toContain(
      "当前桌面 Codex 版本未启用远程停止能力（未完成活体验证），无法远程打断回合"
    );
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain(
      "capability_unavailable"
    );
    expect(queueGetCalls()).toBe(2);
  });

  it("interrupt_failure_shows_backend_reason_desktop_rejected：409 desktop_rejected（桌面明确拒绝）→ note 显示中文 reason，不裸示机器码", async () => {
    installFetch();
    routes.interruptStatus = 409;
    routes.interruptErrorBody = {
      error: "desktop_rejected",
      reason: "桌面 Codex 明确拒绝了本次打断指令，回合未被停止",
    };
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["t-1"]),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(screen.getByTestId("codex-queue-interrupt-yes"));
    await waitFor(() => expect(screen.getByTestId("codex-queue-note")).toBeTruthy());
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("停止失败");
    expect(screen.getByTestId("codex-queue-note").textContent).toContain(
      "桌面 Codex 明确拒绝了本次打断指令，回合未被停止"
    );
    expect(screen.getByTestId("codex-queue-note").textContent).not.toContain("desktop_rejected");
  });

  it("interrupt_i18n_keys_exist：新键两侧必须存在（键集对齐由 check:i18n 保证）", async () => {
    for (const key of [
      "interrupt",
      "interruptRisk",
      "interruptYes",
      "interruptCancel",
      "interruptDone",
      "interruptUncertain",
      "interruptNoTurn",
      "interruptFailed",
    ]) {
      const v = i18n.t(`mobile.codex.queue.${key}`);
      expect(v, `键 mobile.codex.queue.${key} 必须存在`).not.toBe(`mobile.codex.queue.${key}`);
      expect(v.length).toBeGreaterThan(0);
    }
  });
});
