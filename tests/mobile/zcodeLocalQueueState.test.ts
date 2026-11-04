import { describe, expect, it } from "vitest";
import {
  findRecoverableEdit,
  mergeLocalQueue,
  zcodeQueuePreview,
} from "@/mobile/zcode-local-queue-state";
import type { LocalQueueView } from "@/lib/api/zcodeControl";

const view = (version: number, text = "合成"): LocalQueueView => ({
  sessionId: "s1",
  version,
  autoDrain: true,
  pauseReason: null,
  items: [
    {
      itemId: "i1",
      itemVersion: 1,
      state: "queued",
      position: 1,
      textPreview: text,
      hasAttachments: false,
    },
  ],
  recoverableEdits: [],
});

describe("zcode local queue state", () => {
  it("preview_uses_20_graphemes_and_marks_ellipsis", () => {
    expect(zcodeQueuePreview("😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀")).toBe(
      "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀"
    );
    expect(zcodeQueuePreview("😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀")).toBe(
      "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀…"
    );
    expect(zcodeQueuePreview("第一行\n第二行")).toBe("第一行 第二行");
  });

  it("merge_never_rolls_back_version_or_crosses_session", () => {
    expect(mergeLocalQueue(null, view(2)).version).toBe(2);
    expect(mergeLocalQueue(view(3), view(2)).version).toBe(3);
    expect(mergeLocalQueue(view(2), view(3)).version).toBe(3);
    const other = { ...view(9), sessionId: "s2" };
    expect(mergeLocalQueue(view(3), other).sessionId).toBe("s2");
  });

  it("recoverable_edit_is_found_by_request_id", () => {
    const base = view(1);
    base.recoverableEdits = [{ requestId: "edit-1", itemId: "i1", expiresAtMs: 10 }];
    expect(findRecoverableEdit(base, "edit-1")?.itemId).toBe("i1");
    expect(findRecoverableEdit(base, "missing")).toBeNull();
  });
});
