import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useCodexTurnControl } from "@/mobile/useCodexTurnControl";
import {
  interruptCodexTurn,
  type InterruptTurnResult,
  type ThreadProjection,
} from "@/mobile/codexIpcApi";

// R02（D2 集中审查收口）：停止控制器两缺口的行为锁。
//  - 反例 A：共享 timerRef 被旧请求 finally 无条件清——A 会话停止挂起 → 切 B 会话
//    发起停止 → A 迟到完成，清掉 B 的 30 秒预算计时器 → B 永久 stopping。
//    timer 必须由「每次 stop 局部创建的 handle + 提交代际」共同拥有：timeout 回调
//    与 finally 都只清/判定自己的 handle。
//  - 资格门：stop() 开头检查 canStop 语义 + 冻结目标非空，不满足零请求直接返回
//    refused 结果（消费方不能只靠按钮隐藏）。

vi.mock("@/mobile/codexIpcApi", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/mobile/codexIpcApi")>();
  return { ...actual, interruptCodexTurn: vi.fn() };
});

/** 可停投影夹具（T03 三元组齐备：新鲜 + active + interrupt.allowed + 活动回合） */
function stopProjection(over: Record<string, unknown> = {}): ThreadProjection {
  return {
    threadId: "th-ctl",
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

const OK_RESULT: InterruptTurnResult = {
  ok: true,
  uncertain: false,
  interruptedTurnId: "turn-A",
  expectedTurnId: "turn-A",
  reasonCode: null,
};

/** 挂起的 interrupt 调用登记簿：按发起顺序放行（模拟 A/B 两个在飞请求） */
let pendingResolves: Array<(v: InterruptTurnResult) => void> = [];

/** interrupt 挂起在飞（计时器归属测试用：手动按序放行） */
function hangInterrupt() {
  vi.mocked(interruptCodexTurn).mockImplementation(
    () =>
      new Promise<InterruptTurnResult>((resolve) => {
        pendingResolves.push(resolve);
      })
  );
}

function interruptCalls(): Array<[string, string | undefined]> {
  return vi
    .mocked(interruptCodexTurn)
    .mock.calls.map(([sid, turnId]) => [sid, turnId] as [string, string | undefined]);
}

/** 冲刷挂起的微任务（resolve → await → setState 链） */
async function flushMicro() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

beforeEach(() => {
  pendingResolves = [];
  vi.useFakeTimers();
  // 缺省即时成功回执（资格门测试：门拦截时连调用都不该发生）
  vi.mocked(interruptCodexTurn).mockResolvedValue(OK_RESULT);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
});

describe("useCodexTurnControl：30 秒预算计时器归属（R02 反例 A）", () => {
  it("late_first_stop_never_clears_second_timer：A 停止挂起→切 B 会话发起停止→A 迟到完成，B 的 30s 计时器必须存活并到期退出 stopping", async () => {
    hangInterrupt();
    const { result, rerender } = renderHook(
      ({ sid }: { sid: string }) => useCodexTurnControl(stopProjection(), sid),
      { initialProps: { sid: "s1" } }
    );

    // 会话 A：发起停止（挂起在飞）
    let promiseA: Promise<unknown> = Promise.resolve();
    act(() => {
      promiseA = result.current.stop();
    });
    expect(result.current.stopping).toBe(true);
    expect(interruptCalls()).toEqual([["s1", "turn-A"]]);

    // 切到会话 B：控制器随会话复位（旧停止现场不带入），B 再发起停止（挂起在飞）
    rerender({ sid: "s2" });
    await act(async () => {});
    expect(result.current.stopping).toBe(false);
    let promiseB: Promise<unknown> = Promise.resolve();
    act(() => {
      promiseB = result.current.stop();
    });
    expect(result.current.stopping).toBe(true);
    expect(interruptCalls()).toEqual([
      ["s1", "turn-A"],
      ["s2", "turn-A"],
    ]);

    // A 的响应迟到完成：只能作废 A 自己（代际/会话哨兵），不得清掉 B 的预算计时器
    await act(async () => {
      pendingResolves[0](OK_RESULT);
      await promiseA;
    });
    await flushMicro();
    // A 迟到 = 对当前（B）现场零影响：stopping 仍由 B 持有
    expect(result.current.stopping).toBe(true);

    // B 的 30 秒预算到期：必须强制退出 stopping（旧实现此处计时器已被 A 清掉 → 永久 stopping）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_001);
    });
    expect(result.current.stopping).toBe(false);
    expect(result.current.result).toEqual({ kind: "uncertain", reasonCode: "timeout" });

    // B 的挂起响应此刻放行：迟到一律作废（预算已移交呈现）
    await act(async () => {
      pendingResolves[1]?.(OK_RESULT);
      await promiseB;
    });
    await flushMicro();
    expect(result.current.result).toEqual({ kind: "uncertain", reasonCode: "timeout" });
  });

  it("second_stop_timer_fires_after_first_cancelled：A 被会话切换作废后，B 计时器正常触发（正向对照）", async () => {
    hangInterrupt();
    const { result, rerender } = renderHook(
      ({ sid }: { sid: string }) => useCodexTurnControl(stopProjection(), sid),
      { initialProps: { sid: "s1" } }
    );
    act(() => {
      void result.current.stop();
    });
    rerender({ sid: "s2" });
    await act(async () => {});
    act(() => {
      void result.current.stop();
    });
    // 未放行任何响应：B 自己的预算到期即收尾
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_001);
    });
    expect(result.current.stopping).toBe(false);
    expect(result.current.result).toEqual({ kind: "uncertain", reasonCode: "timeout" });
  });
});

describe("useCodexTurnControl：stop() 资格门（R02）", () => {
  it("stale_projection_refused_no_target：投影视图 stale（冻结目标为空）→ 零请求返回 refused/no_target，不进入 stopping", async () => {
    const { result } = renderHook(() =>
      useCodexTurnControl(stopProjection({ stale: true, activeTurnId: "turn-A" }), "s1")
    );
    let res: Awaited<ReturnType<typeof result.current.stop>> = null;
    await act(async () => {
      res = await result.current.stop();
    });
    expect(res).toEqual({ kind: "refused", reason: "no_target" });
    expect(interruptCalls()).toEqual([]); // 零 POST（不带空 turnId 回退服务器当前回合）
    expect(result.current.stopping).toBe(false);
  });

  it("missing_target_refused_no_target：activeTurnId 缺失 → refused/no_target", async () => {
    const { result } = renderHook(() =>
      useCodexTurnControl(stopProjection({ activeTurnId: null }), "s1")
    );
    let res: Awaited<ReturnType<typeof result.current.stop>> = null;
    await act(async () => {
      res = await result.current.stop();
    });
    expect(res).toEqual({ kind: "refused", reason: "no_target" });
    expect(interruptCalls()).toEqual([]);
  });

  it("interrupt_not_allowed_refused：新鲜 + 运行中 + 有目标但 interrupt 未获准 → refused/not_allowed", async () => {
    const projection = stopProjection({
      effectiveOperations: {
        policyRevision: 1,
        operations: {
          interrupt: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: false,
            evidence: "observed_success",
            reasonCode: "cap_unverified",
          },
        },
      },
    });
    const { result } = renderHook(() => useCodexTurnControl(projection, "s1"));
    let res: Awaited<ReturnType<typeof result.current.stop>> = null;
    await act(async () => {
      res = await result.current.stop();
    });
    expect(res).toEqual({ kind: "refused", reason: "not_allowed" });
    expect(interruptCalls()).toEqual([]);
    expect(result.current.stopping).toBe(false);
  });

  it("not_running_refused：新鲜 + 有目标但回合非运行 → refused/not_allowed", async () => {
    const { result } = renderHook(() =>
      useCodexTurnControl(stopProjection({ runtimeStatus: "idle" }), "s1")
    );
    let res: Awaited<ReturnType<typeof result.current.stop>> = null;
    await act(async () => {
      res = await result.current.stop();
    });
    expect(res).toEqual({ kind: "refused", reason: "not_allowed" });
    expect(interruptCalls()).toEqual([]);
  });

  it("eligible_stop_still_sends_frozen_target：资格齐备时停止照常发出（冻结目标随请求）", async () => {
    hangInterrupt();
    const { result } = renderHook(() => useCodexTurnControl(stopProjection(), "s1"));
    let promise: Promise<unknown> = Promise.resolve();
    act(() => {
      promise = result.current.stop();
    });
    expect(interruptCalls()).toEqual([["s1", "turn-A"]]);
    await act(async () => {
      pendingResolves[0](OK_RESULT);
      await promise;
    });
    expect(result.current.result).toEqual({ kind: "interrupted", turnId: "turn-A" });
  });
});
