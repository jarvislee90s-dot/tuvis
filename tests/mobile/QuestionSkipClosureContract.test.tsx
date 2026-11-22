import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import QuestionSkipControls from "@/mobile/QuestionSkipControls";
import SessionDetail from "@/mobile/SessionDetail";
import {
  buildQuestionSkipFactsSnapshot,
  createQuestionSkipFactsStore,
  getAcceptedQuestionSkipReceipts,
} from "@/mobile/questionSkipOperations";
import { parseQuestionSkipFactFrame } from "@/mobile/codexQuestionSkipApi";
import { peekDraftEnvelope } from "@/mobile/draftEnvelope";
import { MockEventSource } from "./eventSourceMock";
import { questionSkipDetailFixture, questionSkipViews, response } from "./questionSkipFixtures";

const { group, active, skipped } = questionSkipViews("closure-thread", "gen-1", 1);
const { question, session, projection, detailFetch } = questionSkipDetailFixture(group, {
  questionKey: "closure-q",
  title: "Original question",
  options: ["A"],
});
const preview = {
  challengeId: "c",
  candidates: [{ token: "a", question: "Original question", options: ["A"] }],
};
const ack = () => {
  for (const name of ["thread", "count", "draft"])
    fireEvent.click(screen.getByTestId(`question-skip-ack-${name}`));
};
async function issueSkip() {
  fireEvent.click(screen.getByTestId("question-skip-start"));
  fireEvent.click(await screen.findByTestId("question-skip-candidate-a"));
  ack();
  fireEvent.click(screen.getByTestId("question-skip-confirm"));
}
const routes = (fetchMock: ReturnType<typeof vi.fn>, suffix: string) =>
  fetchMock.mock.calls.filter(([url]) => String(url).endsWith(`/${suffix}`));
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

describe("question skip closure regressions", () => {
  it("allows a new pair after restore receipt and matching canonical active projection", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/restore")) {
        const body = JSON.parse(String(init?.body));
        return response({ actionId: body.actionId, group, displayDecision: "active", revision: 2 });
      }
      if (String(url).endsWith("/pair-preview")) return response(preview);
      throw new Error(`unexpected ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    const rendered = render(
      <QuestionSkipControls sessionId="closure-restore-active" view={skipped} />
    );
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-restored");
    rendered.rerender(
      <QuestionSkipControls sessionId="closure-restore-active" view={{ ...active, revision: 2 }} />
    );
    fireEvent.click(await screen.findByTestId("question-skip-start"));
    await screen.findByTestId("question-skip-candidate-a");
    expect(routes(fetchMock, "pair-preview")).toHaveLength(1);
  });

  it("uses the newer native effect rather than an accepted older skip receipt", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      if (String(url).endsWith("/pair-preview")) return response(preview);
      if (String(url).endsWith("/pair-confirm"))
        return response({ bindingId: "binding", expiresAt: 1234 });
      const body = JSON.parse(String(init?.body));
      return response({
        actionId: body.actionId,
        group,
        displayDecision: "skipped",
        nativeEffect: "closed_verified",
        revision: 1,
      });
    });
    vi.stubGlobal("fetch", fetchMock);
    const rendered = render(<QuestionSkipControls sessionId="closure-effect" view={active} />);
    await issueSkip();
    await screen.findByTestId("question-skip-effect-closed_verified");
    rendered.rerender(
      <QuestionSkipControls
        sessionId="closure-effect"
        view={{ ...skipped, revision: 3, latestNativeEffect: "unresolved" }}
      />
    );
    expect(screen.getByTestId("question-skip-effect-unresolved")).toBeTruthy();
    expect(screen.queryByTestId("question-skip-effect-closed_verified")).toBeNull();
  });

  it("retains the unknown restore retry when a newer active history is read-only", async () => {
    const fetchMock = vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => {
      throw new Error("unknown delivery");
    });
    vi.stubGlobal("fetch", fetchMock);
    const rendered = render(
      <QuestionSkipControls sessionId="closure-unknown-history" view={skipped} />
    );
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-retry");
    const body = fetchMock.mock.calls[0][1]?.body;
    rendered.rerender(
      <QuestionSkipControls
        sessionId="closure-unknown-history"
        view={{ ...active, revision: 2 }}
        readOnly
        questionTitle="Original question"
      />
    );
    fireEvent.click(screen.getByTestId("question-skip-retry"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[1][1]?.body).toBe(body);
  });

  it("renders the merged active fact when old display and newer active history disagree", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session("closure-cross-ledger")} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({
          questionDisplayGroups: [skipped],
          questionSkipHistory: [{ ...active, revision: 2, question: null }],
        })
      )
    );
    expect(await screen.findByTestId("codex-async-question")).toBeTruthy();
    expect(screen.getByTestId("question-skip-start")).toBeTruthy();
    expect(screen.queryByTestId("question-skip-restore")).toBeNull();
  });

  it("updates the parent dock from a skip receipt even when the subsequent refresh fails", async () => {
    let threadCalls = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/pair-preview")) return response(preview);
      if (url.endsWith("/pair-confirm")) return response({ bindingId: "binding", expiresAt: 1234 });
      if (url.endsWith("/skip")) {
        const body = JSON.parse(String(init?.body));
        return response({
          actionId: body.actionId,
          group,
          displayDecision: "skipped",
          revision: 1,
          nativeEffect: "unresolved",
        });
      }
      if (url.endsWith("/restore")) {
        const body = JSON.parse(String(init?.body));
        return response({ actionId: body.actionId, group, displayDecision: "active", revision: 2 });
      }
      if (url.includes("/codex/thread")) {
        threadCalls += 1;
        return response({ error: "offline" }, 503);
      }
      return detailFetch(input);
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<SessionDetail session={session("closure-receipt-parent")} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit("snapshot", projection({ questionDisplayGroups: [active], questionSkipHistory: [] }))
    );
    await screen.findByTestId("codex-async-question");
    await issueSkip();
    await waitFor(() => expect(routes(fetchMock, "skip")).toHaveLength(1));
    await waitFor(() => expect(threadCalls).toBeGreaterThan(0));
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
    expect(await screen.findByTestId("question-skip-effect-unresolved")).toBeTruthy();
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await waitFor(() => expect(routes(fetchMock, "restore")).toHaveLength(1));
    const restoreBody = JSON.parse(String(routes(fetchMock, "restore")[0][1]?.body));
    expect(restoreBody).toMatchObject({
      sessionId: "closure-receipt-parent",
      group,
      expectedRevision: 1,
    });
    await screen.findByTestId("question-skip-restored");
  });

  it("orders a newly accepted receipt after earlier same-revision frames regardless of frame count", async () => {
    const id = "closure-receipt-order";
    const store = createQuestionSkipFactsStore();
    const versionOne = { ...active, revision: 1 };
    for (let i = 0; i < 20; i += 1)
      store.advance(parseQuestionSkipFactFrame({ questionDisplayGroups: [versionOne] }), [
        question,
      ]);
    const changed = vi.fn();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
        if (String(url).endsWith("/pair-preview")) return response(preview);
        if (String(url).endsWith("/pair-confirm"))
          return response({ bindingId: "binding", expiresAt: 1234 });
        const body = JSON.parse(String(init?.body));
        return response({
          actionId: body.actionId,
          group,
          displayDecision: "skipped",
          nativeEffect: "unresolved",
          revision: 1,
        });
      })
    );
    render(<QuestionSkipControls sessionId={id} view={versionOne} onChanged={changed} />);
    await issueSkip();
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
    expect(
      buildQuestionSkipFactsSnapshot(store.snapshot(), getAcceptedQuestionSkipReceipts(id))
        .unifiedByGid[0].fact
    ).toMatchObject({ decision: "skipped", revision: 1 });
  });

  it("retains revision high water through a legal empty membership and later stale reappearance", () => {
    const store = createQuestionSkipFactsStore();
    store.advance(
      parseQuestionSkipFactFrame({ questionDisplayGroups: [{ ...active, revision: 5 }] }),
      [question]
    );
    store.advance(parseQuestionSkipFactFrame({ questionDisplayGroups: [] }), [question]);
    expect(store.snapshot().displayByGid).toHaveLength(0);
    store.advance(
      parseQuestionSkipFactFrame({ questionDisplayGroups: [{ ...skipped, revision: 2 }] }),
      [question]
    );
    expect(buildQuestionSkipFactsSnapshot(store.snapshot(), []).unifiedByGid[0].fact).toMatchObject(
      { revision: 5, decision: "active" }
    );
  });

  it("treats two current generations for one source as invalid while retaining the last legal frame", () => {
    const store = createQuestionSkipFactsStore();
    const second = { ...active, group: { ...group, contentGeneration: "gen-2" } };
    store.advance(parseQuestionSkipFactFrame({ questionDisplayGroups: [active] }), [question]);
    store.advance(parseQuestionSkipFactFrame({ questionDisplayGroups: [active, second] }), [
      question,
    ]);
    const snapshot = store.snapshot();
    expect(snapshot.displayInvalid).toBe(true);
    expect(snapshot.displayByGid.map((entry) => entry.view.group)).toEqual([group]);
  });

  it("does not authorize old generation skip or verify a new body while its descriptor is absent", async () => {
    const id = "closure-body-change";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session(id)} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit("snapshot", projection({ questionDisplayGroups: [active], questionSkipHistory: [] }))
    );
    expect(await screen.findByTestId("question-skip-start")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    const changed = { ...question, title: "Different generation body", contextToken: "token-new" };
    act(() => es.emit("state", projection({ asyncQuestions: [changed], questionSkipHistory: [] })));
    expect(await screen.findByText("Different generation body")).toBeTruthy();
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    const envelope = peekDraftEnvelope({
      sessionId: id,
      sourceTurnId: "turn",
      sourceItemId: "item",
    });
    const changedRecord = envelope?.records.find(
      (record) => record.question?.title === changed.title && record.value.selected === "A"
    );
    expect(changedRecord?.origin.kind).toBe("unverified");
  });

  it("does not attach an old verified group to a changed body when the descriptor disappears", async () => {
    const id = "closure-body-draft";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session(id)} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit("snapshot", projection({ questionDisplayGroups: [active], questionSkipHistory: [] }))
    );
    await screen.findByTestId("codex-async-question");
    const changed = { ...question, title: "Unverified new body", contextToken: "token-new" };
    act(() => es.emit("state", projection({ asyncQuestions: [changed], questionSkipHistory: [] })));
    await screen.findByText(changed.title);
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    const envelope = peekDraftEnvelope({
      sessionId: id,
      sourceTurnId: "turn",
      sourceItemId: "item",
    });
    const record = envelope?.records.find(
      (item) => item.question?.title === changed.title && item.value.selected === "A"
    );
    expect(record?.origin.kind).toBe("unverified");
  });

  it("blocks skip when a descriptor arrived without an observed body and the body arrives later", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session("closure-unobserved-skip")} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({ asyncQuestions: [], questionDisplayGroups: [active], questionSkipHistory: [] })
      )
    );
    expect(screen.queryByTestId("codex-async-question")).toBeNull();
    const newQuestion = { ...question, title: "Late unobserved body", contextToken: "late-token" };
    act(() =>
      es.emit("state", projection({ asyncQuestions: [newQuestion], questionSkipHistory: [] }))
    );
    expect(await screen.findByText(newQuestion.title)).toBeTruthy();
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
  });

  it("keeps a later body unverified when its earlier descriptor had no observation", async () => {
    const id = "closure-unobserved-draft";
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session(id)} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({ asyncQuestions: [], questionDisplayGroups: [active], questionSkipHistory: [] })
      )
    );
    expect(screen.queryByTestId("codex-async-question")).toBeNull();
    const newQuestion = { ...question, title: "Late unverified body", contextToken: "late-token" };
    act(() =>
      es.emit("state", projection({ asyncQuestions: [newQuestion], questionSkipHistory: [] }))
    );
    expect(await screen.findByText(newQuestion.title)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    const envelope = peekDraftEnvelope({
      sessionId: id,
      sourceTurnId: "turn",
      sourceItemId: "item",
    });
    const record = envelope?.records.find(
      (item) => item.question?.title === newQuestion.title && item.value.selected === "A"
    );
    expect(record?.origin.kind).toBe("unverified");
  });

  it("distinguishes same-source null-snapshot historical generations in visible and accessible labels", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => detailFetch(input))
    );
    render(<SessionDetail session={session("closure-history-generations")} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    const another = { ...skipped, group: { ...group, contentGeneration: "gen-2" } };
    act(() =>
      es.emit(
        "snapshot",
        projection({
          questionDisplayGroups: [],
          questionSkipHistory: [
            { ...skipped, question: null },
            { ...another, question: null },
          ],
        })
      )
    );
    const controls = await screen.findAllByTestId("question-skip-controls");
    const records = controls.filter((element) =>
      within(element).queryByTestId("question-skip-restore")
    );
    expect(records).toHaveLength(2);
    expect(records[0].getAttribute("aria-label")).not.toBe(records[1].getAttribute("aria-label"));
    expect(records[0].textContent).not.toBe(records[1].textContent);
  });
});
