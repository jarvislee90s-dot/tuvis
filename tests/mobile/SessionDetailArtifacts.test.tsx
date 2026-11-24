// 任务 6：SessionDetail × IPC 正文链接与附件卡片的接线锁。
// 覆盖：IPC 正文沿用既有链接渲染器点击开预览；文档 chip / 图片卡走同一 openFile；
// 预览已打开时再次显式打开同一路径必须重读并显示新内容；每回合 12 图上限且超额不读。
// 任务 7：文件池来源合并/排序、来源按钮与文件名预览两个独立热区、五页有界定位
// （当前页零请求 / 五页未命中四次更早读取）、窄屏先回消息区、滚动高亮两秒、失败可返回预览。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SessionDetail from "@/mobile/SessionDetail";
import type { ProjectedArtifact, ProjectedMessage, ThreadProjection } from "@/mobile/codexIpcApi";
import type { SessionFileEntry, SessionMessage } from "@/mobile/api";
import type { Session } from "@/types/session";
import { MockEventSource } from "./eventSourceMock";
import { MockIntersectionObserver } from "./intersectionObserverMock";

function makeSession(over: Partial<Session> = {}): Session {
  return {
    id: "sess-codex",
    agentType: "codex",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "codex 会话",
    gitBranch: null,
    githubUrl: null,
    status: "idle",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-10-20T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "app",
    jumpSupported: false,
    unread: false,
    ...over,
  };
}

function fileEntry(path: string, over: Partial<SessionFileEntry> = {}): SessionFileEntry {
  return {
    path,
    fileKey: `k-${path}`,
    lastSeq: 1,
    lastTs: 1000,
    hits: 1,
    modified: false,
    origin: "user",
    ...over,
  };
}

function artifact(
  over: Partial<ProjectedArtifact> & Pick<ProjectedArtifact, "artifactId">
): ProjectedArtifact {
  return {
    kind: "document",
    source: "mention",
    path: null,
    url: null,
    fileKey: null,
    ordinal: 1,
    imageIndexInTurn: null,
    summary: null,
    sourceStatus: null,
    changeKind: null,
    unavailableReason: null,
    ...over,
  };
}

function projectedMessage(
  over: Partial<ProjectedMessage> & Pick<ProjectedMessage, "id">
): ProjectedMessage {
  return {
    role: "user",
    kind: "userMessage",
    text: "",
    ts: null,
    turnId: "turn-1",
    artifacts: [],
    turnImageCount: 0,
    omittedArtifactCount: 0,
    ...over,
  };
}

function projection(messages: ProjectedMessage[]): ThreadProjection {
  return {
    threadId: "thread-1",
    revision: 5,
    stale: false,
    availability: "controllable",
    capabilities: {
      send: true,
      state: true,
      command: true,
      permissions: false,
      blockingInput: true,
    },
    runtimeStatus: "idle",
    contentState: "ready",
    messages,
    truncated: false,
    pending: [],
    historyToken: "hist-1",
  };
}

let files: SessionFileEntry[];
let fileCalls: string[];
let fileListCalls: string[];
let fileVersion = 0;
// Task 7：同代历史页请求（五页定位）
let pageCalls: string[];
let pageHandler: (callIndex: number) => Response;

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

/** 历史页响应体（Task 4 契约；ready 时 messages 才有效） */
function pageBody(
  messages: ProjectedMessage[],
  over: {
    status?: string;
    reasonCode?: string | null;
    historyToken?: string;
    nextBefore?: string | null;
  } = {}
) {
  return json({
    status: over.status ?? "ready",
    messages,
    historyToken: over.historyToken ?? "hist-2",
    nextBefore: over.nextBefore === undefined ? "cursor-next" : over.nextBefore,
    reasonCode: over.reasonCode ?? null,
    revision: 5,
  });
}

type TestModelSummary = { model: string; reasoningEffort?: string; source: "next" | "last_used" };

function installFetch(dsh?: {
  modelSummary?: TestModelSummary;
  messages: (limit: number) => { messages: SessionMessage[]; truncated: boolean };
  media: TestDshMedia[];
  read: (url: URL, method: string) => Response;
}) {
  files = [];
  fileCalls = [];
  fileListCalls = [];
  fileVersion = 0;
  pageCalls = [];
  pageHandler = () => json({ status: "end", messages: [], historyToken: "", nextBefore: null });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = input instanceof Request ? input.url : String(input);
      if (dsh && url.includes("/dsh/media?")) {
        return dsh.read(
          new URL(url, "http://localhost"),
          init?.method ?? (input instanceof Request ? input.method : "GET")
        );
      }
      if (url.includes("/codex/thread/events")) return new Response("", { status: 200 });
      if (url.includes("/codex/queue")) return json({ actions: [], blockedReasonCode: null });
      if (url.includes("/codex/thread/messages")) {
        const index = pageCalls.length;
        pageCalls.push(url);
        return pageHandler(index);
      }
      if (url.includes("/codex/thread")) return json(projection([]));
      if (url.includes("/session-messages")) {
        const limit = Number(new URL(url, "http://localhost").searchParams.get("limit"));
        return json({
          ...(dsh?.messages(limit) ?? { messages: [], truncated: false }),
          ...(dsh?.modelSummary ? { dshModelSummary: dsh.modelSummary } : {}),
        });
      }
      if (url.includes("/session-files")) {
        fileListCalls.push(url);
        return json({ files, truncated: false, media: dsh?.media });
      }
      if (url.includes("/session-subagents")) return json({ subagents: [] });
      if (url.includes("/session-queue")) return json({ items: [] });
      if (url.includes("/session-approve-options")) {
        return json({
          available: false,
          options: [],
          verifiedWith: "t",
          currentVersion: null,
          drift: false,
        });
      }
      if (url.includes("/session-question")) return json({ available: false, questions: [] });
      if (url.includes("/session-send-info")) {
        return json({ injectable: false, channels: [], visibility: "none" });
      }
      if (url.includes("/file?")) {
        fileCalls.push(url);
        const path = decodeURIComponent(
          new URL(url, "http://localhost").searchParams.get("path") ?? ""
        );
        if (path.endsWith(".png")) {
          return new Response("png-bytes", {
            status: 200,
            headers: { "content-type": "image/png" },
          });
        }
        fileVersion += 1;
        return json({ content: `版本 ${fileVersion}`, mime: "text/plain" });
      }
      return json({});
    })
  );
}

function emit(es: MockEventSource, type: string, body: unknown) {
  act(() => es.emit(type, body));
}

beforeEach(async () => {
  await i18n.changeLanguage("zh");
  MockEventSource.reset();
  MockIntersectionObserver.install();
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  Object.defineProperty(URL, "createObjectURL", {
    value: vi.fn(() => "blob:detail-image"),
    configurable: true,
    writable: true,
  });
  Object.defineProperty(URL, "revokeObjectURL", {
    value: vi.fn(),
    configurable: true,
    writable: true,
  });
  // 窄屏默认语义（jsdom 无 matchMedia）
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    configurable: true,
    value: undefined,
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SessionDetail × IPC 附件：正文链接与卡片接线", () => {
  it("IPC 正文已有路径沿用既有链接器，点击打开当前文件内容", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/notes.md")];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection([
        projectedMessage({
          id: "m1",
          text: "请查看 /tmp/proj/notes.md 的内容",
        }),
      ])
    );
    expect(await screen.findByTestId("codex-msg-m1")).toBeTruthy();
    fireEvent.click(await screen.findByTestId("file-link"));
    const preview = await screen.findByTestId("preview-code");
    expect(preview.textContent).toContain("版本 1");
    expect(fileCalls.length).toBe(1);
    expect(decodeURIComponent(fileCalls[0])).toContain("/tmp/proj/notes.md");
  });

  it("预览已打开时再次点同一路径的附件：重新读取并显示当前内容", async () => {
    installFetch();
    const doc = artifact({
      artifactId: "doc-1",
      kind: "document",
      source: "mention",
      path: "/tmp/proj/doc.md",
    });
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection([
        projectedMessage({
          id: "m1",
          role: "user",
          text: "",
          artifacts: [doc],
        }),
      ])
    );
    const chip = await screen.findByTestId("artifact-chip-doc-1");
    fireEvent.click(chip);
    await waitFor(() => expect(screen.getByTestId("preview-code").textContent).toContain("版本 1"));
    expect(fileCalls.length).toBe(1);

    // 预览仍开着、同一路径再次显式打开 → 以显式打开计数为刷新键重读
    fireEvent.click(chip);
    await waitFor(() => expect(screen.getByTestId("preview-code").textContent).toContain("版本 2"));
    expect(fileCalls.length).toBe(2);
  });

  it("服务端预算把附件全部省略：消息级提示仍进入消息列且只出现一次", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection([
        projectedMessage({
          id: "m1",
          text: "看看这个附件",
          // artifacts 为空：服务端预算把唯一附件也省略了，仍须如实提示
          omittedArtifactCount: 2,
        }),
      ])
    );
    await screen.findByTestId("codex-msg-m1");
    const notes = screen.getAllByTestId("artifact-server-omitted");
    expect(notes).toHaveLength(1);
    expect(notes[0].textContent).toContain("2");
    // 不冒充「仅显示最近的消息」横幅
    expect(screen.queryByTestId("codex-projection-truncated")).toBeNull();
  });

  it("每回合最多 12 张图片卡：汇总一次，超额图片不读取", async () => {
    installFetch();
    const images = Array.from({ length: 13 }, (_, i) =>
      artifact({
        artifactId: `img-${i + 1}`,
        kind: "image",
        source: "image_generation",
        path: `/tmp/proj/gen-${i + 1}.png`,
        imageIndexInTurn: i + 1,
      })
    );
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection([
        projectedMessage({
          id: "m1",
          role: "system",
          kind: "imageGeneration",
          text: "",
          artifacts: images,
          turnImageCount: 13,
        }),
      ])
    );
    expect(await screen.findByTestId("artifact-card-img-1")).toBeTruthy();
    expect(screen.getAllByTestId(/^artifact-card-/).length).toBe(12);
    expect(screen.queryByTestId("artifact-card-img-13")).toBeNull();
    expect(screen.getAllByTestId("artifact-image-overflow").length).toBe(1);

    act(() => MockIntersectionObserver.triggerAll(true));
    await waitFor(() => expect(fileCalls.length).toBe(12));
    // 稳定后仍只有 12 次图片读取（第 13 张未挂载、未请求）
    await new Promise((r) => setTimeout(r, 20));
    expect(fileCalls.length).toBe(12);
  });
});

// ==== Task 7：文件池来源与五页反向定位 ====

describe("SessionDetail × 文件池来源与反向定位（Task 7）", () => {
  let scrollSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    // jsdom 未实现 scrollIntoView：本组用例断言“滚动到命中节点”
    scrollSpy = vi.fn();
    Object.defineProperty(Element.prototype, "scrollIntoView", {
      value: scrollSpy,
      configurable: true,
      writable: true,
    });
  });

  /** 起一个已就绪的 Codex 详情页，并发出指定投影快照 */
  async function mountWith(messages: ProjectedMessage[]) {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection(messages));
    for (const message of messages) {
      await screen.findByTestId(`codex-msg-${message.id}`);
    }
  }

  /** 依次：打开文件面板 → 文件名打开预览 → 预览页头点定位 */
  async function locateFromPreview(rowIndex = 0) {
    fireEvent.click(screen.getByTestId("file-panel-button"));
    fireEvent.click(await screen.findByTestId(`file-row-${rowIndex}-open`));
    await screen.findByTestId("preview-code");
    fireEvent.click(screen.getByTestId("preview-locate"));
  }

  it("文件池合并：同文件只显示一次、已有来源与已修改标志保持、补充条目按来源映射并倒序排列", async () => {
    installFetch();
    files = [
      fileEntry("/tmp/proj/keep.md"),
      fileEntry("/tmp/proj/written.md", { origin: "tool_write", modified: true }),
      // 与 m1 的 file_change 附件同 fileKey：合并成一行，既有来源（tool_read）不被提升
      fileEntry("/tmp/proj/new.md", { origin: "tool_read", modified: false }),
    ];
    await mountWith([
      projectedMessage({
        id: "m1",
        text: "看 /tmp/proj/keep.md",
        artifacts: [
          artifact({
            artifactId: "chg-1",
            kind: "document",
            source: "file_change",
            path: "/tmp/proj/new.md",
            fileKey: "k-/tmp/proj/new.md",
            sourceStatus: "in_progress",
            changeKind: "modify",
          }),
        ],
      }),
    ]);
    fireEvent.click(screen.getByTestId("file-panel-button"));
    await screen.findByTestId("file-panel");

    expect(screen.getAllByTestId(/^file-row-\d+$/)).toHaveLength(3);
    // 最近引用倒序：new.md（附件节点）→ keep.md（正文）→ written.md（无引用，保持后端相对顺序）
    expect(screen.getByTestId("file-row-0-open").textContent).toContain("new.md");
    expect(screen.getByTestId("file-row-0-origin").textContent).toBe("工具读取");
    expect(screen.getByTestId("file-row-0-open").className).not.toContain("font-semibold");
    expect(screen.getByTestId("file-row-0-locate").textContent).toBe("修改");
    expect(screen.getByTestId("file-row-0-status").textContent).toBe("in_progress");
    expect(screen.getByTestId("file-row-1-open").textContent).toContain("keep.md");
    expect(screen.getByTestId("file-row-1-origin").textContent).toBe("我上传的");
    expect(
      within(screen.getByTestId("file-row-2")).getByTestId("file-row-2-open").textContent
    ).toContain("written.md");
    // 写过不撤销：后端 tool_write / modified=true 原样保留
    expect(screen.getByTestId("file-row-2-open").className).toContain("font-semibold");
  });

  it("文件名预览与来源按钮是两个独立热区；当前页命中零分页请求，滚动并高亮两秒", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/notes.md")];
    await mountWith([projectedMessage({ id: "m1", text: "看 /tmp/proj/notes.md" })]);

    // 文件名热区：打开预览，不触发定位、零分页请求
    fireEvent.click(screen.getByTestId("file-panel-button"));
    fireEvent.click(await screen.findByTestId("file-row-0-open"));
    await screen.findByTestId("preview-code");
    expect(pageCalls).toHaveLength(0);
    expect(screen.queryByTestId("artifact-jump-banner")).toBeNull();
    fireEvent.click(screen.getByTestId("preview-close"));

    // 来源按钮热区：定位当前页引用（窄屏先回消息区）
    fireEvent.click(screen.getByTestId("file-panel-button"));
    fireEvent.click(await screen.findByTestId("file-row-0-locate"));
    const li = await screen.findByTestId("codex-msg-m1");
    await waitFor(() => expect(li.getAttribute("data-jump-hit")).toBe("true"));
    expect(screen.getByTestId("artifact-jump-banner").getAttribute("data-phase")).toBe("hit");
    expect(pageCalls).toHaveLength(0);
    expect(scrollSpy).toHaveBeenCalled();
    // 高亮约两秒后自动清理
    await waitFor(() => expect(li.getAttribute("data-jump-hit")).toBeNull(), { timeout: 3000 });
  });

  it("仅正文链接出现的文件也能从预览定位：窄屏先切回消息区，可返回原预览", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/notes.md")];
    await mountWith([projectedMessage({ id: "m1", text: "看 /tmp/proj/notes.md" })]);

    // 从正文链接进入预览（窄屏 = fullscreen 浮层）
    fireEvent.click(screen.getByTestId("file-link"));
    await screen.findByTestId("preview-code");
    expect(screen.queryByTestId("preview-shell")).not.toBeNull();
    fireEvent.click(screen.getByTestId("preview-locate"));
    // 先切回消息区（浮层关闭），命中当前页
    await waitFor(() => expect(screen.queryByTestId("preview-shell")).toBeNull());
    const li = await screen.findByTestId("codex-msg-m1");
    await waitFor(() => expect(li.getAttribute("data-jump-hit")).toBe("true"));
    expect(pageCalls).toHaveLength(0);
    // 失败/结束后仍可返回原文件预览（此处验证命中后同样保留）
    fireEvent.click(screen.getByTestId("artifact-jump-return-preview"));
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
  });

  it("五页未命中：恰好四次更早读取后停止，提示限定范围且不提示继续加载", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/missing.md")];
    pageHandler = (index) =>
      pageBody([projectedMessage({ id: `older-${index}`, text: "更早消息，无引用" })]);
    await mountWith([projectedMessage({ id: "m1", text: "无关内容" })]);

    await locateFromPreview();
    const banner = await screen.findByTestId("artifact-jump-banner");
    await waitFor(() => expect(banner.getAttribute("data-phase")).toBe("miss"));
    expect(pageCalls).toHaveLength(4);
    expect(banner.textContent).toContain("最近五页消息中未找到");
    expect(banner.textContent).toContain("不代表");
    // 未命中不提示继续加载、也不把失败态伪装成未命中
    expect(screen.queryByTestId("load-more")).toBeNull();
    expect(screen.queryByTestId("artifact-jump-retry")).toBeNull();
    // 失败/未命中仍可返回原文件预览
    fireEvent.click(screen.getByTestId("artifact-jump-return-preview"));
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
  });

  it("加载失败单独提示并停止，可重试（新动作）且可返回预览", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/missing.md")];
    pageHandler = () => pageBody([], { status: "unavailable", reasonCode: "request_failed" });
    await mountWith([projectedMessage({ id: "m1", text: "无关内容" })]);

    await locateFromPreview();
    const banner = await screen.findByTestId("artifact-jump-banner");
    await waitFor(() => expect(banner.getAttribute("data-phase")).toBe("failed"));
    expect(banner.getAttribute("data-failure-kind")).toBe("load_failed");
    expect(banner.textContent).toContain("读取更早消息失败");
    expect(banner.textContent).not.toContain("最近五页消息中未找到");
    expect(pageCalls).toHaveLength(1);
    // 可重试：一次新动作（再次失败时仍是独立失败态）
    fireEvent.click(screen.getByTestId("artifact-jump-retry"));
    await waitFor(() => expect(pageCalls).toHaveLength(2));
    expect(screen.getByTestId("artifact-jump-banner").getAttribute("data-failure-kind")).toBe(
      "load_failed"
    );
    fireEvent.click(screen.getByTestId("artifact-jump-return-preview"));
    expect(await screen.findByTestId("preview-code")).toBeTruthy();
  });

  it("历史不完整 / 游标失效 / 代际失效各自独立提示并停止", async () => {
    installFetch();
    files = [fileEntry("/tmp/proj/missing.md")];
    await mountWith([projectedMessage({ id: "m1", text: "无关内容" })]);

    const rounds: Array<[string, () => Response, string]> = [
      ["history_incomplete", () => pageBody([], { status: "loading" }), "历史尚未完整载入"],
      [
        "cursor_invalid",
        () => pageBody([], { status: "cursor_invalid", reasonCode: "cursor_not_found" }),
        "历史游标已失效",
      ],
      [
        "generation_invalid",
        () => pageBody([], { status: "unavailable", reasonCode: "generation_mismatch" }),
        "历史代际已失效",
      ],
    ];
    let first = true;
    for (const [kind, handler, text] of rounds) {
      pageHandler = handler;
      if (first) {
        await locateFromPreview();
        first = false;
      } else {
        // 失败后回到预览，再发起一次新动作
        fireEvent.click(await screen.findByTestId("artifact-jump-return-preview"));
        await screen.findByTestId("preview-code");
        fireEvent.click(screen.getByTestId("preview-locate"));
      }
      const banner = await screen.findByTestId("artifact-jump-banner");
      await waitFor(() => expect(banner.getAttribute("data-failure-kind")).toBe(kind));
      expect(banner.textContent).toContain(text);
      expect(banner.textContent).not.toContain("最近五页消息中未找到");
    }
    expect(pageCalls).toHaveLength(3);
  });
});

// T03：在 HTTP 边界描述尚未落地的可选字段，RED 时也能运行真实详情组件。
type TestDshMedia = { refId: string; kind: "image" | "file"; name: string };
type TestDshMessage = SessionMessage & {
  media?: TestDshMedia[];
  blocks?: ({ kind: "text"; text: string } | { kind: "media"; refId: string })[];
};

const pureImage = { refId: "ref-pure", kind: "image", name: "pure.png" } as const;
const mixedImage = { refId: "ref-mixed", kind: "image", name: "mixed.png" } as const;
const presentFile = { refId: "ref-present", kind: "file", name: "report.txt" } as const;
const presentToolArgs = JSON.stringify({
  files: [{ path: `/tmp/proj/${presentFile.name}`, description: "交付报告" }],
});
// 两个接口可提供不同显示名；文件池以 refId 而非文件名合并。
const indexedPresentFile = { ...presentFile, name: "indexed-report.txt" };
const olderPoolFile = { refId: "ref-old", kind: "file", name: "earlier.txt" } as const;
const dshSession = makeSession({
  id: "dsh-session-t03",
  dshInstanceId: "dsh-instance-t03",
  agentType: "dsh",
  status: "processing",
});
const ordinaryPath = "/tmp/proj/notes.md";

function dshMessage(seq: number, over: Partial<TestDshMessage>): TestDshMessage {
  return { seq, role: "user", kind: "user", content: "", ts: null, collapsed: false, ...over };
}

function mountDsh({
  failMixedImage = false,
  status = dshSession.status,
  dshInstanceId = dshSession.dshInstanceId,
  modelSummary,
}: {
  failMixedImage?: boolean;
  status?: Session["status"];
  dshInstanceId?: Session["dshInstanceId"];
  modelSummary?: TestModelSummary;
} = {}) {
  const reads = vi.fn((url: URL, _method: string) => {
    const refId = url.searchParams.get("refId");
    if (refId === mixedImage.refId && failMixedImage) {
      return json({ error: "not_found" }, 404);
    }
    if (refId === pureImage.refId || refId === mixedImage.refId) {
      return new Response("png-bytes", { headers: { "content-type": "image/png" } });
    }
    if (refId === presentFile.refId) {
      return json({ content: "交付文件正文", mime: "text/plain" });
    }
    return json({ error: "not_found" }, 404);
  });
  const tail = [
    dshMessage(2, { media: [pureImage] }),
    dshMessage(3, {
      content: "图片前面的文字 图片后面的文字",
      media: [mixedImage],
      blocks: [
        { kind: "text", text: "图片前面的文字" },
        { kind: "media", refId: mixedImage.refId },
        { kind: "text", text: "图片后面的文字" },
      ],
    }),
    dshMessage(4, {
      role: "assistant",
      kind: "tool-call",
      toolName: "present",
      toolArgs: presentToolArgs,
      collapsed: true,
      media: [presentFile],
    }),
    dshMessage(5, { content: `普通路径：${ordinaryPath}` }),
  ];
  installFetch({
    modelSummary,
    messages: (limit) =>
      limit > 200
        ? { messages: [dshMessage(1, { content: "更早的普通消息" }), ...tail], truncated: false }
        : { messages: tail, truncated: true },
    media: [pureImage, mixedImage, indexedPresentFile, olderPoolFile],
    read: reads,
  });
  files = [fileEntry(ordinaryPath)];
  render(<SessionDetail session={{ ...dshSession, status, dshInstanceId }} onBack={() => {}} />);
  return reads;
}

function precedes(first: HTMLElement, second: HTMLElement) {
  expect(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
}

function expectReferenceGet(url: URL, method: string, refId: string) {
  expect(method.toUpperCase()).toBe("GET");
  expect(url.pathname).toBe("/m/api/v1/dsh/media");
  expect([...url.searchParams.entries()].sort()).toEqual([
    ["instanceId", dshSession.dshInstanceId],
    ["refId", refId],
    ["sessionId", dshSession.id],
  ]);
}

describe("SessionDetail × DSH 媒体引用（T03）", () => {
  beforeEach(() => {
    vi.stubGlobal("innerWidth", 390);
    vi.stubGlobal("innerHeight", 844);
  });

  it("纯图片消息在自身气泡渲染，并可打开图片预览", async () => {
    const reads = mountDsh();
    const row = await screen.findByTestId("msg-2");
    act(() => MockIntersectionObserver.triggerAll());
    await within(row).findByRole("img", { name: pureImage.name });

    fireEvent.click(within(row).getByRole("button", { name: /pure\.png/ }));
    expect(await screen.findByTestId("preview-image")).toBeTruthy();
    const pureReads = reads.mock.calls.filter(
      ([url]) => url.searchParams.get("refId") === pureImage.refId
    );
    expect(pureReads.length).toBeGreaterThan(0);
    for (const [url, method] of pureReads) expectReferenceGet(url, method, pureImage.refId);
    expect(fileCalls).toEqual([]);
  });

  it("图文气泡按 blocks 顺序显示文字、图片、文字，不重复显示兼容正文", async () => {
    mountDsh();
    const row = within(await screen.findByTestId("msg-3"));
    act(() => MockIntersectionObserver.triggerAll());
    const image = await row.findByRole("img", { name: mixedImage.name });
    precedes(row.getByText("图片前面的文字"), image);
    precedes(image, row.getByText("图片后面的文字"));
    expect(row.queryByText("图片前面的文字 图片后面的文字")).toBeNull();
  });

  it("图片引用读取失败时以可读说明替代图片，保留该条图文气泡的两段文字", async () => {
    mountDsh({ failMixedImage: true });
    const row = within(await screen.findByTestId("msg-3"));
    act(() => MockIntersectionObserver.triggerAll());
    const placeholder = await row.findByText(/无法|失败|不可用|未找到|不存在/);
    precedes(row.getByText("图片前面的文字"), placeholder);
    precedes(placeholder, row.getByText("图片后面的文字"));
    expect(row.queryByRole("img", { name: mixedImage.name })).toBeNull();
  });

  describe.each([
    { mode: "运行中默认模式", status: "processing", summary: false },
    { mode: "总结模式", status: "idle", summary: true },
  ] as const)("F04 · $mode", ({ status, summary }) => {
    it("不展开 present 工具详情就能看到交付卡并打开预览", async () => {
      const user = userEvent.setup();
      const reads = mountDsh({ status });
      const row = within(await screen.findByTestId("msg-4"));
      expect(screen.queryByTestId("summary-banner") !== null).toBe(summary);
      const toggle = row.getByRole("button", { name: /present/, expanded: false });
      const card = row.getByRole("button", { name: presentFile.name });
      expect(card).toBeVisible();

      await user.click(card);
      expect((await screen.findByTestId("preview-code")).textContent).toContain("交付文件正文");
      expect(toggle).toHaveAttribute("aria-expanded", "false");
      expect(reads).toHaveBeenCalledTimes(1);
      expectReferenceGet(...reads.mock.calls[0], presentFile.refId);
      expect(fileCalls).toEqual([]);
    });

    it("present 参数只在展开后可见，交付卡不重复", async () => {
      const user = userEvent.setup();
      mountDsh({ status });
      const row = within(await screen.findByTestId("msg-4"));
      expect(screen.queryByTestId("summary-banner") !== null).toBe(summary);
      const toggle = row.getByRole("button", { name: /present/, expanded: false });
      expect(row.queryByText(presentToolArgs)).toBeNull();
      const card = row.getByRole("button", { name: presentFile.name });
      expect(card).toBeVisible();

      await user.click(toggle);
      expect(toggle).toHaveAttribute("aria-expanded", "true");
      expect(row.getByText(presentToolArgs)).toBeVisible();
      const cards = row.getAllByRole("button", { name: presentFile.name });
      expect(cards).toHaveLength(1);
      expect(cards[0]).toBeVisible();
    });
  });

  it("右侧文件池按 refId 合并不同显示名的 present 交付件，只保留一项并按引用打开", async () => {
    const user = userEvent.setup();
    const reads = mountDsh();
    await screen.findByTestId("msg-4");
    await user.click(screen.getByTestId("file-panel-button"));
    const pool = within(await screen.findByTestId("file-panel"));
    await pool.findByRole("button", { name: olderPoolFile.name });
    for (const media of [pureImage, mixedImage]) {
      expect(pool.getAllByRole("button", { name: media.name })).toHaveLength(1);
    }
    const deliveryRows = pool.getAllByRole("button", { name: /^(indexed-)?report\.txt$/ });
    expect(deliveryRows).toHaveLength(1);
    await user.click(deliveryRows[0]);
    expect((await screen.findByTestId("preview-code")).textContent).toContain("交付文件正文");
    expect(reads).toHaveBeenCalledTimes(1);
    expectReferenceGet(...reads.mock.calls[0], presentFile.refId);
    expect(fileCalls).toEqual([]);
  });

  it("同页普通正文链接继续走原文件预览，关闭后回到消息", async () => {
    const reads = mountDsh();
    fireEvent.click(await within(await screen.findByTestId("msg-5")).findByTestId("file-link"));
    expect((await screen.findByTestId("preview-code")).textContent).toContain("版本 1");
    expect(fileCalls).toHaveLength(1);
    expect(new URL(fileCalls[0], "http://localhost").searchParams.get("path")).toBe(ordinaryPath);
    expect(reads).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(screen.queryByTestId("preview-code")).toBeNull();
    expect(within(screen.getByTestId("msg-5")).getByTestId("file-link")).toBeTruthy();
  });

  it("加载更早消息会将其放在已有含媒体消息前，并补偿顶部插入的滚动高度", async () => {
    mountDsh();
    await screen.findByTestId("msg-2");
    // 首拉对齐 effect 可能迟于 DOM 提交；先冲刷再设置当前阅读位置。
    await act(async () => {});
    const area = screen.getByTestId("message-area");
    Object.defineProperty(area, "scrollHeight", {
      get: () => (area.querySelector('[data-seq="1"]') ? 8000 : 5000),
      configurable: true,
    });
    area.scrollTop = 1000;

    fireEvent.click(screen.getByTestId("load-more"));
    precedes(await screen.findByTestId("msg-1"), screen.getByTestId("msg-2"));
    await waitFor(() => expect(area.scrollTop).toBe(4000));
  });
});

// N04：DSH 文件池随正文刷新。面板已打开时，宿主在会话里新写一个文件（工具改写），
// 用户不必切追溯档或重开面板，下一个正文刷新周期就应看到该条并可预览真实内容。
describe("DSH 实时待答", () => {
  const session = makeSession({
    agentType: "dsh",
    id: "session-dsh-live",
    dshInstanceId: "instance-live",
    form: undefined,
    status: "processing",
  });
  const question = {
    eventId: "host-event-1",
    questions: [{ id: "q1", question: "保留哪份？", options: [{ label: "保留两份" }] }],
  };

  function mockQuestions(
    live: () => Response,
    answer = () => json({ receiptAccepted: true, receiptOnly: true })
  ) {
    installFetch();
    const fallback = fetch;
    const submissions: unknown[] = [];
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      if (url.pathname === "/m/api/v1/dsh/questions/live/answer") {
        submissions.push(JSON.parse(String(init?.body)));
        return answer();
      }
      if (url.pathname === "/m/api/v1/dsh/questions/live") return live();
      if (url.pathname === "/m/api/v1/dsh/questions") {
        return json({ verified: true, snapshot: { groups: [] } });
      }
      return fallback(input, init);
    });
    return submissions;
  }

  it("DSH 运行时以停止替换发送，回执受理后保留草稿直到宿主确认结束", async () => {
    mockQuestions(() => json({ live: true, pending: [] }));
    const fallback = fetch;
    let turn: string | null = "7:1:42";
    const stopRequests: unknown[] = [];
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      if (url.pathname === "/m/api/v1/session-send-info")
        return json({ injectable: true, channels: ["dsh_desktop"], visibility: "after_refresh" });
      if (url.pathname === "/m/api/v1/dsh/current-turn") {
        if (init?.method === "POST") {
          stopRequests.push(JSON.parse(String(init.body)));
          return json({ receipt: { state: "accepted" } });
        }
        return json({ turn });
      }
      return fallback(input, init);
    });
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    const stop = await screen.findByTestId("dsh-composer-stop");
    await waitFor(() => expect(stop).toBeEnabled());
    const input = screen.getByTestId("composer-input");
    fireEvent.change(input, { target: { value: "保留这个草稿" } });
    expect(screen.queryByTestId("composer-send")).not.toBeInTheDocument();
    fireEvent.click(stop);
    fireEvent.click(stop);
    await waitFor(() => expect(stopRequests).toHaveLength(1));
    expect(stopRequests[0]).toMatchObject({
      instanceId: session.dshInstanceId,
      sessionId: session.id,
      turn,
    });
    expect(stop).toBeDisabled();
    expect(input).toHaveValue("保留这个草稿");
    turn = null;
    fireEvent.click(screen.getByTestId("detail-refresh"));
    expect(await screen.findByTestId("composer-send")).toBeInTheDocument();
    expect(input).toHaveValue("保留这个草稿");
  });

  it("实时事件优先于只读回退，可整组提交且以回执文案确认送达", async () => {
    const submissions = mockQuestions(() => json({ live: true, pending: [question] }));
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    const card = await screen.findByTestId("dsh-question-group");
    expect(screen.getByTestId("message-area")).toContainElement(card);
    fireEvent.click(within(card).getByTestId("dsh-question-option-0"));
    fireEvent.click(within(card).getByTestId("dsh-question-submit"));
    await waitFor(() =>
      expect(submissions).toEqual([
        {
          instanceId: "instance-live",
          sessionId: "session-dsh-live",
          eventId: "host-event-1",
          answers: [{ id: "q1", selected: ["保留两份"] }],
        },
      ])
    );
    expect(await screen.findByText("已提交，等待宿主确认")).toBeInTheDocument();
  });

  it("正文收到答题结果后，移除等待宿主确认提示", async () => {
    let pending = [question];
    mockQuestions(() => json({ live: true, pending }));
    const fallback = fetch;
    const call: SessionMessage = {
      seq: 9,
      kind: "tool-call",
      role: "assistant",
      content: "选哪一份？",
      toolName: "ask_user_question",
      ts: null,
      collapsed: false,
    };
    let messages = [call];
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) =>
      String(input).includes("/session-messages")
        ? Promise.resolve(json({ messages, truncated: false }))
        : fallback(input, init)
    );
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    await screen.findByTestId("msg-9");
    const card = await screen.findByTestId("dsh-question-group");
    fireEvent.click(within(card).getByTestId("dsh-question-option-0"));
    fireEvent.click(within(card).getByTestId("dsh-question-submit"));
    expect(await screen.findByText("已提交，等待宿主确认")).toBeInTheDocument();
    pending = [];
    messages = [
      call,
      {
        seq: 10,
        kind: "tool-result",
        role: "assistant",
        content: "已保留两份",
        ts: null,
        collapsed: false,
      },
    ];
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await screen.findByTestId("msg-10");
    expect(screen.queryByText("已提交，等待宿主确认")).not.toBeInTheDocument();
  });

  it("空闲会话的补答回执不显示等待宿主确认", async () => {
    mockQuestions(() => json({ live: true, pending: [] }));
    const fallback = fetch;
    const request = vi.fn();
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input), "http://localhost").pathname;
      if (path === "/m/api/v1/dsh/questions")
        return Promise.resolve(
          json({
            verified: true,
            snapshot: {
              revisionToken: "r1",
              groups: [
                { callId: "continued-1", state: "continued", questions: question.questions },
              ],
            },
          })
        );
      if (path === "/m/api/v1/dsh/questions/answer") {
        request();
        return Promise.resolve(json({ hostAccepted: true }));
      }
      return fallback(input, init);
    });
    render(<SessionDetail session={{ ...session, status: "idle" }} onBack={vi.fn()} />);
    const card = await screen.findByTestId("dsh-question-group");
    fireEvent.click(within(card).getByTestId("dsh-question-option-0"));
    fireEvent.click(within(card).getByTestId("dsh-question-submit"));
    await waitFor(() => expect(request).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(within(card).getByTestId("dsh-question-submit")).toBeEnabled());
    expect(screen.queryByText("已提交，等待宿主确认")).not.toBeInTheDocument();
  });

  it("另一个设备提交中时保留题目，直到宿主快照确认它已结束", async () => {
    let pending = [question];
    mockQuestions(
      () => json({ live: true, generation: 1, pending }),
      () => json({ error: "DSH_ACTION_STATE_CONFLICT", reasonCode: "receipt in flight" }, 409)
    );
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    const card = await screen.findByTestId("dsh-question-group");
    fireEvent.click(within(card).getByTestId("dsh-question-option-0"));
    fireEvent.click(within(card).getByTestId("dsh-question-submit"));
    expect(await screen.findByText("已请求刷新题组状态，请稍后重试。")).toBeInTheDocument();
    expect(card).toBeInTheDocument();
    pending = [];
    fireEvent.click(screen.getByTestId("detail-refresh"));
    expect(await screen.findByText("该提问已在别处回答或取消。")).toBeInTheDocument();
    expect(card).not.toBeInTheDocument();
  });

  it("临时读取失败期间保留题面和草稿，恢复后可以继续提交", async () => {
    let ready = true;
    mockQuestions(() =>
      ready ? json({ live: true, generation: 1, pending: [question] }) : json({}, 503)
    );
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    const card = await screen.findByTestId("dsh-question-group");
    const submit = within(card).getByTestId("dsh-question-submit");
    fireEvent.click(within(card).getByTestId("dsh-question-option-0"));
    expect(submit).toBeEnabled();
    ready = false;
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(() => expect(submit).toBeDisabled());
    expect(card).toBeInTheDocument();
    ready = true;
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(() => expect(submit).toBeEnabled());
  });

  it("把携带操作意图的待答交给官方控制台", async () => {
    const submissions = mockQuestions(() =>
      json({
        live: true,
        pending: [
          {
            ...question,
            questions: [{ ...question.questions[0], intent: { kind: "plan-review" } }],
          },
        ],
      })
    );
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    expect(await screen.findByTestId("dsh-question-console-only")).toBeInTheDocument();
    expect(screen.queryByTestId("dsh-question-submit")).not.toBeInTheDocument();
    expect(submissions).toEqual([]);
  });
});

describe("N04 DSH 文件池随正文刷新", () => {
  it("面板打开时新文件在下一个刷新周期出现并可预览，无需切档或重开", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      mountDsh();
      await screen.findByTestId("msg-2");
      // 明确打开文件池（N04 场景：面板**已打开**时宿主写入新文件）。
      fireEvent.click(await screen.findByTestId("file-panel-button"));
      const panel = await screen.findByTestId("file-panel");
      expect(within(panel).getAllByTestId(/^file-row-\d+$/)).toHaveLength(1);
      const before = fileListCalls.length;

      // 宿主在会话里写入新文件：文件端点随后会返回这条「工具改写」。
      const written = "/tmp/proj/note.md";
      files = [
        fileEntry(ordinaryPath),
        { ...fileEntry(written), changeKind: "tool-write" } as SessionFileEntry,
      ];

      // 不切档、不重开：推进既有 10s 正文刷新周期（复用同一轮询，不新增定时器）。
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10_500);
      });

      // 文件端点被重拉，且新条目出现在同一个已打开的面板里。
      await waitFor(() => expect(fileListCalls.length).toBeGreaterThan(before));
      await waitFor(() =>
        expect(
          within(screen.getByTestId("file-panel")).getAllByTestId(/^file-row-\d+$/)
        ).toHaveLength(2)
      );
      const rows = within(screen.getByTestId("file-panel")).getAllByTestId(/^file-row-\d+$/);
      expect(rows.some((r) => (r.textContent ?? "").includes("note.md"))).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it("面板关闭或页面隐藏时，刷新周期不产生文件列表请求", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      mountDsh();
      await screen.findByTestId("msg-2");
      // 打开再关闭面板：回到「无面板」态。
      fireEvent.click(await screen.findByTestId("file-panel-button"));
      await screen.findByTestId("file-panel");
      fireEvent.click(screen.getByTestId("file-panel-button"));
      await waitFor(() => expect(screen.queryByTestId("file-panel")).toBeNull());

      const closedBefore = fileListCalls.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(31_000);
      });
      expect(fileListCalls.length, "面板关闭后不得随正文刷新拉文件列表").toBe(closedBefore);

      // 隐藏页面：正文轮询停拍，文件列表同样零增量。
      fireEvent.click(screen.getByTestId("file-panel-button"));
      await screen.findByTestId("file-panel");
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      const hiddenBefore = fileListCalls.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(31_000);
      });
      expect(fileListCalls.length, "页面隐藏时不得拉文件列表").toBe(hiddenBefore);
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("N06 只读模型摘要", () => {
  const model = "deepseek-v4.1-flash";

  it("last_used_summary_is_labelled_as_previous_use", async () => {
    mountDsh({ modelSummary: { model, reasoningEffort: "low", source: "last_used" } });
    await screen.findByText(`上次使用 · ${model}`);
    const summary = screen.getByTestId("dsh-model");
    expect(summary.textContent).toContain("上次使用");
    expect(summary.textContent).toContain(model);
    expect(screen.getByTestId("dsh-effort")).toHaveTextContent("上次思考强度 · low");
    expect(summary.textContent).not.toContain("当前");
  });

  it("missing_effort_is_omitted", async () => {
    mountDsh({ modelSummary: { model, source: "last_used" } });
    const summary = await screen.findByText(`上次使用 · ${model}`);
    expect(summary.textContent).toBe(`上次使用 · ${model}`);
  });

  it("absent_summary_renders_nothing", async () => {
    mountDsh();
    await screen.findByTestId("msg-3");
    expect(screen.queryByTestId("dsh-model")?.textContent).not.toContain("上次使用");
  });

  it("summary_does_not_leak_across_instances", async () => {
    mountDsh({ modelSummary: { model, source: "last_used" } });
    await screen.findByText(`上次使用 · ${model}`);
    cleanup();

    mountDsh({ dshInstanceId: "dsh-instance-n06-other" });
    await screen.findByTestId("msg-3");
    expect(screen.queryByTestId("dsh-model")?.textContent).not.toContain("上次使用");
  });
});

describe("DSH 官方会话设置", () => {
  const session = makeSession({
    id: "options",
    agentType: "dsh",
    dshInstanceId: "host-options",
    status: "idle",
  });
  const path = "/m/api/v1/dsh/session-settings";
  const next = { provider: "p", model: "new-model", reasoningEffort: "high" };
  const old = { provider: "p", model: "old-model", reasoningEffort: "low" };
  function mock() {
    installFetch();
    const base = fetch;
    const requests: Record<string, unknown>[] = [];
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      if (url.pathname !== path) return base(input, init);
      if (init?.method === "POST") {
        requests.push(JSON.parse(String(init.body)));
        return Promise.resolve(json({ state: "accepted", confirmed: true }));
      }
      return Promise.resolve(
        json({
          model: {
            next,
            lastUsed: old,
            default: null,
            groups: [
              {
                id: "p",
                name: "Provider",
                models: [
                  {
                    id: "new-model",
                    name: "New model",
                    reasoning: {
                      efforts: [
                        { id: "low", name: "Low" },
                        { id: "high", name: "High" },
                      ],
                    },
                  },
                ],
              },
            ],
          },
          permission: {
            currentValue: "workspace-write",
            options: [
              { value: "workspace-write", name: "Workspace" },
              { value: "danger-full-access", name: "Full" },
            ],
          },
        })
      );
    });
    return requests;
  }

  it("shows the verified next settings and sends full access only after acknowledgement", async () => {
    const requests = mock();
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    const permission = await screen.findByText("权限 · 工作区内修改");
    expect(screen.getByTestId("dsh-model")).toHaveTextContent("模型 · New model");
    expect(screen.getByTestId("dsh-effort")).toHaveTextContent("思考强度 · high");
    fireEvent.click(permission);
    fireEvent.change(screen.getByTestId("dsh-setting-choice"), {
      target: { value: "danger-full-access" },
    });
    const apply = screen.getByTestId("dsh-setting-apply");
    expect(apply).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: /我确认更改权限/ }));
    fireEvent.click(apply);
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      instanceId: session.dshInstanceId,
      sessionId: session.id,
      change: { type: "permission", preset: "danger-full-access", acknowledged: true },
    });
  });

  it("limits effort to the current model and discloses the default side effect", async () => {
    const requests = mock();
    render(<SessionDetail session={session} onBack={vi.fn()} />);
    fireEvent.click(await screen.findByText("思考强度 · high"));
    const choice = screen.getByTestId("dsh-setting-choice");
    expect(within(choice).getAllByRole("option")).toHaveLength(3);
    fireEvent.change(choice, { target: { value: "low" } });
    expect(screen.getByText(/保存为该配置的默认模型/)).toBeVisible();
    fireEvent.click(screen.getByTestId("dsh-setting-apply"));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      change: { type: "model", provider: "p", model: "new-model", reasoningEffort: "low" },
    });
  });
});
