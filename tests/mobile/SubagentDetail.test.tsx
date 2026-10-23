import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SubagentDetail from "@/mobile/SubagentDetail";
import type { Session } from "@/types/session";

// 观察台 §三：supported 态 / 消息渲染复用（thinking 默认折叠可展开）/ 任务原文 /
// running 自动刷新起停（fake timers）/ 不活跃定格。
// 实施备注（v1 教训）：RTL findBy* 不推进 vitest fake timers——用例 3/4 以
// `await act(async () => {})` 冲刷初始加载的微任务链后改用同步 getBy* 断言。

const baseSession: Session = {
  id: "s1",
  agentType: "claude",
  projectName: "p",
  projectPath: "p",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "processing",
  lastMessage: null,
  lastMessageRole: null,
  lastActivityAt: "2026-10-08T07:00:00Z",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "cli",
  jumpSupported: false,
  unread: false,
};

let fetchMock: ReturnType<typeof vi.fn>;
let routes: {
  body?: { messages: unknown[]; truncated?: boolean; supported?: boolean };
  fail?: boolean;
};

beforeEach(async () => {
  await i18n.changeLanguage("zh");
  routes = {};
  vi.stubGlobal(
    "fetch",
    (fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (!url.includes("/session-subagent-messages")) throw new Error(`unexpected fetch: ${url}`);
      if (routes.fail) throw new TypeError("network down");
      return new Response(
        JSON.stringify({
          messages: routes.body?.messages ?? [],
          truncated: routes.body?.truncated ?? false,
          supported: routes.body?.supported ?? true,
        }),
        { status: 200 }
      );
    }))
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function msg(p: Partial<Record<string, unknown>> & { seq: number; kind: string; content: string }) {
  return {
    role: "assistant",
    ts: null,
    toolName: null,
    toolArgs: null,
    collapsed: p.kind === "thinking" || p.kind === "tool-call",
    ...p,
  };
}

/** 只数 /session-subagent-messages 的调用（挂载期还有一发 /session-files，
 *  mock 对它 throw 后由 api 层静默——不计入轮询节奏断言，SessionDetail.test
 *  「只数 /session-messages」同款口径） */
function detailCalls(): number {
  return fetchMock.mock.calls.filter((c: unknown[]) =>
    String(c[0]).includes("/session-subagent-messages")
  ).length;
}

describe("SubagentDetail（实时预览对话框）", () => {
  it("supported=false → 明确「暂不支持查看详情」态，不渲染消息区（§三.6）", async () => {
    routes.body = { messages: [], supported: false };
    render(
      <SubagentDetail
        session={baseSession}
        subagentId="x1"
        subagentName="Explore"
        running={false}
        openFile={() => {}}
      />
    );
    expect(await screen.findByTestId("subagent-detail-unsupported")).toBeTruthy();
    // 不渲染消息区：unsupported 时无任何消息条目（subagent-messages 容器是
    // 滚动视口壳恒在——提示态渲染在其内部，故断言条目缺席而非容器缺席）
    expect(document.querySelector('[data-testid="subagent-messages"] li')).toBeNull();
  });

  it("任务原文 + 消息渲染复用（thinking 默认折叠、点击展开）+ 工具行折叠（§三.2/§三.5）", async () => {
    routes.body = {
      messages: [
        msg({
          seq: 0,
          kind: "user",
          content: '<teammate-message teammate_id="team-lead">设计新方案</teammate-message>',
        }),
        msg({ seq: 1, kind: "thinking", content: "先看目录" }),
        msg({
          seq: 2,
          kind: "tool-call",
          content: "",
          toolName: "Bash",
          toolArgs: '{"command":"ls"}',
        }),
        msg({ seq: 3, kind: "assistant", content: "已完成" }),
      ],
    };
    render(
      <SubagentDetail
        session={baseSession}
        subagentId="a1"
        subagentName="Plan"
        running={true}
        openFile={() => {}}
      />
    );
    // 任务原文：首条 user 剥 teammate-message 壳（§三.2「任务原文=首条指令」）
    // （实施订正：await 须括住 findBy* 再取 textContent——计划稿写法先取
    // Promise.textContent 恒 undefined）
    expect((await screen.findByTestId("subagent-task-text")).textContent).toBeTruthy();
    expect(screen.getByTestId("subagent-task-text").textContent).toContain("设计新方案");
    expect(screen.getByTestId("subagent-task-text").textContent).not.toContain("teammate-message");
    // thinking / tool-call 默认折叠（wire collapsed 语义），点击展开
    expect(screen.getByTestId("subagent-msg-1").textContent).toContain("思考过程");
    fireEvent.click(screen.getByTestId("subagent-msg-1-toggle"));
    expect(screen.getByTestId("subagent-msg-1").textContent).toContain("先看目录");
    // tool-call 同为 wire 默认折叠（M4 评审修复：此前该语义从未真正断言）——
    // 折叠时参数缺席、仅显「调用 Bash」摘要头，点击展开后参数在场
    expect(screen.getByTestId("subagent-msg-2").textContent).toContain("调用 Bash");
    expect(screen.getByTestId("subagent-msg-2").textContent).not.toContain('"command"');
    fireEvent.click(screen.getByTestId("subagent-msg-2-toggle"));
    expect(screen.getByTestId("subagent-msg-2").textContent).toContain('"command":"ls"');
    expect(screen.getByTestId("subagent-msg-3").textContent).toContain("已完成");
  });

  it("running：打开期间自动刷新（数秒一拍），输出逐步增长（§三.3）", async () => {
    vi.useFakeTimers();
    routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "步骤 1" })] };
    render(
      <SubagentDetail
        session={baseSession}
        subagentId="a1"
        subagentName="Plan"
        running={true}
        openFile={() => {}}
      />
    );
    // findBy* 不推进 fake timers：act 冲刷初始加载，再同步断言
    await act(async () => {});
    expect(screen.getByTestId("subagent-msg-0")).toBeTruthy();
    routes.body = {
      messages: [
        msg({ seq: 0, kind: "assistant", content: "步骤 1" }),
        msg({ seq: 1, kind: "assistant", content: "步骤 2" }),
      ],
    };
    // 定时器推进包进 act：interval 回调触发的 setState 在 act 环境内冲刷
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000); // SUBAGENT_DETAIL_REFRESH_MS
    });
    await vi.waitFor(() => expect(detailCalls()).toBeGreaterThanOrEqual(2));
    await vi.waitFor(() => expect(screen.getByTestId("subagent-msg-1")).toBeTruthy());
  });

  it("running=false：不装轮询（定格快照，§三.4）；关闭即停（卸载清 interval）", async () => {
    vi.useFakeTimers();
    routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "终稿" })] };
    const { unmount } = render(
      <SubagentDetail
        session={baseSession}
        subagentId="a1"
        subagentName="Plan"
        running={false}
        openFile={() => {}}
      />
    );
    // findBy* 不推进 fake timers：act 冲刷初始加载，再同步断言
    await act(async () => {});
    expect(screen.getByTestId("subagent-msg-0")).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    // 不活跃 → 不轮询（定格快照）
    expect(detailCalls()).toBe(1);
    unmount(); // 关闭即停：卸载后再无任何调用（防御断言）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(detailCalls()).toBe(1);
  });

  it("加载失败 → 错误文案 + 手动重试（M3 评审修复：定格快照的临时失败不再死局）", async () => {
    routes.fail = true;
    render(
      <SubagentDetail
        session={baseSession}
        subagentId="a1"
        subagentName="Plan"
        running={false}
        openFile={() => {}}
      />
    );
    expect(await screen.findByTestId("subagent-detail-error")).toBeTruthy();
    expect(screen.getByTestId("subagent-detail-retry")).toBeTruthy();
    // 网络恢复后重试：单发重拉成功 → 错误态退场、消息照常渲染
    //（running=false 仍不装轮询——重试不改变定格语义）
    routes.fail = false;
    routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "终稿" })] };
    fireEvent.click(screen.getByTestId("subagent-detail-retry"));
    expect(await screen.findByTestId("subagent-msg-0")).toBeTruthy();
    expect(screen.queryByTestId("subagent-detail-error")).toBeNull();
  });
});
