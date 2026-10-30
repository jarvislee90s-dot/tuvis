// 三轮 C：ZCode 面板附件接入行为锁 —— 与 MessageComposer 同一上传管线
// （api.uploadAttachment → /m/api/v1/session-attachment）、同一正文引用约定
// （attachments.attachmentMarkup），发送只走官方 /m/api/v1/zcode/actions。
//
// 契约：
// - 附件钮 `zcode-attach` + 隐藏 input `zcode-attachment-input`；条目
//   `zcode-attachment`（data-status = uploading/ready/failed）+ `zcode-attachment-remove`。
// - send 正文 = `text\n<image|file path>`；仅附件（空文本）也允许发送。
// - 附件闸：有 uploading/failed 条目时点发送 → 零动作 POST + `zcode-attach-notice`
//   可见（不静默丢附件）。
// - send accepted 只清「随本次发送」的条目；unknown/失败保留草稿与全部附件，
//   不自动重传/重发。
// - 切会话清附件并中断在途上传（XHR abort）。
// - 全程零旧 session-send / 零无头通道。

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import { ZCODE_ACTIONS_PATH } from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000a";
const SID2 = "session_0000000000000000000000000000000b";
const GEN = 3;

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

function capabilities() {
  return {
    send: flag(true),
    singleSelect: flag(true),
    multiSelect: flag(true),
    freeText: flag(true),
    interrupt: flag(true),
    approvalReply: flag(true),
    model: flag(true),
    thoughtLevel: flag(true),
  };
}

function connection() {
  return {
    state: "paired",
    generation: GEN,
    reasonCode: null,
    deviceLabel: "Synthetic Mac",
  };
}

function sessionView() {
  return {
    sessionId: SID,
    generation: GEN,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
  };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

let fetchMock: ReturnType<typeof vi.fn>;

function installFetch(actions: (actionId: string) => Record<string, unknown>) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.startsWith("/m/api/v1/zcode/connection")) return json(connection());
    if (url.startsWith("/m/api/v1/zcode/session?")) return json(sessionView());
    if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
      const body = JSON.parse(String(init.body)) as { actionId: string };
      return json(actions(body.actionId));
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

function postedActions(): { actionId: string; action: { kind: string; text?: string } }[] {
  return fetchMock.mock.calls
    .filter(
      ([input, init]) =>
        String(input) === ZCODE_ACTIONS_PATH && (init as RequestInit | undefined)?.method === "POST"
    )
    .map(([, init]) => JSON.parse(String((init as RequestInit).body)))
    .map((body: { actionId: string; action: { kind: string; text?: string } }) => body);
}

const legacySendCalls = () =>
  fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-send")).length;

// ==== FakeXHR：uploadAttachment 的 XHR 假体（同 MessageComposer.test 模式）====
const attachOpens: string[] = [];
const attachBodies: unknown[] = [];
let attachResponder: ((xhr: FakeXHR) => void) | null = null;

class FakeXHR {
  upload = { onprogress: null as ((e: ProgressEvent) => void) | null };
  onload: ((e: ProgressEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  status = 0;
  responseText = "";
  open(_method: string, url: string) {
    attachOpens.push(url);
  }
  setRequestHeader() {}
  send(_body: unknown) {
    attachBodies.push(_body);
    if (attachResponder) {
      attachResponder(this);
      return;
    }
    this.respond(
      200,
      JSON.stringify({ path: "/tmp/travel/.tuvis-attachments/s-1/a.png", size: 1 })
    );
  }
  abort() {
    this.onabort?.();
  }
  respond(status: number, body: string) {
    this.status = status;
    this.responseText = body;
    this.onload?.(new ProgressEvent("load"));
  }
}

function installXhr() {
  attachOpens.length = 0;
  attachBodies.length = 0;
  attachResponder = null;
  vi.stubGlobal("XMLHttpRequest", FakeXHR as unknown as typeof XMLHttpRequest);
}

function makeFile(name = "shot.png"): File {
  return new File(["binary"], name, { type: "image/png" });
}

async function attachOneFile(responder?: (xhr: FakeXHR) => void) {
  attachResponder = responder ?? null;
  fireEvent.change(screen.getByTestId("zcode-attachment-input"), {
    target: { files: [makeFile()] },
  });
  await waitFor(() => expect(screen.getByTestId("zcode-attachment")).toBeTruthy());
}

beforeEach(() => {
  vi.stubGlobal("EventSource", undefined);
  installXhr();
});

afterEach(async () => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  if (i18nLang() !== "zh") await changeLang("zh");
});

import i18n from "@/i18n";
const i18nLang = () => i18n.language;
const changeLang = (lang: string) => i18n.changeLanguage(lang);

describe("ZCode 面板：附件接入（三轮 C）", () => {
  it("二轮 P2-2：延迟 ACK 期间续写草稿 → accepted 只清附件、草稿保留，再发不带旧附件", async () => {
    // actions POST 挂起：回执延迟到续写之后才放行（accepted）。
    let releaseActions: ((actionId: string) => void) | null = null;
    const waiters: string[] = [];
    const deferredBodies: (string | undefined)[] = [];
    installXhr();
    fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.startsWith("/m/api/v1/zcode/connection")) return json(connection());
      if (url.startsWith("/m/api/v1/zcode/session?")) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        deferredBodies.push(init?.body as string | undefined);
        waiters.push(body.actionId);
        return new Promise((resolve) => {
          releaseActions = (id: string) => {
            const idx = waiters.indexOf(id);
            resolve(
              json({ actionId: id, state: idx === 0 ? "accepted" : "accepted", reasonCode: null })
            );
          };
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    await attachOneFile();

    // 发送「A + 附件」→ 不放行回执。
    fireEvent.change(draft, { target: { value: "A" } });
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(waiters).toHaveLength(1));

    // 等待期间续写 B（草稿已变化）。
    fireEvent.change(draft, { target: { value: "B" } });

    // 放行 accepted：附件（本次提交批次）必须清，草稿 B 保留。
    await act(async () => {
      releaseActions?.(waiters[0]);
    });
    await waitFor(() => expect(screen.queryByTestId("zcode-attachment")).toBeNull());
    expect((screen.getByTestId("zcode-draft") as HTMLTextAreaElement).value).toBe("B");

    // 再发 B：正文恰为 B，不再携带旧附件引用。
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(waiters).toHaveLength(2));
    await act(async () => {
      releaseActions?.(waiters[1]);
    });
    const texts = deferredBodies.map((b) =>
      String((JSON.parse(String(b)) as { action: { text?: string } }).action.text ?? "")
    );
    expect(texts[1]).toBe("B");
    expect(texts[1]).not.toContain("<image path=");
  });

  it("二轮 P2-1：离开页面（组件卸载）取消在途上传", async () => {
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    const aborted: boolean[] = [];
    const { unmount } = render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-draft");
    await attachOneFile((xhr) => {
      const original = xhr.abort.bind(xhr);
      xhr.abort = () => {
        aborted.push(true);
        original();
      };
      // 挂起：不放行
    });
    expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("uploading");

    // 实际挂载点以会话 id 为 key（SessionDetail）：切会话 = 卸载整个组件，
    // 卸载必须与「换 sessionId prop」同语义——取消在途上传。
    unmount();
    expect(aborted).toHaveLength(1);
  });
});

describe("ZCode 面板：附件接入（三轮 C）", () => {
  it("ready 附件随 send 进正文并按 accepted 清条目；零旧通道", async () => {
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    await attachOneFile();

    fireEvent.change(draft, { target: { value: "看这个" } });
    fireEvent.click(screen.getByTestId("zcode-send"));

    await waitFor(() => expect(postedActions()).toHaveLength(1));
    const action = postedActions()[0].action;
    expect(action.kind).toBe("send");
    expect(action.text).toContain("看这个");
    expect(action.text).toContain('<image path="/tmp/travel/.tuvis-attachments/s-1/a.png">');
    // 先等清理 effect 落地（chip 移除），再断言草稿——POST 计数到位早于 hook 的
    // accepted 清理，直接断言草稿会与 effect 竞态。
    await waitFor(() => expect(screen.queryByTestId("zcode-attachment")).toBeNull());
    expect(screen.getByTestId("zcode-draft").value).toBe("");
    expect(legacySendCalls()).toBe(0);
    expect(attachOpens[0]).toContain(
      `/m/api/v1/session-attachment?session_id=${SID}&name=shot.png`
    );
  });

  it("上传中点发送 → 零动作 POST + 可见提示；完成后可发送", async () => {
    let release: ((xhr: FakeXHR) => void) | null = null;
    const gate = new Promise<void>((resolve) => {
      release = () => resolve();
    });
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    await attachOneFile((xhr) => {
      // 挂起上传：测试手动放行
      void gate.then(() => xhr.respond(200, JSON.stringify({ path: "/p/a.png", size: 1 })));
    });

    fireEvent.change(draft, { target: { value: "带附件" } });
    fireEvent.click(screen.getByTestId("zcode-send"));
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
    expect(screen.getByTestId("zcode-attach-notice")).toBeVisible();

    // 放行上传 → ready → 再点发送 → 恰一次动作。
    await act(async () => {
      release?.();
    });
    await waitFor(() =>
      expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("ready")
    );
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(screen.queryByTestId("zcode-attach-notice")).toBeNull();
  });

  it("失败附件阻止发送并显示原因；移除后恢复；仅附件可发", async () => {
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-draft");
    await attachOneFile((xhr) => xhr.respond(413, JSON.stringify({ error: "too_large" })));
    await waitFor(() =>
      expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("failed")
    );

    // 仅附件（空文本）也会被 failed 条目挡住：零 POST + 提示。
    fireEvent.click(screen.getByTestId("zcode-send"));
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
    expect(screen.getByTestId("zcode-attach-notice")).toBeVisible();

    // 移除失败条目 → 仅附件（空文本）发送：正文恰为 markup。
    fireEvent.click(screen.getByTestId("zcode-attachment-remove"));
    await waitFor(() => expect(screen.queryByTestId("zcode-attachment")).toBeNull());
    await attachOneFile();
    await waitFor(() =>
      expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("ready")
    );
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));
    expect(postedActions()[0].action.text).toBe(
      '<image path="/tmp/travel/.tuvis-attachments/s-1/a.png">'
    );
  });

  it("unknown 回执保留草稿与附件（不自动重传/重发）", async () => {
    installFetch((actionId) => ({ actionId, state: "unknown", reasonCode: "unknown" }));
    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    await attachOneFile();
    fireEvent.change(draft, { target: { value: "未知也保留" } });
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(postedActions()).toHaveLength(1));

    // 草稿与附件条目都在；不重发（actions 仍只一次）。
    expect((screen.getByTestId("zcode-draft") as HTMLTextAreaElement).value).toBe("未知也保留");
    expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("ready");
    await act(async () => {});
    expect(postedActions()).toHaveLength(1);
  });

  it("切会话清附件并中断在途上传（XHR abort）", async () => {
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    const aborted: boolean[] = [];
    const hangAndTrack = (xhr: FakeXHR) => {
      const original = xhr.abort.bind(xhr);
      xhr.abort = () => {
        aborted.push(true);
        original();
      };
      // 挂起：不放行
    };
    const { rerender } = render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-draft");
    await attachOneFile(hangAndTrack);
    expect(screen.getByTestId("zcode-attachment").getAttribute("data-status")).toBe("uploading");

    rerender(<ZcodeControlPanel sessionId={SID2} />);
    await waitFor(() => expect(screen.queryByTestId("zcode-attachment")).toBeNull());
    expect(aborted).toHaveLength(1);
    expect(attachOpens[0]).toContain(`session_id=${SID}&`);
  });
});

// R4（U1）：附件「＋」旁的问号 = 既有存储说明的知情披露入口。
//
// 契约：
// - 默认折叠；点开展示**既有**说明文案（逐字复用 mobile.composer.attachHint，
//   不新写存储说明、不改存储目录、不接官方原生附件上传）；再点折叠。
// - `aria-expanded` 如实反映开合；`aria-controls` 指向真实存在的说明元素
//   （折叠态不指向未渲染元素 = 不悬空引用）。
// - 问号交互**零副作用**：不打开文件选择器（file input 未被 click）、不上传
//   （零 XHR）、不发送（零动作 POST）。
describe("ZCode 面板：附件问号说明（R4/U1）", () => {
  it("attach_help_toggles_existing_storage_hint_without_upload_or_send", async () => {
    installFetch((actionId) => ({ actionId, state: "accepted", reasonCode: null }));
    render(<ZcodeControlPanel sessionId={SID} />);
    await screen.findByTestId("zcode-draft");

    const help = screen.getByTestId("zcode-attach-help");
    const input = screen.getByTestId("zcode-attachment-input") as HTMLInputElement;
    const inputClick = vi.spyOn(input, "click");

    // 默认折叠：说明不渲染，问号如实自报未展开。
    expect(help.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByTestId("zcode-attach-hint")).toBeNull();

    // 点开：展示**既有**存储说明（逐字复用，不另写文案）。
    fireEvent.click(help);
    const hint = screen.getByTestId("zcode-attach-hint");
    expect(hint.textContent).toBe(i18n.t("mobile.composer.attachHint"));
    expect(hint.textContent).toContain(".tuvis-attachments");
    expect(help.getAttribute("aria-expanded")).toBe("true");
    // aria-controls 指向真实存在的说明元素（不悬空）。
    expect(help.getAttribute("aria-controls")).toBe(hint.id);
    expect(document.getElementById(hint.id)).toBe(hint);

    // 问号交互零副作用：不开文件选择器、不上传、不发送。
    expect(inputClick).not.toHaveBeenCalled();
    expect(attachOpens).toHaveLength(0);
    expect(postedActions()).toHaveLength(0);

    // 再点：折叠。
    fireEvent.click(help);
    expect(screen.queryByTestId("zcode-attach-hint")).toBeNull();
    expect(help.getAttribute("aria-expanded")).toBe("false");
    expect(inputClick).not.toHaveBeenCalled();
    expect(attachOpens).toHaveLength(0);
    expect(postedActions()).toHaveLength(0);
  });
});
