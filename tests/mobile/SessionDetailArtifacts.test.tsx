// 任务 6：SessionDetail × IPC 正文链接与附件卡片的接线锁。
// 覆盖：IPC 正文沿用既有链接渲染器点击开预览；文档 chip / 图片卡走同一 openFile；
// 预览已打开时再次显式打开同一路径必须重读并显示新内容；每回合 12 图上限且超额不读。
// 任务 7：文件池来源合并/排序、来源按钮与文件名预览两个独立热区、五页有界定位
// （当前页零请求 / 五页未命中四次更早读取）、窄屏先回消息区、滚动高亮两秒、失败可返回预览。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SessionDetail from "@/mobile/SessionDetail";
import type { ProjectedArtifact, ProjectedMessage, ThreadProjection } from "@/mobile/codexIpcApi";
import type { SessionFileEntry } from "@/mobile/api";
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

function installFetch() {
  files = [];
  fileCalls = [];
  fileVersion = 0;
  pageCalls = [];
  pageHandler = () => json({ status: "end", messages: [], historyToken: "", nextBefore: null });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/codex/thread/events")) return new Response("", { status: 200 });
      if (url.includes("/codex/queue")) return json({ actions: [], blockedReasonCode: null });
      if (url.includes("/codex/thread/messages")) {
        const index = pageCalls.length;
        pageCalls.push(url);
        return pageHandler(index);
      }
      if (url.includes("/codex/thread")) return json(projection([]));
      if (url.includes("/session-messages")) return json({ messages: [], truncated: false });
      if (url.includes("/session-files")) return json({ files, truncated: false });
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
