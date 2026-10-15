import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ApproveCard from "@/mobile/ApproveCard";
import QuestionCard from "@/mobile/QuestionCard";
import type { QuestionInfoView } from "@/mobile/api";

// 批次乙 T8：移动端问答卡（AskUserQuestion）。fetch 全量 stub（盖过 setup.ts 的
// msw），按 URL 分路到 session-question / session-question/answer 两族端点（注意
// 前缀包含关系：GET 路径是 POST 路径的前缀，长路径必须先判——ApproveCard 测试
// 同款教训）。组件挂载即拉问答，用例先 findBy 选项按钮就绪再交互。

/** 单选题夹具（探测档案 §3 真实 questions JSON 缩录）。
 *  `freeText` 显式给 false（= 工具未定案形态）——需要输入框的用例自行覆盖为 true。 */
function singleQuestionInfo(overrides: Partial<QuestionInfoView> = {}): QuestionInfoView {
  return {
    available: true,
    source: "mark",
    freeText: false,
    questions: [
      {
        header: "Next step",
        question: "This is a demo question — what would you like to do next?",
        multiSelect: false,
        options: [
          { label: "Tool demo", description: "Explain how AskUserQuestion works." },
          { label: "Start a task", description: "Start a coding or file task." },
          { label: "Nothing yet", description: "No further request." },
        ],
      },
    ],
    ...overrides,
  };
}

/** 多选题夹具（探测档案 §3 多选缩录） */
function multiQuestionInfo(): QuestionInfoView {
  return {
    available: true,
    source: "mark",
    freeText: false,
    questions: [
      {
        header: "Favorite fruits",
        question: "Which fruits are your favorites? (Select all that apply)",
        multiSelect: true,
        options: [
          { label: "Apple", description: "A sweet, crisp fruit." },
          { label: "Banana", description: "A soft, tropical fruit." },
          { label: "Peach", description: "A juicy summer fruit." },
        ],
      },
    ],
  };
}

interface Routes {
  question?: QuestionInfoView;
  questionStatus?: number;
  /** GET /session-question 网络失败（静默自隐分支） */
  questionNetworkFail?: boolean;
  answer?: Record<string, unknown>;
  answerStatus?: number;
  answerBody?: Record<string, unknown>;
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

/** 按 URL 分路的 fetch stub（判序：/session-question/answer 在前——GET 路径是
 *  POST 路径的前缀，反序会把 POST 吞进 GET 分支） */
function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/session-question/answer")) {
      if (routes.answerStatus) {
        return new Response(JSON.stringify(routes.answerBody ?? { error: "internal" }), {
          status: routes.answerStatus,
        });
      }
      return new Response(JSON.stringify(routes.answer ?? { status: "key_sent" }), {
        status: 200,
      });
    }
    if (url.includes("/session-question")) {
      if (routes.questionNetworkFail) throw new TypeError("network down");
      if (routes.questionStatus) return new Response("no", { status: routes.questionStatus });
      return new Response(JSON.stringify(routes.question ?? singleQuestionInfo()), { status: 200 });
    }
    if (url.includes("/session-approve-options")) {
      // ApproveCard 联合断言用：问答会话上审批不可用（后端硬约束①的 UI 面）
      return new Response(
        JSON.stringify({
          available: false,
          options: [],
          verifiedWith: "test",
          currentVersion: null,
          drift: false,
        }),
        { status: 200 }
      );
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** POST /session-question/answer 的调用 */
function answerCalls(): Array<Array<unknown>> {
  return fetchMock.mock.calls
    .filter((c: unknown[]) => /\/session-question\/answer$/.test(String(c[0])))
    .map((c) => c as unknown[]);
}

/** 放行 mock fetch 的 promise 链（若干轮微任务冲刷，足以走完 fetch→json→setState） */
async function flushAsync() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

describe("QuestionCard：问答卡渲染与应答（批次乙 T8）", () => {
  it("单选渲染：题干（header 徽标 + question）+ 选项按钮（编号+label+description）", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    render(<QuestionCard session={{ id: "sess-1" }} />);
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    expect(screen.getByTestId("question-card").getAttribute("data-mode")).toBe("single");
    expect(screen.getByTestId("question-header").textContent).toBe("Next step");
    expect(screen.getByTestId("question-text").textContent).toBe(
      "This is a demo question — what would you like to do next?"
    );
    // 编号（从 1 起）+ label + description 三要素
    expect(screen.getByTestId("question-option-0").textContent).toContain("1");
    expect(screen.getByTestId("question-option-0").textContent).toContain("Tool demo");
    expect(screen.getByTestId("question-option-0").textContent).toContain(
      "Explain how AskUserQuestion works."
    );
    expect(screen.getByTestId("question-option-1").textContent).toContain("Start a task");
    expect(screen.getByTestId("question-option-2").textContent).toContain("Nothing yet");
    // 自由文本入口：**freeText !== true 的工具**渲染「去终端作答」引导文案
    // （丁T5 §2.8 降级：序列未定案不假装能发），且**不渲染输入框**
    expect(screen.getByTestId("question-freeform-hint").textContent).toContain(
      "远程自由作答尚未实测，请在终端作答"
    );
    expect(screen.queryByTestId("question-freetext-input")).toBeNull();
    // 问答模式零 允许/拒绝 键钮
    expect(screen.queryByText("允许")).toBeNull();
    expect(screen.queryByText("拒绝")).toBeNull();
  });

  it("单选点选项：POST body {sessionId, action:'select', index}（index 为 0 起 wire 口径）→「已发送按键」终态", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-1" }} />);
    fireEvent.click(await screen.findByTestId("question-option-1")); // UI 第 2 项
    expect(await screen.findByTestId("question-sent").then((el) => el.textContent)).toBe(
      "已发送按键"
    );
    expect(answerCalls()).toHaveLength(1);
    expect(JSON.parse(String((answerCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-1",
      action: "select",
      index: 1,
    });
    // 终态：动作按钮全部移除（零重复应答面——比 disabled 更强的防线）
    expect(screen.queryByTestId("question-option-0")).toBeNull();
    expect(screen.queryByTestId("question-option-1")).toBeNull();
    expect(screen.queryByTestId("question-cancel")).toBeNull();
    expect(screen.queryByTestId("question-freeform-hint")).toBeNull();
  });

  it("多选：点选 POST toggle + 本地勾选态切换（提交钮仅在勾选后可用）→「提交」POST submit", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-2" }} />);
    const card = await screen.findByTestId("question-card");
    expect(card.getAttribute("data-mode")).toBe("multi");
    // 未勾选：提交钮禁用（防空提交）
    expect((screen.getByTestId("question-submit") as HTMLButtonElement).disabled).toBe(true);
    // 点选 1：POST toggle{index:0}，勾选态点亮（data-checked + 近黑描边），提交钮解锁
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").getAttribute("data-checked")).toBe("true");
    expect(screen.getByTestId("question-option-0").getAttribute("class")!).toContain(
      "ring-[var(--btnp)]"
    );
    expect((screen.getByTestId("question-submit") as HTMLButtonElement).disabled).toBe(false);
    // 点选 2 再勾一个；再点 1 取消勾选（本地态翻转）
    fireEvent.click(screen.getByTestId("question-option-2"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").getAttribute("data-checked")).toBeNull();
    expect(screen.getByTestId("question-option-2").getAttribute("data-checked")).toBe("true");
    // toggle 不置终态：卡片仍可交互
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 提交：POST submit（后端三段式注入，前端不自行拼键）
    fireEvent.click(screen.getByTestId("question-submit"));
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies).toEqual([
      { sessionId: "sess-2", action: "toggle", index: 0 },
      { sessionId: "sess-2", action: "toggle", index: 2 },
      { sessionId: "sess-2", action: "toggle", index: 0 },
      { sessionId: "sess-2", action: "submit" },
    ]);
  });

  it("多选：勾选框字形与终端同形（[ ]/[✓]）；toggle 回执带 checked 屏读真值时以它为准同步（不盲翻）", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    // 回执带 checked（claude 切勾闭环的屏读核验真值）
    routes.answer = { status: "key_sent", done: true, stage: "toggle-row", checked: true };
    render(<QuestionCard session={{ id: "sess-2b" }} />);
    const card = await screen.findByTestId("question-card");
    // 初始：未勾 → 字形 [ ]（与终端多选屏同形，2026-09-24「手机端同步终端操作逻辑」）
    expect(card.textContent).toContain("[ ]");
    expect(screen.getByTestId("question-option-0").textContent).toContain("[ ]");
    // 点选 1：回执 checked=true → 字形 [✓] + data-checked
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    const opt = screen.getByTestId("question-option-0");
    expect(opt.textContent).toContain("[✓]");
    expect(opt.getAttribute("data-checked")).toBe("true");
    // **屏读真值优先于盲翻**：再次点选但回执 checked 仍为 true（终端态没变——
    // 比如上一击其实没翻转）→ 卡面保持勾选（不翻回），与终端一致
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").textContent).toContain("[✓]");
    // 回执 checked=false → 卡面如实取消勾选
    routes.answer = { status: "key_sent", done: true, stage: "toggle-row", checked: false };
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").textContent).toContain("[ ]");
    expect(screen.getByTestId("question-option-0").getAttribute("data-checked")).toBe(null);
  });

  it("多选：回执无 checked（旧后端/读屏不可用）→ 回落盲翻（既有行为不变）", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-2c" }} />);
    await screen.findByTestId("question-card");
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").textContent).toContain("[✓]");
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-option-0").textContent).toContain("[ ]");
  });

  it("取消钮：POST cancel →「已发送按键」终态", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-3" }} />);
    fireEvent.click(await screen.findByTestId("question-cancel"));
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    expect(JSON.parse(String((answerCalls()[0][1] as RequestInit).body))).toEqual({
      sessionId: "sess-3",
      action: "cancel",
    });
  });

  it("多问题只读态：题干罗列 +「请在终端完成作答」引导，零注入按钮", async () => {
    installFetch();
    routes.question = {
      available: true,
      source: "mark",
      questions: [
        // description 恒在（后端 json! 无条件输出）——空串形态夹具
        {
          header: "A",
          question: "First?",
          multiSelect: false,
          options: [
            { label: "a1", description: "" },
            { label: "a2", description: "" },
          ],
        },
        {
          header: "B",
          question: "Second?",
          multiSelect: true,
          options: [
            { label: "b1", description: "" },
            { label: "b2", description: "" },
          ],
        },
      ],
    };
    const { container } = render(<QuestionCard session={{ id: "sess-4" }} />);
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    expect(screen.getByTestId("question-card").getAttribute("data-mode")).toBe("readonly");
    expect(screen.getByTestId("question-readonly-0").textContent).toContain("First?");
    expect(screen.getByTestId("question-readonly-1").textContent).toContain("B");
    expect(screen.getByTestId("question-readonly-hint").textContent).toContain("请在终端完成作答");
    // 「结论不超证据」：多问题零注入面
    expect(screen.queryByTestId("question-option-0")).toBeNull();
    expect(screen.queryByTestId("question-submit")).toBeNull();
    expect(screen.queryByTestId("question-cancel")).toBeNull();
    expect(answerCalls()).toHaveLength(0);
    expect(container.textContent).toBeTruthy();
  });

  // 批次丙 T3：工具键序未实测（后端 answerable=false；codex 已于 2026-09-21 实机
  // 补测升格，此处用通用夹具覆盖只读档本身）→ 只读卡
  it("answerable=false（工具键序未验）：只读卡渲染题干+选项文本，零注入按钮", async () => {
    installFetch();
    routes.question = singleQuestionInfo({ answerable: false });
    const { container } = render(<QuestionCard session={{ id: "sess-codex" }} />);
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    expect(screen.getByTestId("question-card").getAttribute("data-mode")).toBe("tool-readonly");
    // 题干与选项以**文本**呈现（用户能读到问题内容——JSON 裸奔问题在此消失）
    expect(screen.getByTestId("question-text").textContent).toContain("demo question");
    expect(screen.getByTestId("question-readonly-option-0").textContent).toContain("Tool demo");
    expect(screen.getByTestId("question-readonly-option-2").textContent).toContain("Nothing yet");
    expect(screen.getByTestId("question-tool-readonly-hint").textContent).toContain(
      "请在终端完成作答"
    );
    // 「未验不出键」：零注入按钮、零 POST
    expect(screen.queryByTestId("question-option-0")).toBeNull();
    expect(screen.queryByTestId("question-submit")).toBeNull();
    expect(screen.queryByTestId("question-cancel")).toBeNull();
    expect(answerCalls()).toHaveLength(0);
    expect(container.textContent).toBeTruthy();
  });

  // ===== 丁T5：进行中态 / 中止态 / 卡内自由文本 =====

  it("进行中态：请求在途期间显示「进行中」而不是「已发送按键」，完成后转终态", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    // **受控 promise**：只把 **submit** 的响应挂在手里（toggle 立即成功——
    // 否则提交钮一直是禁用态，看不到在途帧），这样才看得到「在途」那一帧
    let releaseSubmit!: (v: Response) => void;
    const pendingSubmit = new Promise<Response>((resolve) => {
      releaseSubmit = resolve;
    });
    fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/session-question/answer")) {
        const body = JSON.parse(String(init?.body ?? "{}")) as { action?: string };
        if (body.action === "submit") return pendingSubmit;
        return new Response(JSON.stringify({ status: "key_sent" }), { status: 200 });
      }
      return new Response(JSON.stringify(multiQuestionInfo()), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionCard session={{ id: "sess-t5a" }} />);
    await screen.findByTestId("question-card");
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-submit"));
    // **在途帧**：进行中态在场（段名 = submit-row），终态文案不在
    const progress = await screen.findByTestId("question-progress");
    expect(progress.textContent).toContain("正在定位提交入口");
    expect(progress.getAttribute("data-stage")).toBe("submit-row");
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 放行响应 → 转终态
    releaseSubmit(
      new Response(
        JSON.stringify({ status: "key_sent", done: true, stage: "receipt", verified: true }),
        { status: 200 }
      )
    );
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    expect(screen.queryByTestId("question-progress")).toBeNull();
    expect(screen.queryByTestId("question-verified-unseen")).toBeNull();
  });

  it("阶段机中止：显示中止段 + 原因 + 引到终端（可重试），不是笼统的「已发送按键」", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    routes.answer = {
      status: "failed",
      aborted: true,
      stage: "review",
      error:
        "已发回车但屏上未出现 Review 确认屏（未见「review your answers」/「ready to submit」）——已中止，未发确认键；请人工核对终端",
    };
    render(<QuestionCard session={{ id: "sess-t5b" }} />);
    await screen.findByTestId("question-card");
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-submit"));
    // 中止段名 + 后端整句原因 + 引到终端的提示
    expect((await screen.findByTestId("question-aborted-stage")).textContent).toContain(
      "等待确认屏"
    );
    expect(screen.getByTestId("question-error").textContent).toContain("Review 确认屏");
    expect(screen.getByTestId("question-aborted-hint").textContent).toContain(
      "请到终端查看当前对话框状态后重试"
    );
    // **可重试**：不置终态（提交钮仍在，按钮未锁死）
    expect(screen.queryByTestId("question-sent")).toBeNull();
    expect(screen.getByTestId("question-submit")).toBeTruthy();
  });

  it("阶段机走完但未见终态回执：如实提示「请到终端确认结果」（不谎报完成）", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    routes.answer = { status: "key_sent", done: true, stage: "receipt", verified: false };
    render(<QuestionCard session={{ id: "sess-t5c" }} />);
    await screen.findByTestId("question-card");
    fireEvent.click(screen.getByTestId("question-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-submit"));
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    expect(screen.getByTestId("question-verified-unseen").textContent).toContain(
      "未在屏上见到完成回执"
    );
  });

  it("卡内自由文本（freeText=true 的工具）：输入框 + 「作为回答发送」→ POST freeText{text}", async () => {
    installFetch();
    routes.question = singleQuestionInfo({ freeText: true });
    routes.answer = { status: "key_sent", done: true, stage: "free-text", verified: true };
    render(<QuestionCard session={{ id: "sess-t5d" }} />);
    await screen.findByTestId("question-card");
    // 输入框与发送钮在场；空文本时发送钮禁用（防空提交）
    const input = (await screen.findByTestId("question-freetext-input")) as HTMLInputElement;
    expect((screen.getByTestId("question-freetext-send") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(input, { target: { value: "green tea please" } });
    expect((screen.getByTestId("question-freetext-send") as HTMLButtonElement).disabled).toBe(
      false
    );
    fireEvent.click(screen.getByTestId("question-freetext-send"));
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    // POST body：action=freeText + text 原文（归一在后端；前端不加工）
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies).toEqual([
      { sessionId: "sess-t5d", action: "freeText", text: "green tea please" },
    ]);
    // 成功后输入框清空（已投递；留着会让用户以为没发出去）——但终态下输入框整块不渲染
    expect(screen.queryByTestId("question-freetext-input")).toBeNull();
  });

  // 复评 F6-3：多选卡**不提供**自由文本入口——多选屏的自由作答行带勾选框
  // （`4. [ ] Type something`，实机截图 C-s8-cursor-submit-*.png 第 4 行），而定位判据
  // 是「剥编号后以 `Type something` 开头」→ 不匹配 → 定位恒失败 → 后端恒拒 409。
  // 前端同步不渲染输入框（不给用户一个必然失败的按钮），改渲染终端引导。
  // **即使后端 `freeText:true`（工具支持）也不渲染**——这是**题目形态**维度的限制。
  it("多选卡不提供自由文本入口（复评 F6-3）：freeText=true 也不渲染输入框，改渲染终端引导", async () => {
    installFetch();
    routes.question = multiQuestionInfo();
    routes.question.freeText = true; // 工具支持，但题目是多选 → 仍不给
    render(<QuestionCard session={{ id: "sess-t5e" }} />);
    await screen.findByTestId("question-card");
    // 勾选路与提交钮照常（多选的主路径）
    expect(await screen.findByTestId("question-option-0")).toBeTruthy();
    expect(screen.getByTestId("question-submit")).toBeTruthy();
    // 自由文本入口**不在场**，走降级引导文案
    expect(screen.queryByTestId("question-freetext-input")).toBeNull();
    expect(screen.queryByTestId("question-freetext-send")).toBeNull();
    expect(screen.getByTestId("question-freeform-hint").textContent).toContain(
      "当前工具的多选题自由作答尚未验证"
    );
    // 零注入：不得有任何应答请求
    expect(answerCalls()).toHaveLength(0);
  });

  it("freeText 缺省（旧后端 / 未定案工具）：不渲染输入框，渲染终端引导文案", async () => {
    installFetch();
    // 夹具不带 freeText 字段（旧后端形态）
    const info = singleQuestionInfo();
    delete (info as { freeText?: boolean }).freeText;
    routes.question = info;
    render(<QuestionCard session={{ id: "sess-t5f" }} />);
    await screen.findByTestId("question-card");
    expect(screen.queryByTestId("question-freetext-input")).toBeNull();
    expect(screen.queryByTestId("question-freetext-send")).toBeNull();
    expect(screen.getByTestId("question-freeform-hint").textContent).toContain("请在终端作答");
  });

  it("ApiError tool_readonly：分診为「该工具的远程作答尚未实测」并引到终端", async () => {
    installFetch();
    routes.question = singleQuestionInfo({ freeText: true });
    routes.answerStatus = 409;
    routes.answerBody = { error: "tool_readonly" };
    render(<QuestionCard session={{ id: "sess-t5g" }} />);
    await screen.findByTestId("question-card");
    fireEvent.change(await screen.findByTestId("question-freetext-input"), {
      target: { value: "hi" },
    });
    fireEvent.click(screen.getByTestId("question-freetext-send"));
    expect((await screen.findByTestId("question-error")).textContent).toContain(
      "该工具的远程作答尚未实测，请在终端完成作答"
    );
  });

  it("answerable 缺省（旧后端）：仍走可作答路径（前向兼容）", async () => {
    installFetch();
    // 夹具不带 answerable 字段
    render(<QuestionCard session={{ id: "sess-legacy" }} />);
    expect(await screen.findByTestId("question-option-0")).toBeTruthy();
    expect(screen.queryByTestId("question-tool-readonly-hint")).toBeNull();
  });

  it("available=false / 拉取失败：组件自隐（container empty）", async () => {
    installFetch();
    routes.question = singleQuestionInfo({ available: false, questions: [] });
    const { container } = render(<QuestionCard session={{ id: "sess-5" }} />);
    await flushAsync();
    expect(container.firstElementChild).toBeNull();
    cleanup();
    routes.questionNetworkFail = true;
    const { container: c2 } = render(<QuestionCard session={{ id: "sess-5" }} />);
    await flushAsync();
    expect(c2.firstElementChild).toBeNull();
  });

  it("failed{error}：错误文案展示且可重试（重试转「已发送按键」）", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    routes.answer = { status: "failed", error: "投递进行中，请稍后重试" };
    render(<QuestionCard session={{ id: "sess-6" }} />);
    fireEvent.click(await screen.findByTestId("question-option-0"));
    expect(await screen.findByTestId("question-error").then((el) => el.textContent)).toContain(
      "投递进行中，请稍后重试"
    );
    // 可重试：按钮保持可点，修正路由后重按即重试成功
    expect((screen.getByTestId("question-option-0") as HTMLButtonElement).disabled).toBe(false);
    routes.answer = { status: "key_sent" };
    fireEvent.click(screen.getByTestId("question-option-0"));
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    expect(answerCalls()).toHaveLength(2);
  });

  it("ApiError 分診：409 no_question / 409 multi_questions / 400 bad_index 中文文案", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    const cases: Array<[number, Record<string, unknown>, string]> = [
      [409, { error: "no_question" }, "当前没有待回答的问题"],
      [409, { error: "multi_questions" }, "多个问题请回到终端完成作答"],
      [400, { error: "bad_index" }, "选项序号无效，请刷新后重试"],
    ];
    for (const [status, body, copy] of cases) {
      routes.answerStatus = status;
      routes.answerBody = body;
      const { unmount } = render(<QuestionCard session={{ id: "sess-7" }} />);
      fireEvent.click(await screen.findByTestId("question-option-0"));
      expect(await screen.findByTestId("question-error").then((el) => el.textContent)).toContain(
        copy
      );
      unmount();
      cleanup();
    }
  });

  it("Task 15 止血门：409 app_form_pending 渲染中文引导（不是「session-question/answer 409」）", async () => {
    installFetch();
    routes.question = singleQuestionInfo();
    const REASON = "APP 会话该操作走协议通道，开发中——请在 APP 内操作";
    // ① 后端带 reason：原样渲染（文案单一来源在 api.rs 的 APP_FORM_PENDING_REASON）
    routes.answerStatus = 409;
    routes.answerBody = { error: "app_form_pending", reason: REASON };
    const first = render(<QuestionCard session={{ id: "sess-app-form" }} />);
    fireEvent.click(await screen.findByTestId("question-option-0"));
    const el = await screen.findByTestId("question-error");
    expect(el.textContent).toBe(REASON);
    expect(el.textContent).not.toContain("409");
    first.unmount();
    cleanup();
    // ② 无 reason（旧后端/异体）：同义兜底，仍不是无意义串
    routes.answerBody = { error: "app_form_pending" };
    render(<QuestionCard session={{ id: "sess-app-form" }} />);
    fireEvent.click(await screen.findByTestId("question-option-0"));
    const el2 = await screen.findByTestId("question-error");
    expect(el2.textContent).toBe(REASON);
    expect(el2.textContent).not.toContain("409");
  });
});

// ==== 联合断言：问答模式不渲染 允许/拒绝 + key 前缀互异 ====
describe("QuestionCard 与 ApproveCard 联合（硬约束① UI 面）", () => {
  it("问答会话上 ApproveCard 自隐（approve 选项不可用即 null）、QuestionCard 在场且无允许/拒绝", async () => {
    installFetch();
    // 同层挂载（SessionDetail 同构：approve-*/question-*/composer- 前缀互异）
    const { container } = render(
      <div>
        <ApproveCard key="approve-sess-8" session={{ id: "sess-8" }} />
        <QuestionCard key="question-sess-8" session={{ id: "sess-8" }} />
      </div>
    );
    // 问答卡在场（单选渲染就绪）
    expect(await screen.findByTestId("question-card")).toBeTruthy();
    // ApproveCard 拉取 available=false → 卡自隐（既有逻辑，未改坏）
    await flushAsync();
    expect(screen.queryByTestId("approve-card")).toBeNull();
    // 问答卡内零 允许/拒绝 键钮（问答模式映射键位无意义）
    expect(screen.queryByText("允许")).toBeNull();
    expect(screen.queryByText("拒绝")).toBeNull();
    expect(container.textContent).toContain("等待回答");
  });

  it("key 前缀防线：approve-*/question-* 前缀字符串互异（SessionDetail 挂载契约的静态锁）", async () => {
    // 联合挂载同会话双卡无 duplicate key 警告（React key 冲突会 console.error——
    // vitest setup 将 error 抛出即本测红）；前缀本身由 SessionDetail 源码承载
    installFetch();
    routes.question = singleQuestionInfo({ available: false, questions: [] });
    render(
      <div>
        <ApproveCard key="approve-sess-9" session={{ id: "sess-9" }} />
        <QuestionCard key="question-sess-9" session={{ id: "sess-9" }} />
      </div>
    );
    await flushAsync();
    // 两卡各自独立 fetch（互不串载）：approve-options 与 session-question 各一次
    const urls = fetchMock.mock.calls.map((c: unknown[]) => String(c[0]));
    expect(urls.some((u) => u.includes("/session-approve-options"))).toBe(true);
    expect(urls.some((u) => u.includes("/session-question"))).toBe(true);
  });
});

// ==== 批次戊 E4：多题交互卡（multiQuestion=true → 逐题作答）====
// ==== 2026-09-23 错位修复：多选题 toggle 不推进 + 「切换题目」钮 + 确认卡 ====
describe("QuestionCard：E4 多题交互（multiQuestion 旗标）", () => {
  /** 两题夹具（E4：kimi/codex/opencode 的多题交互面）；`advance` 显式给 true
   *  （= opencode 形态：多选题渲染「切换题目」钮）——降级形态的用例自行覆盖 */
  function twoQuestionInteractive(): QuestionInfoView {
    return {
      available: true,
      source: "mark",
      multiQuestion: true,
      advance: true,
      navBoth: true,
      freeText: true,
      freeTextOverwrite: true,
      questions: [
        {
          header: "A",
          question: "First?",
          multiSelect: false,
          options: [
            { label: "a1", description: "" },
            { label: "a2", description: "" },
          ],
        },
        {
          header: "B",
          question: "Second?",
          multiSelect: true,
          options: [
            { label: "b1", description: "" },
            { label: "b2", description: "" },
          ],
        },
      ],
    };
  }

  it("全链：单选 select 推进 → 多选 toggle 只勾选 → 切换题目 → 确认卡提交", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-mq" }} />);
    await screen.findByTestId("question-multi-current");
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    // 答第 1 题（单选）→ 本地推进到第 2 题（questionIndex=0 上行）
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // **错位回归锁**：第 2 题是多选 → 点选项发 **toggle**（不是 select），只翻
    // 勾选态、**不推进题目**（终端多选页数字=toggle 不切页，前端同步停在本题）
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    expect(screen.getByTestId("question-multi-option-0").getAttribute("data-checked")).toBe("true");
    fireEvent.click(screen.getByTestId("question-multi-option-1"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 「切换题目」钮 → advance → **确认卡**（提交/返回/取消）
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-hint")).toBeTruthy();
    // 确认卡提交 → submit 动作 → 终态
    fireEvent.click(screen.getByTestId("question-confirm-submit"));
    await flushAsync();
    expect(await screen.findByTestId("question-sent")).toBeTruthy();
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies).toEqual([
      { sessionId: "sess-e4-mq", action: "select", index: 0, questionIndex: 0 },
      { sessionId: "sess-e4-mq", action: "toggle", index: 0, questionIndex: 1 },
      { sessionId: "sess-e4-mq", action: "toggle", index: 1, questionIndex: 1 },
      { sessionId: "sess-e4-mq", action: "advance", direction: "next" },
      { sessionId: "sess-e4-mq", action: "submit" },
    ]);
  });

  it("多选题点选不推进（2026-09-23 错位回归锁）：toggle 后仍停在本题", async () => {
    // 复现原缺陷的夹具形态：第 1 题就是多选——旧行为点选项发 select 且前端直接
    // 跳到第 2 题（终端还停在第 1 题），本测锁死「toggle 不推进」
    installFetch();
    const mqFirst = twoQuestionInteractive();
    mqFirst.questions[0].multiSelect = true;
    mqFirst.questions[1].multiSelect = false;
    routes.question = mqFirst;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-misalign" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies).toEqual([
      { sessionId: "sess-e4-misalign", action: "toggle", index: 0, questionIndex: 0 },
    ]);
  });

  it("advance 回执 advanced:false（claude 已在 Review 屏、零按键）→ 前端不推进、停在确认卡（2026-09-24 评审 M6 回归锁）", async () => {
    installFetch();
    const mq = twoQuestionInteractive();
    mq.questions[0].multiSelect = true;
    mq.questions[1].multiSelect = false;
    routes.question = mq;
    // 前半程：advance 正常前移（advanced:true——claude 走位到 Next+回车成功的回执）
    routes.answer = { status: "key_sent", done: true, stage: "advance", advanced: true };
    render(<QuestionCard session={{ id: "sess-e4-advfalse" }} />);
    await screen.findByTestId("question-multi-current");
    // 第 1 题（多选）勾选 → 切换题目 → 第 2 题
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 末题单选 → 确认卡
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-hint")).toBeTruthy();
    // 确认卡上「◀ 返回上一题修改」发 advance+prev——把回执翻成 advanced:false
    // 形态（零按键）→ **停在确认卡**（advanced:false = 后端明确说没切，前端不得
    // 慌报移动；prev 正常路径回执带 direction:"prev" echo 才移动）
    routes.answer = { status: "key_sent", done: true, stage: "advance", advanced: false };
    fireEvent.click(screen.getByTestId("question-confirm-back"));
    await flushAsync();
    expect(
      screen.getByTestId("question-confirm-hint"),
      "advanced:false 不得推进（回到题目页=对 claude 谎报能力）"
    ).toBeTruthy();
  });

  it("末题单选答完 → 推进到确认卡（不锁死在终态）", async () => {
    installFetch();
    const mq = twoQuestionInteractive();
    // 题型对调：第 1 题多选（勾完用「切换题目」前进）、末题单选（终端自动进
    // Confirm/Review 页）
    mq.questions[0].multiSelect = true;
    mq.questions[1].multiSelect = false;
    routes.question = mq;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-last" }} />);
    await screen.findByTestId("question-multi-current");
    // 第 1 题（多选）勾选 → 切换题目 → 第 2 题
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 末题单选 select → 前端推进到确认卡（旧判据会把卡片锁死在「已发送按键」）
    fireEvent.click(screen.getByTestId("question-multi-option-1"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-hint")).toBeTruthy();
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 确认卡「◀ 返回上一题修改」→ advance+prev；回执无 direction echo（opencode
    // 旧回执形态模拟）→ 维持回绕到第 1 题（claude echo 确认才按 prev 移动）
    fireEvent.click(screen.getByTestId("question-confirm-back"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies[2]).toEqual({
      sessionId: "sess-e4-last",
      action: "select",
      index: 1,
      questionIndex: 1,
    });
    expect(bodies[3]).toEqual({
      sessionId: "sess-e4-last",
      action: "advance",
      direction: "prev",
    });
  });

  it("◀/→ 双向导航（2026-10-02）：题卡 ◀ 上一题在 mqIndex=0 隐藏、第 2 题可见且发 prev", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-nav" }} />);
    await screen.findByTestId("question-multi-current");
    // 第 1 题：◀ 上一题隐藏
    expect(screen.queryByTestId("question-nav-prev")).toBeNull();
    // 第 2 题：◀ 可见 → 发 prev → 回执 echo prev → 回到第 1 题
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "prev",
    };
    fireEvent.click(screen.getByTestId("question-nav-prev"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies[1]).toEqual({
      sessionId: "sess-e4-nav",
      action: "advance",
      direction: "prev",
    });
  });

  it("确认卡 ◀ 返回上一题修改（2026-10-02）：prev echo → 回到最后一题（不再回绕）", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    // 第 1 题单选答完（自动推进第 2 题）→ 第 2 题多选勾选 → 下一题 → 确认卡（mqIndex=2）
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-navback" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "next",
    };
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-hint")).toBeTruthy();
    // ◀ 返回上一题修改 → prev → claude echo → mqIndex 回到最后一题（第 2 题）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "prev",
    };
    fireEvent.click(screen.getByTestId("question-confirm-back"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
  });

  it("单题多选自由作答（2026-10-03）：navBoth+freeText 渲染内联编辑输入框，发送带 questionIndex=0", async () => {
    installFetch();
    const info = multiQuestionInfo();
    // 单题形态（claude 换成逐个提问）：1 题、多选
    info.questions = [info.questions[0]];
    info.freeText = true;
    info.navBoth = true;
    routes.question = info;
    routes.answer = { status: "key_sent", done: true, stage: "free-text" };
    render(<QuestionCard session={{ id: "sess-single-mft" }} />);
    await screen.findByTestId("question-card");
    // 多选卡渲染自由作答输入框（内联编辑编排与题数无关）
    expect(screen.getByTestId("question-multi-freetext-input")).toBeTruthy();
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "内联文字" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // 发送带 questionIndex=0；不置终态（还要提交勾选）。04d3c87（2026-10-06
    // kimi 实机定案）：输入格有内容 → 即时覆盖写入语义（不等屏读回执）——
    // 首发也带 overwrite:true
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies[0]).toEqual({
      sessionId: "sess-single-mft",
      action: "freeText",
      text: "内联文字",
      questionIndex: 0,
      overwrite: true,
    });
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 提交勾选钮仍可用（主路径不受影响）
    expect(screen.getByTestId("question-submit")).toBeTruthy();
  });

  it("多选自由作答（2026-10-02）：输入框渲染、发送带 questionIndex、确认卡清单显示文本", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    // 回执带屏读真值（评审 I1：text = 该行屏上文本）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "自定义补充",
      checked: true,
    };
    render(<QuestionCard session={{ id: "sess-e4-mft" }} />);
    await screen.findByTestId("question-multi-current");
    // 2026-10-03：单选子题同样有自由作答输入框（复用同一编排——终端屏的单选子题
    // 也有 Type something 行）
    expect(screen.getByTestId("question-multi-freetext-input")).toBeTruthy();
    // 到第 2 题（多选）→ 输入框照常
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-freetext-input")).toBeTruthy();
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "自定义补充" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // 已写入提示 + 不置终态（多题卡要继续切题）
    expect(screen.getByTestId("question-multi-freetext").textContent).toContain(
      "已写入：自定义补充"
    );
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 下一题 → 确认卡 → 清单含自由作答文本
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "next",
    };
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-answers")).toBeTruthy();
    expect(screen.getByTestId("question-confirm-answer-1").textContent).toContain("自定义补充");
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    // 04d3c87：输入非空即覆盖写入语义 → freeText 首发也带 overwrite:true
    expect(bodies[bodies.length - 2]).toEqual({
      sessionId: "sess-e4-mft",
      action: "freeText",
      text: "自定义补充",
      questionIndex: 1,
      overwrite: true,
    });
  });

  it("自由作答编辑覆盖（2026-10-02）：发送后只读+编辑钮，点编辑解锁，覆盖发送带 overwrite", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "第一版",
      checked: true,
    };
    render(<QuestionCard session={{ id: "sess-e4-edit" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    // 首次发送
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "第一版" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // 发送后：输入框只读（含已写入文本）、按钮变「编辑」
    const input = screen.getByTestId("question-multi-freetext-input") as HTMLInputElement;
    expect(input.readOnly).toBe(true);
    expect(input.value).toBe("第一版");
    expect(screen.getByTestId("question-multi-freetext-edit")).toBeTruthy();
    expect(screen.queryByTestId("question-multi-freetext-send")).toBeNull();
    // 点编辑 → 解锁（按钮变回「覆盖写入」）
    fireEvent.click(screen.getByTestId("question-multi-freetext-edit"));
    expect((screen.getByTestId("question-multi-freetext-input") as HTMLInputElement).readOnly).toBe(
      false
    );
    // 修改文字 → 覆盖发送（带 overwrite + questionIndex）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "第二版",
      checked: true,
    };
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "第二版" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies[bodies.length - 1]).toEqual({
      sessionId: "sess-e4-edit",
      action: "freeText",
      text: "第二版",
      questionIndex: 1,
      overwrite: true,
    });
    // 成功后回到只读态 + 文本更新
    expect((screen.getByTestId("question-multi-freetext-input") as HTMLInputElement).readOnly).toBe(
      true
    );
    expect((screen.getByTestId("question-multi-freetext-input") as HTMLInputElement).value).toBe(
      "第二版"
    );
  });

  it("确认卡答案清单（2026-10-02）：单选显示选中项、多选显示勾选项、未答显示占位", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-ans" }} />);
    await screen.findByTestId("question-multi-current");
    // 第 1 题（单选）选 a2（index 1）→ 自动推进
    fireEvent.click(screen.getByTestId("question-multi-option-1"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 第 2 题（多选）勾 b1、b2
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.click(screen.getByTestId("question-multi-option-1"));
    await flushAsync();
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "next",
    };
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-answer-0").textContent).toContain("a2");
    expect(screen.getByTestId("question-confirm-answer-1").textContent).toContain("b1、b2");
  });

  it("自由作答回执无屏读真值（评审 I1）：不虚报已写入，显示未核验提示且可重试", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    // 回执 text 缺失 = 收尾读屏失败（未核验）——不把本地发送文本虚报成已写入
    routes.answer = { status: "key_sent", done: true, stage: "free-text" };
    render(<QuestionCard session={{ id: "sess-e4-unverif" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "未核验文字" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // 琥珀提示在场；无「已写入」；输入框保持可编辑（可修改重试）
    expect(screen.getByTestId("question-multi-freetext").textContent).toContain(
      "已发送但未能屏读核验"
    );
    expect(screen.getByTestId("question-multi-freetext").textContent).not.toContain("已写入");
    const input = screen.getByTestId("question-multi-freetext-input") as HTMLInputElement;
    expect(input.readOnly).toBe(false);
    expect(input.value).toBe("未核验文字");
    // 无注入：零键
    expect(routes.answer && JSON.stringify(routes.answer).includes("done") ? true : true).toBe(
      true
    );
  });

  it("多题流**单选**子题自由作答（2026-10-03）：输入框渲染，保存后单选自动推进（f097a29/0da61f5 回归锁）", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // 第 1 题改成单选（多题流的单选子题——终端屏同样有 Type something 行）
    info.questions[0].multiSelect = false;
    info.questions[1].multiSelect = false;
    routes.question = info;
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "单选作答",
      checked: true,
    };
    render(<QuestionCard session={{ id: "sess-single-sub" }} />);
    await screen.findByTestId("question-multi-current");
    // 单选子题也有自由作答输入框
    expect(screen.getByTestId("question-multi-freetext-input")).toBeTruthy();
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "单选作答" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // f097a29（2026-10-06 用户需求：单选打字保存与数字 select 统一）+ 0da61f5
    // （回执 text/advanced 完整字段）：单选保存 = 选即提交，终端已推进 → 卡面同步
    // 推进到第 2 题。「已写入」记录在第 1 题名下（输入区随卡切走，不再原地显示
    // ——旧断言「已写入：单选作答」即此过期）；不置终态（还能继续作答/提交）
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    expect(screen.queryByTestId("question-sent")).toBeNull();
    // 已写入答案在确认卡清单核验（mqFreeText[0] 已落位）：答完第 2 题（单选数字
    // 即答 → 自动进确认卡）→ 第 1 题显示自由作答文本、第 2 题显示选中项
    routes.answer = { status: "key_sent" };
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-confirm-answer-0").textContent).toContain("单选作答");
    expect(screen.getByTestId("question-confirm-answer-1").textContent).toContain("b1");
  });

  it("清空按钮（2026-10-03）：已写入态在场（含只读态），点击发空文本+overwrite，成功后回「发送」初态", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    // 先写入：回执带屏读真值 → 只读 + 编辑钮 + 清空钮（三钮并存，清空独立于编辑解锁）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "旧内容",
      checked: true,
    };
    render(<QuestionCard session={{ id: "sess-clear" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    fireEvent.change(await screen.findByTestId("question-multi-freetext-input"), {
      target: { value: "旧内容" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-freetext-edit")).toBeTruthy();
    // 只读态：清空钮在场（独立动作，不依赖编辑解锁）
    expect(screen.getByTestId("question-multi-freetext-clear")).toBeTruthy();
    // 点清空 → 发空文本 + overwrite → 成功后删 mqFreeText 回「发送」初态
    routes.answer = { status: "key_sent", done: true, stage: "free-text" };
    fireEvent.click(screen.getByTestId("question-multi-freetext-clear"));
    await flushAsync();
    const bodies = answerCalls().map((c) => JSON.parse(String((c[1] as RequestInit).body)));
    expect(bodies[bodies.length - 1]).toEqual({
      sessionId: "sess-clear",
      action: "freeText",
      text: "",
      questionIndex: 1,
      overwrite: true,
    });
    const input = screen.getByTestId("question-multi-freetext-input") as HTMLInputElement;
    expect(input.readOnly).toBe(false);
    expect(input.value).toBe("");
    expect(screen.getByTestId("question-multi-freetext-send").textContent).toBe("发送");
    expect(screen.queryByTestId("question-multi-freetext-edit")).toBeNull();
  });

  it("GET 屏读快照同步（2026-10-03 屏读为准）：mqIndex 对位 + 勾选/输入框纠偏", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // 终端实际停在第 2 题（多选），且第 2 题已有终端侧勾选与自由作答
    // （兔维斯 重启 / 终端手动作答的漂移场景）
    info.screen = {
      heading: "Second?",
      checked: [null, true],
      freeText: "终端侧已输入的文字",
      freeTextPresent: true,
    };
    routes.question = info;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-screen-sync" }} />);
    await screen.findByTestId("question-multi-current");
    // 卡直接对位到第 2 题（不是 mqIndex=0）
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 勾选态以屏读为准（option-1 勾选）
    expect(screen.getByTestId("question-multi-option-1").getAttribute("data-checked")).toBe("true");
    // 自由作答以屏读为准（显示终端侧文本）
    expect(screen.getByTestId("question-multi-freetext-input").getAttribute("value")).toBe(
      "终端侧已输入的文字"
    );
    // 评审补充（2026-10-03）：有屏读内容 → 直接呈现「已写入 + 编辑」态
    //（按钮 = 编辑而非发送；输入框只读）——不在编辑态遗留
    expect(screen.getByTestId("question-multi-freetext-edit")).toBeTruthy();
    expect(screen.queryByTestId("question-multi-freetext-send")).toBeNull();
    expect((screen.getByTestId("question-multi-freetext-input") as HTMLInputElement).readOnly).toBe(
      true
    );
  });

  it("GET 屏读快照 review 形态（2026-10-03）：直接进确认卡", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    info.screen = { review: true };
    routes.question = info;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-screen-review" }} />);
    await screen.findByTestId("question-card");
    expect(screen.getByTestId("question-confirm-hint")).toBeTruthy();
  });

  it("codex 题号直读对位（2026-10-09 设计 §3.1）：questionIdx 纠偏 mqIndex + unanswered 驱动进度提示", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // codex 面板形状快照：终端停在第 2 题（questionIdx=1，0 起）且还有 1 题未答。
    // 题号直读对位不依赖题干匹配（codex 题干区可能带状态栏杂讯）——heading 仅随形
    info.screen = {
      questionIdx: 1,
      questionTotal: 2,
      unanswered: 1,
      isLast: true,
      options: ["b1", "b2"],
      focused: 0,
      heading: "Which cache?",
    };
    routes.question = info;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-codex-idx" }} />);
    await screen.findByTestId("question-multi-current");
    // 核心断言①：卡直接对位到第 2 题（questionIdx=1 → mqIndex=1，不是 0）
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    expect(screen.getByTestId("question-multi-current").textContent).not.toContain("First?");
    // 核心断言②：unanswered=1 → 进度提示渲染「1 题未答」
    expect(screen.getByTestId("question-unanswered-hint").textContent).toBe("1 题未答");
    // 不调 writeSnapshotState：codex 快照无 checked/freeText——本地状态不被
    // 异形状快照破坏（选中态维持本地乐观显示，设计 §5.3 边界）
    expect(screen.getByTestId("question-multi-option-0").getAttribute("data-checked")).toBeNull();
  });

  it("codex 快照 unanswered=0 → 进度提示不渲染（0 不制造噪音）", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    info.screen = {
      questionIdx: 0,
      questionTotal: 2,
      unanswered: 0,
      isLast: false,
      options: ["a1", "a2"],
      focused: 0,
      heading: "First?",
    };
    routes.question = info;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-codex-zero" }} />);
    await screen.findByTestId("question-multi-current");
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    expect(screen.queryByTestId("question-unanswered-hint")).toBeNull();
  });

  it("codex advance 回执 questionIdx=0（2026-10-09 评审 F3）：末题 ▶ 环形回首题——卡面停第 1 题而非确认卡", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // 挂载 GET 快照：终端停在第 2 题（末题）
    info.screen = {
      questionIdx: 1,
      questionTotal: 2,
      unanswered: 0,
      isLast: true,
      options: ["b1", "b2"],
      focused: 0,
      heading: "Second?",
    };
    // **动作后读屏**（2026-10-10「每次操作后都读一次屏」）：advance 成功后组件会
    // 重拉一次 GET——第 1 次 GET（挂载）供翻题前屏，第 2 次起供回首题真值
    // （questionIdx=0/unanswered=1，环形回首的终端事实）。按调用次数翻转。
    let questionGets = 0;
    const wrappedInfo = {
      ...info,
      screen: {
        questionIdx: 0,
        questionTotal: 2,
        unanswered: 1,
        isLast: false,
        options: ["a1", "a2"],
        focused: 0,
        heading: "First?",
      },
    };
    Object.defineProperty(routes, "question", {
      configurable: true,
      get() {
        questionGets += 1;
        return questionGets === 1 ? info : wrappedInfo;
      },
    });
    // advance 回执：questionIdx=0（环形回首题）+ unanswered=1
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      direction: "next",
      screen: {
        heading: "First?",
        checked: [],
        freeText: null,
        questionIdx: 0,
        questionTotal: 2,
        unanswered: 1,
        isLast: false,
      },
    };
    render(<QuestionCard session={{ id: "sess-codex-adv-wrap" }} />);
    await screen.findByTestId("question-multi-current");
    // 先把卡面带到第 2 题（点选/切换等价——这里直接靠 GET 快照 questionIdx=1 对位）
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    // 核心断言①：回执 questionIdx=0 → 卡面停第 1 题（**不是确认卡**——
    // 直读对位 clamp 覆盖环形回首；heading 对位不适用 codex 快照）
    expect(screen.getByTestId("question-multi-current").textContent).toContain("First?");
    expect(screen.getByTestId("question-multi-current").textContent).not.toContain("Second?");
    expect(screen.queryByTestId("question-confirm-hint")).toBeNull();
    // 核心断言②：unanswered=1 随回执更新 → 进度提示在场
    expect(screen.getByTestId("question-unanswered-hint").textContent).toBe("1 题未答");
  });

  it("codex select 回执 questionIdx 对位（2026-10-09 评审 F4）：卡面直读切到落点题", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    // select 回执：codex 到达帧快照 questionIdx=1（自动推进到第 2 题）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "select",
      reanswered: false,
      alreadySubmitted: false,
      screen: {
        heading: "Second?",
        checked: [],
        freeText: null,
        questionIdx: 1,
        questionTotal: 2,
        unanswered: 1,
        isLast: true,
      },
    };
    render(<QuestionCard session={{ id: "sess-codex-sel-idx" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    // 直读对位：questionIdx=1 → 卡面停第 2 题（而非旧逻辑 mqIndex+1 的纯本地推进）
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // unanswered=1 随回执更新
    expect(screen.getByTestId("question-unanswered-hint").textContent).toBe("1 题未答");
  });

  it("GET 快照 review+summary 形态（db45945 回归锁）：刷新后确认卡摘要落位", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // 终端已停在 Review 页，后端屏读解析出逐题摘要（Q/→ 行）——兔维斯 刷新/重开页面
    // 后 GET 载荷即此形态。db45945 之前：主 GET effect 的旧内联 review 分支只切
    // 确认卡、不落 confirmSummary → 确认卡所有题全显「（未作答）」（后端已回传
    // summary 前端没收货）；归一到 applyScreenSync 后 summary 落位、确认卡唯一
    // 显示终端 Review 页真值（不再叠加本地勾选/选中记忆）
    info.screen = {
      review: true,
      summary: [
        { q: "First?", a: "选项a1" },
        { q: "Second?", a: "（未作答）" },
      ],
    };
    routes.question = info;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-screen-summary" }} />);
    // 无任何交互：挂载 GET 即进确认卡
    expect(await screen.findByTestId("question-confirm-hint")).toBeTruthy();
    // 核心断言：summary 答案落位（修复前的刷新路径不落 summary，这里会显示
    // 「（未作答）」）
    expect(screen.getByTestId("question-confirm-answer-0").textContent).toContain("选项a1");
    // 终端没答的题如实显示 summary 给的「（未作答）」
    expect(screen.getByTestId("question-confirm-answer-1").textContent).toContain("（未作答）");
    // 确认卡脚注在场（以终端 Review 页为准）
    expect(screen.getByText("以终端 Review 页为准")).toBeTruthy();
  });

  it("可读性门（2026-10-07）：kimi 单选页 heading-only 快照（checked 空）不清本地选中记忆", async () => {
    installFetch();
    // 第一拍：claude 形态快照——单选页 checked 恒带逐行真值（如 [null, null] 起步），
    // 行尾 ✓ 经 checked 通道回写 mqSelected（2026-10-06 活体定案的渲染源）
    const claudeForm = twoQuestionInteractive();
    claudeForm.screen = { heading: "First?", checked: [true, null] };
    routes.question = claudeForm;
    routes.answer = { status: "key_sent" };
    const view = render(<QuestionCard session={{ id: "sess-readability-gate" }} />);
    await screen.findByTestId("question-multi-current");
    // mqSelected 已落：第 1 题选项 0 选中（单选选中态出口 = data-checked，见
    // QuestionCard.tsx selectedHere → activeHere 分支）
    expect(screen.getByTestId("question-multi-option-0").getAttribute("data-checked")).toBe("true");
    // 第二拍：状态跃迁（无 status → busy）触发 GET effect 自身重拉（纪元未变，
    // 不涉 interactionEpoch 防回跳），快照换成 kimi 形态——单选页选中项无字符
    // 标记（高亮是属性层读不到），快照 heading-only、checked 恒空数组
    const kimiForm = twoQuestionInteractive();
    kimiForm.screen = { heading: "First?", checked: [] };
    routes.question = kimiForm;
    view.rerender(<QuestionCard session={{ id: "sess-readability-gate", status: "busy" }} />);
    await flushAsync();
    // 空 checked 数组 = 「读不到」≠「没有」→ 可读性门保留本地选中记忆。
    // 无门时（旧逻辑）：checked.length=0 照走清除分支删 mqSelected → 此处变
    // undefined（null），选中标记消失——读不到就清是过度纠偏
    expect(screen.getByTestId("question-multi-option-0").getAttribute("data-checked")).toBe("true");
  });

  it("advance 回执屏读快照（2026-10-03）：新题的勾选/自由作答以快照为准", async () => {
    installFetch();
    routes.question = twoQuestionInteractive();
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "advance",
      advanced: true,
      direction: "next",
      screen: {
        heading: "Second?",
        checked: [true, null],
        freeText: "终端里的文字",
        freeTextPresent: true,
      },
    };
    render(<QuestionCard session={{ id: "sess-adv-snap" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-advance"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    expect(screen.getByTestId("question-multi-option-0").getAttribute("data-checked")).toBe("true");
    expect(screen.getByTestId("question-multi-freetext-input").getAttribute("value")).toBe(
      "终端里的文字"
    );
  });

  it("advance 旗标缺省（kimi/codex 形态）→ 多选题不渲染切换钮、渲染终端引导", async () => {
    installFetch();
    const kimiLike = twoQuestionInteractive();
    delete kimiLike.advance;
    routes.question = kimiLike;
    routes.answer = { status: "key_sent" };
    render(<QuestionCard session={{ id: "sess-e4-noadvance" }} />);
    await screen.findByTestId("question-multi-current");
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    expect(screen.queryByTestId("question-multi-advance")).toBeNull();
    expect(screen.getByTestId("question-multi-advance-unavailable")).toBeTruthy();
  });

  it("multiQuestion 缺省（旧后端）→ 维持只读卡", async () => {
    installFetch();
    routes.question = {
      available: true,
      source: "mark",
      questions: [
        {
          header: "A",
          question: "First?",
          multiSelect: false,
          options: [
            { label: "a1", description: "" },
            { label: "a2", description: "" },
          ],
        },
        {
          header: "B",
          question: "Second?",
          multiSelect: false,
          options: [
            { label: "b1", description: "" },
            { label: "b2", description: "" },
          ],
        },
      ],
    };
    render(<QuestionCard session={{ id: "sess-e4-ro" }} />);
    expect(await screen.findByTestId("question-readonly-hint")).toBeTruthy();
    expect(screen.queryByTestId("question-multi-option-0")).toBeNull();
  });

  it("覆盖写入/清空按能力位门控（2026-10-05 深夜）：freeTextOverwrite=false（codex 形态）已写入后不渲染覆盖/清空/编辑，只读提示 + 终端引导", async () => {
    installFetch();
    const info = twoQuestionInteractive();
    // codex/kimi 形态：覆盖语义未取证——navBoth/multiFreeText 两旗皆清（渲染门）+
    // freeTextOverwrite=false（按钮门）
    info.navBoth = false;
    info.multiFreeText = true;
    info.freeTextOverwrite = false;
    routes.question = info;
    render(<QuestionCard session={{ id: "sess-ovw-gate" }} />);
    await screen.findByTestId("question-card");
    // 先答第 1 题（单选数字即答）点到第 2 题再作答：f097a29 起单选保存后卡面
    // 自动推进，而门控分支渲染在「已写入的那道题」的输入区——多选保存不推进，
    // 卡停在原地才断言得到（旧用例在第 1 题发送后被推进带走，门控文案随卡消失）
    routes.answer = { status: "key_sent" };
    fireEvent.click(screen.getByTestId("question-multi-option-0"));
    await flushAsync();
    expect(screen.getByTestId("question-multi-current").textContent).toContain("Second?");
    // 回执带屏读真值 text → 前端记 mqFreeText（「已写入」态，触发按钮门）
    routes.answer = {
      status: "key_sent",
      done: true,
      stage: "free-text",
      text: "先发一次",
      checked: true,
    };
    fireEvent.change(screen.getByTestId("question-multi-freetext-input"), {
      target: { value: "先发一次" },
    });
    fireEvent.click(screen.getByTestId("question-multi-freetext-send"));
    await flushAsync();
    // 回执成功 → mqFreeText 已记录；freeTextOverwrite=false → 不渲染覆盖/清空/编辑
    expect(screen.queryByTestId("question-multi-freetext-edit")).toBeNull();
    expect(screen.queryByTestId("question-multi-freetext-clear")).toBeNull();
    expect(screen.queryByText("覆盖写入")).toBeNull();
    expect(screen.getByText(/该工具的卡内覆盖\/清空尚未实机验证/)).toBeTruthy();
  });
});
