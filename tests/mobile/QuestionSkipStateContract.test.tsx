import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect, useLayoutEffect } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import QuestionSkipControls from "@/mobile/QuestionSkipControls";
import { questionSkipViews, response } from "./questionSkipFixtures";

const { group, active, skipped } = questionSkipViews("contract-thread", "generation");
const callsTo = (mock: ReturnType<typeof vi.fn>, endpoint: string) =>
  mock.mock.calls.filter(([url]) => String(url).endsWith(`/${endpoint}`));
const ack = () => {
  for (const name of ["thread", "count", "draft"])
    fireEvent.click(screen.getByTestId(`question-skip-ack-${name}`));
};
const preview = {
  challengeId: "challenge",
  candidates: [{ token: "a", question: "Question?", options: ["A"] }],
};
async function beginSkip() {
  fireEvent.click(screen.getByTestId("question-skip-start"));
  fireEvent.click(await screen.findByTestId("question-skip-candidate-a"));
  ack();
  fireEvent.click(screen.getByTestId("question-skip-confirm"));
}
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("committed question skip operation contract", () => {
  it("preserves candidate and acknowledgements when an equal gateFact gets a new reference", async () => {
    const fetchMock = vi.fn().mockResolvedValue(response(preview));
    vi.stubGlobal("fetch", fetchMock);
    const firstGate = { decision: "active" as const, revision: 0 };
    const nextGate = { decision: "active" as const, revision: 0 };
    expect(nextGate).not.toBe(firstGate);
    const rendered = render(
      <QuestionSkipControls sessionId="gate-reference-pairing" view={active} gateFact={firstGate} />
    );
    fireEvent.click(screen.getByTestId("question-skip-start"));
    fireEvent.click(await screen.findByTestId("question-skip-candidate-a"));
    ack();
    expect((screen.getByTestId("question-skip-confirm") as HTMLButtonElement).disabled).toBe(false);
    rendered.rerender(
      <QuestionSkipControls sessionId="gate-reference-pairing" view={active} gateFact={nextGate} />
    );
    expect(screen.getByTestId("question-skip-candidate-a").getAttribute("aria-pressed")).toBe(
      "true"
    );
    for (const name of ["thread", "count", "draft"])
      expect(screen.getByTestId(`question-skip-ack-${name}`).getAttribute("aria-pressed")).toBe(
        "true"
      );
    expect((screen.getByTestId("question-skip-confirm") as HTMLButtonElement).disabled).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("retains an unknown skip across disabled, unmount and A-B-A without new pairing", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).endsWith("/pair-preview")) return response(preview);
      if (String(url).endsWith("/pair-confirm"))
        return response({ bindingId: "binding", expiresAt: 1234 });
      throw new Error("unknown delivery");
    });
    vi.stubGlobal("fetch", fetchMock);
    const ui = (sessionId: string, disabled = false) => (
      <QuestionSkipControls sessionId={sessionId} view={active} disabled={disabled} />
    );
    const rendered = render(ui("skip-scope-A"));
    await beginSkip();
    await screen.findByTestId("question-skip-retry");
    const original = callsTo(fetchMock, "skip")[0][1]?.body;
    rendered.rerender(ui("skip-scope-A", true));
    rendered.rerender(ui("skip-scope-A"));
    rendered.rerender(ui("skip-scope-B"));
    rendered.rerender(ui("skip-scope-A"));
    rendered.unmount();
    render(ui("skip-scope-A"));
    fireEvent.click(await screen.findByTestId("question-skip-retry"));
    await waitFor(() => expect(callsTo(fetchMock, "skip")).toHaveLength(2));
    expect(callsTo(fetchMock, "skip")[1][1]?.body).toBe(original);
    expect(callsTo(fetchMock, "pair-preview")).toHaveLength(1);
    expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(1);
  });

  it("keeps unknown restore immutable across disabled and remount; ordinary restore cannot issue a new ID", async () => {
    const fetchMock = vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => {
      throw new Error("unknown delivery");
    });
    vi.stubGlobal("fetch", fetchMock);
    const ui = (disabled = false) => (
      <QuestionSkipControls sessionId="restore-scope" view={skipped} disabled={disabled} />
    );
    const rendered = render(ui());
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-retry");
    const original = fetchMock.mock.calls[0][1]?.body;
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    expect(fetchMock).toHaveBeenCalledTimes(1);
    rendered.rerender(ui(true));
    rendered.rerender(ui());
    rendered.unmount();
    render(ui());
    fireEvent.click(await screen.findByTestId("question-skip-retry"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[1][1]?.body).toBe(original);
  });

  it("does not repeat a successful skip while active props are stale", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
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
    });
    vi.stubGlobal("fetch", fetchMock);
    const onChanged = vi.fn();
    const rendered = render(
      <QuestionSkipControls sessionId="done-skip" view={active} onChanged={onChanged} />
    );
    await beginSkip();
    await waitFor(() => expect(callsTo(fetchMock, "skip")).toHaveLength(1));
    await waitFor(() => expect(onChanged).toHaveBeenCalledTimes(1));
    await waitFor(() => {
      const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
      expect(start === null || start.disabled).toBe(true);
    });
    rendered.unmount();
    render(<QuestionSkipControls sessionId="done-skip" view={active} onChanged={onChanged} />);
    const start = screen.queryByTestId("question-skip-start") as HTMLButtonElement | null;
    expect(start === null || start.disabled).toBe(true);
    if (start) fireEvent.click(start);
    expect(callsTo(fetchMock, "pair-preview")).toHaveLength(1);
  });

  it("does not repeat a successful restore while skipped props are stale", async () => {
    const fetchMock = vi.fn(async (_url: RequestInfo | URL, init?: RequestInit) => {
      const body = JSON.parse(String(init?.body));
      return response({ actionId: body.actionId, group, displayDecision: "active", revision: 2 });
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="done-restore" view={skipped} />);
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await screen.findByTestId("question-skip-restored");
    const restore = screen.queryByTestId("question-skip-restore") as HTMLButtonElement | null;
    expect(restore === null || restore.disabled).toBe(true);
    if (restore) fireEvent.click(restore);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("clears all confirmations when switching labeled candidates and requires them again", async () => {
    const candidates = [
      Object.assign({ token: "a", question: "Same?", options: ["A"] }, { label: "Window Alpha" }),
      Object.assign({ token: "b", question: "Same?", options: ["A"] }, { label: "Window Beta" }),
    ];
    const fetchMock = vi
      .fn()
      .mockResolvedValue(response({ challengeId: "candidate-challenge", candidates }));
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="candidate-switch" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    const first = await screen.findByTestId("question-skip-candidate-a");
    fireEvent.click(first);
    ack();
    fireEvent.click(screen.getByTestId("question-skip-candidate-b"));
    for (const name of ["thread", "count", "draft"])
      expect(screen.getByTestId(`question-skip-ack-${name}`).getAttribute("aria-pressed")).toBe(
        "false"
      );
    fireEvent.click(screen.getByTestId("question-skip-confirm"));
    expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(0);
  });

  it("shows distinct labels for candidates with identical question text", async () => {
    const candidates = [
      Object.assign({ token: "a", question: "Same?", options: ["A"] }, { label: "Window Alpha" }),
      Object.assign({ token: "b", question: "Same?", options: ["A"] }, { label: "Window Beta" }),
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(response({ challengeId: "labels", candidates }))
    );
    render(<QuestionSkipControls sessionId="candidate-labels" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    expect((await screen.findByTestId("question-skip-candidate-a")).textContent).toContain(
      "Window Alpha"
    );
    expect(screen.getByTestId("question-skip-candidate-b").textContent).toContain("Window Beta");
  });

  it("does not confirm indistinguishable candidates without labels", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      response({
        challengeId: "ambiguous",
        candidates: [
          { token: "a", question: "Same?", options: ["A"] },
          { token: "b", question: "Same?", options: ["A"] },
        ],
      })
    );
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="candidate-ambiguous" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    fireEvent.click(await screen.findByTestId("question-skip-candidate-a"));
    ack();
    const confirm = screen.getByTestId("question-skip-confirm") as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(confirm);
    expect(callsTo(fetchMock, "pair-confirm")).toHaveLength(0);
  });

  it("does not send skip when disabled commits before the pending confirm resolves in a sibling layout effect", async () => {
    let resolveConfirm!: (value: Response) => void;
    const events: string[] = [];
    const fetchMock = vi.fn((url: RequestInfo | URL) => {
      if (String(url).endsWith("/pair-preview")) return Promise.resolve(response(preview));
      if (String(url).endsWith("/pair-confirm"))
        return new Promise<Response>((resolve) => {
          resolveConfirm = resolve;
        });
      events.push("skip-send");
      return Promise.reject(new Error("unexpected skip"));
    });
    vi.stubGlobal("fetch", fetchMock);
    function Sibling({ disabled }: { disabled: boolean }) {
      useLayoutEffect(() => {
        if (disabled) {
          events.push("disabled-layout");
          resolveConfirm(response({ bindingId: "binding", expiresAt: 1234 }));
        }
      }, [disabled]);
      useEffect(() => {
        if (disabled) events.push("sibling-passive");
      }, [disabled]);
      return null;
    }
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);
    try {
      await act(async () => {
        root.render(
          <>
            <QuestionSkipControls sessionId="layout-gap" view={active} />
            <Sibling disabled={false} />
          </>
        );
      });
      await beginSkip();
      await waitFor(() => expect(resolveConfirm).toBeTypeOf("function"));
      // flushSync commits the new props; promise continuations then run outside act.
      const { flushSync } = await import("react-dom");
      flushSync(() =>
        root.render(
          <>
            <QuestionSkipControls sessionId="layout-gap" view={active} disabled />
            <Sibling disabled />
          </>
        )
      );
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(events).toEqual(["disabled-layout", "sibling-passive"]);
      expect(callsTo(fetchMock, "skip")).toHaveLength(0);
    } finally {
      await act(async () => root.unmount());
      container.remove();
    }
  });
});
