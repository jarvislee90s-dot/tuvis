import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import AsyncQuestionGroupCard from "@/mobile/AsyncQuestionGroupCard";
import type { CodexAsyncQuestionView } from "@/mobile/codexIpcApi";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function question(
  index: number,
  over: Partial<CodexAsyncQuestionView> = {}
): CodexAsyncQuestionView {
  return {
    questionKey: `["request_user_input_async","call-g",${index}]`,
    contextToken: `ctx:${index}`,
    sourceTurnId: "t1",
    sourceItemId: "call-g",
    questionIndex: index,
    title: `问题${index + 1}`,
    options: ["甲", "乙"],
    state: "pending",
    actionable: true,
    disabledReasonCode: null,
    ...over,
  };
}

const five = () => [0, 1, 2, 3, 4].map((i) => question(i));

let fetchMock: ReturnType<typeof vi.fn>;
let respondStatus = 200;
let respondBody: Record<string, unknown> = {};
let verifyBody: Record<string, unknown> = {
  actionId: "act-g",
  stage: "settled",
  outcome: "unresolved",
  turnId: null,
  runLimitReleased: false,
  reasonCode: null,
  verifiedAt: 1,
  answeredQuestionKeys: [],
};

beforeEach(() => {
  respondStatus = 200;
  verifyBody = {
    actionId: "act-g",
    stage: "settled",
    outcome: "unresolved",
    turnId: null,
    runLimitReleased: false,
    reasonCode: null,
    verifiedAt: 1,
    answeredQuestionKeys: [],
  };
  respondBody = {
    actionId: "act-g",
    delivery: "accepted",
    route: "desktop_ipc",
    turnId: "turn-g",
    envelopeId: "env-g",
    reasonCode: null,
  };
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/codex/action/verify")) {
      return new Response(JSON.stringify(verifyBody), { status: 200 });
    }
    return respondStatus === 200
      ? new Response(JSON.stringify(respondBody), { status: 200 })
      : new Response(
          JSON.stringify({
            error: respondStatus === 409 ? "question_already_answered" : "internal",
          }),
          { status: respondStatus }
        );
  });
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  vi.useRealTimers();
  window.sessionStorage.clear();
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function posts(): { url: string; body: Record<string, unknown> }[] {
  return fetchMock.mock.calls
    .filter(([input]) => String(input).includes("/async-answer-group"))
    .map(([input, init]) => ({
      url: String(input),
      body: JSON.parse(String((init as RequestInit).body)) as Record<string, unknown>,
    }));
}

describe("B02 统一蓝卡", () => {
  it("uses the shared InteractiveCard question tone and numbered rows", () => {
    render(<SessionDetailGroup />);
    const card = screen.getByTestId("codex-async-question");
    expect(card.getAttribute("data-tone")).toBe("question");
    expect(card.className).toContain("border-sky-500/60");
    const option = screen.getByRole("button", { name: "甲" });
    expect(option.className).toContain("bg-[var(--btnp)]/5");
  });
});

describe("C03/C04 可见选中与实发答案一致", () => {
  it("single_select_has_visible_selected_style_and_survives_return", () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={five()} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    const selected = screen.getByRole("button", { name: "甲" });
    expect(selected.className).toContain("ring-1");
    expect(selected.className).toContain("bg-[var(--btnp)]/15");
    expect(screen.getByRole("button", { name: "乙" }).className).toContain("bg-[var(--btnp)]/5");
    fireEvent.click(screen.getByTestId("codex-async-next"));
    fireEvent.click(screen.getByTestId("codex-async-prev"));
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("last_option_click_overrides_old_free_text_in_submitted_answer", async () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    // T04：自由填写默认收起——先展开再输入
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    fireEvent.change(screen.getByTestId("codex-async-free"), {
      target: { value: "旧的自由答案" },
    });
    fireEvent.click(screen.getByRole("button", { name: "乙" }));
    // 可见模式切到选项，旧文本不再作为有效答案。
    expect(screen.getByRole("button", { name: "乙" }).getAttribute("aria-pressed")).toBe("true");
    expect((screen.getByTestId("codex-async-free") as HTMLInputElement).value).toBe("");
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(posts()).toHaveLength(1));
    const answers = posts()[0].body.answers as { answer: string }[];
    expect(answers[0].answer).toBe("乙");
  });

  it("free_text_after_option_click_wins_and_clears_visible_selection", async () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    // T04：自由填写默认收起——点「其他答案」展开后输入
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    fireEvent.change(screen.getByTestId("codex-async-free"), {
      target: { value: "新的自由答案" },
    });
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(posts()).toHaveLength(1));
    const answers = posts()[0].body.answers as { answer: string }[];
    expect(answers[0].answer).toBe("新的自由答案");
  });
});

describe("B04/B05 同组多题单卡一次提交", () => {
  it("b04_one_card_progress_and_prev_next_keep_drafts", () => {
    render(<SessionDetailGroup />);
    expect(screen.getByTestId("codex-async-question").getAttribute("data-progress")).toBe("1/5");
    expect(screen.getByText("问题1")).toBeTruthy();
    expect(screen.queryByText("问题2")).toBeNull();
    const next = screen.getByTestId("codex-async-next") as HTMLButtonElement;
    expect(next.disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(next);
    expect(screen.getByTestId("codex-async-question").getAttribute("data-progress")).toBe("2/5");
    fireEvent.click(screen.getByRole("button", { name: "乙" }));
    fireEvent.click(screen.getByTestId("codex-async-prev"));
    // 返回第 1 题仍保留选择
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("true");
    // 前 4 题零 POST
    expect(posts()).toHaveLength(0);
  });

  it("b05_final_click_posts_group_once_with_all_answers", async () => {
    render(<SessionDetailGroup />);
    for (let i = 0; i < 4; i += 1) {
      fireEvent.click(screen.getByRole("button", { name: "甲" }));
      fireEvent.click(screen.getByTestId("codex-async-next"));
    }
    fireEvent.click(screen.getByRole("button", { name: "乙" }));
    const submit = screen.getByTestId("codex-async-submit") as HTMLButtonElement;
    expect(submit.textContent).toContain("提交全部答案");
    fireEvent.click(submit);
    fireEvent.click(submit); // 双击：同轮只发一次
    await waitFor(() => expect(posts()).toHaveLength(1));
    const body = posts()[0].body as {
      actionId: string;
      answers: { questionKey: string; contextToken: string; answer: string }[];
      frontBudgetMs: number;
    };
    expect(body.actionId).toBeTruthy();
    expect(body.answers).toHaveLength(5);
    expect(body.answers.map((entry) => entry.answer)).toEqual(["甲", "甲", "甲", "甲", "乙"]);
    expect(body.answers.map((entry) => entry.questionKey)).toEqual(
      five().map((entry) => entry.questionKey)
    );
    expect(typeof body.frontBudgetMs).toBe("number");
    expect(JSON.stringify(body)).not.toContain("bypassPausedQueue");
  });

  it("b06_paused_group_has_single_submit_entry_without_bypass_button", () => {
    const paused = five().map((q) => ({
      ...q,
      actionable: false,
      state: "temporarily_blocked" as const,
      disabledReasonCode: "operation_queue_paused",
    }));
    render(
      <AsyncQuestionGroupCard sessionId="s1" questions={paused} onChanged={() => undefined} />
    );
    expect(screen.getByTestId("codex-async-pause-note")).toBeTruthy();
    expect(screen.queryByTestId("codex-async-bypass")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-next"));
  });

  it("b05_already_answered_is_pre_delivery_not_unknown", async () => {
    respondStatus = 409;
    render(<SessionDetailGroup />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    for (let i = 0; i < 4; i += 1) {
      fireEvent.click(screen.getByTestId("codex-async-next"));
      fireEvent.click(screen.getByRole("button", { name: "甲" }));
    }
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    expect(screen.queryByTestId("codex-async-verify")).toBeNull();
    expect(screen.getByTestId("codex-async-note").textContent).not.toContain("结果未知");
  });

  it("b05_unknown_verifies_without_resend_and_retires_anchor", async () => {
    respondStatus = 500;
    render(<SessionDetailGroup />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    for (let i = 0; i < 4; i += 1) {
      fireEvent.click(screen.getByTestId("codex-async-next"));
      fireEvent.click(screen.getByRole("button", { name: "甲" }));
    }
    // 503 不是“投递前明确拒绝”白名单 → 结果未知，只能核对
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    respondStatus = 200;
    verifyBody = {
      actionId: "act-g",
      stage: "settled",
      outcome: "not_delivered",
      turnId: null,
      runLimitReleased: false,
      reasonCode: "stale_context",
      verifiedAt: 1,
      answeredQuestionKeys: [],
    };
    fireEvent.click(screen.getByTestId("codex-async-verify"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    // 重挂不得复活已退休动作
    cleanup();
    render(<SessionDetailGroup />);
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
    expect(posts()).toHaveLength(1);
  });
});

function SessionDetailGroup() {
  return <AsyncQuestionGroupCard sessionId="s1" questions={five()} />;
}

describe("G03/G09 组身份与提交状态机", () => {
  it("g03_non_first_member_change_requires_reconfirmation", () => {
    const { rerender } = render(<AsyncQuestionGroupCard sessionId="s1" questions={five()} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" })); // q0
    fireEvent.click(screen.getByTestId("codex-async-next"));
    fireEvent.click(screen.getByRole("button", { name: "甲" })); // q1
    // 只改第二题的身份（token + 题干）
    const changed = five().map((q, i) =>
      i === 1 ? { ...q, contextToken: "ctx:1-new", title: "问题2已更新" } : q
    );
    rerender(<AsyncQuestionGroupCard sessionId="s1" questions={changed} />);
    expect(screen.getByTestId("codex-async-stale-note")).toBeTruthy();
    // q0 旧稿保留；q1 旧答案不得带入新题。
    expect(screen.getByTestId("codex-async-question").getAttribute("data-progress")).toBe("1/5");
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByTestId("codex-async-next"));
    expect(screen.getByText("问题2已更新")).toBeTruthy();
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("false");
    expect((screen.getByTestId("codex-async-next") as HTMLButtonElement).disabled).toBe(true);
    // 新题必须重新填写后才能继续。
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    expect((screen.getByTestId("codex-async-next") as HTMLButtonElement).disabled).toBe(false);
  });

  it("g09_accepted_is_not_completion_and_waits_for_answers", async () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe(
        "submitted"
      )
    );
    expect(screen.queryByTestId("codex-async-done")).toBeNull();
    expect(screen.getByTestId("codex-async-waiting")).toBeTruthy();
  });

  it("g09_unknown_cannot_resubmit", async () => {
    respondStatus = 500;
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    const submit = screen.getByTestId("codex-async-submit") as HTMLButtonElement;
    fireEvent.click(submit);
    expect(posts()).toHaveLength(1);
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
  });

  it("g09_confirmed_notsent_after_deadline_allows_retry", async () => {
    vi.useFakeTimers();
    let resolveFirst: ((r: Response) => void) | null = null;
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/codex/action/verify")) {
        return new Response(JSON.stringify(verifyBody), { status: 200 });
      }
      if (url.includes("/async-answer-group")) {
        if (!resolveFirst) {
          return new Promise<Response>((resolve) => {
            resolveFirst = resolve;
          });
        }
        return new Response(JSON.stringify(respondBody), { status: 200 });
      }
      return new Response("{}", { status: 200 });
    });
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown");
    verifyBody = {
      actionId: "act-g",
      stage: "settled",
      outcome: "not_delivered",
      turnId: null,
      runLimitReleased: false,
      reasonCode: "stale_context",
      verifiedAt: 1,
      answeredQuestionKeys: [],
    };
    fireEvent.click(screen.getByTestId("codex-async-verify"));
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    vi.useRealTimers();
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    // 明确未投递后可换新 ID 重试，且草稿仍在。
    const submit = screen.getByTestId("codex-async-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    await act(async () => {
      await Promise.resolve();
    });
    expect(posts()).toHaveLength(2);
    expect(posts()[0].body.actionId).not.toBe(posts()[1].body.actionId);
    vi.useRealTimers();
    void resolveFirst;
  });
});

describe("J03—J09 核对/复核/草稿恢复", () => {
  it("j09_normal_group_has_no_pause_warning_and_paused_has_exactly_one", () => {
    const { unmount } = render(<AsyncQuestionGroupCard sessionId="s1" questions={five()} />);
    expect(screen.queryByTestId("codex-async-pause-note")).toBeNull();
    unmount();
    const paused = five().map((q) => ({
      ...q,
      actionable: false,
      state: "temporarily_blocked" as const,
      disabledReasonCode: "operation_queue_paused",
    }));
    render(<AsyncQuestionGroupCard sessionId="s1" questions={paused} />);
    expect(screen.getAllByTestId("codex-async-pause-note")).toHaveLength(1);
    expect(screen.queryByTestId("codex-async-blocked-reason")).toBeNull();
  });

  it("j03_accepted_verification_is_time_bounded", async () => {
    vi.useFakeTimers();
    let verifyCalls = 0;
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/codex/action/verify")) {
        verifyCalls += 1;
        return new Promise<Response>(() => {
          /* 永不返回 */
        });
      }
      return new Response(JSON.stringify(respondBody), { status: 200 });
    });
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("submitted");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(130_000);
    });
    expect(verifyCalls).toBeGreaterThan(0);
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown");
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
  });

  it("j04_late_verification_cannot_relock_retired_action", async () => {
    respondStatus = 500; // 先进入 unknown，保留锚
    let firstResolve: ((r: Response) => void) | null = null;
    let verifyCalls = 0;
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/codex/action/verify")) {
        verifyCalls += 1;
        if (verifyCalls === 1) {
          return new Promise<Response>((resolve) => {
            firstResolve = resolve;
          });
        }
        return new Response(
          JSON.stringify({
            actionId: "act-g",
            stage: "settled",
            outcome: "not_delivered",
            turnId: null,
            runLimitReleased: false,
            reasonCode: "stale_context",
            verifiedAt: 1,
            answeredQuestionKeys: [],
          }),
          { status: 200 }
        );
      }
      return new Response(JSON.stringify({ error: "internal" }), { status: 500 });
    });
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    const verify = screen.getByTestId("codex-async-verify");
    fireEvent.click(verify);
    fireEvent.click(verify);
    await waitFor(() => expect(verifyCalls).toBe(2));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    // 先发请求的旧 unresolved 迟到，不得重新锁死。
    await act(async () => {
      firstResolve?.(
        new Response(
          JSON.stringify({
            actionId: "act-g",
            stage: "settled",
            outcome: "unresolved",
            turnId: null,
            runLimitReleased: false,
            reasonCode: null,
            verifiedAt: 2,
            answeredQuestionKeys: [],
          }),
          { status: 200 }
        )
      );
      await Promise.resolve();
    });
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
    expect(screen.queryByTestId("codex-async-verify")).toBeNull();
  });

  it("j05_newly_answered_member_requires_its_own_confirmation", () => {
    const q0Answered = { ...question(0), state: "answered" as const, actionable: false };
    const { rerender } = render(
      <AsyncQuestionGroupCard sessionId="s1" questions={[q0Answered, question(1), question(2)]} />
    );
    fireEvent.click(screen.getByTestId("codex-async-partial-confirm"));
    // 新增 q1 已答 → 旧确认不能覆盖新成员。
    const q1Answered = { ...question(1), state: "answered" as const, actionable: false };
    rerender(
      <AsyncQuestionGroupCard sessionId="s1" questions={[q0Answered, q1Answered, question(2)]} />
    );
    const confirm = screen.getByTestId("codex-async-partial-confirm");
    expect(confirm.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(confirm);
    expect(screen.getByTestId("codex-async-partial-confirm").getAttribute("aria-pressed")).toBe(
      "true"
    );
  });

  it("r01_removed_option_restores_as_visible_text_not_invisible_answer", async () => {
    const { rerender } = render(
      <AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />
    );
    fireEvent.click(screen.getByRole("button", { name: "乙" }));
    rerender(
      <AsyncQuestionGroupCard
        sessionId="s1"
        questions={[question(0, { contextToken: "ctx:0-new", options: ["丙", "丁"] })]}
      />
    );
    fireEvent.click(screen.getByTestId("codex-async-stale-toggle"));
    fireEvent.click(screen.getByTestId("codex-async-stale-restore"));
    // 旧选项不再存在 → 旧标签进入可见自由输入，没有任何隐形 selected。
    expect((screen.getByTestId("codex-async-free") as HTMLInputElement).value).toBe("乙");
    expect(screen.getByRole("button", { name: "丙" }).getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(posts()).toHaveLength(1));
    const answers = posts()[0].body.answers as { answer: string }[];
    expect(answers[0].answer).toBe("乙");
  });

  it("r01_restore_keeps_selected_when_old_option_still_exists", () => {
    const { rerender } = render(
      <AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />
    );
    fireEvent.click(screen.getByRole("button", { name: "乙" }));
    rerender(
      <AsyncQuestionGroupCard
        sessionId="s1"
        questions={[question(0, { contextToken: "ctx:0-reset" })]}
      />
    );
    fireEvent.click(screen.getByTestId("codex-async-stale-toggle"));
    fireEvent.click(screen.getByTestId("codex-async-stale-restore"));
    expect(screen.getByRole("button", { name: "乙" }).getAttribute("aria-pressed")).toBe("true");
    // T04：恢复的是选项稿（无自由文本）→ 自由填写保持收起；展开后确认为空。
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    expect((screen.getByTestId("codex-async-free") as HTMLInputElement).value).toBe("");
  });

  it("j06_changed_member_draft_can_be_viewed_and_restored", () => {
    const { rerender } = render(<AsyncQuestionGroupCard sessionId="s1" questions={five()} />);
    fireEvent.click(screen.getByTestId("codex-async-next")); // q0 未答，next 禁用
    fireEvent.click(screen.getByRole("button", { name: "甲" })); // q0
    fireEvent.click(screen.getByTestId("codex-async-next"));
    // T04：自由填写默认收起——先展开再写稿
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    fireEvent.change(screen.getByTestId("codex-async-free"), {
      target: { value: "第二题的旧长答案" },
    });
    const changed = five().map((q, i) =>
      i === 1 ? { ...q, contextToken: "ctx:1-new", title: "问题2已更新" } : q
    );
    rerender(<AsyncQuestionGroupCard sessionId="s1" questions={changed} />);
    expect(screen.getByTestId("codex-async-stale-note")).toBeTruthy();
    fireEvent.click(screen.getByTestId("codex-async-stale-toggle"));
    expect(screen.getByTestId("codex-async-stale-item").textContent).toContain("第二题的旧长答案");
    fireEvent.click(screen.getByTestId("codex-async-stale-restore"));
    expect(screen.queryByTestId("codex-async-stale-item")).toBeNull();
    // 恢复后草稿进入当前 q1（仍需用户显式操作，不自动提交）。
    fireEvent.click(screen.getByRole("button", { name: "甲" })); // q0 若未选则选上
    fireEvent.click(screen.getByTestId("codex-async-next"));
    expect((screen.getByTestId("codex-async-free") as HTMLTextAreaElement).value).toBe(
      "第二题的旧长答案"
    );
  });
});

// ============================================================
// T04：紧凑题卡（spec §3.4–§3.6）
// - 没有回执不留空行；
// - 自由填写默认收起、点「其他答案」展开、有自由稿自动展开；收起/展开不丢稿；
// - 长题干默认三行可展开；长选项组内部滚动；单题不显示翻页说明；
// - 题干/选项内容在字号档位作用域内（14px 基准 = text-sm 相对单位），
//   主按钮不进入该作用域（稳定操作尺寸）。
// ============================================================
describe("T04 紧凑题卡", () => {
  it("t04_no_receipt_renders_no_placeholder_note_line", () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={five()} />);
    expect(screen.queryByTestId("codex-async-note")).toBeNull();
  });

  it("t04_free_answer_collapsed_by_default_toggle_keeps_draft_on_collapse", () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    // 默认收起
    expect(screen.queryByTestId("codex-async-free")).toBeNull();
    // 点「其他答案」展开
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    const input = screen.getByTestId("codex-async-free") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "自由草稿" } });
    // 收起不丢稿
    fireEvent.click(screen.getByTestId("codex-async-free-collapse"));
    expect(screen.queryByTestId("codex-async-free")).toBeNull();
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    expect((screen.getByTestId("codex-async-free") as HTMLInputElement).value).toBe("自由草稿");
  });

  it("t04_long_title_clamped_three_lines_and_expandable", () => {
    // 集中审查 D4 修订：展开入口改由**实测溢出**驱动（字符数判据废止）——
    // jsdom 无布局引擎，按仓库惯例挂载前在 Element 原型注入几何
    // （SessionDetail.test.tsx 同款；afterEach restoreAllMocks 自动还原）：
    // scrollHeight 63 > clientHeight 42 = 三行装不下（mock 四行内容）。
    vi.spyOn(Element.prototype, "scrollHeight", "get").mockReturnValue(63);
    vi.spyOn(Element.prototype, "clientHeight", "get").mockReturnValue(42);
    const longTitle = "这是一个特别长的题干".repeat(12);
    render(
      <AsyncQuestionGroupCard sessionId="s1" questions={[question(0, { title: longTitle })]} />
    );
    const title = screen.getByText(longTitle);
    expect(title.className).toContain("line-clamp-3");
    fireEvent.click(screen.getByTestId("codex-async-title-toggle"));
    expect(screen.getByText(longTitle).className).not.toContain("line-clamp-3");
    fireEvent.click(screen.getByTestId("codex-async-title-toggle"));
    expect(screen.getByText(longTitle).className).toContain("line-clamp-3");
  });

  it("t04_short_title_with_measured_overflow_gets_expand_entry_no_post_no_draft_loss", () => {
    // 审查点名场景：≤80 字符的题干在窄屏/125% 字号下也可能超过三行——入口必须由
    // 实测溢出（scrollHeight > clientHeight）驱动，而不是字符数。短题干 + mock 溢出
    // → 入口在场可展开；展开不触发任何 POST、不丢自由稿；收起回三行。
    vi.spyOn(Element.prototype, "scrollHeight", "get").mockReturnValue(63);
    vi.spyOn(Element.prototype, "clientHeight", "get").mockReturnValue(42);
    const shortTitle = "这是一个不到八十字符但窄屏会超过三行的题干";
    expect(shortTitle.length).toBeLessThanOrEqual(80);
    render(
      <AsyncQuestionGroupCard sessionId="s1" questions={[question(0, { title: shortTitle })]} />
    );
    // 恒三行收纳（展开前），实测溢出 → 展开入口在场
    const title = screen.getByText(shortTitle);
    expect(title.className).toContain("line-clamp-3");
    fireEvent.click(screen.getByTestId("codex-async-title-toggle"));
    expect(screen.getByText(shortTitle).className).not.toContain("line-clamp-3");
    // 展开动作零 POST（纯本地显隐）
    expect(posts()).toHaveLength(0);
    // 展开不丢稿：写自由稿 → 收起回三行 → 稿仍在（有自由稿时输入区自动展开，
    // 无需再点 free-toggle——T04 既有行为）
    fireEvent.click(screen.getByTestId("codex-async-free-toggle"));
    fireEvent.change(screen.getByTestId("codex-async-free"), { target: { value: "自由稿" } });
    fireEvent.click(screen.getByTestId("codex-async-title-toggle"));
    expect(screen.getByText(shortTitle).className).toContain("line-clamp-3");
    expect((screen.getByTestId("codex-async-free") as HTMLInputElement).value).toBe("自由稿");
    expect(posts()).toHaveLength(0);
  });

  it("t04_short_title_has_no_expand_toggle_and_options_area_scrollable", () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    expect(screen.queryByTestId("codex-async-title-toggle")).toBeNull();
    // 长选项组内部滚动（spec §3.6）
    const wrapper = screen.getByTestId("codex-async-current");
    const optionsBox = wrapper.querySelector(".overflow-y-auto");
    expect(optionsBox).not.toBeNull();
  });

  it("t04_single_question_hides_pagination_and_turn_hint", () => {
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} />);
    expect(screen.queryByTestId("codex-async-progress")).toBeNull();
    expect(screen.queryByTestId("codex-async-prev")).toBeNull();
    expect(screen.queryByTestId("codex-async-next")).toBeNull();
    expect(screen.getByTestId("codex-async-question").textContent).not.toContain("翻题");
  });

  it("t04_question_content_follows_font_scale_while_main_button_stays_fixed", () => {
    // 单题组：提交主按钮直接在场（多题组此处是「下一题」，属动作钮同样不跟随）
    render(<AsyncQuestionGroupCard sessionId="s1" questions={[question(0)]} fontScale={1.25} />);
    const current = screen.getByTestId("codex-async-current");
    // 内容作用域：题干/选项/自由输入在 [data-font-scale] 内（相对档位单位 text-sm）
    const scope = current.querySelector("[data-font-scale]");
    expect(scope?.getAttribute("data-font-scale")).toBe("1.25");
    const title = within(current).getByText("问题1");
    expect(scope!.contains(title)).toBe(true);
    expect(title.className).toContain("text-sm");
    expect(title.className).not.toMatch(/text-\[\d+px\]/);
    const option = screen.getByRole("button", { name: "甲" });
    // 选项在（同档位的）字号作用域内；选项文字缩放、按钮 padding 保持 px
    expect(option.closest("[data-font-scale]")?.getAttribute("data-font-scale")).toBe("1.25");
    // 主按钮不跟随字号档位（保持固定操作尺寸）
    expect(screen.getByTestId("codex-async-submit").closest("[data-font-scale]")).toBeNull();
    // 卡头（页头）不跟随
    expect(
      screen.getByTestId("codex-async-question").querySelector(".text-sm.font-semibold")
    ).not.toBeNull();
    expect(screen.getByText("待作答（异步）").closest("[data-font-scale]")).toBeNull();
  });
});
