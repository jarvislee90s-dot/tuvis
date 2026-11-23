import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import { ApiError } from "./api";
import {
  confirmOpenCodeActionUnknown,
  fetchNativeOutbox,
  isConclusiveState,
  isDefiniteRefusal,
  nativeErrorCode,
  nativeErrorKey,
  newNativeActionId,
  readOpenCodeAction,
  submitOpenCodeAction,
  verifyOpenCodeActionWithTimeout,
  type ActionView,
  type NativeOperation,
  type OutboxSnapshot,
} from "./opencodeNativeApi";

interface Props {
  sessionId: string;
  bindingToken: string | null;
  readBindingToken?: string | null;
  refreshKey?: string;
  /** 已由权威视图确认的写阻断码：B1 同实例归属门 + 归属未证呈现族（H05 起传导
   *  能力表归属/门族码——路由级码优先，能力表带 `OC_INSTANCE_*` / `OC_GATE_*` 时
   *  同样传入，见 OpenCodeNativePanel 的 viewWriteBlockedCode）。只用于呈现/禁用
   *  本地写控件，**不影响**既有未决动作的只读核对：可能已写出的动作仍按未知保留锚。 */
  writeBlockedCode?: string | null;
  /** I2：服务读取失败 ⇒ 本组件手里的 outbox 快照与绑定令牌都是旧快照的附属，
   *  不是当前可写凭证：**新派发类操作**（rebind_outbox 重新确认发送）一律禁止
   *  （按钮禁用 + execute 层双重闸）。本地撤回不受影响（后端 local_only，
   *  只动兔维斯尚未派发的本地项，零原生写）；既有动作的只读核对保持开放。 */
  readFailed?: boolean;
  onBindingRefusal?: (code: string) => void;
  /** 暂停状态汇报（2026-11-22 用户口径）：暂停不再有独立说明框/按钮，只在发送键上
   *  体现（简短文案 + 图标态）。本组件是 outbox 快照的唯一读取方，因此把权威
   *  `paused` 值上报给面板；未暂停/读不到时恒报 false（不猜）。 */
  onPausedChange?: (paused: boolean) => void;
}
function anchorKey(sessionId: string) {
  return `tuvis.opencodeNative.outboxControl.${sessionId}`;
}
// 绑定只留在内存；重载后未知归属的 action ID 对所有目标保守锁定。
const controlBindings = new Map<string, Map<string, string | null>>();
const volatileControls = new Map<string, Set<string>>();
const observedControls = new Map<string, Set<string>>();
const removedControls = new Map<string, Set<string>>();
const postingControls = new Set<string>();
/** 测试用例边界隔离；生产运行不得清空未决控制。 */
export function resetQueueControlMemoryForTests(): void {
  if (import.meta.env.MODE !== "test") return;
  controlBindings.clear();
  volatileControls.clear();
  observedControls.clear();
  removedControls.clear();
  postingControls.clear();
}
const CONTROL_ANCHOR_CHANGED = "tuvis:opencode-control-anchor-changed";
function notifyControls(sessionId: string): void {
  globalThis.dispatchEvent?.(new CustomEvent(CONTROL_ANCHOR_CHANGED, { detail: sessionId }));
}
function historyKey(sessionId: string) {
  return `${anchorKey(sessionId)}.ids`;
}
function readPersistedIds(sessionId: string): string[] | null {
  try {
    const raw = sessionStorage.getItem(historyKey(sessionId));
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    if (!Array.isArray(parsed)) return null;
    const ids = parsed.filter((id): id is string => typeof id === "string" && id !== "");
    let observed = observedControls.get(sessionId);
    if (!observed) {
      observed = new Set();
      observedControls.set(sessionId, observed);
    }
    for (const id of ids) if (!removedControls.get(sessionId)?.has(id)) observed.add(id);
    const primary = sessionStorage.getItem(anchorKey(sessionId));
    if (primary && !ids.includes(primary)) ids.push(primary);
    if (primary && !removedControls.get(sessionId)?.has(primary)) observed.add(primary);
    return ids;
  } catch {
    return null;
  }
}
function storedIds(sessionId: string): string[] {
  const ids = readPersistedIds(sessionId) ?? [];
  for (const id of observedControls.get(sessionId) ?? []) if (!ids.includes(id)) ids.push(id);
  for (const id of volatileControls.get(sessionId) ?? []) if (!ids.includes(id)) ids.push(id);
  return ids.filter((id) => !removedControls.get(sessionId)?.has(id));
}
function storedAnchor(sessionId: string, binding: string | null): string | null {
  const known = controlBindings.get(sessionId);
  return storedIds(sessionId).find((id) => !known?.has(id) || known.get(id) === binding) ?? null;
}
function addControlAnchor(sessionId: string, id: string, binding: string | null): void {
  const persisted = readPersistedIds(sessionId);
  const ids = storedIds(sessionId);
  if (!ids.includes(id)) ids.push(id);
  let known = controlBindings.get(sessionId);
  if (!known) {
    known = new Map();
    controlBindings.set(sessionId, known);
  }
  known.set(id, binding);
  let observed = observedControls.get(sessionId);
  if (!observed) {
    observed = new Set();
    observedControls.set(sessionId, observed);
  }
  observed.add(id);
  try {
    if (persisted === null) throw new Error("control anchor history unavailable");
    sessionStorage.setItem(historyKey(sessionId), JSON.stringify(ids));
    sessionStorage.setItem(anchorKey(sessionId), id);
  } catch {
    let pending = volatileControls.get(sessionId);
    if (!pending) {
      pending = new Set();
      volatileControls.set(sessionId, pending);
    }
    pending.add(id);
  }
  notifyControls(sessionId);
}
function removeControlAnchor(sessionId: string, id: string): void {
  const persisted = readPersistedIds(sessionId);
  const ids = storedIds(sessionId).filter((item) => item !== id);
  controlBindings.get(sessionId)?.delete(id);
  volatileControls.get(sessionId)?.delete(id);
  observedControls.get(sessionId)?.delete(id);
  let removed = removedControls.get(sessionId);
  if (!removed) {
    removed = new Set();
    removedControls.set(sessionId, removed);
  }
  removed.add(id);
  try {
    if (persisted === null) throw new Error("control anchor history unavailable");
    if (ids.length) sessionStorage.setItem(historyKey(sessionId), JSON.stringify(ids));
    else sessionStorage.removeItem(historyKey(sessionId));
    if (sessionStorage.getItem(anchorKey(sessionId)) === id) {
      if (ids.length) sessionStorage.setItem(anchorKey(sessionId), ids[ids.length - 1]);
      else sessionStorage.removeItem(anchorKey(sessionId));
    }
    removed.delete(id);
    if (!removed.size) removedControls.delete(sessionId);
  } catch {
    /* 读取或写入失败时保留内存删除标记，不覆盖未知历史 */
  }
  notifyControls(sessionId);
}

/** 所有者分列。未知动作只核对。本地暂停态经 `onPausedChange` 上报给面板
 *  （2026-11-22 用户口径：暂停只在发送键上体现，队列区不再有说明框/恢复按钮）。 */
export default function OpenCodeNativeQueue({
  sessionId,
  bindingToken,
  readBindingToken = bindingToken,
  refreshKey,
  writeBlockedCode,
  readFailed = false,
  onBindingRefusal,
  onPausedChange,
}: Props) {
  const [snapshot, setSnapshot] = useState<OutboxSnapshot | null>(null);
  const [error, setError] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [busy, setBusy] = useState(false);
  const [anchor, setAnchor] = useState<string | null>(() =>
    storedAnchor(sessionId, readBindingToken)
  );
  const [historyIncomplete, setHistoryIncomplete] = useState(
    () => readPersistedIds(sessionId) === null
  );
  const syncControlAnchors = useCallback(() => {
    setHistoryIncomplete(readPersistedIds(sessionId) === null);
    setAnchor(storedAnchor(sessionId, readBindingToken));
  }, [sessionId, readBindingToken]);
  /** 最近一次本地控制的受控拒绝文案（i18n 键）；null = 只有通用提示。 */
  const [noticeKey, setNoticeKey] = useState<string | null>(null);
  const busyRef = useRef(false);
  const loadEpoch = useRef(0);
  useLayoutEffect(() => {
    loadEpoch.current += 1;
    setSnapshot(null);
    setLoadError(false);
    syncControlAnchors();
    busyRef.current = false;
    setBusy(false);
    setError(false);
    setNoticeKey(null);
    return () => {
      loadEpoch.current += 1;
    };
  }, [sessionId, readBindingToken, syncControlAnchors]);
  const load = useCallback(
    async (signal?: AbortSignal) => {
      if (!readBindingToken) return;
      const epoch = loadEpoch.current;
      try {
        const value = await fetchNativeOutbox(sessionId, signal, readBindingToken);
        if (!signal?.aborted && loadEpoch.current === epoch && value.sessionId === sessionId) {
          setSnapshot((previous) =>
            previous && previous.revision > value.revision ? previous : value
          );
          setLoadError(false);
        }
      } catch {
        if (!signal?.aborted && loadEpoch.current === epoch) setLoadError(true);
      }
    },
    [sessionId, readBindingToken]
  );
  const settle = useCallback(
    (action: ActionView, actionId: string, epoch: number) => {
      if (isConclusiveState(action.state) || action.state === "queued") {
        removeControlAnchor(sessionId, actionId);
        if (loadEpoch.current !== epoch) return;
        setAnchor(storedAnchor(sessionId, readBindingToken));
        if (action.state === "not_accepted" || action.state === "not_sent") {
          setNoticeKey("mobile.openCodeNative.outbox.controlRejected");
          setError(true);
        }
      }
    },
    [sessionId, readBindingToken]
  );
  useEffect(() => {
    const onChange = (event: Event) => {
      if ((event as CustomEvent<string>).detail === sessionId) syncControlAnchors();
    };
    globalThis.addEventListener?.(CONTROL_ANCHOR_CHANGED, onChange);
    return () => globalThis.removeEventListener?.(CONTROL_ANCHOR_CHANGED, onChange);
  }, [sessionId, syncControlAnchors]);
  useEffect(() => {
    const controller = new AbortController();
    void load(controller.signal);
    const timer = setInterval(() => void load(controller.signal), 2000);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, [load, refreshKey]);
  // 暂停态上报（2026-11-22 用户口径）：本组件是 outbox 快照的唯一读取方；面板据此
  // 在发送键上体现暂停（简短文案），不再有独立说明框。读不到快照时恒报 false（不猜）。
  const pausedNow = snapshot?.paused === true;
  useEffect(() => {
    onPausedChange?.(pausedNow);
  }, [onPausedChange, pausedNow]);
  useEffect(() => {
    const id = anchor;
    if (!id || postingControls.has(id)) return;
    const epoch = loadEpoch.current;
    let current = true;
    void readOpenCodeAction(id)
      .then((action) => {
        if (current) settle(action, id, epoch);
      })
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [anchor, sessionId, readBindingToken, settle]);
  const execute = async (operation: NativeOperation) => {
    // 暂停/撤回只操作兔维斯尚未派发的本地项，不需要服务归属；重绑/恢复派发仍受门。
    const localOnly =
      (operation.kind === "set_outbox_paused" && operation.paused) ||
      operation.kind === "retract_outbox";
    if ((!bindingToken && !localOnly) || busyRef.current || anchor || historyIncomplete) return;
    if (readPersistedIds(sessionId) === null) {
      syncControlAnchors();
      return;
    }
    const currentAnchor = storedAnchor(sessionId, readBindingToken);
    if (currentAnchor !== null || readPersistedIds(sessionId) === null) {
      syncControlAnchors();
      return;
    }
    if (writeBlockedCode != null && !localOnly) return;
    // I2：读取失败期间绑定令牌只是旧快照的附属——新派发类操作（rebind_outbox）
    // 双重禁止（另一道是按钮 disabled）；本地暂停/撤回与只读核对不受影响。
    if (readFailed && !localOnly) return;
    busyRef.current = true;
    setBusy(true);
    setError(false);
    setNoticeKey(null);
    const actionId = newNativeActionId();
    const epoch = loadEpoch.current;
    postingControls.add(actionId);
    addControlAnchor(sessionId, actionId, readBindingToken);
    setAnchor(actionId);
    try {
      const action = await submitOpenCodeAction({
        actionId,
        sessionId,
        bindingToken: (localOnly ? readBindingToken : bindingToken) ?? "",
        operation,
      });
      settle(action, actionId, epoch);
    } catch (error) {
      const code = error instanceof ApiError ? nativeErrorCode(error) : null;
      if (error instanceof ApiError && error.status !== null && isDefiniteRefusal(code)) {
        // 派发前受控拒绝（含 B1 归属门）：后端在 reserve 之前就拒绝 ⇒ 确定没有写出，
        // 本次锚没有可核对的动作，必须清掉，不能显示“结果未知”把控制锁死。
        removeControlAnchor(sessionId, actionId);
        if (loadEpoch.current !== epoch) return;
        setAnchor(storedAnchor(sessionId, readBindingToken));
        setNoticeKey(nativeErrorKey(code) ?? "mobile.openCodeNative.outbox.readFailed");
        if (!localOnly && code) onBindingRefusal?.(code);
      } else {
        if (loadEpoch.current !== epoch) return;
        // 网络异常/传输与响应类 409/未收录码：可能已写出，锚保留（只读核对）。
        setNoticeKey("mobile.openCodeNative.outbox.readFailed");
      }
      setError(true);
    } finally {
      postingControls.delete(actionId);
      if (loadEpoch.current === epoch) {
        busyRef.current = false;
        setBusy(false);
        void load();
      }
    }
  };
  const verify = async (actionId: string, controlsAnchor: boolean) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError(false);
    const epoch = loadEpoch.current;
    try {
      const action = await verifyOpenCodeActionWithTimeout(actionId);
      // 持久发送项从 outbox 刷新，不能顺带清掉另一个本地控制动作的恢复锚。
      if (controlsAnchor) settle(action, actionId, epoch);
    } catch {
      if (loadEpoch.current === epoch) setError(true);
    } finally {
      if (loadEpoch.current === epoch) {
        busyRef.current = false;
        setBusy(false);
        void load();
      }
    }
  };
  /** patch3 D2：死代际 acknowledged/unconfirmed 原生行的「确认终态未知并解锁重选」。
   *  确认框文案如实（结果最终未知；账本保留归档记录；不会自动重发）；后端对仍可
   *  核对的活代际行 fail-closed 拒绝 → 这里如实报错、行保持原状。 */
  const confirmUnknown = async (actionId: string) => {
    if (busyRef.current) return;
    if (!window.confirm(i18n.t("mobile.openCodeNative.confirmUnknownHint"))) return;
    busyRef.current = true;
    setBusy(true);
    setError(false);
    const epoch = loadEpoch.current;
    try {
      const action = await confirmOpenCodeActionUnknown(actionId);
      if (loadEpoch.current !== epoch) return;
      if (action.state !== "unknown_confirmed") setError(true);
    } catch {
      if (loadEpoch.current === epoch) setError(true);
    } finally {
      if (loadEpoch.current === epoch) {
        busyRef.current = false;
        setBusy(false);
        void load();
      }
    }
  };
  // R3-slim（2026-11-22）：正常聊天页**不常驻**任何队列区块。
  //
  // 本组件现在只在两种「用户需要处理」的形态下出现：
  //  ① 有兔维斯尚未派发的待发项（给撤回/重绑；正常排队不再成列表）；
  //  ② 控制动作结果未知、历史读取不完整或读取失败（给只读核对/重试）；
  // 原生「已受理 / 已确认」属正常流转：进入正式记录后只在正文出现一次，这里不再列。
  //
  // 2026-11-22 用户口径（暂停并入发送区）：暂停不再有独立说明框——`pausedTitle` /
  // `scope` / 「恢复本地派发」整块删除；暂停状态只经 `onPausedChange` 上报给面板，
  // 由发送键体现（禁用 + 简短文案）。暂停期间本地待发项仍列在这里（可撤回）。
  const entries = snapshot?.items ?? [];
  /** 需要用户处理的待发项：已暂停期间的全部本地项，或带阻断原因的排队项，或孤儿项。
   *  普通排队（未暂停、无 reasonCode）属正常流转，不渲染任何行（R3-slim）。 */
  const localAttention = entries.filter(
    (item) =>
      item.owner === "tuvis" &&
      (snapshot?.paused === true || Boolean(item.reasonCode) || item.state === "not_sent")
  );
  /** 结果未知的原生发送仍需只读核对；已受理/已确认不再列。 */
  const nativeAttention = entries.filter(
    (item) =>
      item.owner === "native" && (item.state === "unconfirmed" || item.state === "dispatching")
  );
  // `loadError` 只在**本组件自己**能给出解释时才值得建框：`readFailed` 表示会话级读取
  // 失败已由面板统一解释，此时再建一个空外框只会多出一块没有内容的区域（复审 I-2）。
  // 暂停本身不再要求建框（它已在发送键上体现）：已暂停但无任何条目/未决/错误时整块不渲染。
  const needsAttention =
    localAttention.length > 0 ||
    nativeAttention.length > 0 ||
    anchor !== null ||
    historyIncomplete ||
    error ||
    (loadError && !readFailed);
  const showError = anchor === null && (error || (loadError && !readFailed));
  if (!needsAttention) return null;
  const disabledLocal = busy || anchor !== null || historyIncomplete;
  const disabledNative =
    disabledLocal || bindingToken === null || writeBlockedCode != null || readFailed;
  const t = (key: string) => i18n.t(`mobile.openCodeNative.outbox.${key}`);
  return (
    <section
      data-testid="native-outbox"
      className="mb-3 space-y-2 rounded-lg border border-[var(--bd)] p-3 text-xs"
    >
      {/* 暂停/恢复的**唯一**入口：只在本组件已因「有待发项」出现时附一行小字，
          不单独成框、不常驻。暂停语义 = 只拦兔维斯尚未派发的消息。 */}
      {localAttention.length > 0 && (
        <p data-testid="outbox-pause-line" className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-testid="outbox-pause"
            disabled={disabledLocal || snapshot === null || (snapshot.paused && disabledNative)}
            onClick={() =>
              snapshot &&
              void execute({
                kind: "set_outbox_paused",
                paused: !snapshot.paused,
                expectedRevision: snapshot.revision,
              })
            }
            className="underline disabled:opacity-50"
          >
            {snapshot?.paused ? t("resume") : t("pause")}
          </button>
          {snapshot?.paused && <span className="text-[var(--mut)]">{t("pausedInline")}</span>}
        </p>
      )}
      {historyIncomplete && (
        <p role="status" data-testid="outbox-history-incomplete">
          {t("historyIncomplete")}{" "}
          <button type="button" className="underline" onClick={syncControlAnchors}>
            {i18n.t("mobile.openCodeNative.retry")}
          </button>
        </p>
      )}
      {/* 暂停不再有独立说明框/恢复按钮（2026-11-22 用户口径）：它只在发送键上体现；
          暂停期间需要用户处理的本地待发项仍按下方列表呈现（可撤回）。 */}
      {/* 连接原因由会话级提示统一解释；队列仅显示本地控制的结果。 */}
      {showError && <p role="status">{noticeKey ? i18n.t(noticeKey) : t("readFailed")}</p>}
      {anchor && (
        <div data-testid="outbox-unknown">
          <span>{t("unknown")}</span>
          <button
            type="button"
            onClick={() => void verify(anchor, true)}
            disabled={busy}
            className="ml-2 underline"
          >
            {t("verify")}
          </button>
          <button
            type="button"
            data-testid="outbox-abandon-verification"
            disabled={busy}
            onClick={() => {
              removeControlAnchor(sessionId, anchor);
              setAnchor(storedAnchor(sessionId, readBindingToken));
            }}
            className="ml-2 underline"
          >
            {t("abandonVerification")}
          </button>
        </div>
      )}
      {/* 待发项只在「需要用户处理」时出现：已暂停、带阻断原因或孤儿项。正常排队交给
          后端 FIFO 自行推进，不再把每一项画成状态列表（R3-slim）。 */}
      {localAttention.length > 0 && <h3 className="font-medium">{t("pendingTitle")}</h3>}
      {localAttention.map((item) => (
        <div
          key={item.actionId}
          data-testid={`outbox-local-${item.actionId}`}
          className="rounded bg-[var(--cb)] p-2"
        >
          {/* D-N5-2：孤儿待发项（门轮被中止、从未出网）没有冻结载荷，正文不可得——
              摘要留空时以受控文案如实标注「正文未知」，不冒充原文。 */}
          <p className="break-words">{item.textSummary || t("orphanNoText")}</p>
          {item.reasonCode && item.state === "queued" && (
            <p className="text-amber-700 dark:text-amber-300">
              {t(
                item.reasonCode === "OC_BINDING_STALE"
                  ? "needsRebind"
                  : item.reasonCode === "OC_ATTACHMENT_REFUSED"
                    ? "attachmentUnavailable"
                    : "waitingConnection"
              )}
            </p>
          )}
          {/* D-N5-2：排队项与无理由 not_sent 孤儿（从未出网）都可撤回；重绑仍只对
              排队项开放（孤儿没有冻结载荷可重绑）。 */}
          {(item.state === "queued" || (item.state === "not_sent" && !item.reasonCode)) && (
            <div className="mt-1 flex gap-3">
              <button
                type="button"
                data-testid={`outbox-retract-${item.actionId}`}
                disabled={disabledLocal}
                onClick={() =>
                  void execute({ kind: "retract_outbox", targetActionId: item.actionId })
                }
                className="underline disabled:opacity-50"
              >
                {t("retract")}
              </button>
              {item.state === "queued" && (
                <button
                  type="button"
                  data-testid={`outbox-rebind-${item.actionId}`}
                  disabled={disabledNative}
                  onClick={() => {
                    if (
                      window.confirm(
                        i18n.t("mobile.openCodeNative.outbox.confirmRebind", {
                          summary: item.textSummary,
                        })
                      )
                    )
                      void execute({ kind: "rebind_outbox", targetActionId: item.actionId });
                  }}
                  className="underline disabled:opacity-50"
                >
                  {t("rebind")}
                </button>
              )}
              {/* 插队（2026-11-22 用户口径）：不是独立常驻按钮，而是**对队列里这一条**的
                  行内动作。它派发的就是本行（原 actionId / 原正文），依靠 OpenCode 原生
                  steer 自带打断当前回合，所以我们不先调 stop_session。
                  点击前明说会打断——用户必须知道这一步会中断正在跑的回合。 */}
              {item.state === "queued" && (
                <button
                  type="button"
                  data-testid={`outbox-prioritize-${item.actionId}`}
                  disabled={disabledNative}
                  onClick={() => {
                    if (
                      window.confirm(
                        i18n.t("mobile.openCodeNative.outbox.confirmPrioritize", {
                          summary: item.textSummary,
                        })
                      )
                    )
                      void execute({ kind: "prioritize_outbox", targetActionId: item.actionId });
                  }}
                  className="underline disabled:opacity-50"
                >
                  {t("prioritize")}
                </button>
              )}
            </div>
          )}
        </div>
      ))}
      {/* 原生侧只剩**结果未知/仍在派发**需要用户核对的动作；已受理与已确认属正常
          流转，进入正式记录后由正文呈现，这里不再列（R3-slim）。 */}
      {nativeAttention.map((item) => (
        <div
          key={item.actionId}
          data-testid={`outbox-native-${item.actionId}`}
          className="rounded bg-[var(--cb)] p-2"
        >
          <p className="break-words">{item.textSummary}</p>
          <p className="text-[var(--mut)]">{t(`states.${item.state}`)}</p>
          <button
            type="button"
            data-testid={`outbox-verify-${item.actionId}`}
            aria-label={`${t("verify")}: ${item.textSummary}`}
            disabled={busy}
            onClick={() => void verify(item.actionId, false)}
            className="mt-1 underline disabled:opacity-50"
          >
            {t("verify")}
          </button>
          {item.state === "unconfirmed" && (
            <button
              type="button"
              data-testid={`outbox-confirm-unknown-${item.actionId}`}
              aria-label={`${i18n.t("mobile.openCodeNative.confirmUnknown")}: ${item.textSummary}`}
              disabled={busy}
              onClick={() => void confirmUnknown(item.actionId)}
              className="mt-1 ml-2 underline disabled:opacity-50"
            >
              {i18n.t("mobile.openCodeNative.confirmUnknown")}
            </button>
          )}
        </div>
      ))}
    </section>
  );
}
