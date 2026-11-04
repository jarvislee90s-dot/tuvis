// tests/mobile/zcodeControlAvailability.test.tsx — A5（设计 §4.3/§5.1）：
// ZCode 手机控制面的可用性只由**活的统一控制路径**承担：SessionDetail 对 zcode 会话
// 路由到 ZcodeControlPanel（连接态只来自官方 GET），旧 composer 的 ZCode 只读分支
// 已删除（质量评审 I7 死码）。本文件测的就是这条活路径，不再渲染死分支。
// 三条底线：
//  1. 未配置／格式错误／待连接／读取失败——**正文与文件预览继续走既有读取链**，
//     不整页报错、不把「未联机」当「无消息」、不把读取失败当「链接已删除」；
//  2. 控制区保留草稿、给官方原因码，不调用旧发送／审批 API（零 session-send /
//     零无头通道），写面只允许官方 `/m/api/v1/zcode/actions`；
//  3. 「打开官方页面」是用户主动选择的辅助动作：新标签页打开已存链接，
//     不标已读、不改本页、不把外跳冒充统一控制。
// 夹具只用合成 sid（完整链接不入真实日志/夹具）。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import SessionDetail from "@/mobile/SessionDetail";
import MessageComposer from "@/mobile/MessageComposer";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import type { SessionFileEntry, SessionMessage, SendInfo } from "@/mobile/api";
import type { Session } from "@/types/session";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => {
  unpinZh();
});

const SAVED_URL = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a5-mobile-0001";

function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: "sess-zc",
    agentType: "zcode",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "合成会话",
    gitBranch: null,
    githubUrl: null,
    // idle = 总结态：审批/问答卡按既有门自隐，本文件只锁远控可用性这一件事
    status: "idle",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-10-24T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "app",
    jumpSupported: false,
    unread: true,
    ...overrides,
  };
}

function msg(
  overrides: Partial<SessionMessage> & Pick<SessionMessage, "seq" | "kind" | "content">
): SessionMessage {
  return {
    role: overrides.kind === "user" ? "user" : "assistant",
    ts: 1000,
    collapsed: overrides.kind === "thinking" || overrides.kind === "tool-call",
    ...overrides,
  };
}

function fileEntry(path: string): SessionFileEntry {
  return { path, lastSeq: 1, lastTs: 1000, hits: 1, modified: true };
}

/** 官方远控配置的三种可读态 + 读取失败（网络） */
type ControlRoute =
  { kind: "missing" } | { kind: "invalid" } | { kind: "saved" } | { kind: "networkFail" };

let controlRoute: ControlRoute = { kind: "missing" };
let fetchMock: ReturnType<typeof vi.fn>;

/** 未登记 URL 直接抛（与 tests/mobile/SessionDetail.test.tsx 同口径：防「默默少调
 *  一个接口」把断言变成假阴性） */
function stubFetch(route: (url: string, init?: RequestInit) => Response) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    return route(url, init);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** 详情页读取链 + 远控配置的分路（正文/文件/send-info 恒正常——本文件只动远控读） */
function installDetailFetch() {
  stubFetch((url, init) => {
    if (url.includes("/official-control")) {
      if (init?.method && init.method !== "GET") {
        throw new Error(`手机侧不得写远控配置：${init.method} ${url}`);
      }
      if (controlRoute.kind === "networkFail") throw new TypeError("network down");
      const body =
        controlRoute.kind === "missing"
          ? { configState: "missing", url: null }
          : controlRoute.kind === "invalid"
            ? { configState: "invalid", url: null }
            : { configState: "saved", url: SAVED_URL };
      return new Response(JSON.stringify(body), { status: 200 });
    }
    if (url === "/m/api/v1/zcode/connection") {
      // T7.2 统一控制：连接态只来自官方 GET；网络失败 → 面板按 offline 就地申报
      if (controlRoute.kind === "networkFail") throw new TypeError("network down");
      const state = controlRoute.kind === "saved" ? "needs_auth" : "needs_config";
      return new Response(
        JSON.stringify({ state, generation: 1, reasonCode: state, deviceLabel: null }),
        { status: 200 }
      );
    }
    if (url.startsWith("/m/api/v1/zcode/session?")) {
      const flag = { enabled: true, reasonCode: null };
      return new Response(
        JSON.stringify({
          sessionId: "sess-zc",
          generation: 1,
          revision: 1,
          ready: true,
          pending: [],
          control: { activeWorks: [] },
          selection: null,
          capabilities: {
            send: flag,
            singleSelect: flag,
            multiSelect: flag,
            freeText: flag,
            interrupt: flag,
            approvalReply: flag,
            model: flag,
            thoughtLevel: flag,
          },
        }),
        { status: 200 }
      );
    }
    if (url.includes("/session-messages")) {
      return new Response(
        JSON.stringify({
          messages: [
            msg({ seq: 0, kind: "user", content: "合成正文甲" }),
            msg({ seq: 1, kind: "assistant", content: "合成正文乙" }),
          ],
          truncated: false,
        }),
        { status: 200 }
      );
    }
    if (url.includes("/session-files")) {
      return new Response(
        JSON.stringify({ files: [fileEntry("/tmp/proj/synthetic.rs")], truncated: false }),
        { status: 200 }
      );
    }
    if (url.includes("/host")) {
      return new Response(
        JSON.stringify({
          host: { name: "n", platform: "windows", version: "0", bootId: "boot-test" },
          enabledTools: [],
          installedTools: [],
        }),
        { status: 200 }
      );
    }
    if (url.includes("/session-send-info")) {
      // 后端既有裁决：APP 形态不可注入（reasonCode=app_form）；本用例锁的是手机侧
      // ZCode 原因**覆盖**这句通用文案
      const info: SendInfo = {
        injectable: false,
        reasonCode: "app_form",
        reason: "APP 形态无外部写 API",
      };
      return new Response(JSON.stringify(info), { status: 200 });
    }
    if (url.includes("/session-queue")) {
      return new Response(JSON.stringify({ items: [] }), { status: 200 });
    }
    if (url.includes("/channel")) {
      return new Response(
        JSON.stringify({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 }),
        { status: 200 }
      );
    }
    if (url.includes("/session-subagents")) {
      return new Response(JSON.stringify({ subagents: [] }), { status: 200 });
    }
    if (url.includes("/session-mode")) {
      // ModeBar 挂载即拉（与远控无关的既有读面）：给「不支持回显」的最小载荷
      return new Response(
        JSON.stringify({
          tool: "zcode",
          current: null,
          currentLabel: null,
          readback: false,
          switchKind: "unsupported",
        }),
        { status: 200 }
      );
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
}

/** composer 单挂用的最小分路（可注入态——非 ZCode 回归对照用） */
function installInjectableFetch() {
  stubFetch((url) => {
    if (url.includes("/session-send-info")) {
      return new Response(
        JSON.stringify({ injectable: true, channels: ["tmux"], visibility: "realtime" }),
        { status: 200 }
      );
    }
    if (url.includes("/session-queue")) return new Response(JSON.stringify({ items: [] }));
    if (url.includes("/channel")) {
      return new Response(
        JSON.stringify({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 })
      );
    }
    if (url.includes("/session-approve-options")) {
      return new Response(
        JSON.stringify({
          available: false,
          options: [],
          verifiedWith: "t",
          currentVersion: null,
          drift: false,
        })
      );
    }
    if (url.includes("/session-question")) {
      return new Response(JSON.stringify({ available: false, questions: [] }));
    }
    if (url.includes("/official-control")) {
      return new Response(JSON.stringify({ configState: "saved", url: SAVED_URL }));
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
}

/** 某一 URL 前缀的**请求**次数（前缀用完整路径，避免 /session-send 命中 /session-send-info 这类误配） */
const fetched = (prefix: string) =>
  fetchMock.mock.calls.filter((c) => String(c[0]).startsWith(prefix)).length;
/** 某个 POST 端点是否被调用过（发送/审批是写动作，计数只看 POST） */
const postedTo = (path: string) =>
  fetchMock.mock.calls.filter(
    (c) => String(c[0]) === path && (c[1] as RequestInit | undefined)?.method === "POST"
  ).length;

/** 旧发送 / 无头通道请求数：ZCode 活路径（面板）零回退——含 GET send-info。 */
const legacySendCalls = () =>
  fetchMock.mock.calls.filter((c) => {
    const url = String(c[0]);
    return url.includes("/session-send") || /headless/i.test(url);
  }).length;

beforeEach(() => {
  controlRoute = { kind: "missing" };
  // jsdom 无 EventSource；显式置空让 hook 走「GET 首拍权威」路径（本文件只测可用性）。
  vi.stubGlobal("EventSource", undefined);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("ZCode 统一控制不可用时阅读面继续工作（A5 read_view_survives_control_unavailable）", () => {
  // T7.2 起 ZCode 会话的发送区由统一控制面板接管（连接态只来自 GET /zcode/*，
  // 已保存链接绝不等于已连接）。原 A5 的 official-control 占位档随之迁移：
  // missing/invalid → needs_config；saved → needs_auth；网络失败 → offline。
  it.each<[string, ControlRoute, string, string, RegExp]>([
    ["未配置", { kind: "missing" }, "needs_config", "needs_config", /配置/],
    ["格式错误", { kind: "invalid" }, "needs_config", "needs_config", /配置/],
    ["链接已保存但未授权", { kind: "saved" }, "needs_auth", "needs_auth", /授权/],
    // 连接 GET 网络失败：没有连接视图（state 保持 unknown），就地原因按 offline 申报
    ["读取失败", { kind: "networkFail" }, "unknown", "offline", /离线/],
  ])(
    "%s：正文与文件仍在，控制区给就地原因",
    async (_label, route, expectedState, expectedReason, expected) => {
      controlRoute = route;
      installDetailFetch();
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);

      // ① 读取链不受控制态影响：正文在、页头没有整页报错
      expect(await screen.findByText("合成正文乙")).toBeTruthy();
      expect(screen.queryByTestId("detail-error")).toBeNull();

      // ② 文件面板仍可打开并列出条目（既有 /session-files 数据源）
      fireEvent.click(await screen.findByTestId("file-panel-button"));
      const fileRow = await screen.findByTestId("file-row-0-open");
      expect(fileRow.textContent).toContain("synthetic.rs");

      // ③ 发送区 = 统一控制面板，就地给原因（等收敛后的连接态，不拿首拍当结果）
      const panel = await screen.findByTestId("zcode-control-panel");
      await waitFor(() => expect(panel.getAttribute("data-connection-state")).toBe(expectedState));
      const reason = await screen.findByTestId("zcode-control-reason");
      expect(reason.getAttribute("data-reason-code")).toBe(expectedReason);
      expect(reason.textContent).toMatch(expected);

      // ④ 控制未就绪时发送是硬门：草稿保留、零动作 POST
      const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
      fireEvent.change(draft, { target: { value: "这段草稿不能丢" } });
      const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
      expect(send.disabled).toBe(true);
      fireEvent.click(send);
      await act(async () => {});
      expect(draft.value).toBe("这段草稿不能丢");
      expect(postedTo("/m/api/v1/zcode/actions")).toBe(0);
    }
  );

  it("读取失败不被当成「未配置／已删除」，且只读不写", async () => {
    controlRoute = { kind: "networkFail" };
    installDetailFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const reason = await screen.findByTestId("zcode-control-reason");
    await waitFor(() => expect(reason.textContent).toMatch(/离线/));
    // 读取失败 ≠ 未配置/已删除（不把失败说成「去电脑端配置」）
    expect(reason.textContent).not.toMatch(/配置/);
    expect(fetched("/m/api/v1/zcode/connection")).toBeGreaterThan(0);
    for (const call of fetchMock.mock.calls) {
      const init = call[1] as RequestInit | undefined;
      expect(init?.method ?? "GET").toBe("GET");
    }
  });
});

describe("ZCode 发送区（活路径 = 统一控制面板）：保留草稿、不调旧发送／审批 API、外跳是显式动作", () => {
  /** 面板单挂（SessionDetail 对 zcode 会话挂的就是它）+ 等原因码收敛到位 */
  const renderPanel = async (expectedReason: string) => {
    installDetailFetch();
    render(<ZcodeControlPanel sessionId="sess-zc" />);
    const reason = await screen.findByTestId("zcode-control-reason");
    await waitFor(() => expect(reason.getAttribute("data-reason-code")).toBe(expectedReason));
    return reason;
  };

  it("a5_availability_uses_live_zcode_path", async () => {
    // 生产挂载点 = SessionDetail；ZCode 只允许走面板，旧 composer 的只读分支已删除。
    controlRoute = { kind: "saved" };
    installDetailFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);

    const panel = await screen.findByTestId("zcode-control-panel");
    await waitFor(() => expect(panel.getAttribute("data-connection-state")).toBe("needs_auth"));
    // 死分支不可达：composer 与其 ZCode 占位面都不在 DOM 里
    expect(screen.queryByTestId("message-composer")).toBeNull();
    expect(screen.queryByTestId("composer-input")).toBeNull();
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();

    // 显式辅助入口挂在活路径（面板）上：只开已保存链接，不带任何接口调用
    const openSpy = vi.fn(() => null);
    vi.spyOn(window, "open").mockImplementation(openSpy);
    const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
    fireEvent.change(draft, { target: { value: "活路径上的草稿" } });
    const callsBefore = fetchMock.mock.calls.length;
    fireEvent.click(await screen.findByTestId("zcode-open-official"));
    expect(openSpy).toHaveBeenCalledWith(SAVED_URL, "_blank", "noopener,noreferrer");
    expect(fetchMock.mock.calls).toHaveLength(callsBefore);
    expect(draft.value).toBe("活路径上的草稿");
    vi.spyOn(window, "open").mockRestore();

    // 未配对 → 发送是硬门：零动作 POST、零旧 send/无头通道
    fireEvent.click(screen.getByTestId("zcode-send"));
    await act(async () => {});
    expect(draft.value).toBe("活路径上的草稿");
    expect(postedTo("/m/api/v1/zcode/actions")).toBe(0);
    expect(postedTo("/m/api/v1/session-send")).toBe(0);
    expect(postedTo("/m/api/v1/session-approve")).toBe(0);
    expect(legacySendCalls()).toBe(0);
  });

  it("输入区仍可编辑（草稿保留），发送钮禁用且点击不发请求", async () => {
    controlRoute = { kind: "missing" };
    const reason = await renderPanel("needs_config");
    expect(reason.textContent).toMatch(/配置/);
    const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
    expect(draft.disabled).toBe(false);
    fireEvent.change(draft, { target: { value: "这段草稿不能丢" } });
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    expect(send.disabled).toBe(true);
    fireEvent.click(send);
    await act(async () => {});
    // 草稿不丢：禁用的是「发送」这个动作，不是把用户输入清掉
    expect(draft.value).toBe("这段草稿不能丢");
    // 不碰注入/审批面：零写、零旧 send/无头通道，连旧卡片探针也不发
    expect(postedTo("/m/api/v1/session-send")).toBe(0);
    expect(postedTo("/m/api/v1/session-approve")).toBe(0);
    expect(postedTo("/m/api/v1/zcode/actions")).toBe(0);
    expect(legacySendCalls()).toBe(0);
    expect(fetched("/m/api/v1/session-approve-options")).toBe(0);
    expect(fetched("/m/api/v1/session-question")).toBe(0);
  });

  it("已配置（待授权）同样不调用发送／审批 API，原因说「连接待授权」", async () => {
    controlRoute = { kind: "saved" };
    const reason = await renderPanel("needs_auth");
    expect(reason.textContent).toMatch(/授权/);
    const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
    fireEvent.change(draft, { target: { value: "留着" } });
    fireEvent.click(screen.getByTestId("zcode-send"));
    await act(async () => {});
    expect(draft.value).toBe("留着");
    expect(postedTo("/m/api/v1/session-send")).toBe(0);
    expect(postedTo("/m/api/v1/session-approve")).toBe(0);
    expect(postedTo("/m/api/v1/zcode/actions")).toBe(0);
    expect(legacySendCalls()).toBe(0);
  });

  it("「打开官方页面」只在用户点击时新标签页打开，且不标已读／不离开本页", async () => {
    controlRoute = { kind: "saved" };
    const openSpy = vi.fn(() => null);
    vi.spyOn(window, "open").mockImplementation(openSpy);
    const reason = await renderPanel("needs_auth");
    // 草稿先打在输入框里（外跳不该把它冲掉）
    const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
    fireEvent.change(draft, { target: { value: "外跳前就写好的草稿" } });
    const callsBefore = fetchMock.mock.calls.length;
    fireEvent.click(await screen.findByTestId("zcode-open-official"));
    // 用户主动选择的辅助动作：新标签页 + noopener，不带任何接口调用
    expect(openSpy).toHaveBeenCalledWith(SAVED_URL, "_blank", "noopener,noreferrer");
    expect(fetchMock.mock.calls).toHaveLength(callsBefore);
    expect(draft.value).toBe("外跳前就写好的草稿");
    // 尤其不是「标已读／归档／打开」这类会动未读池的写接口，也不是发送/审批
    expect(postedTo("/m/api/v1/session-hide")).toBe(0);
    expect(postedTo("/m/api/v1/session-open")).toBe(0);
    expect(postedTo("/m/api/v1/session-send")).toBe(0);
    expect(postedTo("/m/api/v1/session-approve")).toBe(0);
    expect(legacySendCalls()).toBe(0);
    // 本页原样：原因仍在（外跳不冒充统一控制完成）
    expect(reason.textContent).toMatch(/授权/);
  });

  it("未配置／读取失败时不给「打开官方页面」（坏值与空值都没有可开链接）", async () => {
    controlRoute = { kind: "missing" };
    await renderPanel("needs_config");
    expect(screen.queryByTestId("zcode-open-official")).toBeNull();
    cleanup();
    controlRoute = { kind: "networkFail" };
    await renderPanel("offline");
    expect(screen.queryByTestId("zcode-open-official")).toBeNull();
  });

  it("非 ZCode 会话不读远控配置，也不禁用输入区（既有注入路径零回归）", async () => {
    installInjectableFetch();
    render(<MessageComposer session={{ id: "sess-cl" }} />);
    await screen.findByTestId("composer-input");
    expect(fetched("/m/api/v1/official-control")).toBe(0);
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(false);
    // 既有卡片在场探针照旧发（本任务没动非 ZCode 的分流）
    expect(fetched("/m/api/v1/session-approve-options")).toBeGreaterThan(0);
  });
});
