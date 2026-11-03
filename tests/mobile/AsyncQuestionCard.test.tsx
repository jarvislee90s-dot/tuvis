import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import AsyncQuestionCard from "@/mobile/AsyncQuestionCard";
import type { CodexAsyncQuestionView } from "@/mobile/codexIpcApi";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function question(over: Partial<CodexAsyncQuestionView> = {}): CodexAsyncQuestionView {
  return {
    questionKey: '["request_user_input_async","call-a",0]',
    contextToken: "ctx:1",
    sourceTurnId: "t1",
    sourceItemId: "call-a",
    questionIndex: 0,
    title: "选一个",
    options: ["甲", "乙"],
    state: "pending",
    actionable: true,
    disabledReasonCode: null,
    ...over,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;
let respondStatus = 200;
let respondBody: Record<string, unknown> = {};

beforeEach(() => {
  respondStatus = 200;
  respondBody = {
    actionId: "act-1",
    delivery: "accepted",
    route: "desktop_ipc",
    turnId: "turn-1",
    envelopeId: "env-1",
    reasonCode: null,
  };
  fetchMock = vi.fn(async () =>
    respondStatus === 200
      ? new Response(JSON.stringify(respondBody), { status: 200 })
      : new Response(
          JSON.stringify(respondStatus >= 500 ? { error: "internal" } : { error: "stale_context" }),
          { status: respondStatus }
        )
  );
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  window.sessionStorage.clear();
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("F03/T09 异步题卡", () => {
  it("idle_answer_posts_strict_body：只发三个字段 + 成功后回调", async () => {
    const onChanged = vi.fn();
    render(<AsyncQuestionCard sessionId="s1" question={question()} onChanged={onChanged} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(onChanged).toHaveBeenCalled());
    const call = fetchMock.mock.calls[0];
    const url = String(call[0]);
    expect(url).toContain("/m/api/v1/codex/async-answer");
    const body = JSON.parse(String((call[1] as RequestInit).body)) as Record<string, unknown>;
    expect(Object.keys(body).sort()).toEqual([
      "actionId",
      "answer",
      "contextToken",
      "frontBudgetMs",
      "questionKey",
      "sessionId",
    ]);
    expect(body.answer).toBe("甲");
    expect(body.questionKey).toBe(question().questionKey);
    // R13：受理 ≠ 已答——本地只进入"已提交待确认"，不宣布已回答。
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe(
        "submitted"
      )
    );
    expect(screen.queryByTestId("codex-async-answered")).toBeNull();
  });

  it("blocked_keeps_draft_and_never_submits：运行中保留草稿、按钮禁用、显示原因", async () => {
    render(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({
          options: [],
          state: "temporarily_blocked",
          actionable: false,
          disabledReasonCode: "operation_runtime_active",
        })}
      />
    );
    const input = screen.getByLabelText("输入答案");
    fireEvent.change(input, { target: { value: "我的答案" } });
    expect((input as HTMLTextAreaElement).value).toBe("我的答案");
    const submit = screen.getByTestId("codex-async-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    expect(screen.getByTestId("codex-async-blocked").textContent).toContain("暂不可提交");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("rejected_keeps_draft_and_explains：NotSent/冲突不清草稿、不自动重发", async () => {
    respondStatus = 409;
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    const input = screen.getByLabelText("输入答案");
    fireEvent.change(input, { target: { value: "保留我" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    expect(await screen.findByTestId("codex-async-error")).toBeTruthy();
    expect((input as HTMLTextAreaElement).value).toBe("保留我");
    expect(screen.getByTestId("codex-async-error").textContent).toContain("答案已保留");
  });

  it("unknown_state_does_not_offer_submit：历史不完整时不装可答按钮", () => {
    render(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({
          state: "unknown",
          actionable: false,
          disabledReasonCode: "async_answer_history_incomplete",
        })}
      />
    );
    expect((screen.getByTestId("codex-async-submit") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByTestId("codex-async-blocked").textContent).toContain("待核对");
  });
});

describe("F08/T11 迟回执不污染另一题", () => {
  it("late_failure_for_a_cannot_mutate_b_draft", async () => {
    // A 的请求挂起到"迟到"；期间用户在 B 上编辑草稿——A 的失败回执不得清 B。
    let resolveA: ((value: Response) => void) | null = null;
    const fetchMockLate = vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          resolveA = resolve;
        })
    );
    vi.stubGlobal("fetch", fetchMockLate);
    render(
      <>
        <AsyncQuestionCard
          sessionId="s1"
          question={question({ questionKey: "q-a", options: [], title: "A 题" })}
        />
        <AsyncQuestionCard
          sessionId="s1"
          question={question({ questionKey: "q-b", options: [], title: "B 题" })}
        />
      </>
    );
    const areas = screen.getAllByLabelText("输入答案");
    fireEvent.change(areas[0], { target: { value: "A 的答案" } });
    fireEvent.change(areas[1], { target: { value: "B 的草稿" } });
    const buttons = screen.getAllByTestId("codex-async-submit");
    fireEvent.click(buttons[0]);
    await waitFor(() => expect(fetchMockLate).toHaveBeenCalled());
    // A 迟到回执（失败）落地
    resolveA!(new Response(JSON.stringify({ error: "conflict" }), { status: 409 }));
    await waitFor(() => expect(screen.getAllByTestId("codex-async-error")).toHaveLength(1));
    // B 的草稿与输入框不受影响
    expect((screen.getAllByLabelText("输入答案")[1] as HTMLTextAreaElement).value).toBe("B 的草稿");
    // A 的失败只作用在 A 卡（错误文本出现在第一张卡内）
    const cards = screen.getAllByTestId("codex-async-question");
    expect(cards[0].querySelector("[data-testid='codex-async-error']")).toBeTruthy();
    expect(cards[1].querySelector("[data-testid='codex-async-error']")).toBeNull();
  });
});

// ============================================================
// R13/R14：交互状态机与输入方式
// ============================================================

describe("R13/R14 状态机", () => {
  it("r13_projection_answered_is_the_only_authority", async () => {
    // 本地已 Accepted（submitted），但投影仍是 pending → 不显示"已回答"
    const onChanged = vi.fn();
    const { rerender } = render(
      <AsyncQuestionCard sessionId="s1" question={question()} onChanged={onChanged} />
    );
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(onChanged).toHaveBeenCalled());
    expect(screen.queryByTestId("codex-async-answered")).toBeNull();
    // 投影转 answered（唯一权威）→ 清卡
    rerender(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({ state: "answered", actionable: false })}
      />
    );
    expect(screen.getByTestId("codex-async-answered")).toBeTruthy();
  });

  it("r14_options_and_free_text_are_both_available", async () => {
    render(<AsyncQuestionCard sessionId="s1" question={question()} />);
    // 有选项时自由作答输入框也在，且可编辑
    const area = screen.getByLabelText("输入答案") as HTMLTextAreaElement;
    expect(area.disabled).toBe(false);
    fireEvent.change(area, { target: { value: "甲（但我想补充说明）" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const body = JSON.parse(String((fetchMock.mock.calls[0][1] as RequestInit).body)) as Record<
      string,
      unknown
    >;
    expect(body.answer).toBe("甲（但我想补充说明）");
  });

  it("h10_not_sent_keeps_draft_and_next_click_uses_a_new_id", async () => {
    respondStatus = 200;
    respondBody = {
      actionId: "act-1",
      delivery: "not_sent",
      route: "none",
      turnId: null,
      envelopeId: null,
      reasonCode: "busy_before_write",
    };
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    const area = screen.getByLabelText("输入答案");
    fireEvent.change(area, { target: { value: "保留我" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    expect((area as HTMLTextAreaElement).value).toBe("保留我");
    // H10：明确零投递 = 该动作已终态 → 下一次点击**换新 ID**（同 ID 只会拿回旧终态）。
    respondBody = {
      actionId: "act-2",
      delivery: "accepted",
      route: "desktop_ipc",
      turnId: "turn-2",
      envelopeId: "env-2",
      reasonCode: null,
    };
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(fetchMock.mock.calls.length).toBe(2));
    const first = JSON.parse(String((fetchMock.mock.calls[0][1] as RequestInit).body)) as Record<
      string,
      unknown
    >;
    const second = JSON.parse(String((fetchMock.mock.calls[1][1] as RequestInit).body)) as Record<
      string,
      unknown
    >;
    expect(second.actionId).not.toBe(first.actionId);
  });

  it("h10_definitive_409_returns_to_idle_without_querying_fake_action", async () => {
    respondStatus = 409;
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "保稿" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    expect(screen.queryByTestId("codex-async-verify")).toBeNull();
    expect((screen.getByLabelText("输入答案") as HTMLTextAreaElement).value).toBe("保稿");
    expect(fetchMock.mock.calls.length).toBe(1);
  });

  it("h11_deadline_runs_from_click_even_when_post_hangs", async () => {
    vi.useFakeTimers();
    fetchMock.mockImplementation(() => new Promise<Response>(() => {}));
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "甲" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe(
      "submitting"
    );
    await act(async () => {
      vi.advanceTimersByTime(30_000);
    });
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown");
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
    expect(fetchMock.mock.calls.length).toBe(1);
    vi.useRealTimers();
  });

  it("h11_inflight_result_for_old_token_is_discarded", async () => {
    let resolvePost: ((value: Response) => void) | null = null;
    fetchMock.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          resolvePost = resolve;
        })
    );
    const onChanged = vi.fn();
    const { rerender } = render(
      <AsyncQuestionCard sessionId="s1" question={question()} onChanged={onChanged} />
    );
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    rerender(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({ contextToken: "ctx:2" })}
        onChanged={onChanged}
      />
    );
    await act(async () => {
      resolvePost?.(
        new Response(
          JSON.stringify({
            actionId: "act-1",
            delivery: "accepted",
            route: "desktop_ipc",
            turnId: "turn-1",
            envelopeId: "env-1",
            reasonCode: null,
          }),
          { status: 200 }
        )
      );
    });
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
  });

  it("r13_unknown_offers_readonly_verify_and_never_resubmits", async () => {
    respondStatus = 500;
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "甲" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    const callsBefore = fetchMock.mock.calls.length;
    // H11：核对走**只读核对**端点（10 秒有界、零发送），不查旧账本、不产生第二次提交
    respondStatus = 200;
    respondBody = {
      actionId: "act-1",
      stage: "settled",
      outcome: "unresolved",
      turnId: null,
      runLimitReleased: false,
      reasonCode: "unknown_delivery",
      verifiedAt: 1,
    };
    fireEvent.click(screen.getByTestId("codex-async-verify"));
    await waitFor(() => expect(fetchMock.mock.calls.length).toBe(callsBefore + 1));
    const verifyUrl = String(fetchMock.mock.calls[callsBefore][0]);
    expect(verifyUrl).toContain("/codex/action/verify");
    expect(verifyUrl).not.toContain("/async-answer");
  });

  it("r13_new_token_resets_old_accepted_state", async () => {
    const onChanged = vi.fn();
    const { rerender } = render(
      <AsyncQuestionCard sessionId="s1" question={question()} onChanged={onChanged} />
    );
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe(
        "submitted"
      )
    );
    // 同题换上下文 token：旧 Accepted 不得继续显示"待确认"
    rerender(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({ contextToken: "ctx:2" })}
        onChanged={onChanged}
      />
    );
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
  });
});

describe("H11/H12 复核补充", () => {
  it("h12_paused_question_requires_explicit_bypass_and_sends_once", async () => {
    render(
      <AsyncQuestionCard
        sessionId="s1"
        question={question({
          state: "temporarily_blocked",
          actionable: false,
          disabledReasonCode: "operation_queue_paused",
        })}
      />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "先发这题" } });
    // 默认：普通提交禁用（零帧）
    expect((screen.getByTestId("codex-async-submit") as HTMLButtonElement).disabled).toBe(true);
    const bypass = screen.getByTestId("codex-async-bypass") as HTMLButtonElement;
    expect(bypass.disabled).toBe(false);
    fireEvent.click(bypass);
    await waitFor(() => expect(fetchMock.mock.calls.length).toBe(1));
    const body = JSON.parse(String((fetchMock.mock.calls[0][1] as RequestInit).body)) as Record<
      string,
      unknown
    >;
    expect(body.bypassPausedQueue).toBe(true);
  });

  it("h11_submitted_keeps_readonly_verify_entry", async () => {
    render(<AsyncQuestionCard sessionId="s1" question={question()} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe(
        "submitted"
      )
    );
    // 受理后仍有核对入口（不宣布已答、不锁死）
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
  });

  it("h11_unknown_anchor_survives_remount_without_extra_post", async () => {
    respondStatus = 500;
    const { unmount } = render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "甲" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    const posts = fetchMock.mock.calls.length;
    unmount();
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    // 重进仍是 Unknown + 可核对；绝不自动重发
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown");
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
    expect(fetchMock.mock.calls.length).toBe(posts);
  });

  it("h10_machine_code_pre_delivery_refusal_returns_to_idle", async () => {
    respondStatus = 503;
    fetchMock.mockImplementation(
      async () => new Response(JSON.stringify({ error: "capability_unavailable" }), { status: 503 })
    );
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "保稿" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    expect(screen.queryByTestId("codex-async-verify")).toBeNull();
    expect((screen.getByLabelText("输入答案") as HTMLTextAreaElement).value).toBe("保稿");
    expect(fetchMock.mock.calls.length).toBe(1);
  });
});

describe("第四轮 async 拥有者（N12/N13/N14）", () => {
  it("n12_plain_projection_refresh_does_not_invalidate_inflight_post", async () => {
    let resolvePost: ((r: Response) => void) | null = null;
    fetchMock.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          resolvePost = resolve;
        })
    );
    const { rerender } = render(<AsyncQuestionCard sessionId="s1" question={question()} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    // 相同 token/内容的普通刷新（新对象）——不得作废在途拥有者
    rerender(<AsyncQuestionCard sessionId="s1" question={{ ...question() }} />);
    await act(async () => {
      resolvePost?.(
        new Response(
          JSON.stringify({
            actionId: "act-1",
            delivery: "not_sent",
            route: "none",
            turnId: null,
            envelopeId: null,
            reasonCode: "busy_before_write",
          }),
          { status: 200 }
        )
      );
    });
    // 迟回结果正确落地（明确零投递 → idle + 保稿）
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
    // 草稿（选中项）保留、可再次提交
    expect(screen.getByRole("button", { name: "甲" }).getAttribute("aria-pressed")).toBe("true");
    expect((screen.getByTestId("codex-async-submit") as HTMLButtonElement).disabled).toBe(false);
  });

  it("n13_settled_not_delivered_clears_persisted_anchor", async () => {
    respondStatus = 200;
    respondBody = {
      actionId: "act-9",
      delivery: "unknown_delivery",
      route: "desktop_ipc",
      turnId: null,
      envelopeId: "e9",
      reasonCode: "unknown_delivery",
    };
    const { unmount } = render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    fireEvent.change(screen.getByLabelText("输入答案"), { target: { value: "甲" } });
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("unknown")
    );
    // 核对到 settled not_delivered → 锚退休（内存 + 存储）
    respondStatus = 200;
    respondBody = {
      actionId: "act-9",
      stage: "settled",
      outcome: "not_delivered",
      turnId: null,
      runLimitReleased: false,
      reasonCode: "stale_context",
      verifiedAt: 1,
    };
    fireEvent.click(screen.getByTestId("codex-async-verify"));
    await waitFor(() =>
      expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle")
    );
    unmount();
    render(
      <AsyncQuestionCard sessionId="s1" question={question({ options: [], state: "pending" })} />
    );
    // 重挂不得恢复已退休动作
    expect(screen.getByTestId("codex-async-question").getAttribute("data-phase")).toBe("idle");
    expect(fetchMock.mock.calls.length).toBeLessThanOrEqual(3);
  });

  it("n14_async_body_carries_front_budget", async () => {
    render(<AsyncQuestionCard sessionId="s1" question={question()} />);
    fireEvent.click(screen.getByRole("button", { name: "甲" }));
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(0));
    const body = JSON.parse(String((fetchMock.mock.calls[0][1] as RequestInit).body)) as Record<
      string,
      unknown
    >;
    expect(typeof body.frontBudgetMs).toBe("number");
    expect(body.frontBudgetMs as number).toBeLessThanOrEqual(30_000);
  });
});
