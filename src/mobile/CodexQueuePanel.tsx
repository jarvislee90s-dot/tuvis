// Codex 待发队列面板（Task 9）：本设备在当前 Thread 的动作账本视图。
//
// - 显示本设备待发与**实际 FIFO 位置**（position 来自服务端 codex_actions 的
//   (thread_id,state,seq) 排序——跨设备真队列序，不是本端发送顺序）；
// - 撤回：仅 queued 可撤（queued→cancelled CAS）。成功 → 操作后重拉（权威列表
//   收敛，条目消失）；409（已派发）→ 条目保留 + 权威刷新 + 说明——绝不谎报
//   撤回成功；404（不存在/他人动作）→ 重拉对账；
// - blockedReasonCode 非空（如 compat_outstanding）= 队列暂停派发：说明条可见，
//  不让用户盲等（types.rs CodexQueueView 的字段注释即本条契约）；
// - **不提供插队**：本面板没有「立即发送/跳到队首」语义（宪法 R2.3 忙态留队、
//   FIFO 按序，任何插队入口都是越权）——cancelled/终态条目也不出现重试按钮
//   （重投 = 同 actionId 双投风险，绝不自动）。
import { useCallback, useEffect, useState } from "react";
import i18n from "@/i18n";
import {
  fetchCodexQueue,
  retractCodexAction,
  type CodexActionState,
  type CodexQueueView,
} from "./codexIpcApi";
import { ApiError } from "./api";

/** 动作状态 → 展示词（snake_case wire 词 → 用户可读；unknown 如实「结果未知」） */
function stateLabel(s: CodexActionState): string {
  switch (s) {
    case "queued":
      return i18n.t("mobile.codex.queue.stateQueued");
    case "reserved":
      return i18n.t("mobile.codex.queue.stateReserved");
    case "accepted":
      return i18n.t("mobile.codex.queue.stateAccepted");
    case "rejected":
      return i18n.t("mobile.codex.queue.stateRejected");
    case "unknown":
      return i18n.t("mobile.codex.queue.stateUnknown");
    case "cancelled":
      return i18n.t("mobile.codex.queue.stateCancelled");
  }
}

export default function CodexQueuePanel({ sessionId }: { sessionId: string }) {
  const [view, setView] = useState<CodexQueueView | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const v = await fetchCodexQueue(sessionId);
      if (v !== null) setView(v); // 403（设备失效）静默保留现有视图，全局判废通道处理
    } catch {
      /* 拉取失败静默：保留上一份（面板是增强信息，不阻塞对话） */
    }
  }, [sessionId]);

  // 挂载/换会话拉取；换会话先清（防串台）
  useEffect(() => {
    setView(null);
    setNote(null);
    void load();
  }, [sessionId, load]);

  const retract = useCallback(
    async (actionId: string) => {
      if (busyId !== null) return;
      setBusyId(actionId);
      setNote(null);
      try {
        await retractCodexAction(actionId);
      } catch (e) {
        if (e instanceof ApiError && e.status === 409) {
          // 已派发不可撤：如实说明（条目按下方重拉的权威状态保留）
          setNote(i18n.t("mobile.codex.queue.retractBusy"));
        } else {
          setNote(
            e instanceof ApiError
              ? i18n.t("mobile.codex.queue.retractFailed", { message: e.message })
              : i18n.t("mobile.codex.queue.retractFailed", { message: String(e) })
          );
        }
      } finally {
        setBusyId(null);
        // 操作后重拉（成功/失败都拉）：以服务端账本为唯一权威呈现
        await load();
      }
    },
    [busyId, load]
  );

  if (view === null) return null;
  // 全空且无暂停原因：面板自隐（不渲染空壳）
  if (view.actions.length === 0 && view.blockedReasonCode === null) return null;

  return (
    <div
      data-testid="codex-queue-panel"
      className="rounded-xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span data-testid="codex-queue-title" className="text-xs font-semibold text-[var(--mut)]">
          {i18n.t("mobile.codex.queue.title")}
        </span>
      </div>
      {view.blockedReasonCode !== null && (
        <p
          data-testid="codex-queue-blocked"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {i18n.t("mobile.codex.queue.blockedCompat")}
        </p>
      )}
      <ul className="mt-1 space-y-1">
        {view.actions.map((a) => (
          <li
            key={a.actionId}
            data-testid={`codex-queue-row-${a.actionId}`}
            className="flex flex-wrap items-center gap-1.5 rounded-lg bg-[var(--btnp)]/5 px-2 py-1 text-xs"
          >
            <span
              data-testid={`codex-queue-position-${a.actionId}`}
              className="shrink-0 text-amber-700 dark:text-amber-300"
            >
              {/* queued 且有位次 → 「第N位」（FIFO 真序）；否则按状态词呈现
                  （cancelled/unknown 等终态如实留痕，无重试/插队入口） */}
              {a.state === "queued" && a.position !== null && a.position >= 1
                ? i18n.t("mobile.codex.queue.position", { n: a.position })
                : stateLabel(a.state)}
            </span>
            {/* 撤回：仅 queued 可撤；**没有插队/重试入口**（注释见文件头） */}
            {a.state === "queued" && (
              <button
                type="button"
                data-testid={`codex-queue-retract-${a.actionId}`}
                disabled={busyId !== null}
                onClick={() => {
                  void retract(a.actionId);
                }}
                className="ml-auto rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)] disabled:opacity-40"
              >
                {i18n.t("mobile.codex.queue.retract")}
              </button>
            )}
          </li>
        ))}
      </ul>
      {note !== null && (
        <p
          data-testid="codex-queue-note"
          className="mt-1 text-xs text-slate-600 dark:text-slate-300"
        >
          {note}
        </p>
      )}
    </div>
  );
}
