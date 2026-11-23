import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const session = "anchor-test-session";
type AnchorModule = typeof import("@/mobile/opencodeActionAnchors");
let anchors: AnchorModule;

function anchor(actionId: string, at: number) {
  return { actionId, kind: "answer" as const, targetId: `target-${actionId}`, at };
}

beforeEach(async () => {
  vi.resetModules();
  sessionStorage.clear();
  anchors = await import("@/mobile/opencodeActionAnchors");
});

afterEach(() => {
  vi.restoreAllMocks();
  sessionStorage.clear();
});

describe("OpenCode 动作恢复锚", () => {
  it("合并内存和存储时按时间去重并只显示最新 20 项", () => {
    const write = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota", "QuotaExceededError");
    });
    anchors.addActionAnchor(session, anchor("memory", 50), 50);
    anchors.addActionAnchor(session, anchor("duplicate", 40), 40);
    write.mockRestore();
    const stored = Array.from({ length: 21 }, (_, i) => anchor(`stored-${i}`, i + 1));
    sessionStorage.setItem(
      anchors.actionAnchorStorageKey(session),
      JSON.stringify([anchor("duplicate", 5), ...stored.reverse()])
    );

    expect(anchors.readActionAnchors(session)).toEqual([
      ...Array.from({ length: 18 }, (_, i) => anchor(`stored-${i + 3}`, i + 4)),
      anchor("duplicate", 40),
      anchor("memory", 50),
    ]);
  });

  it("写入时持久化合并后按时间去重的最新 20 项", () => {
    const write = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota", "QuotaExceededError");
    });
    anchors.addActionAnchor(session, anchor("memory", 40), 40);
    write.mockRestore();
    const stored = Array.from({ length: 21 }, (_, i) => anchor(`stored-${i}`, i + 1));
    sessionStorage.setItem(
      anchors.actionAnchorStorageKey(session),
      JSON.stringify(stored.reverse())
    );

    anchors.addActionAnchor(session, anchor("new", 50), 50);

    expect(JSON.parse(sessionStorage.getItem(anchors.actionAnchorStorageKey(session))!)).toEqual([
      ...Array.from({ length: 18 }, (_, i) => anchor(`stored-${i + 3}`, i + 4)),
      anchor("memory", 40),
      anchor("new", 50),
    ]);
  });

  it.each(["{}", "null", '"invalid"'])("存储为非数组 %s 时仍显示内存恢复锚", (raw) => {
    anchors.addActionAnchor(session, anchor("memory", 10), 10);
    sessionStorage.setItem(anchors.actionAnchorStorageKey(session), raw);

    expect(anchors.readActionAnchors(session)).toEqual([anchor("memory", 10)]);
  });

  it("删除仅存在内存中的锚时通知对应会话", () => {
    const write = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota", "QuotaExceededError");
    });
    anchors.addActionAnchor(session, anchor("memory", 10), 10);
    write.mockRestore();
    sessionStorage.setItem(anchors.actionAnchorStorageKey(session), "[]");
    const events: string[] = [];
    const listener = (event: Event) =>
      events.push((event as CustomEvent<{ sessionId: string }>).detail.sessionId);
    globalThis.addEventListener(anchors.ACTION_ANCHOR_CHANGED_EVENT, listener);
    try {
      anchors.removeActionAnchor(session, "memory");
      expect(events).toEqual([session]);
      expect(anchors.readActionAnchors(session)).toEqual([]);
    } finally {
      globalThis.removeEventListener(anchors.ACTION_ANCHOR_CHANGED_EVENT, listener);
    }
  });

  it("超过显示上限后最旧的内存动作仍保持未决锁", () => {
    for (let i = 0; i < 21; i++) {
      anchors.addActionAnchor(session, anchor(`action-${i}`, i), i);
    }

    expect(anchors.hasUnresolvedAction(session, "answer", "target-action-0", "binding")).toBe(true);
  });
});
