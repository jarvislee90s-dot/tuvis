import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ApproveCard from "@/mobile/ApproveCard";
import type { ApproveOptionsView } from "@/mobile/api";

// M8 Task 12：移动端红卡审批选项卡。fetch 全量 stub（盖过 setup.ts 的 msw），
// 按 URL 分路到 approve-options / approve 两族端点（注意前缀包含关系：
// /session-approve-options ⊃ /session-approve，长路径必须先判——MessageComposer
// 测试同款教训）。组件挂载即拉选项，用例先 findBy 选项按钮就绪再交互。

/** 审批选项夹具（GET /session-approve-options 载荷；选项只含 id+label——
 *  键位是投递层机密不外泄 UI，组件契约只消费 id/label） */
function approveOptions(overrides: Partial<ApproveOptionsView> = {}): ApproveOptionsView {
  return {
    available: true,
    options: [
      { id: "approve", label: "允许" },
      { id: "reject", label: "拒绝" },
    ],
    verifiedWith: "claude 1.2.3 (2026-09-18 实测)",
    currentVersion: "claude 1.2.3",
    drift: false,
    ...overrides,
  };
}

interface Routes {
  options?: ApproveOptionsView;
  optionsStatus?: number;
  approve?: Record<string, unknown>;
  approveStatus?: number;
  approveBody?: Record<string, unknown>;
  /** 2026-10-04 计划批准卡：/session-plan-feedback 回执（缺省 editor_ready） */
  planFeedback?: Record<string, unknown>;
  planFeedbackStatus?: number;
  planFeedbackBody?: Record<string, unknown>;
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

/** 按 URL 分路的 fetch stub（判序：/session-approve-options 在前，避免被
 *  /session-approve 前缀误吞） */
function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-approve-options")) {
      if (routes.optionsStatus) return new Response("no", { status: routes.optionsStatus });
      return new Response(JSON.stringify(routes.options ?? approveOptions()), { status: 200 });
    }
    if (url.includes("/session-plan-feedback")) {
      if (routes.planFeedbackStatus) {
        return new Response(JSON.stringify(routes.planFeedbackBody ?? { error: "internal" }), {
          status: routes.planFeedbackStatus,
        });
      }
      return new Response(JSON.stringify(routes.planFeedback ?? { status: "editor_ready" }), {
        status: 200,
      });
    }
    if (url.includes("/session-approve")) {
      if (routes.approveStatus) {
        return new Response(JSON.stringify(routes.approveBody ?? { error: "internal" }), {
          status: routes.approveStatus,
        });
      }
      return new Response(JSON.stringify(routes.approve ?? { status: "key_sent" }), {
        status: 200,
      });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** POST /session-approve 的调用（URL 精确到 /session-approve 结尾，排除 -options 前缀） */
function approveCalls(): Array<Array<unknown>> {
  return fetchMock.mock.calls.filter((c: unknown[]) => /\/session-approve$/.test(String(c[0])));
}

/** 放行 mock fetch 的 promise 链（若干轮微任务冲刷，足以走完 fetch→json→setState） */
async function flushAsync() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

describe("ApproveCard：红卡审批选项卡（M8 Task 12）", () => {
  it("available=true：「允许」「拒绝」按钮按 label 渲染（载荷无 key 字段，组件只消费 id/label）", async () => {
    installFetch();
    routes.options = approveOptions();
    const { container } = render(<ApproveCard session={{ id: "sess-1" }} />);
    expect(await screen.findByTestId("approve-option-approve")).toBeTruthy();
    expect(screen.getByTestId("approve-option-approve").textContent).toContain("允许");
    expect(screen.getByTestId("approve-option-reject").textContent).toContain("拒绝");
    expect(screen.getByTestId("approve-card").textContent).toContain("等待批准");
    // 契约锚点：响应载荷不含 key（键位不外泄 UI 语义字段），组件仅凭 id/label 完整渲染
    expect(routes.options.options.every((o) => !("key" in o))).toBe(true);
    // 无漂移时提示条不出现
    expect(container.querySelector("[data-testid='approve-drift']")).toBeNull();
  });

  it('点「允许」：POST body {sessionId, optionId:"approve"} → 进入「已发送按键」态（按钮禁用）', async () => {
    installFetch();
    routes.options = approveOptions();
    routes.approve = { status: "key_sent" };
    render(<ApproveCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-sent").then((el) => el.textContent)).toBe(
      "已发送按键"
    );
    expect(approveCalls()).toHaveLength(1);
    expect(JSON.parse(String((approveCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      optionId: "approve",
    });
    // 终态：选项按钮禁用（防重复应答）
    expect((screen.getByTestId("approve-option-approve") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("approve-option-reject") as HTMLButtonElement).disabled).toBe(true);
  });

  it("available=false：组件不渲染（container empty，卡自身自隐）", async () => {
    installFetch();
    routes.options = approveOptions({ available: false, options: [] });
    const { container } = render(<ApproveCard session={{ id: "sess-1" }} />);
    await flushAsync(); // 选项落地（available=false）后依旧不渲染
    expect(container.firstElementChild).toBeNull();
  });

  it("drift=true：提示条「映射待实测确认，若提示不符请用普通发送」出现", async () => {
    installFetch();
    routes.options = approveOptions({ drift: true, currentVersion: "claude 2.0.0" });
    render(<ApproveCard session={{ id: "sess-1" }} />);
    expect(await screen.findByTestId("approve-drift").then((el) => el.textContent)).toContain(
      "映射待实测确认，若提示不符请用普通发送"
    );
  });

  it("failed{error}：错误文案展示且可重试（重试转「已发送按键」）", async () => {
    installFetch();
    routes.options = approveOptions();
    routes.approve = { status: "failed", error: "该会话投递进行中，请稍后重试" };
    render(<ApproveCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-error").then((el) => el.textContent)).toContain(
      "该会话投递进行中，请稍后重试"
    );
    // 可重试：按钮保持可点，修正路由后重按即重试成功
    expect((screen.getByTestId("approve-option-approve") as HTMLButtonElement).disabled).toBe(
      false
    );
    routes.approve = { status: "key_sent" };
    fireEvent.click(screen.getByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-sent")).toBeTruthy();
    expect(approveCalls()).toHaveLength(2);
  });

  it("ApiError 409 not_waiting：中文文案「会话不在等待状态」", async () => {
    installFetch();
    routes.options = approveOptions();
    routes.approveStatus = 409;
    routes.approveBody = { error: "not_waiting" };
    render(<ApproveCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-error").then((el) => el.textContent)).toContain(
      "会话不在等待状态"
    );
    expect(approveCalls()).toHaveLength(1);
  });

  it("ApiError 404 no_mapping：降级文案「该工具暂不支持审批应答，请用普通发送」", async () => {
    installFetch();
    routes.options = approveOptions();
    routes.approveStatus = 404;
    routes.approveBody = { error: "no_mapping" };
    render(<ApproveCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-error").then((el) => el.textContent)).toContain(
      "该工具暂不支持审批应答，请用普通发送"
    );
    expect(approveCalls()).toHaveLength(1);
  });
});

// ==== M9R 注入加固前端对齐（严格档 reason 提示条 / P2-10 no_session 文案补锁）====
describe("ApproveCard：M9R 严格档与补锁", () => {
  it("probe_pending_options_show_hint_only：available=false 且后端下发 reason（严格档）→ 卡片只渲染提示条不渲染按键组", async () => {
    installFetch();
    routes.options = approveOptions({
      available: false,
      reason: "键位待实测确认，请用普通发送",
    });
    render(<ApproveCard session={{ id: "sess-1" }} />);
    // 提示条在场：后端中文 reason 原文内联展示
    expect(await screen.findByTestId("approve-hint").then((el) => el.textContent)).toBe(
      "键位待实测确认，请用普通发送"
    );
    expect(screen.getByTestId("approve-card")).toBeTruthy();
    // 严格档要点：不渲染任何按键（键位未实测，防误发）
    expect(screen.queryByTestId("approve-option-approve")).toBeNull();
    expect(screen.queryByTestId("approve-option-reject")).toBeNull();
  });

  it("approve_404_no_session_copy：session-approve 404 no_session → 中文文案「会话已结束，请返回看板刷新」（P2-10 补锁）", async () => {
    installFetch();
    routes.options = approveOptions();
    routes.approveStatus = 404;
    routes.approveBody = { error: "no_session" };
    render(<ApproveCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("approve-option-approve"));
    expect(await screen.findByTestId("approve-error").then((el) => el.textContent)).toContain(
      "会话已结束"
    );
    expect(approveCalls()).toHaveLength(1);
  });
});

// ==== 批次丙 T5：N 选项审批对话框（屏读解析出的真实选项）====
describe("ApproveCard：N 选项对话框模式（T5）", () => {
  it("dialog=true：渲染编号按钮组（真实选项文本 + 编号徽标），点按提交 dialog:<n>", async () => {
    installFetch();
    routes.options = approveOptions({
      dialog: true,
      options: [
        { id: "dialog:1", label: "Yes, and use auto mode" },
        { id: "dialog:2", label: "Yes, manually approve edits" },
        { id: "dialog:3", label: "Tell Claude what to do differently" },
      ],
    });
    render(<ApproveCard session={{ id: "sess-dlg" }} />);
    // 卡在场 + 对话框模式标记 + 说明行
    const card = await screen.findByTestId("approve-card");
    expect(card.getAttribute("data-mode")).toBe("dialog");
    expect(screen.getByTestId("approve-dialog-label").textContent).toContain("终端对话框");
    // 三个真实选项文本可见（图2/图3 的修复目标：不降级成二元）
    expect(screen.getByTestId("approve-option-dialog:1").textContent).toContain(
      "Yes, and use auto mode"
    );
    expect(screen.getByTestId("approve-option-dialog:3").textContent).toContain(
      "Tell Claude what to do differently"
    );
    // 编号徽标 = 将注入的数字键（所见即所按）
    expect(screen.getByTestId("approve-option-dialog:2").textContent).toContain("2");
    // 点第 3 项 → POST optionId = "dialog:3"
    fireEvent.click(screen.getByTestId("approve-option-dialog:3"));
    await flushAsync();
    expect(approveCalls()).toHaveLength(1);
    const body = JSON.parse(String((approveCalls()[0][1] as RequestInit).body));
    expect(body).toEqual({ sessionId: "sess-dlg", optionId: "dialog:3" });
    expect(await screen.findByTestId("approve-sent")).toBeTruthy();
  });

  it("dialog 缺省：维持二元渲染（前向兼容旧后端）", async () => {
    installFetch();
    routes.options = approveOptions(); // 无 dialog 字段
    render(<ApproveCard session={{ id: "sess-bin" }} />);
    expect((await screen.findByTestId("approve-card")).getAttribute("data-mode")).toBe("binary");
    expect(screen.getByTestId("approve-card").textContent).toContain("等待批准");
    expect(screen.queryByTestId("approve-dialog-label")).toBeNull();
    // 二元项照常可点
    expect(screen.getByTestId("approve-option-approve")).toBeTruthy();
  });
});

// ==== 批次丙 T8：审批点 plan 聚合（计划确认卡带计划全文）====
describe("ApproveCard：审批点 plan 聚合（T8）", () => {
  it("plan 为 markdown：卡片主体渲染计划全文（markdown 直出）+ 选项照常可点", async () => {
    installFetch();
    routes.options = approveOptions({
      dialog: true,
      options: [
        { id: "dialog:1", label: "Yes, and use auto mode" },
        { id: "dialog:2", label: "No, keep planning" },
      ],
      plan: { content: "# 实施计划\n\n- 第一步\n- 第二步", isFile: false },
    });
    render(<ApproveCard session={{ id: "sess-plan" }} />);
    const planBox = await screen.findByTestId("approve-plan");
    expect(planBox.getAttribute("data-plan-file")).toBe("false");
    // markdown 结构化渲染（# → H1，- → LI）
    expect(screen.getByText("实施计划").tagName).toBe("H1");
    expect(screen.getByText("第一步").tagName).toBe("LI");
    // 选项仍在（计划主体 + 选项并存）
    expect(screen.getByTestId("approve-option-dialog:1")).toBeTruthy();
  });

  it("plan 为文件路径（kimi）：显「计划文件」+ 路径文本，不渲染 markdown", async () => {
    installFetch();
    routes.options = approveOptions({
      plan: { content: "/w/.kimi-code/sessions/x/agents/main/plans/p.md", isFile: true },
    });
    render(<ApproveCard session={{ id: "sess-pf" }} />);
    const planBox = await screen.findByTestId("approve-plan");
    expect(planBox.getAttribute("data-plan-file")).toBe("true");
    expect(planBox.textContent).toContain("计划文件");
    expect(screen.getByTestId("approve-plan-file").textContent).toContain("plans/p.md");
  });

  it("plan 为 null/缺省：不渲染计划主体（降级——只渲染选项，不阻塞审批）", async () => {
    installFetch();
    routes.options = approveOptions(); // 无 plan 字段
    render(<ApproveCard session={{ id: "sess-noplan" }} />);
    await screen.findByTestId("approve-option-approve");
    expect(screen.queryByTestId("approve-plan")).toBeNull();
  });
});

// ==== 批次丙 R1-3：降级二元卡的防重警示（计划红线 3）====
describe("ApproveCard：降级警示脚注（R1-3）", () => {
  it("degradedHint 在场：二元卡渲染警示脚注（终端可能是多选对话框）", async () => {
    installFetch();
    routes.options = approveOptions({
      degradedHint: "未读到终端对话框选项——终端可能正显示多选项，二元键可能错位，建议到终端确认",
    });
    render(<ApproveCard session={{ id: "sess-deg" }} />);
    const hint = await screen.findByTestId("approve-degraded-hint");
    expect(hint.textContent).toContain("未读到终端对话框选项");
    expect(hint.textContent).toContain("建议到终端确认");
    // **降级禁用**（2026-10-10 用户裁决「读不到就别给钮」+ 评审 Important 4 锁）：
    // 按钮在场但禁用——多选面板误读成审批框时点「允许」会真实发键（21:55 事故）
    const allow = screen.getByTestId("approve-option-approve") as HTMLButtonElement;
    const reject = screen.getByTestId("approve-option-reject") as HTMLButtonElement;
    expect(allow.disabled).toBe(true);
    expect(reject.disabled).toBe(true);
    // 降级态辅助面：手动检查钮 + 核对中状态文案
    expect(screen.getByTestId("approve-degraded-check")).toBeTruthy();
    expect(screen.getByTestId("approve-degraded-state").textContent).toContain("核对");
  });

  it("degradedHint=null/缺省：不渲染脚注（未降级零变化）", async () => {
    installFetch();
    routes.options = approveOptions({ degradedHint: null });
    render(<ApproveCard session={{ id: "sess-nodeg" }} />);
    await screen.findByTestId("approve-option-approve");
    expect(screen.queryByTestId("approve-degraded-hint")).toBeNull();
  });

  it("降级态自动重读：2.5s 后重拉选项端点，读到选项即恢复可用（fake timers）", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      installFetch();
      // 首拉 = 降级态；重拉 = 读到对话框选项（降级解除）——按调用次数翻转
      const degraded = approveOptions({
        degradedHint: "未读到终端对话框选项——终端可能正显示多选项，二元键可能错位，建议到终端确认",
      });
      const recovered = approveOptions({
        dialog: true,
        options: [
          { id: "dialog:1", label: "Yes, and use auto mode" },
          { id: "dialog:2", label: "No" },
        ],
      });
      let optionsGets = 0;
      const prev = routes.options;
      Object.defineProperty(routes, "options", {
        configurable: true,
        get() {
          optionsGets += 1;
          return optionsGets === 1 ? degraded : recovered;
        },
      });
      render(<ApproveCard session={{ id: "sess-deg-retry" }} />);
      const allow = await screen.findByTestId("approve-option-approve");
      expect((allow as HTMLButtonElement).disabled).toBe(true);

      // 快进 2.5s → 自动重拉触发（fetch 的 options 端点被再次调用）
      await vi.advanceTimersByTimeAsync(2600);
      expect(optionsGets).toBeGreaterThanOrEqual(2);
      // 重拉读到选项 → 按钮恢复可用（dialog 编号选项形态）
      const dialogOpt = await screen.findByTestId("approve-option-dialog:1");
      expect((dialogOpt as HTMLButtonElement).disabled).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("降级态自动重读：仍读不到选项 → 按钮保持禁用且显示未读到文案", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      installFetch();
      routes.options = approveOptions({
        degradedHint: "未读到终端对话框选项——终端可能正显示多选项，二元键可能错位，建议到终端确认",
      });
      render(<ApproveCard session={{ id: "sess-deg-still" }} />);
      const allow = await screen.findByTestId("approve-option-approve");
      expect((allow as HTMLButtonElement).disabled).toBe(true);
      await vi.advanceTimersByTimeAsync(2600);
      // 重拉是异步 promise——用 waitFor 等 state 文案翻转（同步 getBy 会抢在冲刷前）
      await screen.findByText(/仍未读到/);
      expect(screen.getByTestId("approve-degraded-state").textContent).toContain("仍未读到");
      expect((screen.getByTestId("approve-option-approve") as HTMLButtonElement).disabled).toBe(
        true
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("对话框模式（dialog=true）不渲染降级脚注（读到选项即未降级）", async () => {
    installFetch();
    routes.options = approveOptions({
      dialog: true,
      options: [
        { id: "dialog:1", label: "Yes, and use auto mode" },
        { id: "dialog:2", label: "No" },
      ],
    });
    render(<ApproveCard session={{ id: "sess-ok" }} />);
    await screen.findByTestId("approve-option-dialog:1");
    expect(screen.queryByTestId("approve-degraded-hint")).toBeNull();
  });
});

// ==== 丁T2：计划待确认（planPending——codex/kimi 的计划确认框入口）====
describe("ApproveCard：计划待确认条（丁T2）", () => {
  /** 计划待确认载荷：可用但无键（y/esc 是补丁审批键位，对计划框未取证） */
  function planPendingOptions(over: Partial<ApproveOptionsView> = {}): ApproveOptionsView {
    return approveOptions({
      available: true,
      options: [],
      planPending: true,
      plan: { content: "# 计划正文\n\n- 第一步", isFile: false },
      ...over,
    });
  }

  it("planPending=true + 零选项：渲染计划待确认条 +「检查终端对话框」按钮，不出任何键位按钮", async () => {
    installFetch();
    routes.options = planPendingOptions();
    render(<ApproveCard session={{ id: "sess-pp" }} />);
    const bar = await screen.findByTestId("approve-plan-pending");
    expect(bar.textContent).toContain("终端正在等待这个计划的确认");
    // 卡标题（红卡语义：这是审批卡，不是普通消息）
    expect(screen.getByTestId("approve-card").textContent).toContain("计划待确认");
    // 检查按钮在场（用户点它 = 重拉一次选项；后端屏读命中即出 N 选项）
    expect(screen.getByTestId("approve-plan-check")).toBeTruthy();
    // 零键位按钮（不得下发映射表二元键——那是补丁审批的键位）
    expect(screen.queryByTestId("approve-option-approve")).toBeNull();
    expect(screen.queryByTestId("approve-option-reject")).toBeNull();
    // 计划全文照常聚合（点检查前用户先看到计划）
    expect(screen.getByTestId("approve-plan").textContent).toContain("计划正文");
  });

  it("点「检查终端对话框」：重拉一次选项端点；未命中时给降级提示（不假装成功）", async () => {
    installFetch();
    routes.options = planPendingOptions();
    render(<ApproveCard session={{ id: "sess-pp2" }} />);
    await screen.findByTestId("approve-plan-pending");
    // 检查前无未命中提示（用户还没点过——提示只在点过之后出现）
    expect(screen.queryByTestId("approve-plan-check-miss")).toBeNull();
    fireEvent.click(screen.getByTestId("approve-plan-check"));
    await flushAsync();
    // 重拉发生（挂载 1 次 + 检查 1 次）
    const optionsCalls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-approve-options")
    );
    expect(optionsCalls.length).toBeGreaterThanOrEqual(2);
    // 屏读仍没读到选项 → 降级提示（不假装成功）
    expect((await screen.findByTestId("approve-plan-check-miss")).textContent).toContain(
      "未读到终端对话框选项"
    );
    // **检查未命中即清除预期态**（任务书语义）：待确认条不再显示（「终端正在等」是
    // 未经验证的声明，§2.8 不假装）；检查钮保留（可再试）+ 计划正文保留（内容仍真实）
    expect(screen.queryByTestId("approve-plan-pending")).toBeNull();
    expect(screen.getByTestId("approve-plan-check")).toBeTruthy();
    expect(screen.getByTestId("approve-plan")).toBeTruthy();
  });

  it("检查后屏读命中（dialog 选项）：切到 N 选项编号按钮组，待确认条消失", async () => {
    installFetch();
    routes.options = planPendingOptions();
    render(<ApproveCard session={{ id: "sess-pp3" }} />);
    await screen.findByTestId("approve-plan-pending");
    // 第二次拉取（点检查后）返回屏读选项
    routes.options = approveOptions({
      dialog: true,
      options: [
        { id: "dialog:1", label: "Yes, implement this plan" },
        { id: "dialog:2", label: "Yes, clear context and implement" },
        { id: "dialog:3", label: "No, stay in Plan mode" },
      ],
    });
    fireEvent.click(screen.getByTestId("approve-plan-check"));
    const opt = await screen.findByTestId("approve-option-dialog:1");
    expect(opt.textContent).toContain("Yes, implement this plan");
    expect(screen.queryByTestId("approve-plan-pending")).toBeNull();
    // 选项卡点按 → POST dialog:1（codex 走数字直选，后端分发）
    fireEvent.click(opt);
    await flushAsync();
    expect(JSON.parse(String((approveCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-pp3",
      optionId: "dialog:1",
    });
  });

  it("planPending 缺省/ false：不渲染待确认条（前向兼容旧后端 + 普通审批零变化）", async () => {
    installFetch();
    routes.options = approveOptions();
    render(<ApproveCard session={{ id: "sess-pp0" }} />);
    await screen.findByTestId("approve-option-approve");
    expect(screen.queryByTestId("approve-plan-pending")).toBeNull();
  });

  it("planPending=true 但已有 dialog 选项：选项卡优先（待确认条不与选项并存）", async () => {
    installFetch();
    routes.options = planPendingOptions({
      dialog: true,
      options: [{ id: "dialog:1", label: "Approve" }],
    });
    render(<ApproveCard session={{ id: "sess-pp4" }} />);
    await screen.findByTestId("approve-option-dialog:1");
    expect(screen.queryByTestId("approve-plan-pending")).toBeNull();
  });
});

// ==== 丁T2 复评 F3-4：kimi 审批卡不含 plan 正文（任务书成文要求）====
describe("ApproveCard：F3-4 kimi 审批卡不带 plan 正文", () => {
  it("kimi 载荷 plan=null（后端 F3-4 收口）→ 不渲染计划主体，选项照常", async () => {
    installFetch();
    routes.options = approveOptions({
      dialog: true,
      options: [{ id: "dialog:1", label: "Approve" }],
      plan: null,
    });
    render(<ApproveCard session={{ id: "sess-kimi-noplan" }} />);
    await screen.findByTestId("approve-option-dialog:1");
    expect(screen.queryByTestId("approve-plan")).toBeNull();
    expect(screen.queryByTestId("approve-plan-file")).toBeNull();
  });

  it("kimi 计划待确认条（planPending）同样不带正文 → 只渲染条 + 检查钮", async () => {
    installFetch();
    routes.options = approveOptions({
      available: true,
      options: [],
      planPending: true,
      plan: null,
    });
    render(<ApproveCard session={{ id: "sess-kimi-pp" }} />);
    expect(await screen.findByTestId("approve-plan-pending")).toBeTruthy();
    expect(screen.getByTestId("approve-plan-check")).toBeTruthy();
    expect(screen.queryByTestId("approve-plan")).toBeNull();
  });

  it("对照：claude/codex 的 plan 正文照常渲染（F3-4 只收 kimi，不误伤）", async () => {
    installFetch();
    routes.options = approveOptions({
      plan: { content: "# 计划正文", isFile: false },
    });
    render(<ApproveCard session={{ id: "sess-claude-plan" }} />);
    expect((await screen.findByTestId("approve-plan")).textContent).toContain("计划正文");
  });
});

// ==== 批次戊 E2③：裁12 布局契约（卡体高度上限 + 长内容默认折叠 + 点开）====
describe("ApproveCard：裁12 计划正文折叠", () => {
  /** 构造超过折叠阈值的计划正文（阈值 600 字符，见组件 PLAN_COLLAPSE_CHARS） */
  const longPlan = "# 长计划\n\n" + "正文段落，用于撑破折叠阈值。".repeat(60);

  it("长内容默认折叠：data-collapsed=true + 渲染「展开全文」按钮", async () => {
    installFetch();
    routes.options = approveOptions({ plan: { content: longPlan, isFile: false } });
    render(<ApproveCard session={{ id: "sess-e2-fold" }} />);
    const body = await screen.findByTestId("approve-plan");
    expect(body.getAttribute("data-collapsed")).toBe("true");
    expect(body.className).toContain("max-h-24");
    const toggle = screen.getByTestId("approve-plan-toggle");
    expect(toggle.textContent).toBe("展开全文");
  });

  it("点开切换：展开后 data-collapsed=false + max-h-64 内滚 + 按钮变「收起计划」", async () => {
    installFetch();
    routes.options = approveOptions({ plan: { content: longPlan, isFile: false } });
    render(<ApproveCard session={{ id: "sess-e2-open" }} />);
    await screen.findByTestId("approve-plan");
    await act(async () => {
      fireEvent.click(screen.getByTestId("approve-plan-toggle"));
    });
    const body = screen.getByTestId("approve-plan");
    expect(body.getAttribute("data-collapsed")).toBe("false");
    expect(body.className).toContain("max-h-64");
    expect(screen.getByTestId("approve-plan-toggle").textContent).toBe("收起计划");
  });

  it("短内容不折叠：无按钮、data-collapsed=false（完整渲染）", async () => {
    installFetch();
    routes.options = approveOptions({ plan: { content: "# 短计划\n\n一两行。", isFile: false } });
    render(<ApproveCard session={{ id: "sess-e2-short" }} />);
    const body = await screen.findByTestId("approve-plan");
    expect(body.getAttribute("data-collapsed")).toBe("false");
    expect(screen.queryByTestId("approve-plan-toggle")).toBeNull();
  });
});

// ==== 批次戊 E7：裁11 配色（对话框卡并入 sky 蓝系；二元审批保留红系）====
describe("ApproveCard：裁11 配色 tone 映射", () => {
  it("对话框卡：data-tone=question（sky 蓝系）+ 选项按钮用蓝族 token", async () => {
    installFetch();
    routes.options = approveOptions({
      dialog: true,
      options: [
        { id: "dialog:1", label: "Yes, and use auto mode" },
        { id: "dialog:2", label: "Yes, manually approve edits" },
      ],
    });
    render(<ApproveCard session={{ id: "sess-e7-dialog" }} />);
    const card = await screen.findByTestId("approve-card");
    expect(card.getAttribute("data-tone")).toBe("question");
    expect(card.className).toContain("border-sky-500/60");
    const btn = screen.getByTestId("approve-option-dialog:1");
    expect(btn.className).toContain("bg-sky-500/10");
    expect(btn.className).not.toContain("bg-rose-500/10");
  });

  it("二元审批卡：并入问答同族蓝系 data-tone=question + 纵向列表（2026-09-24 用户裁决「完全并成一套」，裁11 活口收口）；徽标只在 dialog 分支渲染（评审 I2：二元键位各家不同且不外泄，编造序号=谎报）", async () => {
    installFetch();
    routes.options = approveOptions();
    render(<ApproveCard session={{ id: "sess-e7-binary" }} />);
    const card = await screen.findByTestId("approve-card");
    expect(card.getAttribute("data-tone")).toBe("question");
    expect(card.className).toContain("border-sky-500/60");
    const btn = screen.getByTestId("approve-option-approve");
    expect(btn.className).toContain("bg-sky-500/10");
    expect(btn.className).not.toContain("bg-rose-500/10");
    // 二元分支不编造编号徽标：按钮文本 = 纯 label（不夹带序号/键名）
    expect(screen.getByTestId("approve-option-approve").textContent).toBe("允许");
    expect(screen.getByTestId("approve-option-reject").textContent).toBe("拒绝");
  });
});

// ==== 2026-10-04 计划批准卡：planDialog 形态 + 反馈入口 ====
describe("ApproveCard：计划批准卡（2026-10-04）", () => {
  /** claude 计划批准框夹具：屏读三选项 + 账本反馈锚命中 dialog:3 */
  function planDialogOptions(): ApproveOptionsView {
    return approveOptions({
      dialog: true,
      planDialog: true,
      feedbackOption: "dialog:3",
      options: [
        { id: "dialog:1", label: "Yes, and use auto mode" },
        { id: "dialog:2", label: "Yes, manually approve edits" },
        { id: "dialog:3", label: "Tell Claude what to change" },
      ],
    });
  }

  it("planDialog 形态：标题「计划批准」+ 反馈项渲染为入口（其余项照常渲染）", async () => {
    installFetch();
    routes.options = planDialogOptions();
    render(<ApproveCard session={{ id: "sess-plan-1" }} />);
    expect(
      await screen
        .findByTestId("approve-card")
        .then((el) => el.textContent)
        .then((t) => t?.includes("计划批准")),
    ).toBe(true);
    // 反馈项 = 独立 testid 入口（不直发按键）
    expect(screen.getByTestId("approve-option-feedback").textContent).toContain(
      "Tell Claude what to change",
    );
    // 其余选项照常（dialog 编号 id 形态）
    expect(screen.getByTestId("approve-option-dialog:1")).toBeTruthy();
    expect(screen.getByTestId("approve-option-dialog:2")).toBeTruthy();
  });

  it('点反馈项：POST /session-approve {optionId:"dialog:3"} → key_sent 后上报 onPlanFeedbackReady（零 plan-feedback 调用）', async () => {
    installFetch();
    routes.options = planDialogOptions();
    routes.approve = { status: "key_sent" };
    const onReady = vi.fn();
    render(<ApproveCard session={{ id: "sess-plan-2" }} onPlanFeedbackReady={onReady} />);
    fireEvent.click(await screen.findByTestId("approve-option-feedback"));
    await flushAsync();
    expect(onReady).toHaveBeenCalledTimes(1);
    // 选 3 = 走既有审批端点（探测定案 2026-10-04 §S3：选 3 = 回空 composer）
    expect(approveCalls()).toHaveLength(1);
    expect(JSON.parse(String((approveCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-plan-2",
      optionId: "dialog:3",
    });
    const fbCalls = fetchMock.mock.calls.filter((c: unknown[]) =>
      String(c[0]).includes("/session-plan-feedback"),
    );
    expect(fbCalls).toHaveLength(0);
  });

  it("选 3 失败（failed 回执）：显示错误文案、不上报父级（按钮保持可点可重试）", async () => {
    installFetch();
    routes.options = planDialogOptions();
    routes.approve = { status: "failed", error: "对话框已不在场，请到终端核对" };
    const onReady = vi.fn();
    render(<ApproveCard session={{ id: "sess-plan-3" }} onPlanFeedbackReady={onReady} />);
    fireEvent.click(await screen.findByTestId("approve-option-feedback"));
    expect(
      await screen
        .findByTestId("approve-error")
        .then((el) => el.textContent)
        .then((t) => t?.includes("对话框已不在场")),
    ).toBe(true);
    expect(onReady).not.toHaveBeenCalled();
    expect((screen.getByTestId("approve-option-feedback") as HTMLButtonElement).disabled).toBe(
      false,
    );
  });
});
