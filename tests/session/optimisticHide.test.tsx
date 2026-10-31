// F5-opt 组件级验收：invoke 未 resolve 前卡片已不可见；失败后 invalidate 回滚。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("sonner", () => ({ toast: { info: vi.fn(), error: vi.fn(), success: vi.fn() } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
import i18n from "@/i18n";
import { SessionCard } from "@/components/sessions/SessionCard";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
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

const seed: SessionsResponse = {
  sessions: [mk({}), mk({ id: "s2", projectName: "项目B", status: "idle" })],
  totalCount: 2,
  waitingCount: 1,
};

/** 从 ["sessions"] 缓存渲染卡片（home 的真实消费方式：useSessions → store） */
function Harness() {
  const { data } = useQuery({
    queryKey: SESSIONS_QUERY_KEY,
    queryFn: async () => seed,
  });
  return (
    <div>
      {data?.sessions.map((s) => (
        <SessionCard key={`${s.agentType}:${s.id}`} session={s} />
      ))}
    </div>
  );
}

function renderHarness() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  client.setQueryData(SESSIONS_QUERY_KEY, seed);
  const invalidate = vi.spyOn(client, "invalidateQueries");
  render(
    <QueryClientProvider client={client}>
      <Harness />
    </QueryClientProvider>
  );
  return invalidate;
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe("F5-opt 单卡乐观隐藏", () => {
  it("invoke 未 resolve 前卡片已从列表消失", async () => {
    let resolveInvoke!: (v: unknown) => void;
    invokeMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveInvoke = resolve;
        })
    );
    const invalidate = renderHarness();
    await screen.findByText("项目A");
    fireEvent.click(screen.getAllByRole("button", { name: /Hide for now/i })[0]);
    // 命令仍挂起：卡片应已消失，另一张卡保留
    await waitFor(() => expect(screen.queryByText("项目A")).toBeNull());
    expect(screen.getByText("项目B")).toBeInTheDocument();
    resolveInvoke({ applied: true, version: 2 });
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["sessions"] }));
  });

  it("invoke 失败也 invalidate（回滚乐观隐藏）", async () => {
    invokeMock.mockRejectedValueOnce(new Error("ipc failed"));
    const invalidate = renderHarness();
    await screen.findByText("项目A");
    fireEvent.click(screen.getAllByRole("button", { name: /Hide for now/i })[0]);
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["sessions"] }));
    // invalidate 触发 refetch（queryFn 返回 seed）→ 卡片回滚出现
    await waitFor(() => expect(screen.getByText("项目A")).toBeInTheDocument());
  });
});
