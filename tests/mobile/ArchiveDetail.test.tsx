import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { toast } from "sonner";
import ArchiveDetail from "@/mobile/ArchiveDetail";
import type { ArchivedSession } from "@/mobile/api";

// T3 未信任预检：toast 走 mock（断言「trustPromptExpected → 提醒被调」；
// 本文件其余用例不依赖真实 sonner 行为，文件级 mock 无涉）
vi.mock("sonner", () => ({ toast: vi.fn() }));

const card: ArchivedSession = {
  sessionId: "dead-1",
  agentType: "codex",
  projectPath: "/tmp/p1",
  projectName: "proj-1",
  title: "标题",
  lastStatus: "idle",
  lastSeenAt: new Date().toISOString(),
};

const DEFAULT_PAGE = {
  messages: [{ seq: 1, role: "user", content: "旧消息", kind: "text", ts: 1 }],
  truncated: false,
};

function installFetch(routes: Record<string, unknown>) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, _init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/session-open")) {
        const body = routes["sessionOpen"] as Record<string, string> | undefined;
        const status = routes["sessionOpenStatus"] as number | undefined;
        return new Response(JSON.stringify(body ?? { status: "opening" }), {
          status: status ?? 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url.includes("/session-messages")) {
        const page =
          (routes["messagesPage"] as Record<string, unknown> | undefined) ?? DEFAULT_PAGE;
        return new Response(JSON.stringify(page), {
          headers: { "content-type": "application/json" },
        });
      }
      if (url.includes("/sessions-archived")) {
        return new Response(JSON.stringify({ deleted: 1 }), {
          headers: { "content-type": "application/json" },
        });
      }
      return new Response("{}", { status: 404 });
    })
  );
}

describe("ArchiveDetail：归档详情与激活", () => {
  beforeEach(() => installFetch({}));
  beforeEach(() => vi.mocked(toast).mockClear());
  afterEach(() => {
    vi.unstubAllGlobals();
    cleanup();
  });

  it("渲染历史消息内容 + 在桌面端打开按钮存在", async () => {
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    expect(await screen.findByText("旧消息")).toBeTruthy();
    expect(screen.getByTestId("session-open").textContent).toBe("在桌面端打开");
  });

  it("opening 回执 → onActivated 回调（乐观回看板，裁决 6 闭环）", async () => {
    const activated = vi.fn();
    installFetch({ sessionOpen: { status: "opening" } });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={activated} />);
    fireEvent.click(screen.getByTestId("session-open"));
    await screen.findByText("正在电脑上打开终端…");
    expect(activated).toHaveBeenCalled();
    expect(toast).not.toHaveBeenCalled();
  });

  it("T3 未信任预检：opening + trustPromptExpected → toast 提醒去主机应答", async () => {
    installFetch({ sessionOpen: { status: "opening", trustPromptExpected: true } });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    fireEvent.click(screen.getByTestId("session-open"));
    await screen.findByText("正在电脑上打开终端…");
    expect(toast).toHaveBeenCalledWith(
      "重开的会话所在目录未做信任确认，请在主机终端应答信任提示，否则会话将挂起"
    );
  });

  it("failed 回执 → 错误文案上屏、不回调（可重试）", async () => {
    const activated = vi.fn();
    installFetch({ sessionOpen: { status: "failed", error: "终端启动失败（模拟）" } });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    fireEvent.click(screen.getByTestId("session-open"));
    expect(await screen.findByTestId("session-open-error")).toBeTruthy();
    expect(activated).not.toHaveBeenCalled();
  });

  it("打开失败哨兵分诊：no_session（404 载荷）→ 归档不存在文案", async () => {
    installFetch({ sessionOpen: { error: "no_session" }, sessionOpenStatus: 404 });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    fireEvent.click(screen.getByTestId("session-open"));
    expect(await screen.findByText("归档记录已不存在，请刷新列表")).toBeTruthy();
  });

  it("打开失败哨兵分诊：no_resume_command（404 载荷）→ 复用工具不支持文案", async () => {
    installFetch({ sessionOpen: { error: "no_resume_command" }, sessionOpenStatus: 404 });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    fireEvent.click(screen.getByTestId("session-open"));
    expect(
      await screen.findByText("打开失败：该工具 resume 命令待查证，暂不支持一键打开")
    ).toBeTruthy();
  });

  it("信息行 agentType 用 TOOL_LABELS 中文（不再裸显工具 id）", async () => {
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    await screen.findByText("旧消息");
    expect(screen.getByText(/Codex · \/tmp\/p1 · /)).toBeTruthy();
  });

  it("不支持 resume 的工具 → 按钮禁用 + 原因（门迁移回归锁）", () => {
    render(
      <ArchiveDetail
        session={{ ...card, agentType: "workbuddy" }}
        onBack={() => {}}
        onActivated={() => {}}
      />
    );
    const btn = screen.getByTestId("session-open") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(screen.getByText("该工具 resume 命令待查证，暂不支持一键打开")).toBeTruthy();
  });

  it("从归档移除 → DELETE 后返回", async () => {
    const back = vi.fn();
    render(<ArchiveDetail session={card} onBack={back} onActivated={() => {}} />);
    fireEvent.click(screen.getByTestId("archive-remove"));
    fireEvent.click(screen.getByTestId("archive-remove-confirm"));
    await new Promise((r) => setTimeout(r, 0));
    expect(back).toHaveBeenCalled();
  });

  it("从归档移除是描边真按钮（样式契约：border + button 元素）", async () => {
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    await screen.findByText("旧消息");
    const btn = screen.getByTestId("archive-remove");
    expect(btn.tagName).toBe("BUTTON");
    expect(btn.className).toContain("border");
  });

  it("进入落底：消息加载后滚动容器落到最底（对齐活会话首拉语义）", async () => {
    let resolveFetch!: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((res) => {
            resolveFetch = res;
          })
      )
    );
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    const area = screen.getByTestId("message-area");
    // 几何量先注入再放行响应（活会话测试同款：jsdom 无布局引擎，scrollHeight 恒 0）
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: 1000, configurable: true });
    await act(async () => {
      resolveFetch(
        new Response(
          JSON.stringify({
            messages: [
              { seq: 1, role: "user", content: "开头消息", kind: "text", ts: 1 },
              { seq: 2, role: "assistant", content: "结尾回复", kind: "assistant", ts: 2 },
            ],
            truncated: false,
          }),
          { headers: { "content-type": "application/json" } }
        )
      );
    });
    await waitFor(() => expect(area.scrollTop).toBe(5000));
  });

  it("双浮动钮：距顶/距底超阈值各自浮现，点击落顶/落底", async () => {
    installFetch({
      messagesPage: {
        messages: Array.from({ length: 10 }, (_, i) => ({
          seq: i + 1,
          role: "user",
          content: `第 ${i + 1} 条`,
          kind: "text",
          ts: i + 1,
        })),
        truncated: false,
      },
    });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    await screen.findByText("第 1 条");
    const area = screen.getByTestId("message-area");
    Object.defineProperty(area, "scrollHeight", { value: 8000, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: 2000, configurable: true });
    area.scrollTop = 3000; // 距顶 3000、距底 3000，双钮都该显
    fireEvent.scroll(area);
    expect(screen.getByTestId("jump-to-top")).toBeTruthy();
    expect(screen.getByTestId("jump-to-bottom")).toBeTruthy();
    fireEvent.click(screen.getByTestId("jump-to-top"));
    expect(area.scrollTop).toBe(0);
    fireEvent.scroll(area);
    expect(screen.queryByTestId("jump-to-top")).toBeNull();
    area.scrollTop = 3000;
    fireEvent.scroll(area);
    fireEvent.click(screen.getByTestId("jump-to-bottom"));
    expect(area.scrollTop).toBe(8000);
    fireEvent.scroll(area);
    expect(screen.queryByTestId("jump-to-bottom")).toBeNull();
  });

  it("总结模式默认折叠：过程消息折叠头可见、最后 assistant 直显；展开全部/收起", async () => {
    installFetch({
      messagesPage: {
        messages: [
          { seq: 1, role: "user", content: "帮我查下", kind: "text", ts: 1 },
          {
            seq: 2,
            role: "assistant",
            content: "thinking 过程内容",
            kind: "thinking",
            ts: 2,
            collapsed: true,
          },
          {
            seq: 3,
            role: "assistant",
            content: "tool-call 过程内容",
            kind: "tool-call",
            ts: 3,
            toolName: "grep",
            collapsed: true,
          },
          {
            seq: 4,
            role: "assistant",
            content: "早先回复",
            kind: "assistant",
            ts: 4,
            collapsed: false,
          },
          {
            seq: 5,
            role: "assistant",
            content: "最终总结回复",
            kind: "assistant",
            ts: 5,
            collapsed: false,
          },
        ],
        truncated: false,
      },
    });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    expect(await screen.findByText("帮我查下")).toBeTruthy();
    // user 与最后 assistant 直显；更早 assistant 与过程消息默认折叠
    expect(screen.getByText("最终总结回复")).toBeTruthy();
    expect(screen.queryByText("早先回复")).toBeNull();
    expect(screen.queryByText("thinking 过程内容")).toBeNull();
    expect(screen.getByTestId("msg-2-toggle").textContent).toContain("思考过程");
    expect(screen.getByTestId("msg-3-toggle").textContent).toContain("调用 grep");
    // 单条点开
    fireEvent.click(screen.getByTestId("msg-2-toggle"));
    expect(screen.getByText("thinking 过程内容")).toBeTruthy();
    // 一键展开全部 → 一键收起回默认
    fireEvent.click(screen.getByTestId("expand-all"));
    expect(screen.getByText("早先回复")).toBeTruthy();
    fireEvent.click(screen.getByTestId("collapse-all"));
    expect(screen.queryByText("早先回复")).toBeNull();
    expect(screen.queryByText("thinking 过程内容")).toBeNull();
  });

  it("truncated → 窗口顶显示截断提示行", async () => {
    installFetch({ messagesPage: { ...DEFAULT_PAGE, truncated: true } });
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    expect((await screen.findByTestId("archive-truncated")).textContent).toContain("200");
  });
});

describe("ArchiveDetail 软归档与加载更早（体验批二）", () => {
  beforeEach(() => installFetch({}));
  afterEach(() => {
    vi.unstubAllGlobals();
    cleanup();
  });

  it("hiddenAlive 会话：未结束徽标 + 移回看板（无打开按钮），成功后回历史页", async () => {
    const back = vi.fn();
    const unhideCalls: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/session-unhide")) {
          unhideCalls.push(url);
          return new Response(JSON.stringify({ ok: true, removed: 1 }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url.includes("/session-messages")) {
          return new Response(JSON.stringify(DEFAULT_PAGE), {
            headers: { "content-type": "application/json" },
          });
        }
        return new Response("{}", { status: 404 });
      })
    );
    render(
      <ArchiveDetail
        session={{ ...card, hiddenAlive: true }}
        onBack={back}
        onActivated={() => {}}
      />
    );
    expect(await screen.findByTestId("hidden-alive-badge")).toBeTruthy();
    expect(screen.queryByTestId("session-open")).toBeNull();
    expect(screen.queryByTestId("archive-remove")).toBeNull();
    fireEvent.click(screen.getByTestId("unhide-session"));
    await new Promise((r) => setTimeout(r, 0));
    expect(unhideCalls).toHaveLength(1);
    expect(back).toHaveBeenCalled();
  });

  it("加载更早消息：达 limit 显按钮，点击以翻倍 limit 整页重拉并锚定原顶部", async () => {
    Element.prototype.scrollIntoView = vi.fn();
    const requested: number[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/session-messages")) {
          const limit = Number(new URL(url, "http://localhost").searchParams.get("limit") ?? "0");
          requested.push(limit);
          return new Response(
            JSON.stringify({
              messages: Array.from({ length: limit }, (_, i) => ({
                seq: i + 1,
                role: "user",
                content: `m${i + 1}`,
                kind: "text",
                ts: i + 1,
              })),
              truncated: false,
            }),
            { headers: { "content-type": "application/json" } }
          );
        }
        return new Response("{}", { status: 404 });
      })
    );
    render(<ArchiveDetail session={card} onBack={() => {}} onActivated={() => {}} />);
    expect(await screen.findByText("m200")).toBeTruthy();
    expect(screen.getByTestId("load-more")).toBeTruthy();
    fireEvent.click(screen.getByTestId("load-more"));
    expect(await screen.findByText("m400")).toBeTruthy();
    expect(requested).toEqual([200, 400]);
    // 锚定在被动 effect 里调度（useEffect [messages]），而 findByText 走 MutationObserver
    // 微任务可先于 effect flush 解析——同步断言是竞态（CI 慢机红过：run 36207251281）。
    // 条件等待（同 SessionDetail bookmark-jump 的先例）：等被动 effect 落地再断言。
    await waitFor(
      () => expect(Element.prototype.scrollIntoView).toHaveBeenCalled() // 锚定原顶部消息
    );
  });
});
