import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import QuestionCardApp from "@/mobile/QuestionCardApp";
import type { QuestionInfoView } from "@/mobile/api";

// Task 8（P1 codex 轨）：QuestionCardApp——APP 形态问答卡（裁决 28b 降级补投）。
// 契约锚点（后端 Task 6b `session_question_answer` 的 AppRepost 臂）：
// - 有作答文本的动作（select/toggle/freeText）经 H8 queue 通道补投，模板由后端组装
//（select=「选择第 N 项：<文本>」/ toggle=「勾选第 N 项：<文本>」/ freeText=原文）；
// - 回执 = **无头封套**（{status:"headless", receipt:{status:ok|queued|failed,…}}）
//   + `screen:null`（显式 null——补投不产屏读快照）+ `note`（仅 ok/queued 附
//   「已补投为消息，请回 APP 核对题面」）；
// - 无作答文本的动作（切题/提交/中止）→ 409 {error:"app_form_no_repost", reason}。
// 红线：**永不声称已作答、不显示选项翻转**（补投是自然语言消息，翻转不保证）。
// 覆盖（任务书测试清单）——
// ① 单选补投模板与请求形状；② 自由作答原文补投；③ 回执如实（note + screen=null，
// 不冒充已作答）。

interface Routes {
  question?: QuestionInfoView;
  answer?: Record<string, unknown>;
  answerStatus?: number;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  routes = {};
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/session-question/answer")) {
      if (routes.answerStatus) {
        return new Response(JSON.stringify(routes.answer ?? { error: "internal" }), {
          status: routes.answerStatus,
        });
      }
      return new Response(JSON.stringify(routes.answer ?? repostEnvelope("ok")), { status: 200 });
    }
    if (url.includes("/session-question")) {
      return new Response(JSON.stringify(routes.question ?? singleChoicePayload()), {
        status: 200,
      });
    }
    throw new Error(`unexpected fetch: ${url} ${init?.method ?? ""}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** 单选题载荷（APP 面：screen 恒 null——无屏读快照，读侧走 rollout 事件流） */
function singleChoicePayload(): QuestionInfoView {
  return {
    available: true,
    answerable: true,
    freeText: true,
    multiQuestion: false,
    screen: null,
    questions: [
      {
        header: "Next step",
        question: "下一步做什么？",
        multiSelect: false,
        options: [
          { label: "Tool demo", description: "跑一遍工具演示" },
          { label: "Skip", description: "" },
        ],
      },
    ],
    source: "scan",
  };
}

/** Task 6b 补投回执（无头封套 + screen:null + note）。failed 不带 note（不谎报送达）。 */
function repostEnvelope(
  status: "ok" | "queued" | "failed",
  reason?: string
): Record<string, unknown> {
  const receipt: Record<string, unknown> = { status, sessionId: "s1", durationMs: 900 };
  if (status === "failed") {
    receipt.stage = "channel_error";
    receipt.reason = reason ?? "codex 协议通道不可达";
  } else {
    receipt.lastAssistant = "（回合摘要）";
  }
  return {
    status: "headless",
    channel: "headless_codex_queue",
    receipt,
    screen: null,
    ...(status === "ok" || status === "queued" ? { note: "已补投为消息，请回 APP 核对题面" } : {}),
  };
}

/** 最近一次 POST /session-question/answer 的请求体（JSON 解析） */
function lastAnswerBody(): Record<string, unknown> {
  const call = fetchMock.mock.calls.find(
    ([u, init]) => String(u).includes("/session-question/answer") && init?.method === "POST"
  );
  expect(call).toBeTruthy();
  return JSON.parse(String(call?.[1]?.body)) as Record<string, unknown>;
}

describe("QuestionCardApp：APP 形态问答卡（Task 8 · 补投作答）", () => {
  it("question_card_app_repost_single_choice_template：单选点选 → select{index}，预告「选择第 N 项：<文本>」", async () => {
    installFetch();
    render(<QuestionCardApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("question-app-option-0-0"));
    // 请求形状：action=select + index（模板由后端组装，前端只发语义参数）
    expect(lastAnswerBody()).toEqual({
      sessionId: "s1",
      action: "select",
      index: 0,
      questionIndex: 0,
    });
    // 展示镜像（前端预告文案，与后端 codex_app_answer_repost_text 同模板）
    expect((await screen.findByTestId("question-app-progress")).textContent).toContain(
      "选择第 1 项：Tool demo"
    );
    // 回执 note（ok/queued 附带；原样透出不另编）
    expect((await screen.findByTestId("question-app-note")).textContent).toBe(
      "已补投为消息，请回 APP 核对题面"
    );
  });

  it("question_card_app_repost_free_text_template：自由作答原文补投（freeText{text}）", async () => {
    installFetch();
    render(<QuestionCardApp session={{ id: "s1" }} />);
    fireEvent.change(await screen.findByTestId("question-app-freetext-input-0"), {
      target: { value: "我的自由作答" },
    });
    fireEvent.click(screen.getByTestId("question-app-freetext-send-0"));
    // 请求形状：freeText 原文（后端原样补投，不加模板包装）
    expect(lastAnswerBody()).toEqual({
      sessionId: "s1",
      action: "freeText",
      text: "我的自由作答",
      questionIndex: 0,
    });
    expect((await screen.findByTestId("question-app-progress")).textContent).toContain(
      "我的自由作答"
    );
    expect((await screen.findByTestId("question-app-note")).textContent).toContain(
      "请回 APP 核对题面"
    );
  });

  it("question_card_app_repost_multi_select_toggle_template：多选逐项补投 → toggle{index}，预告「勾选第 N 项：<文本>」", async () => {
    installFetch();
    routes.question = {
      ...singleChoicePayload(),
      questions: [
        {
          header: "Scope",
          question: "覆盖哪些范围？",
          multiSelect: true,
          options: [
            { label: "仅文档", description: "" },
            { label: "含测试", description: "" },
          ],
        },
      ],
    };
    render(<QuestionCardApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("question-app-option-0-1"));
    // 请求形状（spec 审 m-2）：多选逐项 = toggle + index + questionIndex
    expect(lastAnswerBody()).toEqual({
      sessionId: "s1",
      action: "toggle",
      index: 1,
      questionIndex: 0,
    });
    // 预告镜像与后端 toggle 模板同形（勾选第 N 项：<文本>）
    expect((await screen.findByTestId("question-app-progress")).textContent).toContain(
      "勾选第 2 项：含测试"
    );
    // 多选如实边界词条在场（取消勾选请在 APP 内操作）
    expect(screen.getByTestId("question-app-uncheck-hint-0").textContent).toContain(
      "取消勾选请在 APP 内操作"
    );
  });

  it("question_card_app_receipt_never_claims_answered_and_screen_null：回执不冒充已作答、无翻转", async () => {
    installFetch();
    render(<QuestionCardApp session={{ id: "s1" }} />);
    const opt = await screen.findByTestId("question-app-option-0-0");
    fireEvent.click(opt);
    await screen.findByTestId("question-app-note");
    // 红线（设计 §3.3）：永不声称已作答——回执只说「已补投为消息」+ 引导核对；
    // screen=null 不进任何翻转/勾选状态（选项按钮无 data-checked）
    expect(screen.getByTestId("question-app-note").textContent).toContain("已补投为消息");
    expect(screen.getByTestId("question-app-note").textContent).not.toContain("已作答");
    for (const t of ["question-app-option-0-0", "question-app-option-0-1"]) {
      expect(screen.getByTestId(t).getAttribute("data-checked")).toBeNull();
    }
    const card = screen.getByTestId("question-card-app");
    expect(card.textContent).not.toContain("已作答");
    expect(card.textContent).not.toContain("已选中");
    // failed 回执如实（不带「已补投」note——不谎报送达）
    routes.answer = repostEnvelope("failed");
    fireEvent.click(screen.getByTestId("question-app-option-0-1"));
    expect((await screen.findByTestId("question-app-error")).textContent).toContain(
      "codex 协议通道不可达"
    );
    expect(screen.queryByTestId("question-app-note")).toBeNull();
  });

  // ===== R8 native_only 接线（2026-10-19 复审：i18n 孤儿键 errors.codexIpcNativeOnly
  // 的消费点）——后端对 APP 旧问答文本补投无条件 503 阻断，卡内必须显示引导文案
  // 而不是带机器前缀的 reason 原文 =====
  it("question_card_app_native_only_refusal_shows_i18n_guidance：503 native_only → 引导文案，不带 CODEX_IPC_NATIVE_ONLY 机器前缀", async () => {
    installFetch();
    routes.answerStatus = 503;
    // 后端 api.rs AppRepost 拒绝臂的真实载荷形状（error/reasonCode/reason 三件套）
    routes.answer = {
      error: "capability_unavailable",
      reasonCode: "native_only",
      reason:
        "CODEX_IPC_NATIVE_ONLY: 该 Codex 会话已支持原生远程控制，问答与审批请使用原生卡片（自然语言补投不再受理）",
    };
    render(<QuestionCardApp session={{ id: "s1" }} />);
    fireEvent.click(await screen.findByTestId("question-app-option-0-0"));
    const err = await screen.findByTestId("question-app-error");
    // i18n 单点文案（zh 逐字 = errors.codexIpcNativeOnly），机器前缀不直出
    expect(err.textContent).toBe(
      "该 Codex 会话已支持原生远程控制，问答与审批请使用原生卡片（自然语言补投不再受理）"
    );
    expect(err.textContent).not.toContain("CODEX_IPC_NATIVE_ONLY:");
  });

  // ===== 收口 I-1：换题指纹重置（与 CLI 套 F2-1 同款判据）=====
  // 同一会话可连续多次提问（CLI 套实测 rollout-2026-09-21 单会话连续 8 次）而组件
  // key 随 session.id 不重挂 → 重拉 effect 此前只清 error：载荷换成新题时旧 note
  // 谎称「已补投」（那是旧题的回执）、旧草稿灌进新题输入框、旧「最近补投」预览留痕。
  it("question_card_app_fingerprint_reset_on_question_swap：同会话换题 → 旧 note/草稿/预览清空", async () => {
    installFetch();
    routes.question = singleChoicePayload();
    const { rerender } = render(<QuestionCardApp session={{ id: "s1", status: "waiting" }} />);
    // 第一题：留草稿 + 补投一拍（note + 最近补投预览在场）
    const draft = (await screen.findByTestId("question-app-freetext-input-0")) as HTMLInputElement;
    fireEvent.change(draft, { target: { value: "旧题的草稿" } });
    fireEvent.click(screen.getByTestId("question-app-option-0-0"));
    await screen.findByTestId("question-app-note");
    expect(screen.getByTestId("question-app-progress").textContent).toContain("最近补投");
    // 同一载荷重复拉取（状态跃迁 waiting→processing）：**不**清——补投痕迹属于本轮
    rerender(<QuestionCardApp session={{ id: "s1", status: "processing" }} />);
    await screen.findByTestId("question-app-q-0");
    expect(screen.getByTestId("question-app-note")).toBeTruthy();
    // 换题（题干+选项摘要变）→ 旧 note/预览/草稿一并清空（不把旧题回执带进新题）
    routes.question = {
      ...singleChoicePayload(),
      questions: [
        {
          header: "Next step",
          question: "换了一道新题：测试范围怎么选？",
          multiSelect: false,
          options: [
            { label: "仅单元", description: "" },
            { label: "含集成", description: "" },
            { label: "全量", description: "" },
          ],
        },
      ],
    };
    rerender(<QuestionCardApp session={{ id: "s1", status: "idle" }} />);
    await screen.findByTestId("question-app-text-0");
    expect(screen.getByTestId("question-app-text-0").textContent).toContain("换了一道新题");
    expect(screen.queryByTestId("question-app-note")).toBeNull();
    expect(screen.queryByTestId("question-app-progress")).toBeNull();
    expect((screen.getByTestId("question-app-freetext-input-0") as HTMLInputElement).value).toBe(
      ""
    );
  });
});
