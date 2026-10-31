// F5 用户感知口径（2026-10-31 裁决）：点击后的卡片消失由**前端乐观隐藏**保证 ≤1s；
// 后端命令内部的补扫等待（在飞 ≤2s）只影响权威快照收敛，不作为门禁。
//
// 缓存形状 = `useSessionsQuery` 的 `SessionsResponse`（home 经 useSessions 同步到
// Zustand store）。乐观更新后，`useSessions` 的 effect 会同步列表 → 卡片立即消失；
// 命令结束后（成功/失败）调用方一律 invalidateQueries 回取权威快照（失败即回滚）。
import type { QueryClient } from "@tanstack/react-query";
import type { SessionsResponse } from "@/types/session";

export const SESSIONS_QUERY_KEY = ["sessions"] as const;

export type SessionKey = { id: string; agentType: string };

const sameSession = (s: SessionKey, k: SessionKey) => s.id === k.id && s.agentType === k.agentType;

/**
 * 单卡乐观隐藏：
 * - 未读卡 X = 已读语义（spec W4）→ 缓存里该会话 `unread=false`（卡面不再是未读）；
 * - 非未读卡 X = 暂离不提示 → 从列表移除，并同步 `totalCount`/`waitingCount`。
 */
export function hideSessionOptimistically(
  queryClient: QueryClient,
  target: SessionKey & { unread: boolean }
): void {
  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    if (target.unread) {
      return {
        ...prev,
        sessions: prev.sessions.map((s) => (sameSession(s, target) ? { ...s, unread: false } : s)),
      };
    }
    const removed = prev.sessions.find((s) => sameSession(s, target));
    if (!removed) return prev;
    const sessions = prev.sessions.filter((s) => !sameSession(s, target));
    return {
      ...prev,
      sessions,
      totalCount: sessions.length,
      waitingCount: prev.waitingCount - (removed.status === "waiting" ? 1 : 0),
    };
  });
}

/**
 * 批量乐观隐藏（一键清除）：一次 `setQueryData` 移除全部目标卡，避免 N 次渲染。
 * 未读/已读的分流仍由后端 `dismiss_session_cards` 按未读池权威判定。
 */
export function hideSessionsOptimistically(queryClient: QueryClient, targets: SessionKey[]): void {
  if (targets.length === 0) return;
  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    const keys = new Set(targets.map((t) => `${t.agentType}:${t.id}`));
    const removed = prev.sessions.filter((s) => keys.has(`${s.agentType}:${s.id}`));
    if (removed.length === 0) return prev;
    const sessions = prev.sessions.filter((s) => !keys.has(`${s.agentType}:${s.id}`));
    const removedWaiting = removed.filter((s) => s.status === "waiting").length;
    return {
      ...prev,
      sessions,
      totalCount: sessions.length,
      waitingCount: Math.max(0, prev.waitingCount - removedWaiting),
    };
  });
}
