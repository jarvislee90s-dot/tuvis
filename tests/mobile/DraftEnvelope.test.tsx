import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import AsyncQuestionGroupCard from "@/mobile/AsyncQuestionGroupCard";
import type { CodexAsyncQuestionView } from "@/mobile/codexIpcApi";

const group = {
  threadId: "thread",
  sourceTurnId: "turn",
  sourceItemId: "item",
  contentGeneration: "gen-1",
};
const question: CodexAsyncQuestionView = {
  questionKey: "real-question-key",
  contextToken: "token-1",
  sourceTurnId: "turn",
  sourceItemId: "item",
  questionIndex: 0,
  title: "Question one",
  options: ["A", "B"],
  state: "pending",
  actionable: true,
  disabledReasonCode: null,
};
const view = {
  group,
  revision: 0,
  decision: "active" as const,
  latestNativeEffect: null,
  reasonCode: null,
};
// 附加投影由服务端提供，现阶段父组件尚未声明该接口。
const props = (q: CodexAsyncQuestionView, g = view, sessionId = "draft-token") =>
  Object.assign({ sessionId, questions: [q] }, { questionDisplayGroup: g });
beforeAll(async () => {
  await i18n.changeLanguage("zh");
});
afterEach(() => {
  cleanup();
  localStorage.clear();
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

describe("verified question draft persistence", () => {
  it("preserves the answer across token rotation and submits only the current token", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          actionId: "a",
          delivery: "accepted",
          route: "desktop_ipc",
          turnId: null,
          envelopeId: null,
          reasonCode: null,
        }),
        { status: 200 }
      )
    );
    vi.stubGlobal("fetch", fetchMock);
    const rendered = render(<AsyncQuestionGroupCard {...props(question)} />);
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    rendered.rerender(
      <AsyncQuestionGroupCard {...props({ ...question, contextToken: "token-2" })} />
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "A" }).getAttribute("aria-pressed")).toBe("true")
    );
    expect(fetchMock).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("codex-async-submit"));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body.answers[0].contextToken).toBe("token-2");
    expect(JSON.stringify(body)).not.toContain("token-1");
  });

  it("retains a verified nonempty draft through descriptor absence and recovery", async () => {
    const rendered = render(
      <AsyncQuestionGroupCard {...Object.assign(props(question), { sessionId: "draft-absence" })} />
    );
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    rendered.rerender(<AsyncQuestionGroupCard sessionId="draft-absence" questions={[question]} />);
    rendered.unmount();
    render(
      <AsyncQuestionGroupCard {...Object.assign(props(question), { sessionId: "draft-absence" })} />
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "A" }).getAttribute("aria-pressed")).toBe("true")
    );
  });

  it("retains changed-content draft isolation through two unmounts", async () => {
    const rendered = render(<AsyncQuestionGroupCard {...props(question, view, "draft-changed")} />);
    fireEvent.click(screen.getByRole("button", { name: "A" }));
    const changed = { ...question, contextToken: "token-2", title: "New question" };
    const changedView = { ...view, group: { ...group, contentGeneration: "gen-2" } };
    rendered.rerender(<AsyncQuestionGroupCard {...props(changed, changedView, "draft-changed")} />);
    rendered.unmount();
    const next = render(
      <AsyncQuestionGroupCard {...props(changed, changedView, "draft-changed")} />
    );
    next.unmount();
    render(<AsyncQuestionGroupCard {...props(changed, changedView, "draft-changed")} />);
    expect(await screen.findByTestId("codex-async-stale-toggle")).toBeTruthy();
    fireEvent.click(screen.getByTestId("codex-async-stale-toggle"));
    expect(screen.getByTestId("codex-async-stale-item").textContent).toContain("Question one");
    expect(screen.getByTestId("codex-async-stale-restore")).toBeTruthy();
    expect(screen.getByRole("button", { name: "A" }).getAttribute("aria-pressed")).toBe("false");
  });
});
