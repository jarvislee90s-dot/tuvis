// G1：乐观隐藏生命周期单测——未读态 vs 整卡、数据驱动聚合绿卡、pending 防回写。
import { QueryClient } from "@tanstack/react-query";
import { beforeEach, describe, expect, it } from "vitest";
import {
  SESSIONS_QUERY_KEY,
  applyPendingSessions,
  clearPendingSessions,
  hideSessionOptimistically,
  hideSessionsOptimistically,
  isDataDrivenGreenCard,
  markPendingSession,
} from "@/lib/query/optimisticSessions";
import type { Session, SessionsResponse } from "@/types/session";

const mk = (over: Partial<Session>): Session => ({
  id: "s1",
  agentType: "workbuddy",
  projectName: "项目A",
  projectPath: "/a",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "waiting",
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
  unread: false,
  ...over,
});

const seed = (sessions: Session[]): SessionsResponse => ({
  sessions,
  totalCount: sessions.length,
  waitingCount: sessions.filter((s) => s.status === "waiting").length,
});

const cache = (qc: QueryClient) => qc.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;

beforeEach(() => {
  // 清空模块级 pending（每个用例独立）
  const qc = new QueryClient();
  qc.setQueryData(
    SESSIONS_QUERY_KEY,
    seed([mk({ id: "s1" }), mk({ id: "s2" }), mk({ id: "t1" }), mk({ id: "t2" }), mk({ id: "t3" })])
  );
  clearPendingSessions(
    ["s1", "s2", "t1", "t2", "t3"].map((id) => ({ id, agentType: "workbuddy" }))
  );
});

describe("optimisticSessions / G1 生命周期", () => {
  it("非未读卡：整卡从缓存移除并同步计数", () => {
    const qc = new QueryClient();
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([mk({}), mk({ id: "s2", status: "idle" })]));
    hideSessionOptimistically(qc, mk({}));
    const next = cache(qc);
    expect(next.sessions.map((s) => s.id)).toEqual(["s2"]);
    expect(next.totalCount).toBe(1);
  });

  it("普通未读卡：只把 unread 置 false（卡面保留）", () => {
    const qc = new QueryClient();
    const session = mk({ agentType: "claude", status: "idle", unread: true });
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([session]));
    hideSessionOptimistically(qc, session);
    const next = cache(qc);
    expect(next.sessions).toHaveLength(1);
    expect(next.sessions[0].unread).toBe(false);
    expect(applyPendingSessions(next.sessions)[0].unread).toBe(false);
  });

  it("数据驱动聚合绿卡（codex/zcode/dsh App idle/finished）已读 → 整卡移除", () => {
    for (const agentType of ["codex", "zcode", "dsh"] as const) {
      expect(isDataDrivenGreenCard(mk({ agentType, status: "finished", form: "app" }))).toBe(true);
      const qc = new QueryClient();
      const session = mk({ agentType, status: "finished", form: "app", unread: true });
      qc.setQueryData(SESSIONS_QUERY_KEY, seed([session]));
      hideSessionOptimistically(qc, session);
      expect(cache(qc).sessions).toHaveLength(0);
      clearPendingSessions([session]);
    }
    // CLI 形态 / 非绿灯：不算数据驱动聚合卡
    expect(isDataDrivenGreenCard(mk({ agentType: "codex", status: "idle", form: "cli" }))).toBe(
      false
    );
    expect(isDataDrivenGreenCard(mk({ agentType: "codex", status: "waiting", form: "app" }))).toBe(
      false
    );
    clearPendingSessions([{ id: "s1", agentType: "workbuddy" }]);
  });

  it("pending=read：轮询回写旧快照时未读态不复活", () => {
    const session = mk({ agentType: "claude", status: "idle", unread: true });
    markPendingSession(session, "read");
    const oldSnapshot = [mk({ agentType: "claude", status: "idle", unread: true })];
    expect(applyPendingSessions(oldSnapshot)[0].unread).toBe(false);
    clearPendingSessions([session]);
    expect(applyPendingSessions(oldSnapshot)[0].unread).toBe(true);
  });

  it("pending=hidden：轮询回写旧快照时整卡不复活", () => {
    const session = mk({});
    markPendingSession(session, "hidden");
    expect(applyPendingSessions([session])).toHaveLength(0);
    clearPendingSessions([session]);
    expect(applyPendingSessions([session])).toHaveLength(1);
  });

  it("批量：一次移除全部目标卡", () => {
    const qc = new QueryClient();
    qc.setQueryData(
      SESSIONS_QUERY_KEY,
      seed([mk({ id: "a" }), mk({ id: "b" }), mk({ id: "c", status: "idle" })])
    );
    hideSessionsOptimistically(qc, [
      { id: "a", agentType: "workbuddy" },
      { id: "b", agentType: "workbuddy" },
    ]);
    expect(cache(qc).sessions.map((s) => s.id)).toEqual(["c"]);
    clearPendingSessions([
      { id: "a", agentType: "workbuddy" },
      { id: "b", agentType: "workbuddy" },
    ]);
  });

  it("缓存缺失时安全空操作", () => {
    const qc = new QueryClient();
    expect(() => hideSessionOptimistically(qc, mk({ id: "x", agentType: "claude" }))).not.toThrow();
    clearPendingSessions([{ id: "x", agentType: "claude" }]);
  });
});
