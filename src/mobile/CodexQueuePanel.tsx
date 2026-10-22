// Codex 待发队列面板（Task 9）：本设备在当前 Thread 的动作账本视图。
//
// - 显示本设备待发与**实际 FIFO 位置**（position 来自服务端 codex_actions 的
//   (thread_id,state,seq) 排序——跨设备真队列序，不是本端发送顺序）；
// - 撤回：仅 queued 可撤（queued→cancelled CAS）。成功 → 操作后重拉（权威列表
//   收敛，条目消失）；409（已派发）→ 条目保留 + 权威刷新 + 说明——绝不谎报
//   撤回成功；404（不存在/他人动作）→ 重拉对账；
// - blockedReasonCode 非空（如 compat_outstanding）= 队列暂停派发：说明条可见，
//  不让用户盲等（types.rs CodexQueueView 的字段注释即本条契约）；
// - **人工解除无出口屏障（2026-10-19 复审 Important）**：native_delivery_unknown /
//   compat_outstanding 两类无自动恢复路径的屏障，提供「已到桌面端核对，继续队列」
//   按钮——**两步确认**（未确认不发任何请求；文案如实披露重复内容风险，并披露
//   混合屏障面：清单含等待 Turn 完成的条目时将提前解除其顺序限制——行清单不带
//   state，前端无从逐行甄别，只能文案如实）后逐个解除 blockedActionIds 并重拉；
//   单个 id 的确定性「无需解除」回执（409 no_barrier——屏障已被自动路径解除/
//   从未在册；409 barrier_in_flight——屏障在册但派发在飞有自动路径）只收尾该 id
//   不中断剩余；真失败显示错误不静默（note 走 reason →
//   机器码 → message 可读化回退链）。native_turn_pending 有自动恢复路径（Turn
//   终态证据驱动），不提供该按钮；
// - **打断桌面回合（2026-10-21 新增）**：native_turn_pending 的自动恢复路径是
//   「桌面回合自己结束」——回合若一直不结束（长任务/卡死），手机端就完全无解
//   （「解除」只清屏障，消息仍要等桌面空闲才投得出去）。故本面板在该阻塞态
//   额外提供「停止桌面回合并继续」：真发 `thread-follower-interrupt-turn`（破坏性
//   ——**两步确认**，文案如实披露「该回合未完成的工作会丢失」），打断后回合转终态
//   → 屏障释放 → 队列自动接力。仅 canInterrupt=true（该 Desktop build 已活体
//   验证）才渲染按钮（复审 P2-2：无能力 build 不给假出口）；打断成功／结果不确定
//  （指令已发出但未知，不诱导重按）／桌面无可打断回合／失败，四种结果如实分别
//   回报，不谎报不静默（复审 P2-3）。
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
  forceContinueCodexAction,
  interruptCodexTurn,
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

/** ApiError → 可读分诊片段：后端 reason（人类可读，如「该消息仍在排队，请使用
 *  撤回」）优先，其次机器码 error（如 no_barrier / queued_use_retract），最后兜底
 *  e.message——对齐 PendingRequestCard C5 臂的回退链，不把「路径 + 状态码」裸示人 */
function apiErrorReason(e: ApiError): string {
  if (typeof e.data?.reason === "string" && e.data.reason !== "") return e.data.reason;
  if (typeof e.data?.error === "string" && e.data.error !== "") return e.data.error;
  return e.message;
}

/** 该次 force-continue 失败是否「该 id 无需解除」的确定性收尾（2026-10-19 复审
 *  Minor：一败全停修复）——两个确定性「无需人工解除」机器码都收尾该 id 不中断：
 *  409 no_barrier = 屏障已被自动路径解除/从未在册（典型：视图与确认之间屏障被
 *  自动路径清掉）；409 barrier_in_flight = 屏障在册但该 id 派发在飞（有自动落账
 *  路径，天然不需要人工解除）。两者剩余未解除 id 都与该 id 无关，循环继续逐个
 *  解除；仅真失败（网络异常 / 5xx / 403 门禁等）才中断（此时剩余屏障状态不可知，
 *  重拉后由用户再试）。200（含 AlreadyReleased 幂等回执，HTTP 层与普通解除不可分）
 *  天然继续。409 queued_use_retract 不在此列：它指引「走撤回」，按真失败中断并由
 *  note 可读化转述（且屏障在册清单不含 queued 行，本面板实际到不了该臂）。 */
function isPerIdSettledRefusal(e: unknown): boolean {
  return (
    e instanceof ApiError &&
    e.status === 409 &&
    (e.data?.error === "no_barrier" || e.data?.error === "barrier_in_flight")
  );
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
  /** 人工解除的二次确认臂（未确认不发任何请求——与 ModeBarCli 同款前端防线） */
  const [forceArmed, setForceArmed] = useState(false);
  /** 人工解除进行中（逐个解除 blockedActionIds 期间禁用全部按钮） */
  const [forceBusy, setForceBusy] = useState(false);
  /** 打断桌面回合的二次确认臂（未确认不发任何请求——破坏性动作的前端防线） */
  const [interruptArmed, setInterruptArmed] = useState(false);
  /** 打断进行中（请求在飞期间禁用全部按钮） */
  const [interruptBusy, setInterruptBusy] = useState(false);
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
    setForceArmed(false);
    setForceBusy(false);
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
      if (busyId !== null || forceBusy) return;
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
    [busyId, forceBusy, load]
  );

  // 人工解除无出口屏障（确认后调用）：逐个解除在册屏障并重拉。单个 id 的确定性
  // 「无需解除」回执（409 no_barrier / barrier_in_flight——视图与确认之间屏障被
  // 自动路径清除、该行派发在飞有自动路径等）只收尾该 id，**不中断**剩余 id
  // （2026-10-19 复审 Minor：一败全停修复）；仅真失败（网络/5xx/403 等）→
  // 显示错误不静默（已成功的保留——重拉以服务端账本为权威）。
  const forceContinue = useCallback(async () => {
    if (busyId !== null || forceBusy) return;
    const ids = view?.blockedActionIds ?? [];
    setForceArmed(false);
    if (ids.length === 0) return;
    setForceBusy(true);
    setNote(null);
    try {
      for (const id of ids) {
        try {
          await forceContinueCodexAction(id);
        } catch (e) {
          if (!isPerIdSettledRefusal(e)) throw e; // 真失败：中断，剩余未解除
        }
      }
    } catch (e) {
      const message = e instanceof ApiError ? apiErrorReason(e) : String(e);
      setNote(i18n.t("mobile.codex.queue.forceContinueFailed", { message }));
    } finally {
      setForceBusy(false);
      await load();
    }
  }, [busyId, forceBusy, view, load]);

  // 打断桌面当前回合（2026-10-21 新增，破坏性动作）：
  // 桌面 Codex 正在跑的回合不结束 → 在册屏障不释放 → 本面板的待发队列永远不派发
  // （native_turn_pending）。这里给出远程出口：打断 → 回合转终态（interrupted）→
  // 屏障释放 → 队列自动接力。**未确认不发请求**；打断会丢弃该回合未完成的工作，
  // 风险由确认臂的文案如实披露。
  const interruptTurn = useCallback(async () => {
    if (busyId !== null || forceBusy || interruptBusy) return;
    setInterruptArmed(false);
    setInterruptBusy(true);
    setNote(null);
    try {
      const res = await interruptCodexTurn(sessionId);
      // 三种结果如实分别回报（复审 P2-3）：打断成功 / 结果不确定（可能已生效，
      // 不得报失败诱导重按）/ 桌面无可打断回合。失败走 catch 臂。
      setNote(
        res.uncertain
          ? i18n.t("mobile.codex.queue.interruptUncertain")
          : res.interruptedTurnId !== null
            ? i18n.t("mobile.codex.queue.interruptDone")
            : i18n.t("mobile.codex.queue.interruptNoTurn")
      );
    } catch (e) {
      const message = e instanceof ApiError ? apiErrorReason(e) : String(e);
      setNote(i18n.t("mobile.codex.queue.interruptFailed", { message }));
    } finally {
      setInterruptBusy(false);
      // 重拉：回合转终态后屏障会释放、队列接力——面板展示以服务端账本为唯一权威
      await load();
    }
  }, [busyId, forceBusy, interruptBusy, sessionId, load]);

  if (view === null) return null;
  // 全空且无暂停原因：面板自隐（不渲染空壳）
  if (view.actions.length === 0 && view.blockedReasonCode === null) return null;
  // 人工解除按钮仅两类**无自动恢复路径**的阻塞态可见（native_turn_pending 等待
  // Turn 终态自动继续，不提供）；靶向清单来自服务端 blockedActionIds
  const canForceContinue =
    (view.blockedReasonCode === "native_delivery_unknown" ||
      view.blockedReasonCode === "compat_outstanding") &&
    (view.blockedActionIds?.length ?? 0) > 0;
  // 打断出口（2026-10-21）：native_turn_pending = 桌面回合未确认完成 → 队列暂停
  // 派发。自动恢复路径是「回合自己结束」——但回合若一直不结束（长任务/卡住），
  // 手机端就完全无解。这里的出口是**真的让桌面停下**（打断当前回合），而不是
  // 仅清屏障（清屏障后消息仍要等桌面空闲才投得出去）。
  // 能力位门控（复审 P2-2）：canInterrupt=false 的 Desktop build（未活体验证
  // interrupt）按钮在源头隐藏——渲染出来也只会永远 503；旧载荷缺省视为无能力
  //（保守方向：宁可没有按钮，不可有假出口）。
  const canInterrupt =
    view.blockedReasonCode === "native_turn_pending" && view.canInterrupt === true;

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
            : view.blockedReasonCode === "native_delivery_unknown"
              ? i18n.t("mobile.codex.queue.blockedNativeUnknown")
              : i18n.t("mobile.codex.queue.blockedCompat")}
        </p>
      )}
      {/* 人工解除（无出口屏障复审 Important）：两步确认——未确认不发任何请求；
          文案如实说明风险（消息可能实际已投出，继续后续消息可能造成重复内容）。
          人工解除是显式覆盖，「无自动恢复路径」的说明语义不变。 */}
      {canForceContinue && !forceArmed && (
        <button
          type="button"
          data-testid="codex-queue-force-continue"
          disabled={busyId !== null || forceBusy}
          onClick={() => {
            setNote(null);
            setForceArmed(true);
          }}
          className="mt-1 rounded-full border border-amber-500/50 px-2 py-0.5 text-xs text-amber-700 disabled:opacity-40 dark:text-amber-300"
        >
          {i18n.t("mobile.codex.queue.forceContinue")}
        </button>
      )}
      {canForceContinue && forceArmed && (
        <span
          data-testid="codex-queue-force-confirm"
          className="mt-1 flex w-full flex-wrap items-center gap-2 rounded-lg bg-amber-500/10 px-2 py-1 text-[11px] text-amber-700 dark:text-amber-300"
        >
          <span>{i18n.t("mobile.codex.queue.forceContinueRisk")}</span>
          <button
            type="button"
            data-testid="codex-queue-force-yes"
            disabled={busyId !== null || forceBusy}
            onClick={() => {
              void forceContinue();
            }}
            className="rounded-full bg-amber-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-amber-500"
          >
            {i18n.t("mobile.codex.queue.forceContinueYes")}
          </button>
          <button
            type="button"
            data-testid="codex-queue-force-no"
            disabled={busyId !== null || forceBusy}
            onClick={() => {
              setForceArmed(false);
            }}
            className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.codex.queue.forceContinueCancel")}
          </button>
        </span>
      )}
      {/* 打断桌面回合（native_turn_pending 专用出口）：与「人工解除」是两种不同的
          东西——解除只清屏障（消息仍要等桌面空闲才投得出去），打断是真的让桌面
          停下当前回合，队列随即自动接力。两步确认 + 风险如实披露。 */}
      {canInterrupt && !interruptArmed && (
        <button
          type="button"
          data-testid="codex-queue-interrupt"
          disabled={busyId !== null || forceBusy || interruptBusy}
          onClick={() => {
            setNote(null);
            setInterruptArmed(true);
          }}
          className="mt-1 rounded-full border border-red-500/50 px-2 py-0.5 text-xs text-red-700 disabled:opacity-40 dark:text-red-300"
        >
          {i18n.t("mobile.codex.queue.interrupt")}
        </button>
      )}
      {canInterrupt && interruptArmed && (
        <span
          data-testid="codex-queue-interrupt-confirm"
          className="mt-1 flex w-full flex-wrap items-center gap-2 rounded-lg bg-red-500/10 px-2 py-1 text-[11px] text-red-700 dark:text-red-300"
        >
          <span>{i18n.t("mobile.codex.queue.interruptRisk")}</span>
          <button
            type="button"
            data-testid="codex-queue-interrupt-yes"
            disabled={busyId !== null || forceBusy || interruptBusy}
            onClick={() => {
              void interruptTurn();
            }}
            className="rounded-full bg-red-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-red-500"
          >
            {i18n.t("mobile.codex.queue.interruptYes")}
          </button>
          <button
            type="button"
            data-testid="codex-queue-interrupt-no"
            disabled={busyId !== null || forceBusy || interruptBusy}
            onClick={() => {
              setInterruptArmed(false);
            }}
            className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.codex.queue.interruptCancel")}
          </button>
        </span>
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
                disabled={busyId !== null || forceBusy}
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
