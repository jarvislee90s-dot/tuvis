import { render, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useMessageRenderers } from "@/mobile/message-render";

describe("useMessageRenderers：Markdown 文件链接", () => {
  it("只把可见文本路径变成文件按钮，代码路径仍以代码显示", () => {
    const path = "/tmp/proj/a.md";
    const text = [
      `正文 ${path}`,
      "行内：`" + path + "`",
      "```text\n" + path + "\n```",
      "> ```text\n> " + path + "\n> ````",
    ].join("\n\n");
    const { result } = renderHook(() =>
      useMessageRenderers({ openFile: vi.fn(), files: new Set([path]) })
    );
    const { container } = render(result.current.renderMarkdown(text));

    const links = container.querySelectorAll<HTMLButtonElement>("button[data-file-path]");
    expect(links).toHaveLength(1);
    expect(links[0].dataset.filePath).toBe(path);
    expect(container.querySelector("code")?.textContent).toBe(path);
  });

  it("引用式外链中的已知路径保持为外链，不嵌套文件按钮", () => {
    const path = "/tmp/proj/a.md";
    const text = `[${path}][doc]\n\n[doc]: https://example.com`;
    const { result } = renderHook(() =>
      useMessageRenderers({ openFile: vi.fn(), files: new Set([path]) })
    );
    const { container } = render(result.current.renderMarkdown(text));

    expect(container.querySelectorAll("button[data-file-path]")).toHaveLength(0);
    expect(container.querySelector("a[href='https://example.com']")?.textContent).toBe(path);
  });
});
