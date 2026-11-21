import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import QuestionSkipControls from "@/mobile/QuestionSkipControls";
import { questionSkipViews, response } from "./questionSkipFixtures";

const { group, active, skipped } = questionSkipViews("thread", "gen");
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("single-question skip controls", () => {
  it("requires candidate selection and three acknowledgements before confirmation", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      response({
        challengeId: "c",
        candidates: [{ token: "candidate", question: "Question?", options: ["A"] }],
      })
    );
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="controls-candidate" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    await screen.findByText("Question?");
    const confirm = screen.getByTestId("question-skip-confirm") as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(screen.getByTestId("question-skip-candidate-candidate"));
    fireEvent.click(screen.getByTestId("question-skip-ack-thread"));
    fireEvent.click(screen.getByTestId("question-skip-ack-count"));
    expect(confirm.disabled).toBe(true);
    fireEvent.click(screen.getByTestId("question-skip-ack-draft"));
    expect(confirm.disabled).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("keeps active pairing open when the same-scope read-only record unmounts", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        response({
          challengeId: "c",
          candidates: [{ token: "candidate", question: "Question?", options: ["A"] }],
        })
      )
    );
    const sessionId = "controls-history-unmount";
    render(<QuestionSkipControls sessionId={sessionId} view={active} />);
    const history = render(<QuestionSkipControls sessionId={sessionId} view={active} readOnly />);

    fireEvent.click(screen.getByTestId("question-skip-start"));
    await screen.findByTestId("question-skip-candidate-candidate");
    history.unmount();

    expect(screen.queryByTestId("question-skip-candidate-candidate")).toBeTruthy();
  });

  it("retries an issued restore with the identical action and packet", async () => {
    const fetchMock = vi
      .fn()
      .mockRejectedValueOnce(new Error("network"))
      .mockImplementationOnce(async (_url, init) => {
        const body = JSON.parse(String(init.body));
        return response({ actionId: body.actionId, group, displayDecision: "active", revision: 3 });
      });
    vi.stubGlobal("fetch", fetchMock);
    const view = { ...skipped, revision: 2 };
    render(<QuestionSkipControls sessionId="controls-restore" view={view} />);
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    fireEvent.click(await screen.findByTestId("question-skip-retry"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[1][1].body).toBe(fetchMock.mock.calls[0][1].body);
    expect(JSON.parse(fetchMock.mock.calls[0][1].body).actionId).toBeTruthy();
    expect(screen.queryByTestId("question-skip-start")).toBeNull();
  });

  it("retries an issued skip after a network failure with the same packet without pairing again", async () => {
    const fetchMock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      const path = String(url);
      if (path.endsWith("/pair-preview"))
        return response({
          challengeId: "c",
          candidates: [{ token: "candidate", question: "Question?", options: ["A"] }],
        });
      if (path.endsWith("/pair-confirm"))
        return response({ bindingId: "binding", expiresAt: 1234 });
      if (path.endsWith("/skip")) {
        const calls = fetchMock.mock.calls.filter(([called]) => String(called).endsWith("/skip"));
        if (calls.length === 1) throw new Error("network");
        const body = JSON.parse(String(init?.body));
        return response({
          actionId: body.actionId,
          group,
          displayDecision: "skipped",
          nativeEffect: "unresolved",
          revision: 1,
        });
      }
      throw new Error(`unexpected endpoint ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    render(<QuestionSkipControls sessionId="controls-skip" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    fireEvent.click(await screen.findByTestId("question-skip-candidate-candidate"));
    for (const name of ["thread", "count", "draft"])
      fireEvent.click(screen.getByTestId(`question-skip-ack-${name}`));
    expect((screen.getByTestId("question-skip-confirm") as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByTestId("question-skip-confirm"));
    fireEvent.click(await screen.findByTestId("question-skip-retry"));
    await waitFor(() =>
      expect(fetchMock.mock.calls.filter(([url]) => String(url).endsWith("/skip"))).toHaveLength(2)
    );
    const calls = fetchMock.mock.calls.filter(([url]) => String(url).endsWith("/skip"));
    expect(calls[1][1]?.body).toBe(calls[0][1]?.body);
    expect(JSON.parse(String(calls[0][1]?.body))).toMatchObject({
      bindingId: "binding",
      actionId: expect.any(String),
      group,
    });
    expect(
      fetchMock.mock.calls.filter(([url]) => String(url).endsWith("/pair-preview"))
    ).toHaveLength(1);
    expect(
      fetchMock.mock.calls.filter(([url]) => String(url).endsWith("/pair-confirm"))
    ).toHaveLength(1);
  });

  it("cancels a pending confirmation when disabled toggles before the skip write", async () => {
    let resolveConfirm!: (value: Response) => void;
    const fetchMock = vi.fn((url: RequestInfo | URL) => {
      if (String(url).endsWith("/pair-preview"))
        return Promise.resolve(
          response({
            challengeId: "c",
            candidates: [{ token: "candidate", question: "Question?", options: [] }],
          })
        );
      if (String(url).endsWith("/pair-confirm"))
        return new Promise<Response>((resolve) => {
          resolveConfirm = resolve;
        });
      throw new Error(`unexpected write ${String(url)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    const rendered = render(<QuestionSkipControls sessionId="controls-disabled" view={active} />);
    fireEvent.click(screen.getByTestId("question-skip-start"));
    fireEvent.click(await screen.findByTestId("question-skip-candidate-candidate"));
    for (const name of ["thread", "count", "draft"])
      fireEvent.click(screen.getByTestId(`question-skip-ack-${name}`));
    expect((screen.getByTestId("question-skip-confirm") as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByTestId("question-skip-confirm"));
    await waitFor(() => expect(resolveConfirm).toBeTypeOf("function"));
    rendered.rerender(
      <QuestionSkipControls sessionId="controls-disabled" view={active} disabled />
    );
    rendered.rerender(
      <QuestionSkipControls sessionId="controls-disabled" view={active} disabled={false} />
    );
    await act(async () => {
      resolveConfirm(response({ bindingId: "binding", expiresAt: 1234 }));
    });
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toEqual([
      "/m/api/v1/codex/question-skip/pair-preview",
      "/m/api/v1/codex/question-skip/pair-confirm",
    ]);
    expect(screen.getByTestId("question-skip-start")).toBeTruthy();
  });

  it("ignores an old scope's pending restore result while the new scope remains pending", async () => {
    const resolvers: Array<(value: Response) => void> = [];
    const onChanged = vi.fn();
    const fetchMock = vi.fn(
      (_url: RequestInfo | URL, _init?: RequestInit) =>
        new Promise<Response>((resolve) => {
          resolvers.push(resolve);
        })
    );
    vi.stubGlobal("fetch", fetchMock);
    const view = { ...skipped, revision: 2 };
    const rendered = render(
      <QuestionSkipControls sessionId="controls-scope-A" view={view} onChanged={onChanged} />
    );
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    rendered.rerender(
      <QuestionSkipControls sessionId="controls-scope-B" view={view} onChanged={onChanged} />
    );
    fireEvent.click(screen.getByTestId("question-skip-restore"));
    expect(resolvers).toHaveLength(2);
    const first = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    const second = JSON.parse(String(fetchMock.mock.calls[1][1].body));
    await act(async () => {
      resolvers[0](
        response({ actionId: first.actionId, group, displayDecision: "active", revision: 3 })
      );
    });
    expect(onChanged).not.toHaveBeenCalled();
    expect((screen.getByTestId("question-skip-restore") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByTestId("question-skip-restored")).toBeNull();
    await act(async () => {
      resolvers[1](
        response({ actionId: second.actionId, group, displayDecision: "active", revision: 3 })
      );
    });
    expect(onChanged).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("question-skip-restored")).toBeTruthy();
  });

  it("does not expose native closure for an unresolved effect", () => {
    render(<QuestionSkipControls sessionId="controls-effect" view={skipped} />);
    expect(screen.getByTestId("question-skip-effect-unresolved")).toBeTruthy();
    expect(screen.queryByTestId("question-skip-effect-closed_verified")).toBeNull();
  });
});
