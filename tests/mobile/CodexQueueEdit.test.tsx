import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import CodexQueuePanel from "@/mobile/CodexQueuePanel";
import * as api from "@/mobile/codexIpcApi";
vi.mock("@/mobile/codexIpcApi", () => ({
  fetchCodexQueue: vi.fn(),
  editCodexQueueMessage: vi.fn(),
  forceContinueCodexAction: vi.fn(),
  prioritizeCodexQueueMessage: vi.fn(),
  resumeCodexQueue: vi.fn(),
  retractCodexAction: vi.fn(),
}));
afterEach(cleanup);
it("running queue has only row controls and re-edits only after server success", async () => {
  vi.mocked(api.fetchCodexQueue).mockResolvedValue({
    actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
    blockedReasonCode: "native_turn_pending",
    canInterrupt: true,
    blockedActionIds: [],
  });
  const onEdit = vi.fn();
  render(<CodexQueuePanel sessionId="s1" onEditDraft={onEdit} />);
  fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
  expect(screen.queryByTestId("codex-queue-title")).toBeNull();
  expect(screen.queryByTestId("codex-queue-blocked")).toBeNull();
  expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
  let resolve!: (v: { actionId: string; text: string }) => void;
  vi.mocked(api.editCodexQueueMessage).mockReturnValue(
    new Promise((r) => {
      resolve = r;
    })
  );
  fireEvent.click(screen.getByTestId("codex-queue-edit-a1"));
  expect(onEdit).not.toHaveBeenCalled();
  resolve({ actionId: "a1", text: "original" });
  await waitFor(() =>
    expect(onEdit).toHaveBeenCalledWith({ sessionId: "s1", actionId: "a1", text: "original" })
  );
});

it("failed CAS never fills a draft and stale session responses never transfer", async () => {
  vi.mocked(api.fetchCodexQueue).mockResolvedValue({
    actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
    blockedReasonCode: null,
    blockedActionIds: [],
  });
  vi.mocked(api.editCodexQueueMessage).mockRejectedValueOnce(new Error("already dispatched"));
  const onEdit = vi.fn();
  const { rerender } = render(<CodexQueuePanel sessionId="s1" onEditDraft={onEdit} />);
  fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
  fireEvent.click(screen.getByTestId("codex-queue-edit-a1"));
  await screen.findByTestId("codex-queue-note");
  expect(onEdit).not.toHaveBeenCalled();
  let resolve!: (v: { actionId: string; text: string }) => void;
  vi.mocked(api.editCodexQueueMessage).mockReturnValue(
    new Promise((r) => {
      resolve = r;
    })
  );
  fireEvent.click(screen.getByTestId("codex-queue-edit-a1"));
  rerender(<CodexQueuePanel sessionId="s2" onEditDraft={onEdit} />);
  resolve({ actionId: "a1", text: "old" });
  await waitFor(() => expect(api.fetchCodexQueue).toHaveBeenCalledWith("s2"));
  expect(onEdit).not.toHaveBeenCalled();
});

it("unmount invalidates an in-flight edit callback", async () => {
  vi.mocked(api.fetchCodexQueue).mockResolvedValue({
    actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
    blockedReasonCode: null,
    blockedActionIds: [],
  });
  let resolve!: (v: { actionId: string; text: string }) => void;
  vi.mocked(api.editCodexQueueMessage).mockReturnValue(
    new Promise((r) => {
      resolve = r;
    })
  );
  const onEdit = vi.fn();
  const { unmount } = render(<CodexQueuePanel sessionId="s1" onEditDraft={onEdit} />);
  fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
  fireEvent.click(screen.getByTestId("codex-queue-edit-a1"));
  unmount();
  resolve({ actionId: "a1", text: "old" });
  await Promise.resolve();
  expect(onEdit).not.toHaveBeenCalled();
});
