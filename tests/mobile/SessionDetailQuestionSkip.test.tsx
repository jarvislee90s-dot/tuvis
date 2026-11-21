import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { Profiler } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SessionDetail from "@/mobile/SessionDetail";
import type { PendingView } from "@/mobile/codexIpcApi";
import type { Session } from "@/types/session";
import { MockEventSource } from "./eventSourceMock";

const session: Session = {
  id: "skip-session",
  agentType: "codex",
  projectName: "proj",
  projectPath: "/tmp/proj",
  title: "codex",
  gitBranch: null,
  githubUrl: null,
  status: "idle",
  lastMessage: null,
  lastMessageRole: null,
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "2026-10-20T00:00:00Z",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "app",
  jumpSupported: false,
  unread: false,
};
const group = {
  threadId: "thread-1",
  sourceTurnId: "turn",
  sourceItemId: "item",
  contentGeneration: "gen-1",
};
const q = {
  questionKey: "q",
  contextToken: "token",
  sourceTurnId: "turn",
  sourceItemId: "item",
  questionIndex: 0,
  title: "Skip test question",
  options: ["A"],
  state: "pending",
  actionable: true,
  disabledReasonCode: null,
};
const display = {
  group,
  revision: 1,
  decision: "skipped",
  latestNativeEffect: "unresolved",
  reasonCode: null,
  sourceComplete: true,
  sourceQuestionCount: 1,
};
const projection = (extra: Record<string, unknown>) =>
  Object.assign(
    {
      threadId: "thread-1",
      revision: 5,
      stale: false,
      availability: "controllable",
      capabilities: {
        send: true,
        state: true,
        command: true,
        permissions: false,
        blockingInput: true,
      },
      runtimeStatus: "idle",
      contentState: "ready",
      messages: [],
      truncated: false,
      pending: [],
      asyncQuestions: [q],
    },
    extra
  );

beforeEach(() => {
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const body = url.includes("/codex/thread")
        ? projection({})
        : url.includes("/codex/queue")
          ? { actions: [], blockedReasonCode: null }
          : url.includes("/session-messages")
            ? { messages: [], truncated: false }
            : url.includes("/session-subagents")
              ? { subagents: [] }
              : url.includes("/session-files")
                ? { files: [], truncated: false }
                : url.includes("/session-queue")
                  ? { items: [] }
                  : url.includes("/session-send-info")
                    ? { injectable: true, channels: [], visibility: "realtime" }
                    : {};
      return new Response(JSON.stringify(body), { status: 200 });
    })
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

describe("skip projection in the real session detail", () => {
  it("omits skipped questions from the dock while preserving the history restore entry", async () => {
    render(
      <SessionDetail session={{ ...session, id: "detail-history-entry" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({
          questionDisplayGroups: [display],
          questionSkipHistory: [
            {
              ...display,
              question: { questionKey: "q", questionIndex: 0, title: q.title, options: q.options },
            },
          ],
        })
      )
    );
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
    expect(await screen.findByTestId("question-skip-restore")).toBeTruthy();
  });

  it("keeps the last legal active display and disables new skip on an invalid overlay", async () => {
    render(
      <SessionDetail session={{ ...session, id: "detail-invalid-overlay" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    const activeDisplay = { ...display, revision: 0, decision: "active", latestNativeEffect: null };
    act(() =>
      es.emit(
        "snapshot",
        projection({ questionDisplayGroups: [activeDisplay], questionSkipHistory: [] })
      )
    );
    expect(await screen.findByTestId("question-skip-start")).toBeTruthy();
    act(() =>
      es.emit(
        "state",
        projection({
          questionDisplayGroups: [{ ...activeDisplay, revision: -1 }],
          questionSkipHistory: [],
        })
      )
    );
    expect(screen.getByTestId("codex-async-question")).toBeTruthy();
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
  });

  it("does not let a previous generation's skipped history hide a new source generation", async () => {
    render(
      <SessionDetail session={{ ...session, id: "detail-old-generation" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    const newDisplay = {
      ...display,
      group: { ...group, contentGeneration: "gen-2" },
      revision: 0,
      decision: "active",
      latestNativeEffect: null,
    };
    act(() =>
      es.emit(
        "snapshot",
        projection({
          asyncQuestions: [{ ...q, title: "New generation" }],
          questionDisplayGroups: [newDisplay],
          questionSkipHistory: [
            {
              ...display,
              question: {
                questionKey: "q",
                questionIndex: 0,
                title: "Old generation",
                options: ["A"],
              },
            },
          ],
        })
      )
    );
    expect(await screen.findByText("New generation")).toBeTruthy();
    expect(screen.getByTestId("question-skip-restore")).toBeTruthy();
  });

  it("applies a newer same-revision local effect and rejects an older SSE fact", async () => {
    render(<SessionDetail session={{ ...session, id: "detail-sse-effect" }} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    const version = (localSeq: number) => ({
      instanceId: "boot",
      ownerEpoch: 1,
      ownerGeneration: 1,
      revision: 5,
      localSeq,
    });
    const history = (effect: string) => [
      {
        ...display,
        latestNativeEffect: effect,
        question: { questionKey: "q", questionIndex: 0, title: q.title, options: q.options },
      },
    ];
    act(() =>
      es.emit(
        "snapshot",
        projection({
          projectionVersion: version(1),
          questionDisplayGroups: [display],
          questionSkipHistory: history("unresolved"),
        })
      )
    );
    expect(await screen.findByTestId("question-skip-effect-unresolved")).toBeTruthy();
    act(() =>
      es.emit(
        "state",
        projection({
          projectionVersion: version(2),
          questionDisplayGroups: [{ ...display, latestNativeEffect: "closed_verified" }],
          questionSkipHistory: history("closed_verified"),
        })
      )
    );
    expect(await screen.findByTestId("question-skip-effect-closed_verified")).toBeTruthy();
    act(() =>
      es.emit(
        "state",
        projection({
          projectionVersion: version(1),
          questionDisplayGroups: [display],
          questionSkipHistory: history("unresolved"),
        })
      )
    );
    expect(screen.getByTestId("question-skip-effect-closed_verified")).toBeTruthy();
    expect(screen.queryByTestId("question-skip-effect-unresolved")).toBeNull();
  });

  it("keeps higher display revisions over lower history and permits newer equal-revision observations", async () => {
    render(<SessionDetail session={{ ...session, id: "ledger-display-high" }} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    const current = { ...display, revision: 5, decision: "active", latestNativeEffect: null };
    const old = { ...display, revision: 2, question: null };
    act(() =>
      es.emit(
        "snapshot",
        projection({ questionDisplayGroups: [current], questionSkipHistory: [old] })
      )
    );
    expect(
      within(await screen.findByTestId("codex-async-question")).getByText(q.title)
    ).toBeTruthy();
    act(() =>
      es.emit(
        "state",
        projection({
          questionDisplayGroups: [{ ...display, revision: 5 }],
          questionSkipHistory: [{ ...old, revision: 5 }],
        })
      )
    );
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
    act(() =>
      es.emit(
        "state",
        projection({
          questionDisplayGroups: [current],
          questionSkipHistory: [{ ...old, revision: 4 }],
        })
      )
    );
    expect(
      within(await screen.findByTestId("codex-async-question")).getByText(q.title)
    ).toBeTruthy();
  });

  it("keeps higher history revisions over a lower display and ignores a lower-revision native update", async () => {
    render(<SessionDetail session={{ ...session, id: "ledger-history-high" }} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    const oldActive = { ...display, revision: 2, decision: "active", latestNativeEffect: null };
    const high = { ...display, revision: 5, latestNativeEffect: "closed_verified", question: null };
    act(() =>
      es.emit(
        "snapshot",
        projection({ questionDisplayGroups: [oldActive], questionSkipHistory: [high] })
      )
    );
    expect(await screen.findByTestId("question-skip-effect-closed_verified")).toBeTruthy();
    expect(screen.queryByTestId("codex-async-question")).toBeNull();
    act(() =>
      es.emit(
        "state",
        projection({
          questionDisplayGroups: [{ ...display, revision: 3 }],
          questionSkipHistory: [{ ...high, revision: 3, latestNativeEffect: "unresolved" }],
        })
      )
    );
    expect(screen.getByTestId("question-skip-effect-closed_verified")).toBeTruthy();
    expect(screen.queryByTestId("question-skip-effect-unresolved")).toBeNull();
  });

  it("keeps display invalid sticky across valid history and absent display, until a legal empty display", async () => {
    render(
      <SessionDetail session={{ ...session, id: "ledger-invalid-sticky" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    const current = { ...display, revision: 0, decision: "active", latestNativeEffect: null };
    act(() =>
      es.emit("snapshot", projection({ questionDisplayGroups: [current], questionSkipHistory: [] }))
    );
    expect(await screen.findByTestId("question-skip-start")).toBeTruthy();
    act(() =>
      es.emit(
        "state",
        projection({
          questionDisplayGroups: [{ ...current, revision: -1 }],
          questionSkipHistory: [],
        })
      )
    );
    act(() => es.emit("state", projection({ questionSkipHistory: [] })));
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
    act(() => es.emit("state", projection({ questionDisplayGroups: [], questionSkipHistory: [] })));
    expect(screen.getByTestId("codex-async-question")).toBeTruthy();
  });

  it("does not revive an old skip when the current source changes or becomes ambiguous", async () => {
    render(
      <SessionDetail session={{ ...session, id: "source-identity-chain" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({
          questionDisplayGroups: [display],
          questionSkipHistory: [{ ...display, question: null }],
        })
      )
    );
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
    const changed = { ...q, title: "New source body", contextToken: "new-token" };
    act(() =>
      es.emit(
        "state",
        projection({
          asyncQuestions: [changed],
          questionSkipHistory: [{ ...display, question: null }],
        })
      )
    );
    expect(await screen.findByText("New source body")).toBeTruthy();
    const next = {
      ...display,
      group: { ...group, contentGeneration: "gen-2" },
      revision: 0,
      decision: "active",
      latestNativeEffect: null,
    };
    act(() =>
      es.emit(
        "state",
        projection({
          asyncQuestions: [changed],
          questionDisplayGroups: [display, next],
          questionSkipHistory: [],
        })
      )
    );
    expect(screen.getByText("New source body")).toBeTruthy();
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
  });

  it("labels each skipped historical group without borrowing a newer generation title", async () => {
    render(
      <SessionDetail session={{ ...session, id: "history-distinct-labels" }} onBack={() => {}} />
    );
    const es = await waitFor(() => MockEventSource.latest());
    const newer = {
      ...display,
      group: { ...group, contentGeneration: "gen-2" },
      revision: 0,
      decision: "active",
      latestNativeEffect: null,
    };
    const other = {
      ...display,
      group: { ...group, sourceItemId: "other-item", contentGeneration: "gen-other" },
    };
    act(() =>
      es.emit(
        "snapshot",
        projection({
          asyncQuestions: [{ ...q, title: "Current generation only" }],
          questionDisplayGroups: [newer],
          questionSkipHistory: [
            { ...display, question: null },
            {
              ...other,
              question: {
                questionKey: "other",
                questionIndex: 0,
                title: "Other historical question",
                options: ["A"],
              },
            },
          ],
        })
      )
    );
    await screen.findByText("Current generation only");
    const controls = screen
      .getAllByTestId("question-skip-controls")
      .filter((node) => within(node).queryByTestId("question-skip-restore"));
    expect(controls).toHaveLength(2);
    expect(controls[0].textContent).not.toContain("Current generation only");
    expect(controls[1].textContent).toContain("Other historical question");
    for (const control of controls) {
      expect(control.getAttribute("role")).toBe("group");
      expect(control.getAttribute("aria-label")).toBeTruthy();
    }
    expect(controls[0].getAttribute("aria-label")).not.toBe(controls[1].getAttribute("aria-label"));
  });

  it("keeps the existing archive for a complete multi-question group", async () => {
    render(<SessionDetail session={{ ...session, id: "detail-archive" }} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    act(() =>
      es.emit(
        "snapshot",
        projection({
          asyncQuestions: [q, { ...q, questionKey: "q2", questionIndex: 1, title: "Second" }],
          questionDisplayGroups: [
            {
              ...display,
              revision: 0,
              decision: "active",
              latestNativeEffect: null,
              sourceQuestionCount: 2,
            },
          ],
        })
      )
    );
    const dock = await screen.findByTestId("codex-dock-cards");
    expect(within(dock).getByTestId("codex-async-archive")).toBeTruthy();
    expect(within(dock).queryByTestId("question-skip-start")).toBeNull();
  });
});

const localSkipLabel = /^跳过[（(]仅\s*Tuvis[）)]$/;
const localRestoreLabel = /^仍待答\s*·\s*展开$/;
const localDraft = "pending-local-draft";
const firstOption = "pending-local-option";

function immediateQuestion(count: number, overrides: Partial<PendingView> = {}): PendingView {
  return {
    requestKey: "n:12",
    contextToken: "tok-first",
    kind: "user_input",
    actionable: true,
    disabledReasonCode: null,
    command: null,
    cwd: null,
    reason: null,
    permissionSummary: null,
    questions: Array.from({ length: count }, (_, index) => ({
      id: `b6-q${index + 1}`,
      header: "",
      question: `即时问答 ${index + 1}`,
      isOther: true,
      isSecret: false,
      options: [{ label: firstOption, description: "" }],
    })),
    ...overrides,
  };
}

function immediateProjection(pending: PendingView[], localSeq = 1, stale = false, revision = 5) {
  return projection({
    pending,
    asyncQuestions: [],
    stale,
    revision,
    projectionVersion: {
      instanceId: "b6-boot",
      ownerEpoch: 1,
      ownerGeneration: 1,
      revision,
      localSeq,
    },
  });
}

function storageEntries() {
  const storage = window.sessionStorage;
  return Object.fromEntries(
    Array.from({ length: storage.length }, (_, index) => {
      const key = storage.key(index)!;
      return [key, storage.getItem(key)];
    })
  );
}

function stubImmediateFetch(pending: PendingView[]) {
  const fallback = vi.mocked(fetch).getMockImplementation()!;
  let releasePendingResponse!: (response: Response) => void;
  const pendingResponse = new Promise<Response>((resolve) => {
    releasePendingResponse = resolve;
  });
  let releaseVerification!: (response: Response) => void;
  const verification = new Promise<Response>((resolve) => {
    releaseVerification = resolve;
  });
  const fetchMock = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/codex/thread")) {
      return Promise.resolve(Response.json(immediateProjection(pending)));
    }
    if (url.includes("/codex/pending-response") && init?.method === "POST") {
      return pendingResponse;
    }
    if (url.includes("/codex/action/verify")) return verification;
    return fallback(input, init);
  });
  vi.stubGlobal("fetch", fetchMock);
  return { fetchMock, releasePendingResponse, releaseVerification };
}

function deliveredVerification(actionId: string) {
  return Response.json({
    actionId,
    stage: "settled",
    outcome: "delivered",
    turnId: "verified-turn",
    runLimitReleased: false,
    reasonCode: null,
    verifiedAt: 1,
  });
}

async function mountImmediate(pending: PendingView[], id = session.id, onCommit = () => {}) {
  const view = render(
    <Profiler id="session-detail" onRender={onCommit}>
      <SessionDetail session={{ ...session, id }} onBack={() => {}} />
    </Profiler>
  );
  const stream = await waitFor(() => MockEventSource.latest());
  act(() => stream.emit("snapshot", immediateProjection(pending)));
  return { ...view, stream };
}

async function findImmediateCard(questionText = "即时问答 1") {
  return (await screen.findByText(questionText)).closest<HTMLElement>(
    '[data-testid="codex-pending-card"]'
  )!;
}

function skipImmediate(card: HTMLElement) {
  fireEvent.click(within(card).getByRole("button", { name: localSkipLabel }));
}

function restoreImmediate() {
  const expand = screen.getByRole("button", { name: localRestoreLabel });
  expect(expand).toBeVisible();
  fireEvent.click(expand);
}

function draftOnSecondPage(card: HTMLElement) {
  expect(card).toBeVisible();
  fireEvent.click(within(card).getByTestId("codex-q-b6-q1-opt-0"));
  fireEvent.click(within(card).getByTestId("codex-pending-next"));
  expect(within(card).getByTestId("codex-pending-progress")).toHaveTextContent("2／2");
  const draft = within(card).getByTestId("codex-q-b6-q2-free");
  fireEvent.change(draft, { target: { value: localDraft } });
  return draft;
}

describe("B6 即时问答的本地收起", () => {
  beforeEach(async () => {
    await i18n.changeLanguage("zh");
  });

  it.each([1, 2])("b6_pending_skip_collapses_the_whole_request (%i questions)", async (count) => {
    const request = immediateQuestion(count);
    const approval = immediateQuestion(0, {
      requestKey: "n:13",
      kind: "command_approval",
      command: "git status --short",
    });
    const other = immediateQuestion(1, {
      requestKey: "n:14",
      questions: [{ ...request.questions[0], id: "b6-q-other", question: "其他即时问答" }],
    });
    const pending = [request, approval, other];
    stubImmediateFetch(pending);
    await mountImmediate(pending);
    const card = await findImmediateCard();
    const otherCard = await findImmediateCard("其他即时问答");
    const approvalCard = screen.getByTestId("codex-pending-command");
    expect(card).toBeVisible();

    skipImmediate(card);

    expect(card).not.toBeVisible();
    const restore = screen.getByRole("button", { name: localRestoreLabel });
    expect(restore).toBeVisible();
    expect(restore).toHaveFocus();
    expect(approvalCard).toBeVisible();
    expect(otherCard).toBeVisible();
    // 展开按钮和控制该卡可见性的 hidden 容器是同一 wrapper 的直接子节点。
    const wrapper = restore.parentElement!;
    expect(wrapper).toBe(card.parentElement?.parentElement);
    expect(wrapper).toHaveAttribute("tabindex", "-1");

    fireEvent.click(restore);
    expect(card).toBeVisible();
    expect(wrapper).toHaveFocus();
  });

  it("lets a read-only immediate question collapse and expand without writing", async () => {
    const pending = [immediateQuestion(1, { actionable: false })];
    const { fetchMock } = stubImmediateFetch(pending);
    await mountImmediate(pending);
    const reason = await screen.findByTestId("codex-pending-disabled");
    const card = reason.closest<HTMLElement>('[data-testid="codex-pending-card"]')!;
    expect(reason).toBeVisible();
    expect(within(card).queryByTestId("codex-pending-submit")).toBeNull();

    skipImmediate(card);
    expect(card).not.toBeVisible();
    expect(screen.getByRole("button", { name: localRestoreLabel })).toHaveFocus();
    restoreImmediate();
    expect(reason).toBeVisible();
    expect(within(card).queryByTestId("codex-pending-submit")).toBeNull();
    expect(fetchMock.mock.calls.filter(([, init]) => (init?.method ?? "GET") !== "GET")).toEqual(
      []
    );
  });

  it("b6_pending_restore_keeps_the_mounted_page_and_draft", async () => {
    const pending = [immediateQuestion(2)];
    stubImmediateFetch(pending);
    await mountImmediate(pending);
    const card = await findImmediateCard();
    const draft = draftOnSecondPage(card);

    skipImmediate(card);
    expect(card).toBeInTheDocument();
    expect(card).not.toBeVisible();
    restoreImmediate();

    expect(card).toBeVisible();
    expect(within(card).getByTestId("codex-pending-progress")).toHaveTextContent("2／2");
    expect(within(card).getByTestId("codex-q-b6-q2-free")).toBe(draft);
    expect(draft).toHaveValue(localDraft);

    fireEvent.click(within(card).getByTestId("codex-pending-prev"));
    expect(within(card).getByTestId("codex-pending-progress")).toHaveTextContent("1／2");
    expect(within(card).getByTestId("codex-q-b6-q1-opt-0")).toBeChecked();
    fireEvent.click(within(card).getByTestId("codex-pending-next"));
    expect(within(card).getByTestId("codex-q-b6-q2-free")).toHaveValue(localDraft);
  });

  it("keeps the original action anchor through collapse, expansion, and reload until verified", async () => {
    const pending = [immediateQuestion(2)];
    const { fetchMock, releasePendingResponse, releaseVerification } = stubImmediateFetch(pending);
    const first = await mountImmediate(pending);
    const card = await findImmediateCard();
    const draft = draftOnSecondPage(card);
    const submit = within(card).getByTestId("codex-pending-submit");
    fireEvent.click(submit);
    const writes = fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
    expect(writes).toHaveLength(1);
    const { actionId } = JSON.parse(String(writes[0][1]?.body));
    const anchor = `codex:pending:${session.id}:n:12:tok-first`;
    expect(sessionStorage.getItem(anchor)).toBe(actionId);
    expect(submit).toBeDisabled();
    const beforeSkip = storageEntries();

    skipImmediate(card);
    expect(card).toBeInTheDocument();
    expect(card).not.toBeVisible();
    expect(draft).toBeInTheDocument();
    expect(submit).toBeDisabled();
    expect(storageEntries()).toMatchObject(beforeSkip);
    expect(Object.keys(storageEntries())).toHaveLength(Object.keys(beforeSkip).length + 1);

    restoreImmediate();
    expect(card).toBeVisible();
    expect(within(card).getByTestId("codex-q-b6-q2-free")).toBe(draft);
    expect(within(card).getByTestId("codex-pending-progress")).toHaveTextContent("2／2");
    expect(draft).toHaveValue(localDraft);
    expect(submit).toBeDisabled();
    expect(storageEntries()).toEqual(beforeSkip);

    await act(async () => {
      releasePendingResponse(
        Response.json({
          actionId,
          delivery: "unknown_delivery",
          route: "desktop_ipc",
          turnId: null,
        })
      );
    });
    expect(within(card).getByTestId("codex-pending-verify")).toBeVisible();
    expect(sessionStorage.getItem(anchor)).toBe(actionId);

    first.unmount();
    await mountImmediate(pending);
    const reloaded = await findImmediateCard();
    expect(sessionStorage.getItem(anchor)).toBe(actionId);
    fireEvent.click(within(reloaded).getByTestId("codex-pending-verify"));
    const verifyCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes("/codex/action/verify")
    );
    expect(
      new URL(String(verifyCall?.[0]), window.location.href).searchParams.get("action_id")
    ).toBe(actionId);
    await act(async () => {
      releaseVerification(deliveredVerification(actionId));
    });
    expect(sessionStorage.getItem(anchor)).toBeNull();
    expect(within(reloaded).queryByTestId("codex-pending-verify")).toBeNull();
  });

  it("b6_pending_reload_keeps_only_the_collapse_marker", async () => {
    const pending = [immediateQuestion(2)];
    stubImmediateFetch(pending);
    const first = await mountImmediate(pending);
    const card = await findImmediateCard();
    draftOnSecondPage(card);
    const beforeSkip = storageEntries();
    skipImmediate(card);
    const collapsedStorage = storageEntries();
    expect(collapsedStorage).toMatchObject(beforeSkip);
    const newKeys = Object.keys(collapsedStorage).filter((key) => !(key in beforeSkip));
    expect(newKeys).toHaveLength(1);
    const marker = JSON.stringify([newKeys[0], collapsedStorage[newKeys[0]]]);
    for (const transient of [localDraft, firstOption, "2／2", "b6-q2", "即时问答 2"]) {
      expect(marker).not.toContain(transient);
    }

    first.unmount();
    const reloaded = await mountImmediate(pending);
    expect(screen.getByRole("button", { name: localRestoreLabel })).toBeVisible();
    expect(await findImmediateCard()).not.toBeVisible();
    restoreImmediate();
    const restored = await findImmediateCard();
    expect(restored).toBeVisible();
    expect(within(restored).getByTestId("codex-pending-progress")).toHaveTextContent("1／2");
    expect(within(restored).getByTestId("codex-q-b6-q1-free")).toHaveValue("");
    expect(within(restored).getByTestId("codex-q-b6-q1-opt-0")).not.toBeChecked();
    expect(within(restored).getByTestId("codex-pending-next")).toBeDisabled();
    skipImmediate(restored);
    reloaded.unmount();

    // jsdom 只有当前窗口。用独立的 Storage 模拟另一个未继承存储的标签页。
    const originalTabStorage = window.sessionStorage;
    const independentTab = new Map<string, string>();
    const independentStorage: Storage = {
      get length() {
        return independentTab.size;
      },
      clear: () => independentTab.clear(),
      getItem: (key) => independentTab.get(key) ?? null,
      key: (index) => Array.from(independentTab.keys())[index] ?? null,
      removeItem: (key) => {
        independentTab.delete(key);
      },
      setItem: (key, value) => {
        independentTab.set(key, value);
      },
    };
    const storage = vi.spyOn(window, "sessionStorage", "get").mockReturnValue(independentStorage);
    try {
      const otherTab = await mountImmediate(pending);
      expect(await findImmediateCard()).toBeVisible();
      expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();
      expect(independentStorage.getItem(newKeys[0])).toBeNull();
      expect(originalTabStorage.getItem(newKeys[0])).toBe(collapsedStorage[newKeys[0]]);
      otherTab.unmount();
    } finally {
      storage.mockRestore();
    }
  });

  it("b6_pending_token_rotation_invalidates_the_old_marker", async () => {
    const original = immediateQuestion(1);
    const pending = [original];
    stubImmediateFetch(pending);
    const first = await mountImmediate(pending);
    const card = await findImmediateCard();
    const beforeSkip = storageEntries();
    skipImmediate(card);
    const marked = storageEntries();
    const newKeys = Object.keys(marked).filter((key) => !(key in beforeSkip));
    expect(newKeys).toHaveLength(1);
    const marker = newKeys[0];

    first.unmount();
    const otherSession = await mountImmediate(pending, "b6-independent-session");
    expect(await findImmediateCard()).toBeVisible();
    expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();
    expect(sessionStorage.getItem(marker)).toBe(marked[marker]);
    otherSession.unmount();

    const { stream } = await mountImmediate(pending);
    expect(screen.getByRole("button", { name: localRestoreLabel })).toBeVisible();
    act(() => stream.emit("state", immediateProjection([], 2, true)));
    expect(screen.getByTestId("codex-projection")).toHaveAttribute("data-stale", "true");
    expect(sessionStorage.getItem(marker)).toBe(marked[marker]);

    const next = immediateQuestion(1, {
      contextToken: "tok-next",
      questions: [{ ...original.questions[0], question: "换代后的即时问答" }],
    });
    // 恢复帧须比 stale 帧的 revision 更新。
    act(() => stream.emit("state", immediateProjection([next], 3, false, 6)));
    expect(await findImmediateCard("换代后的即时问答")).toBeVisible();
    expect(screen.getByTestId("codex-projection")).toHaveAttribute("data-stale", "false");
    expect(sessionStorage.getItem(marker)).toBeNull();
    expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();

    act(() => stream.emit("state", immediateProjection(pending, 1)));
    expect(screen.queryByText("即时问答 1")).toBeNull();
    expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();
    expect(await findImmediateCard("换代后的即时问答")).toBeVisible();
  });

  it("clears a collapsed marker on fresh withdrawal and rejects a late lower version", async () => {
    const pending = [immediateQuestion(1)];
    stubImmediateFetch(pending);
    const { stream } = await mountImmediate(pending);
    const card = await findImmediateCard();
    const beforeSkip = storageEntries();
    skipImmediate(card);
    expect(storageEntries()).not.toEqual(beforeSkip);

    act(() => stream.emit("state", immediateProjection([], 2, false, 6)));
    expect(card).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();
    expect(storageEntries()).toEqual(beforeSkip);

    act(() => stream.emit("state", immediateProjection(pending, 1)));
    expect(screen.queryByText("即时问答 1")).toBeNull();
    expect(screen.queryByRole("button", { name: localRestoreLabel })).toBeNull();
    expect(storageEntries()).toEqual(beforeSkip);
  });

  it("never commits a collapsed secret draft into a new ordinary input with the same qid", async () => {
    const original = immediateQuestion(1);
    original.questions[0].isSecret = true;
    const pending = [original];
    stubImmediateFetch(pending);
    const ordinaryTitle = "复用题号的新普通题";
    const ordinaryCommits: Array<{ type: string; value: string }> = [];
    const { stream } = await mountImmediate(pending, session.id, () => {
      // Profiler 在提交阶段运行，采集 passive effect 清稿前的 DOM 值。
      const title = screen.queryByText(ordinaryTitle);
      const input = title?.parentElement?.querySelector<HTMLInputElement>(
        "input[data-testid='codex-q-b6-q1-free']"
      );
      if (input) ordinaryCommits.push({ type: input.type, value: input.value });
    });
    const card = await findImmediateCard();
    const draft = within(card).getByTestId("codex-q-b6-q1-free");
    expect(draft).toHaveAttribute("type", "password");
    fireEvent.change(draft, { target: { value: "old-secret-b6" } });
    skipImmediate(card);
    expect(card).not.toBeVisible();

    const next = immediateQuestion(1, {
      contextToken: "tok-next",
      questions: [{ ...original.questions[0], isSecret: false, question: ordinaryTitle }],
    });
    act(() => stream.emit("state", immediateProjection([next], 2, false, 6)));
    const newCard = await findImmediateCard(ordinaryTitle);
    expect(newCard).toBeVisible();
    expect(ordinaryCommits[0]).toEqual({ type: "text", value: "" });
    expect(ordinaryCommits.every(({ value }) => value === "")).toBe(true);
    expect(within(newCard).queryByTestId("codex-pending-restore-draft")).toBeNull();
  });

  it("offers a collapsed ordinary draft on token rotation only for explicit restoration", async () => {
    const original = immediateQuestion(1);
    const pending = [original];
    stubImmediateFetch(pending);
    const { stream } = await mountImmediate(pending);
    const card = await findImmediateCard();
    fireEvent.change(within(card).getByTestId("codex-q-b6-q1-free"), {
      target: { value: localDraft },
    });
    skipImmediate(card);

    const next = immediateQuestion(1, {
      contextToken: "tok-next",
      questions: [{ ...original.questions[0], question: "可以恢复普通旧稿的新题" }],
    });
    act(() => stream.emit("state", immediateProjection([next], 2, false, 6)));
    const newCard = await findImmediateCard("可以恢复普通旧稿的新题");
    const draft = within(newCard).getByTestId("codex-q-b6-q1-free");
    expect(draft).toHaveValue("");
    expect(within(newCard).getByTestId("codex-pending-submit")).toBeDisabled();
    fireEvent.click(within(newCard).getByTestId("codex-pending-restore-draft"));
    expect(draft).toHaveValue(localDraft);
  });

  it("keeps a new token's draft, submit state, and receipt unchanged when old verification resolves", async () => {
    const original = immediateQuestion(1);
    const pending = [original];
    const { fetchMock, releasePendingResponse, releaseVerification } = stubImmediateFetch(pending);
    const { stream } = await mountImmediate(pending);
    const card = await findImmediateCard();
    fireEvent.click(within(card).getByTestId("codex-q-b6-q1-opt-0"));
    fireEvent.click(within(card).getByTestId("codex-pending-submit"));
    const post = fetchMock.mock.calls.find(
      ([url, init]) => String(url).endsWith("/codex/pending-response") && init?.method === "POST"
    );
    expect(post).toBeDefined();
    const { actionId, contextToken } = JSON.parse(String(post?.[1]?.body));
    expect(actionId).toBeTruthy();
    expect(contextToken).toBe(original.contextToken);
    const oldAnchor = `codex:pending:${session.id}:${original.requestKey}:${contextToken}`;
    expect(sessionStorage.getItem(oldAnchor)).toBe(actionId);
    await act(async () => {
      releasePendingResponse(
        Response.json({ actionId, delivery: "unknown_delivery", route: "desktop_ipc" })
      );
    });
    fireEvent.click(within(card).getByTestId("codex-pending-verify"));
    const verificationCalls = fetchMock.mock.calls.filter(([url]) =>
      String(url).includes("/codex/action/verify")
    );
    expect(verificationCalls).toHaveLength(1);
    const [verifyUrl, verifyInit] = verificationCalls[0];
    expect(new URL(String(verifyUrl), window.location.href).searchParams.get("action_id")).toBe(
      actionId
    );
    expect(verifyInit?.method ?? "GET").toBe("GET");
    expect(within(card).queryByTestId("codex-pending-verify")).toBeNull();
    skipImmediate(card);

    const next = immediateQuestion(1, {
      contextToken: "tok-next",
      questions: [{ ...original.questions[0], question: "核对期间到达的新题", isSecret: true }],
    });
    act(() => stream.emit("state", immediateProjection([next], 2, false, 6)));
    const newCard = await findImmediateCard("核对期间到达的新题");
    const draft = within(newCard).getByTestId("codex-q-b6-q1-free");
    const submit = within(newCard).getByTestId("codex-pending-submit");
    fireEvent.change(draft, { target: { value: localDraft } });
    expect(submit).toBeEnabled();
    expect(within(newCard).queryByTestId("codex-pending-note")).toBeNull();

    await act(async () => {
      releaseVerification(deliveredVerification(actionId));
    });
    expect(draft).toHaveValue(localDraft);
    expect(submit).toBeEnabled();
    expect(within(newCard).queryByTestId("codex-pending-note")).toBeNull();
    expect(within(newCard).queryByTestId("codex-pending-verify")).toBeNull();
    expect(fetchMock.mock.calls.filter(([, init]) => (init?.method ?? "GET") !== "GET")).toEqual([
      post,
    ]);
  });

  // 真实普通发送门的 Rust 对照用例归主 agent 回归。
  // pending_idle_rejects_new_send_via_direct_http / unsupported_pending_blocks_send_too。
  it("b6_pending_skip_and_restore_issue_no_mutations", async () => {
    const request = immediateQuestion(1);
    const pending = [request];
    const { fetchMock } = stubImmediateFetch(pending);
    const { stream } = await mountImmediate(pending);
    const card = await findImmediateCard();
    const beforeClicks = fetchMock.mock.calls.length;

    await act(async () => skipImmediate(card));
    expect(card).not.toBeVisible();
    act(() => stream.emit("state", immediateProjection(pending, 2)));
    expect(card).not.toBeVisible();
    await act(async () => restoreImmediate());

    expect(card).toBeVisible();
    expect(within(card).getByTestId("codex-pending-request-key")).toHaveTextContent(
      request.requestKey
    );
    const callsDuringClicks = fetchMock.mock.calls.slice(beforeClicks);
    const writes = callsDuringClicks.filter(([, init]) => (init?.method ?? "GET") !== "GET");
    expect(writes).toEqual([]);
  });
});
