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
//  3) applied=false：以单调 `scan_epoch` 为「已反映」凭据——变更命令返回**全部写入
//     完成之后**捕获的门槛 epoch，前端保留 pending，直到收到 `epoch > 门槛` 的
//     `scan-bus-snapshot` 事件（复核 R-2：门槛取晚不取早，入口取值会放行「写入之前
//     启动、之后发布」的旧快照，让卡片/未读态当场复活）；
//     有界 10s 超时强制收口并 invalidate（回滚可见）；命令抛错立即 invalidate + 清 pending。
//  4) 缓存写入统一过版本闸（复核 R-3）：事件 / 结算 / 兜底轮询共用
//     `applySessionsSnapshot`，更旧的快照一律不落缓存。
import type { QueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
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

// ---- 会话缓存写入仲裁（复核 R-3）----
//
// 快照写入缓存有三条路：`scan-bus-snapshot` 事件、变更结算（epoch 门控）、
// 兜底轮询的查询结果。三条路必须共用**同一道版本闸**，否则出现「迟到的更旧数据
// 覆盖已应用的新快照」：轮询先取到旧快照 N，事件把 N+1 写好，轮询随后落地 → 缓存
// 回退到 N（React Query 对查询结果落地没有版本概念，事件侧的水位拦不住它）。
//
// 每窗口一份水位（模块作用域 = 单个 WebView 的 JS 上下文）：生产下每个窗口各自
// 一个上下文，天然隔离；单测在同一 jsdom 里模拟主窗/桌宠时按 label 分账，
// 保证「多窗口广播不互相吞事件」这条契约仍可验证。

/**
 * 当前 WebView 的窗口 label（后端按窗口记账的键）。非 Tauri 环境/取不到时回落 "main"，
 * 保证浏览器预览与单测仍有一个稳定源，不抛错。
 */
export function currentScanWindowLabel(): string {
  try {
    const label = getCurrentWebviewWindow().label;
    return typeof label === "string" && label.trim() !== "" ? label : "main";
  } catch {
    return "main";
  }
}

const snapshotVersionByWindow = new Map<string, number>();
const snapshotDataByWindow = new Map<string, SessionsResponse>();
interface QuerySnapshotMetadata {
  version: number;
  epoch: number;
}
let querySnapshotMetadata = new WeakMap<object, QuerySnapshotMetadata>();
let authorizedSnapshotWrites = new WeakMap<object, Set<string>>();

/** 单测用：清空版本水位（避免用例间共享模块状态） */
export function __resetScanSnapshotVersionsForTest(): void {
  snapshotVersionByWindow.clear();
  snapshotDataByWindow.clear();
  querySnapshotMetadata = new WeakMap();
  authorizedSnapshotWrites = new WeakMap();
}

/** 本窗口已应用的最大快照版本（0 = 尚未应用过任何快照） */
export function lastAppliedSnapshotVersion(label = currentScanWindowLabel()): number {
  return snapshotVersionByWindow.get(label) ?? 0;
}

/**
 * 把查询读取到的 version/epoch 绑定到其 data 对象；React Query 在最终 setData 时会把同一对象
 * 传给 structuralSharing 仲裁器，避免在 queryFn 返回前做完检查后又有事件插队。
 */
export function registerFetchedSessionSnapshot(
  data: unknown,
  version: number,
  epoch: number
): SessionsResponse {
  if (!isSessionsResponse(data)) throw new Error("get_sessions_snapshot returned invalid data");
  if (!Number.isFinite(version) || !Number.isFinite(epoch)) {
    throw new Error("get_sessions_snapshot returned invalid version metadata");
  }
  querySnapshotMetadata.set(data, { version, epoch });
  return data;
}

function claimSnapshotVersion(label: string, version: number, data: SessionsResponse): boolean {
  if (version <= lastAppliedSnapshotVersion(label)) return false;
  snapshotVersionByWindow.set(label, version);
  snapshotDataByWindow.set(label, data);
  return true;
}

/**
 * React Query v5 在 Query.setData 的最终缓存提交点调用此 structuralSharing 函数。
 * 有 metadata 的数据来自轮询 command；无 metadata 的数据是乐观更新等本地写入。
 */
export function arbitrateFetchedSessionSnapshot(
  oldData: SessionsResponse | undefined,
  newData: SessionsResponse,
  label = currentScanWindowLabel()
): SessionsResponse {
  const authorized = authorizedSnapshotWrites.get(newData);
  if (authorized?.has(label)) return newData;

  const metadata = querySnapshotMetadata.get(newData);
  if (!metadata) return newData;
  if (!claimSnapshotVersion(label, metadata.version, newData)) {
    return oldData ?? snapshotDataByWindow.get(label) ?? newData;
  }

  // structuralSharing 执行在 Query.setData 的 synchronous commit 内；epoch waiter 结算放到
  // 随后的 microtask，确保 cache 已提交后再清 pending / invalidate。
  queueMicrotask(() => {
    noteAppliedSnapshotEpoch(metadata.epoch, {
      version: metadata.version,
      data: newData,
    });
  });
  return newData;
}

/**
 * 会话缓存的**唯一**按版本写入点：版本必须严格大于该窗口水位才落缓存。
 * 返回 true = 已写入；相同/更旧版本（多窗口广播、乱序到达、迟到的查询结果）忽略。
 *
 * `label` 由调用方在**监听建立时**捕获并传入（生产 = 当前 WebView，单测在同一段
 * JS 里模拟主窗/桌宠时也不会互相吞事件）；缺省取当前窗口。
 */
export function applySessionsSnapshot(
  queryClient: QueryClient,
  version: unknown,
  data: unknown,
  label = currentScanWindowLabel()
): boolean {
  if (typeof version !== "number" || !Number.isFinite(version)) return false;
  if (!isSessionsResponse(data)) return false;
  if (!claimSnapshotVersion(label, version, data)) return false;
  // 先记水位，再给同步 setData 一次性授权；structuralSharing 与 poll 共用同一道 claim 闸。
  let labels = authorizedSnapshotWrites.get(data);
  if (!labels) {
    labels = new Set();
    authorizedSnapshotWrites.set(data, labels);
  }
  labels.add(label);
  try {
    queryClient.setQueryData(SESSIONS_QUERY_KEY, data);
  } finally {
    labels.delete(label);
    if (labels.size === 0) authorizedSnapshotWrites.delete(data);
  }
  return true;
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
 * 同一会话重复点击时，新票根只接管该目标；旧批次的其余目标继续沿用原 epoch 等待器。
 */
export function markPendingSession(target: SessionKey, mode: PendingMode): PendingMutationTicket {
  const ticket = makeTicket([{ id: target.id, agentType: target.agentType }]);
  cancelSupersededWaiters(ticket);
  if (setPendingEntry(target, mode, ticket.owner)) emitPending();
  return ticket;
}

/** 清空 pending（测试/显式回滚入口）：无归属限制，并只从相关等待器移除这些目标 */
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

/**
 * 从当前版本水位对应的权威快照重建 Query 缓存，并叠回仍有效的乐观 pending。
 * 结算票根会在随后清除；同版本轮询可能因仲裁保留已被乐观改写的 oldData，
 * 因此失败/超时不能只 invalidate 后假设 React Query 一定覆盖缓存。
 */
function restoreCurrentAuthoritativeSnapshot(
  queryClient: QueryClient,
  settledOwners: ReadonlySet<symbol>,
  label = currentScanWindowLabel()
): boolean {
  const authoritative = snapshotDataByWindow.get(label);
  if (!authoritative) return false;

  let removed = 0;
  let removedWaiting = 0;
  const sessions: Session[] = [];
  for (const session of authoritative.sessions) {
    const pendingEntry = pending.get(sessionKey(session));
    const entry = pendingEntry && !settledOwners.has(pendingEntry.owner) ? pendingEntry : undefined;
    if (!entry) {
      sessions.push(session);
    } else if (entry.mode === "read") {
      sessions.push(session.unread ? { ...session, unread: false } : session);
    } else {
      removed += 1;
      if (session.status === "waiting") removedWaiting += 1;
    }
  }

  // 始终构造无版本 metadata 的缓存对象；版本水位仍由 authoritative 保持，
  // 后续同版本回取不会让旧数据绕过仲裁，也不会把乐观改写的对象当权威快照。
  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, {
    ...authoritative,
    sessions,
    totalCount: Math.max(0, authoritative.totalCount - removed),
    waitingCount: Math.max(0, authoritative.waitingCount - removedWaiting),
  });
  return true;
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

/**
 * 显式标已读操作：不经过关闭分类、不改变卡面数量，只对目标会话立即清 unread 并登记 read pending。
 * 跳转成功后使用此入口，避免把已读意图误分到 dismiss。
 */
export function applyOptimisticSessionRead(
  queryClient: QueryClient,
  target: SessionKey
): PendingMutationTicket {
  const key = sessionKey(target);
  const ticket = makeTicket([{ id: target.id, agentType: target.agentType }]);
  cancelSupersededWaiters(ticket);
  if (setPendingEntry(target, "read", ticket.owner)) emitPending();

  queryClient.setQueryData<SessionsResponse>(SESSIONS_QUERY_KEY, (prev) => {
    if (!prev) return prev;
    let changed = false;
    const sessions = prev.sessions.map((session) => {
      if (sessionKey(session) !== key || !session.unread) return session;
      changed = true;
      return { ...session, unread: false };
    });
    return changed ? { ...prev, sessions } : prev;
  });
  return ticket;
}

// ---- 命令结算：applied=false 的 epoch 门控 ----

/** 与 Rust `MutationOutcome` 对应（epoch = 变更全部写入完成后捕获的**门槛**纪元） */
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
  /** 门槛纪元（写入完成之后捕获）：只有 `epoch >` 它的快照才证明变更已反映 */
  gateEpoch: number;
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

/**
 * 记录本窗口已应用的快照 epoch。轮询在 structuralSharing 提交后传载荷，
 * 使事件丢失时已有 waiter 也能结算。
 */
export function noteAppliedSnapshotEpoch(
  epoch: number,
  snapshot?: { version?: unknown; data?: unknown }
): void {
  if (typeof epoch !== "number" || !Number.isFinite(epoch)) return;
  if (epoch > latestAppliedSnapshotEpoch) latestAppliedSnapshotEpoch = epoch;
  if (snapshot) handleSnapshotEpoch(epoch, snapshot);
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

function uniqueTargets(targets: SessionKey[]): SessionKey[] {
  const unique = new Map<string, SessionKey>();
  for (const target of targets) unique.set(sessionKey(target), target);
  return [...unique.values()];
}

function retainWaiterTargets(
  waiter: EpochWaiter,
  removed: Set<string>
): "unchanged" | "remaining" | "empty" {
  const remaining = waiter.ticket.targets.filter((target) => !removed.has(sessionKey(target)));
  if (remaining.length === waiter.ticket.targets.length) return "unchanged";
  waiter.ticket = { ...waiter.ticket, targets: remaining };
  return remaining.length > 0 ? "remaining" : "empty";
}

/**
 * 收缩旧 waiter 的目标子集，不重建 waiter/timer；新票根只接管目标交集，
 * 剩余目标沿用旧票根的截止时间。
 */
function cancelSupersededWaiters(ticket: PendingMutationTicket): void {
  if (epochWaiters.size === 0) return;
  const replaced = targetKeysOf(ticket.targets);
  let changed = false;
  for (const waiter of [...epochWaiters]) {
    if (waiter.ticket.seq >= ticket.seq) continue;
    const state = retainWaiterTargets(waiter, replaced);
    if (state === "unchanged") continue;
    changed = true;
    if (state === "empty") {
      epochWaiters.delete(waiter);
      clearTimeout(waiter.timer);
    }
  }
  if (changed) stopEpochListenerIfIdle();
}

function cancelWaitersForTargets(targets: SessionKey[]): void {
  if (epochWaiters.size === 0) return;
  const removed = targetKeysOf(targets);
  let changed = false;
  for (const waiter of [...epochWaiters]) {
    const state = retainWaiterTargets(waiter, removed);
    if (state === "unchanged") continue;
    changed = true;
    if (state === "empty") {
      epochWaiters.delete(waiter);
      clearTimeout(waiter.timer);
    }
  }
  if (changed) stopEpochListenerIfIdle();
}

/**
 * 结算一组命中 epoch 事件的等待器：**同一事件多张卡只 invalidate 一次**；
 * 快照载荷（事件自带完整 SessionsResponse）先写缓存，避免 invalidate 回取前的一帧复活。
 * 写入统一走 [`applySessionsSnapshot`]（复核 R-3）：同步通道已把该版本写好时这里跳过，
 * 更旧版本（乱序到达）也拦得住——结算不得成为第二条「无视版本往缓存里倒数据」的路。
 */
function settleWaiters(waiters: EpochWaiter[], snapshotVersion?: unknown, snapshotData?: unknown) {
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
    // 先落快照载荷，再用该窗口当前已登记的权威快照重建缓存，并重新应用仍有效的
    // 其它 pending；这一步覆盖「版本相同 → structuralSharing 保留乐观 oldData」路径。
    if (typeof snapshotVersion === "number") {
      applySessionsSnapshot(queryClient, snapshotVersion, snapshotData);
    }
    restoreCurrentAuthoritativeSnapshot(
      queryClient,
      new Set(tickets.map((ticket) => ticket.owner))
    );
    for (const ticket of tickets) clearPendingForTicket(ticket);
    // 超时路径仍强制有界收口；invalidate 只负责随后更新权威数据，不阻塞清 pending。
    void Promise.resolve(queryClient.invalidateQueries({ queryKey: SESSIONS_QUERY_KEY })).catch(
      () => {}
    );
  }
}

function handleSnapshotEpoch(
  epoch: number,
  snapshot?: { version?: unknown; data?: unknown }
): void {
  if (epoch > latestAppliedSnapshotEpoch) latestAppliedSnapshotEpoch = epoch;
  const ready: EpochWaiter[] = [];
  for (const waiter of epochWaiters) {
    if (epoch > waiter.gateEpoch) ready.push(waiter);
  }
  if (ready.length > 0) settleWaiters(ready, snapshot?.version, snapshot?.data);
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
  void listen<{ epoch?: unknown; version?: unknown; data?: unknown }>(
    "scan-bus-snapshot",
    (event) => {
      const payload = event.payload;
      const epoch = payload?.epoch;
      if (typeof epoch !== "number" || !Number.isFinite(epoch)) return;
      handleSnapshotEpoch(epoch, { version: payload?.version, data: payload?.data });
    }
  )
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
 * applied=false 的等待器：收到 `epoch > gateEpoch` 的 `scan-bus-snapshot` →
 * 写缓存 + invalidate + 清 pending；有界超时强制收口。
 *
 * 返回取消函数（重复点击由票根 seq 自动取代；显式取消用于测试/卸载）。
 */
export function awaitReflectedSnapshot(
  queryClient: QueryClient,
  ticket: PendingMutationTicket,
  gateEpoch: number,
  timeoutMs: number = SESSION_SETTLE_TIMEOUT_MS
): () => void {
  // 只等待当前仍由这张票根拥有的目标；旧批量命令迟到时，新 owner 的目标不得回到旧 waiter。
  const ownedTargets = uniqueTargets(
    ticket.targets.filter((target) => pending.get(sessionKey(target))?.owner === ticket.owner)
  );
  if (ownedTargets.length === 0) return () => {};

  // 同一张票根的重复/迟到回执不得复制 waiter 或重置原截止时间。
  for (const waiter of epochWaiters) {
    if (waiter.queryClient === queryClient && waiter.ticket.owner === ticket.owner) return () => {};
  }
  cancelSupersededWaiters(ticket);

  const waiter: EpochWaiter = {
    ticket: { ...ticket, targets: ownedTargets },
    queryClient,
    gateEpoch: Number.isFinite(gateEpoch) ? gateEpoch : 0,
    timer: setTimeout(() => settleWaiters([waiter]), timeoutMs),
  };
  epochWaiters.add(waiter);
  // 事件在等待器注册前已到达（同步通道已写缓存）：立即收口。此处与入队同一
  // 同步块，无异步间隙——检查之后才挂监听不会漏掉中间事件。
  if (latestAppliedSnapshotEpoch > waiter.gateEpoch) {
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
  restoreCurrentAuthoritativeSnapshot(queryClient, new Set([ticket.owner]));
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
