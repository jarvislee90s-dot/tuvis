import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "@/mobile/App";
import SessionDetail, { isPlanPending } from "@/mobile/SessionDetail";
import type { SessionFileEntry, SessionMessage, SubagentView } from "@/mobile/api";
import SessionDetail, {
  HEADLESS_STAGE_TRIAGE,
  HeadlessReceiptCard,
  headlessStageText,
  isPlanPending,
} from "@/mobile/SessionDetail";
import type { SessionFileEntry, SessionMessage } from "@/mobile/api";
import { BOOKMARK_COLORS, clearBookmarks, messageAnchor } from "@/mobile/bookmarks";
import { MockEventSource } from "./eventSourceMock";
import type { Session } from "@/types/session";
import planPendingCases from "../fixtures/plan_pending_cases.json";
import stagesFixture from "../fixtures/headless_stages.json";

// M3 Task 8：ZCode 式会话详情页渲染矩阵。fetch 全量 stub（盖过 setup.ts 的 msw），
// 按 URL 分路到 messages / session-files / file 三端点；jsdom 无真实高亮，
// 只断言容器与文本存在（遵循任务约束）。

function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: "sess-1",
    agentType: "claude",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "修 bug",
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
    ...overrides,
  };
}

/** M3+ 文件面板条目夹具（后端 FileEntry camelCase 契约） */
function fileEntry(path: string, over: Partial<SessionFileEntry> = {}): SessionFileEntry {
  return { path, lastSeq: 1, lastTs: 1000, hits: 1, modified: true, ...over };
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

interface Routes {
  messages?: SessionMessage[];
  messagesStatus?: number;
  messagesNetworkFail?: boolean;
  /** Bug 1（M3 验收）：后端头部截断标记，随 /session-messages 载荷返回 */
  truncated?: boolean;
  files?: SessionFileEntry[];
  filesTruncated?: boolean;
  fileContent?: string;
  fileMime?: string;
  fileStatus?: number;
  /** 评审 M4：/session-open 路由（缺省 200 opening；failed 载荷驱动 C1 分诊测试）；
   *  sessionOpenStatus 驱动 404 错误码分支（no_cwd / no_resume_command / no_session） */
  sessionOpen?: { status?: string; error?: string };
  sessionOpenStatus?: number;
  /** 发送能力探测（MessageComposer 挂载即拉）：可注入态夹具。
   *  组件在 infoReady 前 / sendInfo 为 null 时自隐——缺省给可注入，使 composer 渲染 */
  sendInfo?: { injectable: boolean; channels: string[]; visibility: string };
  /** 审批选项卡数据源（ApproveCard 挂载即拉）：available 为假时卡自隐——
   *  分屏挂载断言需给 available=true，否则断言的是「卡自隐」而非「没挂载」 */
  approveOptions?: {
    available: boolean;
    options: { id: string; label: string }[];
    verifiedWith: string;
    currentVersion: string | null;
    drift: boolean;
    reason?: string;
    /** 丁T2：计划待确认预期态（codex/kimi 的计划确认框入口） */
    planPending?: boolean;
    /** 丁T2：屏读选项（dialog:<n>）与计划聚合（T8） */
    dialog?: boolean;
    plan?: { content: string; isFile: boolean } | null;
  };
  /** 审批应答 POST 回执（丁T2 全链用例） */
  approve?: { status: string; error?: string };
  /** 问答卡数据源（批次乙 T8，QuestionCard 挂载即拉）：available 为假时卡自隐——
   *  缺省 available=false（不改既有用例渲染）；问答挂载断言需显式给可用载荷 */
  questionInfo?: { available: boolean; questions: unknown[]; source?: string };
  /** 问答应答 POST 回执（F2-1 用例需要 key_sent 终态；缺省 key_sent） */
  questionAnswer?: { status: string; error?: string };
  /** 子 agent 全量名单（观察台 T5：SessionDetail 挂载即拉；缺省 [] = 无子 agent，
   *  卡区/chip 不渲染——既有用例行为不变） */
  subagents?: SubagentView[];
  /** **Task 8（H7）无头回合回执**：给对象 → 立即 200 返回该载荷；给 `"pending"` → 返回
   *  可手动 resolve 的 promise（驱动「发送中」态：发送中卡片与取消钮必须在场） */
  sessionSend?: Record<string, unknown> | "pending" | "reject";
  /** 无头回合取消（POST /session-headless-cancel）回执 */
  headlessCancel?: { cancelled: boolean; reason?: string };
}

/** 手动闸：`routes.sessionSend === "pending"` 时由用例自行 resolve（零真实等待） */
let sendGate: { promise: Promise<Response>; resolve: (body: unknown) => void } | null = null;

function openSendGate() {
  let resolve!: (body: unknown) => void;
  const promise = new Promise<Response>((res) => {
    resolve = (body: unknown) => res(new Response(JSON.stringify(body), { status: 200 }));
  });
  sendGate = { promise, resolve };
  return promise;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  routes = {};
  sendGate = null;
  // 书签 store 用例间隔离（模块级单例 + localStorage 镜像，不清理会串场——
  // 如上一个用例占用了某颜色，下一个用例的调色板里该色就变置灰不可点）
  clearBookmarks("sess-1");
  window.localStorage.removeItem("mam-bookmarks");
  // matchMedia 用例间隔离：默认「无 matchMedia」= 窄屏语义（jsdom 原生行为），
  // 需要宽屏的用例自行安装后由 afterEach 还原（旧版仅少数用例安装，
  // 泄漏会让后续用例误判宽屏——如面板默认布局用例）
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

/** 按 URL 分路的 fetch stub，返回 mock 供断言调用参数 */
function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-messages")) {
      if (routes.messagesNetworkFail) throw new TypeError("network down");
      if (routes.messagesStatus) {
        return new Response("gone", { status: routes.messagesStatus });
      }
      return new Response(
        JSON.stringify({ messages: routes.messages ?? [], truncated: routes.truncated === true }),
        { status: 200 }
      );
    }
    if (url.includes("/session-files")) {
      return new Response(
        JSON.stringify({ files: routes.files ?? [], truncated: routes.filesTruncated === true }),
        { status: 200 }
      );
    }
    if (url.includes("/host")) {
      // 书签恢复（M3+）依赖 bootId：默认给固定值（用例间由 beforeEach 清 store）
      return new Response(
        JSON.stringify({
          host: { name: "n", platform: "windows", version: "0", bootId: "boot-test" },
          enabledTools: [],
          installedTools: ["claude", "codex", "kimi", "opencode"],
        }),
        { status: 200 }
      );
    }
    if (url.includes("/session-open")) {
      return new Response(JSON.stringify(routes.sessionOpen ?? { status: "opening" }), {
        status: routes.sessionOpenStatus ?? 200,
      });
    }
    if (url.includes("/session-send-info")) {
      // MessageComposer 挂载即拉；缺省给可注入，使分屏态 composer 真正渲染出来
      // （sendInfo 为 null 时组件自隐，会把「分屏有没有挂载」的断言变成假阴性）
      return new Response(
        JSON.stringify(
          routes.sendInfo ?? { injectable: true, channels: ["tmux"], visibility: "realtime" }
        ),
        { status: 200 }
      );
    }
    if (url.includes("/session-headless-cancel")) {
      // Task 8：取消钮目标（无头回执卡）
      return new Response(JSON.stringify(routes.headlessCancel ?? { cancelled: true }), {
        status: 200,
      });
    }
    if (url.includes("/session-send")) {
      // Task 8：发送回执（缺省 delivered，与既有终端路径同形）；"pending" = 手动闸；
      // "reject" = 请求本身抛异常（网络断/开关中途关/500）
      if (routes.sessionSend === "reject") throw new TypeError("network down");
      if (routes.sessionSend === "pending") return openSendGate();
      return new Response(JSON.stringify(routes.sessionSend ?? { status: "delivered" }), {
        status: 200,
      });
    }
    if (url.includes("/session-question/answer")) {
      // 问答应答 POST（F2-1 用例需要 key_sent 终态）；判序在 GET 之前——
      // /session-question 是 /session-question/answer 的前缀（QuestionCard.test 同款教训）
      return new Response(JSON.stringify(routes.questionAnswer ?? { status: "key_sent" }), {
        status: 200,
      });
    }
    if (url.includes("/session-question")) {
      // QuestionCard 挂载即拉（批次乙 T8）；缺省给 available=false（卡自隐，不改
      // 既有用例渲染）。「问答卡挂载」用例须显式给可用载荷
      return new Response(
        JSON.stringify(routes.questionInfo ?? { available: false, questions: [] }),
        { status: 200 }
      );
    }
    if (url.includes("/session-approve-options")) {
      // ApproveCard 挂载即拉；缺省给 available=false 无 reason（卡自隐，不改既有用例渲染）。
      // 「分屏红卡挂载」用例须显式给 available=true——否则断言的是卡自隐而非没挂载
      return new Response(
        JSON.stringify(
          routes.approveOptions ?? {
            available: false,
            options: [],
            verifiedWith: "test",
            currentVersion: null,
            drift: false,
          }
        ),
        { status: 200 }
      );
    }
    if (url.includes("/session-approve")) {
      // 审批应答 POST（丁T2 全链用例）；判序在 -options 之后（前缀包含关系，同 ApproveCard 测试）
      return new Response(JSON.stringify(routes.approve ?? { status: "key_sent" }), {
        status: 200,
      });
    }
    if (url.includes("/file?")) {
      if (routes.fileStatus) return new Response("no", { status: routes.fileStatus });
      return new Response(
        JSON.stringify({
          content: routes.fileContent ?? "",
          mime: routes.fileMime ?? "text/plain",
        }),
        { status: 200 }
      );
    }
    if (url.includes("/session-subagents")) {
      // 观察台 T5：SessionDetail 挂载即拉全量名单——读 routes.subagents（缺省 []
      // 与旧桩同形，既有用例行为不变）
      return new Response(JSON.stringify({ subagents: routes.subagents ?? [] }), { status: 200 });
    }
    if (url.includes("/session-subagent-messages")) {
      // 观察台 T6：SubagentDetail 详情载荷桩（本页既有用例不开详情对话框，
      // 缺省空载荷即可——防御 SubagentDetail 挂载时的意外请求）
      return new Response(
        JSON.stringify({ messages: [], truncated: false, supported: true }),
        { status: 200 }
      );
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

describe("SessionDetail：消息渲染与折叠交互（P9）", () => {
  it("渲染对话：assistant 正文走 markdown，user 直显", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "帮我看看" }),
      msg({ seq: 1, kind: "assistant", content: "已修复，重点在 **并发** 处" }),
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect(await screen.findByText("帮我看看")).toBeTruthy();
    // markdown：**并发** → <strong>（jsdom 无真实高亮，断言元素与文本即可）
    const strong = await screen.findByText("并发");
    expect(strong.tagName).toBe("STRONG");
    // 页头：项目名 + 工具名
    expect(screen.getByText("proj")).toBeTruthy();
    expect(screen.getByText(/Claude/)).toBeTruthy();
  });

  it("运行中：thinking/tool-call 默认折叠，点击展开显示 toolName+toolArgs", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "查一下" }),
      msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
      msg({
        seq: 2,
        kind: "tool-call",
        content: "调用 Bash",
        toolName: "Bash",
        toolArgs: '{"command":"ls"}',
      }),
      msg({ seq: 3, kind: "assistant", content: "结论" }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    expect(await screen.findByText("结论")).toBeTruthy();
    // 默认折叠：内容不可见，折叠头可见
    expect(screen.queryByText("内部思考内容")).toBeNull();
    expect(screen.queryByText('{"command":"ls"}')).toBeNull();
    expect(screen.getByTestId("msg-1-toggle").textContent).toContain("思考过程");
    expect(screen.getByTestId("msg-2-toggle").textContent).toContain("调用 Bash");
    // 点击展开：thinking 内容 + toolArgs 均出现，aria-expanded 翻转
    fireEvent.click(screen.getByTestId("msg-1-toggle"));
    expect(screen.getByText("内部思考内容")).toBeTruthy();
    fireEvent.click(screen.getByTestId("msg-2-toggle"));
    expect(screen.getByText('{"command":"ls"}')).toBeTruthy();
    expect(screen.getByTestId("msg-2-toggle").getAttribute("aria-expanded")).toBe("true");
    // 再点回收起
    fireEvent.click(screen.getByTestId("msg-2-toggle"));
    expect(screen.queryByText('{"command":"ls"}')).toBeNull();
  });

  it("idle 会话（总结模式）：过程消息与更早 assistant 自动折叠，只显最后 assistant 总结", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "查一下" }),
      msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
      msg({ seq: 2, kind: "assistant", content: "中间回复" }),
      msg({ seq: 3, kind: "tool-call", content: "调用 Grep", toolName: "Grep" }),
      msg({ seq: 4, kind: "assistant", content: "最终总结" }),
    ];
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    // user 与最后 assistant 直显；过程消息与中间 assistant 折叠
    expect(await screen.findByText("查一下")).toBeTruthy();
    expect(screen.getByText("最终总结")).toBeTruthy();
    expect(screen.queryByText("内部思考内容")).toBeNull();
    expect(screen.queryByText("中间回复")).toBeNull();
    // tool-call 只剩折叠头（标签=「调用 Grep」），无展开内容
    expect(screen.getByTestId("msg-3-toggle").textContent).toContain("调用 Grep");
    // 更早 assistant 折叠头标签 =「更早的回复」，点击展开
    fireEvent.click(screen.getByTestId("msg-2-toggle"));
    expect(screen.getByText("中间回复")).toBeTruthy();
    // 过程消息同样可展开
    fireEvent.click(screen.getByTestId("msg-1-toggle"));
    expect(screen.getByText("内部思考内容")).toBeTruthy();
  });

  it("加载更早消息：点击以更大 limit 整页重拉（200 → 400）", async () => {
    installFetch();
    routes.messages = Array.from({ length: 200 }, (_, i) =>
      msg({ seq: i, kind: "user", content: `m${i}` })
    );
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect(await screen.findByText("m199")).toBeTruthy();
    fireEvent.click(screen.getByTestId("load-more"));
    // timeout 加固（flaky 修复）：mock 本身同步 resolve（无真实 timer 需求），
    // 但 59 文件并行时 fetch mock→state 更新链可能被资源竞争拉长，默认 1s 不够
    await waitFor(
      () => {
        const called400 = fetchMock.mock.calls.some((c: unknown[]) =>
          String(c[0]).includes("limit=400")
        );
        expect(called400).toBe(true);
      },
      { timeout: 3000 }
    );
  }, 15000); // 用例级 timeout 加固（flaky 修复②）：60 文件并行时 waitFor 内的
  // mock→state 链可能吃满默认 5s 用例预算（b7a407d 只加固了 waitFor 自身）

  it("Bug 1：truncated=true 且条数 < limit 时仍显示加载更早（胖会话字节截断）", async () => {
    installFetch();
    // 胖 JSONL 单行吃掉整个 512KB 字节窗：返回条数远小于 limit，但头部被切
    routes.messages = Array.from({ length: 5 }, (_, i) =>
      msg({ seq: i, kind: "user", content: `m${i}` })
    );
    routes.truncated = true;
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect(await screen.findByText("m4")).toBeTruthy();
    // 旧实现条件 length >= limit 恒 false → 按钮死功能；新实现 truncated 也能触发
    expect(screen.getByTestId("load-more")).toBeTruthy();
    // 点击 → 以更大 limit 重拉（后端字节窗随 limit 放大，更早内容可达）
    fireEvent.click(screen.getByTestId("load-more"));
    await waitFor(
      () => {
        const called400 = fetchMock.mock.calls.some((c: unknown[]) =>
          String(c[0]).includes("limit=400")
        );
        expect(called400).toBe(true);
      },
      { timeout: 3000 }
    );
  }, 15000); // 用例级 timeout 加固：同上
});

describe("SessionDetail：文件链接化与预览联动", () => {
  it("文件路径渲染为链接：点击开全屏预览，可切分屏，关闭回到对话", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    routes.fileMime = "text/rust";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 链接化按钮出现在正文里（content 中出现的已知路径）
    fireEvent.click(await screen.findByTestId("file-link"));
    // 默认全屏浮层
    const preview = await screen.findByTestId("file-preview");
    expect(preview.getAttribute("data-mode")).toBe("fullscreen");
    expect((await screen.findByTestId("preview-code")).textContent).toContain("fn main() {}");
    // 切分屏：data-mode 翻转（split = 上对话下文件由布局类承担，此处锁语义切换）
    // —— 切换器唯一实例在顶栏 sheet bar（T1 审查 S5 裁决：组件内部实例已删）
    fireEvent.click(screen.getByTestId("preview-toggle-split"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split");
    // 关闭预览：对话仍在（同一组件树内状态保持）
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(screen.queryByTestId("file-preview")).toBeNull();
    expect(screen.getByText("改了", { exact: false })).toBeTruthy();
  });

  it("Bug 2：全屏预览态下切换按钮可达（FilePreview 页头），一键切分屏出分屏容器", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    // 默认全屏浮层（jsdom 无 matchMedia → 窄屏默认；宽屏自适应由 openFile 判定）
    const preview = await screen.findByTestId("file-preview");
    expect(preview.getAttribute("data-mode")).toBe("fullscreen");
    // 全屏浮层 fixed inset-0：顶栏 sheet bar 在浮层顶部可达（唯一实例）；
    // FilePreview 内部切换器仅显式传 onModeChange 的直用场景渲染
    fireEvent.click(screen.getByTestId("preview-toggle-split"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split");
    // 分屏容器出现（上对话下文件布局，Task 8 裁决）
    expect(screen.getByTestId("split-container")).toBeTruthy();
    // 分屏态可一键切回全屏
    fireEvent.click(screen.getByTestId("preview-toggle-fullscreen"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("fullscreen");
  });

  it("Bug 2 顺手项：宽屏（matchMedia ≥768px）打开文件自动进左右分屏 split-h，窄屏默认全屏", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    // jsdom 未实现 matchMedia：装 shim（theme.test.ts 同款模式）
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (q: string) => ({ matches: q.includes("min-width"), media: q }),
    });
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    // T2 决策 2：宽屏默认 split-h（左右分行），不再是纵向 split
    expect((await screen.findByTestId("file-preview")).getAttribute("data-mode")).toBe("split-h");
    expect(screen.getByTestId("split-container").className).toContain("flex-row");
  });

  it("Bug 5：markdown 标题/列表渲染结构化元素且容器挂 md-body 排版类", async () => {
    installFetch();
    routes.messages = [
      msg({
        seq: 0,
        kind: "assistant",
        content:
          "## 小节标题\n\n- 第一项\n- 第二项\n\n1. 有序\n\n> 引用\n\n| 列A | 列B |\n|---|---|\n| a | b |",
      }),
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect(await screen.findByText("小节标题")).toBeTruthy();
    // 结构化元素存在（ReactMarkdown 本就产出；视觉拍平是 CSS 层问题）
    const h2 = screen.getByText("小节标题").closest("h2");
    expect(h2).toBeTruthy();
    const firstLi = screen.getByText("第一项").closest("li");
    expect(firstLi).toBeTruthy();
    expect(firstLi!.closest("ul")).toBeTruthy();
    expect(screen.getByText("有序").closest("ol")).toBeTruthy();
    expect(screen.getByText("引用").closest("blockquote")).toBeTruthy();
    expect(document.querySelector(".md-body table")).toBeTruthy();
    // Bug 5 修复锚点：渲染容器必须挂 .md-body 排版类（mobile.css 提供
    // 标题分级/列表符号/引用边框/表格边框，对抗 preflight 重置）
    expect(h2!.closest(".md-body")).toBeTruthy();
  });

  it("Bug 8：总结模式提示行计数正确，展开全部/收起全部生效；运行中不出现", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "查一下" }),
      msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
      msg({ seq: 2, kind: "assistant", content: "中间回复" }),
      msg({ seq: 3, kind: "tool-call", content: "调用 Grep", toolName: "Grep" }),
      msg({ seq: 4, kind: "assistant", content: "最终总结" }),
    ];
    const { unmount } = render(
      <SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />
    );
    await screen.findByText("最终总结");
    // 计数 = 当前被折叠的可折叠条数（thinking / 中间 assistant / tool-call = 3；
    // user 直显、最终 assistant 总结直显，不计入）
    const banner = screen.getByTestId("summary-banner");
    expect(banner.textContent).toContain("总结模式");
    expect(banner.textContent).toContain("已折叠 3 条过程消息");
    // 展开全部：过程消息全部可见，计数归零，收起全部出现
    fireEvent.click(screen.getByTestId("expand-all"));
    expect(screen.getByText("内部思考内容")).toBeTruthy();
    expect(screen.getByText("中间回复")).toBeTruthy();
    expect(screen.getByTestId("summary-banner").textContent).toContain("已折叠 0 条过程消息");
    // 收起全部：恢复默认折叠语义
    fireEvent.click(screen.getByTestId("collapse-all"));
    expect(screen.queryByText("内部思考内容")).toBeNull();
    expect(screen.getByTestId("summary-banner").textContent).toContain("已折叠 3 条过程消息");
    unmount();
    // 运行中模式：折叠是 wire 语义，不出现总结提示行
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    expect(await screen.findByText("查一下")).toBeTruthy();
    expect(screen.queryByTestId("summary-banner")).toBeNull();
  });

  it("需求 1：横向分屏（左对话右文件）可用，与上下分屏/全屏三态互通", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    // 打开默认全屏 → 切横向分屏（左对话右文件）
    fireEvent.click(await screen.findByTestId("preview-toggle-split-h"));
    const preview = screen.getByTestId("file-preview");
    expect(preview.getAttribute("data-mode")).toBe("split-h");
    const split = screen.getByTestId("split-container");
    // 横向分屏容器：flex-row（左对话右文件）——与纵向 split 的 flex-col 区分
    expect(split.className).toContain("flex-row");
    // 对话与文件都在同一屏（同一容器内两个子区）；文件内容为异步拉取，等就绪
    expect(split.textContent).toContain("改了");
    expect((await screen.findByTestId("preview-code")).textContent).toContain("fn main() {}");
    // 三态互通：纵分屏 ↔ 横分屏 ↔ 全屏
    fireEvent.click(screen.getByTestId("preview-toggle-split"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split");
    expect(screen.getByTestId("split-container").className).toContain("flex-col");
    fireEvent.click(screen.getByTestId("preview-toggle-split-h"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split-h");
    fireEvent.click(screen.getByTestId("preview-toggle-fullscreen"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("fullscreen");
    expect(screen.queryByTestId("split-container")).toBeNull();
  });

  it("图标语义不得颠倒：split 按钮画上下两格（rows-2）、split-h 画左右两格（columns-2）", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    const btn = (id: string) => screen.getByTestId(id);
    // SVG 的 className 是 SVGAnimatedString，取 class 属性字符串
    const iconClass = (id: string) => btn(id).querySelector("svg")!.getAttribute("class") ?? "";
    // 上下分屏（split）= 上下两格 → rows-2
    expect(iconClass("preview-toggle-split")).toContain("lucide-rows-2");
    // 左右分屏（split-h）= 左右两格 → columns-2
    expect(iconClass("preview-toggle-split-h")).toContain("lucide-columns-2");
  });

  // 分屏态对话能力（2026-09-19 用户裁决）：分屏 = 对话列 + 文件列的并列布局，
  // 对话列必须保有完整对话能力。原实现把 MessageComposer / ApproveCard 排除在
  // 分屏分支外（仅非分屏正文视图挂载），致分屏看文件时**输入框消失、红卡不可见**——
  // 用户实测报告，且该行为在 docs/ 全库无任何设计依据（系实现越权）。
  // 本组为用户可见行为的回归锁：分屏两态（split / split-h）下两者都必须挂载。
  describe("分屏态对话能力（2026-09-19 用户裁决回归锁）", () => {
    it("上下分屏（split）下发送输入框仍挂载——分屏看文件也能发消息", async () => {
      installFetch();
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      fireEvent.click(await screen.findByTestId("preview-toggle-split"));
      const split = screen.getByTestId("split-container");
      // 关键断言：输入框**在分屏容器内**（修正前分屏分支不挂 composer；
      // 必须用包含关系锁死，避免被其他分支误命中）
      expect(await within(split).findByTestId("message-composer")).toBeTruthy();
    });

    it("左右分屏（split-h）下发送输入框仍挂载", async () => {
      installFetch();
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      fireEvent.click(await screen.findByTestId("preview-toggle-split-h"));
      const split = screen.getByTestId("split-container");
      expect(await within(split).findByTestId("message-composer")).toBeTruthy();
    });

    it("waiting 态上下分屏下审批红卡仍挂载——分屏也必须能看到待审批", async () => {
      installFetch();
      // 消息正文须含项目文件路径才会渲染 file-link（openFile 入口）
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "等批准，相关文件 /tmp/proj/src/app.rs" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      routes.approveOptions = {
        available: true,
        options: [
          { id: "allow", label: "允许" },
          { id: "deny", label: "拒绝" },
        ],
        verifiedWith: "claude 2.1.251",
        currentVersion: "claude 2.1.251",
        drift: false,
      };
      render(<SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      fireEvent.click(await screen.findByTestId("preview-toggle-split"));
      const split = screen.getByTestId("split-container");
      expect(split).toBeTruthy();
      // 关键断言：红卡**在分屏容器内**（修正前分屏分支不挂红卡；仅断言
      // findByTestId 会被非分屏分支或浮层误命中 → 必须用包含关系锁死）。
      // findBy 等选项载荷落地：T1 组件钥匙前缀区分后（approve-*/composer-* 互异，
      // 防同 key 兄弟复用错乱），正文→分屏的布局切换是真实的卸载/重挂，红卡
      // 选项拉取异步就绪——同步 getBy 会读到拉取前的自隐窗（既有语义不变）
      expect(await within(split).findByTestId("approve-card")).toBeTruthy();
    });

    it("waiting 态左右分屏下审批红卡仍挂载", async () => {
      installFetch();
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "等批准，相关文件 /tmp/proj/src/app.rs" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      routes.approveOptions = {
        available: true,
        options: [
          { id: "allow", label: "允许" },
          { id: "deny", label: "拒绝" },
        ],
        verifiedWith: "claude 2.1.251",
        currentVersion: "claude 2.1.251",
        drift: false,
      };
      render(<SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      fireEvent.click(await screen.findByTestId("preview-toggle-split-h"));
      const split = screen.getByTestId("split-container");
      // 同上：包含关系锁死 + 等选项载荷落地（T1 组件钥匙前缀区分后的重挂语义）
      expect(await within(split).findByTestId("approve-card")).toBeTruthy();
    });

    it("非 waiting 态分屏下不渲染红卡（状态门不变）", async () => {
      installFetch();
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      fireEvent.click(await screen.findByTestId("preview-toggle-split"));
      expect(screen.getByTestId("split-container")).toBeTruthy();
      expect(screen.queryByTestId("approve-card")).toBeNull();
      // 但输入框仍在（两者门控条件不同：红卡看状态，输入框无条件）
      expect(await screen.findByTestId("message-composer")).toBeTruthy();
    });

    it("全屏浮层态：红卡与输入框不挂载（浮层覆盖对话属预期，非本裁决范围）", async () => {
      installFetch();
      routes.messages = [
        msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
      ];
      routes.files = [fileEntry("/tmp/proj/src/app.rs")];
      routes.fileContent = "fn main() {}";
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);
      fireEvent.click(await screen.findByTestId("file-link"));
      // file-link 打开默认即全屏
      expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("fullscreen");
      expect(screen.queryByTestId("split-container")).toBeNull();
    });
  });

  it("需求：分屏分隔条可拖动——横向拖动改变文件栏宽度，纵向拖动改变高度", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));

    // ---- 横向分屏：拖分隔条 → 文件栏宽度变化（百分比） ----
    fireEvent.click(await screen.findByTestId("preview-toggle-split-h"));
    const container = screen.getByTestId("split-container");
    // jsdom 无布局引擎：注入容器矩形（宽 400）使拖动换算可算
    container.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 400, height: 600, right: 400, bottom: 600 }) as DOMRect;
    const handleH = screen.getByTestId("split-handle");
    const filePane = screen.getByTestId("split-file-pane");
    const initial = parseFloat(filePane.style.width);
    fireEvent.pointerDown(handleH, { clientX: 300 });
    fireEvent.pointerMove(window, { clientX: 320 });
    fireEvent.pointerUp(window);
    const narrower = parseFloat(filePane.style.width);
    // 向右拖 20px / 容器宽 400 → 分隔条右移 → 右侧文件栏变窄 5 个百分点
    // （2026-09-16 用户裁决：分隔条跟随指针）
    expect(narrower).toBeCloseTo(initial - 5, 1);
    // 反向：向左拖回（文件栏变宽）
    fireEvent.pointerDown(handleH, { clientX: 320 });
    fireEvent.pointerMove(window, { clientX: 260 });
    fireEvent.pointerUp(window);
    expect(parseFloat(filePane.style.width)).toBeCloseTo(narrower + 15, 1);

    // ---- 纵向分屏：拖分隔条 → 文件栏高度变化 ----
    // 2026-09-20 用户裁决：竖屏 split 换位为文件在上、对话在下（ratioPane="before"）——
    // 文件栏在分隔条**上方**，拖动方向随之取反：向下拖 = 分隔条下移把上方文件栏撑大。
    // 「分隔条跟随指针」裁决不变（拖哪边文件栏都变小）
    fireEvent.click(screen.getByTestId("preview-toggle-split"));
    const containerV = screen.getByTestId("split-container");
    containerV.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 400, height: 800, right: 400, bottom: 800 }) as DOMRect;
    const handleV = screen.getByTestId("split-handle");
    const filePaneV = screen.getByTestId("split-file-pane");
    const h0 = parseFloat(filePaneV.style.height);
    fireEvent.pointerDown(handleV, { clientY: 400 });
    fireEvent.pointerMove(window, { clientY: 500 });
    fireEvent.pointerUp(window);
    // 向下拖 100px / 容器高 800 → 分隔条下移 → 上方文件栏变高 12.5 个百分点
    expect(parseFloat(filePaneV.style.height)).toBeCloseTo(h0 + 12.5, 1);
    // 反向：向上拖回（上方文件栏变矮）
    fireEvent.pointerDown(handleV, { clientY: 500 });
    fireEvent.pointerMove(window, { clientY: 400 });
    fireEvent.pointerUp(window);
    expect(parseFloat(filePaneV.style.height)).toBeCloseTo(h0, 1);

    // ---- 拖动不得越界（钳制 15%–85%） ----
    fireEvent.pointerDown(handleV, { clientY: 0 });
    fireEvent.pointerMove(window, { clientY: -100000 });
    fireEvent.pointerUp(window);
    expect(parseFloat(filePaneV.style.height)).toBeGreaterThanOrEqual(14.9);
    fireEvent.pointerDown(handleV, { clientY: 800 });
    fireEvent.pointerMove(window, { clientY: 100000 });
    fireEvent.pointerUp(window);
    expect(parseFloat(filePaneV.style.height)).toBeLessThanOrEqual(85.1);
  });

  // 竖屏分屏换位（2026-09-20 用户裁决）：split 视觉顺序 = 文件在上、对话在下
  // （输入框贴底）；split-h 维持对话在左、文件在右。CSS order 视觉换位，
  // DOM 顺序两态一致（对话优先，a11y 不变）
  it("竖屏 split 视觉换位：文件 order-1 在上、对话 order-3 在下；split-h 无 order", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    fireEvent.click(await screen.findByTestId("preview-toggle-split"));
    const container = screen.getByTestId("split-container");
    const convCol = container.querySelector(".flex.min-h-0.min-w-0.flex-1.flex-col")!;
    const filePane = screen.getByTestId("split-file-pane");
    const handle = screen.getByTestId("split-handle");
    expect(convCol.className).toContain("order-3");
    expect(filePane.className).toContain("order-1");
    expect(handle.className).toContain("order-2");
    // 最小高度保护（仅 split）：对话列有 minHeight，文件栏有 maxHeight 上限
    expect(convCol.getAttribute("style")).toContain("min-height");
    expect(filePane.getAttribute("style")).toContain("max-height");

    // 切横向分屏：两态语义各自独立——无 order 类、无高度保护
    fireEvent.click(await screen.findByTestId("preview-toggle-split-h"));
    const containerH = screen.getByTestId("split-container");
    const convColH = containerH.querySelector(".flex.min-h-0.min-w-0.flex-1.flex-col")!;
    const filePaneH = screen.getByTestId("split-file-pane");
    expect(convColH.className).not.toContain("order-");
    expect(filePaneH.className).not.toContain("order-");
    expect(convColH.getAttribute("style")).not.toContain("min-height");
    expect(filePaneH.getAttribute("style")).not.toContain("max-height");
  });

  it("切换器与关闭钮唯一实例在顶栏 sheet bar（T1 审查 S5 裁决），预览组件内部不再有", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);

    // 预览未打开：无顶栏 sheet bar，整个页面没有任何布局切换器
    expect(screen.queryByTestId("sheet-bar")).toBeNull();
    expect(screen.queryAllByTestId("preview-toggle-split")).toHaveLength(0);
    expect(screen.queryAllByTestId("preview-mode-split")).toHaveLength(0);

    fireEvent.click(await screen.findByTestId("file-link"));
    // 打开后：切换器与关闭钮恰好各一份，且都在顶栏 sheet bar 内（唯一实例）
    expect(screen.getAllByTestId("preview-toggle-split")).toHaveLength(1);
    expect(screen.getAllByTestId("preview-close")).toHaveLength(1);
    const bar = screen.getByTestId("sheet-bar");
    expect(bar.contains(screen.getByTestId("preview-toggle-split"))).toBe(true);
    expect(bar.contains(screen.getByTestId("preview-close"))).toBe(true);
    // 预览组件内部不得再有切换器/关闭钮（S5：内部实例删除或不再渲染）
    const previewEl = screen.getByTestId("file-preview");
    expect(previewEl.contains(screen.getByTestId("preview-toggle-split"))).toBe(false);
    expect(previewEl.contains(screen.getByTestId("preview-close"))).toBe(false);
    // 详情页头旧前缀（preview-mode-*）不得复活
    expect(screen.queryAllByTestId("preview-mode-split")).toHaveLength(0);
    expect(screen.queryAllByTestId("preview-mode-split-h")).toHaveLength(0);
    expect(screen.queryAllByTestId("preview-mode-fullscreen")).toHaveLength(0);

    // 三态在唯一入口下仍全通：全屏 → 左右 → 上下 → 回全屏（含分屏态仍只有一份）
    fireEvent.click(screen.getByTestId("preview-toggle-split-h"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split-h");
    expect(screen.getAllByTestId("preview-toggle-split")).toHaveLength(1);
    fireEvent.click(screen.getByTestId("preview-toggle-split"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("split");
    fireEvent.click(screen.getByTestId("preview-toggle-fullscreen"));
    expect(screen.getByTestId("file-preview").getAttribute("data-mode")).toBe("fullscreen");
  });

  it("Task 2：页头恒有文件面板入口，点击进入列表视图（窄屏默认全屏布局）", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 页头按钮恒可见（预览未打开时也有）
    const btn = await screen.findByTestId("file-panel-button");
    expect(btn.getAttribute("aria-label")).toBe("文件面板");
    // 预览未打开：无侧栏容器
    expect(screen.queryByTestId("preview-shell")).toBeNull();
    fireEvent.click(btn);
    // 进入列表视图：侧栏容器出现（jsdom 无 matchMedia → 窄屏默认 fullscreen）
    // T1 sheet 化：data-view 三形收敛为 data-sheet + data-open（files sheet 看板态）
    const shell = await screen.findByTestId("preview-shell");
    expect(shell.getAttribute("data-sheet")).toBe("files");
    expect(shell.getAttribute("data-open")).toBe("none");
    expect(shell.getAttribute("data-mode")).toBe("fullscreen");
  });

  it("Task 2：宽屏打开面板默认 split-h（左对话右列表）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "hi" })];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (q: string) => ({ matches: q.includes("min-width"), media: q }),
    });
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    const shell = await screen.findByTestId("preview-shell");
    expect(shell.getAttribute("data-mode")).toBe("split-h");
    expect(screen.getByTestId("split-container").className).toContain("flex-row");
  });

  it("Task 2：面板挂载复用详情页已拉的提取结果，切档才重拉（scope→limit）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs" })];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 挂载时已拉一次（scope 默认 200，用于正文链接化）
    await screen.findByTestId("file-panel-button");
    await waitFor(() => {
      expect(fetchMock.mock.calls.some((c: unknown[]) => String(c[0]).includes("limit=200"))).toBe(
        true
      );
    });
    const before = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-files")
    ).length;
    // 打开面板不重拉（复用挂载结果）
    fireEvent.click(screen.getByTestId("file-panel-button"));
    await screen.findByTestId("preview-shell");
    expect(
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-files")).length
    ).toBe(before);
  }, 15000);

  it("Task 3 集成：面板 → 点文件 → 预览带返回按钮 → 返回列表（布局保持）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "hi" })];
    // 后端契约：已按 lastSeq 降序（app.rs seq 9 在前，doc.md seq 5 在后）
    routes.files = [
      fileEntry("/tmp/proj/src/app.rs", { lastSeq: 9 }),
      fileEntry("/p/doc.md", { lastSeq: 5 }),
    ];
    routes.fileContent = "fn main() {}";
    routes.fileMime = "text/rust";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 打开面板 → files sheet 看板态（窄屏全屏）
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    const shell = await screen.findByTestId("preview-shell");
    expect(shell.getAttribute("data-sheet")).toBe("files");
    expect(shell.getAttribute("data-open")).toBe("none");
    // 面板列表渲染（后端顺序）
    expect(screen.getByTestId("file-row-0-open").textContent).toContain("app.rs");
    expect(screen.getByTestId("file-row-1-open").textContent).toContain("doc.md");
    // 点文件名 → files sheet 内选中该文件（open.file）+ 返回按钮
    fireEvent.click(screen.getByTestId("file-row-0-open"));
    expect((await screen.findByTestId("preview-shell")).getAttribute("data-open")).toBe("file");
    expect((await screen.findByTestId("preview-code")).textContent).toContain("fn main() {}");
    // 从面板进入 → 有返回按钮
    const backBtn = screen.getByTestId("preview-back-list");
    expect(backBtn.getAttribute("aria-label")).toBe("返回文件列表");
    // 返回：open 收回看板态（none），布局 mode 保持
    fireEvent.click(backBtn);
    const back = screen.getByTestId("preview-shell");
    expect(back.getAttribute("data-open")).toBe("none");
    expect(back.getAttribute("data-sheet")).toBe("files");
    expect(back.getAttribute("data-mode")).toBe("fullscreen");
  }, 15000);

  it("Task 3：从消息正文链接进入预览无返回按钮（来源区分）", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    await screen.findByTestId("file-preview");
    expect(screen.queryByTestId("preview-back-list")).toBeNull();
  });

  it("ZCode 式面板按钮：开启态高亮 + tooltip，再点收回面板", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "hi" })];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const btn = await screen.findByTestId("file-panel-button");
    // 未开启：无高亮（aria-pressed=false），tooltip 提示打开
    expect(btn.getAttribute("aria-pressed")).toBe("false");
    expect(btn.getAttribute("title")).toBe("打开文件面板");
    fireEvent.click(btn);
    // 开启：高亮（aria-pressed=true）+ tooltip 变「收起面板」
    expect(await screen.findByTestId("preview-shell"));
    expect(btn.getAttribute("aria-pressed")).toBe("true");
    expect(btn.getAttribute("title")).toBe("收起文件面板");
    // 再点：收回面板（回正文视图）
    fireEvent.click(btn);
    expect(screen.queryByTestId("preview-shell")).toBeNull();
    expect(btn.getAttribute("aria-pressed")).toBe("false");
  });

  it("字号档位（2026-09-16 用户裁决）：面板按钮旁选择，50/75/100/125 四档，主对话窗口生效", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "正文" })];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("正文");
    const sel = (await screen.findByTestId("font-scale-select")) as HTMLSelectElement;
    // 四档可选，默认 100
    expect([...sel.options].map((o) => o.value)).toEqual(["0.5", "0.75", "1", "1.25"]);
    expect(sel.value).toBe("1");
    // 切到 125% → 消息区容器带档位类（字号经 CSS 变量缩放）
    fireEvent.change(sel, { target: { value: "1.25" } });
    expect(screen.getByTestId("message-area").getAttribute("data-font-scale")).toBe("1.25");
    // 切到 50%
    fireEvent.change(sel, { target: { value: "0.5" } });
    expect(screen.getByTestId("message-area").getAttribute("data-font-scale")).toBe("0.5");
  });

  it("进入详情默认滚到底部（最新消息）；加载更早按钮在消息列表最上方", async () => {
    installFetch();
    routes.messages = Array.from({ length: 200 }, (_, i) =>
      msg({ seq: i, kind: "user", content: `m${i}` })
    );
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("m199");

    // 进入即滚到底：jsdom 无布局引擎（scrollHeight 恒 0），注入有限值后
    // 触发一次手动刷新（等价于「数据到达」）让滚动 effect 有可观测量
    const area = screen.getByTestId("message-area");
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(() => expect(area.scrollTop).toBe(5000));

    // 「加载更早消息」应排在消息列表**之前**（用户往上翻到头才点它）
    const list = area.querySelector("ul");
    const loadMore = screen.getByTestId("load-more");
    expect(list).toBeTruthy();
    expect(list!.compareDocumentPosition(loadMore) & Node.DOCUMENT_POSITION_PRECEDING).toBeTruthy();
  }, 15000);

  it("点加载更早后保持阅读位置（顶部插入量补偿，视线不跳）", async () => {
    installFetch();
    routes.messages = Array.from({ length: 200 }, (_, i) =>
      msg({ seq: i, kind: "user", content: `m${i}` })
    );
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("m199");
    // **排空挂载那一次数据落地的对齐 effect**（存量 flake 根因，2026-10-07 探针实证）。
    // `findByText` 只保证 DOM 已提交，**不保证被动 effect 已冲刷**：React 的 passive
    // effect 经调度器异步冲刷，负载下可拖到 `fireEvent.click` 的 act 作用域里才跑。
    // 而 click 的派发发生在 act 冲刷**之前**——于是「挂载落地的对齐 effect」会看到
    // click 刚装好的锚，用 `inserted = 5000 - 5000 = 0` 把它消费掉（补偿退化成空操作），
    // 随后真正的 limit=400 落地走 `pollFollow === true` 分支
    // `el.scrollTop = el.scrollHeight = 8000` → 断言偶发红（实测 12 次重压跑红 1 次、
    // 3 次全量套件跑红 1 次，探针逐字轨迹：click 后紧跟 align-PENDING{inserted:0,
    // result:1000}，再 align-BOTTOM{scrollHeight:8000}）。
    // 显式 act 冲刷把这一步提前到几何量注入**之前**，竞态窗口随之消失（不是加等待）。
    await act(async () => {});
    const area = screen.getByTestId("message-area");
    // 模拟：当前滚动位置 1000，内容总高 5000（此刻挂载对齐已跑完，不会再覆写）
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    area.scrollTop = 1000;
    // 点「加载更早」→ 记录锚点；limit=400 重拉在途。先挂起响应、注入新内容总高
    // （8000）后放行——保证补偿算的是「新高度 - 旧高度 = 3000」这一真实插入量，
    // 而不是被别的落地抢先把锚消费掉。
    const inner = fetchMock;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const gated = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/session-messages") && url.includes("limit=400")) {
        await gate; // 挂起重拉响应，等几何量注入
      }
      return inner(input);
    });
    vi.stubGlobal("fetch", gated);
    fireEvent.click(screen.getByTestId("load-more"));
    await waitFor(() => {
      expect(gated.mock.calls.some((c: unknown[]) => String(c[0]).includes("limit=400"))).toBe(
        true
      );
    });
    Object.defineProperty(area, "scrollHeight", { value: 8000, configurable: true });
    release(); // 放行 limit=400 响应 → 数据落地 → 补偿对齐
    await waitFor(() => {
      // 补偿：1000 + (8000 - 5000) = 4000（视线停在原内容处）
      expect(area.scrollTop).toBe(4000);
    });
  }, 15000);

  it("书签集成：打标签 → 消息旁角标 → 点色点跳转（scrollIntoView 落在目标）", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "第一条指令" }),
      msg({ seq: 1, kind: "assistant", content: "第一段回复" }),
      msg({ seq: 2, kind: "user", content: "待会回来看这条" }),
      msg({ seq: 3, kind: "assistant", content: "最后一段" }),
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("最后一段");

    const area = screen.getByTestId("message-area");
    // jsdom 无布局：注入几何量——容器顶边 100，seq 2 的底边 200（= 视口首条）
    area.getBoundingClientRect = () =>
      ({ top: 100, bottom: 700, left: 0, right: 400, width: 400, height: 600 }) as DOMRect;
    const liOf = (seq: number) => screen.getByTestId(`msg-${seq}`);
    // 视口顶边 = 100：seq 0/1 已完全滚出上方（bottom ≤ 100），seq 2 是首条可见
    liOf(0).getBoundingClientRect = () =>
      ({ top: -60, bottom: -10, left: 0, right: 400, width: 400, height: 50 }) as DOMRect;
    liOf(1).getBoundingClientRect = () =>
      ({ top: 20, bottom: 90, left: 0, right: 400, width: 400, height: 70 }) as DOMRect;
    liOf(2).getBoundingClientRect = () =>
      ({ top: 110, bottom: 260, left: 0, right: 400, width: 400, height: 150 }) as DOMRect;
    liOf(3).getBoundingClientRect = () =>
      ({ top: 270, bottom: 400, left: 0, right: 400, width: 400, height: 130 }) as DOMRect;

    // 打标签：点 + → 选第一个颜色 → 落在视口首条（seq 2「待会回来看这条」）
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[0]}`));
    // 色点出现 + 目标消息旁有角标
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`)).toBeTruthy();
    expect(screen.getByTestId("msg-bookmark-2")).toBeTruthy();
    // 角标只落在命中那条
    expect(screen.queryByTestId("msg-bookmark-0")).toBeNull();

    // 点色点跳转：scrollIntoView 落在 seq 2 的 li 上
    const target = liOf(2);
    const scrollSpy = vi.fn();
    target.scrollIntoView = scrollSpy;
    fireEvent.click(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[0]}`));
    expect(scrollSpy).toHaveBeenCalled();
    expect(scrollSpy.mock.calls[0][0]).toMatchObject({ block: "start" });
  }, 15000);

  it("书签集成：删除单条与清空全部", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "甲" }),
      msg({ seq: 1, kind: "user", content: "乙" }),
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("乙");
    const area = screen.getByTestId("message-area");
    area.getBoundingClientRect = () =>
      ({ top: 0, bottom: 600, left: 0, right: 400, width: 400, height: 600 }) as DOMRect;
    screen.getByTestId("msg-0").getBoundingClientRect = () =>
      ({ top: 0, bottom: 50, left: 0, right: 400, width: 400, height: 50 }) as DOMRect;
    screen.getByTestId("msg-1").getBoundingClientRect = () =>
      ({ top: 50, bottom: 100, left: 0, right: 400, width: 400, height: 100 }) as DOMRect;

    // 打两个不同颜色的标签
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[0]}`));
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[1]}`));
    expect(screen.getAllByTestId(/^bookmark-dot-/)).toHaveLength(2);

    // 管理态删单条
    fireEvent.click(screen.getByTestId("bookmark-manage"));
    fireEvent.click(screen.getByTestId(`bookmark-remove-${BOOKMARK_COLORS[0]}`));
    expect(screen.getAllByTestId(/^bookmark-(dot|remove)-/)).toHaveLength(1);
    // 清空全部
    fireEvent.click(screen.getByTestId("bookmark-clear-all"));
    expect(screen.queryAllByTestId(/^bookmark-(dot|remove)-/)).toHaveLength(0);
    // 角标也一并消失
    expect(screen.queryByTestId("msg-bookmark-0")).toBeNull();
  }, 15000);

  it("书签集成：返回看板再进同一会话，书签仍在（store 跨卸载恢复）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "记住我" })];
    const { unmount } = render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("记住我");
    const area = screen.getByTestId("message-area");
    area.getBoundingClientRect = () =>
      ({ top: 0, bottom: 600, left: 0, right: 400, width: 400, height: 600 }) as DOMRect;
    screen.getByTestId("msg-0").getBoundingClientRect = () =>
      ({ top: 0, bottom: 50, left: 0, right: 400, width: 400, height: 50 }) as DOMRect;
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[3]}`));
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[3]}`)).toBeTruthy();

    // 「返回看板」= 卸载本组件（App.tsx 条件挂载）
    unmount();
    // 再进同一会话（同 session.id）→ 书签从模块级 store 恢复
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("记住我");
    expect(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[3]}`)).toBeTruthy();
    expect(screen.getByTestId("msg-bookmark-0")).toBeTruthy();
  }, 15000);

  it("书签集成：刷新页面（同 bootId 重挂）书签从 localStorage 恢复", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "记住我" })];
    // 第一次「打开页面」：打书签（写入 localStorage 镜像）
    const { unmount } = render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("记住我");
    const area = screen.getByTestId("message-area");
    area.getBoundingClientRect = () =>
      ({ top: 0, bottom: 600, left: 0, right: 400, width: 400, height: 600 }) as DOMRect;
    screen.getByTestId("msg-0").getBoundingClientRect = () =>
      ({ top: 0, bottom: 50, left: 0, right: 400, width: 400, height: 50 }) as DOMRect;
    fireEvent.click(screen.getByTestId("bookmark-add"));
    fireEvent.click(screen.getByTestId(`bookmark-color-${BOOKMARK_COLORS[3]}`));
    expect(window.localStorage.getItem("mam-bookmarks")).toContain(BOOKMARK_COLORS[3]);
    unmount();

    // 「刷新页面」：重挂（bootId 不变，内存单例从 localStorage 种回）
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("记住我");
    expect(await screen.findByTestId(`bookmark-dot-${BOOKMARK_COLORS[3]}`)).toBeTruthy();
    expect(screen.getByTestId("msg-bookmark-0")).toBeTruthy();
  }, 15000);

  it("未知路径不出链接：files 为空时正文原样", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "见 /tmp/other/x.rs" })];
    routes.files = [];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText(/\/tmp\/other\/x\.rs/);
    expect(screen.queryByTestId("file-link")).toBeNull();
  });
});

describe("SessionDetail：错误态与手动刷新", () => {
  it("404 →「无法读取该会话内容」+ 重试可恢复", async () => {
    installFetch();
    routes.messagesStatus = 404;
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect((await screen.findByTestId("detail-error")).textContent).toContain("无法读取该会话内容");
    routes.messagesStatus = undefined;
    routes.messages = [msg({ seq: 0, kind: "user", content: "恢复后可见" })];
    fireEvent.click(screen.getByTestId("detail-retry"));
    expect(await screen.findByText("恢复后可见")).toBeTruthy();
  });

  it("网络异常 → 加载失败 + 重试；刷新按钮重拉不自动轮询", async () => {
    installFetch();
    routes.messagesNetworkFail = true;
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    expect((await screen.findByTestId("detail-error")).textContent).toContain("加载失败");
    routes.messagesNetworkFail = false;
    routes.messages = [msg({ seq: 0, kind: "user", content: "刷新结果" })];
    fireEvent.click(screen.getByTestId("detail-retry"));
    expect(await screen.findByText("刷新结果")).toBeTruthy();
    // 手动刷新（refresh 按钮）同样走重拉；timeout 加固同上（并行资源竞争双保险）
    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(
      () => {
        expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(4);
      },
      { timeout: 3000 }
    );
  });
});

describe("书签跨加载窗口跳转（M5 P3-c）", () => {
  /** 250 条合成消息（seq = 下标）；书签目标 seq=10 在首批 200 条窗口之外 */
  function bigMessages(): SessionMessage[] {
    return Array.from({ length: 250 }, (_, i) =>
      msg({
        seq: i,
        kind: i === 10 ? "user" : i % 2 ? "assistant" : "user",
        content: i === 10 ? "书签目标：这条在很早的分页里" : `填充消息 ${i}`,
        ts: 1000 + i,
      })
    );
  }

  function installLimitAwareFetch(all: SessionMessage[]) {
    fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/session-messages")) {
        const limit = Number(new URL(url, "http://x").searchParams.get("limit") ?? 200);
        return new Response(JSON.stringify({ messages: all.slice(-limit), truncated: true }), {
          status: 200,
        });
      }
      if (url.includes("/host")) {
        return new Response(
          JSON.stringify({
            host: { name: "n", platform: "windows", version: "0", bootId: "boot-test" },
            enabledTools: [],
            installedTools: ["claude", "codex", "kimi", "opencode"],
          }),
          { status: 200 }
        );
      }
      if (url.includes("/session-files")) {
        return new Response(JSON.stringify({ files: [], truncated: false }), { status: 200 });
      }
      if (url.includes("/session-subagents")) {
        // 观察台 T5：与 installFetch 同口径读 routes.subagents（缺省 []）
        return new Response(JSON.stringify({ subagents: routes.subagents ?? [] }), { status: 200 });
      }
      if (url.includes("/session-subagent-messages")) {
        // 观察台 T6：详情载荷桩（同 installFetch，缺省空载荷）
        return new Response(
          JSON.stringify({ messages: [], truncated: false, supported: true }),
          { status: 200 }
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);
  }

  function seedBookmarkFor(m: SessionMessage) {
    window.localStorage.setItem(
      "mam-bookmarks",
      JSON.stringify({
        bootId: "boot-test",
        sessions: {
          "sess-1": [
            {
              color: BOOKMARK_COLORS[1],
              seq: m.seq,
              anchor: messageAnchor(m),
              preview: m.content.slice(0, 40),
            },
          ],
        },
      })
    );
  }

  it("书签目标在首批窗口之外 → 点击自动逐级加载更早直至命中滚动", async () => {
    const all = bigMessages();
    const target = all[10];
    seedBookmarkFor(target);
    installLimitAwareFetch(all);

    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 首批窗口（200 条）：目标 seq 10 不在窗口
    await screen.findByText("填充消息 249");

    const scrollSpy = vi.fn();
    Element.prototype.scrollIntoView = scrollSpy;
    fireEvent.click(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[1]}`));

    // 自动扩窗：limit 200→400 重拉，目标（seq 10）出现并被滚动定位。
    // timeout 10s：与下例「扩窗到顶」同因——扩窗重拉在整库并行负载下可超 waitFor
    // 默认 1s（Task 8 门前实测整库跑两次假失败两次，单文件连跑 5/5 绿）；
    // 只放宽等待上限，断言语义不变
    await waitFor(
      () => {
        expect(fetchMock).toHaveBeenCalledWith(expect.stringContaining("limit=400"));
      },
      { timeout: 10_000 }
    );
    await screen.findByTestId("msg-10");
    await waitFor(() => expect(scrollSpy).toHaveBeenCalled(), { timeout: 10_000 });
    expect(scrollSpy.mock.calls[0][0]).toMatchObject({ block: "start" });
    // 定位成功：加载/miss 横幅均不在场
    expect(screen.queryByTestId("bookmark-jump-miss")).toBeNull();
    expect(screen.queryByTestId("bookmark-jump-loading")).toBeNull();
  }, 15000);

  it("扩窗到顶（MAX_LIMIT）仍未命中 → miss 横幅", async () => {
    const all = bigMessages();
    seedBookmarkFor({
      seq: 999,
      kind: "user",
      content: "这条书签指向不存在的消息",
      ts: 1234,
    });
    installLimitAwareFetch(all);

    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await screen.findByText("填充消息 249");

    const scrollSpy = vi.fn();
    Element.prototype.scrollIntoView = scrollSpy;
    fireEvent.click(screen.getByTestId(`bookmark-dot-${BOOKMARK_COLORS[1]}`));

    // 逐级扩到 MAX_LIMIT（1000）仍未命中 → miss 横幅，且未发生任何滚动。
    // timeout 10s：四级扩窗（200→…→1000）在 CI 慢机上实测 >4s（本地快机 <1s），
    // 3s 曾在 CI 抖动失败（run 35314166316）
    await waitFor(() => expect(screen.getByTestId("bookmark-jump-miss")).toBeTruthy(), {
      timeout: 10_000,
    });
    expect(scrollSpy).not.toHaveBeenCalled();
  }, 15000);
});

// ==== F6：详情页 10s 轮询（假计时器锁节奏与可见性语义）====
describe("SessionDetail：详情页 10s 轮询（F6）", () => {
  /** 只数 /session-messages 调用（/host、/session-files 的拉取不计入节奏断言） */
  function messagesCalls(): number {
    return fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-messages"))
      .length;
  }

  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("节奏：挂载首拉 1 次，+10s 轮询第 2 次，再 +10s 第 3 次（首拉不双触发）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await act(async () => {}); // 挂载首拉落地（轮询 interval 首拍在 +10s，不立即触发）
    expect(messagesCalls()).toBe(1);
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(2);
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(3);
  });

  it("hidden 暂停：推进计时器不触发；恢复 visible 立即补刷一次再续 10s 节奏", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    const visSpy = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await act(async () => {});
    expect(messagesCalls()).toBe(1);
    // 切后台（hidden + visibilitychange）：暂停轮询——推进 30s 零新增拉取
    visSpy.mockReturnValue("hidden");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await act(async () => {
      vi.advanceTimersByTime(30_000);
    });
    expect(messagesCalls()).toBe(1);
    // 切回前台：立即补刷一次（追回隐藏期间错过的更新）
    visSpy.mockReturnValue("visible");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(messagesCalls()).toBe(2);
    // 补刷后重启节奏：+10s 下一拍
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(3);
    visSpy.mockRestore();
  });

  // 收尾批 P2：F6 卸载清理回归钉（评审修复批遗留的显式验证）——unmount 必须
  // 清 interval + 移除 visibilitychange 监听，长驻页面来回进出不泄漏计时器/监听。
  // 监听移除按 spyOn add/removeEventListener 捕获引用比对（同一函数引用注册且移除）；
  // interval 清理由 getTimerCount 直证（泄漏则卸载后仍挂 1 个待触发拍），并按
  // 「clearAllTimers 后再推进不再触发拉取」行为口径兜底断言
  it("unmount 清理：interval 已清 + visibilitychange 监听已移除", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    const addSpy = vi.spyOn(document, "addEventListener");
    const removeSpy = vi.spyOn(document, "removeEventListener");
    try {
      const { unmount } = render(<SessionDetail session={makeSession()} onBack={() => {}} />);
      await act(async () => {}); // 挂载首拉落地
      expect(messagesCalls()).toBe(1);
      await act(async () => {
        vi.advanceTimersByTime(10_000);
      });
      expect(messagesCalls()).toBe(2); // 轮询确实在跑（前提自证，断言不空转）
      unmount();
      // visibilitychange 监听已移除：注册与移除是同一函数引用（组件只挂这一个
      // document 级监听——比对引用即精确钉住 F6 effect 的清理半边）
      const visListener = addSpy.mock.calls.find((c) => c[0] === "visibilitychange")?.[1];
      expect(visListener).toBeDefined();
      expect(removeSpy).toHaveBeenCalledWith("visibilitychange", visListener);
      // interval 已清：卸载后零待触发计时器（泄漏则此处为 1）
      expect(vi.getTimerCount()).toBe(0);
      // 行为口径兜底：清掉全部计时器再推进，不再触发任何拉取
      vi.clearAllTimers();
      await act(async () => {
        vi.advanceTimersByTime(60_000);
      });
      expect(messagesCalls()).toBe(2);
    } finally {
      addSpy.mockRestore();
      removeSpy.mockRestore();
    }
  });
});

// ==== P2-B（评审修复批）：轮询滚动跟随条件化 ====
// 轮询刷新数据落地时，仅在刷新前采样为「贴底」（距底 <120px）才跟随落底；
// 上翻阅读历史不被每 10s 拽回底部。假计时器 + 滚动容器几何量 mock
//（jsdom 无布局引擎：scrollHeight/clientHeight 逐实例注入，scrollTop 可赋可读）。
describe("SessionDetail：轮询滚动跟随条件化（P2-B）", () => {
  /** 只数 /session-messages 调用（证明刷新确实发生，断言不空转） */
  function messagesCalls(): number {
    return fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-messages"))
      .length;
  }

  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    // 假计时器必须还原（与文件内真实计时器用例共存，e39c3d9 自审提示）
    vi.useRealTimers();
  });

  /** 注入消息滚动容器几何量（元素挂载后逐实例 defineProperty，重渲染不丢） */
  function installGeometry(area: HTMLElement, geo: { scrollHeight: number; clientHeight: number }) {
    Object.defineProperty(area, "scrollHeight", { value: geo.scrollHeight, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: geo.clientHeight, configurable: true });
  }

  it("poll_keeps_scroll_when_reading_history：上翻阅读（距底 ≥120px）两拍轮询刷新不拽回底部", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await act(async () => {}); // 首拉落地（首拉无条件落底；此刻 scrollHeight=0 → scrollTop=0）
    const area = screen.getByTestId("message-area");
    // 距底 = 2000 - 0 - 500 = 1500 ≥ 120 → 非贴底（用户上翻阅读历史）
    installGeometry(area, { scrollHeight: 2000, clientHeight: 500 });
    area.scrollTop = 0;
    // 第一拍轮询：新消息落地（mock 按 routes 现取 → 新数组触发对齐 effect）
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "首拉" }),
      msg({ seq: 1, kind: "assistant", content: "轮询新消息一" }),
    ];
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(2); // 刷新确实发生
    expect(area.scrollTop).toBe(0); // 但滚动位置不动（距底 ≥120px 不跟随）
    // 第二拍轮询：仍不跟随
    routes.messages = [
      ...routes.messages,
      msg({ seq: 2, kind: "assistant", content: "轮询新消息二" }),
    ];
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(3);
    expect(area.scrollTop).toBe(0);
  });

  it("poll_follows_when_near_bottom：贴底（距底 <120px）轮询刷新到新消息跟随落底", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await act(async () => {});
    const area = screen.getByTestId("message-area");
    // 距底 = 2000 - 1400 - 500 = 100 < 120 → 贴底
    installGeometry(area, { scrollHeight: 2000, clientHeight: 500 });
    area.scrollTop = 1400;
    // 轮询拍新消息落地 → 跟随落底：scrollTop = scrollHeight = 2000
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "首拉" }),
      msg({ seq: 1, kind: "assistant", content: "贴底时的新消息" }),
    ];
    await act(async () => {
      vi.advanceTimersByTime(10_000);
    });
    expect(messagesCalls()).toBe(2);
    expect(area.scrollTop).toBe(2000);
  });

  it("first load 仍无条件落底：首拉对齐不依赖贴底采样（既有行为的显式回归锁）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "user", content: "首拉" })];
    // 挂载前在 Element 原型注入 scrollHeight（元素尚不存在，无法逐实例注入；
    // jsdom 将 scrollHeight 定义为 Element.prototype 自有 getter，jsdom 探明）：
    // 首拉对齐量即可观测量 = scrollHeight
    const scrollHeightSpy = vi
      .spyOn(Element.prototype, "scrollHeight", "get")
      .mockReturnValue(2000);
    try {
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);
      await act(async () => {}); // 首拉落地
      const area = screen.getByTestId("message-area");
      // 首拉落底：scrollTop = scrollHeight = 2000（不采样、不受 120px 阈值约束）
      expect(area.scrollTop).toBe(2000);
    } finally {
      scrollHeightSpy.mockRestore();
    }
  });
});

// ==== 计划正文渲染（2026-09-20 用户实测：ExitPlanMode 整篇计划在详情页是 \n 字面量汤）====
// 根因：后端把工具输入原封透传为 JSON 串（字符串值换行全为 \n 转义），前端 <pre> 原样上屏。
// 修法：toolArgs 解析出非空字符串 plan 字段 → 该正文走 markdown 渲染；不看 toolName——
// zcode 的 ExitPlanMode 输入同为 {plan} 但 兔维斯 记录的是显示 title，按名字匹配会漏。
// 其他工具参数维持原样（用户裁决：不做通用美化）。
describe("SessionDetail：计划正文渲染（2026-09-20）", () => {
  function expandToolCall(seq: number) {
    fireEvent.click(screen.getByTestId(`msg-${seq}-toggle`));
  }

  it("ExitPlanMode：plan 字段按 markdown 渲染（标题/列表正常排版，配「计划」标签）", async () => {
    installFetch();
    routes.messages = [
      msg({
        seq: 4,
        kind: "tool-call",
        content: "调用 ExitPlanMode",
        toolName: "ExitPlanMode",
        toolArgs: JSON.stringify({ plan: "# 计划标题\n\n- 第一步\n- 第二步" }),
      }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("调用 ExitPlanMode");
    expandToolCall(4);
    // markdown 已渲染：# 标题 → H1，列表项 → LI（对齐既有 markdown 断言的 tagName 手法）
    expect(screen.getByText("计划标题").tagName).toBe("H1");
    expect(screen.getByText("第一步").tagName).toBe("LI");
    // 「计划」标签存在（标识这是计划正文）
    expect(screen.getByText("计划")).toBeTruthy();
  });

  it("zcode 同形覆盖：toolName 是显示 title 也能命中 plan 字段（形态识别回归锁）", async () => {
    installFetch();
    routes.messages = [
      msg({
        seq: 5,
        kind: "tool-call",
        content: "调用 制定执行计划",
        toolName: "制定执行计划",
        toolArgs: JSON.stringify({ plan: "## 方案\n\n正文段落" }),
      }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("调用 制定执行计划");
    expandToolCall(5);
    expect(screen.getByText("方案").tagName).toBe("H2");
  });

  it("无 plan 字段：维持原样渲染（既有 command 断言不改）", async () => {
    installFetch();
    routes.messages = [
      msg({
        seq: 6,
        kind: "tool-call",
        content: "调用 Bash",
        toolName: "Bash",
        toolArgs: '{"command":"ls"}',
      }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("调用 Bash");
    expandToolCall(6);
    expect(screen.getByTestId("tool-args-6").textContent).toBe('{"command":"ls"}');
    expect(screen.queryByText("计划")).toBeNull();
  });

  it("坏 JSON / plan 非字符串 / 空串：均原样回落，不抛错", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 7, kind: "tool-call", content: "t7", toolName: "T7", toolArgs: '{"plan":' }),
      msg({ seq: 8, kind: "tool-call", content: "t8", toolName: "T8", toolArgs: '{"plan":123}' }),
      msg({ seq: 9, kind: "tool-call", content: "t9", toolName: "T9", toolArgs: '{"plan":""}' }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("调用 T7");
    expandToolCall(7);
    expect(screen.getByTestId("tool-args-7").textContent).toBe('{"plan":');
    expandToolCall(8);
    expect(screen.getByTestId("tool-args-8").textContent).toBe('{"plan":123}');
    expandToolCall(9);
    expect(screen.getByTestId("tool-args-9").textContent).toBe('{"plan":""}');
  });
});

// ==== 跳到最新（2026-09-20）：距底超阈值出现浮动按钮，点击瞬时落底 ====
describe("SessionDetail：跳到最新（2026-09-20）", () => {
  /** jsdom 无布局引擎：注入滚动几何量（既有 :841 手法），distance = 距底像素 */
  function stubGeometry(area: HTMLElement, distance: number) {
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: 1000, configurable: true });
    area.scrollTop = 5000 - 1000 - distance;
  }

  function renderWithMessage() {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "一段回复" })];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    return waitFor(() => screen.getByTestId("message-area"));
  }

  it("距底超阈值（>240px）出现按钮；滚回贴底消失", async () => {
    const area = await renderWithMessage();
    stubGeometry(area, 3000);
    fireEvent.scroll(area);
    expect(screen.getByTestId("jump-to-bottom")).toBeTruthy();
    stubGeometry(area, 0);
    fireEvent.scroll(area);
    expect(screen.queryByTestId("jump-to-bottom")).toBeNull();
  });

  it("点击按钮：scrollTop 瞬时落到 scrollHeight，按钮消失（正文分支）", async () => {
    const area = await renderWithMessage();
    stubGeometry(area, 3000);
    fireEvent.scroll(area);
    fireEvent.click(screen.getByTestId("jump-to-bottom"));
    expect(area.scrollTop).toBe(5000);
    expect(screen.queryByTestId("jump-to-bottom")).toBeNull();
  });

  it("距顶超阈值（>240px）出现到顶钮；滚回顶部消失", async () => {
    const area = await renderWithMessage();
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: 1000, configurable: true });
    area.scrollTop = 2400; // 距顶 = scrollTop > 240
    fireEvent.scroll(area);
    expect(screen.getByTestId("jump-to-top")).toBeTruthy();
    area.scrollTop = 0;
    fireEvent.scroll(area);
    expect(screen.queryByTestId("jump-to-top")).toBeNull();
  });

  it("点击到顶钮：scrollTop 瞬时落 0，按钮消失", async () => {
    const area = await renderWithMessage();
    Object.defineProperty(area, "scrollHeight", { value: 5000, configurable: true });
    Object.defineProperty(area, "clientHeight", { value: 1000, configurable: true });
    area.scrollTop = 2400;
    fireEvent.scroll(area);
    fireEvent.click(screen.getByTestId("jump-to-top"));
    expect(area.scrollTop).toBe(0);
    expect(screen.queryByTestId("jump-to-top")).toBeNull();
  });

  it("分屏（split）分支同样可用", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "assistant", content: "改了 /tmp/proj/src/app.rs 请看" }),
    ];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-link"));
    fireEvent.click(await screen.findByTestId("preview-toggle-split"));
    const area = await screen.findByTestId("message-area");
    stubGeometry(area, 3000);
    fireEvent.scroll(area);
    fireEvent.click(screen.getByTestId("jump-to-bottom"));
    expect(area.scrollTop).toBe(5000);
  });
});

// ==== 过程一键折叠（2026-09-20）：书签栏右侧开关，运行态/总结态都可用 ====
describe("SessionDetail：过程一键折叠（2026-09-20）", () => {
  function runningMessages() {
    return [
      msg({ seq: 0, kind: "user", content: "查一下" }),
      msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
      msg({
        seq: 2,
        kind: "tool-call",
        content: "调用 Bash",
        toolName: "Bash",
        toolArgs: '{"command":"ls"}',
      }),
      msg({ seq: 3, kind: "assistant", content: "结论" }),
    ];
  }

  it("运行态（非总结模式）折叠开关出现：默认全折叠 → 一键全展 → 一键全收", async () => {
    installFetch();
    routes.messages = runningMessages();
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("结论");
    const toggle = screen.getByTestId("process-collapse-toggle");
    // 运行态默认 thinking/tool-call 折叠 → allCollapsed=true → 动作=全部展开
    expect(toggle.getAttribute("aria-label")).toBe("展开全部过程");
    expect(screen.queryByText("内部思考内容")).toBeNull();
    fireEvent.click(toggle);
    expect(screen.getByText("内部思考内容")).toBeTruthy();
    expect(screen.getByTestId("process-collapse-toggle").getAttribute("aria-label")).toBe(
      "折叠全部过程"
    );
    // 再点全收
    fireEvent.click(screen.getByTestId("process-collapse-toggle"));
    expect(screen.queryByText("内部思考内容")).toBeNull();
  });

  it("运行态含 tool-result（wire 默认展开）：「过程折叠」真收起（2026-10-04 no-op 修复）", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "查一下" }),
      msg({ seq: 1, kind: "tool-result", content: "工具输出正文" }),
      msg({ seq: 2, kind: "assistant", content: "结论" }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("结论");
    const toggle = () => screen.getByTestId("process-collapse-toggle");
    // 默认 tool-result 展开（wire collapsed=false）→ 按钮显示「过程折叠」；
    // 旧实现此处点击 = 清覆盖表回落默认 = no-op（根因），修复后必须真收起
    expect(screen.getByText("工具输出正文")).toBeTruthy();
    expect(toggle().getAttribute("aria-label")).toBe("折叠全部过程");
    fireEvent.click(toggle());
    expect(screen.queryByText("工具输出正文")).toBeNull();
    expect(toggle().getAttribute("aria-label")).toBe("展开全部过程");
    // 单条手动展开（override）→ 计数变 → 按钮回到「过程折叠」；再点全收仍生效
    fireEvent.click(screen.getByTestId("msg-1-toggle"));
    expect(screen.getByText("工具输出正文")).toBeTruthy();
    expect(toggle().getAttribute("aria-label")).toBe("折叠全部过程");
    fireEvent.click(toggle());
    expect(screen.queryByText("工具输出正文")).toBeNull();
    // 「过程展开」恢复：全部强制展开（含 wire 默认折叠的 thinking/tool-call）
    fireEvent.click(toggle());
    expect(screen.getByText("工具输出正文")).toBeTruthy();
  });

  it("无过程消息（纯 user/assistant）：折叠开关不渲染", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "你好" }),
      msg({ seq: 1, kind: "assistant", content: "你好呀" }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("你好呀");
    expect(screen.queryByTestId("process-collapse-toggle")).toBeNull();
  });
});

// ==== 计划一等卡片（T1 手工验收修复批）：ExitPlanMode 形态在后端升格 kind="plan" ====
// content = 计划 markdown 本体，前端常驻渲染：无折叠头、豁免总结模式折叠、
// 不计入总结横幅「已折叠 N 条」计数（用户裁决：不做消息合并/重复折叠，本件不碰）
describe("SessionDetail：计划一等卡片（T1）", () => {
  it("运行态：plan 消息渲染常驻计划卡片（markdown 直出，无折叠头）", async () => {
    installFetch();
    routes.messages = [
      msg({
        seq: 0,
        kind: "plan",
        content: "# 大计划\n\n- 步骤甲\n- 步骤乙",
        toolName: "ExitPlanMode",
      }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    // markdown 结构化渲染（content 已是计划本体，无需展开动作）
    expect((await screen.findByText("大计划")).tagName).toBe("H1");
    expect(screen.getByText("步骤甲").tagName).toBe("LI");
    // 「计划」标签 + 常驻卡片锚点；无折叠头（不可折叠）
    expect(screen.getByText("计划")).toBeTruthy();
    expect(screen.getByTestId("plan-0")).toBeTruthy();
    expect(screen.queryByTestId("msg-0-toggle")).toBeNull();
    expect(screen.getByTestId("msg-0").getAttribute("data-kind")).toBe("plan");
  });

  // 丁T5（问题 11 的真实断点）：计划卡此前**漏挂** `.md-body` 排版层——
  // Tailwind v4 preflight 把 h1-h6 的字号/字重与 ul/ol 的 list-style 全重置，
  // 故计划正文里的 `###` 小标题与 `-` 列表在这张卡上被拍平成正文
  // （普通消息卡与「工具参数升格」卡都挂了 `.md-body`，唯独计划卡漏了）。
  // 真机夹具：本机 rollout 2026-09-21T17-38-17 行 115（4 个 `###` + 14 行列表）。
  it("计划卡必须挂 .md-body 排版层（preflight 拍平 ### 标题/列表的回归锁）", async () => {
    installFetch();
    // 真机计划原文缩录（结构不变：## 一级 + ### 二级 + 嵌套列表）
    const realPlan =
      "## 修改《末班车》情感救赎版\n\n### 概要\n在现有文件基础上改写为约 500 字的短篇版本。\n\n" +
      "### 修改方案\n- 新建文件：`悬疑小说-末班车-情感救赎版-500字.md`，不覆盖原稿。\n- 开头直接进入场景。\n" +
      "  - 嵌套项一\n  - 嵌套项二\n\n### 验证方式\n- 使用 UTF-8 读取新文件。\n";
    routes.messages = [msg({ seq: 0, kind: "plan", content: realPlan, toolName: "codex" })];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    const card = (await screen.findByTestId("plan-0")) as HTMLElement;
    // **判据**：卡片内存在挂 `.md-body` 的容器（排版层生效的锚点）——
    // 只断言「渲染出了 H2/H3/LI」不足以锁住本 bug（ReactMarkdown 一直都能解析出来，
    // 被 preflight 拍平的是**样式**；`.md-body` 类才是样式的载体）
    const mdBody = card.querySelector(".md-body");
    expect(mdBody).not.toBeNull();
    // 结构也在（markdown 解析正常）：H2 / H3 / 列表项
    expect(screen.getByText("修改《末班车》情感救赎版").tagName).toBe("H2");
    expect(screen.getByText("概要").tagName).toBe("H3");
    expect(screen.getByText("验证方式").tagName).toBe("H3");
    // 列表项文本被行内 `<code>` 切分（`悬疑小说-…md` 是 code 元素），故按 li 元素断言
    const items = mdBody!.querySelectorAll("li");
    expect(items.length).toBeGreaterThanOrEqual(4);
    expect(Array.from(items).some((li) => li.textContent?.includes("不覆盖原稿"))).toBe(true);
    // 嵌套列表（真机原文的 `  - 嵌套项`）必须在 `.md-body` 内（排版层覆盖到嵌套层）
    expect(mdBody!.contains(screen.getByText("嵌套项一"))).toBe(true);
  });

  // 丁T5 复评 F6-1：问题 11 的另一半——`case "tool-call"` 的**工具参数升格支**
  // （claude 的 ExitPlanMode 走这里）同样漏挂 `.md-body`。上面那条锁只覆盖
  // `case "plan"`，对本支**零区分力**（复评实测：删掉本支的 `.md-body` → 上面仍绿）。
  // 夹具：ExitPlanMode 的 toolArgs 里 `plan` 字段是整篇 markdown（真实形态，见
  // `extractPlanBody`），含 `###` 二级标题与 `-` 列表。
  it("工具参数升格支（tool-call 的 plan 字段）也必须挂 .md-body 排版层", async () => {
    installFetch();
    // 真实形态：toolArgs 是 JSON 串，顶层 plan 字段 = 计划 markdown 本体
    routes.messages = [
      msg({
        seq: 0,
        kind: "tool-call",
        content: "调用 ExitPlanMode",
        toolName: "ExitPlanMode",
        toolArgs: JSON.stringify({
          plan:
            "## 修改《末班车》情感救赎版\n\n### 概要\n改写为约 500 字的短篇版本。\n\n" +
            "### 修改方案\n- 新建文件，不覆盖原稿。\n  - 嵌套项一\n\n### 验证方式\n- 读取新文件确认无乱码。\n",
        }),
      }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("调用 ExitPlanMode");
    // tool-call 行在本 describe 的渲染下**默认展开**（tool-call 卡在 run 态直出）；
    // 若折叠开关在场则先展开（与「工具参数升格」既有 describe 的 expandToolCall 同法；
    // 那个 helper 在另一个 describe 作用域内，此处就地取用）
    const toggle = screen.queryByTestId("msg-0-toggle");
    if (toggle) fireEvent.click(toggle);
    const card = (await screen.findByTestId("tool-args-0")) as HTMLElement;
    // **判据**：本支渲染出的卡片里存在挂 `.md-body` 的容器（与 `case "plan"` 同判据）
    const mdBody = card.querySelector(".md-body");
    expect(mdBody).not.toBeNull();
    // 结构也在（markdown 解析正常，被 preflight 拍平的是样式）
    expect(screen.getByText("修改《末班车》情感救赎版").tagName).toBe("H2");
    expect(screen.getByText("概要").tagName).toBe("H3");
    expect(mdBody!.querySelectorAll("li").length).toBeGreaterThanOrEqual(3);
    expect(mdBody!.contains(screen.getByText("嵌套项一"))).toBe(true);
  });

  it("总结模式：plan 不折叠、不计入「已折叠 N 条」计数", async () => {
    installFetch();
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "做个计划" }),
      msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
      msg({
        seq: 2,
        kind: "tool-call",
        content: "调用 Bash",
        toolName: "Bash",
        toolArgs: '{"command":"ls"}',
      }),
      msg({ seq: 3, kind: "plan", content: "## 方案\n\n落地步骤", toolName: "ExitPlanMode" }),
      msg({ seq: 4, kind: "assistant", content: "最终总结" }),
    ];
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    await screen.findByText("最终总结");
    // 折叠计数只含 thinking + tool-call = 2（plan 豁免；user 与最终 assistant 本就直显）
    expect(screen.getByTestId("summary-banner").textContent).toContain("已折叠 2 条过程消息");
    // plan 常驻直出：正文可见、无折叠头
    expect(screen.getByText("方案").tagName).toBe("H2");
    expect(screen.getByTestId("plan-3")).toBeTruthy();
    expect(screen.queryByTestId("msg-3-toggle")).toBeNull();
  });
});

// ==== 计划文件卡（批次丙 T7）：kimi 计划是一等文件，后端从工具结果识别引用后
// 补 kind="plan-file" 消息（content = 文件路径）→ 卡片显文件名 + 查看按钮 ====
describe("SessionDetail：计划文件卡（T7）", () => {
  it("plan-file 消息渲染计划文件卡：文件名可见、点击走文件预览", async () => {
    installFetch();
    const full = "C:/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans/miss-martian.md";
    routes.messages = [
      msg({ seq: 0, kind: "user", content: "做个计划" }),
      msg({
        seq: 1,
        kind: "tool-result",
        content: `Wrote 4263 bytes to ${full}`,
      }),
      msg({ seq: 2, kind: "plan-file", content: full }),
    ];
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    expect(await screen.findByTestId("plan-file-2")).toBeTruthy();
    // 「计划文件」标签 + 文件名（不含全路径，全路径在 title）
    expect(screen.getByText("计划文件")).toBeTruthy();
    expect(screen.getByTestId("plan-file-name-2").textContent).toBe("miss-martian.md");
    // 工具结果原文仍在（卡片是追加而非替换）
    expect(screen.getByText(/Wrote 4263 bytes to/)).toBeTruthy();
    // 点击「查看计划」→ 进入文件预览（openFile → preview 层）
    fireEvent.click(screen.getByTestId("plan-file-open-2"));
    expect(await screen.findByTestId("file-preview")).toBeTruthy();
  });

  it("plan-file 是常驻卡：不折叠（无折叠头）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "plan-file", content: "/w/plans/a.md" })];
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    expect(await screen.findByTestId("plan-file-0")).toBeTruthy();
    expect(screen.queryByTestId("msg-0-toggle")).toBeNull();
  });
});

// ==== 活状态流（T1 可选项，本批裁决要做）：详情页停留期间 selected 随既有
// 看板轮询数据（Board 的 SSE 跃迁/快照 + 降级 3s 轮询）自动更新——红卡与总结
// 横幅随状态切换，无需重进页面。App 级集成测试：Board 数据一拍更新 →
// onSessionsChanged 上报 → App 按 (agentType,id) 对齐 selected。反向 waiting→idle
// 同验（总结横幅切换）。组件不重挂的判据：/session-messages 不重拉
describe("SessionDetail：活状态流（T1）", () => {
  /** 可手动投帧的 EventSource（不自动发快照，测试按节奏 emit） */
  class ManualEventSource extends MockEventSource {}

  /** App 级 fetch 分路：Board 三端点 + 详情页四端点；messagesCalls 计数用于
   *  「组件不重挂」断言（重挂必触发 /session-messages 重拉） */
  function installAppFetch() {
    let messagesCalls = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes("/session-messages")) {
          messagesCalls += 1;
          return new Response(
            JSON.stringify({
              messages: [
                msg({ seq: 0, kind: "user", content: "详情页首条" }),
                msg({ seq: 1, kind: "thinking", content: "内部思考内容" }),
                msg({ seq: 2, kind: "assistant", content: "回复正文" }),
              ],
              truncated: false,
            }),
            { status: 200 }
          );
        }
        if (url.includes("/session-files")) {
          return new Response(JSON.stringify({ files: [], truncated: false }), { status: 200 });
        }
        if (url.includes("/session-approve-options")) {
          // available=true：红卡真正渲染（false 会自隐，断言会变假阴性）
          return new Response(
            JSON.stringify({
              available: true,
              options: [{ id: "1", label: "允许" }],
              verifiedWith: "test",
              currentVersion: "1.0",
              drift: false,
            }),
            { status: 200 }
          );
        }
        if (url.includes("/session-send-info")) {
          return new Response(
            JSON.stringify({ injectable: true, channels: ["tmux"], visibility: "realtime" }),
            { status: 200 }
          );
        }
        if (url.includes("/m/api/v1/host")) {
          return new Response(
            JSON.stringify({
              host: { name: "n", platform: "windows", version: "0", bootId: "boot-test" },
              enabledTools: ["claude"],
              installedTools: ["claude", "codex", "kimi", "opencode"],
            }),
            { status: 200 }
          );
        }
        if (url.includes("/m/api/v1/sessions")) {
          return new Response(JSON.stringify({ sessions: [], totalCount: 0, waitingCount: 0 }), {
            status: 200,
          });
        }
        if (url.includes("/session-subagents")) {
          // 观察台 T5：与 installFetch 同口径读 routes.subagents（缺省 []）
          return new Response(JSON.stringify({ subagents: routes.subagents ?? [] }), {
            status: 200,
          });
        }
        if (url.includes("/session-subagent-messages")) {
          // 观察台 T6：详情载荷桩（同 installFetch，缺省空载荷）
          return new Response(
            JSON.stringify({ messages: [], truncated: false, supported: true }),
            { status: 200 }
          );
        }
        throw new Error(`unexpected fetch: ${url}`);
      })
    );
    return {
      calls: () => messagesCalls,
    };
  }

  function transition(from: Session["status"], to: Session["status"]) {
    return {
      sessionId: "sess-1",
      agentType: "claude",
      from,
      to,
      projectName: "proj",
      lastMessage: null,
      ts: 1,
    };
  }

  it("idle 详情页停留期间收到 waiting 跃迁：红卡出现且组件不重挂；反向切回恢复总结横幅", async () => {
    vi.stubGlobal("EventSource", ManualEventSource);
    const appFetch = installAppFetch();
    const idle = makeSession({ status: "idle" });
    render(<App />);

    // SSE 建连后手动投首帧快照（idle 会话上卡）→ 探测成功，看板出卡
    const es = await waitFor(() => MockEventSource.latest());
    act(() => {
      es.emit("snapshot", { sessions: [idle], totalCount: 1, waitingCount: 0 });
    });
    // 卡片点击 → 进入详情：idle 是总结模式（横幅在），非 waiting（无红卡）
    fireEvent.click(screen.getByText("proj").closest("li") as HTMLLIElement);
    await screen.findByTestId("detail-back");
    expect(await screen.findByTestId("summary-banner")).toBeTruthy();
    expect(screen.queryByTestId("approve-card")).toBeNull();
    const callsAfterOpen = appFetch.calls();
    expect(callsAfterOpen).toBeGreaterThanOrEqual(1);

    // 既有数据通道一拍跃迁（idle → waiting）：selected 同步 → 红卡出现。
    // 组件不重挂：/session-messages 不重拉（重挂必重拉），detail-back 仍在
    act(() => {
      es.emit("transition", transition("idle", "waiting"));
    });
    expect(await screen.findByTestId("approve-card")).toBeTruthy();
    expect(await screen.findByText("等待批准")).toBeTruthy();
    expect(screen.queryByTestId("summary-banner")).toBeNull();
    expect(appFetch.calls()).toBe(callsAfterOpen);
    expect(screen.getByTestId("detail-back")).toBeTruthy();

    // 反向跃迁（waiting → idle）：红卡卸载，总结横幅自动恢复
    act(() => {
      es.emit("transition", transition("waiting", "idle"));
    });
    await waitFor(() => expect(screen.queryByTestId("approve-card")).toBeNull());
    expect(screen.getByTestId("summary-banner")).toBeTruthy();
    // 全程消息不重拉（一次打开，一次拉取）
    expect(appFetch.calls()).toBe(callsAfterOpen);
  });
});

// ==== 批次乙 T8：问答卡挂载（SessionDetail 正文视图）
// 丁T1（2026-09-21）挂载口径变更：**非结束态**（!isSummary）挂载——问答端点不看
// 状态（可用性由数据形态门决定），状态只是门牌；旧口径 waiting 门由本批放宽 ====
describe("SessionDetail：问答卡挂载（批次乙 T8 / 丁T1 放宽）", () => {
  /** 探测档案 §3 单选真实夹具（缩录）——QuestionCard 可用载荷 */
  const questionInfo = {
    available: true,
    source: "mark",
    questions: [
      {
        header: "Next step",
        question: "This is a demo question — what would you like to do next?",
        multiSelect: false,
        options: [
          { label: "Tool demo", description: "Explain how AskUserQuestion works." },
          { label: "Start a task", description: "Start a coding or file task." },
        ],
      },
    ],
  };

  it("waiting 会话 + 问答可用：question-card 挂载在 messageArea 上方；问答会话上 approve-card 自隐（硬约束① UI 面）", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    // last_message 给审批 marker 命中句也不出红卡——问答会话的审批不可用由后端
    // 硬约束①保证（approve-options 载荷 available=false，ApproveCard 自隐）
    render(
      <SessionDetail
        session={makeSession({ status: "waiting", lastMessage: "Do you want to proceed?" })}
        onBack={() => {}}
      />
    );
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    expect(screen.getByTestId("question-text").textContent).toContain("demo question");
    expect(screen.getByTestId("question-option-0").textContent).toContain("Tool demo");
    // 问答会话上无 允许/拒绝（approve 选项不可用即 null + 问答卡零允许/拒绝）
    expect(screen.queryByTestId("approve-card")).toBeNull();
    expect(screen.queryByText("允许")).toBeNull();
    expect(screen.queryByText("拒绝")).toBeNull();
  });

  // 丁T1 放宽的核心断言：**运行态（processing/thinking）也挂载**——codex/opencode
  // 的问题待决红灯由 T1 状态链给出，但问答卡的真正显示**不看状态**（数据形态门）；
  // 旧的 waiting 门会在状态链尚未收敛时漏掉可作答的卡
  it.each(["processing", "thinking"] as const)(
    "丁T1：%s 会话（非结束态）+ 问答可用 → 卡照常挂载",
    async (status) => {
      installFetch();
      routes.questionInfo = questionInfo;
      render(<SessionDetail session={makeSession({ status })} onBack={() => {}} />);
      expect(await screen.findByTestId("question-card")).toBeTruthy();
      // ApproveCard 不受放宽影响：非 waiting 仍不挂载（审批红灯门是刻意的）
      expect(screen.queryByTestId("approve-card")).toBeNull();
    }
  );

  it("丁T1：结束态（idle/finished）不挂载问答卡（既已聊完，不必每进详情再打 GET）", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    await screen.findByText("proj"); // 页面就绪
    await flushDetail();
    expect(screen.queryByTestId("question-card")).toBeNull();
  });

  it("waiting 会话 + 问答不可用（缺省 available=false）：卡自隐，零问答 fetch 之外的副作用", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />);
    await screen.findByText("proj");
    await flushDetail();
    expect(screen.queryByTestId("question-card")).toBeNull();
  });

  it("丁T1：运行态 + 问答不可用 → 放宽挂载也不闪空卡（可用性自隐兜底）", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await screen.findByText("proj");
    await flushDetail();
    expect(screen.queryByTestId("question-card")).toBeNull();
    // 挂载确实发生了（否则本用例断言的是「没挂载」而非「自隐」）：GET 打过一发
    expect(
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-question"))
        .length
    ).toBeGreaterThanOrEqual(1);
  });

  /** 冲刷挂载后的异步拉取链（mount fetch → setState） */
  async function flushDetail() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  // ==== 丁T1 复评 F-1：状态跃迁驱动问答卡重拉 ====
  // 「回答后回落」在详情页停留期间必须可达：QuestionCard 的 effect deps 含
  // session.status（Board 的既有数据通道把活会话 status 对齐进 selected）——
  // status 一变即重拉 /session-question

  it("丁T1 F-1：selected.status 跃迁（waiting → processing）→ 问答卡重拉", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    const { rerender } = render(
      <SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />
    );
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    const questionCalls = () =>
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-question"))
        .length;
    const before = questionCalls();
    expect(before).toBeGreaterThanOrEqual(1);

    // 模拟 App 数据通道把 status 对齐进来（同一会话对象被替换）
    rerender(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await flushDetail();
    expect(questionCalls()).toBeGreaterThan(before);
  });

  it("丁T1 F-1：反向跃迁（processing → waiting）同样重拉；id 不变不重挂（key 稳定）", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    const { rerender } = render(
      <SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />
    );
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    const questionCalls = () =>
      fetchMock.mock.calls.filter((c: unknown[]) => String(c[0]).includes("/session-question"))
        .length;
    const before = questionCalls();
    rerender(<SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />);
    await flushDetail();
    expect(questionCalls()).toBeGreaterThan(before);
    // 组件未重挂（卡仍在，无卸载-重建闪烁）：同一会话 id 的 key 稳定
    expect(screen.getByTestId("question-card")).toBeTruthy();
  });

  // ==== 丁T1 复评 F2-1：重拉只在**问题内容变化**时重置终态 ====
  // 判据动机：同会话可连续多次提问（实测单会话连续 8 次 request_user_input，
  // 其间无 task_complete），key 恒为 question-${id} 不重挂 → 无条件不清会让
  // sent=true 残留到下一题（「已发送按键」且无按钮的伪终态卡）；而无条件清会在
  // 「投递成功 → 状态回落」窗口内丢掉防连投语义。故取内容指纹判据。

  /** 点击第一个选项造成 key_sent 终态（卡显示「已发送按键」、按钮消失） */
  async function sendFirstOption() {
    fireEvent.click(await screen.findByTestId("question-option-0"));
    await flushDetail();
    expect(screen.getByTestId("question-sent")).toBeTruthy();
    expect(screen.queryByTestId("question-option-0")).toBeNull();
  }

  it("丁T1 F2-1：重拉拿到**相同**问题 → sent 保留（伪按钮不复活，防连投语义不破）", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    const { rerender } = render(
      <SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />
    );
    await sendFirstOption();
    // 状态跃迁触发重拉，但载荷是**同一个问题**（routes.questionInfo 未变）
    rerender(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await flushDetail();
    expect(screen.getByTestId("question-sent")).toBeTruthy();
    expect(
      screen.queryByTestId("question-option-0"),
      "同一问题重拉后按钮不得复活（防连投）"
    ).toBeNull();
  });

  it("丁T1 F2-1：重拉拿到**不同**问题 → sent 清（新问题可作答，无伪终态）", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    const { rerender } = render(
      <SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />
    );
    await sendFirstOption();
    // 换题（模型连续提问的第二问）：内容指纹变化 → 终态重置
    routes.questionInfo = {
      available: true,
      source: "mark",
      questions: [
        {
          header: "Ship it?",
          question: "Second question — should the project ship a README?",
          multiSelect: false,
          options: [{ label: "Yes", description: "Add a README." }],
        },
      ],
    };
    rerender(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await flushDetail();
    expect(screen.queryByTestId("question-sent")).toBeNull();
    expect(await screen.findByTestId("question-option-0")).toBeTruthy();
    expect(screen.getByTestId("question-text").textContent).toContain("Second question");
  });

  it("丁T1 F2-1 边界：重拉拿到**不可用**载荷（opencode pending 拍 input 未就绪）→ sent 不清", async () => {
    installFetch();
    routes.questionInfo = questionInfo;
    const { rerender } = render(
      <SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />
    );
    await sendFirstOption();
    // 不可用载荷（F2-3 的 opencode 空窗形态）：不是「换了题」，不得重置终态——
    // 否则刚投递的 sent 被清、按钮复活、防连投语义削弱
    routes.questionInfo = { available: false, questions: [] };
    rerender(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    await flushDetail();
    // 卡自隐（无题可显），但内部 sent 保留：再拿回**同一问题**时仍是终态
    expect(screen.queryByTestId("question-card")).toBeNull();
    routes.questionInfo = questionInfo;
    rerender(<SessionDetail session={makeSession({ status: "waiting" })} onBack={() => {}} />);
    await flushDetail();
    expect(screen.getByTestId("question-sent")).toBeTruthy();
    expect(
      screen.queryByTestId("question-option-0"),
      "空窗载荷不得把终态洗掉（按钮仍禁用）"
    ).toBeNull();
  });
});

// ==== 丁T2：计划待确认（codex/kimi）——提示条挂载与提示条→检查→N 选项全链 ====
describe("SessionDetail：计划待确认条（丁T2）", () => {
  /** 微任务冲刷（本 describe 自带；外层 flushDetail 定义在别的 describe 作用域内） */
  async function flushAsyncDetail() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  /** 计划消息条目（T1/T4 升格产物同形） */
  function planMsg(seq: number, content: string): SessionMessage {
    return {
      seq,
      role: "assistant",
      kind: "plan",
      content,
      ts: null,
      toolName: null,
      toolArgs: null,
      collapsed: false,
    };
  }
  /** 用户消息条目 */
  function userMsg(seq: number, content = "继续"): SessionMessage {
    return {
      seq,
      role: "user",
      kind: "user",
      content,
      ts: null,
      toolName: null,
      toolArgs: null,
      collapsed: false,
    };
  }
  /** 工具事件条目（批准后 wire 落 ExitPlanMode 回执的形态） */
  function toolMsg(seq: number, kind: "tool-call" | "tool-result"): SessionMessage {
    return {
      seq,
      role: "assistant",
      kind,
      content: "ExitPlanMode",
      ts: null,
      toolName: kind === "tool-call" ? "ExitPlanMode" : null,
      toolArgs: null,
      collapsed: true,
    };
  }

  it("codex processing + 尾部计划提案：挂载审批卡并渲染「计划待确认」条（waiting 门被数据形态门放宽）", async () => {
    installFetch();
    routes.messages = [userMsg(0), planMsg(1, "# 方案\n\n正文")];
    routes.approveOptions = {
      available: true,
      options: [],
      verifiedWith: "0.154.0",
      currentVersion: "0.155.1",
      drift: false,
      planPending: true,
      plan: { content: "# 方案\n\n正文", isFile: false },
    };
    render(
      <SessionDetail
        session={makeSession({ status: "processing", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
    expect(screen.getByTestId("approve-plan-check")).toBeTruthy();
  });

  it("codex 计划之后已有用户消息（Implement the plan.）：不挂载审批卡（预期态清除，无新存储）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# 方案"), userMsg(1, "Implement the plan.")];
    render(
      <SessionDetail
        session={makeSession({ status: "processing", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    await screen.findByText("proj");
    await flushAsyncDetail();
    expect(screen.queryByTestId("approve-card")).toBeNull();
  });

  it("claude processing + 尾部计划（既有计划批准走 waiting 门）：不挂载（门不放宽到 claude——零回归）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# 方案")];
    render(
      <SessionDetail
        session={makeSession({ status: "processing", agentType: "claude" })}
        onBack={() => {}}
      />
    );
    await screen.findByText("proj");
    await flushAsyncDetail();
    expect(screen.queryByTestId("approve-card")).toBeNull();
  });

  it("计划后已有工具事件（批准后 ExitPlanMode 回执）：不挂载（预期态清除）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# 方案"), toolMsg(1, "tool-call")];
    render(
      <SessionDetail
        session={makeSession({ status: "processing", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    await screen.findByText("proj");
    await flushAsyncDetail();
    expect(screen.queryByTestId("approve-card")).toBeNull();
  });

  it("全链：提示条 → 点检查 → 屏读命中 N 选项 → 点按生效（POST dialog:<n>）", async () => {
    installFetch();
    routes.messages = [userMsg(0), planMsg(1, "# 方案")];
    routes.approveOptions = {
      available: true,
      options: [],
      verifiedWith: "0.154.0",
      currentVersion: "0.155.1",
      drift: false,
      planPending: true,
      plan: { content: "# 方案", isFile: false },
    };
    render(
      <SessionDetail
        session={makeSession({ status: "processing", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    // 第一次拉取：计划待确认条（零选项）
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
    // 第二次拉取（点检查）：屏读命中三选项
    routes.approveOptions = {
      available: true,
      options: [
        { id: "dialog:1", label: "Yes, implement this plan" },
        { id: "dialog:2", label: "Yes, clear context and implement" },
        { id: "dialog:3", label: "No, stay in Plan mode" },
      ],
      verifiedWith: "0.154.0",
      currentVersion: "0.155.1",
      drift: false,
      dialog: true,
    };
    routes.approve = { status: "key_sent" };
    fireEvent.click(screen.getByTestId("approve-plan-check"));
    const opt = await screen.findByTestId("approve-option-dialog:1");
    expect(opt.textContent).toContain("Yes, implement this plan");
    fireEvent.click(opt);
    expect(await screen.findByTestId("approve-sent")).toBeTruthy();
    const post = fetchMock.mock.calls.find((c: unknown[]) =>
      /\/session-approve$/.test(String(c[0]))
    );
    expect(JSON.parse(String((post?.[1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      optionId: "dialog:1",
    });
  });
});

// ==== 丁T2：计划待确认挂载门的边界（结束态不挂载——与 QuestionCard 挂载门同规）====
describe("SessionDetail：计划待确认挂载门的真机状态矩阵（丁T2 复评 F3-1）", () => {
  /** 计划消息条目（T1/T4 升格产物同形） */
  function planMsg(seq: number, content: string): SessionMessage {
    return {
      seq,
      role: "assistant",
      kind: "plan",
      content,
      ts: null,
      toolName: null,
      toolArgs: null,
      collapsed: false,
    };
  }
  /** 计划待确认的**真机载荷**（后端 `available=true` + 零 options + planPending） */
  const realPlanPendingPayload = {
    available: true,
    options: [],
    verifiedWith: "0.154.0",
    currentVersion: "0.155.1",
    drift: false,
    planPending: true,
    plan: { content: "# 方案", isFile: false },
  };
  async function flush() {
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        await Promise.resolve();
      });
    }
  }

  // **F3-1 主用例**：codex 计划提案后的**真机状态就是 Idle**（assistant(<proposed_plan>)
  // → task_complete → TurnEnd → Idle；codex 兜底红已废）。首版实现用 `!isSummary` 排除
  // idle → 真机上卡片恒不挂载（后端 available=true 无消费方）。本用例锁死修复。
  it("codex **idle**（真机形态）+ 尾部计划提案：审批卡必须挂载 + 计划待确认条出现", async () => {
    installFetch();
    // 真机尾序：user → assistant(前导文本) → plan(<proposed_plan> 升格产物)
    routes.messages = [
      {
        seq: 0,
        role: "user",
        kind: "user",
        content: "改写成情感救赎版",
        ts: null,
        toolName: null,
        toolArgs: null,
        collapsed: false,
      },
      planMsg(1, "# 《末班车》情感救赎版改写方案\n\n## Summary\n改写重点…"),
    ];
    routes.approveOptions = realPlanPendingPayload;
    render(
      <SessionDetail
        session={makeSession({ status: "idle", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    // 挂载 → 拉载 → 提示条与检查钮在场（真机全链的入口）
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
    expect(screen.getByTestId("approve-plan-check")).toBeTruthy();
    expect(screen.getByTestId("approve-card").textContent).toContain("计划待确认");
  });

  // 边界对照：kimi 的计划审批真机落 **Waiting**（interaction.request 红灯），
  // idle 只是防御位（兔维斯 未运行时状态可能回落）——两者都必须挂载。
  it("kimi idle + 尾部计划提案：同样挂载（防御位——真机在 Waiting 已由既有用例覆盖）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# Plan: Create hi.txt"), planMsg(1, "# Plan: Create yo.txt")];
    routes.approveOptions = realPlanPendingPayload;
    render(
      <SessionDetail
        session={makeSession({ status: "idle", agentType: "kimi" })}
        onBack={() => {}}
      />
    );
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
  });

  // `finished` 仍排除：会话真的结束（进程退出/归档），重进详情不再打 GET。
  it("codex finished + 尾部计划提案：不挂载（会话真的结束——唯一排除态）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# 已结束会话里的旧计划")];
    routes.approveOptions = realPlanPendingPayload;
    render(
      <SessionDetail
        session={makeSession({ status: "finished", agentType: "codex" })}
        onBack={() => {}}
      />
    );
    await screen.findByText("proj");
    await flush();
    expect(screen.queryByTestId("approve-card")).toBeNull();
  });

  // 2026-10-04 翻转（审批卡不出修复）：claude **入**计划预期态门——其计划批准等待
  // 的挂载面与后端扫描器 claude_plan_pending 同判据（实况取证：标记 0 行、detect
  // 对原生 UI 文案恒 miss，旧依据「Waiting + detect 就够」已被证伪）。真机常态下
  // 状态层（ExitPlanMode→Waiting）已亮红灯，本用例锁的是 idle+尾计划的**陈旧窗口**
  // 也照样挂载（后端 available 裁决不变——门放宽只多一次 GET，卡自隐兜底）。
  it("claude idle + 尾部计划：挂载计划待确认卡（2026-10-04 入族翻转）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# claude 的计划")];
    routes.approveOptions = realPlanPendingPayload;
    render(
      <SessionDetail
        session={makeSession({ status: "idle", agentType: "claude" })}
        onBack={() => {}}
      />
    );
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
  });

  // 反向锁（保留）：**finished 仍排除**对 claude 同样生效——会话真结束不挂载不打 GET
  it("claude finished + 尾部计划：不挂载（finished 排除对全工具一致）", async () => {
    installFetch();
    routes.messages = [planMsg(0, "# claude 的旧计划")];
    routes.approveOptions = realPlanPendingPayload;
    render(
      <SessionDetail
        session={makeSession({ status: "finished", agentType: "claude" })}
        onBack={() => {}}
      />
    );
    await screen.findByText("proj");
    await flush();
    expect(screen.queryByTestId("approve-card")).toBeNull();
  });
});

// ==== 丁T2 复审 N2：前后端判据的**跨语言共享夹具锁**（真锁，非人工镜像）====
//
// `isPlanPending`（前端）与 `remote::api::plan_pending_tail_index` / `_strict_` 系列
// （后端）是**两份同口径实现**——判据一致、窗口不同（前端吃详情页已拉取的整页
// 200/1000 条；后端读 40 条尾部窗口，性能面）。
//
// **本组用例与 Rust 侧 `remote::api::tests::plan_pending_cross_language_fixture_cases`
// 读同一份夹具文件**（`tests/fixtures/plan_pending_cases.json`）：夹具与期望都在文件里，
// 两侧只负责「驱动各自实现 + 对照同一份期望」。**这才是跨语言可达的真约束**——
// 只改一侧实现而不更新夹具文件，该侧必红（另一侧仍绿，但漂移一定被抓）。
//
// 前身（复评 M1）曾把这段写成「单边漂移即本锁失败」的**人工镜像**注释——那是不实声明
// （跨语言无法 import，Rust 侧改动不会让 vitest 失败）；N2 已改为共享夹具，声明与实现对齐。
describe("丁T2 N2：isPlanPending 与后端判据的跨语言共享夹具锁", () => {
  function m(kind: string): SessionMessage {
    return {
      seq: 0,
      role: kind === "user" ? "user" : "assistant",
      kind,
      content: "x",
      ts: null,
      toolName: null,
      toolArgs: null,
      collapsed: false,
    };
  }

  it("共享夹具逐例：前端 isPlanPending 与文件里的期望一致（与 Rust 侧同表）", () => {
    let seenTrue = 0;
    let seenFalse = 0;
    let seenLenientOnly = 0;
    for (const c of planPendingCases.cases) {
      const msgs = c.kinds.map(m);
      expect(isPlanPending(msgs, c.tool)).toBe(c.pending);
      expect(msgs.length).toBe(c.kinds.length); // 夹具形态自检（防 map 退化）
      if (c.pending) seenTrue += 1;
      else seenFalse += 1;
      if (c.lenient_only) seenLenientOnly += 1;
      // 共享表只描述**宽松档**（前端 isPlanPending 即宽松档——它与后端审批侧同判据）；
      // `lenient_only` 用例在前端同样为 true（严档是后端问答压制专用，前端不实现）
      if (c.lenient_only) expect(c.pending).toBe(true);
    }
    // 用例集自检（与 Rust 侧同款断言，防「夹具被删空后测试恒绿」）
    expect(seenTrue).toBeGreaterThan(0);
    expect(seenFalse).toBeGreaterThan(0);
    expect(seenLenientOnly).toBe(1);
  });

  it("共享夹具的工具族名单：非计划族一律不参与（族内 = codex/kimi + claude，2026-10-04 起claude 入族）", () => {
    for (const tool of planPendingCases.non_plan_tools) {
      // 用一条「在场」夹具驱动：工具不在族内 → 恒 false
      expect(isPlanPending([m("plan")], tool)).toBe(false);
    }
    // 族内三家：同一夹具恒 true（对照，防「全员 false」的假绿）。claude 2026-10-04
    // 起入族（审批卡不出修复——其计划批准等待的挂载门与后端扫描器 claude_plan_pending
    // 同判据；当年排除 claude 的依据已被实况证伪，见 isPlanPending 注释）
    for (const tool of ["codex", "kimi", "claude"]) {
      expect(isPlanPending([m("plan")], tool)).toBe(true);
    }
    // 缺省/ null 工具 → 不放宽（不在族内）
    expect(isPlanPending([m("plan")], null)).toBe(false);
    expect(isPlanPending([m("plan")], undefined)).toBe(false);
  });

  it("窗口差异的如实申报（本锁不能覆盖的面）", () => {
    // 前端整页（200/1000）vs 后端 40 条尾部窗口：计划卡若落在第 41+ 条历史里，后端看不到
    // 而前端看得到 → 前端挂载卡片、后端回 available=false、卡片按自隐契约消失
    // （代价 = 一次多余 GET，不是错误界面）。「卡片挂着但点了报错」由 POST 门与 GET
    // 同口径保证（F3-2 + N1），不在本锁范围。
    const longHistory: SessionMessage[] = [];
    for (let i = 0; i < 60; i += 1) longHistory.push(m("user"));
    longHistory.push(m("plan"));
    // 前端全页看得见尾部计划 → true（后端 40 条窗口会看不到）
    expect(isPlanPending(longHistory, "codex")).toBe(true);
    // 但若那 60 条里有用户消息紧跟在计划之后（真实消费信号），前端也判 false
    const consumed = [...longHistory, m("user")];
    expect(isPlanPending(consumed, "codex")).toBe(false);
  });
});

// ==== 子 Agent 观察台 T5：清单拉取上提 SessionDetail（单一数据源）+ 点击跳转 ====
// T1 sheet 化：清单从 FilePanel 卡区迁出为独立 sheet 看板——集成面改为：
// 顶栏切「子 Agent」sheet → 清单全量名单（绿灰同列）→ 点卡进详情（带返回钮）→ 返回回清单。
describe("SessionDetail：子 Agent 清单与跳转（观察台 §二）", () => {
  const subagentRoutes = [
    {
      id: "a1",
      name: "Plan",
      description: "设计新方案",
      spawnTs: "2026-10-08T07:31:07Z",
      tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
      status: "running" as const,
      endTs: null,
    },
    {
      id: "b1",
      name: "Explore",
      description: null,
      spawnTs: "2026-10-08T07:00:00Z",
      tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
      status: "idle" as const,
      endTs: "2026-10-08T07:30:00Z",
    },
  ];

  it("顶栏切「子 Agent」sheet → 清单渲染全量名单（绿灰同列）；点卡进详情带返回钮；返回回清单", async () => {
    installFetch();
    routes.subagents = subagentRoutes;
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // jsdom 无 matchMedia → 窄屏语义，面板默认 fullscreen 浮层
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    // files sheet 的看板是 FilePanel，不再含子 Agent 卡区
    expect(await screen.findByTestId("file-panel")).toBeTruthy();
    expect(screen.queryByTestId("subagent-list")).toBeNull();
    // 顶栏切到「子 Agent」sheet → 清单看板
    fireEvent.click(screen.getByTestId("sheet-tab-subagents"));
    const shell = await screen.findByTestId("preview-shell");
    expect(shell.getAttribute("data-sheet")).toBe("subagents");
    expect(shell.getAttribute("data-open")).toBe("none");
    expect(await screen.findByTestId("subagent-card-a1")).toBeTruthy();
    expect(screen.getByTestId("subagent-dot-b1").className).toContain("bg-gray-400");
    // 点卡 → 详情（open.subagent，从清单进入 → backToList=true → 返回钮在场）
    fireEvent.click(screen.getByTestId("subagent-card-a1"));
    await vi.waitFor(() => {
      const el = document.querySelector('[data-sheet="subagents"][data-open="subagent"]');
      expect(el).toBeTruthy();
    });
    expect(screen.getByTestId("subagent-detail")).toBeTruthy();
    expect(screen.getByTestId("subagent-back")).toBeTruthy();
    // 返回 → 收回看板态（清单复现，选中高亮消失）
    fireEvent.click(screen.getByTestId("subagent-back"));
    expect((await screen.findByTestId("preview-shell")).getAttribute("data-open")).toBe("none");
    expect(screen.getByTestId("subagent-card-a1")).toBeTruthy();
  });
});

// ==== T1 预览区 sheet 化：顶栏 / 决策 7 / 空态 / chip 直达 ====
describe("SessionDetail：预览区 sheet 化（T1）", () => {
  it("顶栏两 sheet 钮 + 运行数徽标「子 Agent (n)」；点钮互切看板", async () => {
    installFetch();
    routes.subagents = [
      {
        id: "a1",
        name: "Plan",
        description: null,
        spawnTs: "2026-10-08T07:31:07Z",
        tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
        status: "running",
        endTs: null,
      },
      {
        id: "b1",
        name: "Explore",
        description: null,
        spawnTs: "2026-10-08T07:00:00Z",
        tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
        status: "idle",
        endTs: "2026-10-08T07:30:00Z",
      },
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    // 顶栏：文件钮高亮；子 Agent 钮带 running 数徽标（1 running / 2 总数）
    const filesTab = screen.getByTestId("sheet-tab-files");
    expect(filesTab.getAttribute("aria-pressed")).toBe("true");
    const saTab = screen.getByTestId("sheet-tab-subagents");
    expect(saTab.getAttribute("aria-pressed")).toBe("false");
    expect(saTab.textContent).toContain("子 Agent (1)");
    // 切到子 Agent sheet：看板换为清单，FilePanel 退场
    fireEvent.click(saTab);
    expect((await screen.findByTestId("preview-shell")).getAttribute("data-sheet")).toBe(
      "subagents"
    );
    expect(await screen.findByTestId("subagent-list")).toBeTruthy();
    expect(screen.queryByTestId("file-panel")).toBeNull();
    expect(screen.getByTestId("sheet-tab-subagents").getAttribute("aria-pressed")).toBe("true");
    // 切回文件 sheet
    fireEvent.click(screen.getByTestId("sheet-tab-files"));
    expect((await screen.findByTestId("preview-shell")).getAttribute("data-sheet")).toBe("files");
    expect(await screen.findByTestId("file-panel")).toBeTruthy();
  });

  it("无子 agent 名单（空数组）→ 子 Agent 钮不渲染（决策 7），文件钮仍在", async () => {
    installFetch();
    routes.subagents = [];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    expect(screen.getByTestId("sheet-tab-files")).toBeTruthy();
    expect(screen.queryByTestId("sheet-tab-subagents")).toBeNull();
  });

  it("宽屏分屏：文件 sheet 一级=看板；点文件后两层级换文件预览（返回钮回看板）", async () => {
    installFetch();
    routes.messages = [msg({ seq: 0, kind: "assistant", content: "hi" })];
    routes.files = [fileEntry("/tmp/proj/src/app.rs")];
    routes.fileContent = "fn main() {}";
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (q: string) => ({ matches: q.includes("min-width"), media: q }),
    });
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    // 宽屏打开面板 → 默认 split-h：左栏对话、右栏预览区（顶栏 + 一级看板）
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    expect(await screen.findByTestId("preview-shell")).toBeTruthy();
    expect(screen.getByTestId("split-container").className).toContain("flex-row");
    expect(screen.getByTestId("sheet-board")).toBeTruthy();
    expect(screen.getByTestId("file-panel")).toBeTruthy();
    // 点文件 → 二级：文件预览替换看板（单栏整宽），页头返回钮回看板
    fireEvent.click(screen.getByTestId("file-row-0-open"));
    expect(await screen.findByTestId("file-preview")).toBeTruthy();
    expect((await screen.findByTestId("preview-shell")).getAttribute("data-open")).toBe("file");
    expect(screen.queryByTestId("sheet-board")).toBeNull();
    expect(screen.getByTestId("preview-back-list")).toBeTruthy();
    // 返回钮 → 回一级看板
    fireEvent.click(screen.getByTestId("preview-back-list"));
    expect(await screen.findByTestId("file-panel")).toBeTruthy();
    expect(screen.queryByTestId("file-preview")).toBeNull();
    // 预览区关闭钮照旧收回整个预览区
    fireEvent.click(screen.getByTestId("preview-close"));
    expect(screen.queryByTestId("preview-shell")).toBeNull();
  });

  it("chip 点击直达：切到「子 Agent」sheet 并进详情，页头带返回钮（两层级导航，2026-10-09 裁决）；宽屏默认 split-h", async () => {
    installFetch();
    routes.subagents = [
      {
        id: "a1",
        name: "Plan",
        description: "设计新方案",
        spawnTs: "2026-10-08T07:31:07Z",
        tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
        status: "running",
        endTs: null,
      },
    ];
    // 宽屏 shim：chip 直达默认进 split-h（左对话右详情，T2 决策 2）
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (q: string) => ({ matches: q.includes("min-width"), media: q }),
    });
    render(<SessionDetail session={makeSession({ status: "processing" })} onBack={() => {}} />);
    // 运行态 chip 行在 cardDock（finished 不挂载——既有门不变）
    fireEvent.click(await screen.findByTestId("subagent-chip-a1"));
    // sheet=subagents + open=subagent + mode=split-h（两层级导航：chip 直达落详情级，返回钮回清单）
    await vi.waitFor(() => {
      expect(
        document.querySelector('[data-sheet="subagents"][data-open="subagent"][data-mode="split-h"]')
      ).toBeTruthy();
    });
    expect(screen.getByTestId("subagent-detail")).toBeTruthy();
    expect(screen.getByTestId("split-container").className).toContain("flex-row");
    expect(screen.getByTestId("subagent-back")).toBeTruthy(); // 两层级导航：详情级必有返回钮


// ==== Task 8（H7）：无头回执卡（发送中 / 回执 / 失败分诊 + 取消钮）====
//
// 无头会话（zcode）的发送是**回合级**动作：HTTP 请求要等整个无头进程跑完（实测 8–23s，
// 长则看门狗 600s），故回执卡必须与「发送中」共存并给取消入口；回执里的 assistant 摘要 /
// token / 耗时 / 可见性提示全部由后端下发（**后端给文案、前端只渲染**——与 H3 置灰同纪律）。
// ==== Task 9（H8）：codex 入队回执（**入队 ≠ 已送达**）====
//
// `codex queue` 的 exit 0 只证入队：后端在 60s 消费确认后给 `ok`（rollout 追加命中 =
// 已消费）或 `queued`（已入队未消费，附可执行建议）。同一 `queued` 状态自 Task 9 起有
// 两个成因（H4 全局并发名额满 / H8 APP 入队待消费）⇒ 卡片**只渲染中性「排队中」**，
// 成因一律由后端 `reason` 原样透出（后端给文案、前端只渲染的同一纪律）。
describe("SessionDetail：codex 入队回执（Task 9 / H8）", () => {
  function codexSession() {
    return makeSession({ id: "sess-h8", agentType: "codex", form: "app", status: "idle" });
  }

  async function sendFromComposer(text: string) {
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: text } });
    fireEvent.click(screen.getByTestId("composer-send"));
  }

  it("已入队未消费：queued 卡不冒充成功、不编成因；输入框保留正文（可重试/改道）", async () => {
    installFetch();
    routes.sendInfo = {
      injectable: true,
      channels: ["headless_codex_queue", "headless_codex_exec"],
      visibility: "realtime",
    };
    routes.sessionSend = {
      status: "headless",
      channel: "headless_codex_queue",
      receipt: {
        status: "queued",
        sessionId: "sess-h8",
        durationMs: 61234,
        reason:
          "已入队未消费：thread 01a10735-1354-7d10-822a-f3bd9e041c12 的条目仍滞留在 codex 队列" +
          "（queue_1.sqlite 副本的 queued_items 查到）= thread 未被 APP 打开；建议在 APP 打开该会话，" +
          "或改走 exec resume（入队回执 id=m-2）",
      },
    };
    render(<SessionDetail session={codexSession()} onBack={() => {}} />);
    await sendFromComposer("hi [mobile]");
    await waitFor(() => expect(screen.getByTestId("headless-queued")).toBeTruthy());
    // 中性成因文案：不得再对 H8 谎报「全局并发名额已满」
    expect(screen.getByTestId("headless-queued").textContent).not.toContain("全局并发名额");
    expect(screen.getByTestId("headless-reason").textContent).toContain("已入队未消费");
    expect(screen.getByTestId("headless-reason").textContent).toContain("APP 打开该会话");
    // 未确认消费 ⇒ 不是成功：无末条回复、无「完成」标、正文保留
    expect(screen.queryByTestId("headless-ok")).toBeNull();
    expect(screen.queryByTestId("headless-last-assistant")).toBeNull();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("hi [mobile]");
  });

  it("已消费：ok 卡如实标注「回合归 APP 自身」（不编末条回复）", async () => {
    installFetch();
    routes.sendInfo = {
      injectable: true,
      channels: ["headless_codex_queue", "headless_codex_exec"],
      visibility: "realtime",
    };
    routes.sessionSend = {
      status: "headless",
      channel: "headless_codex_queue",
      receipt: {
        status: "ok",
        sessionId: "sess-h8",
        durationMs: 21234,
        reason:
          "APP 已消费该消息（目标 rollout 追加命中消息文本；入队回执 id=m-7）——" +
          "回合执行与回复由 codex APP 自身完成，请在 APP 内或看板刷新后查看",
      },
    };
    render(<SessionDetail session={codexSession()} onBack={() => {}} />);
    await sendFromComposer("hi");
    await waitFor(() => expect(screen.getByTestId("headless-ok")).toBeTruthy());
    expect(screen.getByTestId("headless-reason").textContent).toContain("已消费该消息");
    expect(screen.queryByTestId("headless-last-assistant")).toBeNull();
    // 已消费 = 消息真的进了会话 → 清空输入区（与终端 delivered 同口径）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });
});

describe("SessionDetail：无头回执卡（Task 8 / H7）", () => {
  function headlessSession() {
    return makeSession({ id: "sess-h7", agentType: "zcode", status: "waiting" });
  }

  /** send-info 报无头通道（输入区可用 + 通道名 + 可见性档） */
  function headlessSendInfo(visibility = "after_restart") {
    return { injectable: true, channels: ["headless_zcode"], visibility };
  }

  function headlessReceipt(over: Record<string, unknown> = {}) {
    return {
      status: "ok",
      sessionId: "sess-h7",
      lastAssistant: "已经改好了",
      tokens: 321,
      durationMs: 8456,
      ...over,
    };
  }

  async function sendFromComposer(text: string) {
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: text } });
    fireEvent.click(screen.getByTestId("composer-send"));
  }

  it("发送中出无头进度卡与取消钮；回执到达后展示末条 assistant/token/耗时/可见性提示", async () => {
    installFetch();
    routes.sendInfo = headlessSendInfo("after_restart");
    routes.sessionSend = "pending"; // 手动闸：驱动「发送中」态
    render(<SessionDetail session={headlessSession()} onBack={() => {}} />);
    await sendFromComposer("无头你好");

    const card = await screen.findByTestId("headless-receipt-card");
    expect(card.getAttribute("data-phase")).toBe("sending");
    expect(screen.getByTestId("headless-sending")).toBeTruthy();
    const cancel = screen.getByTestId("headless-cancel");
    expect(cancel).toBeTruthy();

    // 取消钮 → POST /session-headless-cancel（带会话号；回合照常收尾）
    fireEvent.click(cancel);
    await waitFor(() => {
      const call = fetchMock.mock.calls.find((c) =>
        String(c[0]).includes("/session-headless-cancel")
      );
      expect(call).toBeTruthy();
      expect(String((call?.[1] as RequestInit | undefined)?.body)).toContain("sess-h7");
    });

    act(() => {
      sendGate?.resolve({
        status: "headless",
        channel: "headless_zcode",
        receipt: headlessReceipt(),
        visibility: "after_restart",
        visibilityNote: "已信任工作区：重启 ZCode 应用后可见",
      });
    });
    await waitFor(() =>
      expect(screen.getByTestId("headless-receipt-card").getAttribute("data-phase")).toBe("done")
    );
    expect(screen.getByTestId("headless-last-assistant").textContent).toContain("已经改好了");
    expect(screen.getByTestId("headless-tokens").textContent).toContain("321");
    expect(screen.getByTestId("headless-duration").textContent).toContain("8.5");
    expect(screen.getByTestId("headless-visibility").textContent).toContain(
      "重启 ZCode 应用后可见"
    );
    expect(screen.queryByTestId("headless-cancel")).toBeNull();
    // 成功回合 = 消息已进 ZCode → 输入框清空（与终端的 delivered 同口径）
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("");
  });

  it("无头失败：分诊卡如实展示阶段与原因，输入框保留正文（可重试）且不承诺可见性", async () => {
    installFetch();
    routes.sendInfo = headlessSendInfo("tuvis_only");
    routes.sessionSend = {
      status: "headless",
      channel: "headless_zcode",
      receipt: headlessReceipt({
        status: "failed",
        stage: "workspace_busy",
        reason:
          "工作区忙：ZCode 应用在项目 /tmp/proj 活跃；已重试 2 次仍失败——请在该工作区空闲后重发",
        lastAssistant: undefined,
        tokens: undefined,
        durationMs: 15234,
      }),
    };
    render(<SessionDetail session={headlessSession()} onBack={() => {}} />);
    await sendFromComposer("给我改代码");

    await waitFor(() =>
      expect(screen.getByTestId("headless-receipt-card").getAttribute("data-phase")).toBe("done")
    );
    expect(screen.getByTestId("headless-failed")).toBeTruthy();
    expect(screen.getByTestId("headless-stage").textContent).toContain("工作区忙");
    expect(screen.getByTestId("headless-reason").textContent).toContain("请在该工作区空闲后重发");
    expect(screen.queryByTestId("headless-visibility")).toBeNull();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("给我改代码");
    expect(screen.queryByTestId("headless-cancel")).toBeNull();
  });

  it("无头取消回执：如实报「已取消」，不冒充成功；输入框保留正文", async () => {
    installFetch();
    routes.sendInfo = headlessSendInfo();
    routes.sessionSend = {
      status: "headless",
      channel: "headless_zcode",
      receipt: headlessReceipt({
        status: "cancelled",
        reason: "已取消（移动端请求，先到者生效）；kill 进程树 = job_object",
        lastAssistant: undefined,
        tokens: undefined,
      }),
    };
    render(<SessionDetail session={headlessSession()} onBack={() => {}} />);
    await sendFromComposer("这条会被取消");
    await waitFor(() => expect(screen.getByTestId("headless-cancelled")).toBeTruthy());
    expect(screen.getByTestId("headless-reason").textContent).toContain("已取消");
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "这条会被取消"
    );
  });

  it("终端通道会话不渲染无头卡（零回归）", async () => {
    installFetch();
    routes.sendInfo = { injectable: true, channels: ["tmux"], visibility: "realtime" };
    routes.sessionSend = { status: "delivered" };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await sendFromComposer("终端消息");
    await waitFor(() => expect(screen.getByTestId("send-receipt-delivered")).toBeTruthy());
    expect(screen.queryByTestId("headless-receipt-card")).toBeNull();
  });

  /// **复审 Important 1（假在飞态）**：请求本身抛异常（网络断/开关中途关闭/500）时，
  /// 卡片**不得**停在「无头回合进行中…」（那是一个编造的在飞回合，且取消钮永远挂着）——
  /// 必须落到失败终态：无取消钮、分诊如实、输入框保留正文
  it("请求抛异常时无头卡落到失败终态（不留假在飞态、无取消钮）", async () => {
    installFetch();
    routes.sendInfo = headlessSendInfo("tuvis_only");
    routes.sessionSend = "reject";
    render(<SessionDetail session={headlessSession()} onBack={() => {}} />);
    await sendFromComposer("这条发不出去");
    await waitFor(() =>
      expect(screen.getByTestId("headless-receipt-card").getAttribute("data-phase")).toBe("done")
    );
    const card = screen.getByTestId("headless-receipt-card");
    expect(card.getAttribute("data-status")).toBe("failed");
    expect(screen.getByTestId("headless-failed")).toBeTruthy();
    expect(screen.getByTestId("headless-stage").textContent).toContain("通道异常");
    expect(screen.getByTestId("headless-reason").textContent).toContain("网络");
    expect(screen.queryByTestId("headless-cancel")).toBeNull();
    expect(screen.queryByTestId("headless-sending")).toBeNull();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe(
      "这条发不出去"
    );
    // 终端通道的同型请求异常：仍走既有 failed chip（零回归），不产生无头卡
    cleanup();
    installFetch();
    routes.sendInfo = { injectable: true, channels: ["tmux"], visibility: "realtime" };
    routes.sessionSend = "reject";
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    await sendFromComposer("终端也断了");
    await waitFor(() => expect(screen.getByTestId("send-receipt-failed")).toBeTruthy());
    expect(screen.queryByTestId("headless-receipt-card")).toBeNull();
  });

  /// **复审：跨语言 stage 名单锁**（`tests/fixtures/headless_stages.json` 是唯一名单）：
  /// 前端分诊表必须**恰好**覆盖后端 `inject::headless::receipt::Stage` 的全部 wire 名——
  /// 任一侧新增变体而另一侧没跟上，Rust 侧（receipt.rs 同名断言）或本测必有一侧先红
  it("分诊表与跨语言 stage 名单逐项一致（后端变体不多不少）", () => {
    const fixture = stagesFixture as { stages: string[] };
    const mine = Object.keys(HEADLESS_STAGE_TRIAGE).sort();
    const want = [...fixture.stages].sort();
    expect(mine).toEqual(want);
    // 每个 stage 都有非空分诊文案（不得留空串导致卡片显示空白）
    for (const key of want) {
      expect(HEADLESS_STAGE_TRIAGE[key]?.length ?? 0).toBeGreaterThan(0);
    }
    // 未知档如实兜底（不编成因）
    expect(headlessStageText("brand_new_stage")).toContain("未分类失败");
    expect(headlessStageText(undefined)).toBe("未分类失败");
  });

  /// **Task 10（H5）审批卡接口预留**：回执卡里如实说明「无头审批面**未接线**」（claude
  /// 通道 C4 才启用）——占位面存在且不假装可用。文案与 i18n
  /// `settings.remote.headlessApprovalPending`（zh/en 双语键；桌面设置页有真实消费者）同字面。
  it("回执卡渲染审批占位：明说 claude 通道（C4）才启用，卡上无可交互审批控件", () => {
    render(
      <HeadlessReceiptCard
        session={headlessSession()}
        turn={{ phase: "done", receipt: headlessReceipt() }}
        onDismiss={() => {}}
      />
    );
    const note = screen.getByTestId("headless-approval-pending");
    expect(note.textContent).toContain("无头通道审批将在 claude 通道（C4）启用");
    // 未接线 = 卡片上除「收起」外**没有任何可交互控件**（不假装可用）：全卡按钮表逐项
    // 钉死（多出任何审批钮即红），且占位子树内不得有按钮/输入框/role=button
    const buttons = screen.getAllByRole("button");
    expect(buttons.map((b) => b.getAttribute("data-testid"))).toEqual(["headless-dismiss"]);
    expect(within(note).queryAllByRole("button")).toHaveLength(0);
    expect(within(note).queryAllByRole("textbox")).toHaveLength(0);
    expect(note.querySelectorAll("input, button, [role='button']")).toHaveLength(0);
    expect(screen.queryByTestId("headless-approval-allow")).toBeNull();
    expect(screen.queryByTestId("headless-approval-deny")).toBeNull();
  });
});
