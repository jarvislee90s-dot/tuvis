// G1 组件级验收（09b95a3c F5 双口径）：
//  - 未读聚合绿卡（codex/zcode/dsh 的 App 形态 idle/finished）：invoke 挂起时**整卡** ≤1s 消失；
//  - 普通未读卡：invoke 挂起时卡面保留、**未读标记**消失；
//  - 轮询把旧快照写回缓存（setQueryData）→ 卡片/未读态不复活（pending 集合拦截）；
//  - 命令失败：finally invalidate 回取权威快照 → 卡片回滚可见。
// Harness 与 home 同构：缓存 → useSyncExternalStore 订阅 pending → applyPendingSessions → 卡片。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { useSyncExternalStore } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("sonner", () => ({ toast: { info: vi.fn(), error: vi.fn(), success: vi.fn() } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
// tests/setup.ts 未初始化 i18n，显式引入（fallbackLng=en，与断言文案一致）
import i18n from "@/i18n";
import { SessionCard } from "@/components/sessions/SessionCard";
import {
  SESSIONS_QUERY_KEY,
  applyPendingSessions,
  clearPendingSessions,
  getPendingSessionsVersion,
  subscribePendingSessions,
} from "@/lib/query/optimisticSessions";
import type { Session, SessionsResponse } from "@/types/session";

void i18n;

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
  pid: 42,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "app",
  jumpSupported: true,
  unread: false,
  ...over,
});

const seedOf = (sessions: Session[]): SessionsResponse => ({
  sessions,
  totalCount: sessions.length,
  waitingCount: sessions.filter((s) => s.status === "waiting").length,
});

/** 与 home 的消费方式同构：pending 变化触发重渲染 → applyPendingSessions 过滤 */
function Harness({ seed }: { seed: SessionsResponse }) {
  const { data } = useQuery({
    queryKey: SESSIONS_QUERY_KEY,
    queryFn: async () => seed,
  });
  useSyncExternalStore(subscribePendingSessions, getPendingSessionsVersion);
  const sessions = applyPendingSessions(data?.sessions ?? []);
  return (
    <div>
      {sessions.map((s) => (
        <SessionCard key={`${s.agentType}:${s.id}`} session={s} />
      ))}
    </div>
  );
}

function renderHarness(seed: SessionsResponse) {
  const client = new QueryClient({
    // staleTime=∞：挂载不自动重取，invalidate 时才 refetch（回滚路径可控）
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(SESSIONS_QUERY_KEY, seed);
  const invalidate = vi.spyOn(client, "invalidateQueries");
  render(
    <QueryClientProvider client={client}>
      <Harness seed={seed} />
    </QueryClientProvider>
  );
  return { client, invalidate };
}

/** invoke 挂起：返回 resolve 句柄，调用前命令一直未结算 */
function pendingInvoke() {
  let resolve!: (v: unknown) => void;
  invokeMock.mockImplementation(
    () =>
      new Promise((r) => {
        resolve = r;
      })
  );
  return (v: unknown) => resolve(v);
}

const ALL_TARGETS = [
  { id: "c1", agentType: "codex" },
  { id: "c1", agentType: "claude" },
  { id: "w1", agentType: "workbuddy" },
];

beforeEach(() => {
  invokeMock.mockReset();
  // 模块级 pending 跨用例共享，逐用例清空
  clearPendingSessions(ALL_TARGETS);
});

describe("G1 单卡乐观隐藏（09b95a3c 双口径）", () => {
  it("未读聚合绿卡（codex App idle）：invoke 挂起时整卡消失（≤1s）", async () => {
    const seed = seedOf([
      mk({
        id: "c1",
        agentType: "codex",
        status: "idle",
        form: "app",
        unread: true,
        projectName: "项目A",
      }),
      mk({ id: "w1", projectName: "项目B" }),
    ]);
    const resolve = pendingInvoke();
    const { invalidate } = renderHarness(seed);
    await screen.findByText("项目A");
    expect(screen.getByLabelText("Unread")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /Mark read and dismiss/i }));
    // waitFor 默认 timeout=1000ms：整卡在 1s 感知预算内消失
    await waitFor(() => expect(screen.queryByText("项目A")).toBeNull(), { timeout: 1000 });
    expect(screen.getByText("项目B")).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("mark_session_read", {
      agentType: "codex",
      sessionId: "c1",
    });
    // 命令仍挂起 → 上述消失来自乐观隐藏而非权威快照回取
    expect(invalidate).not.toHaveBeenCalled();
    resolve({ applied: true, version: 2 });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: SESSIONS_QUERY_KEY }));
  });

  it("普通未读卡（claude）：invoke 挂起时卡仍在、未读标记消失", async () => {
    const seed = seedOf([
      mk({
        id: "c1",
        agentType: "claude",
        status: "idle",
        unread: true,
        projectName: "项目A",
      }),
      mk({ id: "w1", projectName: "项目B" }),
    ]);
    const resolve = pendingInvoke();
    const { invalidate } = renderHarness(seed);
    await screen.findByText("项目A");
    expect(screen.getByLabelText("Unread")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /Mark read and dismiss/i }));
    await waitFor(() => expect(screen.queryByLabelText("Unread")).toBeNull(), { timeout: 1000 });
    // 卡面保留（标已读 ≠ 关卡），其他卡不受影响
    expect(screen.getByText("项目A")).toBeInTheDocument();
    expect(screen.getByText("项目B")).toBeInTheDocument();
    resolve({ applied: true, version: 2 });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: SESSIONS_QUERY_KEY }));
  });

  it("轮询回写旧快照（setQueryData）：整卡已隐藏则不复活", async () => {
    const seed = seedOf([
      mk({
        id: "c1",
        agentType: "codex",
        status: "finished",
        form: "app",
        unread: true,
        projectName: "项目A",
      }),
    ]);
    const resolve = pendingInvoke();
    const { client, invalidate } = renderHarness(seed);
    await screen.findByText("项目A");
    fireEvent.click(screen.getByRole("button", { name: /Mark read and dismiss/i }));
    await waitFor(() => expect(screen.queryByText("项目A")).toBeNull());
    // 模拟 useSessions 轮询把旧快照写回缓存
    act(() => {
      client.setQueryData(SESSIONS_QUERY_KEY, seed);
    });
    expect(screen.queryByText("项目A")).toBeNull();
    resolve({ applied: true, version: 2 });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: SESSIONS_QUERY_KEY }));
  });

  it("轮询回写旧快照（setQueryData）：普通未读卡未读标记不复活", async () => {
    const seed = seedOf([
      mk({
        id: "c1",
        agentType: "claude",
        status: "idle",
        unread: true,
        projectName: "项目A",
      }),
    ]);
    const resolve = pendingInvoke();
    const { client, invalidate } = renderHarness(seed);
    await screen.findByText("项目A");
    fireEvent.click(screen.getByRole("button", { name: /Mark read and dismiss/i }));
    await waitFor(() => expect(screen.queryByLabelText("Unread")).toBeNull());
    act(() => {
      client.setQueryData(SESSIONS_QUERY_KEY, seed);
    });
    expect(screen.queryByLabelText("Unread")).toBeNull();
    expect(screen.getByText("项目A")).toBeInTheDocument();
    resolve({ applied: true, version: 2 });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: SESSIONS_QUERY_KEY }));
  });

  it("命令失败：invalidate 回取权威快照后卡片回滚可见", async () => {
    const seed = seedOf([mk({ id: "w1", status: "waiting", unread: false, projectName: "项目A" })]);
    invokeMock.mockRejectedValueOnce(new Error("ipc failed"));
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const { invalidate } = renderHarness(seed);
    await screen.findByText("项目A");
    fireEvent.click(screen.getByRole("button", { name: /Hide for now/i }));
    await waitFor(() => expect(screen.queryByText("项目A")).toBeNull());
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: SESSIONS_QUERY_KEY }));
    // invalidate 触发 refetch（queryFn 返回 seed）+ finally 清 pending → 卡片回滚出现
    await waitFor(() => expect(screen.getByText("项目A")).toBeInTheDocument());
    error.mockRestore();
  });
});
