// F5 用户感知口径（2026-10-31 终稿 09b95a3c）：
//  - 普通未读卡标已读：只要求**未读态** ≤1s 消失，卡面保留；
//  - 数据驱动聚合绿卡（codex/zcode/dsh 的 App 形态 idle/finished）：后端「已读即剔除」，
//    前端按同一规则**整卡**乐观移除；
//  - 隐藏/归档：整卡 ≤1s 消失。
//
// 双层保障：
//  1) `setQueryData` 立即改写 React Query 缓存（useSessions 同步到 store → 卡面立即变化）；
//  2) 模块级 pending 集合（useSyncExternalStore 订阅）：命令等在飞 ≤2s 期间，轮询回写的
//     旧快照不得让卡片/未读态复活；命令 finally invalidate 回取权威快照后清集合。
import type { QueryClient } from "@tanstack/react-query";
import type { Session, SessionsResponse } from "@/types/session";

export const SESSIONS_QUERY_KEY = ["sessions"] as const;

export type SessionKey = { id: string; agentType: string };

export const sessionKey = (s: SessionKey): string => `${s.agentType}:${s.id}`;

/** 镜像后端 `green_card_is_data_driven`（codex/zcode/dsh 的 App 形态绿灯聚合卡） */
const DATA_DRIVEN_GREEN_TOOLS = new Set(["codex", "zcode", "dsh"]);

export function isDataDrivenGreenCard(s: Session): boolean {
  return (
    DATA_DRIVEN_GREEN_TOOLS.has(s.agentType) &&
    s.form === "app" &&
    (s.status === "idle" || s.status === "finished")
  );
}

// ---- pending 集合（模块级，跨实例共享）----
export type PendingMode = "hidden" | "read";

const pending = new Map<string, PendingMode>();
let pendingVersion = 0;
const listeners = new Set<() => void>();

function emitPending(): void {
  pendingVersion += 1;
  for (const listener of listeners) listener();
}

export function subscribePendingSessions(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getPendingSessionsVersion(): number {
  return pendingVersion;
}

export function markPendingSession(target: SessionKey, mode: PendingMode): void {
  const key = sessionKey(target);
  if (pending.get(key) === mode) return;
  pending.set(key, mode);
  emitPending();
}

export function clearPendingSessions(targets: SessionKey[]): void {
  let changed = false;
  for (const target of targets) {
    if (pending.delete(sessionKey(target))) changed = true;
  }
  if (changed) emitPending();
}

/** home 渲染前的派生：hidden 整卡剔除；read 强制 unread=false（防轮询回写） */
export function applyPendingSessions(sessions: Session[]): Session[] {
  if (pending.size === 0) return sessions;
  const result: Session[] = [];
  for (const s of sessions) {
    const mode = pending.get(sessionKey(s));
    if (mode === "hidden") continue;
    result.push(mode === "read" ? { ...s, unread: false } : s);
  }
  return result;
}

// ---- 缓存乐观更新 ----

const sameSession = (s: SessionKey, k: SessionKey) => s.id === k.id && s.agentType === k.agentType;

function removeFromList(prev: SessionsResponse, target: SessionKey): SessionsResponse {
  const removed = prev.sessions.find((s) => sameSession(s, target));
  if (!removed) return prev;
  const sessions = prev.sessions.filter((s) => !sameSession(s, target));
  return {
    ...prev,
    sessions,
    totalCount: sessions.length,
    waitingCount: prev.waitingCount - (removed.status === "waiting" ? 1 : 0),
  };
}

/**
 * 单卡乐观隐藏（G1）：
 * - 非未读卡 / 数据驱动聚合绿卡 → 整卡从缓存移除 + pending=hidden；
 * - 普通未读卡 → 仅置 `unread=false` + pending=read（未读态立即消失，卡面保留）。
 */
export function hideSessionOptimistically(queryClient: QueryClient, session: Session): void {
  const removeEntirely = !session.unread || isDataDrivenGreenCard(session);
  if (removeEntirely) {
    markPendingSession(session, "hidden");
    queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) =>
      prev ? removeFromList(prev, session) : prev
    );
    return;
  }
  markPendingSession(session, "read");
  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    return {
      ...prev,
      sessions: prev.sessions.map((s) => (sameSession(s, session) ? { ...s, unread: false } : s)),
    };
  });
}

/**
 * 批量乐观隐藏（一键清除）：一次 `setQueryData` 移除全部目标卡，
 * 并统一登记 pending=hidden（轮询回写不复活）。
 */
export function hideSessionsOptimistically(queryClient: QueryClient, targets: SessionKey[]): void {
  if (targets.length === 0) return;
  for (const target of targets) markPendingSession(target, "hidden");
  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    const keys = new Set(targets.map(sessionKey));
    const removed = prev.sessions.filter((s) => keys.has(sessionKey(s)));
    if (removed.length === 0) return prev;
    const sessions = prev.sessions.filter((s) => !keys.has(sessionKey(s)));
    const removedWaiting = removed.filter((s) => s.status === "waiting").length;
    return {
      ...prev,
      sessions,
      totalCount: sessions.length,
      waitingCount: Math.max(0, prev.waitingCount - removedWaiting),
    };
  });
}
