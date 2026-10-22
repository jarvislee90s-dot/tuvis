import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import FilePreview from "@/mobile/FilePreview";
import type { Session } from "@/types/session";

beforeEach(async () => {
  // Task 7 的定位按钮文案走 i18n（本文件其余断言为硬编码中文，zh 与之一致）
  await i18n.changeLanguage("zh");
});

// M3 Task 8：文件预览三态（文本 / markdown / 图片）+ 错误态 + 模式标记。
// fetch 全量 stub；jsdom 无真实高亮，只断言容器与文本存在。

function makeSession(): Session {
  return {
    id: "sess-1",
    agentType: "claude",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: null,
    gitBranch: null,
    githubUrl: null,
    status: "processing",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-09-15T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "cli",
    jumpSupported: false,
    unread: false,
  };
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** /file 端点的 fetch stub（其余 URL 一律抛错，用例顺序错误立即暴露） */
function installFileFetch(impl: (url: string) => Response) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (!url.includes("/file?")) throw new Error(`unexpected fetch: ${url}`);
      return impl(url);
    })
  );
}

function renderPreview(
  filePath = "/tmp/proj/main.rs",
  mode: "split" | "fullscreen" = "fullscreen"
) {
  return render(
    <FilePreview session={makeSession()} filePath={filePath} mode={mode} onClose={() => {}} />
  );
}

describe("FilePreview：文本 / markdown / 图片三态", () => {
  it("文本文件：<pre><code> 渲染内容（带 hljs 容器）", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "fn main() {}", mime: "text/rust" }), {
          status: 200,
        })
    );
    renderPreview();
    const code = await screen.findByTestId("preview-code");
    expect(code.textContent).toContain("fn main() {}");
    expect(code.querySelector("code.hljs")).toBeTruthy();
  });

  it("markdown 文件：ReactMarkdown 渲染（标题/加粗）", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "# 标题\n\n**加粗**", mime: "text/markdown" }), {
          status: 200,
        })
    );
    renderPreview("/tmp/proj/README.md");
    const md = await screen.findByTestId("preview-markdown");
    expect(md.textContent).toContain("标题");
    // Bug 5：markdown 渲染容器挂 .md-body 排版类（标题/列表语义样式）
    expect(md.classList.contains("md-body")).toBe(true);
    expect(md.querySelector("h1")).toBeTruthy();
    expect(screen.getByText("加粗").tagName).toBe("STRONG");
  });

  it("图片文件：<img> object URL，卸载时 revoke", async () => {
    const createObjectURL = vi.fn(() => "blob:mock-url");
    const revokeObjectURL = vi.fn();
    Object.defineProperty(URL, "createObjectURL", {
      value: createObjectURL,
      configurable: true,
      writable: true,
    });
    Object.defineProperty(URL, "revokeObjectURL", {
      value: revokeObjectURL,
      configurable: true,
      writable: true,
    });
    installFileFetch(
      () =>
        // 字符串体（不用 Blob）：CI 的 Node 22 undici 对 Blob 体 Response 的
        // blob() 存在兼容性问题会抛错；分支判定只依赖 content-type 头
        new Response("fake-png-bytes", {
          status: 200,
          headers: { "content-type": "image/png" },
        })
    );
    const { unmount } = renderPreview("/tmp/proj/pic.png");
    const img = (await screen.findByTestId("preview-image")) as HTMLImageElement;
    expect(img.getAttribute("src")).toBe("blob:mock-url");
    unmount();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:mock-url");
  });
});

describe("FilePreview：错误态与模式标记", () => {
  it("403（越界/超限/不存在不可区分）→「无法预览该文件」+ 重试按钮", async () => {
    installFileFetch(() => new Response("", { status: 403 }));
    renderPreview();
    expect((await screen.findByTestId("preview-error")).textContent).toContain("无法预览该文件");
    expect(screen.getByTestId("preview-retry")).toBeTruthy();
  });

  it("网络异常 → 通用加载失败文案（不给探测预言机）", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    renderPreview();
    expect((await screen.findByTestId("preview-error")).textContent).toContain("预览加载失败");
  });

  it("mode 双态：data-mode 标记 split / fullscreen", async () => {
    installFileFetch(
      () => new Response(JSON.stringify({ content: "x", mime: "text/plain" }), { status: 200 })
    );
    const { unmount } = renderPreview("/tmp/proj/a.txt", "split");
    expect((await screen.findByTestId("file-preview")).getAttribute("data-mode")).toBe("split");
    unmount();
    renderPreview("/tmp/proj/a.txt", "fullscreen");
    expect((await screen.findByTestId("file-preview")).getAttribute("data-mode")).toBe(
      "fullscreen"
    );
  });
});

// ==== M5 B4：源码/渲染双态（仅 .md 与 .html 出现 seg）====

describe("FilePreview 源码/渲染 seg（M5 B4）", () => {
  it(".html：默认源码态；点「渲染」→ 沙箱 iframe（allow-scripts 无 same-origin）", async () => {
    installFileFetch(
      () =>
        new Response(
          JSON.stringify({
            content: "<h1>中东 2026 行程</h1><script>window.x=1</script>",
            mime: "text/html",
          }),
          { status: 200 }
        )
    );
    renderPreview("/tmp/proj/report.html");
    // 默认源码：code 分支
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
    // seg 在场且「源码」为当前态
    expect(screen.getByTestId("preview-seg")).toBeTruthy();
    expect(
      (screen.getByTestId("preview-seg-source") as HTMLButtonElement).getAttribute("aria-pressed")
    ).toBe("true");
    // 点「渲染」→ iframe：沙箱仅脚本（无 allow-same-origin），srcDoc 携带原文
    fireEvent.click(screen.getByTestId("preview-seg-render"));
    const frame = await screen.findByTestId("preview-html-frame");
    expect(frame.getAttribute("sandbox")).toBe("allow-scripts");
    expect(frame.getAttribute("srcdoc")).toContain("中东 2026 行程");
    expect(screen.queryByTestId("preview-code")).toBeNull();
    // 回「源码」：iframe 撤下，源码回来
    fireEvent.click(screen.getByTestId("preview-seg-source"));
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
    expect(screen.queryByTestId("preview-html-frame")).toBeNull();
  });

  it(".md：默认渲染态（现状延续）；点「源码」→ 高亮源码；切回渲染恢复 markdown", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "# 标题\n\n正文", mime: "text/markdown" }), {
          status: 200,
        })
    );
    renderPreview("/tmp/proj/NOTES.md");
    expect(await screen.findByTestId("preview-markdown")).toBeTruthy();
    expect(
      (screen.getByTestId("preview-seg-render") as HTMLButtonElement).getAttribute("aria-pressed")
    ).toBe("true");
    fireEvent.click(screen.getByTestId("preview-seg-source"));
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
    expect(screen.getByTestId("preview-code").textContent).toContain("# 标题");
    expect(screen.queryByTestId("preview-markdown")).toBeNull();
    fireEvent.click(screen.getByTestId("preview-seg-render"));
    expect(await screen.findByTestId("preview-markdown")).toBeTruthy();
  });

  it(".txt 等其余类型：seg 不出现（行为与 M3 一致）", async () => {
    installFileFetch(
      () => new Response(JSON.stringify({ content: "plain", mime: "text/plain" }), { status: 200 })
    );
    renderPreview("/tmp/proj/a.txt");
    await screen.findByTestId("preview-code");
    expect(screen.queryByTestId("preview-seg")).toBeNull();
  });
});

describe("FilePreview 403 原因分診（M5 P2-a）", () => {
  function case403(reason: string) {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ error: reason }), {
          status: 403,
          headers: { "content-type": "application/json" },
        })
    );
    renderPreview("/tmp/proj/x.txt");
  }

  it("sensitive → 安全策略文案", async () => {
    case403("sensitive");
    expect((await screen.findByTestId("preview-error")).textContent).toContain("安全策略保护");
  });

  it("too_large → 上限文案（文本 500KB / 图片 5MB）", async () => {
    case403("too_large");
    expect((await screen.findByTestId("preview-error")).textContent).toContain("500KB");
    expect((await screen.findByTestId("preview-error")).textContent).toContain("5MB");
  });

  it("not_found → 不存在文案", async () => {
    case403("not_found");
    expect((await screen.findByTestId("preview-error")).textContent).toContain("不存在或已被移动");
  });

  it("空错误体（旧后端/代理）→ 兜底文案「无法预览该文件」", async () => {
    installFileFetch(() => new Response("", { status: 403 }));
    renderPreview();
    expect((await screen.findByTestId("preview-error")).textContent).toContain("无法预览该文件");
  });
});

describe("FilePreview HTML 缩放与源码换行（M5 P2-b）", () => {
  it("HTML 渲染态：缩放控件 −/＋ 步进 25%（50%–200%），srcDoc 注入 zoom 样式", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "<p>hello</p>", mime: "text/html" }), {
          status: 200,
        })
    );
    renderPreview("/tmp/proj/page.html");
    await screen.findByTestId("preview-code"); // 等文本加载完成（seg 仅加载后出现）
    fireEvent.click(screen.getByTestId("preview-seg-render"));
    const frame = await screen.findByTestId("preview-html-frame");
    expect(frame.getAttribute("srcdoc")).toContain("zoom:1");
    // ＋ → 125% → srcDoc 注入更新
    fireEvent.click(screen.getByTestId("preview-zoom-in"));
    expect(frame.getAttribute("srcdoc")).toContain("zoom:1.25");
    // 越界钳制：连点 ＋ 到 200% 封顶
    fireEvent.click(screen.getByTestId("preview-zoom-in"));
    fireEvent.click(screen.getByTestId("preview-zoom-in"));
    fireEvent.click(screen.getByTestId("preview-zoom-in"));
    expect(frame.getAttribute("srcdoc")).toContain("zoom:2");
    // − 回落；百分比标签点击重置 100%
    fireEvent.click(screen.getByTestId("preview-zoom-out"));
    fireEvent.click(screen.getByTestId("preview-zoom-reset"));
    expect(frame.getAttribute("srcdoc")).toContain("zoom:1");
  });

  it("md 源码态软换行（pre-wrap），代码类源码不换行", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "# t", mime: "text/markdown" }), {
          status: 200,
        })
    );
    renderPreview("/tmp/proj/NOTES.md");
    await screen.findByTestId("preview-markdown");
    fireEvent.click(screen.getByTestId("preview-seg-source"));
    const pre = await screen.findByTestId("preview-code");
    expect(pre.className).toContain("whitespace-pre-wrap");
    expect(pre.className).toContain("break-words");
  });

  it("代码类（.rs）源码保持横向滚动不换行", async () => {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "fn a() {}", mime: "text/rust" }), { status: 200 })
    );
    renderPreview("/tmp/proj/a.rs");
    const pre = await screen.findByTestId("preview-code");
    expect(pre.className).not.toContain("whitespace-pre-wrap");
    expect(pre.className).toContain("overflow-auto");
  });
});

// ==== 任务 6：显式打开计数刷新 + 读取取消 ====

describe("FilePreview 当前内容刷新（任务 6）", () => {
  it("布局（mode）切换不重新读取；显式打开计数 refreshKey 递增才重读并显示新内容", async () => {
    let calls = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (!url.includes("/file?")) throw new Error(`unexpected fetch: ${url}`);
        calls += 1;
        return new Response(JSON.stringify({ content: `版本 ${calls}`, mime: "text/plain" }), {
          status: 200,
        });
      })
    );
    const { rerender } = render(
      <FilePreview
        session={makeSession()}
        filePath="/tmp/proj/live.txt"
        mode="fullscreen"
        refreshKey={0}
      />
    );
    expect((await screen.findByTestId("preview-code")).textContent).toContain("版本 1");
    expect(calls).toBe(1);

    // 宽窄屏 / 布局切换不是新的显式打开：不重读
    rerender(
      <FilePreview
        session={makeSession()}
        filePath="/tmp/proj/live.txt"
        mode="split"
        refreshKey={0}
      />
    );
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split");
    expect(screen.getByTestId("preview-code").textContent).toContain("版本 1");
    expect(calls).toBe(1);

    // 同一路径再次显式打开（refreshKey+1）→ 重新读取并显示磁盘当前内容
    rerender(
      <FilePreview
        session={makeSession()}
        filePath="/tmp/proj/live.txt"
        mode="split"
        refreshKey={1}
      />
    );
    await waitFor(() => {
      expect(screen.getByTestId("preview-code").textContent).toContain("版本 2");
    });
    expect(calls).toBe(2);
  });

  it("关闭/卸载时取消在途读取（AbortSignal 传递）", async () => {
    const signals: AbortSignal[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (_input: RequestInfo | URL, init?: RequestInit) =>
          new Promise<Response>((_resolve, reject) => {
            const signal = init?.signal;
            if (!signal) throw new Error("FilePreview 未把 AbortSignal 传给 fetch");
            signals.push(signal);
            signal.addEventListener("abort", () =>
              reject(new DOMException("aborted", "AbortError"))
            );
          })
      )
    );
    const { unmount } = render(
      <FilePreview
        session={makeSession()}
        filePath="/tmp/proj/big.png"
        mode="fullscreen"
        refreshKey={0}
      />
    );
    await waitFor(() => expect(signals.length).toBe(1));
    expect(signals[0].aborted).toBe(false);
    unmount();
    expect(signals[0].aborted).toBe(true);
  });

  it("换文件时取消旧读取并发起新读取", async () => {
    const signals: AbortSignal[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (!url.includes("/file?")) throw new Error(`unexpected fetch: ${url}`);
        if (init?.signal) signals.push(init.signal);
        const name = decodeURIComponent(url);
        const content = name.includes("a.txt") ? "A" : "B";
        return new Response(JSON.stringify({ content, mime: "text/plain" }), { status: 200 });
      })
    );
    const { rerender } = render(
      <FilePreview session={makeSession()} filePath="/tmp/proj/a.txt" mode="fullscreen" />
    );
    await screen.findByTestId("preview-code");
    expect(signals.length).toBe(1);
    rerender(<FilePreview session={makeSession()} filePath="/tmp/proj/b.txt" mode="fullscreen" />);
    await waitFor(() => {
      expect(screen.getByTestId("preview-code").textContent).toContain("B");
    });
    expect(signals[0].aborted).toBe(true);
    expect(signals.length).toBe(2);
  });
});

// ==== Task 7：预览页头的“最近引用”定位入口（与文件池同一查找流程）====

describe("FilePreview 定位最近引用（Task 7）", () => {
  function textFetch() {
    installFileFetch(
      () =>
        new Response(JSON.stringify({ content: "fn main() {}", mime: "text/rust" }), {
          status: 200,
        })
    );
  }

  it("提供 onLocate 时渲染定位按钮并回调", async () => {
    textFetch();
    const onLocate = vi.fn();
    render(
      <FilePreview
        session={makeSession()}
        filePath="/tmp/proj/main.rs"
        mode="fullscreen"
        onLocate={onLocate}
      />
    );
    await screen.findByTestId("preview-code");
    const locate = screen.getByTestId("preview-locate");
    expect(locate.getAttribute("aria-label")).toContain("定位");
    fireEvent.click(locate);
    expect(onLocate).toHaveBeenCalledTimes(1);
  });

  it("未提供 onLocate（其他工具/既有用法）不渲染定位按钮", async () => {
    textFetch();
    renderPreview();
    await screen.findByTestId("preview-code");
    expect(screen.queryByTestId("preview-locate")).toBeNull();
  });
});
