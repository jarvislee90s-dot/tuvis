import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import CodexQueuePanel from "@/mobile/CodexQueuePanel";
import type { CodexActionView, CodexQueueView, ThreadProjection } from "@/mobile/codexIpcApi";
// T05：页面共享停止控制器（Harness 挂真实 hook）
import { useCodexTurnControl } from "@/mobile/useCodexTurnControl";

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
  canInterrupt: boolean = true,
  /** T06：队列暂停位（S2 持久暂停；缺省 undefined = 未暂停） */
  queuePaused?: boolean
): CodexQueueView {
  return { actions, blockedReasonCode, blockedActionIds, canInterrupt, queuePaused };
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
  /** 首次加载后模拟队列查询失败。 */
  queueFailAfterInitial?: boolean;
  /** POST /codex/turn/interrupt 的失败状态码（缺省 200） */
  interruptStatus?: number;
  /** POST /codex/turn/interrupt 的错误体覆盖（缺省 {error:"capability_unavailable"}） */
  interruptErrorBody?: Record<string, unknown>;
  /** POST /codex/turn/interrupt 的回执体覆盖（null = 桌面无可打断回合） */
  interruptBody?: Record<string, unknown>;
  /** T05：interrupt 挂起不响应（共享控制器 stopping 窗；_releaseInterrupt 放行） */
  interruptHang?: boolean;
  /** T06：POST /codex/queue/prioritize 的成功回执体（判别联合；缺省 prioritized） */
  prioritizeBody?: Record<string, unknown>;
  /** T06：prioritize 的失败状态码（缺省 200） */
  prioritizeStatus?: number;
  prioritizeErrorBody?: Record<string, unknown>;
  prioritizeHang?: boolean;
  prioritizeNetworkError?: boolean;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
/** queueHang 挂起请求的放行器（测试中手动 resolve 模拟旧会话迟到响应） */
let releaseQueue: ((v: CodexQueueView) => void) | null = null;
/** T05：interruptHang 挂起请求的放行器 */
let _releaseInterrupt: ((r: Response) => void) | null = null;

/** 等待队列载入面板后再断言行和操作。空队列自隐的用例直接渲染。 */
async function renderLoadedPanel(jsx: React.ReactElement) {
  const result = render(jsx);
  await screen.findByTestId("codex-queue-toggle");
  return result;
}

beforeEach(() => {
  routes = {};
  releaseQueue = null;
  _releaseInterrupt = null;
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
      if (routes.interruptHang) {
        return new Promise<Response>((resolve) => {
          _releaseInterrupt = resolve;
        });
      }
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
    if (url.includes("/codex/queue/prioritize")) {
      if (routes.prioritizeNetworkError) throw new Error("offline");
      if (routes.prioritizeHang) return new Promise<Response>(() => {});
      if (routes.prioritizeStatus) {
        return new Response(JSON.stringify(routes.prioritizeErrorBody ?? { error: "internal" }), {
          status: routes.prioritizeStatus,
        });
      }
      const body = JSON.parse(String(init?.body)) as { actionId: string };
      return new Response(
        JSON.stringify(
          routes.prioritizeBody ?? {
            result: "prioritized",
            actionId: body.actionId,
            receipt: {
              actionId: body.actionId,
              delivery: "accepted",
              route: "desktop_ipc",
              turnId: "turn-x",
              envelopeId: null,
              reasonCode: null,
            },
          }
        ),
        { status: 200 }
      );
    }
    if (url.includes("/codex/queue")) {
      if (routes.queueFailAfterInitial && routes.queue?.length === 0) {
        return new Response(JSON.stringify({ error: "internal" }), { status: 500 });
      }
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
      !String(u).includes("/force-continue") &&
      !String(u).includes("/prioritize")
  ).length;
}

/** T06：优先处理请求体列表（载荷断言用） */
function prioritizeBodies(): string[] {
  return fetchMock.mock.calls
    .filter(([u]) => String(u).includes("/codex/queue/prioritize"))
    .map(([, init]) => String((init as RequestInit)?.body));
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
  it("默认展开并按队列顺序显示所有消息行", async () => {
    installFetch();
    const actions = Array.from({ length: 5 }, (_, index) =>
      action({ actionId: `a${index + 1}`, position: index + 1 })
    );
    routes.queue = [queueView(actions)];

    render(<CodexQueuePanel sessionId="s1" />);

    const toggle = await screen.findByTestId("codex-queue-toggle");
    expect(toggle.textContent).toBe(i18n.t("mobile.codex.queue.summaryHide"));
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getAllByTestId(/^codex-queue-row-/).map((row) => row.dataset.testid)).toEqual(
      actions.map(({ actionId }) => `codex-queue-row-${actionId}`)
    );
  });

  it("手动收起后，同一挂载中的刷新、入队和清空后重新入队都保持收起", async () => {
    installFetch();
    routes.queue = [queueView([action()])];
    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);

    await screen.findByTestId("codex-queue-row-a1");
    fireEvent.click(screen.getByTestId("codex-queue-toggle"));
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();

    // 普通刷新改动摘要状态，用摘要确认这次响应已落地。
    routes.queue = [queueView([action()], "native_turn_pending")];
    rerender(<CodexQueuePanel sessionId="s1" refreshSignal={1} />);
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-awaiting-turn")).toBe(
        "true"
      )
    );
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();

    routes.queue = [queueView([action(), action({ actionId: "a2", position: 2 })])];
    rerender(<CodexQueuePanel sessionId="s1" refreshSignal={2} />);
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-count")).toBe("2")
    );
    expect(screen.queryAllByTestId(/^codex-queue-row-/)).toHaveLength(0);

    routes.queue = [queueView([])];
    rerender(<CodexQueuePanel sessionId="s1" refreshSignal={3} />);
    await waitFor(() => expect(screen.queryByTestId("codex-queue-panel")).toBeNull());

    routes.queue = [queueView([action({ actionId: "a3" })])];
    rerender(<CodexQueuePanel sessionId="s1" refreshSignal={4} />);
    const toggle = await screen.findByTestId("codex-queue-toggle");
    expect(toggle.textContent).toBe(i18n.t("mobile.codex.queue.summaryView"));
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("codex-queue-row-a3")).toBeNull();
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("codex-queue-row-a3")).toBeTruthy();
  });

  it("renders_fifo_positions：挂载拉取显示待发与实际 FIFO 位置；无插队按钮", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 }), action({ actionId: "a2", position: 2 })]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const row1 = await screen.findByTestId("codex-queue-row-a1");
    expect(row1.textContent).toContain("第1位");
    expect(screen.getByTestId("codex-queue-row-a2").textContent).toContain("第2位");
    // 撤回按钮在 queued 行上
    expect(screen.getByTestId("codex-queue-retract-a1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-retract-a2")).toBeTruthy();
    // **不提供插队**：立即发送/插队语义的按钮一律不存在
    expect(screen.queryByText("立即发送")).toBeNull();
    expect(screen.getAllByText("插队")).toHaveLength(2);
    expect(screen.queryByTestId("codex-queue-jump")).toBeNull();
  });

  it("retract_success_removes_entry：撤回成功（cancelled）→ 操作后重拉，条目随权威列表消失", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })]),
      queueView([]), // 操作后重拉：已撤条目不在队
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-row-a3");
    expect(screen.getByTestId("codex-queue-row-a3").textContent).toContain("已取消");
    expect(screen.queryByTestId("codex-queue-retract-a3")).toBeNull();
    expect(screen.queryByRole("button", { name: "重试" })).toBeNull();
  });

  it("compat_outstanding_banner：blockedReasonCode 非空 → 暂停派发说明条（compat_outstanding）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "compat_outstanding")];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.textContent).toContain("兼容");
    expect(banner.textContent).toContain("暂停");
  });

  it("native_delivery_unknown_banner：原生 Unknown 屏障 → 如实说无自动恢复（不得承诺自动继续）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a3" })], "native_delivery_unknown")];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const banner = await screen.findByTestId("codex-queue-blocked");
    expect(banner.getAttribute("data-reason")).toBe("native_delivery_unknown");
    expect(banner.textContent).toContain("未知");
    expect(banner.textContent).toContain("无自动恢复");
    expect(banner.textContent).not.toContain("自动继续");
  });

  it("native_turn_pending_banner：原生 Turn 屏障 → 分文案（不得说成兼容模式/回 Desktop 手工解除）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a2" })], "native_turn_pending")];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(screen.queryByTestId("codex-queue-blocked")).toBeNull();
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
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
    // C04：挂起/空视图面板自隐——无 toggle 可点，普通渲染即可（断言本就是"不回灌"）。
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
    const { unmount } = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-force-continue")).toBeTruthy();
    unmount();
    // compat_outstanding：按钮同样可见
    routes.queue = [queueView([action({ actionId: "a2" })], "compat_outstanding", ["c-1"])];
    const r2 = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-force-continue")).toBeTruthy();
    r2.unmount();
    // native_turn_pending（等待 Turn 完成**自动**继续）：不提供人工解除
    routes.queue = [queueView([action({ actionId: "a3" })], "native_turn_pending", ["t-1"])];
    const r3 = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(screen.queryByTestId("codex-queue-blocked")).toBeNull();
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    r3.unmount();
    // 无靶向清单（旧后端/无在册屏障）：不显示
    routes.queue = [queueView([action({ actionId: "a4" })], "native_delivery_unknown")];
    const r4 = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-blocked");
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    r4.unmount();
  });

  it("force_continue_requires_confirmation：点击按钮先出确认条（含风险文案），确认前不发任何请求；取消不打请求", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
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

  it("c05_summary_waits_not_manual_verify_for_running_turn：已受理并绑定回合的正常屏障 → 摘要说「等待自动完成」而非「待人工核对」（复审 Q02）", async () => {
    installFetch();
    // 普通发送被受理后的真实形状：accepted + turnId → native_turn_pending，
    // blockedActionIds 非空（在册屏障清单包含它）——但它有自动恢复路径
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_turn_pending", ["act-accepted"], true, false),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    const summary = await screen.findByTestId("codex-queue-summary");
    // 摘要如实说「等待回合完成」——不能让用户以为有需要人工解除的东西
    expect(summary.getAttribute("data-verify-pending")).toBeNull();
    expect(summary.getAttribute("data-awaiting-turn")).toBe("true");
    expect(summary.textContent).toContain("等待");
    expect(summary.textContent).not.toContain("人工核对");
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
  });

  it("c05_summary_manual_verify_for_true_unknown：真实 Unknown / 兼容屏障 → 摘要明示待人工核对，处理入口直接可见", async () => {
    installFetch();
    for (const reason of ["native_delivery_unknown", "compat_outstanding"]) {
      routes.queue = [queueView([action({ actionId: "a1" })], reason, ["u-1"], true, false)];
      const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
      const summary = await screen.findByTestId("codex-queue-summary");
      expect(summary.getAttribute("data-verify-pending"), reason).toBe("true");
      expect(summary.getAttribute("data-awaiting-turn"), reason).toBeNull();
      expect(summary.textContent, reason).toContain("人工核对");
      expect(await screen.findByTestId("codex-queue-force-continue"), reason).toBeTruthy();
      unmount();
    }
  });

  it("c05_summary_unknown_beats_plain_pause：Unknown 与普通暂停并存 → 摘要优先核对原因（同一暂停只解释一次）", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1" })], "native_delivery_unknown", ["u-1"], true, true),
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    const summary = await screen.findByTestId("codex-queue-summary");
    expect(summary.getAttribute("data-paused")).toBe("true");
    expect(summary.getAttribute("data-verify-pending")).toBe("true");
    expect(summary.textContent).toContain("人工核对");
    expect(summary.textContent).not.toContain("已暂停");
  });

  it("c05_summary_blocked_list_read_failure_not_claimed_normal：阻塞码声称 Unknown 但清单读取降级为空 → 仍如实提示人工核对（不假称正常）", async () => {
    installFetch();
    // 后端 barrier_action_ids 读取失败时如实降级为空清单，而 blockedReasonCode
    // 由另一路读取独立派生——此时清单为空不能反证「无需核对」
    routes.queue = [queueView([action({ actionId: "a1" })], "native_delivery_unknown", [])];
    render(<CodexQueuePanel sessionId="s1" />);
    const summary = await screen.findByTestId("codex-queue-summary");
    expect(summary.getAttribute("data-verify-pending")).toBe("true");
    expect(summary.textContent).toContain("人工核对");
  });

  it("running_queue_has_no_duplicate_stop_or_long_banner", async () => {
    installFetch();
    routes.queue = [queueView([action()], "native_turn_pending", ["t-1"])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(screen.queryByTestId("codex-queue-title")).toBeNull();
    expect(screen.queryByTestId("codex-queue-blocked")).toBeNull();
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
  });
  it("hides the queue panel when an accepted turn is running and nothing is queued", async () => {
    installFetch();
    routes.queue = [queueView([], "native_turn_pending", ["act-accepted"])];

    await act(async () => {
      render(<CodexQueuePanel sessionId="s1" />);
    });

    expect(queueGetCalls()).toBe(1);
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
  });

  it("empty_unknown_keeps_check_entry", async () => {
    installFetch();
    routes.queue = [queueView([], "native_delivery_unknown", ["u1"])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(screen.getByTestId("codex-queue-force-continue")).toBeTruthy();
  });
});

// ============================================================
// T05：页面共享停止控制器（SessionDetail 单例）——队列面板打断确认经它发起：
// 与 composer 停止入口共用 stopping 互斥闸；目标在点击时刻冻结；分诊/文案同源。
// ============================================================

/** 运行中投影夹具（与 MessageComposer.test 同款三元组） */
function stopProjection(over: Record<string, unknown> = {}): ThreadProjection {
  return {
    threadId: "th-q",
    revision: 7,
    stale: false,
    availability: "controllable",
    capabilities: {
      send: true,
      state: true,
      command: true,
      permissions: false,
      blockingInput: true,
      interrupt: true,
    },
    runtimeStatus: "active",
    contentState: "ready",
    messages: [],
    truncated: false,
    pending: [],
    effectiveOperations: {
      policyRevision: 1,
      operations: {
        interrupt: {
          implemented: true,
          allowed: true,
          requiresAutoAdapt: false,
          evidence: "observed_success",
          reasonCode: null,
        },
      },
    },
    activeTurnId: "turn-A",
    ...over,
  } as ThreadProjection;
}

/** 挂真实共享控制器的 Harness（SessionDetail 持有方式的组件级等价）。 */
function PanelHarness({
  sessionId = "s1",
  projection,
}: {
  sessionId?: string;
  projection: ThreadProjection | null;
}) {
  const control = useCodexTurnControl(projection, sessionId);
  return (
    <div>
      <CodexQueuePanel sessionId={sessionId} turnControl={control} />
      <span data-testid="hook-stopping">{control.stopping ? "1" : "0"}</span>
      <span data-testid="hook-result">{control.result?.kind ?? ""}</span>
    </div>
  );
}

describe("CodexQueuePanel：T06 先处理这条", () => {
  it("t06_entry_visibility：入队前不显示；入队后仅 queued 行显示；终态行没有入口", async () => {
    installFetch();
    // 入队前：面板自隐（无 queued 无阻塞）→ 入口自然不存在
    routes.queue = [queueView([])];
    const { unmount } = render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    unmount();
    // 入队后：queued 行有「先处理这条」；cancelled 终态行没有
    routes.queue = [
      queueView([
        action({ actionId: "a1", position: 1 }),
        action({ actionId: "a2", state: "cancelled", position: null }),
      ]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(await screen.findByTestId("codex-queue-prioritize-a1")).toBeTruthy();
    expect(screen.queryByTestId("codex-queue-prioritize-a2")).toBeNull();
    // 入口是显式单条触发，不是 FIFO 拖动排序（无任何拖动/排序 UI）
    expect(screen.queryByTestId("codex-queue-reorder")).toBeNull();
  });

  it("t06_confirm_before_request：确认前零请求；取消不打请求", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1", position: 1 })], null, [], true, true)];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    // 确认臂出现；确认前零请求
    expect(screen.getByTestId("codex-queue-prioritize-confirm-a1").textContent).toContain(
      "越过暂停的队列"
    );
    expect(prioritizeBodies()).toEqual([]);
    // 取消：收起确认臂，仍零请求
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-no-a1"));
    expect(screen.queryByTestId("codex-queue-prioritize-confirm-a1")).toBeNull();
    expect(prioritizeBodies()).toEqual([]);
  });

  it("t06_paused_idle_bypass：暂停空闲仅确认本条越队——载荷 confirmInterrupt:false、无 turnId；成功后状态推进且其余行不动", async () => {
    installFetch();
    const receipt = {
      actionId: "a2",
      delivery: "accepted",
      route: "desktop_ipc",
      turnId: "turn-x",
      envelopeId: null,
      reasonCode: null,
    };
    routes.prioritizeBody = { result: "prioritized", actionId: "a2", receipt };
    routes.queue = [
      queueView(
        [action({ actionId: "a1", position: 1 }), action({ actionId: "a2", position: 2 })],
        null,
        [],
        true,
        true
      ),
      // 服务端队列只返回 queued 行。
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a2"));
    // 暂停空闲：确认文案不含停止风险（仅越队确认）
    expect(screen.getByTestId("codex-queue-prioritize-confirm-a2").textContent).toContain(
      "越过暂停的队列"
    );
    expect(screen.getByTestId("codex-queue-prioritize-confirm-a2").textContent).not.toContain(
      "停止桌面"
    );
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a2"));
    // 载荷：原 actionId + confirmInterrupt:false；**不接受正文副本**（无 text 字段）
    await waitFor(() => expect(prioritizeBodies()).toHaveLength(1));
    expect(JSON.parse(prioritizeBodies()[0])).toEqual({
      sessionId: "s1",
      actionId: "a2",
      confirmInterrupt: false,
    });
    // 回执：已优先发送；a2 移出待发列表，a1 原样在队。
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("已优先发送")
    );
    expect(screen.queryByTestId("codex-queue-row-a2")).toBeNull();
    expect(screen.getByTestId("codex-queue-position-a1").textContent).toContain("第1位");
    // 零独立打断帧（单条编排请求；停止只会在编排内部按需发生）
    expect(interruptBodies()).toEqual([]);
  });

  it("t06_active_stop_confirm：运行中先确认停止该回合——文案披露停止风险，载荷带冻结 turnId + confirmInterrupt:true，且零独立打断请求", async () => {
    installFetch();
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })]),
      queueView([action({ actionId: "a1", position: 1 })]),
    ];
    render(<PanelHarness projection={stopProjection()} />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    // 运行中：确认文案如实披露停止风险
    const strip = screen.getByTestId("codex-queue-prioritize-confirm-a1");
    expect(strip.textContent).toContain("停止桌面当前回合");
    expect(strip.textContent).toContain("会丢失");
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    // 载荷：expectedTurnId = 点击确认时刻冻结的活动回合 + confirmInterrupt:true
    await waitFor(() => expect(prioritizeBodies()).toHaveLength(1));
    expect(JSON.parse(prioritizeBodies()[0])).toEqual({
      sessionId: "s1",
      actionId: "a1",
      expectedTurnId: "turn-A",
      confirmInterrupt: true,
    });
    // 单条编排：不另发 /codex/turn/interrupt
    expect(interruptBodies()).toEqual([]);
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("已优先发送")
    );
  });

  it("t06_receipt_not_sent_keeps_entry：明确零投递（queued=true）→ 行保留 + 拒绝原因如实；队列不 cleared", async () => {
    installFetch();
    routes.prioritizeBody = {
      result: "not_sent",
      actionId: "a1",
      reasonCode: "native_pending",
      receipt: null,
      queued: true,
    };
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("仍在队列")
    );
    // 拒绝原因可读化（native_pending → 原生请求文案，不裸示机器码）
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("原生请求");
    // 行保留（入口仍在，可重试）；其他消息不受影响
    expect(screen.getByTestId("codex-queue-prioritize-a1")).toBeTruthy();
  });

  it("t06_receipt_unknown_honest：unknown → 如实呈现不诱导重按；行与核对入口保留", async () => {
    installFetch();
    routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: "stop_timeout" };
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("结果未知")
    );
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("不会自动重试");
    // 行保留：用户可稍后核对/重试（入口不清除）
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
  });

  it("t06_receipt_already_dispatched：已派发 → 回实际状态且明确未重复发送", async () => {
    installFetch();
    routes.prioritizeBody = {
      result: "already_dispatched",
      actionId: "a1",
      action: {
        actionId: "a1",
        state: "reserved",
        position: null,
        receipt: null,
        stage: "dispatching",
      },
    };
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
      queueView(
        [action({ actionId: "a1", state: "reserved", position: null })],
        null,
        [],
        true,
        true
      ),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("未重复发送")
    );
    expect(screen.getByTestId("codex-queue-note").textContent).toContain("派发中");
  });

  // ============================================================
  // P4：队列行正文预览（CodexActionView.textPreview，可选字段；旧后端缺省
  // undefined → 不渲染预览行，前端必须正常工作）
  // ============================================================

  it("queue_row_text_preview：queued 行带 textPreview → 剥尾部移动端签名后渲染预览行", async () => {
    installFetch();
    routes.queue = [
      queueView([
        action({
          actionId: "a1",
          position: 1,
          textPreview: "帮我看看这个报错 [mobile iPhone 15]",
        }),
      ]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const preview = await screen.findByTestId("codex-queue-preview-a1");
    // 尾部 [mobile …] 签名剥离；预览是普通文本节点（不 aria-hidden，可被读屏读到）
    expect(preview.textContent).toBe("帮我看看这个报错");
    expect(preview.textContent).not.toContain("[mobile");
    expect(preview.getAttribute("aria-hidden")).toBeNull();
  });

  it("queue_row_text_preview_absent：旧后端无 textPreview 或 accepted/unknown 行（契约不携带）→ 不渲染预览", async () => {
    installFetch();
    routes.queue = [
      queueView([
        action({ actionId: "a1", position: 1 }), // 旧后端：字段整体缺省
        // 契约：只有 state=queued 的行携带 textPreview，accepted/unknown 行为 undefined
        action({ actionId: "a2", state: "accepted", position: null }),
        action({ actionId: "a3", state: "unknown", position: null }),
        // 同屏混合：queued 行带预览 → 仅该行渲染（按字段存在性逐行分流）
        action({
          actionId: "a4",
          position: 2,
          textPreview: "只有 queued 行有预览 [mobile iPad]",
        }),
      ]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-row-a1");
    expect(screen.queryByTestId("codex-queue-preview-a1")).toBeNull();
    expect(screen.queryByTestId("codex-queue-preview-a2")).toBeNull();
    expect(screen.queryByTestId("codex-queue-preview-a3")).toBeNull();
    expect(screen.getByTestId("codex-queue-preview-a4").textContent).toBe("只有 queued 行有预览");
  });

  it("queue_row_text_preview_truncate：剥签名后超过 40 字符 → 截前 40 字符 + 省略号（按字符数）", async () => {
    installFetch();
    routes.queue = [queueView([action({ actionId: "a1", textPreview: "字".repeat(50) })])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const preview = await screen.findByTestId("codex-queue-preview-a1");
    expect(preview.textContent).toBe("字".repeat(40) + "…");
  });

  it("queue_row_text_preview_single_line：换行折叠为空格归一单行，附件标记替换为可读占位", async () => {
    installFetch();
    routes.queue = [
      queueView([
        action({
          actionId: "a1",
          textPreview: '第一行\n第二行\n<image path="/tmp/a.png"> 看这张图 [mobile iPad]',
        }),
      ]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const preview = await screen.findByTestId("codex-queue-preview-a1");
    // 单行归一（无换行）；<image …> → [附件]（mobile.composer.attachMark 同文案）；
    // 尾部签名剥离
    expect(preview.textContent).toBe("第一行 第二行 [附件] 看这张图");
  });

  // ============================================================
  // P6（2026-10-31 用户要求）：队列行压成一行——位次 / 正文 / 长度 / 三个按钮
  // 同排并列，不再用「位次独占一行 + 预览独占一行 + 按钮占第三行」的三行排版。
  // ============================================================

  it("queue_row_single_line_layout：位次、正文预览、长度和三个按钮在同一条 flex 行内（无 w-full 换行块）", async () => {
    installFetch();
    routes.queue = [
      queueView([
        action({
          actionId: "a1",
          position: 1,
          textPreview: "sleep 300秒 然后计算3*5",
        }),
      ]),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    const row = await screen.findByTestId("codex-queue-row-a1");
    // 行容器必须禁换行（nowrap）：单行排版的前提
    expect(row.className).toContain("flex-nowrap");
    expect(row.className).not.toContain("flex-wrap");
    // 预览必须是行内元素（不再独占整行）——出现 w-full 就等于又变回多行
    const preview = screen.getByTestId("codex-queue-preview-a1");
    expect(preview.className).not.toContain("w-full");
    // 预览元素带「剩余空间收缩」语义，避免把按钮挤出屏幕
    expect(preview.className).toContain("truncate");
    // 长度标识：同一行内显示该条正文的**真实字符数**（剥签名后、不按预览截断）
    const length = screen.getByTestId("codex-queue-length-a1");
    expect(length.textContent).toContain("18");
    // 位次、预览、长度、按钮同一父节点（同一行），顺序为 位次→预览→长度→按钮
    const children = Array.from(row.children).map((c) => c.getAttribute("data-testid"));
    const posIdx = children.indexOf("codex-queue-position-a1");
    const prevIdx = children.indexOf("codex-queue-preview-a1");
    const lenIdx = children.indexOf("codex-queue-length-a1");
    const prioIdx = children.indexOf("codex-queue-prioritize-a1");
    expect(posIdx).toBeGreaterThanOrEqual(0);
    expect(prevIdx).toBeGreaterThan(posIdx);
    expect(lenIdx).toBeGreaterThan(prevIdx);
    expect(prioIdx).toBeGreaterThan(lenIdx);
  });

  // ============================================================
  // P2：插队拒绝文案按屏障分型——「某条消息投递结果未知」与「桌面正忙」语义
  // 不同，不得共用同一句（后者对前者是误导）
  // ============================================================

  it("t06_receipt_not_sent_unknown_barrier：unknown_delivery_barrier → 「投递结果未知」文案，不说桌面正忙", async () => {
    installFetch();
    routes.prioritizeBody = {
      result: "not_sent",
      actionId: "a1",
      reasonCode: "unknown_delivery_barrier",
      receipt: null,
      queued: true,
    };
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("投递结果未知")
    );
    const note = screen.getByTestId("codex-queue-note").textContent ?? "";
    // 如实：这条仍在队列、没有被发出去；且不与「桌面正忙」混用
    expect(note).toContain("仍在队列");
    expect(note).not.toContain("桌面正忙");
  });

  it("t06_receipt_not_sent_busy_barrier：barrier_outstanding → 仍是原「桌面正忙」文案（两码不混同）", async () => {
    installFetch();
    routes.prioritizeBody = {
      result: "not_sent",
      actionId: "a1",
      reasonCode: "barrier_outstanding",
      receipt: null,
      queued: true,
    };
    routes.queue = [
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
      queueView([action({ actionId: "a1", position: 1 })], null, [], true, true),
    ];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-note").textContent).toContain("桌面正忙")
    );
  });
});

describe("priority queue reconciliation", () => {
  const target = action({ textPreview: "same text" });
  const other = action({ actionId: "a2", position: 2, textPreview: "same text" });
  const both = queueView([target, other]);
  const remaining = queueView([{ ...other, position: 1 }]);

  async function confirmPriority() {
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-a1"));
    expect(prioritizeBodies()).toHaveLength(0);
    await act(async () => {
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
    });
  }

  it.each(["prioritized", "already_dispatched"])(
    "%s acceptance removes only the confirmed action before a delayed GET, and a stale GET cannot restore it",
    async (result) => {
      installFetch();
      routes.queue = [both];
      if (result === "already_dispatched") {
        routes.prioritizeBody = {
          result,
          actionId: "a1",
          action: action({ state: "accepted", position: null }),
        };
      }
      const panel = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      vi.useFakeTimers();
      routes.queueHang = true;
      await confirmPriority();
      expect(queueGetCalls()).toBe(2);
      expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
      expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();

      await act(async () => releaseQueue?.(both));
      expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
      expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();

      routes.queueFailAfterInitial = true;
      panel.rerender(<CodexQueuePanel sessionId="s1" refreshSignal={1} />);
      await act(async () => vi.advanceTimersByTimeAsync(500));
      expect(queueGetCalls()).toBe(3);
      expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
      expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();
    }
  );

  it.each([
    ["not_sent", "busy_before_write"],
    ["not_sent", "dispatch_in_flight"],
    ["unknown", "front_budget_elapsed"],
  ])("reconciles a later departure after %s %s without a projection tick", async (result, code) => {
    installFetch();
    routes.queue = [both, both, both, remaining];
    routes.prioritizeBody = {
      result,
      actionId: "a1",
      reasonCode: code,
      ...(result === "not_sent" ? { queued: true, receipt: null } : {}),
    };
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    vi.useFakeTimers();
    await confirmPriority();
    expect(queueGetCalls()).toBe(2);
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-note").textContent).toMatch(/仍在队列|仍在排队/);

    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(3);
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();

    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(4);
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();
    expect(screen.queryByTestId("codex-queue-note")?.textContent ?? "").not.toMatch(
      /仍在队列|仍在排队|已优先发送|已接受/
    );
    // 短窗内继续观察可能的回队，但截止后不再读。
    routes.queue = [remaining, remaining, remaining, remaining];
    await act(async () => vi.advanceTimersByTimeAsync(15000));
    expect(queueGetCalls()).toBe(7);
    expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();
    expect(prioritizeBodies()).toHaveLength(1);
  });

  function mockBusyRefusal() {
    routes.prioritizeBody = {
      result: "not_sent",
      actionId: "a1",
      reasonCode: "busy_before_write",
      queued: true,
      receipt: null,
    };
  }

  it("requires a successful snapshot to claim the action is still queued and recovers from GET failures", async () => {
    installFetch();
    routes.queue = [both];
    routes.queueFailAfterInitial = true;
    mockBusyRefusal();
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    vi.useFakeTimers();
    await confirmPriority();
    expect(screen.getByTestId("codex-queue-note").textContent).not.toMatch(/仍在队列|仍在排队/);
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(3);

    routes.queue = [remaining];
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(4);
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    expect(screen.queryByTestId("codex-queue-note")).toBeNull();
  });

  it("aborts a hanging GET and continues the observation without applying its late response", async () => {
    installFetch();
    routes.queue = [both, both, remaining];
    mockBusyRefusal();
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    vi.useFakeTimers();
    await confirmPriority();
    routes.queueHang = true;
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(3);
    const observation = fetchMock.mock.calls.at(-1)?.[1] as RequestInit;
    expect(observation.signal?.aborted).toBe(false);
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(3);
    expect(observation.signal?.aborted).toBe(true);
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(queueGetCalls()).toBe(4);
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    await act(async () => releaseQueue?.(both));
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    expect(screen.getByTestId("codex-queue-row-a2")).toBeTruthy();
  });

  it("restores a nonterminal action that returns to the queue", async () => {
    installFetch();
    routes.queue = [both, both, remaining, both];
    mockBusyRefusal();
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    vi.useFakeTimers();
    await confirmPriority();
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
    await act(async () => vi.advanceTimersByTimeAsync(2000));
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
  });

  it.each(["scope change", "unmount", "new operation"])(
    "%s aborts an in-flight observation and prevents further observation reads",
    async (event) => {
      installFetch();
      routes.queue = [both, both, remaining];
      mockBusyRefusal();
      const panel = await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      vi.useFakeTimers();
      await confirmPriority();
      routes.queueHang = true;
      await act(async () => vi.advanceTimersByTimeAsync(2000));
      expect(queueGetCalls()).toBe(3);
      const observation = fetchMock.mock.calls.at(-1)?.[1] as RequestInit;
      expect(observation.signal?.aborted).toBe(false);

      if (event === "scope change") {
        panel.rerender(<CodexQueuePanel sessionId="s2" />);
      } else if (event === "unmount") {
        panel.unmount();
      } else {
        await act(async () => fireEvent.click(screen.getByTestId("codex-queue-retract-a2")));
      }
      expect(observation.signal?.aborted).toBe(true);
      await act(async () => releaseQueue?.(both));
      await act(async () => vi.advanceTimersByTimeAsync(15000));
      expect(queueGetCalls()).toBe(event === "unmount" ? 3 : 4);
      expect(screen.queryByTestId("codex-queue-row-a1")).toBeNull();
      expect(screen.queryByTestId("codex-queue-note")).toBeNull();
    }
  );
});

// 响应直接取自 queue_prioritize 的判别联合；每项都经过真实面板及原确认臂。
describe("queue resolution reason", () => {
  afterEach(async () => {
    await i18n.changeLanguage("zh");
  });

  async function submit(body: Record<string, unknown>, next = action()) {
    installFetch();
    routes.prioritizeBody = body;
    routes.queue = [queueView([action()]), queueView([next])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    expect(prioritizeBodies()).toHaveLength(0);
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
    await waitFor(() => expect(prioritizeBodies()).toHaveLength(1));
    expect(JSON.parse(prioritizeBodies()[0])).toEqual({
      sessionId: "s1",
      actionId: "a1",
      confirmInterrupt: false,
    });
    return await screen.findByTestId("codex-queue-note");
  }

  it.each([
    ["zh", /仍在队列/, /(?:其他|另一|已有).*(?:进行中|在途|处理中|派发)/, /桌面正忙/],
    [
      "en",
      /still queued/i,
      /(?:another|other|existing).*(?:in flight|progress|dispatch)/i,
      /desktop is busy/i,
    ],
  ])(
    "queue_resolution_reason_dispatch_in_flight_is_not_generic_busy %s",
    async (lang, queued, inFlight, busy) => {
      await i18n.changeLanguage(lang);
      const note = await submit({
        result: "not_sent",
        actionId: "a1",
        reasonCode: "dispatch_in_flight",
        receipt: null,
        queued: true,
      });
      expect(note.textContent).toMatch(queued);
      expect(note.textContent).toMatch(inFlight);
      expect(note.textContent).not.toMatch(busy);
    }
  );

  it.each([
    ["zh", /撤回|取消/, /仍在队列|已优先发送|已接受/],
    ["en", /withdrawn|cancelled|canceled/i, /still queued|sent this one first|accepted/i],
  ])(
    "queue_resolution_reason_not_sent_keeps_actual_queue_state cancelled %s",
    async (lang, cancelled, forbidden) => {
      await i18n.changeLanguage(lang);
      const note = await submit(
        {
          result: "not_sent",
          actionId: "a1",
          reasonCode: "action_cancelled",
          receipt: null,
          queued: false,
        },
        action({ actionId: "a2", position: 1 })
      );
      expect(note.textContent).toMatch(cancelled);
      expect(note.textContent).not.toMatch(forbidden);
    }
  );

  it.each(["zh", "en"])(
    "queue_resolution_reason_not_sent_keeps_actual_queue_state rejected receipt %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      const receipt = {
        actionId: "a1",
        delivery: "rejected",
        route: "desktop_ipc",
        turnId: null,
        envelopeId: "env-1",
        reasonCode: "desktop_rejected",
      };
      const note = await submit(
        {
          result: "not_sent",
          actionId: "a1",
          reasonCode: "desktop_rejected",
          receipt,
          queued: false,
        },
        action({ actionId: "a2", position: 1 })
      );
      expect(note.textContent).toMatch(lang === "zh" ? /拒绝/ : /reject/i);
      expect(note.textContent).not.toMatch(
        lang === "zh" ? /仍在队列|已优先发送/ : /still queued|sent this one first/i
      );
    }
  );

  it.each([
    ["state_unknown", /状态.*(?:无法|未知|确认)/, /state.*(?:unknown|confirm)/i],
    ["native_pending", /原生请求|审批|提问/, /native request|approval|question/i],
    [
      "expected_turn_required",
      /回合.*(?:确认|指定|目标)|(?:确认|指定|目标).*回合/,
      /(?:turn.*(?:confirm|target|identif)|(?:confirm|target|identif).*turn)/i,
    ],
    ["interrupt_confirmation_required", /确认.*停止|停止.*确认/, /confirm.*stop|stop.*confirm/i],
    ["stop_desktop_rejected", /停止.*拒绝|拒绝.*停止/, /stop.*reject|reject.*stop/i],
    ["stop_disconnected", /断开|断连|连接/, /disconnect|connection/i],
    ["stop_stale_context", /上下文|过期|变化/, /context|stale|changed/i],
    [
      "stop_claim_unavailable",
      /(?:停止|操作).*(?:无法|不可|失败)|(?:无法|不可|失败).*(?:停止|操作)/,
      /(?:stop|operation).*(?:unavailable|fail|unable)|(?:unavailable|fail|unable).*(?:stop|operation)/i,
    ],
  ])("queue_resolution_reason_stop_and_state_failures_are_distinct %s", async (reason, zh, en) => {
    for (const [lang, expected] of [
      ["zh", zh],
      ["en", en],
    ] as const) {
      await i18n.changeLanguage(lang);
      const note = await submit({
        result: "not_sent",
        actionId: "a1",
        reasonCode: reason,
        receipt: null,
        queued: true,
      });
      expect(note.textContent).toMatch(expected);
      expect(note.textContent).toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
      expect(note.textContent).not.toContain(reason);
      cleanup();
    }
  });

  it.each(["stop_timeout", "stop_state_unavailable", "front_budget_elapsed"])(
    "queue_resolution_reason_unknown_never_invites_resend %s",
    async (reason) => {
      for (const lang of ["zh", "en"]) {
        await i18n.changeLanguage(lang);
        const note = await submit({ result: "unknown", actionId: "a1", reasonCode: reason });
        expect(note.textContent).toMatch(lang === "zh" ? /未知/ : /unknown/i);
        expect(note.textContent).toMatch(
          lang === "zh"
            ? /桌面.*核对|核对.*桌面/
            : /(?:desktop.*(?:check|verif)|(?:check|verif).*desktop)/i
        );
        expect(note.textContent).toMatch(
          lang === "zh" ? /不会自动重试|不自动重发/ : /no automatic retry|will not automatically/i
        );
        expect(note.textContent).not.toMatch(
          lang === "zh" ? /直接重发|立即重试/ : /resend now|retry immediately/i
        );
        expect(prioritizeBodies()).toHaveLength(1);
        cleanup();
      }
    }
  );

  it.each([
    [500, { error: "internal" }, /失败|错误/, /fail|error/i],
    [403, { error: "forbidden" }, /权限|设备|拒绝/, /permission|device|forbidden|denied/i],
    [503, { error: "capability_unavailable" }, /能力|不可用/, /capability|unavailable/i],
  ])(
    "queue_resolution_reason_stop_and_state_failures_are_distinct HTTP %s",
    async (status, body, zh, en) => {
      for (const [lang, expected] of [
        ["zh", zh],
        ["en", en],
      ] as const) {
        await i18n.changeLanguage(lang);
        installFetch();
        routes.prioritizeStatus = status;
        routes.prioritizeErrorBody = body;
        routes.queue = [queueView([action()]), queueView([action()])];
        await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
        fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
        fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
        const note = await screen.findByTestId("codex-queue-note");
        expect(note.textContent).toMatch(expected);
        expect(note.textContent).not.toMatch(
          lang === "zh"
            ? /仍在队列|已优先发送|数据库|账本/
            : /still queued|sent this one first|database|ledger|sql/i
        );
        expect(prioritizeBodies()).toHaveLength(1);
        cleanup();
      }
    }
  );

  // Unknown 响应本身没有 queued/receipt；队列状态只能来自操作后的 GET。
  // front_budget_elapsed 既可来自停止等待，也可来自消息投递，不能仅凭该码猜阶段。
  it.each([
    ["stop_timeout", "zh"],
    ["stop_timeout", "en"],
    ["stop_state_unavailable", "zh"],
    ["stop_state_unavailable", "en"],
    ["front_budget_elapsed", "zh"],
    ["front_budget_elapsed", "en"],
  ])(
    "queue_resolution_reason_unknown_never_invites_resend queued snapshot %s %s",
    async (reason, lang) => {
      await i18n.changeLanguage(lang);
      await submit({ result: "unknown", actionId: "a1", reasonCode: reason });
      await waitFor(() => expect(queueGetCalls()).toBeGreaterThanOrEqual(2));
      const note = screen.getByTestId("codex-queue-note").textContent ?? "";
      if (reason !== "front_budget_elapsed") {
        expect(note).toMatch(
          lang === "zh"
            ? /停止.*(?:未知|待确认|无法确认)|(?:未知|待确认).*停止/
            : /stop.*(?:unknown|uncertain|unconfirmed)|(?:unknown|uncertain).*stop/i
        );
      } else {
        expect(note).toMatch(
          lang === "zh" ? /未知|未确认|无法确认/ : /unknown|uncertain|unconfirmed/i
        );
      }
      expect(note).toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
      expect(note).toMatch(
        lang === "zh"
          ? /桌面.*核对|核对.*桌面/
          : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
      );
      expect(note).toMatch(
        lang === "zh" ? /不会自动重试|不自动重发/ : /no automatic retry|will not automatically/i
      );
      expect(note).not.toMatch(
        lang === "zh"
          ? /(?:消息|这条|优先发送).*(?:可能已发送|可能已生效|可能已送达)/
          : /(?:message|prioritized send).*(?:may have been sent|may have gone through|may.*taken effect)/i
      );
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  it.each([
    ["front_budget_elapsed", "zh"],
    ["front_budget_elapsed", "en"],
    ["client_message_unverified", "zh"],
    ["client_message_unverified", "en"],
  ])(
    "queue_resolution_reason_unknown_never_invites_resend empty queue remains visible %s %s",
    async (reason, lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: reason };
      routes.queue = [queueView([action()]), queueView([]), queueView([])];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      expect(prioritizeBodies()).toHaveLength(0);
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      await waitFor(() => expect(queueGetCalls()).toBeGreaterThanOrEqual(2));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(
        lang === "zh" ? /未知|未确认|无法确认/ : /unknown|uncertain|unconfirmed/i
      );
      expect(note).toMatch(
        lang === "zh"
          ? /桌面.*核对|核对.*桌面/
          : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
      );
      expect(note).toMatch(
        lang === "zh" ? /不会自动重试|不自动重发/ : /no automatic retry|will not automatically/i
      );
      expect(note).not.toMatch(
        lang === "zh"
          ? /未(?:继续)?发送|没有发送|已送达|仍在队列/
          : /(?:was|were) not sent|already delivered|still queued/i
      );
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  it.each(["zh", "en"])(
    "queue_resolution_reason_unknown_never_invites_resend stop reason survives failed queue refresh %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: "stop_timeout" };
      routes.queue = [queueView([action()])];
      routes.queueFailAfterInitial = true;
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      await waitFor(() => expect(queueGetCalls()).toBeGreaterThanOrEqual(2));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(
        lang === "zh"
          ? /停止.*(?:未知|待确认|无法确认)|(?:未知|待确认).*停止/
          : /stop.*(?:unknown|uncertain|unconfirmed)|(?:unknown|uncertain).*stop/i
      );
      expect(note).toMatch(
        lang === "zh"
          ? /桌面.*核对|核对.*桌面/
          : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
      );
      expect(note).not.toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  // Unknown DTO 不含 queued/receipt；GET 只列 queued，a1 缺席不证明其投递状态。
  // 重复合法快照只用于此用例观察文案，不改变其他用例对刷新次数的复现。
  it.each([
    ["front_budget_elapsed", "zh"],
    ["front_budget_elapsed", "en"],
    ["client_message_unverified", "zh"],
    ["client_message_unverified", "en"],
  ])(
    "queue_resolution_reason_unknown_never_invites_resend generic does not assert not sent %s %s",
    async (reason, lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: reason };
      const remaining = queueView([action({ actionId: "a2", position: 1 })]);
      routes.queue = [queueView([action()]), remaining, remaining, remaining];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      expect(prioritizeBodies()).toHaveLength(0);
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      await waitFor(() => expect(queueGetCalls()).toBeGreaterThanOrEqual(2));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(
        lang === "zh" ? /未知|未确认|无法确认/ : /unknown|uncertain|unconfirmed/i
      );
      expect(note).toMatch(
        lang === "zh"
          ? /桌面.*核对|核对.*桌面/
          : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
      );
      expect(note).toMatch(
        lang === "zh" ? /不会自动重试|不自动重发/ : /no automatic retry|will not automatically/i
      );
      if (reason === "client_message_unverified")
        expect(note).not.toMatch(/预算|budget|timed? out|timeout|超时/i);
      expect(note).not.toMatch(
        lang === "zh"
          ? /未(?:继续)?发送|没有发送|尚未发送|零发送|未送达/
          : /(?:was|were|has|have) not sent|no (?:message )?(?:was )?sent|never sent/i
      );
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  it.each(["unknown_delivery", "front_budget_elapsed"])(
    "queue_resolution_reason_unknown_never_invites_resend message delivery unknown %s",
    async (reason) => {
      for (const lang of ["zh", "en"]) {
        await i18n.changeLanguage(lang);
        installFetch();
        routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: reason };
        routes.queue = [
          queueView([action()]),
          queueView([action({ actionId: "a2", position: 1 })], "native_delivery_unknown", ["a1"]),
        ];
        await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
        fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
        expect(prioritizeBodies()).toHaveLength(0);
        fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
        await waitFor(() => expect(queueGetCalls()).toBeGreaterThanOrEqual(2));
        const note = screen.getByTestId("codex-queue-note").textContent ?? "";
        expect(note).toMatch(
          lang === "zh" ? /未知|未确认|无法确认/ : /unknown|uncertain|unconfirmed/i
        );
        expect(note).toMatch(
          lang === "zh"
            ? /桌面.*核对|核对.*桌面/
            : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
        );
        expect(note).not.toMatch(
          lang === "zh"
            ? /仍在队列|直接重发|立即重试|已接受|未(?:继续)?发送|没有发送/
            : /still queued|resend now|retry immediately|accepted|(?:was|were) not sent/i
        );
        expect(prioritizeBodies()).toHaveLength(1);
        cleanup();
      }
    }
  );

  it.each([
    ["front_budget_elapsed", "zh", /超时|等待.*(?:到期|结束)/],
    ["front_budget_elapsed", "en", /tim(?:e|ed) out|timeout|budget.*(?:elapsed|expired)/i],
    [
      "stop_not_effective",
      "zh",
      /(?:未|无法|尚未).*(?:确认|证明).*(?:目标|回合).*(?:停止|结束)|(?:目标|回合).*(?:停止|结束).*(?:未|无法|尚未).*(?:确认|证明)/,
    ],
    [
      "stop_not_effective",
      "en",
      /(?:could not|cannot|not|unconfirmed).*(?:confirm|verify).*(?:turn|target).*(?:stopp|end)|(?:turn|target).*(?:stopp|end).*(?:not|unconfirmed)/i,
    ],
    ["store_error", "zh", /存储|保存|服务.*失败/],
    ["store_error", "en", /stor(?:age|e)|sav(?:e|ing)|service.*(?:fail|error)/i],
    ["stop_device_revoked", "zh", /设备.*(?:撤销|失效|权限)|(?:撤销|失效|权限).*设备/],
    [
      "stop_device_revoked",
      "en",
      /device.*(?:revok|permission|invalid)|(?:revok|permission|invalid).*device/i,
    ],
    [
      "stop_capability_unavailable",
      "zh",
      /(?:停止|打断).*(?:能力|不可用)|(?:能力|不可用).*(?:停止|打断)/,
    ],
    [
      "stop_capability_unavailable",
      "en",
      /(?:stop|interrupt).*(?:capability|unavailable)|(?:capability|unavailable).*(?:stop|interrupt)/i,
    ],
  ])(
    "queue_resolution_reason_stop_and_state_failures_are_distinct not_sent %s %s",
    async (reason, lang, expected) => {
      await i18n.changeLanguage(lang);
      const note = await submit({
        result: "not_sent",
        actionId: "a1",
        reasonCode: reason,
        receipt: null,
        queued: true,
      });
      const text = note.textContent ?? "";
      expect(text).toMatch(expected);
      expect(text).toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
      expect(text).not.toContain(reason);
      expect(text).not.toMatch(/sql|database|ledger|数据库|账本/i);
      if (reason === "front_budget_elapsed")
        expect(text).not.toMatch(
          lang === "zh"
            ? /停止.*(?:未知|不确定)|(?:未知|不确定).*停止/
            : /stop.*(?:unknown|uncertain)|(?:unknown|uncertain).*stop/i
        );
      if (reason === "stop_not_effective")
        expect(text).not.toMatch(lang === "zh" ? /仍在运行|还在运行/ : /still running/i);
    }
  );

  it.each([
    ["queued", "zh"],
    ["queued", "en"],
    ["grant_consumed", "zh"],
    ["grant_consumed", "en"],
  ])("queue_resolution_reason_not_sent_keeps_actual_queue_state %s %s", async (reason, lang) => {
    await i18n.changeLanguage(lang);
    const note = await submit({
      result: "not_sent",
      actionId: "a1",
      reasonCode: reason,
      receipt: null,
      queued: true,
    });
    const text = note.textContent ?? "";
    expect(text).toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
    expect(text).not.toMatch(/grant_consumed|[（(]\s*queued\s*[）)]/i);
    if (lang === "zh") expect(text).not.toContain("queued");
    expect(text).not.toMatch(
      lang === "zh"
        ? /已发送|已送达|已接受|停止结果|停止未知|设备.*(?:撤销|权限|失效)|授权(?:已|必然).*(?:消费|使用)/
        : /(?:has been|was|already) (?:sent|delivered|accepted)|stop result|stop.*unknown|device.*(?:revoked|permission|invalid)|(?:grant|authorization).*(?:already|definitely).*(?:consumed|used)/i
    );
    if (reason === "grant_consumed") {
      expect(text).toMatch(
        lang === "zh"
          ? /(?:无法|不能|未能).*(?:优先|继续)|(?:优先|请求).*(?:无法|不能|未能)/
          : /(?:unable|cannot|could not|not).*(?:priorit|continu)|(?:priorit|request).*(?:unable|cannot|could not)/i
      );
      expect(text).toMatch(lang === "zh" ? /核对|确认/ : /check|verif/i);
    }
    expect(prioritizeBodies()).toHaveLength(1);
  });

  it.each(["zh", "en"])(
    "queue_resolution_reason_not_sent_keeps_actual_queue_state device_revoked receipt %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      const receipt = {
        actionId: "a1",
        delivery: "not_sent" as const,
        route: "none" as const,
        turnId: null,
        envelopeId: null,
        reasonCode: "device_revoked",
      };
      const note = await submit(
        {
          result: "not_sent",
          actionId: "a1",
          reasonCode: "device_revoked",
          receipt,
          queued: false,
        },
        action({ actionId: "a2", position: 1 })
      );
      expect(note.textContent).toMatch(
        lang === "zh"
          ? /设备.*(?:撤销|失效|权限)|(?:撤销|失效|权限).*设备/
          : /device.*(?:revok|permission|invalid)|(?:revok|permission|invalid).*device/i
      );
      expect(note.textContent).not.toMatch(
        /device_revoked|仍在队列|已接受|已优先发送|still queued|accepted|sent this one first/i
      );
    }
  );

  it.each(["zh", "en"])(
    "queue_resolution_reason_stop_and_state_failures_are_distinct HTTP capability without disconnect inference %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeStatus = 503;
      routes.prioritizeErrorBody = { error: "capability_unavailable" };
      routes.queue = [queueView([action()]), queueView([action()])];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(
        lang === "zh"
          ? /(?:远程|桌面).*(?:停止|打断).*(?:不可用|未启用|无法)/
          : /(?:remote|desktop).*(?:stop|interrupt).*(?:unavailable|not available|unable)/i
      );
      expect(note).not.toMatch(
        lang === "zh" ? /断开|断连|连接中断/ : /disconnect|connection.*(?:lost|broken)/i
      );
    }
  );

  it.each(["zh", "en"])(
    "queue_resolution_reason_stop_and_state_failures_are_distinct HTTP internal %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeStatus = 500;
      routes.prioritizeErrorBody = { error: "internal" };
      routes.queue = [queueView([action()]), queueView([action()])];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(
        lang === "zh"
          ? /服务.*(?:失败|错误)|(?:失败|错误).*服务/
          : /(?:service|server).*(?:fail|error)|(?:fail|error).*(?:service|server)/i
      );
      expect(note).toMatch(lang === "zh" ? /核对|确认/ : /check|verif/i);
      expect(note).not.toMatch(
        /internal|sql|database|ledger|数据库|账本|仍在队列|未发送|still queued|not sent/i
      );
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  it.each([
    ["unknown_stop", "zh"],
    ["unknown_stop", "en"],
    ["not_sent", "zh"],
    ["not_sent", "en"],
    ["http_error", "zh"],
    ["http_error", "en"],
  ])("queue_resolution_reason_post_feedback_survives_hung_refresh %s %s", async (kind, lang) => {
    await i18n.changeLanguage(lang);
    installFetch();
    routes.queue = [queueView([action()])];
    if (kind === "unknown_stop") {
      routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: "stop_timeout" };
    } else if (kind === "not_sent") {
      routes.prioritizeBody = {
        result: "not_sent",
        actionId: "a1",
        reasonCode: "action_cancelled",
        receipt: null,
        queued: false,
      };
    } else {
      routes.prioritizeStatus = 500;
      routes.prioritizeErrorBody = { error: "internal" };
    }
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    expect(queueGetCalls()).toBe(1);
    routes.queueHang = true; // 仅挂起 POST 收尾 GET；挂载 GET 已成功。
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    expect(prioritizeBodies()).toHaveLength(0);
    vi.useFakeTimers(); // 确保收尾 GET 创建时，其期限也受假时钟控制。
    await act(async () => {
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      for (let i = 0; i < 20; i += 1) await Promise.resolve();
    });
    expect(queueGetCalls()).toBe(2);
    expect(releaseQueue).not.toBeNull(); // 确认 GET 真正挂起，而非缓慢完成。
    expect(prioritizeBodies()).toHaveLength(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    const note = screen.getByTestId("codex-queue-note").textContent ?? "";
    if (kind === "unknown_stop") {
      expect(note).toMatch(
        lang === "zh"
          ? /停止.*(?:未知|待确认|无法确认)|(?:未知|待确认).*停止/
          : /stop.*(?:unknown|uncertain|unconfirmed)|(?:unknown|uncertain).*stop/i
      );
      expect(note).toMatch(
        lang === "zh"
          ? /桌面.*核对|核对.*桌面/
          : /desktop.*(?:check|verif)|(?:check|verif).*desktop/i
      );
      expect(note).toMatch(
        lang === "zh" ? /不会自动重试|不自动重发/ : /no automatic retry|will not automatically/i
      );
    } else if (kind === "not_sent") {
      expect(note).toMatch(lang === "zh" ? /撤回|取消/ : /withdrawn|cancelled|canceled/i);
    } else {
      expect(note).toMatch(
        lang === "zh"
          ? /服务.*(?:失败|错误)|(?:失败|错误).*服务/
          : /(?:service|server).*(?:fail|error)|(?:fail|error).*(?:service|server)/i
      );
    }
    expect(note).not.toMatch(lang === "zh" ? /仍在队列/ : /still queued/i);
    expect((screen.getByTestId("codex-queue-prioritize-a1") as HTMLButtonElement).disabled).toBe(
      false
    );
    expect((screen.getByTestId("codex-queue-retract-a1") as HTMLButtonElement).disabled).toBe(
      false
    );
    expect(prioritizeBodies()).toHaveLength(1);
    expect(queueGetCalls()).toBe(2);
  });

  it("queue_resolution_reason_post_feedback_survives_hung_refresh late GET cannot replace the current view or note", async () => {
    installFetch();
    routes.queue = [queueView([action()])];
    routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: "stop_timeout" };
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    routes.queueHang = true;
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    vi.useFakeTimers();
    await act(async () => {
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      for (let i = 0; i < 20; i += 1) await Promise.resolve();
    });
    expect(queueGetCalls()).toBe(2);
    const release = releaseQueue;
    expect(release).not.toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    const note = screen.getByTestId("codex-queue-note").textContent;
    expect(note).toMatch(/停止.*(?:未知|待确认|无法确认)|(?:未知|待确认).*停止/);
    await act(async () => {
      release!(queueView([action({ actionId: "a2", position: 1 })]));
      for (let i = 0; i < 20; i += 1) await Promise.resolve();
    });
    expect(screen.getByTestId("codex-queue-note").textContent).toBe(note);
    expect(screen.getByTestId("codex-queue-row-a1")).toBeTruthy();
    expect(screen.queryByTestId("codex-queue-row-a2")).toBeNull();
    expect(prioritizeBodies()).toHaveLength(1);
    expect(queueGetCalls()).toBe(2);
  });

  it("queue_resolution_reason_post_feedback_survives_hung_refresh old GET cannot unlock a newer in-flight operation", async () => {
    installFetch();
    routes.queue = [queueView([action(), action({ actionId: "a2", position: 2 })])];
    routes.prioritizeBody = { result: "unknown", actionId: "a1", reasonCode: "stop_timeout" };
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    routes.queueHang = true;
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    vi.useFakeTimers();
    await act(async () => {
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      for (let i = 0; i < 20; i += 1) await Promise.resolve();
    });
    expect(queueGetCalls()).toBe(2);
    const release = releaseQueue;
    expect(release).not.toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(screen.getByTestId("codex-queue-note").textContent).toMatch(
      /停止.*(?:未知|待确认|无法确认)|(?:未知|待确认).*停止/
    );
    routes.prioritizeHang = true;
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-a2"));
    fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a2"));
    expect(prioritizeBodies()).toHaveLength(2); // 对另一条明确操作，POST 保持在途。
    expect((screen.getByTestId("codex-queue-prioritize-a2") as HTMLButtonElement).disabled).toBe(
      true
    );
    await act(async () => {
      release!(queueView([action({ actionId: "a3", position: 1 })]));
      for (let i = 0; i < 20; i += 1) await Promise.resolve();
    });
    expect((screen.getByTestId("codex-queue-prioritize-a2") as HTMLButtonElement).disabled).toBe(
      true
    );
    expect(prioritizeBodies()).toHaveLength(2);
    expect(queueGetCalls()).toBe(2);
  });

  it.each(["zh", "en"])(
    "queue_resolution_reason_stop_and_state_failures_are_distinct network failure %s",
    async (lang) => {
      await i18n.changeLanguage(lang);
      installFetch();
      routes.prioritizeNetworkError = true;
      routes.queue = [queueView([action()]), queueView([action()])];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
      fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
      fireEvent.click(screen.getByTestId("codex-queue-prioritize-yes-a1"));
      const note = (await screen.findByTestId("codex-queue-note")).textContent ?? "";
      expect(note).toMatch(lang === "zh" ? /网络|连接/ : /network|connection/i);
      expect(note).toMatch(lang === "zh" ? /核对|确认/ : /check|verif/i);
      expect(note).not.toMatch(
        /MOBILE_NETWORK_FAIL|offline|仍在队列|未发送|still queued|not sent/i
      );
      expect(prioritizeBodies()).toHaveLength(1);
    }
  );

  it("queue_resolution_reason_double_click_posts_once", async () => {
    installFetch();
    routes.prioritizeHang = true;
    routes.queue = [queueView([action()])];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);
    fireEvent.click(await screen.findByTestId("codex-queue-prioritize-a1"));
    const yes = screen.getByTestId("codex-queue-prioritize-yes-a1");
    fireEvent.click(yes);
    fireEvent.click(yes);
    expect(prioritizeBodies()).toHaveLength(1);
    expect(interruptBodies()).toHaveLength(0);
  });
});

describe("CodexQueuePanel stop pause", () => {
  const paused = queueView([action()], null, [], true, true);
  const automaticPause = { ...paused, stopPauseAutoResumes: true };

  it("describes an automatically released stop pause as temporary", async () => {
    installFetch();
    routes.queue = [automaticPause];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);

    expect(screen.getByTestId("codex-queue-summary").textContent).toMatch(/临时|暂时|短暂|暂缓/);
    expect(screen.getByTestId("codex-queue-paused").textContent).toMatch(/自动(?:恢复|继续)/);
  });

  it("offers manual resume only when a stop pause persists", async () => {
    vi.useFakeTimers();
    installFetch();
    routes.queue = [automaticPause];
    await act(async () => {
      render(<CodexQueuePanel sessionId="s1" />);
    });

    expect(screen.queryByTestId("codex-queue-resume")).toBeNull();
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(screen.getByTestId("codex-queue-resume")).toBeVisible();
  });

  it.each([
    { name: "mixed pause", view: { ...paused, stopPauseAutoResumes: false } },
    { name: "legacy pause", view: paused },
  ])("retains manual recovery for a $name", async ({ view }) => {
    installFetch();
    routes.queue = [view];
    await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);

    expect(screen.getByTestId("codex-queue-summary").textContent).toContain("已暂停");
    expect(screen.getByTestId("codex-queue-paused").textContent).toContain("等待你的选择");
    expect(screen.getByTestId("codex-queue-resume")).toBeTruthy();
  });

  it("P1-LOCAL-QUEUE：断联模式仅保留编辑回草稿与撤回，不提供恢复派发、人工解除或插队", async () => {
    installFetch();
    const disconnectedView = queueView(
      [action({ actionId: "q-local-1", state: "queued", position: 1 })],
      "native_delivery_unknown",
      ["act-unk"],
      true,
      true
    );
    routes.queue = [disconnectedView];
    const onEditDraft = vi.fn();
    await renderLoadedPanel(
      <CodexQueuePanel sessionId="s1" onEditDraft={onEditDraft} disconnected />
    );

    expect(screen.getByTestId("codex-queue-row-q-local-1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-edit-q-local-1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-retract-q-local-1")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-local-paused")).toBeTruthy();
    expect(screen.queryByTestId("codex-queue-resume")).toBeNull();
    expect(screen.queryByTestId("codex-queue-force-continue")).toBeNull();
    expect(screen.queryByTestId("codex-queue-prioritize-q-local-1")).toBeNull();
  });

  // 线程级暂停只存在于本设备 actions=[] 时，空队列自隐不得吞掉唯一的人工出口。
  it.each([
    { name: "no barrier code", blockedReasonCode: null },
    { name: "native turn pending", blockedReasonCode: "native_turn_pending" },
  ])(
    "shows manual Resume for a thread-level pause with an empty queue and $name",
    async ({ blockedReasonCode }) => {
      installFetch();
      routes.queue = [
        {
          ...queueView([], blockedReasonCode, [], true, true),
          stopPauseAutoResumes: false,
        },
      ];
      await renderLoadedPanel(<CodexQueuePanel sessionId="s1" />);

      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-paused")).toBe("true");
      expect(screen.getByTestId("codex-queue-paused").textContent).toContain("等待你的选择");
      expect(screen.getByTestId("codex-queue-resume")).toBeVisible();
    }
  );

  it("keeps an auto-resuming stop pause with an empty queue hidden", async () => {
    installFetch();
    routes.queue = [
      {
        ...queueView([], null, [], true, true),
        stopPauseAutoResumes: true,
      },
    ];
    render(<CodexQueuePanel sessionId="s1" />);
    await act(async () => {});

    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    expect(screen.queryByTestId("codex-queue-resume")).toBeNull();
  });

  it("stops batch force-continue immediately when disconnected flips true mid-loop", async () => {
    installFetch();
    routes.queue = [
      queueView([], "native_delivery_unknown", ["b1", "b2"], false, false),
      queueView([], "native_delivery_unknown", ["b2"], false, false),
    ];
    let resolveFirst!: (res: Response) => void;
    const forceCalls: string[] = [];
    const baseFetch = globalThis.fetch;
    globalThis.fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url === "/m/api/v1/codex/queue/force-continue") {
        forceCalls.push(String(init?.body ?? ""));
        if (forceCalls.length === 1) {
          return new Promise<Response>((resolve) => {
            resolveFirst = resolve;
          });
        }
      }
      return baseFetch(input, init);
    }) as typeof fetch;

    const { rerender } = render(<CodexQueuePanel sessionId="s1" />);
    await screen.findByTestId("codex-queue-force-continue");
    fireEvent.click(screen.getByTestId("codex-queue-force-continue"));
    fireEvent.click(screen.getByTestId("codex-queue-force-yes"));
    await waitFor(() => expect(forceCalls).toHaveLength(1));

    rerender(<CodexQueuePanel sessionId="s1" disconnected />);
    await act(async () => {
      resolveFirst(
        new Response(
          JSON.stringify({ actionId: "b1", state: "accepted", position: null, receipt: null }),
          { status: 200, headers: { "Content-Type": "application/json" } }
        )
      );
    });
    await act(async () => {});

    expect(forceCalls).toHaveLength(1);
  });
});
