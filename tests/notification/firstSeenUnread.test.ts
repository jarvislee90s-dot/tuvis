// tests/notification/firstSeenUnread.test.ts — review F5：首见未读卡补发通知的新鲜度门控
// 重启/补偿场景下 lastActivityAt（转绿时间）超过 2 分钟的老卡必须静默显示，不重放历史通知
import { describe, expect, it } from "vitest";
import { FIRST_SEEN_UNREAD_FRESH_MS, isFreshFirstSeenUnread } from "@/hooks/useNotification";

const NOW = 1_800_000_000_000;

const mkSession = (overrides: {
  unread?: boolean;
  status?: string;
  lastActivityAt?: string;
  unreadTurnedGreenAtMs?: number | null;
}) => ({
  unread: overrides.unread ?? true,
  status: overrides.status ?? "idle",
  lastActivityAt: overrides.lastActivityAt ?? new Date(NOW - 30_000).toISOString(),
  unreadTurnedGreenAtMs: overrides.unreadTurnedGreenAtMs,
});

describe("isFreshFirstSeenUnread（review F5 新鲜度门控）", () => {
  it("未读 + 绿色 + 转绿时间新鲜（30s 前）→ 补发通知", () => {
    expect(isFreshFirstSeenUnread(mkSession({}), NOW)).toBe(true);
  });

  it("转绿时间超过 2 分钟的老卡 → 静默（重启重放回归锁）", () => {
    expect(
      isFreshFirstSeenUnread(
        mkSession({ lastActivityAt: new Date(NOW - 10 * 60_000).toISOString() }),
        NOW
      )
    ).toBe(false);
  });

  it("边界：恰好 2 分钟内（含等值）→ 补发；超过 1ms → 静默", () => {
    const atBoundary = mkSession({
      lastActivityAt: new Date(NOW - FIRST_SEEN_UNREAD_FRESH_MS).toISOString(),
    });
    expect(isFreshFirstSeenUnread(atBoundary, NOW)).toBe(true);
    const pastBoundary = mkSession({
      lastActivityAt: new Date(NOW - FIRST_SEEN_UNREAD_FRESH_MS - 1).toISOString(),
    });
    expect(isFreshFirstSeenUnread(pastBoundary, NOW)).toBe(false);
  });

  it("非未读卡不通知", () => {
    expect(isFreshFirstSeenUnread(mkSession({ unread: false }), NOW)).toBe(false);
  });

  it("非绿色（运行中）未读卡不通知", () => {
    expect(isFreshFirstSeenUnread(mkSession({ status: "processing" }), NOW)).toBe(false);
  });

  it("lastActivityAt 无法解析（空串）→ 保守静默", () => {
    expect(isFreshFirstSeenUnread(mkSession({ lastActivityAt: "" }), NOW)).toBe(false);
  });

  it("业务活动十分钟前、转绿现在 → 用独立转绿时间通知一次", () => {
    expect(
      isFreshFirstSeenUnread(
        mkSession({
          lastActivityAt: new Date(NOW - 10 * 60_000).toISOString(),
          unreadTurnedGreenAtMs: NOW,
        }),
        NOW
      )
    ).toBe(true);
  });

  it("转绿时间超过 2 分钟 → 即使业务时间较新也静默", () => {
    expect(
      isFreshFirstSeenUnread(
        mkSession({
          lastActivityAt: new Date(NOW - 10_000).toISOString(),
          unreadTurnedGreenAtMs: NOW - 10 * 60_000,
        }),
        NOW
      )
    ).toBe(false);
  });

  it("已读后无新业务 → 不通知", () => {
    expect(
      isFreshFirstSeenUnread(mkSession({ unread: false, unreadTurnedGreenAtMs: NOW }), NOW)
    ).toBe(false);
  });
});
