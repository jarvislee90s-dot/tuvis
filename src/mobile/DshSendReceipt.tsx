import { useCallback, useEffect, useState } from "react";
import i18n from "@/i18n";
import { dshReasonCopy, fetchDshAction, type DshActionReceipt } from "./api";

const FIRST_CHECK_MS = 1_000;
const MAX_CHECK_MS = 5_000;
const CHECK_WINDOW_MS = 30_000;
const SUCCESS_MS = 3_000;
const MAX_TRACKED = 16;

type State = DshActionReceipt["state"];
type Tracked = {
  receipt: DshActionReceipt;
  updatedAtMs?: number;
  unavailable?: boolean;
  awaitingPost?: boolean;
  // null = 旧记录无法证明载荷；undefined = 仅追踪回执，不再用于重试。
  retryDigest?: string | null;
  retryScope?: string;
  draftDigest?: string;
  restored?: boolean;
  hidden?: boolean;
};

function awaiting(state: State): boolean {
  return (
    state === "queued" ||
    state === "reserved" ||
    state === "accepted" ||
    state === "running" ||
    state === "unknown_delivery"
  );
}

function restore(key: string): Tracked[] {
  try {
    const saved: unknown = JSON.parse(sessionStorage.getItem(key) ?? "[]");
    if (!Array.isArray(saved)) return [];
    const unique = new Map<string, Tracked>();
    for (const entry of saved.slice(-MAX_TRACKED)) {
      const id: unknown = entry?.actionId;
      if (typeof id !== "string" || !id.length || id.length > 256) continue;
      unique.set(id, {
        receipt: {
          actionId: id,
          state: entry.confirmed === true ? "succeeded" : "unknown_delivery",
          nativeId: null,
          reasonCode: entry.identityUnverified === true ? "identity_unverified" : null,
        },
        retryDigest:
          entry.retryDigest === false
            ? undefined
            : typeof entry.retryDigest === "string" && /^[0-9a-f]{64}$/.test(entry.retryDigest)
              ? entry.retryDigest
              : null,
        retryScope: typeof entry.retryScope === "string" ? entry.retryScope : undefined,
        draftDigest:
          typeof entry.draftDigest === "string" && /^[0-9a-f]{64}$/.test(entry.draftDigest)
            ? entry.draftDigest
            : undefined,
        restored: true,
        hidden: entry.confirmed === true,
      });
    }
    return [...unique.values()];
  } catch {
    return [];
  }
}

function limitTracked(actions: Tracked[]): Tracked[] {
  const result = [...actions];
  while (result.length > MAX_TRACKED) {
    const removable = result.findIndex((item) => item.retryDigest === undefined);
    if (removable < 0) break;
    result.splice(removable, 1);
  }
  return result;
}

function persist(key: string, actions: Tracked[]): boolean {
  try {
    const pending = actions
      .filter(
        ({ receipt, retryDigest }) =>
          awaiting(receipt.state) || (receipt.state === "succeeded" && retryDigest !== undefined)
      )
      .map(({ receipt, retryDigest, retryScope, draftDigest }) => ({
        actionId: receipt.actionId,
        identityUnverified: receipt.reasonCode === "identity_unverified",
        confirmed: receipt.state === "succeeded",
        retryDigest: retryDigest === undefined ? false : retryDigest,
        retryScope,
        draftDigest: retryDigest !== undefined ? draftDigest : undefined,
      }));
    if (pending.length) sessionStorage.setItem(key, JSON.stringify(pending));
    else sessionStorage.removeItem(key);
    return true;
  } catch {
    return false;
  }
}

export function useDshSendReceipts(sessionId: string, instanceId?: string, scope?: string) {
  const legacyKey = `dsh-send-receipts:${JSON.stringify([instanceId ?? null, sessionId])}`;
  const key =
    scope === undefined
      ? legacyKey
      : `dsh-send-receipts:${JSON.stringify([instanceId ?? null, sessionId, scope])}`;
  const restoredActions = () => {
    const current = restore(key);
    if (key === legacyKey || current.length > 0) return current;
    // 旧格式不能证明归属/载荷；只迁移到首个设备分区供只读核实。
    return restore(legacyKey).filter((item) => item.retryDigest === null);
  };
  const [tracked, setTracked] = useState(() => ({ key, actions: restoredActions() }));
  if (tracked.key !== key) setTracked({ key, actions: restoredActions() });
  const actions = tracked.key === key ? tracked.actions : [];

  useEffect(() => {
    if (tracked.key !== key || !persist(key, tracked.actions) || key === legacyKey) return;
    if (tracked.actions.some((item) => item.retryDigest === null)) persist(legacyKey, []);
  }, [key, legacyKey, tracked]);

  const findRetry = useCallback(
    (
      digest: string,
      draftDigest: string | undefined,
      scope?: string
    ): string | null | undefined => {
      const actions = tracked.key === key ? tracked.actions : restore(key);
      const candidate = actions.find(
        (item) => item.retryScope === scope && item.retryDigest === digest
      );
      if (candidate) return candidate.receipt.actionId;
      // 旧版本只留下查询 ID，不能用空摘要猜测这是某一条新消息。
      const uncertain = actions.some(
        (item) =>
          (item.retryDigest === null && item.retryScope === undefined) ||
          (item.retryScope === scope &&
            item.restored &&
            item.retryDigest !== undefined &&
            draftDigest !== undefined &&
            item.draftDigest === draftDigest)
      );
      return uncertain ? null : undefined;
    },
    [key, tracked]
  );

  const arm = useCallback(
    (actionId: string, retryDigest: string, draftDigest?: string, retryScope?: string) => {
      const receipt: DshActionReceipt = {
        actionId,
        state: "unknown_delivery",
        nativeId: null,
        reasonCode: null,
      };
      const stored = restore(key);
      if (!stored.some((item) => item.receipt.actionId === actionId)) {
        // 即使这一帧还没提交到 DOM，也要在 POST 前把原 ID 交给刷新恢复。
        // 空间不足或保存失败时不丢旧锚换取新写入。
        const next = limitTracked([...stored, { receipt, retryDigest, retryScope, draftDigest }]);
        if (next.length > MAX_TRACKED || !persist(key, next)) return false;
      }
      setTracked((previous) => {
        if (
          previous.key !== key ||
          previous.actions.some((item) => item.receipt.actionId === actionId)
        )
          return previous;
        return {
          key,
          actions: limitTracked([
            ...previous.actions,
            { receipt, awaitingPost: true, retryDigest, retryScope, draftDigest },
          ]),
        };
      });
      return true;
    },
    [key]
  );

  const record = useCallback(
    (receipt: DshActionReceipt) => {
      setTracked((previous) => {
        if (previous.key !== key) return previous;
        const existing = previous.actions.find(
          (item) => item.receipt.actionId === receipt.actionId
        );
        if (
          existing?.receipt.reasonCode === "identity_unverified" &&
          receipt.state === "unknown_delivery"
        )
          return previous;
        const item =
          existing &&
          !awaiting(existing.receipt.state) &&
          receipt.reasonCode !== "identity_unverified"
            ? existing
            : {
                receipt,
                unavailable: awaiting(receipt.state) && existing?.unavailable,
                retryScope: existing?.retryScope,
                draftDigest: existing?.draftDigest,
                restored: existing?.restored,
                retryDigest:
                  receipt.state === "unknown_delivery" || receipt.state === "reserved"
                    ? existing?.retryDigest
                    : undefined,
              };
        return {
          key,
          actions: limitTracked([
            ...previous.actions.filter((entry) => entry.receipt.actionId !== receipt.actionId),
            item,
          ]),
        };
      });
    },
    [key]
  );

  const update = useCallback(
    (actionId: string, transform: (item: Tracked) => Tracked | null) => {
      setTracked((previous) => {
        if (previous.key !== key) return previous;
        const index = previous.actions.findIndex(({ receipt }) => receipt.actionId === actionId);
        if (index === -1) return previous;
        const current = previous.actions[index];
        const updated = transform(current);
        if (updated === current) return previous;
        const actions = previous.actions.slice();
        if (updated) actions[index] = updated;
        else actions.splice(index, 1);
        return { ...previous, actions };
      });
    },
    [key]
  );

  const discardArmed = useCallback(
    (actionId: string) => {
      update(actionId, (item) => (item.awaitingPost ? null : item));
    },
    [update]
  );

  const releaseSettled = useCallback(() => {
    setTracked((previous) => {
      if (previous.key !== key) return previous;
      return {
        ...previous,
        actions: previous.actions.flatMap((item) => {
          if (item.receipt.state !== "succeeded" || item.retryDigest === undefined) return [item];
          return item.hidden ? [] : [{ ...item, retryDigest: undefined }];
        }),
      };
    });
  }, [key]);

  return { key, actions, findRetry, arm, record, update, discardArmed, releaseSettled };
}

function receiptCopy(receipt: DshActionReceipt, unavailable: boolean): string {
  if (
    unavailable ||
    receipt.state === "unknown_delivery" ||
    receipt.reasonCode === "identity_unverified"
  ) {
    return i18n.t("mobile.composer.dshVerifyPending");
  }
  switch (receipt.state) {
    case "queued":
      return i18n.t("mobile.composer.dshQueued");
    case "reserved":
      return i18n.t("mobile.composer.dshReserved");
    case "accepted":
    case "running":
      return i18n.t("mobile.composer.dshAccepted");
    case "succeeded":
      return i18n.t("mobile.composer.dshSucceeded");
    case "rejected":
      return i18n.t("mobile.composer.dshRejected");
    case "cancelled":
      return i18n.t("mobile.composer.dshCancelled");
    case "failed":
      return i18n.t("mobile.composer.dshStateFailed", {
        code: receipt.reasonCode ?? receipt.state,
      });
  }
}

export function DshSendReceipt({
  item,
  sessionId,
  instanceId,
  update,
}: {
  item: Tracked;
  sessionId: string;
  instanceId?: string;
  update: ReturnType<typeof useDshSendReceipts>["update"];
}) {
  const { receipt, unavailable } = item;
  const { actionId, state } = receipt;
  // 旧行不能证明这次重放的载荷身份，单纯查回原账本不会解除该限制。
  const pending = awaiting(state) && receipt.reasonCode !== "identity_unverified";
  const [check, setCheck] = useState(0);

  // 异步循环按动作及目标挂载，账本状态刷新不延长本轮预算。
  useEffect(() => {
    if (!pending) return;
    let controller: AbortController | undefined;
    let next: ReturnType<typeof setTimeout> | undefined;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    const stop = () => {
      clearTimeout(next);
      clearTimeout(deadline);
      controller?.abort();
      controller = undefined;
    };
    const markUnavailable = () =>
      update(actionId, (current) =>
        awaiting(current.receipt.state) ? { ...current, unavailable: true } : current
      );
    const start = () => {
      stop();
      if (document.hidden) return;
      const request = new AbortController();
      controller = request;
      let delay = FIRST_CHECK_MS;
      const poll = async () => {
        try {
          const view = await fetchDshAction(actionId, request.signal);
          if (request.signal.aborted) return;
          if (
            view.receipt.actionId !== actionId ||
            view.target.sessionId !== sessionId ||
            (instanceId && view.target.hostId !== instanceId) ||
            !Number.isFinite(view.updatedAtMs)
          ) {
            markUnavailable();
          } else {
            update(actionId, (current) => {
              if (!awaiting(current.receipt.state) || view.updatedAtMs < (current.updatedAtMs ?? 0))
                return current;
              return {
                receipt: view.receipt,
                updatedAtMs: view.updatedAtMs,
                retryScope: current.retryScope,
                draftDigest: current.draftDigest,
                restored: current.restored,
                retryDigest:
                  awaiting(view.receipt.state) || view.receipt.state === "succeeded"
                    ? current.retryDigest
                    : undefined,
              };
            });
          }
        } catch {
          if (!request.signal.aborted) markUnavailable();
        }
        if (!request.signal.aborted) {
          delay = Math.min(MAX_CHECK_MS, delay * 2);
          next = setTimeout(poll, delay);
        }
      };
      next = setTimeout(poll, delay);
      deadline = setTimeout(() => {
        stop();
        markUnavailable();
      }, CHECK_WINDOW_MS);
    };
    start();
    document.addEventListener("visibilitychange", start);
    return () => {
      document.removeEventListener("visibilitychange", start);
      stop();
    };
  }, [actionId, sessionId, instanceId, pending, check, update]);

  useEffect(() => {
    if (state !== "succeeded") return;
    const timer = setTimeout(
      () =>
        update(actionId, (current) => {
          if (current.receipt.state !== "succeeded") return current;
          return current.retryDigest === undefined ? null : { ...current, hidden: true };
        }),
      SUCCESS_MS
    );
    return () => clearTimeout(timer);
  }, [state, actionId, update]);

  const failed = !unavailable && (state === "failed" || state === "rejected");
  return (
    <div className="mb-2 flex flex-wrap items-center gap-2">
      <span
        role="status"
        data-testid={failed ? "send-receipt-failed" : "send-receipt-dsh"}
        data-state={unavailable ? "unknown_delivery" : state}
        className="rounded-full bg-[var(--cb)]/70 px-2 py-0.5 text-xs text-[var(--tx)]"
      >
        {receiptCopy(receipt, !!unavailable)}
        {state === "unknown_delivery" && receipt.reasonCode && (
          <span className="ml-1 text-[var(--mut)]">{dshReasonCopy(receipt.reasonCode)}</span>
        )}
      </span>
      {pending && (unavailable || state === "unknown_delivery") && (
        <button
          type="button"
          className="text-xs underline"
          onClick={() => setCheck((previous) => previous + 1)}
        >
          {i18n.t("mobile.composer.dshCheckDelivery")}
        </button>
      )}
    </div>
  );
}
