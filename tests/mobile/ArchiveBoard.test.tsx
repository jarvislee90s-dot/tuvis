import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ArchiveBoard from "@/mobile/ArchiveBoard";
import type { ArchivedPayload } from "@/mobile/api";

const payload: ArchivedPayload = {
  archived: [
    {
      sessionId: "a1",
      agentType: "codex",
      projectPath: "/tmp/p1",
      projectName: "proj-1",
      title: "修复登录",
      lastStatus: "idle",
      lastSeenAt: new Date(Date.now() - 3600_000).toISOString(),
    },
  ],
  projects: ["proj-1"],
};

function installFetch(ok = true) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/sessions-archived") && !url.includes("DELETE")) {
        if (!ok) return new Response("boom", { status: 500 });
        return new Response(JSON.stringify(payload), {
          headers: { "content-type": "application/json" },
        });
      }
      return new Response("{}", { status: 404 });
    })
  );
}

describe("ArchiveBoard：历史页", () => {
  beforeEach(() => installFetch());
  afterEach(() => {
    vi.unstubAllGlobals();
    cleanup();
  });

  it("默认 1 天拉取并渲染卡片；卡片上无任何打开按钮", async () => {
    const calls: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (i: RequestInfo | URL) => {
        calls.push(String(i));
        return new Response(JSON.stringify(payload), {
          headers: { "content-type": "application/json" },
        });
      })
    );
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    // selector 收窄到卡片 span：项目下拉 <option> 同文本，findByText 多匹配会抛错
    expect(await screen.findByText("proj-1", { selector: "span" })).toBeTruthy();
    expect(calls[0]).toContain("days=1");
    expect(screen.queryByTestId("session-open")).toBeNull(); // 裁决 6：按钮不浮在卡片上
  });

  it("切 7 天 → 重新拉取（URL 带 days=7）", async () => {
    const calls: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (i: RequestInfo | URL) => {
        calls.push(String(i));
        return new Response(JSON.stringify(payload), {
          headers: { "content-type": "application/json" },
        });
      })
    );
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    await screen.findByText("proj-1", { selector: "span" });
    fireEvent.click(screen.getByTestId("archive-days-7"));
    await waitFor(() => expect(calls.some((c) => c.includes("days=7"))).toBe(true));
  });

  it("失败态显示重试按钮；点击重发", async () => {
    installFetch(false);
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    expect(await screen.findByTestId("archive-retry")).toBeTruthy();
  });

  it("首载中显示「加载中…」", () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise(() => {}))
    ); // 永不 resolve
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    expect(screen.getByText("加载中…")).toBeTruthy();
  });

  it("有筛选时空态改显「当前筛选无匹配会话」，不误导扩窗", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (i: RequestInfo | URL) => {
        const url = String(i);
        const empty = url.includes("days=7");
        return new Response(JSON.stringify(empty ? { archived: [], projects: [] } : payload), {
          headers: { "content-type": "application/json" },
        });
      })
    );
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    expect(await screen.findByText("proj-1", { selector: "span" })).toBeTruthy();
    // 真实路径构造「有筛选 + 空」组合：选定项目后切小窗，新窗口不含该项目
    fireEvent.change(screen.getByLabelText("按项目筛选"), { target: { value: "proj-1" } });
    fireEvent.click(screen.getByTestId("archive-days-7"));
    expect(await screen.findByText("当前筛选无匹配会话")).toBeTruthy();
    expect(screen.queryByText(/没有非活跃会话/)).toBeNull();
    expect(screen.queryByText("暂无归档记录")).toBeNull();
  });
});

describe("ArchiveBoard 软归档徽标（体验批二）", () => {
  beforeEach(() => installFetch());
  afterEach(() => {
    vi.unstubAllGlobals();
    cleanup();
  });

  it("hiddenAlive 条目带「未结束」徽标，时间后缀为「活跃」", async () => {
    const alivePayload: ArchivedPayload = {
      archived: [{ ...payload.archived[0], hiddenAlive: true }],
      projects: ["proj-1"],
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify(alivePayload), {
            headers: { "content-type": "application/json" },
          })
      )
    );
    render(<ArchiveBoard onBack={() => {}} onOpenCard={() => {}} onUnpaired={() => {}} />);
    expect((await screen.findByTestId("alive-badge")).textContent).toBe("未结束");
    expect(screen.getByText(/活跃$/)).toBeTruthy();
  });
});
