// OpenCode 原生动作的**恢复锚**（G3）：answer / decide 提交一开始就落一条本机会话锚，
// 让「结果未确认」的动作在卡片卸载、页面刷新、换会话再回来之后仍有一个只读核对入口。
//
// 纪律（与 `OpenCodeNativePanel` / `OpenCodeQuestionGroupCard` 的注释同一份）：
// - **只存 id**：`{ actionId, kind, targetId, at }`。答案正文、题干、选项、上下文令牌
//   （`context_token`）、`bindingToken` 一律不得落 storage——恢复只需要能重新读一次动作账本；
// - **按会话分组**：键 = 前缀 + `sessionId`，因此换会话永远读不到另一会话的锚；
// - **有上限**：最多 {@link MAX_ACTION_ANCHORS} 条（保留最新；超出的旧锚由用户显式放弃或
//   账本核对收敛，不无限堆叠）；
// - **永不是正确性依赖**：sessionStorage 在隐私模式/配额/损坏时不可用，读写失败只会退化成
//   「刷新后没有恢复入口」，绝不能让已经发出的动作变成可重发的入口。
//
// 本模块不 fetch、不 React：只做安全读写 + 一条变更事件（供面板在同一页内即时刷新区块）。
// 「动作是否已收敛」的判断属于调用方（`isConclusiveState`）——本模块不猜状态、不删错锚。
/** 锚种类：整组作答 / 审批决定 / 会话停止 / 模式切换 / 撤回待投递项 / 已受理发送。
 *  `send`（N4）只承载**已证受理**的发送（精确 inbox id 命中的 `acknowledged`）：
 *  结果未知的发送仍走单一发送锚（`tuvis.opencodeNative.action.<sessionId>`），
 *  两者语义不同——`send` 锚不锁新意图，只保留只读核对入口。 */
export type OpenCodeActionAnchorKind =
  "answer" | "decide" | "stop" | "set_agent" | "retract" | "send";

/** 一条恢复锚：全部字段都是 id 或时间戳，不含任何正文。 */
export interface OpenCodeActionAnchor {
  actionId: string;
  kind: OpenCodeActionAnchorKind;
  targetId: string;
  at: number;
}

/** 单会话最多保留的锚条数（超出时淘汰最旧的）。 */
export const MAX_ACTION_ANCHORS = 20;

/** 自动重读的最小锚龄：只用于省掉一次「必然 pre-POST 404」的读，**从不构成结论**。
 *
 *  写锚发生在 `POST /opencode/actions` **之前**；在那之后的极短窗口里，账本行可能还没
 *  建立，`GET /actions/{id}` 会短暂 404。自动重读因此跳过更年轻的锚；但 404 在任何
 *  锚龄下都只表示「当前没查到」——锚、锁与 Unknown 一律保留，只有可信的
 *  `not_sent/not_accepted` 结论、账本已定案或用户明确放弃才清锚。 */
export const ANCHOR_AUTO_REREAD_MS = 20_000;

/** 同页内的变更事件（`detail.sessionId`）：面板据此即时重读，不依赖 pending 卡片生命周期。 */
export const ACTION_ANCHOR_CHANGED_EVENT = "tuvis:opencode-action-anchor-changed";

// 绑定只留在进程内；从存储恢复的锚归属未知，必须保守阻止相同操作。
const anchorBindings = new Map<string, string>();
const pendingInMemory = new Map<string, Map<string, OpenCodeActionAnchor>>();

/** 仅用于测试用例之间隔离模块状态，生产运行不可清除未决锁。 */
export function resetActionAnchorMemoryForTests(): void {
  if (import.meta.env.MODE !== "test") return;
  anchorBindings.clear();
  pendingInMemory.clear();
}

export function rememberActionBinding(actionId: string, binding: string): void {
  anchorBindings.set(actionId, binding);
}

export function hasUnresolvedAction(
  sessionId: string,
  kind: OpenCodeActionAnchorKind,
  targetId: string,
  binding: string
): boolean {
  return [
    ...(pendingInMemory.get(sessionId)?.values() ?? []),
    ...readActionAnchors(sessionId),
  ].some(
    (anchor) =>
      anchor.kind === kind &&
      (kind === "set_agent" || anchor.targetId === targetId) &&
      (anchorBindings.get(anchor.actionId) ?? binding) === binding
  );
}

/** 该会话的 storage 键（按会话隔离；键本身不含任何凭据）。 */
export function actionAnchorStorageKey(sessionId: string): string {
  return `tuvis.opencodeNative.actionAnchors.${sessionId}`;
}

/** sessionStorage 的安全句柄（不可用/被禁用 → null）。 */
function safeStorage(): Storage | null {
  try {
    return globalThis.sessionStorage ?? null;
  } catch {
    return null;
  }
}

/** 严格形状校验：坏条目丢弃，绝不把可疑对象当成锚继续传播。 */
function normalizeAnchor(raw: unknown): OpenCodeActionAnchor | null {
  if (raw === null || typeof raw !== "object") return null;
  const { actionId, kind, targetId, at } = raw as {
    actionId?: unknown;
    kind?: unknown;
    targetId?: unknown;
    at?: unknown;
  };
  if (typeof actionId !== "string" || actionId === "") return null;
  if (
    kind !== "answer" &&
    kind !== "decide" &&
    kind !== "stop" &&
    kind !== "set_agent" &&
    kind !== "retract" &&
    kind !== "send"
  ) {
    return null;
  }
  if (typeof targetId !== "string" || targetId === "") return null;
  if (typeof at !== "number" || !Number.isFinite(at)) return null;
  return { actionId, kind, targetId, at };
}

/** 读取本会话的锚（损坏/不可用/非数组一律按空处理；同一 actionId 只留第一条）。 */
export function readActionAnchors(sessionId: string): OpenCodeActionAnchor[] {
  const merged = new Map<string, OpenCodeActionAnchor>();
  try {
    const raw = safeStorage()?.getItem(actionAnchorStorageKey(sessionId));
    const parsed: unknown = raw == null ? [] : JSON.parse(raw);
    if (Array.isArray(parsed)) {
      for (const item of parsed) {
        const anchor = normalizeAnchor(item);
        if (anchor && !merged.has(anchor.actionId)) merged.set(anchor.actionId, anchor);
      }
    }
  } catch {
    // 存储损坏或不可用时仍展示内存中的恢复锚。
  }
  for (const anchor of pendingInMemory.get(sessionId)?.values() ?? []) {
    merged.set(anchor.actionId, anchor);
  }
  return [...merged.values()].sort((a, b) => a.at - b.at).slice(-MAX_ACTION_ANCHORS);
}

/** 广播变更（无可用的 window 时静默：模块在纯 node 测试里也能安全调用）。 */
function emitChanged(sessionId: string): void {
  try {
    const target = globalThis as unknown as {
      dispatchEvent?: (event: Event) => boolean;
      CustomEvent?: typeof CustomEvent;
    };
    if (typeof target.dispatchEvent !== "function") return;
    if (typeof target.CustomEvent !== "function") return;
    target.dispatchEvent(
      new target.CustomEvent(ACTION_ANCHOR_CHANGED_EVENT, { detail: { sessionId } })
    );
  } catch {
    /* 事件只是 UI 加速器：派发失败不影响锚本身 */
  }
}

/** 写入/覆盖一条锚（同 actionId 幂等；列表封顶 {@link MAX_ACTION_ANCHORS}）。 */
export function addActionAnchor(
  sessionId: string,
  anchor: { actionId: string; kind: OpenCodeActionAnchorKind; targetId: string },
  now: number = Date.now()
): void {
  if (sessionId === "" || anchor.actionId === "" || anchor.targetId === "") return;
  let pending = pendingInMemory.get(sessionId);
  if (!pending) {
    pending = new Map();
    pendingInMemory.set(sessionId, pending);
  }
  pending.set(anchor.actionId, { ...anchor, at: now });
  const storage = safeStorage();
  if (storage === null) {
    emitChanged(sessionId);
    return;
  }
  try {
    const next = readActionAnchors(sessionId).filter((item) => item.actionId !== anchor.actionId);
    next.push({
      actionId: anchor.actionId,
      kind: anchor.kind,
      targetId: anchor.targetId,
      at: now,
    });
    const capped = next.sort((a, b) => a.at - b.at).slice(-MAX_ACTION_ANCHORS);
    storage.setItem(actionAnchorStorageKey(sessionId), JSON.stringify(capped));
    emitChanged(sessionId);
  } catch {
    // 持久化失败时仍保留本页内存锁。
    emitChanged(sessionId);
  }
}

/** 删除一条锚（收敛/账本无此动作/用户显式放弃时调用；不存在则零变更、不发事件）。 */
export function removeActionAnchor(sessionId: string, actionId: string): void {
  const current = readActionAnchors(sessionId);
  anchorBindings.delete(actionId);
  const removedMemory = pendingInMemory.get(sessionId)?.delete(actionId) ?? false;
  if (pendingInMemory.get(sessionId)?.size === 0) pendingInMemory.delete(sessionId);
  const next = current.filter((item) => item.actionId !== actionId);
  const changed = removedMemory || next.length !== current.length;
  if (!changed) return;
  try {
    const storage = safeStorage();
    const key = actionAnchorStorageKey(sessionId);
    if (next.length === 0) storage?.removeItem(key);
    else storage?.setItem(key, JSON.stringify(next));
  } catch {
    // 存储失败不阻止通知内存状态的变化。
  }
  emitChanged(sessionId);
}

// ============================================================
// J2（无本地锚发现）：服务端动作清单的 kind 映射与去重合并——本地锚优先，
// 服务端补充本地缺失项（清空本地锚/新标签页后找回核对入口）。纯函数，不 fetch。
// ============================================================

/** 服务端动作行的 operation（NativeOperation snake_case）→ 锚 kind；无对应形态
 *  （`set_outbox_paused`：暂停不是恢复锚语义）⇒ null（合并时跳过该行，如实申报
 *  不进已受理区）。 */
export function serverOperationToAnchorKind(operation: string): OpenCodeActionAnchorKind | null {
  switch (operation) {
    case "send":
      return "send";
    case "answer":
      return "answer";
    case "decide":
      return "decide";
    case "stop_session":
      return "stop";
    case "set_agent":
      return "set_agent";
    case "retract":
    case "retract_outbox":
    case "rebind_outbox":
      return "retract";
    default:
      return null;
  }
}

/** 服务端动作清单 × 本地锚按 action ID 去重合并：本地锚优先（targetId/绑定语义
 *  更全），服务端补充本地缺失项。无法映射 kind 的行跳过；`at: 0` 使服务端补充行
 *  排在区块顶部（发现性优先）。上限沿用 {@link MAX_ACTION_ANCHORS}（超出截断，
 *  服务端清单仍可按行核对）。 */
export interface OpenCodeRecoveryRow extends OpenCodeActionAnchor {
  ledger?: { state: string; nativeId?: string };
}

export function mergeServerActionAnchors(
  local: OpenCodeActionAnchor[],
  server: ReadonlyArray<{
    actionId: string;
    operation: string;
    state?: string;
    nativeId?: string;
  }>
): OpenCodeRecoveryRow[] {
  const discovered = new Map(server.map((row) => [row.actionId, row]));
  const merged: OpenCodeRecoveryRow[] = local.map((anchor) => {
    const row = discovered.get(anchor.actionId);
    discovered.delete(anchor.actionId);
    return row?.state === undefined
      ? anchor
      : { ...anchor, ledger: { state: row.state, nativeId: row.nativeId } };
  });
  for (const row of discovered.values()) {
    const kind = serverOperationToAnchorKind(row.operation);
    if (kind === null) continue;
    const anchor = { actionId: row.actionId, kind, targetId: "", at: 0 };
    merged.push(
      row.state === undefined
        ? anchor
        : { ...anchor, ledger: { state: row.state, nativeId: row.nativeId } }
    );
  }
  return merged.slice(-MAX_ACTION_ANCHORS);
}
