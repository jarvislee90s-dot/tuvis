// F5 卡片生命周期单测：单卡/批量共用同一套分类与乐观执行。
//  - getSessionDismissBehavior：普通未读 → markReadKeep；数据驱动聚合绿卡（未读）→
//    markReadAndRemove；其余 → dismiss；
//  - applyOptimisticSessionMutation：一次 setQueryData，计数只按实际移除数扣减；
//  - pending 拦截旧快照回写：read 保留卡面但未读不复活、hidden 整卡不复活。
import { QueryClient } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  SESSIONS_QUERY_KEY,
  applyOptimisticSessionMutation,
  applyPendingSessions,
  clearPendingSessions,
  getSessionDismissBehavior,
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

const ALL_TARGETS = [
  { id: "s1", agentType: "workbuddy" },
  { id: "s2", agentType: "workbuddy" },
  { id: "t1", agentType: "workbuddy" },
  { id: "g1", agentType: "codex" },
  { id: "r1", agentType: "claude" },
];

beforeEach(() => {
  clearPendingSessions(ALL_TARGETS);
});

describe("getSessionDismissBehavior（单卡/批量唯一分类）", () => {
  it("普通未读卡 → markReadKeep", () => {
    expect(
      getSessionDismissBehavior(mk({ agentType: "claude", status: "idle", unread: true }))
    ).toBe("markReadKeep");
  });

  it("数据驱动聚合绿卡（codex/zcode/dsh App idle/finished）未读 → markReadAndRemove", () => {
    for (const agentType of ["codex", "zcode", "dsh"] as const) {
      for (const status of ["idle", "finished"] as const) {
        const s = mk({ agentType, form: "app", status, unread: true });
        expect(isDataDrivenGreenCard(s)).toBe(true);
        expect(getSessionDismissBehavior(s)).toBe("markReadAndRemove");
      }
    }
    // CLI 形态 / 非绿态：数据驱动绿卡规则不成立（普通未读 → 保留）
    expect(
      getSessionDismissBehavior(
        mk({ agentType: "codex", form: "cli", status: "idle", unread: true })
      )
    ).toBe("markReadKeep");
    expect(
      getSessionDismissBehavior(
        mk({ agentType: "codex", form: "app", status: "waiting", unread: true })
      )
    ).toBe("markReadKeep");
  });

  it("已读卡（含绿卡）→ dismiss（整卡移除）", () => {
    expect(getSessionDismissBehavior(mk({ status: "waiting", unread: false }))).toBe("dismiss");
    expect(
      getSessionDismissBehavior(
        mk({ agentType: "codex", form: "app", status: "finished", unread: false })
      )
    ).toBe("dismiss");
  });
});

describe("applyOptimisticSessionMutation / 单卡", () => {
  it("普通未读卡：卡面保留、未读消失，计数不变", () => {
    const qc = new QueryClient();
    const session = mk({ id: "c1", agentType: "claude", status: "idle", unread: true });
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([session, mk({ id: "w1", unread: false })]));
    applyOptimisticSessionMutation(qc, [session]);
    const next = cache(qc);
    expect(next.sessions.map((s) => s.id)).toEqual(["c1", "w1"]);
    expect(next.sessions[0].unread).toBe(false);
    expect(next.totalCount).toBe(2);
    expect(next.waitingCount).toBe(1); // w1 waiting 未被扣减
    expect(applyPendingSessions(next.sessions)[0].unread).toBe(false);
    clearPendingSessions([session]);
  });

  it("数据驱动聚合绿卡：整卡消失，计数按移除数扣减", () => {
    const qc = new QueryClient();
    const green = mk({
      id: "g1",
      agentType: "codex",
      status: "finished",
      form: "app",
      unread: true,
    });
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([green, mk({ id: "w1" })]));
    applyOptimisticSessionMutation(qc, [green]);
    const next = cache(qc);
    expect(next.sessions.map((s) => s.id)).toEqual(["w1"]);
    expect(next.totalCount).toBe(1);
    expect(next.waitingCount).toBe(1);
    expect(applyPendingSessions(next.sessions)).toHaveLength(1);
    clearPendingSessions([green]);
  });
});

describe("applyOptimisticSessionMutation / 批量混合", () => {
  it("三类行为分别正确：普通未读保留已读、绿卡移除、已读卡移除", () => {
    const qc = new QueryClient();
    const ordinaryUnread = mk({
      id: "r1",
      agentType: "claude",
      status: "idle",
      unread: true,
      projectName: "普通未读",
    });
    const greenUnread = mk({
      id: "g1",
      agentType: "codex",
      status: "finished",
      form: "app",
      unread: true,
      projectName: "绿卡",
    });
    const readCard = mk({
      id: "d1",
      agentType: "workbuddy",
      status: "idle",
      unread: false,
      projectName: "已读",
    });
    qc.setQueryData(
      SESSIONS_QUERY_KEY,
      seed([ordinaryUnread, greenUnread, readCard, mk({ id: "keep", status: "waiting" })])
    );
    const write = vi.spyOn(qc, "setQueryData");

    applyOptimisticSessionMutation(qc, [ordinaryUnread, greenUnread, readCard]);
    // 批量按行为分组后只写一次缓存（N 张卡 ≠ N 次 setQueryData）
    expect(write).toHaveBeenCalledTimes(1);

    // 缓存：只剩普通未读（已读）+ 非目标卡；计数只扣实际移除的 2 张
    const next = cache(qc);
    expect(next.sessions.map((s) => s.id)).toEqual(["r1", "keep"]);
    expect(next.sessions[0].unread).toBe(false);
    expect(next.totalCount).toBe(2);
    expect(next.waitingCount).toBe(1);

    // 旧快照回写 + pending 派生：绿卡/已读卡不复活，普通未读未读态不复活
    qc.setQueryData(SESSIONS_QUERY_KEY, seed([ordinaryUnread, greenUnread, readCard]));
    const visible = applyPendingSessions(cache(qc).sessions);
    expect(visible.map((s) => s.id)).toEqual(["r1"]);
    expect(visible[0].unread).toBe(false);

    clearPendingSessions([ordinaryUnread, greenUnread, readCard]);
  });
});

describe("pending 防回写", () => {
  it("markPendingSession(read)：轮询回写旧快照时未读态不复活", () => {
    const session = mk({ agentType: "claude", status: "idle", unread: true });
    markPendingSession(session, "read");
    const oldSnapshot = [mk({ agentType: "claude", status: "idle", unread: true })];
    expect(applyPendingSessions(oldSnapshot)[0].unread).toBe(false);
    clearPendingSessions([session]);
    expect(applyPendingSessions(oldSnapshot)[0].unread).toBe(true);
  });

  it("markPendingSession(hidden)：轮询回写旧快照时整卡不复活", () => {
    const session = mk({});
    markPendingSession(session, "hidden");
    expect(applyPendingSessions([session])).toHaveLength(0);
    clearPendingSessions([session]);
    expect(applyPendingSessions([session])).toHaveLength(1);
  });

  it("目标为空 / 缓存缺失时安全空操作", () => {
    const qc = new QueryClient();
    expect(applyOptimisticSessionMutation(qc, [])).toBeNull();
    expect(() =>
      applyOptimisticSessionMutation(qc, [mk({ id: "x", agentType: "claude" })])
    ).not.toThrow();
    clearPendingSessions([{ id: "x", agentType: "claude" }]);
  });
});
