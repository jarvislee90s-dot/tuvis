/**
 * F03／T09／R13–R14：**异步 Question 卡**（空闲正式答题）。
 *
 * 状态机（R13）：
 * - 点击提交 → `submitting`（同题**同一 actionId**，网络异常/NotSent 后沿用它）；
 * - `Accepted` **不本地宣布已答**：进入 `submitted`（"已提交，待桌面确认"），
 *   最多等 30 秒；只有**原线程投影**（props）转 Answered 才清卡；
 * - `Unknown`/网络异常 → `unknown`：保留原 ID、只读核对（verifyCodexAction，
 *   10 秒有界、零发送），绝不生成新 ID 重发；核对明确未投递才重新开放提交；
 * - `NotSent`/登记前 400/409 → 明确零投递：保稿，**下一次点击换新 ID**
 *   （旧 ID 已是终态，重放只会拿回旧终态）；
 * - 在途归属：token 换代或新一轮提交后，旧回执的任何落地都被丢弃；
 * - 本地截止从**点击**起算（POST 挂起也在 30 秒结束转圈，且不产生第二次 POST）。
 *
 * 输入（R14）：选项与自由文本**同时**提供（有选项也能自由作答）；阻断时仍可编辑
 * 草稿，只是不能提交。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import i18n from "@/i18n";
import {
  answerCodexAsync,
  isCodexPreDeliveryRefusal,
  newActionId,
  verifyCodexAction,
  type CodexAsyncQuestionView,
} from "./codexIpcApi";
import { ApiError } from "./api";

const SUBMIT_DEADLINE_MS = 30_000;

/** H11：原动作锚跨收纳/重挂保留（同 session+题+token 才复用；不跨上下文）。 */
function actionAnchorKey(sessionId: string, question: CodexAsyncQuestionView): string {
  return `codex:async:${sessionId}:${question.questionKey}:${question.contextToken}`;
}

type Phase = "idle" | "submitting" | "submitted" | "unknown";

/** 机器码 → 可读文案（未知码原样显示，不猜）。 */
function reasonText(code: string | null): string | null {
  if (!code) return null;
  const key = `errors.${code.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase())}`;
  return i18n.t(key, { defaultValue: code });
}

export interface AsyncQuestionCardProps {
  sessionId: string;
  question: CodexAsyncQuestionView;
  /** 提交/终态后让页面刷新投影（SSE 之外的兜底）。 */
  onChanged?: () => void;
}

export default function AsyncQuestionCard({
  sessionId,
  question,
  onChanged,
}: AsyncQuestionCardProps) {
  const [selected, setSelected] = useState<string | null>(null);
  const [freeText, setFreeText] = useState("");
  const [phase, setPhase] = useState<Phase>("idle");
  const [error, setError] = useState<string | null>(null);
  const [timedOut, setTimedOut] = useState(false);
  const actionIdRef = useRef<string | null>(null);
  const alive = useRef(true);
  // H10/H11：在途归属——提交轮次 + 提交时 token；旧轮次的回执一律丢弃。
  const epochRef = useRef(0);
  const tokenRef = useRef(question.contextToken);
  const deadlineRef = useRef<{ epoch: number; timer: number } | null>(null);

  const isCurrent = useCallback((epoch: number, token: string) => {
    return alive.current && epochRef.current === epoch && tokenRef.current === token;
  }, []);

  /** H11：只清**本轮拥有**的 timer（旧 A 的 finally 不得清新 B 的截止）。 */
  const clearMyDeadline = useCallback((epoch: number) => {
    if (deadlineRef.current?.epoch === epoch) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
    };
  }, []);

  // H11：题目上下文换代（同一题新 token）→ 旧回执不得落地；草稿保留。
  useEffect(() => {
    tokenRef.current = question.contextToken;
    epochRef.current += 1;
    // H11：换 token 恢复**本上下文**的原动作锚（Unknown 后重进页面仍可核对）。
    const anchor = window.sessionStorage.getItem(actionAnchorKey(sessionId, question));
    actionIdRef.current = anchor;
    if (deadlineRef.current !== null) {
      window.clearTimeout(deadlineRef.current.timer);
      deadlineRef.current = null;
    }
    setPhase(anchor ? "unknown" : "idle");
    setError(null);
    setTimedOut(false);
    // N14：换上下文 token 时清除可提交稿（旧答案不得自动成为新上下文答案）。
    setSelected(null);
    setFreeText("");
    // N12：仅真实来源/题目身份换代才重置；SSE/轮询的普通对象刷新不作废拥有者。
  }, [
    question.contextToken,
    question.questionKey,
    question.sourceTurnId,
    question.sourceItemId,
    question.questionIndex,
    sessionId,
  ]);

  const hasOptions = question.options.length > 0;
  // R14：自由文本优先（有选项也能自由作答）；否则取选中项。
  const typed = freeText.trim();
  const answer = typed.length > 0 ? typed : (selected ?? "");
  // H11：POST 挂起时仍可编辑草稿（不能再次提交）。
  // H11：提交挂起时仍可编辑草稿；只有"待确认"锁输入。
  const canEdit = phase !== "submitted";
  // H12：暂停题只给**显式**「仅本题先发」——默认零帧。
  const pauseOnly =
    !question.actionable && question.disabledReasonCode === "operation_queue_paused";
  const canSubmit = question.actionable && answer.length > 0 && phase === "idle";
  const canBypassSubmit = pauseOnly && answer.length > 0 && phase === "idle";

  const blockedText = useMemo(() => {
    if (question.actionable) return null;
    const reason = reasonText(question.disabledReasonCode);
    const base =
      question.state === "temporarily_blocked"
        ? i18n.t("mobile.codex.async.blocked")
        : question.state === "unknown"
          ? i18n.t("mobile.codex.async.unknown")
          : i18n.t("mobile.codex.async.blocked");
    return reason ? `${base}：${reason}` : base;
  }, [question.actionable, question.disabledReasonCode, question.state]);

  /** H11：只读核对原动作（10 秒有界、零发送；Unknown 后的唯一出口）。 */
  const verify = useCallback(async () => {
    const actionId = actionIdRef.current;
    if (!actionId) return;
    const epoch = epochRef.current;
    const token = tokenRef.current;
    const current = () => isCurrent(epoch, token);
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 10_000);
    try {
      const view = await verifyCodexAction(actionId, controller.signal);
      if (!current()) return;
      if (view.outcome === "delivered" || view.outcome === "accepted_unbound") {
        setPhase("submitted");
        setError(null);
        return;
      }
      if (view.outcome === "not_delivered" && view.stage === "settled") {
        // 明确零投递 → 保稿、重新开放；旧 ID 终态，下一次点击换新 ID。
        actionIdRef.current = null;
        window.sessionStorage.removeItem(actionAnchorKey(sessionId, question));
        setPhase("idle");
        setError(null);
        setTimedOut(false);
        return;
      }
      setPhase("unknown");
      setError(view.reasonCode);
    } catch (e) {
      if (!current()) return;
      setPhase("unknown");
      const message = e instanceof ApiError ? e.message : String(e);
      setError(message);
    } finally {
      window.clearTimeout(timeout);
    }
  }, [isCurrent]);

  async function submit(bypassPausedQueue = false) {
    if (bypassPausedQueue ? !canBypassSubmit : !canSubmit) return;
    // H10：一次用户动作一个稳定 ID；明确零投递/登记前拒绝后**结束**该动作。
    if (actionIdRef.current === null) actionIdRef.current = newActionId();
    const actionId = actionIdRef.current;
    const epoch = ++epochRef.current;
    const token = question.contextToken;
    const current = () => isCurrent(epoch, token);
    setPhase("submitting");
    setError(null);
    setTimedOut(false);
    // H11：截止从**点击**起算（POST 挂起也不能一直转圈）；timer 归本轮。
    if (deadlineRef.current !== null) window.clearTimeout(deadlineRef.current.timer);
    const startedAt = Date.now();
    const timer = window.setTimeout(() => {
      if (!current()) return;
      setTimedOut(true);
      setPhase("unknown");
    }, SUBMIT_DEADLINE_MS);
    deadlineRef.current = { epoch, timer };
    window.sessionStorage.setItem(actionAnchorKey(sessionId, question), actionId);
    try {
      const receipt = await answerCodexAsync({
        sessionId,
        actionId,
        questionKey: question.questionKey,
        contextToken: token,
        answer,
        // N14：实际调用如实传点击起算的剩余预算。
        frontBudgetMs: Math.max(0, SUBMIT_DEADLINE_MS - (Date.now() - startedAt)),
        // 只在显式越队时携带（严格请求体，不多发键）。
        ...(bypassPausedQueue ? { bypassPausedQueue: true } : {}),
      });
      if (!current()) return;
      clearMyDeadline(epoch);
      if (receipt.delivery === "accepted") {
        // 受理 ≠ 已答：等原线程投影（props.state === answered）才清卡。
        setPhase("submitted");
        onChanged?.();
      } else if (receipt.delivery === "not_sent") {
        // 明确零投递：旧 ID 已是终态，下一次点击必须用新 ID。
        actionIdRef.current = null;
        window.sessionStorage.removeItem(actionAnchorKey(sessionId, question));
        setPhase("idle");
        setError(
          `${receipt.reasonCode ?? receipt.delivery} — ${i18n.t("mobile.codex.async.keepDraft")}`
        );
      } else if (receipt.delivery === "rejected") {
        // H11：明确拒绝 = 零投递，不是"投递未知"——结束动作、保稿可改后再发。
        actionIdRef.current = null;
        window.sessionStorage.removeItem(actionAnchorKey(sessionId, question));
        setPhase("idle");
        setError(
          `${receipt.reasonCode ?? receipt.delivery} — ${i18n.t("mobile.codex.async.keepDraft")}`
        );
      } else {
        // unknown：保留原 ID 与核对锚。
        setPhase("unknown");
        setError(
          `${receipt.reasonCode ?? receipt.delivery} — ${i18n.t("mobile.codex.async.keepDraft")}`
        );
      }
    } catch (e) {
      if (!current()) return;
      clearMyDeadline(epoch);
      const message = e instanceof ApiError ? e.message : String(e);
      // H10：按**机器码**分诊（不只看状态码）；代理 5xx/网络异常仍按未知处理。
      if (isCodexPreDeliveryRefusal(e)) {
        actionIdRef.current = null;
        window.sessionStorage.removeItem(actionAnchorKey(sessionId, question));
        setPhase("idle");
        setError(`${message} — ${i18n.t("mobile.codex.async.keepDraft")}`);
        return;
      }
      setPhase("unknown");
      setError(`${message} — ${i18n.t("mobile.codex.async.keepDraft")}`);
    }
  }

  // 投影已答（唯一权威）→ 收敛为一行；否则即使本地 Accepted 也不宣布已答。
  if (question.state === "answered") {
    return (
      <div
        data-testid="codex-async-answered"
        className="rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-sm text-[var(--mut)]"
      >
        <span className="font-medium text-[var(--tx)]">{question.title}</span>
        <span className="ml-2">{i18n.t("mobile.codex.async.answered")}</span>
      </div>
    );
  }

  return (
    <section
      data-testid="codex-async-question"
      data-phase={phase}
      className="rounded-xl border border-[var(--bd)] bg-[var(--cd)] p-3 text-sm"
    >
      <header className="mb-2 flex items-center justify-between gap-2">
        <span className="text-xs font-medium text-[var(--mut)]">
          {i18n.t("mobile.codex.async.title")}
        </span>
        <span className="truncate text-xs text-[var(--mut)]">{question.title}</span>
      </header>
      {hasOptions && (
        <div className="mb-2 flex flex-wrap gap-2" role="group" aria-label={question.title}>
          {question.options.map((option) => (
            <button
              key={option}
              type="button"
              aria-pressed={selected === option && typed.length === 0}
              disabled={!canEdit}
              onClick={() => setSelected(option)}
              className={
                "rounded-lg border px-3 py-1.5 " +
                (selected === option && typed.length === 0
                  ? "border-[var(--ac)] bg-[var(--ac)] text-white"
                  : "border-[var(--bd)]")
              }
            >
              {option}
            </button>
          ))}
        </div>
      )}
      {/* R14：自由作答始终可编辑（有选项时同样可用），便于运行中先写好草稿。 */}
      <textarea
        aria-label={i18n.t("mobile.codex.async.freeText")}
        placeholder={i18n.t("mobile.codex.async.freeText")}
        value={freeText}
        disabled={!canEdit}
        onChange={(event) => setFreeText(event.target.value)}
        rows={3}
        className="w-full rounded-lg border border-[var(--bd)] bg-transparent p-2"
      />
      {blockedText && (
        <p data-testid="codex-async-blocked" className="mt-2 text-xs text-[var(--mut)]">
          {blockedText}
        </p>
      )}
      {phase === "submitted" && (
        <div className="mt-2 flex items-center justify-between gap-2 text-xs text-[var(--mut)]">
          <span data-testid="codex-async-pending">
            {timedOut
              ? i18n.t("mobile.codex.async.verifyHint")
              : i18n.t("mobile.codex.async.submitted")}
          </span>
          <button
            type="button"
            data-testid="codex-async-verify"
            onClick={() => void verify()}
            className="underline"
          >
            {i18n.t("mobile.codex.async.verify")}
          </button>
        </div>
      )}
      {phase === "unknown" && (
        <div
          data-testid="codex-async-unknown"
          className="mt-2 rounded-lg bg-amber-500/10 p-2 text-xs text-amber-700 dark:text-amber-400"
        >
          <p>
            {timedOut
              ? i18n.t("mobile.codex.async.verifyHint")
              : i18n.t("mobile.codex.async.unknownHint")}
          </p>
          <button
            type="button"
            data-testid="codex-async-verify"
            onClick={() => void verify()}
            className="mt-1 underline"
          >
            {i18n.t("mobile.codex.async.verify")}
          </button>
        </div>
      )}
      {error && (
        <p role="alert" data-testid="codex-async-error" className="mt-2 text-xs text-red-600">
          {error}
        </p>
      )}
      {pauseOnly && (
        <button
          type="button"
          data-testid="codex-async-bypass"
          disabled={!canBypassSubmit}
          onClick={() => void submit(true)}
          className="mt-2 w-full rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-800 disabled:opacity-50 dark:text-amber-300"
        >
          {i18n.t("mobile.codex.async.bypassPaused")}
        </button>
      )}
      <div className="mt-2 flex justify-end">
        <button
          type="button"
          data-testid="codex-async-submit"
          disabled={!canSubmit}
          onClick={() => void submit()}
          className="rounded-lg bg-[var(--ac)] px-3 py-1.5 text-white disabled:opacity-50"
        >
          {phase === "submitting"
            ? i18n.t("mobile.codex.async.submitting")
            : i18n.t("mobile.codex.async.submit")}
        </button>
      </div>
    </section>
  );
}
