import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import Board from "@/mobile/Board";
import NewSessionForm from "@/mobile/NewSessionForm";
import { ZCODE_CREATE_CONFIRMATIONS } from "@/mobile/api";
import type { ZcodeCreateInfo, ZcodeCreateResult } from "@/mobile/api";
import { HEADLESS_STAGE_TRIAGE } from "@/mobile/SessionDetail";
import confirmationsFixture from "../fixtures/zcode_create_confirmations.json";
import { MockEventSource } from "./eventSourceMock";

// H10（Task 12）：移动端 zcode 无头新建表单（独立交付）。fetch 全量 stub（盖过
// setup.ts 的 msw），按 URL 分路到 /session-create-zcode-info 与 /session-create-zcode
// （**长路径先判**：-info ⊃ 无后缀者）。
//
// 断言纪律（与后端诚实红线同源）：会话号/可见性/黄字信号/失败分诊全部来自后端回执，
// 前端只渲染；**未确认**（sessionId 空串 + confirmation="none"）时不得显示任何会话号。

interface Routes {
  info?: ZcodeCreateInfo;
  /** info 端点非 2xx（403 设备失效 → api 层返回 null） */
  infoStatus?: number;
  create?: ZcodeCreateResult;
  createStatus?: number;
  createBody?: Record<string, unknown>;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  routes = {};
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function infoPayload(overrides: Partial<ZcodeCreateInfo> = {}): ZcodeCreateInfo {
  return {
    available: true,
    tool: "zcode",
    defaultFirstText: "hi",
    // note 是后端 Visibility::note() 的逐字文案（前端只渲染，复审 Minor 2）
    candidates: [
      {
        path: "E:/trusted_proj",
        source: "trusted",
        trusted: true,
        note: "已信任工作区：重启 ZCode 应用后可见",
      },
      {
        path: "E:/board_proj",
        source: "board",
        trusted: false,
        note: "未信任工作区：仅 MAM 可见",
      },
    ],
    ...overrides,
  };
}

function createPayload(overrides: Partial<ZcodeCreateResult> = {}): ZcodeCreateResult {
  return {
    channel: "headless_zcode",
    sessionId: "sess_22222222-2222-2222-2222-222222222222",
    confirmation: "store",
    receipt: {
      status: "ok",
      sessionId: "sess_22222222-2222-2222-2222-222222222222",
      lastAssistant: "首句已答",
      tokens: 12,
      durationMs: 8000,
    },
    visibility: "after_restart",
    visibilityNote: "已信任工作区：重启 ZCode 应用后可见",
    warning: null,
    ...overrides,
  };
}

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/session-create-zcode-info")) {
      if (routes.infoStatus) return new Response("no", { status: routes.infoStatus });
      return new Response(JSON.stringify(routes.info ?? infoPayload()), { status: 200 });
    }
    if (url.includes("/session-create-zcode")) {
      // 请求体契约：camelCase，project 必填、firstText 只在实际给值时带上
      const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
      routes.createBody = body;
      if (routes.createStatus) {
        return new Response(JSON.stringify({ error: "headless_disabled" }), {
          status: routes.createStatus,
        });
      }
      return new Response(JSON.stringify(routes.create ?? createPayload()), { status: 200 });
    }
    throw new Error(`未预期的请求: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** 挂载并等表单就绪（info 拉取完成） */
async function mount() {
  installFetch();
  render(<NewSessionForm />);
  await waitFor(() => expect(screen.getByTestId("new-session-form")).toBeTruthy());
}

describe("NewSessionForm（H10 zcode 无头新建）", () => {
  it("工具选择器含 zcode 分组；未接线的工具如实置灰并说明去向", async () => {
    await mount();
    const zcode = await screen.findByTestId("tool-option-zcode");
    expect(zcode.getAttribute("aria-disabled")).toBe("false");
    // 本批只有 zcode 接线：其余工具的按钮必须**禁用**且给出如实原因（不假装能发）
    for (const tool of ["claude", "kimi", "opencode"]) {
      const btn = screen.getByTestId(`tool-option-${tool}`);
      expect(btn.getAttribute("aria-disabled")).toBe("true");
      expect(btn.textContent ?? "").toContain("未接线");
    }
  });

  it("候选列表来自后端并按信任档标注（文案逐字来自后端 note，不谎报重启可见）", async () => {
    await mount();
    const trusted = await screen.findByTestId("candidate-E:/trusted_proj");
    expect(screen.getByTestId("candidate-note-E:/trusted_proj").textContent).toBe(
      "已信任工作区：重启 ZCode 应用后可见"
    );
    expect(trusted.textContent).toContain("重启 ZCode 应用后可见");
    const board = screen.getByTestId("candidate-E:/board_proj");
    expect(screen.getByTestId("candidate-note-E:/board_proj").textContent).toBe(
      "未信任工作区：仅 MAM 可见"
    );
    expect(board.textContent).toContain("仅 MAM 可见");
    // 前端**不带**自己的措辞（复审 Minor 2）：把后端文案换成另一份逐字串，界面跟着变
    cleanup();
    routes = {};
    routes.info = infoPayload({
      candidates: [
        {
          path: "E:/trusted_proj",
          source: "trusted",
          trusted: true,
          note: "后端单点文案（替身）",
        },
      ],
    });
    installFetch();
    render(<NewSessionForm />);
    expect((await screen.findByTestId("candidate-note-E:/trusted_proj")).textContent).toBe(
      "后端单点文案（替身）"
    );
  });

  it("选中候选 + 默认首句 hi 提交；确认回执显示新会话号与可见性提示", async () => {
    await mount();
    fireEvent.click(await screen.findByTestId("candidate-E:/trusted_proj"));
    expect((screen.getByTestId("first-text") as HTMLInputElement).value).toBe("hi");
    fireEvent.click(screen.getByTestId("create-submit"));
    const card = await screen.findByTestId("create-receipt");
    expect(routes.createBody).toEqual({ project: "E:/trusted_proj", firstText: "hi" });
    expect(card.textContent).toContain("sess_22222222-2222-2222-2222-222222222222");
    expect(card.getAttribute("data-confirmation")).toBe("store");
    expect(card.textContent).toContain("会话库已确认新会话");
    expect(card.textContent).toContain("已信任工作区：重启 ZCode 应用后可见");
    expect(card.textContent).toContain("首句已答");
  });

  it("手填路径原样上送（不替用户猜路径）", async () => {
    await mount();
    const manual = await screen.findByTestId("manual-path");
    fireEvent.change(manual, { target: { value: "D:\\projects\\demo" } });
    fireEvent.change(screen.getByTestId("first-text"), { target: { value: "看看这个项目" } });
    fireEvent.click(screen.getByTestId("create-submit"));
    await screen.findByTestId("create-receipt");
    expect(routes.createBody).toEqual({
      project: "D:\\projects\\demo",
      firstText: "看看这个项目",
    });
  });

  it("未确认回执：只渲染后端给出的「未确认」与原因，绝不显示任何会话号", async () => {
    routes.create = createPayload({
      sessionId: "",
      confirmation: "none",
      visibility: undefined,
      visibilityNote: undefined,
      receipt: {
        status: "failed",
        sessionId: "",
        durationMs: 900,
        stage: "channel_error",
        reason: "会话库中该项目未出现新会话——未确认新会话（勿盲目重发）",
      },
    });
    await mount();
    fireEvent.click(await screen.findByTestId("candidate-E:/trusted_proj"));
    fireEvent.click(screen.getByTestId("create-submit"));
    const card = await screen.findByTestId("create-receipt");
    expect(card.getAttribute("data-confirmation")).toBe("none");
    expect(card.textContent).toContain("未确认新会话");
    // 阶段分诊复用 SessionDetail 单点（不另抄词表）
    expect(card.textContent).toContain(HEADLESS_STAGE_TRIAGE.channel_error);
    expect(card.textContent).toContain("勿盲目重发");
    expect(card.textContent).not.toContain("sess_");
    // 未确认时不得出现可见性承诺（什么都没落到工作区）
    expect(card.textContent).not.toContain("重启 ZCode 应用后可见");
  });

  it("黄字信号（同项目已有在册会话）如实渲染且不拦截提交", async () => {
    routes.info = infoPayload({
      warning: "同一项目已有 1 个在册 zcode 会话——本提示**不拦截**本次创建",
    });
    routes.create = createPayload({
      warning: "同一项目已有 1 个在册 zcode 会话——本提示**不拦截**本次创建",
    });
    await mount();
    expect((await screen.findByTestId("create-warning")).textContent).toContain("不拦截");
    fireEvent.click(screen.getByTestId("candidate-E:/trusted_proj"));
    fireEvent.click(screen.getByTestId("create-submit"));
    const card = await screen.findByTestId("create-receipt");
    expect(card.textContent).toContain("不拦截");
  });

  it("总开关关闭：表单置灰 + 显示后端逐字原因，且不给候选", async () => {
    routes.info = {
      available: false,
      reasonCode: "headless_disabled",
      reason: "无头通道未开启，请在电脑端 MAM 设置中开启",
    };
    await mount();
    const gate = await screen.findByTestId("create-disabled");
    expect(gate.textContent).toContain("无头通道未开启，请在电脑端 MAM 设置中开启");
    expect(screen.queryByTestId("candidate-E:/trusted_proj")).toBeNull();
    expect((screen.getByTestId("create-submit") as HTMLButtonElement).disabled).toBe(true);
  });

  it("设备失效（403 → info 为 null）如实提示，不渲染半截表单", async () => {
    routes.infoStatus = 403;
    await mount();
    expect((await screen.findByTestId("create-load-failed")).textContent).toContain("设备");
  });

  /// **跨语言 confirmation 词锁**（复审 Minor 4）：`tests/fixtures/
  /// zcode_create_confirmations.json` 是唯一名单——前端常量与 Rust
  /// `remote::api::confirmation_wire` 各自对照它断言（与 headless_stages.json 同款）。
  it("确认来源常量与跨语言名单逐项一致（后端变体不多不少）", () => {
    const fixture = confirmationsFixture as { confirmations: string[] };
    expect([...ZCODE_CREATE_CONFIRMATIONS].sort()).toEqual([...fixture.confirmations].sort());
    // 未确认是名单里的一员且语义明确：卡面必须按「未确认」渲染（不得凭空补会话号）
    expect(ZCODE_CREATE_CONFIRMATIONS).toContain(
      createPayload({ confirmation: "none", sessionId: "" }).confirmation
    );
  });
});

// 看板入口（Task 12 的最小接线）：H10 表单要能从手机上看板进得去（M9 用户路径），
// 但入口**不改变既有用法**——不传回调的老用法（既有 Board 测试）一个按钮都不多渲染。
describe("新建入口（H10 → D1 汇流：CreateSessionSheet 内的 zcode 选项）", () => {
  function installBoardStubs() {
    class ScriptedEventSource extends MockEventSource {
      constructor(url: string) {
        super(url);
        setTimeout(
          () => this.emit("snapshot", { sessions: [], totalCount: 0, waitingCount: 0 }),
          0
        );
      }
    }
    vi.stubGlobal("EventSource", ScriptedEventSource);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        const u = String(url);
        if (u.includes("/host")) {
          return new Response(
            JSON.stringify({
              host: { name: "JARVIS", platform: "windows", version: "0.5.0", bootId: "b1" },
              enabledTools: [],
            }),
            { status: 200 }
          );
        }
        if (u.includes("/create-projects")) {
          return new Response(JSON.stringify({ projects: [] }), { status: 200 });
        }
        if (u.includes("/session-create-zcode-info")) {
          return new Response(
            JSON.stringify({
              available: true,
              tool: "zcode",
              defaultFirstText: "hi",
              candidates: [],
            }),
            { status: 200 }
          );
        }
        return new Response(JSON.stringify({ sessions: [], totalCount: 0, waitingCount: 0 }), {
          status: 200,
        });
      })
    );
  }

  it("看板新建按钮打开唯一入口壳；工具区含 zcode（无头）选项，选中内嵌 NewSessionForm", async () => {
    installBoardStubs();
    render(
      <Board onPaired={vi.fn()} onUnpaired={vi.fn()} onOpenHistory={vi.fn()} />
    );
    // D1：main 的 create-open 是唯一「新建会话」入口（我们原独立入口已并入）
    fireEvent.click(await screen.findByTestId("create-open"));
    const sheet = await screen.findByTestId("create-sheet");
    expect(sheet).toBeTruthy();
    // zcode 选项在场；选中 → 内嵌无头表单接管（CLI 目录/首句区让位）
    fireEvent.click(screen.getByTestId("create-tool-zcode"));
    await waitFor(() => expect(screen.getByTestId("new-session-form")).toBeTruthy());
    // CLI 表单区让位：目录候选区与 CLI 提交钮（「开始创建」文案）不再在场
    //（create-submit testid 在 NewSessionForm 内嵌后同名单钮属其自身，见上注）
    expect(
      screen.queryAllByTestId("create-submit").filter((b) => b.textContent === "开始创建")
    ).toHaveLength(0);
    // 返回 → 回到工具选择（表单退场）
    fireEvent.click(screen.getByTestId("new-session-back"));
    await waitFor(() => expect(screen.queryByTestId("new-session-form")).toBeNull());
    expect(screen.getByTestId("create-tool-zcode")).toBeTruthy();
  });
});
