import type { ReactNode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";

const target = vi.hoisted(() => ({ session: "session-a", binding: "binding-a" }));

vi.mock("@/mobile/useOpenCodeNative", () => ({
  default: () => ({
    view: {
      sessionId: target.session,
      bindingToken: target.binding,
      revision: 1,
      complete: true,
      capabilities: { send: { allowed: true, evidence: "prompt", reasonCode: null } },
      messages: [],
      pending: [],
      inbox: [],
      runtimeState: "ready",
      files: {},
      route: "opencode_native",
      routeReasonCode: null,
    },
    error: null,
    readFailed: false,
    refresh: () => {},
    reselect: () => {},
  }),
}));

class PendingXHR {
  static requests: PendingXHR[] = [];
  upload = { onprogress: null };
  onload: (() => void) | null = null;
  onabort: (() => void) | null = null;
  onerror: (() => void) | null = null;
  status = 0;
  responseText = "";
  aborted = false;
  open() {}
  setRequestHeader() {}
  send() {
    PendingXHR.requests.push(this);
  }
  abort() {
    this.aborted = true;
    this.onabort?.();
  }
  succeed(path: string, excludeWarning?: string) {
    this.status = 200;
    this.responseText = JSON.stringify({ path, size: 1, excludeWarning });
    this.onload?.();
  }
}

let posts: Array<{ bindingToken: string; operation: { attachmentPaths: string[] } }>;
const response = (value: unknown) => new Response(JSON.stringify(value), { status: 200 });

beforeEach(() => {
  target.session = "session-a";
  target.binding = "binding-a";
  PendingXHR.requests = [];
  posts = [];
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  vi.stubGlobal("XMLHttpRequest", PendingXHR);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        posts.push(body);
        return response({ actionId: body.actionId, state: "confirmed" });
      }
      if (url.includes("/opencode/agents")) return response({ current: null, options: [] });
      if (url.includes("/opencode/outbox"))
        return response({ sessionId: target.session, revision: 0, paused: false, items: [] });
      if (url.includes("/opencode/actions/")) return response({ state: "confirmed" });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
});

function add(name: string, size = 1) {
  const file = new File([new Uint8Array(size)], name);
  fireEvent.change(screen.getByTestId("native-file-input"), { target: { files: [file] } });
}
function switchBinding(rerender: (ui: ReactNode) => void) {
  target.binding = "binding-b";
  rerender(<OpenCodeNativePanel sessionId={target.session} />);
}
async function sendText() {
  fireEvent.change(screen.getByTestId("native-input"), { target: { value: "message for B" } });
  await waitFor(() => expect(screen.getByTestId("native-send")).toBeEnabled());
  fireEvent.click(screen.getByTestId("native-send"));
  await waitFor(() => expect(posts).toHaveLength(1));
}

describe("原生附件排除警告", () => {
  it.each([true, false])("上传回执警告=%s 时按需显示且附件仍可发送", async (warning) => {
    render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("attachment.txt");
    await act(async () =>
      PendingXHR.requests[0].succeed(
        "/project/attachment.txt",
        warning ? "exclude failed" : undefined
      )
    );
    const chip = await screen.findByTestId("native-attachment");
    expect(chip).toHaveAttribute("data-status", "ready");
    if (warning)
      expect(within(chip).getByTestId("native-attachment-exclude-warning")).toBeVisible();
    else expect(within(chip).queryByTestId("native-attachment-exclude-warning")).toBeNull();
    await sendText();
    expect(posts[0].operation.attachmentPaths).toEqual(["/project/attachment.txt"]);
  });
});

describe("原生附件按会话和绑定隔离", () => {
  it("A 已完成的附件不随 B 的消息发送", async () => {
    const { rerender } = render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    await act(async () => PendingXHR.requests[0].succeed("/a/secret.txt"));
    await waitFor(() =>
      expect(screen.getByTestId("native-attachment")).toHaveAttribute("data-status", "ready")
    );
    switchBinding(rerender);
    await sendText();
    expect(posts[0].bindingToken).toBe("binding-b");
    expect(posts[0].operation.attachmentPaths).toEqual([]);
  });

  it("A 在途上传的迟到回执不污染 B 或随 B 发送", async () => {
    const { rerender } = render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    const old = PendingXHR.requests[0];
    switchBinding(rerender);
    await act(async () => old.succeed("/a/late.txt"));
    await sendText();
    expect(posts[0].bindingToken).toBe("binding-b");
    expect(posts[0].operation.attachmentPaths).toEqual([]);
    expect(screen.queryByText(/a\.txt/)).toBeNull();
  });

  it("移除在途附件时中止上传", async () => {
    render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    fireEvent.click(screen.getByTestId("native-attachment-remove"));
    expect(PendingXHR.requests[0].aborted).toBe(true);
  });

  it("切换绑定时中止旧上传", async () => {
    const { rerender } = render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    switchBinding(rerender);
    await act(async () => {});
    expect(PendingXHR.requests[0].aborted).toBe(true);
  });

  it("切换会话时中止旧上传", async () => {
    const { rerender } = render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    target.session = "session-b";
    target.binding = "binding-b";
    rerender(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    expect(PendingXHR.requests[0].aborted).toBe(true);
  });

  it("卸载面板时中止在途上传", async () => {
    const { unmount } = render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("a.txt");
    unmount();
    expect(PendingXHR.requests[0].aborted).toBe(true);
  });

  it("移除在途上传释放预算前必须中止它，避免旧上传绕过总预算", async () => {
    render(<OpenCodeNativePanel sessionId={target.session} />);
    await act(async () => {});
    add("old.bin", 5 * 1024 * 1024);
    const old = PendingXHR.requests[0];
    fireEvent.click(screen.getByTestId("native-attachment-remove"));
    for (let i = 0; i < 4; i++) add(`new-${i}.bin`, 5 * 1024 * 1024);
    add("overflow.bin");
    const overflow = screen
      .getAllByTestId("native-attachment")
      .find((chip) => chip.textContent?.includes("overflow.bin"));
    expect(overflow && within(overflow).getByTestId("native-attachment-error")).toHaveAttribute(
      "data-error-code",
      "message_limit"
    );
    expect(PendingXHR.requests).toHaveLength(5);
    expect(old.aborted).toBe(true);
  });
});
