// F5-opt 乐观隐藏缓存助手单测（用户感知 ≤1s 的口径：点击后缓存先变，命令后收敛）。
import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import {
  SESSIONS_QUERY_KEY,
  hideSessionOptimistically,
  hideSessionsOptimistically,
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

describe("optimisticSessions", () => {
  it("非未读卡：从缓存列表移除并同步计数", () => {
    const qc = new QueryClient();
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([mk({}), mk({ id: "s2", status: "idle" })]));
    hideSessionOptimistically(qc, { id: "s1", agentType: "workbuddy", unread: false });
    const next = qc.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;
    expect(next.sessions.map((s) => s.id)).toEqual(["s2"]);
    expect(next.totalCount).toBe(1);
    expect(next.waitingCount).toBe(0);
  });

  it("未读卡：只把 unread 置 false（已读语义，不删卡）", () => {
    const qc = new QueryClient();
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([mk({ unread: true })]));
    hideSessionOptimistically(qc, { id: "s1", agentType: "workbuddy", unread: true });
    const next = qc.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;
    expect(next.sessions).toHaveLength(1);
    expect(next.sessions[0].unread).toBe(false);
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
    const next = qc.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;
    expect(next.sessions.map((s) => s.id)).toEqual(["c"]);
    expect(next.totalCount).toBe(1);
  });

  it("缓存缺失时安全空操作", () => {
    const qc = new QueryClient();
    expect(() =>
      hideSessionOptimistically(qc, { id: "x", agentType: "claude", unread: false })
    ).not.toThrow();
  });
});
