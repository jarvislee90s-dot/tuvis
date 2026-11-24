import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
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
  describe("截断与扩窗", () => {
    function installPagedMessages(messages: ReturnType<typeof msg>[]) {
      fetchMock = vi.fn(async (input: RequestInfo | URL) => {
        const url = new URL(String(input), "http://localhost");
        if (url.pathname.endsWith("/session-files")) {
          return new Response(JSON.stringify({ files: [], truncated: false }), { status: 200 });
        }
        if (!url.pathname.endsWith("/session-subagent-messages")) {
          throw new Error(`unexpected fetch: ${url}`);
        }
        const limit = Number(url.searchParams.get("limit"));
        return new Response(
          JSON.stringify({
            messages: messages.slice(-limit),
            truncated: messages.length > limit,
            supported: true,
          }),
          { status: 200 }
        );
      });
      vi.stubGlobal("fetch", fetchMock);
    }

    function requestedLimits() {
      return fetchMock.mock.calls
        .map(([input]) => new URL(String(input), "http://localhost"))
        .filter((url) => url.pathname.endsWith("/session-subagent-messages"))
        .map((url) => Number(url.searchParams.get("limit")));
    }

    function renderDetail() {
      return render(
        <SubagentDetail
          session={baseSession}
          subagentId="a1"
          subagentName="Plan"
          running={false}
          openFile={() => {}}
        />
      );
    }

    it("201 条记录首屏显示尾部 200 条，扩窗到 400 后显示任务原文", async () => {
      const messages = Array.from({ length: 201 }, (_, seq) =>
        seq === 0
          ? msg({
              seq,
              kind: "user",
              role: "user",
              content: '<teammate-message teammate_id="team-lead">调查完整任务</teammate-message>',
            })
          : msg({ seq, kind: "assistant", content: `执行记录 ${seq}` })
      );
      installPagedMessages(messages);
      renderDetail();

      await screen.findByTestId("subagent-msg-200");
      expect(requestedLimits()).toEqual([200]);
      expect(within(screen.getByTestId("subagent-messages")).getAllByRole("listitem")).toHaveLength(
        200
      );
      expect(screen.queryByTestId("subagent-msg-0")).toBeNull();
      expect(screen.queryByTestId("subagent-task-text")).toBeNull();
      fireEvent.click(screen.getByTestId("subagent-load-more"));

      expect(await screen.findByTestId("subagent-task-text")).toHaveTextContent("调查完整任务");
      expect(screen.getByTestId("subagent-task-text")).not.toHaveTextContent("teammate-message");
      expect(screen.getByTestId("subagent-msg-0")).toBeTruthy();
      expect(within(screen.getByTestId("subagent-messages")).getAllByRole("listitem")).toHaveLength(
        201
      );
      expect(requestedLimits()).toEqual([200, 400]);
      expect(screen.queryByTestId("subagent-load-more")).toBeNull();
    });

    it("扩窗达到 1000 上限后，即使仍有更早记录也不显示入口", { timeout: 15_000 }, async () => {
      const messages = Array.from({ length: 1001 }, (_, seq) =>
        msg({ seq, kind: "assistant", content: `执行记录 ${seq}` })
      );
      installPagedMessages(messages);
      renderDetail();
      await screen.findByTestId("subagent-msg-801");

      for (const limit of [400, 600, 800, 1000]) {
        fireEvent.click(screen.getByTestId("subagent-load-more"));
        await screen.findByTestId(`subagent-msg-${1001 - limit}`);
      }

      expect(requestedLimits()).toEqual([200, 400, 600, 800, 1000]);
      expect(screen.queryByTestId("subagent-msg-0")).toBeNull();
      expect(screen.queryByTestId("subagent-load-more")).toBeNull();
    });

    it("后端明确未截断时，即使返回恰好 200 条也不显示扩窗入口", async () => {
      const messages = Array.from({ length: 200 }, (_, seq) =>
        msg({ seq, kind: "assistant", content: `执行记录 ${seq}` })
      );
      installPagedMessages(messages);
      renderDetail();

      await screen.findByTestId("subagent-msg-199");
      expect(requestedLimits()).toEqual([200]);
      expect(within(screen.getByTestId("subagent-messages")).getAllByRole("listitem")).toHaveLength(
        200
      );
      expect(screen.queryByTestId("subagent-load-more")).toBeNull();
    });
  });

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

  describe.each(["claude", "codex", "opencode", "kimi"] as const)(
    "H08：%s 既有详情状态",
    (agentType) => {
      it.each([
        { status: "running", running: true, dot: "bg-emerald-500" },
        { status: "idle", running: false, dot: "bg-gray-400" },
      ])("$status 保留原状态点与实时提示语义", async ({ running, dot }) => {
        routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "执行记录" })] };
        render(
          <SubagentDetail
            session={{ ...baseSession, agentType }}
            subagentId="a1"
            subagentName="Worker"
            running={running}
            openFile={() => {}}
          />
        );
        await screen.findByTestId("subagent-msg-0");
        expect(screen.getByTestId("subagent-dot").className).toContain(dot);
        const live = screen.queryByText("实时 · 5s");
        if (running) expect(live).toBeTruthy();
        else expect(live).toBeNull();
        expect(screen.getByTestId("subagent-detail").textContent).not.toMatch(/未知/);
      });
    }
  );

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

  describe("DSH 受限续页（2026-11-18 销项批：before 起始索引游标）", () => {
    const TOTAL = 1001;
    const dshSession: Session = { ...baseSession, agentType: "dsh", dshInstanceId: "inst-cur" };

    /** 后端游标契约的本地复刻（生产语义在 Rust 侧锁定，这里只供组件消费）：
     *  skip=0 → 尾窗 [total−limit, total)；before=N → [N−limit, N) 钳制；
     *  seq 每页 0 基重排（后端 finalize 口径）；olderBefore **仅截断页在场**。 */
    function installCursorBackend() {
      const corpus = Array.from({ length: TOTAL }, (_, i) =>
        msg({ seq: i, kind: "assistant", content: `记录 ${i}` })
      );
      const beforeParams: (string | null)[] = [];
      fetchMock = vi.fn(async (input: RequestInfo | URL) => {
        const url = new URL(String(input), "http://localhost");
        if (url.pathname.endsWith("/session-files")) {
          return new Response(JSON.stringify({ files: [], truncated: false }), { status: 200 });
        }
        if (!url.pathname.endsWith("/session-subagent-messages")) {
          throw new Error(`unexpected fetch: ${url}`);
        }
        const limit = Number(url.searchParams.get("limit"));
        const rawBefore = url.searchParams.get("before");
        beforeParams.push(rawBefore);
        const skip = rawBefore === null ? 0 : Number(rawBefore);
        const end = skip === 0 ? TOTAL : Math.min(skip, TOTAL);
        const start = Math.max(0, end - limit);
        const page = corpus.slice(start, end).map((m, i) => ({ ...m, seq: i }));
        const body: Record<string, unknown> = {
          messages: page,
          truncated: start > 0,
          supported: true,
        };
        if (start > 0) body.olderBefore = start;
        return new Response(JSON.stringify(body), { status: 200 });
      });
      vi.stubGlobal("fetch", fetchMock);
      return () => beforeParams;
    }

    function renderDshDetail() {
      return render(
        <SubagentDetail
          session={dshSession}
          subagentId="big"
          subagentName="Big Worker"
          running={false}
          openFile={() => {}}
        />
      );
    }

    it(
      "1001 条：游标逐页向最旧推进，拼接无重复无跳号，到最旧入口退场",
      // 2026-11-19：全量并发下本用例（千条 DOM 逐页渲染）贴 15s 资源阈值
      // 两次超时（隔离实跑 5.4s）——提高用例超时消除基础设施 flake。
      { timeout: 60_000 },
      async () => {
        const requestedBefores = installCursorBackend();
        renderDshDetail();
        // 首页 = 尾窗 200 条（seq 0..199 = 绝对 801..1000）
        await screen.findByTestId("subagent-msg-199");
        expect(screen.getByTestId("subagent-msg-0").textContent).toContain("记录 801");
        // 五跳游标链：801 → 601 → 401 → 201 → 1 → 到最旧（入口退场）
        for (let click = 1; click <= 5; click += 1) {
          fireEvent.click(screen.getByTestId("subagent-load-more"));
          await screen.findByText(`记录 ${Math.max(0, 1001 - (click + 1) * 200)}`);
        }
        expect(screen.queryByTestId("subagent-load-more")).toBeNull();
        // 全量在板：1001 条、按文件序、无重复无跳号
        const items = within(screen.getByTestId("subagent-messages")).getAllByRole("listitem");
        expect(items).toHaveLength(1001);
        for (let i = 0; i < TOTAL; i += 1) {
          expect(screen.getByTestId(`subagent-msg-${i}`).textContent).toContain(`记录 ${i}`);
        }
        // 游标逐请求透传：首页不带 before，此后沿响应游标推进
        expect(requestedBefores()).toEqual([null, "801", "601", "401", "201", "1"]);
      }
    );

    it("running 子代理轮询只刷尾窗：已加载的更早页不丢、滚出尾窗的条目被吸收", async () => {
      vi.useFakeTimers();
      installCursorBackend();
      render(
        <SubagentDetail
          session={dshSession}
          subagentId="big"
          subagentName="Big Worker"
          running={true}
          openFile={() => {}}
        />
      );
      // findBy* 不推进 fake timers：act 冲刷初始加载，再同步断言（套件头注释同款）
      await act(async () => {});
      expect(screen.getByTestId("subagent-msg-199")).toBeTruthy();
      // 加载一页更早（601..800），此时渲染 400 条
      await act(async () => {
        fireEvent.click(screen.getByTestId("subagent-load-more"));
      });
      expect(screen.getByText("记录 601")).toBeTruthy();
      expect(within(screen.getByTestId("subagent-messages")).getAllByRole("listitem")).toHaveLength(
        400
      );
      // 模拟子代理继续输出：语料追加 100 条（绝对 1001..1100）——尾窗锚从 801
      // 前移到 901，旧的 801..900 从尾窗滚出，必须被吸收而不是凭空消失
      fetchMock = vi.fn(async (input: RequestInfo | URL) => {
        const url = new URL(String(input), "http://localhost");
        if (url.pathname.endsWith("/session-files")) {
          return new Response(JSON.stringify({ files: [], truncated: false }), { status: 200 });
        }
        const limit = Number(url.searchParams.get("limit"));
        const rawBefore = url.searchParams.get("before");
        const total = TOTAL + 100;
        const skip = rawBefore === null ? 0 : Number(rawBefore);
        const end = skip === 0 ? total : Math.min(skip, total);
        const start = Math.max(0, end - limit);
        const page = Array.from({ length: end - start }, (_, i) =>
          msg({ seq: i, kind: "assistant", content: `记录 ${start + i}` })
        );
        const body: Record<string, unknown> = {
          messages: page,
          truncated: start > 0,
          supported: true,
        };
        if (start > 0) body.olderBefore = start;
        return new Response(JSON.stringify(body), { status: 200 });
      });
      vi.stubGlobal("fetch", fetchMock);
      // 推进一个轮询拍（SUBAGENT_DETAIL_REFRESH_MS = 5s）→ 尾窗刷新 + 吸收
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });
      // 视图仍完整：601..1100 全部在板（滚出段 801..900 被吸收进更早页）
      expect(within(screen.getByTestId("subagent-messages")).getAllByRole("listitem")).toHaveLength(
        500
      );
      expect(screen.queryByText("记录 601")).toBeTruthy();
      expect(screen.queryByText("记录 900")).toBeTruthy();
      expect(screen.getByTestId("subagent-load-more")).toBeTruthy();
    });
  });
});
