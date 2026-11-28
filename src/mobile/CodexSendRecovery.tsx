// 发送结果待确认面板（S5）：Desktop IPC 发送在「已可能写出但未取得足够证据」时的
// 前台呈现（设计 §5.2 第五分类）——结束转圈后接管：保留可见消息与原动作，提供
// 「检查是否送达」（S4 只读核对 ≤10s）与「报告问题」（R5 前身安全引用），**绝不**
// 渲染普通重试（结果未知时重试 = 双投窗口）。
//
// 消费面（只消费不另造）：
// - `GET /codex/action/verify`（S4 四值 outcome）——核对顺序与判定全在服务端；
// - `POST /codex/action/manual-resume`（S4：唯一后继 + 撤销写出权 + 解除屏障，同一
//   事务；converged/already_resumed 幂等语义）——人工新发必须先展示三件风险
//  （原消息仍可能迟到；再次发送产生新消息；本操作不是证明原消息未送达）；
// - `POST /codex/queue/force-continue`（S4 扩展前置）：accepted 无 turnId 只解除运行
//   限制（新鲜可输入前置，非 Clear 409 `target_not_inputtable`）——不重发原消息。
import { useCallback, useEffect, useRef, useState } from "react";
import i18n from "@/i18n";
import { ApiError } from "./api";
import { formatInvokeError } from "../lib/invokeError";
import {
  fetchCodexAction,
  forceContinueCodexAction,
  manualResumeCodexAction,
  newActionId,
  verifyCodexAction,
  type CodexActionVerifyView,
  type CodexActionView,
  type CodexManualResumeResult,
  type CodexSuccessorSend,
} from "./codexIpcApi";

export interface CodexSendRecoveryProps {
  sessionId: string;
  /** 原动作 ID（结果未知的核对锚；核对与人工继续都沿它） */
  actionId: string;
  /** 原文（人工继续的后继重发就是原消息——服务器权威核验） */
  text: string;
  /** true = 30 秒前台预算到期呈现（补一句「已停止等待」） */
  deadlineExceeded?: boolean;
  /** 进入本面板时的已知理由码（网络异常 → null） */
  initialReasonCode: string | null;
  /** W2 父子关系：本面板动作是原动作（{{short}}）人工继续创建的后继时携带原动作 ID */
  resumedFrom?: string | null;
  /** 明确未送达后的重试：composer 建新动作（新 actionId）并展示关联——本面板不自行发送 */
  onRetryNewAction: (predecessorActionId: string) => void;
  /** 统一报告入口（composer 注入安全引用；诊断服务未部署时由 composer 呈现不可用出口） */
  onReportIssue: (errorCode: string | null) => void;
  /** 断联时禁止人工继续（创建后继）与解除屏障，仅保留只读原动作核对与报告。 */
  disconnected?: boolean;
  /** W2：resumed / already_resumed 的后继结果上抛——composer 释放原动作锚并按
   *  五分类接管呈现（NotSent → 标准 chip + 重试沿后继；Queued → 位次 chip；
   *  Unknown → 面板按后继 ID 重挂）。Accepted 仍由本面板呈现「已重新发送」；
   *  send=null（already_resumed / 无 send 信息）→ 本面板呈现「待查」并自行把
   *  核对锚切到后继。 */
  onSuccessorResolved?: (info: {
    originalActionId: string;
    successorActionId: string;
    text: string;
    send: CodexSuccessorSend | null;
  }) => void;
}

/** 面板内结论（核对四值 + 人工继续三态；渲染单点）。verify 携带 anchor = 该结论
 *  所属动作（W2：核对锚可切到后继——结论必须归属正确的动作，重试/解除沿它）。 */
type RecoveryResult =
  | { kind: "verify"; view: CodexActionVerifyView; anchor: string }
  | { kind: "resumed"; successor: string }
  | { kind: "converged"; state: string }
  | { kind: "already"; successor: string };

/** 账本状态 → 人话（converged 显示最新结果用；复用队列面板同款词汇，不另造）。 */
function convergedStateLabel(state: string): string {
  switch (state) {
    case "accepted":
      return i18n.t("mobile.codex.queue.stateAccepted");
    case "rejected":
      return i18n.t("mobile.codex.queue.stateRejected");
    case "cancelled":
      return i18n.t("mobile.codex.queue.stateCancelled");
    default:
      return i18n.t("mobile.codex.queue.stateUnknown");
  }
}

export default function CodexSendRecoveryPanel({
  sessionId: _sessionId,
  actionId,
  text,
  deadlineExceeded = false,
  initialReasonCode,
  resumedFrom = null,
  disconnected = false,
  onRetryNewAction,
  onReportIssue,
  onSuccessorResolved,
}: CodexSendRecoveryProps) {
  /* eslint-disable-next-line react-hooks/refs -- 断联必须在当帧生效：延一帧会让
     断联瞬间到达的回包仍按"在线"处理，误开人工继续/解除屏障。 */
  const disconnectedRef = useRef(disconnected);
  /* eslint-disable-next-line react-hooks/refs -- 同上。 */
  disconnectedRef.current = disconnected;
  const [verifying, setVerifying] = useState(false);
  const [result, setResult] = useState<RecoveryResult | null>(null);
  const [note, setNote] = useState<string | null>(null);
  /** W2：核对锚——默认 = 本面板动作；already_resumed（或 resumed 无 send 信息）
   *  后切换为后继 ID。复核「核对原动作冒充后继结果」正是 W2 要消灭的形态：
   *  原动作定格 Unknown，对它核对永远 unresolved，只有后继才有真实状态。 */
  const [checkAnchor, setCheckAnchor] = useState(actionId);
  /** 人工新发两步确认：未确认不发任何请求（先展示三件风险） */
  const [resendArmed, setResendArmed] = useState(false);
  useEffect(() => {
    if (disconnected) setResendArmed(false);
  }, [disconnected]);
  const [resendBusy, setResendBusy] = useState(false);
  const [releaseBusy, setReleaseBusy] = useState(false);
  /** T1：本锚已确立 delivered 的单调镜像（自动/手动任一路径置位；换锚复位）。
   *  runVerify 的交错守卫读取它——只防「delivered 被降级」，不拦 resumed/
   *  converged 段的合法核对。 */
  const deliveredRef = useRef(false);
  const autoPollTerminal = useRef(false);
  const checkAnchorRef = useRef(checkAnchor);
  /** 换锚时同步废弃旧锚，防止旧回包在 effect 清理前落地。 */
  const switchAnchor = useCallback((next: string) => {
    if (checkAnchorRef.current === next) return;
    checkAnchorRef.current = next;
    deliveredRef.current = false;
    setCheckAnchor(next);
  }, []);

  const confirmFromAction = useCallback((view: CodexActionView, anchor: string) => {
    const receipt = view.receipt;
    if (
      anchor !== checkAnchorRef.current ||
      view.actionId !== anchor ||
      view.state !== "accepted" ||
      receipt?.actionId !== anchor ||
      receipt.delivery !== "accepted" ||
      typeof receipt.turnId !== "string" ||
      !receipt.turnId.trim()
    ) {
      return false;
    }
    if (!deliveredRef.current) {
      deliveredRef.current = true;
      setResult({
        kind: "verify",
        anchor,
        view: {
          actionId: anchor,
          stage: view.stage ?? "settled",
          outcome: "delivered",
          turnId: receipt.turnId,
          runLimitReleased: false,
          reasonCode: receipt.reasonCode ?? null,
          verifiedAt: Date.now(),
        },
      });
    }
    return true;
  }, []);

  // ---- 只读核对（S4）：≤10s 服务端有界；「没找到」≠「没发送」----
  const runVerify = useCallback(async () => {
    if (verifying) return;
    const anchorAtStart = checkAnchorRef.current;
    setVerifying(true);
    setNote(null);
    try {
      const v = await verifyCodexAction(anchorAtStart);
      // T1：本锚已确立 delivered（单调收敛）时，迟到的手动核对结论不得把结果
      // 覆盖回 unresolved/accepted_unbound；resumed/converged 段的核对不受影响。
      if (deliveredRef.current) return;
      // T1：回写前校验锚——核对在途期间换锚（already_resumed 切后继）时，旧锚
      // 结论不得写进新锚面板（成功路径守卫）。
      if (anchorAtStart !== checkAnchorRef.current) return;
      deliveredRef.current = v.outcome === "delivered";
      setResult({ kind: "verify", view: v, anchor: anchorAtStart });
    } catch {
      // T1：失败路径同锚守卫——换锚后旧核对的网络失败不得清空新锚状态。
      if (anchorAtStart !== checkAnchorRef.current) return;
      // 核对未完成 ≠ 核对失败结论：保持待确认（可稍后再核对），绝不落「未送达」。
      // 异常路径同样不得降级——本锚自动轮询已确认 delivered 时保持结果
      //（仅清 note；未确认时才回退到「无结果」待确认态）。
      if (!deliveredRef.current) setResult(null);
      setNote(i18n.t("mobile.codex.composer.recoveryVerifyFailed"));
    } finally {
      setVerifying(false);
    }
  }, [verifying]);

  // ---- 仅解除运行限制（accepted 无 turnId 出口；S4 新鲜可输入前置在服务端）----
  // W2：靶向 = 产生该结论的核对锚（核对后继 accepted_unbound 时沿后继解除）
  const releaseRunLimit = useCallback(
    async (target: string) => {
      if (disconnectedRef.current || releaseBusy) return;
      setReleaseBusy(true);
      setNote(null);
      try {
        await forceContinueCodexAction(target);
        setNote(i18n.t("mobile.codex.composer.recoveryReleased"));
      } catch (e) {
        if (
          e instanceof ApiError &&
          e.status === 409 &&
          e.data?.error === "target_not_inputtable"
        ) {
          setNote(i18n.t("mobile.codex.composer.recoveryReleaseBlocked"));
        } else {
          const raw =
            e instanceof ApiError
              ? typeof e.data?.reason === "string"
                ? e.data.reason
                : e.message
              : String(e);
          setNote(
            i18n.t("mobile.codex.composer.recoveryReleaseFailed", {
              message: formatInvokeError(raw, i18n.t),
            })
          );
        }
      } finally {
        setReleaseBusy(false);
      }
    },
    [releaseBusy]
  );

  // ---- 人工继续（S4）：两步确认（三件风险）→ 唯一后继 ----
  const doManualResume = useCallback(async () => {
    if (disconnectedRef.current || resendBusy) return;
    setResendArmed(false);
    setResendBusy(true);
    setNote(null);
    try {
      const r: CodexManualResumeResult = await manualResumeCodexAction({
        actionId,
        newActionId: newActionId(),
        text,
        reason: "manual_resume_from_mobile",
      });
      if (r.status === "resumed") {
        // W2：后继随即走正常发送路径，响应的 `send` 携带**真实结果**——按五分类
        // 呈现，绝不凭「后继已创建」宣称已发：
        // - Accepted = 已重新发送（唯一可宣称已发的形态，本面板呈现）；
        // - NotSent/rejected = 明确未送达（composer 标准未送达 chip + 重试沿后继）；
        // - Queued = 已排队（composer 位次 chip + 队列对账沿后继）；
        // - UnknownDelivery = 后继待确认（composer 以 codexUnknown(后继) 重挂面板
        //   ——F7 按 ID 隔离：核对沿后继、绝不自动重发）。
        // 后三类经 onSuccessorResolved 上抛、composer 接管呈现，本面板不 setResult
        //（即将让位或按后继 ID 重挂）。
        const send: CodexSuccessorSend | null = r.send ?? null;
        const receipt = send?.receipt;
        if (receipt && receipt.delivery === "accepted") {
          setResult({ kind: "resumed", successor: r.successorActionId });
        } else if (send === null || (receipt === undefined && send.queued !== true)) {
          // 无 send 信息（旧后端/空对象）：与 already 同面的「待查」——核对锚切后继
          switchAnchor(r.successorActionId);
          setResult({ kind: "already", successor: r.successorActionId });
        }
        onSuccessorResolved?.({
          originalActionId: actionId,
          successorActionId: r.successorActionId,
          text,
          send,
        });
      } else if (r.status === "converged") {
        // converged 没有后继。同锚的持久送达事实在 POST 期间可能已由 GET 确认。
        if (checkAnchorRef.current !== actionId || deliveredRef.current) return;
        if (confirmFromAction(r.action, actionId)) return;
        // 确认期间已收敛：显示最新结果，不重复发送（服务端不建后继不重发）。
        // 复审 round 4（V2）：converged 只能表达真实终态——unknown 防御性按
        // 「仍待确认」处理，绝不渲染「已有结果」（假收敛）。
        if (r.action.state === "unknown") {
          setNote(i18n.t("mobile.codex.composer.recoveryStillPending"));
        } else {
          setResult({ kind: "converged", state: r.action.state });
        }
      } else if (r.status === "write_right_in_flight") {
        // 复审 round 4（V2）：写右已被在途派发取得、尝试未收尾——保持待确认
        //（不是 converged、无后继；稍后可再核对，收尾后人工继续仍可用）
        setNote(i18n.t("mobile.codex.composer.recoveryWriteRightInFlight"));
      } else {
        // W2：already_resumed **不凭存在关系宣称投递成功**——既有后继的真实状态
        // 未知，如实显示「此前已创建后继（ID…）状态待查」，并把核对锚切到后继
        //（对原动作的核对永远 unresolved，冒充不了后继结果）。
        switchAnchor(r.successorActionId);
        setResult({ kind: "already", successor: r.successorActionId });
        onSuccessorResolved?.({
          originalActionId: actionId,
          successorActionId: r.successorActionId,
          text,
          send: null,
        });
      }
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && e.data?.error === "queued_use_retract") {
        setNote(i18n.t("mobile.codex.composer.recoveryResumeQueued"));
      } else if (
        e instanceof ApiError &&
        e.status === 409 &&
        e.data?.error === "manual_resume_text_mismatch"
      ) {
        setNote(i18n.t("mobile.codex.composer.recoveryResumeMismatch"));
      } else {
        const raw =
          e instanceof ApiError
            ? typeof e.data?.reason === "string"
              ? e.data.reason
              : e.message
            : String(e);
        setNote(
          i18n.t("mobile.codex.composer.recoveryResumeFailed", {
            message: formatInvokeError(raw, i18n.t),
          })
        );
      }
    } finally {
      setResendBusy(false);
    }
  }, [actionId, text, resendBusy, onSuccessorResolved, switchAnchor, confirmFromAction]);

  // 结论行的统一渲染（测试锚 codex-recovery-result；未知结论绝不渲染 raw i18n 键）
  const resultText = (() => {
    if (result === null) return null;
    switch (result.kind) {
      case "resumed":
        return i18n.t("mobile.codex.composer.recoveryResumed", {
          short: result.successor.slice(0, 8),
        });
      case "converged":
        return i18n.t("mobile.codex.composer.recoveryConverged", {
          state: convergedStateLabel(result.state),
        });
      case "already":
        return i18n.t("mobile.codex.composer.recoveryAlreadyResumed", {
          short: result.successor.slice(0, 8),
        });
      default:
        // S4 四值 outcome（无裸 "accepted"——送达/已受理都收敛在 delivered）
        switch (result.view.outcome) {
          case "not_delivered":
            return i18n.t("mobile.codex.composer.recoveryNotDelivered");
          case "delivered":
            return i18n.t("mobile.codex.composer.recoveryDelivered");
          case "accepted_unbound":
            return i18n.t("mobile.codex.composer.recoveryAcceptedUnbound");
          default:
            return i18n.t("mobile.codex.composer.recoveryUnresolved");
        }
    }
  })();
  // 明确终态（核对不再重复提供）：未送达 / 已确认送达（送达与已受理收敛于 delivered）
  const terminal =
    result?.kind === "verify" &&
    (result.view.outcome === "not_delivered" || result.view.outcome === "delivered");
  // T1（B6-01/X0）：待确认期间**自动**读取动作状态（只读 GET，零写）——同一锚的
  // 持久 accepted + turn 即自动呈现「已确认送达」并撤下待确认标题。即时挂载读
  // 一次，此后 2s 间隔、最多一条在途；单次 GET 10s 时限（AbortController，超时/
  // 换锚/卸载/终态即取消，迟到的旧响应被锚+取消双保险丢弃）。人工继续（resumed/
  // converged）停止对原动作的轮询——仍待查的后继由换锚后的新一轮跟踪。
  const autoPollHalt = terminal || result?.kind === "resumed" || result?.kind === "converged";
  // T1：停机镜像经 effect 同步（react-hooks/refs：不在渲染期写 ref）。
  useEffect(() => {
    autoPollTerminal.current = autoPollHalt;
  }, [autoPollHalt]);
  const autoPollInflight = useRef<AbortController | null>(null);
  useEffect(() => {
    if (autoPollHalt) return;
    let cancelled = false;
    const poll = async () => {
      if (autoPollInflight.current || autoPollTerminal.current) return;
      // T1（终审修正）：自动 GET 发出与回写都读**同一锚镜像** checkAnchorRef
      //（switchAnchor 同步更新）——去掉独立 effect 同步的第二锚镜像，同 tick
      //「切锚 → 旧锚在途 GET 返回」窗口内的旧锚回执不再通过守卫冒充新锚结论。
      const anchorAtStart = checkAnchorRef.current;
      const ctrl = new AbortController();
      autoPollInflight.current = ctrl;
      const budget = window.setTimeout(() => ctrl.abort(), 10_000);
      try {
        const view = await fetchCodexAction(anchorAtStart, ctrl.signal);
        if (!cancelled && !ctrl.signal.aborted && !autoPollTerminal.current) {
          confirmFromAction(view, anchorAtStart);
        }
      } catch {
        // 取消（超时/换锚/卸载）或短暂网络失败：继续待确认，下一轮重试（绝不落「未送达」）
      } finally {
        window.clearTimeout(budget);
        if (autoPollInflight.current === ctrl) autoPollInflight.current = null;
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), 2000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
      autoPollInflight.current?.abort();
      autoPollInflight.current = null;
    };
  }, [autoPollHalt, checkAnchor, confirmFromAction]);
  // W2：重试/解除靶向 = 产生该结论的核对锚（结论属于后继时沿后继，不再想当然用 actionId）
  const notDeliveredAnchor =
    result?.kind === "verify" && result.view.outcome === "not_delivered" ? result.anchor : null;
  const unboundAnchor =
    result?.kind === "verify" && result.view.outcome === "accepted_unbound" ? result.anchor : null;
  // 人工新发入口（设计 §6.2）：**只读核对仍无结论时**才出现——未核对前不提供
  // （待确认面板只有「检查是否送达」「报告问题」两个出口，绝不普通重试）。
  // W2：仅当 unresolved 结论属于**本面板动作**（原动作）时提供——后继的 unresolved
  // 属于后继自己的待确认面（原动作最多一个后继，再点只会幂等回弹，提供即误导）。
  const showResend =
    result?.kind === "verify" && result.view.outcome === "unresolved" && result.anchor === actionId;
  // T1（B6-01/X0）：同锚持久 accepted+turn 的**已确认送达**成立时撤下待确认
  // 标题（结果行保留「已确认送达」文案；手动核对路径同一呈现）。
  const deliveredConfirmed = result?.kind === "verify" && result.view.outcome === "delivered";

  return (
    <div
      data-testid="codex-recovery-panel"
      className="rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:bg-amber-400/10 dark:text-amber-300"
    >
      {!deliveredConfirmed && (
        <p data-testid="codex-recovery-title" className="font-medium">
          {i18n.t("mobile.codex.composer.recoveryTitle")}
        </p>
      )}
      {!deliveredConfirmed && (
        <p className="mt-0.5">{i18n.t("mobile.codex.composer.recoveryBody")}</p>
      )}
      {!deliveredConfirmed && deadlineExceeded && (
        <p className="mt-0.5">{i18n.t("mobile.codex.composer.recoveryDeadline")}</p>
      )}
      {resumedFrom && (
        // W2 父子关系保留展示：原动作 → 本后继（人工继续创建）
        <p className="mt-0.5" data-testid="codex-recovery-resumed-from">
          {i18n.t("mobile.codex.composer.recoveryResumedFrom", {
            short: resumedFrom.slice(0, 8),
          })}
        </p>
      )}
      <div className="mt-1.5 flex flex-wrap items-center gap-2">
        {/* 检查是否送达（只读核对，≤10s）：明确终态结论后不再重复提供 */}
        {!terminal && (
          <button
            type="button"
            data-testid="codex-recovery-check"
            disabled={verifying || resendBusy}
            onClick={() => {
              void runVerify();
            }}
            className="rounded-full bg-amber-500/20 px-2 py-0.5 disabled:opacity-40 dark:bg-amber-400/20"
          >
            {verifying
              ? i18n.t("mobile.codex.composer.recoveryChecking")
              : i18n.t("mobile.codex.composer.recoveryCheck")}
          </button>
        )}
        {/* 报告问题（R5 前身）：安全引用由 composer 注入 */}
        <button
          type="button"
          data-testid="codex-recovery-report"
          onClick={() => onReportIssue(initialReasonCode)}
          className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--mut)]"
        >
          {i18n.t("reportIssue.button")}
        </button>
      </div>
      {resultText !== null && (
        <p data-testid="codex-recovery-result" className="mt-1.5">
          {resultText}
        </p>
      )}
      {/* 明确未送达 → 重试（新动作，composer 展示关联；绝不重放原失败动作）。
          W2：靶向产生结论的核对锚（后继的未送达 → 重试沿后继 ID） */}
      {!disconnected && notDeliveredAnchor !== null && (
        <button
          type="button"
          data-testid="codex-recovery-retry"
          onClick={() => onRetryNewAction(notDeliveredAnchor)}
          className="mt-1.5 rounded-full bg-emerald-500/15 px-2 py-0.5 text-emerald-800 dark:bg-emerald-400/15 dark:text-emerald-300"
        >
          {i18n.t("mobile.codex.composer.recoveryRetry")}
        </button>
      )}
      {/* 已受理无 turnId → 只解除运行限制（不重发原消息；新鲜可输入前置在服务端）。
          W2：靶向产生结论的核对锚 */}
      {!disconnected && unboundAnchor !== null && (
        <button
          type="button"
          data-testid="codex-recovery-release"
          disabled={releaseBusy || resendBusy}
          onClick={() => {
            void releaseRunLimit(unboundAnchor);
          }}
          className="mt-1.5 rounded-full bg-amber-500/20 px-2 py-0.5 disabled:opacity-40 dark:bg-amber-400/20"
        >
          {i18n.t("mobile.codex.composer.recoveryReleaseLimit")}
        </button>
      )}
      {/* 无结论 → 人工新发（两步确认；三件风险如实披露后才可确认） */}
      {!disconnected && showResend && !resendArmed && (
        <button
          type="button"
          data-testid="codex-recovery-resend"
          disabled={resendBusy || releaseBusy}
          onClick={() => {
            setNote(null);
            setResendArmed(true);
          }}
          className="mt-1.5 rounded-full border border-amber-500/50 px-2 py-0.5 disabled:opacity-40"
        >
          {i18n.t("mobile.codex.composer.recoveryResend")}
        </button>
      )}
      {!disconnected && showResend && resendArmed && (
        <div
          data-testid="codex-recovery-resend-risks"
          className="mt-1.5 rounded-lg bg-amber-500/10 px-2 py-1.5 dark:bg-amber-400/10"
        >
          <ul className="list-disc space-y-0.5 pl-4">
            <li>{i18n.t("mobile.codex.composer.recoveryResendRisk1")}</li>
            <li>{i18n.t("mobile.codex.composer.recoveryResendRisk2")}</li>
            <li>{i18n.t("mobile.codex.composer.recoveryResendRisk3")}</li>
          </ul>
          <button
            type="button"
            data-testid="codex-recovery-resend-confirm"
            disabled={resendBusy}
            onClick={() => {
              void doManualResume();
            }}
            className="mt-1.5 rounded-full bg-amber-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-amber-500"
          >
            {i18n.t("mobile.codex.composer.recoveryResendConfirm")}
          </button>
          <button
            type="button"
            data-testid="codex-recovery-resend-cancel"
            disabled={resendBusy}
            onClick={() => setResendArmed(false)}
            className="mt-1.5 ml-2 rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--mut)] disabled:opacity-40"
          >
            {i18n.t("mobile.codex.composer.recoveryResendCancel")}
          </button>
        </div>
      )}
      {note !== null && (
        <p data-testid="codex-recovery-note" className="mt-1.5 break-words">
          {note}
        </p>
      )}
      <p className="mt-1 text-[10px] text-[var(--mut)]" data-testid="codex-recovery-action-id">
        {i18n.t("mobile.codex.composer.recoveryActionRef", { short: actionId.slice(0, 8) })}
      </p>
    </div>
  );
}
