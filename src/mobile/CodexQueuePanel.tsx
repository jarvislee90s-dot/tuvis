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
//   （重投 = 同 actionId 双投风险，绝不自动）；
// - **刷新统一源（R11）**：面板除挂载/换会话拉取外，还接受 `refreshSignal` 外部
//   触发（投影跃迁 / 发送入队回执，由 SessionDetail 汇聚为同一 tick 下发）——
//   否则「空队列时发送 codex_queued」面板仍自隐、SSE 推进后队位不刷新（新入队
//   消息在当前详情页不可见不可撤，违反宪法 F2.2）。同会话密集触发去抖合并为
//   一次重拉；重拉带请求序号 + 会话哨兵，旧会话/过期响应一律不回灌。
import { useCallback, useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  fetchCodexQueue,
  retractCodexAction,
  type CodexActionState,
  type CodexQueueView,
} from "./codexIpcApi";
import { ApiError } from "./api";

/** 外部触发的去抖窗口（毫秒）：同一会话内的密集触发（发送回执 + 投影跃迁几乎
 *  同时到达）合并为一次重拉；拉取本身是会话级短查询，延迟一拍无感 */
const QUEUE_REFRESH_DEBOUNCE_MS = 250;
/** C6：去抖**上限等待**（毫秒）：流式输出每 ~100ms 推进一次 revision，若每次
 *  都重置去抖，入队回执触发的刷新会被无限推后（条目直到输出暂停才可见/可撤）。
 *  自首个未处理触发起超过本上限即立即重拉——合并与及时性两头保住。 */
const QUEUE_REFRESH_MAX_WAIT_MS = 2000;

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

export default function CodexQueuePanel({
  sessionId,
  refreshSignal = 0,
}: {
  sessionId: string;
  /** R11：外部刷新信号（单调递增 tick）。每次递增 → 去抖重拉一次队列视图；
   *  触发源：a) 投影跃迁（SessionDetail）；b) 发送入队回执（MessageComposer）。
   *  c) 面板自身撤回完成不经此信号（内部直接重拉）。 */
  refreshSignal?: number;
}) {
  const [view, setView] = useState<CodexQueueView | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  /** 乱序防护（R11）：请求序号——每次发起 +1，响应落地时不再是最新序号即作废
   *  （先发后至的响应不得覆盖后发请求的结果） */
  const seqRef = useRef(0);
  /** 乱序防护：会话哨兵——响应落地时会话已切换即作废（旧 session 结果不回灌） */
  const sessionRef = useRef(sessionId);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** C6：首个未处理触发的时刻（maxWait 基准；重拉完成即清） */
  const firstPendingAtRef = useRef<number | null>(null);

  const load = useCallback(async () => {
    const sid = sessionId;
    const seq = ++seqRef.current;
    try {
      const v = await fetchCodexQueue(sid);
      // 乱序防护：会话已切换 / 已有更新的请求发出 → 本次响应作废（不回灌旧数据）
      if (sessionRef.current !== sid || seq !== seqRef.current) return;
      if (v !== null) setView(v); // 403（设备失效）静默保留现有视图，全局判废通道处理
    } catch {
      /* 拉取失败静默：保留上一份（面板是增强信息，不阻塞对话） */
    }
  }, [sessionId]);

  // 挂载/换会话拉取；换会话先清（防串台），并作废在途的去抖定时器与旧响应
  useEffect(() => {
    sessionRef.current = sessionId;
    firstPendingAtRef.current = null;
    setView(null);
    setNote(null);
    void load();
    return () => {
      if (debounceRef.current !== null) {
        clearTimeout(debounceRef.current);
        debounceRef.current = null;
      }
    };
  }, [sessionId, load]);

  // R11 + C6：外部触发 → 去抖合并重拉（信号为 0 = 尚无外部触发，不拉）。
  // 去抖带 maxWait 上限：流式 revision 每 ~100ms 推进会不断重置去抖——自首个
  // 未处理触发起超过 QUEUE_REFRESH_MAX_WAIT_MS 即立即重拉（入队条目在明确上限
  // 内可见/可撤，不被流式输出饿死）。
  useEffect(() => {
    if (refreshSignal <= 0) return;
    if (debounceRef.current !== null) clearTimeout(debounceRef.current);
    const now = Date.now();
    if (firstPendingAtRef.current === null) firstPendingAtRef.current = now;
    const elapsed = now - firstPendingAtRef.current;
    const delay = Math.max(
      0,
      Math.min(QUEUE_REFRESH_DEBOUNCE_MS, QUEUE_REFRESH_MAX_WAIT_MS - elapsed)
    );
    debounceRef.current = setTimeout(() => {
      debounceRef.current = null;
      firstPendingAtRef.current = null;
      void load();
    }, delay);
  }, [refreshSignal, load]);

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
          data-reason={view.blockedReasonCode}
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {view.blockedReasonCode === "native_turn_pending"
            ? i18n.t("mobile.codex.queue.blockedNativeTurn")
            : i18n.t("mobile.codex.queue.blockedCompat")}
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
