// H11 中不依赖 H09 的切片：真实 App → SessionDetail → chip → SubagentDetail。
// 只替换浏览器 HTTP/SSE 边界，不替换组件、mobile/api 或生产 source。
// 响应均为合成 wire 载荷：展示用例通过不证明 H08 的 Rust source/路由已接通。
// 实例用例对请求参数独立断言；严格端点拒绝缺失/错误实例，不能靠兜底载荷变绿。
import { dshSession, json as responseJson } from "./dshSessionFixture";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import App from "@/mobile/App";
import type { Session, SessionsResponse } from "@/types/session";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const json = (body: unknown, status = 200) =>
  responseJson(body, status, { "Content-Type": "application/json" });

const parent = dshSession({
  id: "h11-shared-parent",
  dshInstanceId: "h11-instance-A",
  projectName: "H11 project A",
  projectPath: "/tmp/h11-project",
  title: "H11 parent A",
  status: "processing",
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "2026-11-06T00:00:00Z",
  activeSubagentCount: 1,
  form: "app",
});

const child = {
  id: "h11-shared-child",
  name: "DSH Worker",
  description: "读取项目结构",
  status: "running",
  spawnTs: null,
  endTs: null,
  tokens: { input: 12, cacheRead: 0, cacheCreation: 0, output: 3 },
};

const childMessage = {
  seq: 1,
  role: "assistant",
  kind: "assistant",
  content: "H11 child transcript",
  ts: null,
  toolName: null,
  toolArgs: null,
  collapsed: false,
};

const LIST = "/m/api/v1/session-subagents";
const ENTRY = "/m/api/v1/dsh-console-entry";
const REDEEM = "/m/api/v1/dsh-console-redeem";
const ticket = "synthetic-one-time-ticket";
const consoleUrl = "/console/deadbeefdeadbeefdeadbeefdeadbeef/";
const TRANSCRIPT = "/m/api/v1/session-subagent-messages";
const FILES = "/m/api/v1/session-files";
const draft = "先保留草稿，读完子任务再发送\n第二行也要保留";
let selectedSession: Session;
let strictPaths: Set<string>;
let requests: Array<{ url: URL; method: string }>;
let unexpectedRequests: string[];
let unpinZh: () => void;

function snapshot(sessions: Session[]): SessionsResponse {
  return { sessions, totalCount: sessions.length, waitingCount: 0 };
}

function callsTo(path: string) {
  return requests.filter(({ url }) => url.pathname === path);
}

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => unpinZh());

beforeEach(() => {
  selectedSession = parent;
  strictPaths = new Set();
  requests = [];
  unexpectedRequests = [];
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal("innerWidth", 390);
  vi.stubGlobal("innerHeight", 844);
  vi.stubGlobal("matchMedia", undefined);
  vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), "http://localhost");
    const method = init?.method ?? "GET";
    requests.push({ url, method });
    if (url.pathname === REDEEM && method === "POST") {
      return json({ url: consoleUrl });
    }
    if (method !== "GET") {
      unexpectedRequests.push(`${method} ${url.pathname}`);
      return json({ error: "unexpected_write" }, 405);
    }
    if (
      strictPaths.has(url.pathname) &&
      url.searchParams.get("instanceId") !== selectedSession.dshInstanceId
    ) {
      return json({ error: "instance_required" }, 409);
    }
    switch (url.pathname) {
      case ENTRY:
        return json({ ticket });
      case "/m/api/v1/host":
        return json({
          host: { name: "H11 host", platform: "darwin", version: "0", bootId: "h11-boot" },
          enabledTools: ["dsh"],
          installedTools: ["dsh"],
        });
      case "/m/api/v1/sessions":
        return json(snapshot([selectedSession]));
      case "/m/api/v1/session-messages":
        return json({
          messages: [{ ...childMessage, seq: 0, content: "H11 parent transcript" }],
          truncated: false,
        });
      case FILES:
        return json({ files: [], truncated: false });
      case LIST:
        return json({ subagents: [child] });
      case TRANSCRIPT:
        return json({ supported: true, messages: [childMessage], truncated: false });
      case "/m/api/v1/session-send-info":
        return json({ injectable: true, channels: ["dsh_desktop"], visibility: "after_refresh" });
      case "/m/api/v1/session-queue":
        return json({ items: [] });
      case "/m/api/v1/session-mode":
        return json({ switchKind: "unsupported" });
      case "/m/api/v1/session-approve-options":
        return json({
          available: false,
          options: [],
          verifiedWith: "test",
          currentVersion: null,
          drift: false,
        });
      case "/m/api/v1/session-question":
        return json({ available: false, questions: [] });
      case "/m/api/v1/dsh/questions":
        return json({ verified: false, groups: [] });
      case "/m/api/v1/dsh/session-settings":
        return json({
          model: { next: null, lastUsed: null, default: null, groups: [] },
          permission: null,
        });
      case "/m/api/v1/dsh/current-turn":
        return json({ turn: null });
      case "/m/api/v1/dsh/questions/live":
        return json({ live: true, generation: 1, pending: [] });
      case "/m/api/v1/channel":
        return json({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 });
      default:
        unexpectedRequests.push(`${method} ${url.pathname}`);
        return json({ error: "unexpected_request" }, 404);
    }
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  // 生产代码会吞掉部分增强请求的错误，额外核对以免错误端点/写操作静默假绿。
  expect(unexpectedRequests).toEqual([]);
});

async function openParent(session = parent) {
  selectedSession = session;
  render(<App />);
  act(() => MockEventSource.latest().emit("snapshot", snapshot([session])));
  const card = (await screen.findByText(session.projectName)).closest("li");
  expect(card).not.toBeNull();
  fireEvent.click(card!);
  await screen.findByText("H11 parent transcript");
  await screen.findByTestId("composer-input");
}

async function openChild() {
  fireEvent.click(await screen.findByTestId("subagent-chip-h11-shared-child"));
  const detail = await screen.findByTestId("subagent-detail");
  await within(detail).findByText("H11 child transcript");
  return detail;
}

async function waitForRequest(path: string, after = 0) {
  await waitFor(() => expect(callsTo(path).length).toBeGreaterThan(after));
  return callsTo(path)[after].url.searchParams;
}

describe("H11：DSH chip 复用手机详情流（不依赖 H09）", () => {
  it("dsh_child_chip_uses_existing_detail_flow：直达只读详情，返回现有子任务清单", async () => {
    await openParent();
    const originalLocation = window.location.href;
    const detail = await openChild();

    expect(screen.getByTestId("preview-shell")).toHaveAttribute("data-sheet", "subagents");
    expect(screen.getByTestId("preview-shell")).toHaveAttribute("data-open", "subagent");
    expect(within(detail).getByText("DSH Worker")).toBeInTheDocument();
    expect(within(detail).queryByRole("textbox")).toBeNull();
    expect(within(detail).queryByTestId("message-composer")).toBeNull();
    const query = await waitForRequest(TRANSCRIPT);
    expect(Object.fromEntries(query)).toMatchObject({
      agent_type: "dsh",
      session_id: "h11-shared-parent",
      subagent_id: "h11-shared-child",
      limit: "200",
    });

    fireEvent.click(within(detail).getByTestId("subagent-back"));
    expect(await screen.findByTestId("subagent-card-h11-shared-child")).toHaveTextContent(
      "DSH Worker"
    );
    expect(screen.queryByTestId("subagent-detail")).toBeNull();
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(screen.queryByTestId("preview-shell")).toBeNull();
    expect(screen.getByText("H11 parent transcript")).toBeInTheDocument();
    expect(window.location.href).toBe(originalLocation);
  });

  it("return_keeps_draft：关闭子任务详情后保留父会话未发送的多行草稿", async () => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    await openChild();
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(await screen.findByTestId("composer-input")).toHaveValue(draft);
    expect(screen.getByTestId("composer-send")).toBeEnabled();
  });

  it.each([360, 390])(
    "return_keeps_scroll：%ipx 竖屏关闭子详情仍停在父消息原阅读位置",
    async (width) => {
      vi.stubGlobal("innerWidth", width);
      await openParent();
      const area = screen.getByTestId("message-area");
      // jsdom 不排版：只提供非零几何，scrollTop 的保存/恢复仍由真实组件负责。
      Object.defineProperties(area, {
        scrollHeight: { configurable: true, value: 2400 },
        clientHeight: { configurable: true, value: 600 },
      });
      area.scrollTop = 420;
      fireEvent.scroll(area);
      await openChild();
      fireEvent.click(screen.getByTestId("preview-close"));
      expect(screen.getByTestId("message-area").scrollTop).toBe(420);
    }
  );

  it.each(["h11-instance-A", "h11-instance-B"])(
    "子任务清单请求携带已选实例 %s，同名父会话不依赖全局默认实例",
    async (instanceId) => {
      strictPaths.add(LIST);
      await openParent({ ...parent, dshInstanceId: instanceId });
      const query = await waitForRequest(LIST);
      expect(Object.fromEntries(query)).toMatchObject({
        agent_type: "dsh",
        session_id: "h11-shared-parent",
        instanceId,
      });
      expect(await screen.findByTestId("subagent-chip-h11-shared-child")).toHaveTextContent(
        "DSH Worker"
      );
    }
  );

  it("子任务转写请求保留父会话的实例身份", async () => {
    // 清单用合成响应让详情可达；转写端点严格拒绝缺实例，不声称生产清单已接通。
    strictPaths.add(TRANSCRIPT);
    await openParent();
    fireEvent.click(await screen.findByTestId("subagent-chip-h11-shared-child"));
    const query = await waitForRequest(TRANSCRIPT);
    expect(Object.fromEntries(query)).toMatchObject({
      agent_type: "dsh",
      session_id: "h11-shared-parent",
      subagent_id: "h11-shared-child",
      instanceId: "h11-instance-A",
    });
    expect(await screen.findByText("H11 child transcript")).toBeInTheDocument();
  });

  it("子详情为正文路径读取文件表时保留父会话的实例身份", async () => {
    strictPaths.add(FILES);
    await openParent();
    const parentReads = callsTo(FILES).length;
    expect(parentReads).toBeGreaterThan(0);
    await openChild();
    const query = await waitForRequest(FILES, parentReads);
    expect(Object.fromEntries(query)).toMatchObject({
      agent_type: "dsh",
      session_id: "h11-shared-parent",
      instanceId: "h11-instance-A",
    });
  });

  it("看板收到另一实例的同名父会话后，当前子详情与父草稿仍属于原实例", async () => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    const detail = await openChild();
    const transcriptReads = callsTo(TRANSCRIPT).length;
    act(() => {
      MockEventSource.latest().emit(
        "snapshot",
        snapshot([
          {
            ...parent,
            dshInstanceId: "h11-instance-B",
            projectName: "H11 project B",
            title: "H11 parent B",
            status: "idle",
          },
        ])
      );
    });
    await screen.findByText("H11 project B");
    expect(screen.getByTestId("subagent-detail")).toBe(detail);
    expect(callsTo(TRANSCRIPT)).toHaveLength(transcriptReads);
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(screen.getByTestId("detail-back").parentElement).toHaveTextContent("H11 parent A");
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(() => expect(callsTo("/m/api/v1/session-messages").length).toBeGreaterThan(1));
    const latest = callsTo("/m/api/v1/session-messages").at(-1)!;
    expect(latest.url.searchParams.get("instanceId")).toBe("h11-instance-A");
  });
});

describe("H11：打开 DSH 控制台（新浏览上下文，不打断原页）", () => {
  it("opens_in_a_new_context_and_keeps_url_scroll_and_draft", async () => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    const area = screen.getByTestId("message-area");
    Object.defineProperties(area, {
      scrollHeight: { configurable: true, value: 2400 },
      clientHeight: { configurable: true, value: 600 },
    });
    area.scrollTop = 360;
    fireEvent.scroll(area);

    const originalHref = window.location.href;
    const opened: string[] = [];
    const events: string[] = [];
    const consoleRequests: Array<{ url: URL; init?: RequestInit }> = [];
    let finishRedeem!: (response: Response) => void;
    const redeemResponse = new Promise<Response>((resolve) => {
      finishRedeem = resolve;
    });
    const originalFetch = globalThis.fetch;
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), window.location.href);
      if (url.pathname === ENTRY || url.pathname === REDEEM) {
        consoleRequests.push({ url, init });
        events.push(url.pathname);
        if (url.pathname === REDEEM) return redeemResponse;
      }
      return originalFetch(input, init);
    });
    // 真实语义：先同步开 about:blank（用户手势内），拿到句柄后再导航。
    const handle = {
      opener: {} as unknown,
      location: {
        replace: (url: string) => {
          events.push("navigate");
          opened.push(url);
        },
      },
      close: vi.fn(),
    };
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => {
      // 返回非 null 句柄 = 浏览器接受了新上下文（注意：**不能**用 noopener，
      // 那个模式下 window.open 恒返回 null）。
      return handle as unknown as Window;
    });

    fireEvent.click(await screen.findByTestId("open-dsh-console"));
    expect(openSpy).toHaveBeenCalledWith("about:blank", "_blank");
    expect(handle.opener).toBeNull();
    await waitFor(() => expect(consoleRequests).toHaveLength(2));
    expect(events).toEqual([ENTRY, REDEEM]);
    const [entryRequest, redeemRequest] = consoleRequests;
    expect(entryRequest.init?.method ?? "GET").toBe("GET");
    expect(redeemRequest.init?.method).toBe("POST");
    expect(JSON.parse(String(redeemRequest.init?.body))).toEqual({ ticket });
    expect(new Headers(redeemRequest.init?.headers).get("content-type")).toMatch(
      /application\/json/i
    );
    for (const request of consoleRequests) {
      expect(request.url.origin).toBe(window.location.origin);
      expect([undefined, "same-origin", "include"]).toContain(request.init?.credentials);
    }
    expect(opened).toEqual([]);
    finishRedeem(json({ url: consoleUrl }));
    await waitFor(() => expect(opened).toEqual([consoleUrl]));
    expect(events).toEqual([ENTRY, REDEEM, "navigate"]);
    // 新上下文打开，不导航当前页；句柄必须被断链（opener = null）。
    expect(openSpy).toHaveBeenCalledWith("about:blank", "_blank");
    expect(handle.opener).toBeNull();
    expect(window.location.href).toBe(originalHref);
    // 原页草稿与滚动位置保持不变。
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
    expect(screen.getByTestId("message-area").scrollTop).toBe(360);
    // 入口请求必须带实例，避免跨实例错取控制台。
    const query = await waitForRequest(ENTRY);
    expect(Object.fromEntries(query)).toMatchObject({
      session_id: "h11-shared-parent",
      instanceId: "h11-instance-A",
    });

    openSpy.mockRestore();
  });

  it.each([
    {
      label: "签发缺少票据",
      entry: {},
      redeem: { url: consoleUrl },
      status: 200,
      expectRedeem: false,
    },
    {
      label: "签发票据为空",
      entry: { ticket: "" },
      redeem: { url: consoleUrl },
      status: 200,
      expectRedeem: false,
    },
    {
      label: "签发票据不是字符串",
      entry: { ticket: 123 },
      redeem: { url: consoleUrl },
      status: 200,
      expectRedeem: false,
    },
    {
      label: "兑换路径不是字符串",
      entry: { ticket },
      redeem: { url: 123 },
      status: 200,
      expectRedeem: true,
    },
    {
      label: "兑换失败",
      entry: { ticket },
      redeem: { error: "denied" },
      status: 403,
      expectRedeem: true,
    },
    { label: "兑换缺少路径", entry: { ticket }, redeem: {}, status: 200, expectRedeem: true },
    {
      label: "外部绝对路径",
      entry: { ticket },
      redeem: { url: "https://evil.example/console/token/" },
      status: 200,
      expectRedeem: true,
    },
    {
      label: "协议相对路径",
      entry: { ticket },
      redeem: { url: "//evil.example/console/token/" },
      status: 200,
      expectRedeem: true,
    },
    {
      label: "非控制台路径",
      entry: { ticket },
      redeem: { url: "/m/api/v1/sessions" },
      status: 200,
      expectRedeem: true,
    },
    {
      label: "路径穿越",
      entry: { ticket },
      redeem: { url: "/console/token/../../m/" },
      status: 200,
      expectRedeem: true,
    },
  ])("$label 时关闭空白页并保留原页草稿", async ({ entry, redeem, status, expectRedeem }) => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    const originalHref = window.location.href;
    const originalFetch = globalThis.fetch;
    const paths: string[] = [];
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), window.location.href);
      if (url.pathname === ENTRY) {
        paths.push(ENTRY);
        return json(entry);
      }
      if (url.pathname === REDEEM) {
        paths.push(REDEEM);
        return json(redeem, status);
      }
      return originalFetch(input, init);
    });
    const handle = { opener: {} as unknown, location: { replace: vi.fn() }, close: vi.fn() };
    vi.spyOn(window, "open").mockReturnValue(handle as unknown as Window);

    fireEvent.click(screen.getByTestId("open-dsh-console"));
    await waitFor(() => expect(handle.close).toHaveBeenCalledOnce());
    expect(handle.location.replace).not.toHaveBeenCalled();
    expect(paths).toEqual(expectRedeem ? [ENTRY, REDEEM] : [ENTRY]);
    expect(screen.getByTestId("dsh-console-error")).toBeInTheDocument();
    expect(window.location.href).toBe(originalHref);
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
  });

  it.each([
    { label: "签发 GET 网络拒绝", failure: "entry-network", expectedPaths: [ENTRY] },
    { label: "兑换 POST 网络拒绝", failure: "redeem-network", expectedPaths: [ENTRY, REDEEM] },
    { label: "兑换响应 JSON 解析失败", failure: "redeem-json", expectedPaths: [ENTRY, REDEEM] },
  ])("$label 时关闭空白页、不导航并保留草稿", async ({ failure, expectedPaths }) => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    const originalHref = window.location.href;
    const originalFetch = globalThis.fetch;
    const calls: Array<{ path: string; method: string }> = [];
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), window.location.href);
      if (url.pathname === ENTRY || url.pathname === REDEEM) {
        calls.push({ path: url.pathname, method: init?.method ?? "GET" });
        if (url.pathname === ENTRY && failure === "entry-network") {
          throw new TypeError("network rejected");
        }
        if (url.pathname === REDEEM && failure === "redeem-network") {
          throw new TypeError("network rejected");
        }
        if (url.pathname === REDEEM && failure === "redeem-json") {
          return new Response("{invalid json", { status: 200 });
        }
      }
      return originalFetch(input, init);
    });
    const handle = { opener: {} as unknown, location: { replace: vi.fn() }, close: vi.fn() };
    const openSpy = vi.spyOn(window, "open").mockReturnValue(handle as unknown as Window);

    fireEvent.click(screen.getByTestId("open-dsh-console"));
    expect(openSpy).toHaveBeenCalledWith("about:blank", "_blank");
    await waitFor(() => expect(handle.close).toHaveBeenCalledOnce());
    expect(calls).toEqual(
      expectedPaths.map((path) => ({ path, method: path === ENTRY ? "GET" : "POST" }))
    );
    expect(handle.location.replace).not.toHaveBeenCalled();
    expect(await screen.findByTestId("dsh-console-error")).toBeInTheDocument();
    expect(window.location.href).toBe(originalHref);
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
  });

  it("reports_a_blocked_popup_instead_of_pretending_it_opened", async () => {
    await openParent();
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => null);
    fireEvent.click(await screen.findByTestId("open-dsh-console"));
    expect(await screen.findByTestId("dsh-console-error")).toHaveTextContent("浏览器拦截了新窗口");
    openSpy.mockRestore();
  });

  // T3（spec §5.1）：弹窗被拦截时给出可点击的「在新页面打开」兜底链接——
  // target=_blank + noopener，点击后新上下文加载控制台，原页不导航不丢草稿。
  it("offers_a_noopener_fallback_link_when_the_popup_is_blocked", async () => {
    await openParent();
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: draft } });
    const originalHref = window.location.href;
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => null);

    fireEvent.click(await screen.findByTestId("open-dsh-console"));
    const fallback = await screen.findByTestId("dsh-console-fallback");
    expect(fallback).toHaveAttribute("href", consoleUrl);
    expect(fallback).toHaveAttribute("target", "_blank");
    expect(fallback.getAttribute("rel")).toContain("noopener");
    expect(fallback).toHaveTextContent("在新页面打开");
    // 兜底链接不得导航原页，草稿保留。
    expect(window.location.href).toBe(originalHref);
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
    openSpy.mockRestore();
  });

  it.each([
    {
      label: "网页凭据未就绪",
      status: 503,
      error: "console_credential_invalid",
      expected: "网页凭据未就绪",
    },
    {
      label: "设备授权过期（gate 层拒绝）",
      status: 403,
      error: "device_unpaired",
      expected: "设备授权已过期",
    },
    {
      label: "设备授权过期（挂载设备校验拒绝，复审 Important-1 钉死按码可达）",
      status: 503,
      error: "console_device_invalid",
      expected: "设备授权已过期",
    },
    {
      label: "换代后入口失效",
      status: 503,
      error: "console_target_invalid",
      expected: "已重启",
    },
    {
      label: "实例不可用（宿主未运行）",
      status: 503,
      error: "DSH_HOST_NOT_RUNNING",
      expected: "控制台入口暂不可用",
    },
    {
      label: "实例不可用（归属未核验）",
      status: 503,
      error: "DSH_INSTANCE_UNVERIFIED",
      expected: "实例运行归属未核验",
    },
  ])("入口失败按安全错误码映射可操作文案：$label", async ({ status, error, expected }) => {
    await openParent();
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      if (url.pathname === ENTRY) return json({ error }, status);
      return original(input, init);
    });
    const closeSpy = vi.fn();
    const handle = {
      opener: {} as unknown,
      location: { replace: vi.fn() },
      close: closeSpy,
    };
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => handle as unknown as Window);

    fireEvent.click(await screen.findByTestId("open-dsh-console"));
    const message = await screen.findByTestId("dsh-console-error");
    expect(message).toHaveTextContent(expected);
    // 拒绝时不得留下空白标签，也不得导航原页。
    await waitFor(() => expect(closeSpy).toHaveBeenCalled());
    expect(handle.location.replace).not.toHaveBeenCalled();

    openSpy.mockRestore();
  });

  it("reports_an_unavailable_entry_instead_of_failing_silently", async () => {
    await openParent();
    // 让入口返回失败（实例未运行/未授权时生产即如此）。
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      if (url.pathname === ENTRY) return json({ error: "DSH_HOST_NOT_RUNNING" }, 503);
      return original(input, init);
    });
    const closeSpy = vi.fn();
    const handle = {
      opener: {} as unknown,
      location: { replace: vi.fn() },
      close: closeSpy,
    };
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => handle as unknown as Window);

    fireEvent.click(await screen.findByTestId("open-dsh-console"));
    const error = await screen.findByTestId("dsh-console-error");
    expect(error).toHaveTextContent("控制台入口暂不可用");
    // 入口失败时不得留下空白标签，也不得导航到任何地址。
    await waitFor(() => expect(closeSpy).toHaveBeenCalled());
    expect(handle.location.replace).not.toHaveBeenCalled();

    openSpy.mockRestore();
  });
});
