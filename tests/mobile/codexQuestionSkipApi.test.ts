import { afterEach, describe, expect, it, vi } from "vitest";
import {
  confirmQuestionSkip,
  previewQuestionSkip,
  restoreQuestion,
  skipQuestion,
} from "@/mobile/codexQuestionSkipApi";

const group = {
  threadId: "thread",
  sourceTurnId: "turn",
  sourceItemId: "item",
  contentGeneration: "generation",
};
const base = { sessionId: "session", group };
const skip = { ...base, bindingId: "binding", actionId: "action" };
const restore = { ...base, expectedRevision: 3, actionId: "restore" };
const json = (value: unknown) => new Response(JSON.stringify(value), { status: 200 });
afterEach(() => vi.unstubAllGlobals());

describe("question skip transport contract", () => {
  it("posts preview and confirmation using the same-origin device transport", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        json({
          challengeId: "challenge",
          candidates: [{ token: "candidate", question: "Question?", options: ["A"] }],
        })
      )
      .mockResolvedValueOnce(json({ bindingId: "binding", expiresAt: 1234 }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(previewQuestionSkip(base)).resolves.toMatchObject({ challengeId: "challenge" });
    const confirmation = {
      ...base,
      challengeId: "challenge",
      candidateToken: "candidate",
      sameThreadAndQuestion: true as const,
      nativePanelQuestionCount: 1 as const,
      nativeDraftWillBeCleared: true as const,
    };
    await expect(confirmQuestionSkip(confirmation)).resolves.toMatchObject({
      bindingId: "binding",
    });
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
      "/m/api/v1/codex/question-skip/pair-preview",
      "/m/api/v1/codex/question-skip/pair-confirm",
    ]);
    for (const [, init] of fetchMock.mock.calls) {
      expect(init.method).toBe("POST");
      expect(init.credentials === undefined || init.credentials === "same-origin").toBe(true);
    }
    expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual(confirmation);
  });

  it("accepts a matching skip receipt and sends the exact packet", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      json({
        actionId: "action",
        group,
        displayDecision: "skipped",
        nativeEffect: "unresolved",
        reasonCode: null,
        revision: 1,
      })
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(skipQuestion(skip)).resolves.toMatchObject({
      revision: 1,
      nativeEffect: "unresolved",
    });
    expect(fetchMock).toHaveBeenCalledOnce();
    expect(fetchMock.mock.calls[0][0]).toBe("/m/api/v1/codex/question-skip/skip");
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual(skip);
  });

  it.each([
    { actionId: "other" },
    { group: { ...group, contentGeneration: "other" } },
    { revision: 0 },
    { revision: Number.MAX_SAFE_INTEGER + 1 },
    { displayDecision: "active" },
  ])("rejects a mismatched or unsafe skip receipt %#", async (patch) => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        json({
          actionId: "action",
          group,
          displayDecision: "skipped",
          nativeEffect: "unresolved",
          reasonCode: null,
          revision: 1,
          ...patch,
        })
      )
    );
    await expect(skipQuestion(skip)).rejects.toThrow();
    expect(fetch).toHaveBeenCalledOnce();
  });

  it("accepts restore only when the matching revision advances", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        json({ actionId: "restore", group, displayDecision: "active", revision: 4 })
      );
    vi.stubGlobal("fetch", fetchMock);
    await expect(restoreQuestion(restore)).resolves.toMatchObject({ revision: 4 });
    expect(fetchMock.mock.calls[0][0]).toBe("/m/api/v1/codex/question-skip/restore");
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual(restore);
  });

  it("rejects restore receipts with a wrong action, wrong group, or missing revision", async () => {
    for (const patch of [
      { actionId: "other" },
      { group: { ...group, sourceItemId: "other" } },
      { revision: undefined },
    ]) {
      const fetchMock = vi
        .fn()
        .mockResolvedValue(
          json({ actionId: "restore", group, displayDecision: "active", revision: 4, ...patch })
        );
      vi.stubGlobal("fetch", fetchMock);
      await expect(restoreQuestion(restore)).rejects.toThrow();
      expect(fetchMock).toHaveBeenCalledOnce();
    }
  });

  it.each([3, 2, Number.MAX_SAFE_INTEGER + 1])(
    "rejects stale or unsafe restore revision %s",
    async (revision) => {
      vi.stubGlobal(
        "fetch",
        vi
          .fn()
          .mockResolvedValue(
            json({ actionId: "restore", group, displayDecision: "active", revision })
          )
      );
      await expect(restoreQuestion(restore)).rejects.toThrow();
      expect(fetch).toHaveBeenCalledOnce();
    }
  );
});
