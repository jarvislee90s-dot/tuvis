import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import QuestionSkipControls from "@/mobile/QuestionSkipControls";
import SessionDetail from "@/mobile/SessionDetail";
import {
  issueQuestionSkipDisplayOnly,
  openQuestionSkipPairing,
  questionSkipScopeKey,
} from "@/mobile/questionSkipOperations";
import { MockEventSource } from "./eventSourceMock";
import { questionSkipDetailFixture, questionSkipViews, response } from "./questionSkipFixtures";

const { group, active, skipped } = questionSkipViews("display-only-thread", "generation", 1);
const { question, session, projection, detailFetch } = questionSkipDetailFixture(group, {
  questionKey: "weather",
  title: "Weather?",
  options: ["Sunny"],
});
const callsTo = (mock: ReturnType<typeof vi.fn>, endpoint: string) =>
  mock.mock.calls.filter(([url]) => String(url).endsWith(`/${endpoint}`));
const skipReceipt = (init?: RequestInit) => ({
  actionId: JSON.parse(String(init?.body)).actionId,
  group,
  displayDecision: "skipped",
  nativeEffect: "unresolved",
  reasonCode: "binding_required",
  revision: 1,
});
const acknowledge = () => {
  for (const name of ["thread", "count", "draft"])
    fireEvent.click(screen.getByTestId(`question-skip-ack-${name}`));
};

beforeEach(() => {
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  localStorage.clear();
});

describe("display-only skip for a single source question", () => {
  it("offers an honest Tuvis-only action and issues an unbound skip with a restorable unresolved result", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/skip")) return response(skipReceipt(init));
      throw new Error(`unexpected request ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    render(
      <QuestionSkipControls sessionId="display-only-ready" view={active} questionTitle="Weather?" />
    );
    const action = screen.getByTestId("question-skip-display-only");
    const controls = screen.getByTestId("question-skip-controls");
    expect(controls.textContent).toMatch(/Tuvis/i);
    expect(controls.textContent).toMatch(
      /(?:不会|不會|不关闭|不關閉).{0,12}原生|(?:won't|will not|does not|doesn't|not)\s+close.{0,30}native/i
    );
    fireEvent.click(action);
    await screen.findByTestId("question-skip-effect-unresolved");
    expect(screen.getByTestId("question-skip-restore")).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = callsTo(fetchMock, "skip")[0];
    expect(String(url)).toMatch(/\/skip$/);
    expect(init?.method).toBe("POST");
    const body = JSON.parse(String(init?.body));
    expect(body).toMatchObject({ sessionId: "display-only-ready", group });
    expect(body.actionId).toEqual(expect.any(String));
    expect(body.bindingId == null).toBe(true);
  });

  it("keeps the display-only action available after an empty native preview without inventing candidates", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/pair-preview"))
        return response({ challengeId: "empty", candidates: [] });
      if (String(url).endsWith("/skip")) return response(skipReceipt(init));
      throw new Error(`unexpected request ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="display-only-empty" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    await waitFor(() => expect(callsTo(fetchMock, "pair-preview")).toHaveLength(1));
    expect(screen.queryAllByTestId(/^question-skip-candidate-/)).toHaveLength(0);
    fireEvent.click(await screen.findByTestId("question-skip-display-only"));
    await screen.findByTestId("question-skip-effect-unresolved");
    expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(0);
    expect(JSON.parse(String(callsTo(fetchMock, "skip")[0][1]?.body)).bindingId == null).toBe(true);
  });

  it("does not issue a bound skip when an earlier pair confirmation resolves after display-only was issued", async () => {
    let resolveConfirm!: (value: Response) => void;
    const fetchMock = vi.fn((url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/pair-preview"))
        return Promise.resolve(
          response({
            challengeId: "challenge",
            candidates: [{ token: "native", question: "Native Q01", options: ["A"] }],
          })
        );
      if (String(url).endsWith("/pair-confirm"))
        return new Promise<Response>((resolve) => {
          resolveConfirm = resolve;
        });
      if (String(url).endsWith("/skip")) return Promise.resolve(response(skipReceipt(init)));
      return Promise.reject(new Error(`unexpected request ${String(url)}`));
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="display-only-confirm-race" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    fireEvent.click(await screen.findByTestId("question-skip-candidate-native"));
    acknowledge();
    fireEvent.click(screen.getByTestId("question-skip-confirm"));
    await waitFor(() => expect(resolveConfirm).toBeTypeOf("function"));
    fireEvent.click(screen.getByTestId("question-skip-display-only"));
    await waitFor(() => expect(callsTo(fetchMock, "skip")).toHaveLength(1));
    await act(async () => {
      resolveConfirm(response({ bindingId: "late-binding", expiresAt: 1234 }));
    });
    expect(callsTo(fetchMock, "skip")).toHaveLength(1);
    expect(JSON.parse(String(callsTo(fetchMock, "skip")[0][1]?.body)).bindingId == null).toBe(true);
  });

  it("retries an unknown unbound request with the same body after disable and remount", async () => {
    const fetchMock = vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => {
      throw new Error("unknown delivery");
    });
    vi.stubGlobal("fetch", fetchMock);
    const ui = (disabled = false) => (
      <QuestionSkipControls sessionId="display-only-unknown" view={active} disabled={disabled} />
    );
    const rendered = render(ui());
    fireEvent.click(screen.getByTestId("question-skip-display-only"));
    await screen.findByTestId("question-skip-retry");
    const original = callsTo(fetchMock, "skip")[0][1]?.body;
    const originalBody = JSON.parse(String(original));
    expect(originalBody.bindingId == null).toBe(true);
    expect(originalBody.actionId).toEqual(expect.any(String));
    rendered.rerender(ui(true));
    rendered.unmount();
    render(ui());
    fireEvent.click(await screen.findByTestId("question-skip-retry"));
    await waitFor(() => expect(callsTo(fetchMock, "skip")).toHaveLength(2));
    expect(callsTo(fetchMock, "skip")[1][1]?.body).toBe(original);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("closure-red: a late preview cannot revive pairing after a successful display-only skip", async () => {
    let resolvePreview!: (value: Response) => void;
    const fetchMock = vi.fn((url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/pair-preview"))
        return new Promise<Response>((resolve) => {
          resolvePreview = resolve;
        });
      if (String(url).endsWith("/pair-confirm"))
        return Promise.resolve(response({ bindingId: "late-binding", expiresAt: 1234 }));
      if (String(url).endsWith("/skip")) return Promise.resolve(response(skipReceipt(init)));
      return Promise.reject(new Error(`unexpected request ${String(url)}`));
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="display-only-late-preview" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    await waitFor(() => expect(resolvePreview).toBeTypeOf("function"));
    fireEvent.click(screen.getByTestId("question-skip-display-only"));
    await screen.findByTestId("question-skip-effect-unresolved");
    expect(callsTo(fetchMock, "skip")).toHaveLength(1);
    expect(JSON.parse(String(callsTo(fetchMock, "skip")[0][1]?.body)).bindingId == null).toBe(true);
    await act(async () => {
      resolvePreview(
        response({
          challengeId: "late",
          candidates: [{ token: "late", question: "Native Q01", options: ["A"] }],
        })
      );
    });
    const candidate = screen.queryByTestId("question-skip-candidate-late");
    if (candidate) {
      fireEvent.click(candidate);
      acknowledge();
      fireEvent.click(screen.getByTestId("question-skip-confirm"));
      await waitFor(() => expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(1));
    }
    expect(candidate).toBeNull();
    expect(screen.getByTestId("question-skip-effect-unresolved")).toBeTruthy();
    expect(screen.getByTestId("question-skip-restore")).toBeTruthy();
    expect(callsTo(fetchMock, "pair-preview")).toHaveLength(1);
    expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(0);
    expect(callsTo(fetchMock, "skip")).toHaveLength(1);
  });

  it("closure-red: a captured display-only intent cannot skip after restore until canonical facts catch up", async () => {
    const sessionId = "display-only-restored-stale-intent";
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/restore"))
        return response({
          actionId: JSON.parse(String(init?.body)).actionId,
          group,
          displayDecision: "active",
          revision: 2,
        });
      if (String(url).endsWith("/skip")) return response({ ...skipReceipt(init), revision: 3 });
      throw new Error(`unexpected request ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    const capturedIntent = () =>
      issueQuestionSkipDisplayOnly(questionSkipScopeKey(sessionId, group), { sessionId, group });
    render(<QuestionSkipControls sessionId={sessionId} view={skipped} />);
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-restored");
    await act(async () => {
      await capturedIntent();
    });
    expect(callsTo(fetchMock, "restore")).toHaveLength(1);
    expect(callsTo(fetchMock, "skip")).toHaveLength(0);
    expect(screen.getByTestId("question-skip-restored")).toBeTruthy();
  });

  it("closure-red: a stale native pairing intent cannot reopen pairing after restore until canonical catches up", async () => {
    const sessionId = "display-only-native-done-intent";
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/restore"))
        return response({
          actionId: JSON.parse(String(init?.body)).actionId,
          group,
          displayDecision: "active",
          revision: 2,
        });
      if (String(url).endsWith("/pair-preview"))
        return response({
          challengeId: "native",
          candidates: [{ token: "native", question: "Native Q01", options: ["A"] }],
        });
      throw new Error(`unexpected request ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    // 旧 native 入口回调（capture 于 active 世界）：restore Done 且 canonical 未赶上
    // 时重放同一意图，不得发新 preview/清 accepted。
    const staleNativeIntent = () =>
      openQuestionSkipPairing(questionSkipScopeKey(sessionId, group), {
        sessionId,
        group,
        effectiveDecision: "active",
      });
    const rendered = render(<QuestionSkipControls sessionId={sessionId} view={skipped} />);
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-restored");
    await act(async () => {
      await staleNativeIntent();
    });
    expect(callsTo(fetchMock, "pair-preview")).toHaveLength(0);
    expect(screen.getByTestId("question-skip-restored")).toBeTruthy();
    // canonical 同 rev Active2 追上 → Done 释放回 Ready，主配对路径恢复可达。
    rendered.rerender(
      <QuestionSkipControls sessionId={sessionId} view={{ ...active, revision: 2 }} />
    );
    fireEvent.click(await screen.findByTestId("question-skip-start"));
    await screen.findByTestId("question-skip-candidate-native");
    expect(callsTo(fetchMock, "pair-preview")).toHaveLength(1);
  });

  it("keeps the existing archive without a display-only skip for a multi-question source", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session("display-only-multi")} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({
          asyncQuestions: [
            question,
            { ...question, questionKey: "second", questionIndex: 1, title: "Second?" },
          ],
          questionDisplayGroups: [{ ...active, sourceQuestionCount: 2 }],
        })
      )
    );
    const dock = await screen.findByTestId("codex-dock-cards");
    expect(within(dock).getByTestId("codex-async-archive")).toBeTruthy();
    expect(within(dock).queryByTestId("question-skip-display-only")).toBeNull();
    expect(within(dock).queryByTestId("question-skip-start")).toBeNull();
  });
});
