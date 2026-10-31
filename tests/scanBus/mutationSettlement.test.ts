// F5 applied=false 的 epoch 门控（2026-10-31 定案）：
//  * 旧快照（epoch ≤ request_epoch）回写不得让卡片复活，也不得触发收口；
//  * 只有 epoch > request_epoch 的 scan-bus-snapshot 才 invalidate + 清 pending；
//  * 快照事件自带完整 SessionsResponse，先写缓存再清 pending（避免一帧复活）；
//  * 10s 超时强制收口并 invalidate（回滚可见）；
//  * 命令失败立即 invalidate + 清 pending（回滚）；
//  * 重复点击：等待器可被新票根取代/取消，不泄漏；旧命令结算不得清新 pending。
import { QueryClient } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type Listener = (e: { payload: unknown }) => void;

const listeners = new Map<string, Listener[]>();

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, cb: Listener) => {
    const arr = listeners.get(name) ?? [];
    arr.push(cb);
    listeners.set(name, arr);
    return () => {
      listeners.set(
        name,
        (listeners.get(name) ?? []).filter((f) => f !== cb)
      );
    };
  },
}));

import {
  SESSIONS_QUERY_KEY,
  __resetSessionMutationWaitersForTest,
  applyOptimisticSessionMutation,
  applyPendingSessions,
  clearPendingSessions,
  noteAppliedSnapshotEpoch,
  settleSessionMutation,
} from "@/lib/query/optimisticSessions";
import type { Session, SessionsResponse } from "@/types/session";

const mk = (over: Partial<Session>): Session => ({
  id: "g1",
  agentType: "codex",
  projectName: "项目A",
  projectPath: "/a",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "finished",
  lastMessage: null,
  lastMessageRole: null,
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "app",
  jumpSupported: true,
  unread: true,
  ...over,
});

const seed = (sessions: Session[]): SessionsResponse => ({
  sessions,
  totalCount: sessions.length,
  waitingCount: sessions.filter((s) => s.status === "waiting").length,
});

const cache = (qc: QueryClient) => qc.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;

/** 真实定时器下的轮询等待（用例只等待微任务/短异步） */
async function until(cond: () => boolean, timeoutMs = 1000): Promise<void> {
  const started = Date.now();
  while (!cond()) {
    if (Date.now() - started > timeoutMs) throw new Error("until: 条件超时");
    await new Promise((r) => setTimeout(r, 10));
  }
}

function emit(name: string, payload: unknown): void {
  for (const cb of listeners.get(name) ?? []) cb({ payload });
}

const green = mk({ id: "g1", agentType: "codex", status: "finished", form: "app", unread: true });
const keep = mk({ id: "keep", agentType: "claude", status: "idle", unread: false });
const TARGETS = [{ id: "g1", agentType: "codex" }];

beforeEach(() => {
  listeners.clear();
  clearPendingSessions(TARGETS);
  clearPendingSessions([{ id: "r1", agentType: "claude" }]);
});

afterEach(() => {
  __resetSessionMutationWaitersForTest();
  vi.useRealTimers();
});

describe("settleSessionMutation / applied=false 的 epoch 门控", () => {
  it("旧快照回写与 epoch≤request 的事件都不收口，epoch 更大才收口", async () => {
    const qc = new QueryClient();
    const oldSnapshot = seed([green, keep]);
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    const ticket = applyOptimisticSessionMutation(qc, [green])!;
    expect(cache(qc).sessions.map((s) => s.id)).toEqual(["keep"]);

    await settleSessionMutation(qc, ticket, { applied: false, version: 3, epoch: 5 });
    await until(() => listeners.get("scan-bus-snapshot")?.length === 1);

    // ① 第一次 invalidate/轮询回写旧快照：pending 拦住，卡片不复活
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    expect(applyPendingSessions(cache(qc).sessions).map((s) => s.id)).toEqual(["keep"]);
    expect(invalidate).not.toHaveBeenCalled();

    // ② epoch 不够（== request）的事件：忽略，继续等待
    emit("scan-bus-snapshot", { epoch: 5, data: oldSnapshot });
    expect(invalidate).not.toHaveBeenCalled();
    expect(applyPendingSessions(cache(qc).sessions).map((s) => s.id)).toEqual(["keep"]);

    // ③ epoch 更大的事件：载荷写缓存 + invalidate + 清 pending（监听解除）
    emit("scan-bus-snapshot", { epoch: 6, data: seed([keep]) });
    await until(() => invalidate.mock.calls.length === 1);
    await until(() => applyPendingSessions(cache(qc).sessions).length === 1);
    expect(cache(qc).sessions.map((s) => s.id)).toEqual(["keep"]);
    await until(() => (listeners.get("scan-bus-snapshot") ?? []).length === 0);
  });

  it("10s 超时强制收口：invalidate + 清 pending（回滚可见）", async () => {
    vi.useFakeTimers();
    const qc = new QueryClient();
    const oldSnapshot = seed([green, keep]);
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    const invalidate = vi.spyOn(qc, "invalidateQueries");

    const ticket = applyOptimisticSessionMutation(qc, [green])!;
    await settleSessionMutation(qc, ticket, { applied: false, version: 1, epoch: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(invalidate).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(10_000);
    await vi.advanceTimersByTimeAsync(0);
    expect(invalidate).toHaveBeenCalledTimes(1);

    // 权威快照（旧数据）回写后 pending 已清：绿卡重新可见 = 回滚
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    expect(applyPendingSessions(cache(qc).sessions).map((s) => s.id)).toEqual(["g1", "keep"]);
  });

  it("applied=true：invalidate 回取权威快照后清 pending", async () => {
    const qc = new QueryClient();
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([green, keep]));
    const invalidate = vi.spyOn(qc, "invalidateQueries");
    const ticket = applyOptimisticSessionMutation(qc, [green])!;
    await settleSessionMutation(qc, ticket, { applied: true, version: 4, epoch: 4 });
    expect(invalidate).toHaveBeenCalledTimes(1);
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([keep]));
    expect(applyPendingSessions(cache(qc).sessions)).toHaveLength(1);
  });

  it("事件先于等待器注册到达（同步通道已应用）：立即收口，不空等 10s", async () => {
    vi.useFakeTimers();
    const qc = new QueryClient();
    /* 模拟 useScanSnapshotSync 已收到并写入 epoch=9 的权威快照（绿卡已剔除） */
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([keep]));
    const invalidate = vi.spyOn(qc, "invalidateQueries");
    const ticket = applyOptimisticSessionMutation(qc, [green])!;
    noteAppliedSnapshotEpoch(9);

    await settleSessionMutation(qc, ticket, { applied: false, version: 5, epoch: 8 });
    await vi.advanceTimersByTimeAsync(0);
    // 立即 invalidate + 清 pending（无需等事件或 10s 超时）
    expect(invalidate).toHaveBeenCalledTimes(1);
    expect(applyPendingSessions(cache(qc).sessions).map((s) => s.id)).toEqual(["keep"]);
    // 未注册监听器（无需等待事件）
    expect(listeners.get("scan-bus-snapshot") ?? []).toHaveLength(0);
  });

  it("命令失败（outcome=null）：invalidate + 清 pending（乐观更新回滚）", async () => {
    const qc = new QueryClient();
    const oldSnapshot = seed([green, keep]);
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    const invalidate = vi.spyOn(qc, "invalidateQueries");
    const ticket = applyOptimisticSessionMutation(qc, [green])!;
    expect(cache(qc).sessions).toHaveLength(1);

    await settleSessionMutation(qc, ticket, null);
    expect(invalidate).toHaveBeenCalledTimes(1);
    // invalidate 回取的权威快照（旧数据）+ pending 已清 → 卡片回滚可见
    qc.setQueryData(SESSIONS_QUERY_KEY, oldSnapshot);
    expect(applyPendingSessions(cache(qc).sessions).map((s) => s.id)).toEqual(["g1", "keep"]);
  });

  it("重复点击：新票根取代旧等待器，旧命令结算不清新 pending", async () => {
    const qc = new QueryClient();
    const unreadCard = mk({ id: "r1", agentType: "claude", status: "idle", unread: true });
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([unreadCard]));

    // 第一次点击：普通未读 → 只清未读（pending=read），applied=false 进入等待
    const first = applyOptimisticSessionMutation(qc, [unreadCard])!;
    await settleSessionMutation(qc, first, { applied: false, version: 1, epoch: 10 });
    await until(() => listeners.get("scan-bus-snapshot")?.length === 1);

    // 第二次点击（卡面已显示已读）：dismiss → pending=hidden；旧等待器被取代
    const second = applyOptimisticSessionMutation(qc, [{ ...unreadCard, unread: false }])!;
    await until(() => (listeners.get("scan-bus-snapshot") ?? []).length === 0);

    // 旧命令晚到结算（applied=true）：只清自己的票根，不得清掉新 pending
    await settleSessionMutation(qc, first, { applied: true, version: 2, epoch: 11 });
    expect(applyPendingSessions([unreadCard])).toHaveLength(0);

    // 新命令正常结算后才收口
    await settleSessionMutation(qc, second, { applied: true, version: 3, epoch: 12 });
    expect(applyPendingSessions([unreadCard])).toHaveLength(1);
  });
});
