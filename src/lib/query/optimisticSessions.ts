// F5 用户感知口径（2026-10-31 终稿 09b95a3c + 定案 2026-10-31 §12）：
//  - 普通未读卡标已读：只要求**未读态** ≤1s 消失，卡面保留；
//  - 数据驱动聚合绿卡（codex/zcode/dsh 的 App 形态 idle/finished）：后端「已读即剔除」，
//    前端按同一规则**整卡**乐观移除；
//  - 隐藏/归档：整卡 ≤1s 消失。
//
// 单卡与批量共用同一套生命周期：
//  `getSessionDismissBehavior` 分类 → `applyOptimisticSessionMutation` 一次改写缓存 +
//  登记 pending → 命令结算 `settleSessionMutation`（applied=false 走 epoch 门控）。
//
// 三层保障：
//  1) `setQueryData` 立即改写 React Query 缓存（useSessions 同步到 store → 卡面立即变化）；
//  2) 模块级 pending 集合（useSyncExternalStore 订阅）：命令等在飞期间，轮询/旧快照回写
//     不得让卡片/未读态复活；pending 带**变更票根归属**，旧命令结算不会清掉新点击的 pending；
//  3) applied=false：以单调 `scan_epoch` 为「已反映」凭据——变更命令返回请求时刻的 epoch，
//     前端保留 pending，直到收到 `epoch > request_epoch` 的 `scan-bus-snapshot` 事件；
//     有界 10s 超时强制收口并 invalidate（回滚可见）；命令抛错立即 invalidate + 清 pending。
import type { QueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
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

// ---- 统一生命周期分类（单卡/批量唯一事实源）----

/**
 * 卡片关闭行为（与后端 `apply_dismissals` 的未读池权威分流 + 数据驱动绿卡剔除同口径）：
 * - `markReadAndRemove`：未读 + 数据驱动聚合绿卡 → 标已读，后端已读即剔除 → 整卡移除；
 * - `markReadKeep`：普通未读卡 → 只标已读，卡面保留；
 * - `dismiss`：其余（已读卡/非数据驱动卡）→ 整卡移除。
 *
 * 注意：**不能只看 status**——是否数据驱动聚合卡由工具 + form + status 三者共同决定。
 */
export type SessionDismissBehavior = "markReadAndRemove" | "markReadKeep" | "dismiss";

export function getSessionDismissBehavior(session: Session): SessionDismissBehavior {
  if (!session.unread) return "dismiss";
  if (isDataDrivenGreenCard(session)) return "markReadAndRemove";
  return "markReadKeep";
}

// ---- pending 集合（模块级，跨实例共享）----
export type PendingMode = "hidden" | "read";

interface PendingEntry {
  mode: PendingMode;
  /** 设置该 pending 的变更票根：只有票主结算能清掉它（防旧命令清掉新点击） */
  owner: symbol;
}

const pending = new Map<string, PendingEntry>();
let pendingVersion = 0;
const listeners = new Set<() => void>();
/** 同一会话重复点击时，新票根的序号更大；旧命令的等待器/结算据此让位 */
let mutationSeq = 0;

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

/** 一次用户变更的票根：批量共用同一 owner；seq 处理命令乱序结算 */
export interface PendingMutationTicket {
  owner: symbol;
  seq: number;
  targets: SessionKey[];
}

function makeTicket(targets: SessionKey[]): PendingMutationTicket {
  mutationSeq += 1;
  return { owner: Symbol("session-mutation"), seq: mutationSeq, targets };
}

/** 写入单条 pending（不 emit；调用方负责合并通知） */
function setPendingEntry(target: SessionKey, mode: PendingMode, owner: symbol): boolean {
  const key = sessionKey(target);
  const prev = pending.get(key);
  if (prev?.mode === mode && prev.owner === owner) return false;
  pending.set(key, { mode, owner });
  return true;
}

/**
 * 登记单条 pending（测试/卡片复用入口）。返回本次票根：
 * 同一会话重复点击会取消旧的 epoch 等待器（新票根接管）。
 */
export function markPendingSession(target: SessionKey, mode: PendingMode): PendingMutationTicket {
  const ticket = makeTicket([{ id: target.id, agentType: target.agentType }]);
  cancelSupersededWaiters(ticket);
  if (setPendingEntry(target, mode, ticket.owner)) emitPending();
  return ticket;
}

/** 清空 pending（测试/显式回滚入口）：无归属限制，并取消相关等待器 */
export function clearPendingSessions(targets: SessionKey[]): void {
  let changed = false;
  for (const target of targets) {
    if (pending.delete(sessionKey(target))) changed = true;
  }
  cancelWaitersForTargets(targets);
  if (changed) emitPending();
}

/** 只清「本次票根设置」的 pending（旧命令结算不得清掉新点击的 pending） */
function clearPendingForTicket(ticket: PendingMutationTicket): void {
  let changed = false;
  for (const target of ticket.targets) {
    const key = sessionKey(target);
    const entry = pending.get(key);
    if (entry?.owner !== ticket.owner) continue;
    pending.delete(key);
    changed = true;
  }
  if (changed) emitPending();
}

/** home 渲染前的派生：hidden 整卡剔除；read 强制 unread=false（防轮询回写） */
export function applyPendingSessions(sessions: Session[]): Session[] {
  if (pending.size === 0) return sessions;
  const result: Session[] = [];
  for (const s of sessions) {
    const entry = pending.get(sessionKey(s));
    if (entry?.mode === "hidden") continue;
    result.push(entry?.mode === "read" ? { ...s, unread: false } : s);
  }
  return result;
}

// ---- 缓存乐观更新（单卡/批量共用）----

/**
 * 统一乐观变更（单卡传 1 项、批量传 N 项）：
 * - 按 `getSessionDismissBehavior` 分类，**一次** `setQueryData` 写缓存；
 * - pending 与分类一一对应（read 保留卡面、hidden 整卡拦截旧快照回写）；
 * - 计数只按**实际移除数**扣减（markReadKeep 不动 totalCount/waitingCount）。
 *
 * 返回票根供 `settleSessionMutation` 使用；目标为空时返回 null（无操作）。
 */
export function applyOptimisticSessionMutation(
  queryClient: QueryClient,
  sessions: Session[]
): PendingMutationTicket | null {
  if (sessions.length === 0) return null;
  const ticket = makeTicket(sessions.map((s) => ({ id: s.id, agentType: s.agentType })));
  cancelSupersededWaiters(ticket);

  const behaviors = new Map<string, SessionDismissBehavior>();
  let pendingChanged = false;
  for (const s of sessions) {
    const behavior = getSessionDismissBehavior(s);
    behaviors.set(sessionKey(s), behavior);
    const mode: PendingMode = behavior === "markReadKeep" ? "read" : "hidden";
    if (setPendingEntry(s, mode, ticket.owner)) pendingChanged = true;
  }
  if (pendingChanged) emitPending();

  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    let changed = false;
    let removed = 0;
    let removedWaiting = 0;
    const next: Session[] = [];
    for (const s of prev.sessions) {
      const behavior = behaviors.get(sessionKey(s));
      if (!behavior) {
        next.push(s);
        continue;
      }
      if (behavior === "markReadKeep") {
        if (s.unread) {
          next.push({ ...s, unread: false });
          changed = true;
        } else {
          next.push(s);
        }
        continue;
      }
      changed = true;
      removed += 1;
      if (s.status === "waiting") removedWaiting += 1;
    }
    if (!changed) return prev;
    return {
      ...prev,
      sessions: next,
      totalCount: Math.max(0, prev.totalCount - removed),
      waitingCount: Math.max(0, prev.waitingCount - removedWaiting),
    };
  });
  return ticket;
}

// ---- 命令结算：applied=false 的 epoch 门控 ----

/** 与 Rust `MutationOutcome` 对应（epoch = 变更请求时刻的扫描纪元） */
export interface SessionMutationOutcome {
  applied: boolean;
  version: number;
  epoch: number;
}

/** applied=false 等待「已反映快照」的上限：超时强制收口并 invalidate（回滚可见） */
export const SESSION_SETTLE_TIMEOUT_MS = 10_000;

interface EpochWaiter {
  ticket: PendingMutationTicket;
  queryClient: QueryClient;
  /** 请求时刻的扫描纪元：只有 `epoch >` 它的快照事件才证明变更已反映 */
  requestEpoch: number;
  timer: ReturnType<typeof setTimeout>;
}

const epochWaiters = new Set<EpochWaiter>();
let unlistenSnapshot: (() => void) | null = null;
let snapshotListenPending = false;
/**
 * 本窗口**已写入缓存**的最大快照 epoch（`useScanSnapshotSync` 应用事件时上报）。
 * 补扫超时路径会在命令返回前就排入一次 Action 扫描，事件可能先于等待器注册到达；
 * 结算时据此立即收口，不必空等 10s 超时（缓存此时已由同步通道写好）。
 */
let latestAppliedSnapshotEpoch = 0;

/** 记录本窗口已应用的快照 epoch（由 `useScanSnapshotSync` 在写缓存后调用） */
export function noteAppliedSnapshotEpoch(epoch: number): void {
  if (typeof epoch !== "number" || !Number.isFinite(epoch)) return;
  if (epoch > latestAppliedSnapshotEpoch) latestAppliedSnapshotEpoch = epoch;
}

function isSessionsResponse(v: unknown): v is SessionsResponse {
  if (typeof v !== "object" || v === null) return false;
  const r = v as Partial<SessionsResponse>;
  return (
    Array.isArray(r.sessions) &&
    typeof r.totalCount === "number" &&
    typeof r.waitingCount === "number"
  );
}

function targetKeysOf(targets: SessionKey[]): Set<string> {
  return new Set(targets.map(sessionKey));
}

function overlaps(a: SessionKey[], b: SessionKey[]): boolean {
  const keys = targetKeysOf(b);
  return a.some((t) => keys.has(sessionKey(t)));
}

/** 取消被新票根取代的等待器（seq 更小且目标重叠） */
function cancelSupersededWaiters(ticket: PendingMutationTicket): void {
  if (epochWaiters.size === 0) return;
  let changed = false;
  for (const waiter of [...epochWaiters]) {
    if (waiter.ticket.seq >= ticket.seq) continue;
    if (!overlaps(waiter.ticket.targets, ticket.targets)) continue;
    epochWaiters.delete(waiter);
    clearTimeout(waiter.timer);
    changed = true;
  }
  if (changed) stopEpochListenerIfIdle();
}

function cancelWaitersForTargets(targets: SessionKey[]): void {
  if (epochWaiters.size === 0) return;
  let changed = false;
  for (const waiter of [...epochWaiters]) {
    if (!overlaps(waiter.ticket.targets, targets)) continue;
    epochWaiters.delete(waiter);
    clearTimeout(waiter.timer);
    changed = true;
  }
  if (changed) stopEpochListenerIfIdle();
}

/**
 * 结算一组命中 epoch 事件的等待器：**同一事件多张卡只 invalidate 一次**；
 * 快照载荷（事件自带完整 SessionsResponse）先写缓存，避免 invalidate 回取前的一帧复活。
 */
function settleWaiters(waiters: EpochWaiter[], snapshotData?: unknown): void {
  const byClient = new Map<QueryClient, PendingMutationTicket[]>();
  for (const waiter of waiters) {
    if (!epochWaiters.delete(waiter)) continue;
    clearTimeout(waiter.timer);
    const tickets = byClient.get(waiter.queryClient) ?? [];
    tickets.push(waiter.ticket);
    byClient.set(waiter.queryClient, tickets);
  }
  if (byClient.size === 0) return;
  stopEpochListenerIfIdle();
  for (const [queryClient, tickets] of byClient) {
    // 先落快照载荷再清 pending：清理与渲染之间不会出现旧缓存的一帧复活；
    // 超时路径无载荷（snapshotData 不合法）→ 直接强制收口（回滚可见），
    // invalidate 只负责随后回取权威快照，不阻塞收口（不得让 pending 无限滞留）。
    if (isSessionsResponse(snapshotData)) {
      queryClient.setQueryData(SESSIONS_QUERY_KEY, snapshotData);
    }
    for (const ticket of tickets) clearPendingForTicket(ticket);
    void Promise.resolve(queryClient.invalidateQueries({ queryKey: SESSIONS_QUERY_KEY })).catch(
      () => {}
    );
  }
}

function handleSnapshotEpoch(epoch: number, snapshotData?: unknown): void {
  const ready: EpochWaiter[] = [];
  for (const waiter of epochWaiters) {
    if (epoch > waiter.requestEpoch) ready.push(waiter);
  }
  if (ready.length > 0) settleWaiters(ready, snapshotData);
}

function stopEpochListenerIfIdle(): void {
  if (epochWaiters.size > 0) return;
  const unlisten = unlistenSnapshot;
  unlistenSnapshot = null;
  if (unlisten) unlisten();
}

/** 共享的 `scan-bus-snapshot` 监听（懒启动、无等待器即解除；多张卡共用一个监听） */
function startEpochListener(): void {
  if (unlistenSnapshot || snapshotListenPending) return;
  snapshotListenPending = true;
  void listen<{ epoch?: unknown; data?: unknown }>("scan-bus-snapshot", (event) => {
    const epoch = event.payload?.epoch;
    if (typeof epoch !== "number" || !Number.isFinite(epoch)) return;
    handleSnapshotEpoch(epoch, event.payload?.data);
  })
    .then((unlisten) => {
      snapshotListenPending = false;
      // 监听落地前等待器已全部结算：立即解除，避免泄漏
      if (epochWaiters.size === 0) {
        unlisten();
        return;
      }
      unlistenSnapshot = unlisten;
    })
    .catch(() => {
      // 非 Tauri 环境/监听失败：仍由 10s 超时兜底收口
      snapshotListenPending = false;
    });
}

/**
 * applied=false 的等待器：收到 `epoch > requestEpoch` 的 `scan-bus-snapshot` →
 * 写缓存 + invalidate + 清 pending；有界超时强制收口。
 *
 * 返回取消函数（重复点击由票根 seq 自动取代；显式取消用于测试/卸载）。
 */
export function awaitReflectedSnapshot(
  queryClient: QueryClient,
  ticket: PendingMutationTicket,
  requestEpoch: number,
  timeoutMs: number = SESSION_SETTLE_TIMEOUT_MS
): () => void {
  // 同一批目标已被更新的变更接管：旧命令晚到的结算不得再注册等待器
  for (const waiter of epochWaiters) {
    if (waiter.ticket.seq > ticket.seq && overlaps(waiter.ticket.targets, ticket.targets)) {
      return () => {};
    }
  }
  cancelSupersededWaiters(ticket);

  const waiter: EpochWaiter = {
    ticket,
    queryClient,
    requestEpoch: Number.isFinite(requestEpoch) ? requestEpoch : 0,
    timer: setTimeout(() => settleWaiters([waiter]), timeoutMs),
  };
  epochWaiters.add(waiter);
  // 事件在等待器注册前已到达（同步通道已写缓存）：立即收口。此处与入队同一
  // 同步块，无异步间隙——检查之后才挂监听不会漏掉中间事件。
  if (latestAppliedSnapshotEpoch > waiter.requestEpoch) {
    settleWaiters([waiter]);
    return () => {};
  }
  startEpochListener();
  return () => {
    if (!epochWaiters.delete(waiter)) return;
    clearTimeout(waiter.timer);
    stopEpochListenerIfIdle();
  };
}

/** 回取权威快照后只清本次票根的 pending（失败也清，避免 pending 永久滞留） */
async function refreshAuthoritative(
  queryClient: QueryClient,
  ticket: PendingMutationTicket
): Promise<void> {
  try {
    await queryClient.invalidateQueries({ queryKey: SESSIONS_QUERY_KEY });
  } catch {
    // 失效失败不阻塞收口：pending 有界，下一轮快照仍会带来权威数据
  }
  clearPendingForTicket(ticket);
}

/**
 * 命令结算（单卡/批量共用）：
 * - 成功（applied=true）或命令抛错（outcome=null）：invalidate 回取权威快照后清 pending；
 * - applied=false：**不清 pending**，等待 epoch 更大的快照事件（10s 超时强制收口）。
 */
export async function settleSessionMutation(
  queryClient: QueryClient,
  ticket: PendingMutationTicket | null,
  outcome: SessionMutationOutcome | null | undefined,
  options: { timeoutMs?: number } = {}
): Promise<void> {
  if (!ticket) return;
  if (!outcome || outcome.applied) {
    await refreshAuthoritative(queryClient, ticket);
    return;
  }
  awaitReflectedSnapshot(queryClient, ticket, outcome.epoch, options.timeoutMs);
}

/** 单测用：取消全部 epoch 等待器并清空快照 epoch 水位（避免用例间泄漏/串味） */
export function __resetSessionMutationWaitersForTest(): void {
  for (const waiter of epochWaiters) clearTimeout(waiter.timer);
  epochWaiters.clear();
  latestAppliedSnapshotEpoch = 0;
  stopEpochListenerIfIdle();
}
